//! The `captastic-desktop-ui` thread: the one thread that hosts every window that outlives an
//! overlay run (ADR 0010).
//!
//! It is started by the first request and not before, so a daemon that never pins never creates
//! it. The thread pumps messages for a hidden message-only host window and for the windows it
//! hosts. Requests reach it as messages posted to that host window rather than to the thread: a
//! thread message is discarded by any modal loop running on the thread, and dragging a window or
//! opening a menu runs one.

use std::cell::Cell;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use captastic_core::{CaptureError, CaptureErrorKind, CpuFrame, Rect};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW,
    SetWindowLongPtrW, TranslateMessage, UnregisterClassW, CREATESTRUCTW, GWLP_USERDATA, HCURSOR,
    HWND_MESSAGE, IDC_SIZEALL, MSG, WINDOW_EX_STYLE, WM_APP, WM_NCCREATE, WM_NCDESTROY, WNDCLASSW,
};

use super::pin::{create_pin, pin_window_proc, set_click_through, PIN_CLASS_NAME};
use super::shell::ThreadDpiContext;

const HOST_CLASS_NAME: PCWSTR = w!("CaptasticDesktopUiHost-v1");

/// Posted to the host window: drain the command channel.
const HOST_COMMANDS: u32 = WM_APP + 1;
/// Posted to the host window: close every hosted window and end the thread.
const HOST_STOP: u32 = WM_APP + 2;
/// Posted to the host window by a pin's own menu: close every pin, and keep the thread.
pub(super) const HOST_CLOSE_PINS: u32 = WM_APP + 3;

const START_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_POLL: Duration = Duration::from_millis(5);

enum UiCommand {
    Pin { frame: Box<CpuFrame>, at: Rect },
    ClosePins,
    MakePinsClickable,
}

/// The `captastic-desktop-ui` thread and the way to ask it for windows.
pub struct DesktopUi {
    commands: mpsc::Sender<UiCommand>,
    /// The host window's handle as an integer: `HWND` is a raw handle, and posting to a window
    /// from another thread is exactly what `PostMessageW` is for.
    host_window: isize,
    join: Option<JoinHandle<()>>,
}

impl DesktopUi {
    /// Starts the thread and waits until it can take requests.
    pub fn start() -> Result<Self, CaptureError> {
        let (commands, receiver) = mpsc::channel();
        let (ready_sender, ready) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("captastic-desktop-ui".to_owned())
            .spawn(move || run_host(receiver, ready_sender))
            .map_err(|error| desktop_error("spawn_desktop_ui_thread", error.to_string()))?;
        match ready.recv_timeout(START_TIMEOUT) {
            Ok(Ok(host_window)) => Ok(Self {
                commands,
                host_window,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => Err(desktop_error(
                "start_desktop_ui_thread",
                "the desktop UI thread did not start in time".to_owned(),
            )),
        }
    }

    /// Pins `frame`, placing it where the capture was taken. The frame should already be
    /// [`CpuFrame::detached`]: a pin keeps it until the user closes it.
    pub fn pin(&self, frame: CpuFrame, at: Rect) -> Result<(), CaptureError> {
        self.request(UiCommand::Pin {
            frame: Box::new(frame),
            at,
        })
    }

    pub fn close_all_pins(&self) -> Result<(), CaptureError> {
        self.request(UiCommand::ClosePins)
    }

    pub fn make_all_pins_clickable(&self) -> Result<(), CaptureError> {
        self.request(UiCommand::MakePinsClickable)
    }

    fn request(&self, command: UiCommand) -> Result<(), CaptureError> {
        self.commands.send(command).map_err(|_| {
            desktop_error(
                "send_desktop_ui_command",
                "the desktop UI thread has exited".to_owned(),
            )
        })?;
        self.post(HOST_COMMANDS)
    }

    fn post(&self, message: u32) -> Result<(), CaptureError> {
        // SAFETY: Posting is safe to any window handle; a host window that has already been
        // destroyed makes the call fail, which is reported rather than assumed.
        unsafe { PostMessageW(HWND(self.host_window), message, WPARAM(0), LPARAM(0)) }
            .map_err(|error| desktop_error("post_desktop_ui_message", error.to_string()))
    }

    /// Closes every hosted window and waits for the thread until `deadline`. Returns whether it
    /// ended in time; one that did not is left to end with the process, since it holds nothing but
    /// windows.
    pub fn stop(mut self, deadline: Instant) -> bool {
        self.stop_inner(deadline)
    }

    fn stop_inner(&mut self, deadline: Instant) -> bool {
        let Some(join) = self.join.take() else {
            return true;
        };
        if self.post(HOST_STOP).is_err() {
            // The host window is gone, so the thread is ending or has ended on its own.
        }
        while !join.is_finished() {
            if Instant::now() >= deadline {
                log::warn!("the desktop UI thread did not stop before the shutdown deadline");
                return false;
            }
            thread::sleep(STOP_POLL);
        }
        let _ = join.join();
        true
    }
}

impl Drop for DesktopUi {
    fn drop(&mut self) {
        self.stop_inner(Instant::now() + Duration::from_secs(1));
    }
}

thread_local! {
    /// This thread's host window, for a hosted window that needs to reach it. Zero on every other
    /// thread.
    static HOST_WINDOW: Cell<isize> = const { Cell::new(0) };
}

pub(super) fn host_window() -> Option<HWND> {
    let host = HOST_WINDOW.with(Cell::get);
    (host != 0).then_some(HWND(host))
}

/// What the thread keeps between messages.
struct HostState {
    commands: mpsc::Receiver<UiCommand>,
    instance: HINSTANCE,
    pins: Vec<HWND>,
}

fn run_host(
    commands: mpsc::Receiver<UiCommand>,
    ready: mpsc::SyncSender<Result<isize, CaptureError>>,
) {
    let started = (|| {
        let dpi = ThreadDpiContext::enter_per_monitor_v2()?;
        // SAFETY: No module name requests the current executable module.
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|error| desktop_error("get_module_handle", error.to_string()))?;
        let instance = HINSTANCE(module.0);
        register_class(HOST_CLASS_NAME, instance, host_window_proc, None)?;
        // SAFETY: IDC_SIZEALL is a valid system cursor resource.
        let cursor = unsafe { LoadCursorW(None, IDC_SIZEALL) }.ok();
        register_class(PIN_CLASS_NAME, instance, pin_window_proc, cursor)?;
        let state = Box::new(HostState {
            commands,
            instance,
            pins: Vec::new(),
        });
        let state_pointer = Box::into_raw(state);
        // SAFETY: A message-only window of a class registered above; the state pointer is handed
        // to WM_NCCREATE and reclaimed in WM_NCDESTROY.
        let host = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                HOST_CLASS_NAME,
                w!("Captastic desktop UI"),
                Default::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                instance,
                Some(state_pointer.cast()),
            )
        };
        if host.0 == 0 {
            // SAFETY: Creation failed before WM_NCCREATE could take ownership of the state.
            drop(unsafe { Box::from_raw(state_pointer) });
            return Err(desktop_error(
                "create_desktop_ui_host_window",
                windows::core::Error::from_win32().to_string(),
            ));
        }
        Ok((dpi, host, instance))
    })();
    let (_dpi, host, instance) = match started {
        Ok(started) => started,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    HOST_WINDOW.with(|cell| cell.set(host.0));
    let _ = ready.send(Ok(host.0));
    let mut message = MSG::default();
    // SAFETY: message is writable storage owned by this loop, which runs on the thread that owns
    // every window it dispatches to.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        // SAFETY: message came from GetMessageW on this thread.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    HOST_WINDOW.with(|cell| cell.set(0));
    // Every window of every class is gone, so a later host can register them afresh. Failure
    // changes nothing: `register_class` accepts a class that is still registered.
    for class in [PIN_CLASS_NAME, HOST_CLASS_NAME] {
        // SAFETY: Unregisters a class registered by this thread, after its last window.
        let _ = unsafe { UnregisterClassW(class, instance) };
    }
}

fn register_class(
    name: PCWSTR,
    instance: HINSTANCE,
    procedure: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    cursor: Option<HCURSOR>,
) -> Result<(), CaptureError> {
    let class = WNDCLASSW {
        lpfnWndProc: Some(procedure),
        hInstance: instance,
        hCursor: cursor.unwrap_or_default(),
        lpszClassName: name,
        ..Default::default()
    };
    // SAFETY: class is fully initialized and its name is a static wide string.
    if unsafe { RegisterClassW(&class) } == 0 {
        let error = windows::core::Error::from_win32();
        // A class left registered by an earlier host on this process is the same class.
        const ERROR_CLASS_ALREADY_EXISTS: i32 = 0x8007_0582_u32 as i32;
        if error.code().0 != ERROR_CLASS_ALREADY_EXISTS {
            return Err(desktop_error(
                "register_desktop_ui_class",
                error.to_string(),
            ));
        }
    }
    Ok(())
}

/// Installs the creation parameter as the window's state on `WM_NCCREATE` and returns the state
/// pointer for every later message.
pub(super) fn window_state<T>(hwnd: HWND, message: u32, lparam: LPARAM) -> *mut T {
    if message == WM_NCCREATE {
        // SAFETY: WM_NCCREATE's lparam points to the CREATESTRUCTW for this creation call.
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        // SAFETY: hwnd is the window being created on this thread.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
    }
    // SAFETY: Reads back only the pointer installed above.
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut T }
}

/// Takes back ownership of a window's state as it is destroyed.
pub(super) fn release_window_state<T>(hwnd: HWND) -> Option<Box<T>> {
    // SAFETY: Reads the pointer installed in WM_NCCREATE on this thread.
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut T;
    // SAFETY: Clears it so no later message can reach freed state.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
    // SAFETY: The pointer came from Box::into_raw and is reclaimed exactly once, here.
    (!pointer.is_null()).then(|| unsafe { Box::from_raw(pointer) })
}

unsafe extern "system" fn host_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_pointer = window_state::<HostState>(hwnd, message, lparam);
    match message {
        HOST_COMMANDS if !state_pointer.is_null() => {
            // Drained one at a time with the borrow released between them: creating a window
            // dispatches messages synchronously, and none of them may find this state borrowed.
            loop {
                // SAFETY: The host state lives until WM_NCDESTROY on this thread; the borrow ends
                // with this statement.
                let next = unsafe { (*state_pointer).commands.try_recv() };
                let Ok(command) = next else { break };
                // SAFETY: As above; `apply` re-borrows only for its own bookkeeping.
                unsafe { apply(state_pointer, command) };
            }
            LRESULT(0)
        }
        HOST_CLOSE_PINS if !state_pointer.is_null() => {
            // SAFETY: As above.
            unsafe { close_pins(state_pointer) };
            LRESULT(0)
        }
        HOST_STOP if !state_pointer.is_null() => {
            // SAFETY: As above.
            unsafe { close_pins(state_pointer) };
            // SAFETY: hwnd is this thread's live host window.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            drop(release_window_state::<HostState>(hwnd));
            // SAFETY: Ends this thread's message loop; the host window was the last one alive.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: Forwards an unhandled message with the original arguments.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// # Safety
/// `state` must be the host window's live state, used only on its thread.
unsafe fn apply(state: *mut HostState, command: UiCommand) {
    match command {
        UiCommand::Pin { frame, at } => {
            // SAFETY: Per this function's contract; the borrow ends before create_pin runs.
            let instance = unsafe { (*state).instance };
            match create_pin(instance, &frame, at) {
                // SAFETY: As above.
                Ok(pin) => unsafe { (*state).pins.push(pin) },
                Err(error) => log::warn!("could not pin the capture: {error}"),
            }
        }
        // SAFETY: As above.
        UiCommand::ClosePins => unsafe { close_pins(state) },
        UiCommand::MakePinsClickable => {
            // SAFETY: As above; the list is copied so no borrow is held across window calls.
            let pins = unsafe { (*state).pins.clone() };
            for pin in pins {
                set_click_through(pin, false);
            }
        }
    }
}

/// # Safety
/// `state` must be the host window's live state, used only on its thread.
unsafe fn close_pins(state: *mut HostState) {
    // SAFETY: Per this function's contract. Taken rather than borrowed: destroying a pin reaches
    // `forget_window`, which must not find the list borrowed.
    let pins = std::mem::take(unsafe { &mut (*state).pins });
    for pin in pins {
        // SAFETY: Each handle is a pin window created on this thread; one already destroyed makes
        // the call fail harmlessly.
        let _ = unsafe { DestroyWindow(pin) };
    }
}

/// Removes a hosted window that is being destroyed from the host's lists. A window closed by the
/// host is already out of them, since the host takes a list before closing what is in it.
pub(super) fn forget_window(hwnd: HWND) {
    let Some(host) = host_window() else { return };
    // SAFETY: Reads the host's state pointer on the thread that owns it.
    let state = unsafe { GetWindowLongPtrW(host, GWLP_USERDATA) } as *mut HostState;
    if state.is_null() {
        return;
    }
    // SAFETY: The host state lives until the host's WM_NCDESTROY on this thread, and nothing else
    // holds a borrow of it while a hosted window is being destroyed.
    unsafe { (*state).pins.retain(|pin| *pin != hwnd) };
}

pub(super) fn desktop_error(operation: &'static str, message: String) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-desktop-ui",
        operation,
        message,
        retryable: false,
        native_code: None,
    }
}
