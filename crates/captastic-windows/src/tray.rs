use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use captastic_core::{CaptureError, CaptureErrorKind};
use windows::core::{w, Error as WindowsError, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, LoadIconW, PostMessageW,
    PostQuitMessage, PostThreadMessageW, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, UnregisterClassW,
    CREATESTRUCTW, GWLP_USERDATA, HMENU, HWND_MESSAGE, IDI_APPLICATION, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MSG, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_NCCREATE, WM_NCDESTROY, WM_NULL, WM_QUIT,
    WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};

const CLASS_NAME: PCWSTR = w!("CaptasticTrayWindow-v1");
const WINDOW_NAME: PCWSTR = w!("Captastic");
const TASKBAR_CREATED_NAME: PCWSTR = w!("TaskbarCreated");
const TRAY_CALLBACK: u32 = WM_APP + 1;
const TRAY_ICON_ID: u32 = 1;
const COMMAND_CAPTURE: usize = 1_001;
const COMMAND_PAUSE: usize = 1_002;
const COMMAND_CONFIG: usize = 1_003;
const COMMAND_LOGS: usize = 1_004;
const COMMAND_EXIT: usize = 1_005;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayEvent {
    Capture,
    PausedChanged(bool),
    OpenConfig,
    OpenLogs,
    Exit,
}

pub struct TrayIcon {
    receiver: Receiver<TrayEvent>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl TrayIcon {
    pub fn start() -> Result<Self, CaptureError> {
        let (event_sender, receiver) = mpsc::sync_channel(8);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("captastic-tray".to_owned())
            .spawn(move || {
                if let Err(error) = run_tray(event_sender, &ready_sender) {
                    let _ = ready_sender.send(Err(error.clone()));
                    log::error!("native tray stopped with an error: {error}");
                }
            })
            .map_err(|error| tray_error("spawn_tray_thread", error.to_string()))?;
        match ready_receiver.recv() {
            Ok(Ok(thread_id)) => Ok(Self {
                receiver,
                thread_id,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(error) => {
                let _ = join.join();
                Err(tray_error("start_tray_thread", error.to_string()))
            }
        }
    }

    pub fn try_recv(&self) -> Option<TrayEvent> {
        self.receiver.try_recv().ok()
    }

    pub fn stop(mut self) -> Result<(), CaptureError> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> Result<(), CaptureError> {
        if self.join.is_none() {
            return Ok(());
        }
        // SAFETY: thread_id identifies the live tray message-loop thread initialized by run_tray.
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
            .map_err(|error| native_error("stop_tray_thread", error))?;
        if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| tray_error("join_tray_thread", "tray thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

pub fn open_path(path: &Path) -> Result<(), CaptureError> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // SAFETY: wide is null-terminated for this call; all optional strings are deliberately null.
    let result = unsafe {
        ShellExecuteW(
            HWND(0),
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 <= 32 {
        return Err(tray_error(
            "open_shell_path",
            format!("ShellExecuteW returned {} for {}", result.0, path.display()),
        ));
    }
    Ok(())
}

fn run_tray(
    event_sender: SyncSender<TrayEvent>,
    ready_sender: &SyncSender<Result<u32, CaptureError>>,
) -> Result<(), CaptureError> {
    // SAFETY: No module name requests the current executable module.
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| native_error("get_tray_module", error))?;
    let instance = HINSTANCE(module.0);
    // SAFETY: IDI_APPLICATION is a shared system icon resource.
    let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }
        .map_err(|error| native_error("load_tray_icon", error))?;
    // SAFETY: The registered system message name is static for the process lifetime.
    let taskbar_created = unsafe { RegisterWindowMessageW(TASKBAR_CREATED_NAME) };
    if taskbar_created == 0 {
        return Err(last_error("register_taskbar_created"));
    }
    let class = WNDCLASSW {
        lpfnWndProc: Some(tray_window_proc),
        hInstance: instance,
        hIcon: icon,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: class is fully initialized and the callback/class name remain valid until cleanup.
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(last_error("register_tray_class"));
    }
    let state = Box::new(TrayState {
        event_sender,
        paused: false,
        icon,
        taskbar_created,
    });
    let state_pointer = Box::into_raw(state);
    // SAFETY: The registered class is live and state_pointer remains allocated through the loop.
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            CLASS_NAME,
            WINDOW_NAME,
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            HMENU(0),
            instance,
            Some(state_pointer.cast::<c_void>()),
        )
    };
    if hwnd.0 == 0 {
        // SAFETY: Window creation failed, so no callback can retain the state allocation.
        let _ = unsafe { Box::from_raw(state_pointer) };
        // SAFETY: Balances the successful class registration; no class window was created.
        let _ = unsafe { UnregisterClassW(CLASS_NAME, instance) };
        return Err(last_error("create_tray_window"));
    }
    if let Err(error) = add_tray_icon(hwnd, icon, false) {
        // SAFETY: hwnd is the live hidden window on this thread.
        let _ = unsafe { DestroyWindow(hwnd) };
        // SAFETY: DestroyWindow has completed all callbacks and cleared the stored pointer.
        let _ = unsafe { Box::from_raw(state_pointer) };
        // SAFETY: No window remains for this registered class.
        let _ = unsafe { UnregisterClassW(CLASS_NAME, instance) };
        return Err(error);
    }
    // SAFETY: Called on the tray thread after its message queue and hidden window exist.
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready_sender.send(Ok(thread_id)).is_err() {
        delete_tray_icon(hwnd);
        // SAFETY: hwnd is the live hidden window on this thread.
        let _ = unsafe { DestroyWindow(hwnd) };
        // SAFETY: DestroyWindow has completed all callbacks and cleared the stored pointer.
        let _ = unsafe { Box::from_raw(state_pointer) };
        // SAFETY: No window remains for this registered class.
        let _ = unsafe { UnregisterClassW(CLASS_NAME, instance) };
        return Ok(());
    }

    let mut message = MSG::default();
    let loop_result = loop {
        // SAFETY: message is writable storage and this thread owns the message loop.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            break Err(last_error("tray_message_loop"));
        }
        if result.0 == 0 {
            break Ok(());
        }
        // SAFETY: message was populated by GetMessageW.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    };

    delete_tray_icon(hwnd);
    // SAFETY: hwnd belongs to this thread and is destroyed before state and class cleanup.
    let _ = unsafe { DestroyWindow(hwnd) };
    // SAFETY: Window destruction completed every callback and cleared the stored state pointer.
    let _ = unsafe { Box::from_raw(state_pointer) };
    // SAFETY: No window remains for this class.
    let _ = unsafe { UnregisterClassW(CLASS_NAME, instance) };
    loop_result
}

struct TrayState {
    event_sender: SyncSender<TrayEvent>,
    paused: bool,
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
    taskbar_created: u32,
}

unsafe extern "system" fn tray_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| {
        tray_window_proc_inner(hwnd, message, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            log::error!("native tray callback panicked; requesting shutdown");
            // SAFETY: hwnd belongs to this callback and remains valid for destruction.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
    }
}

fn tray_window_proc_inner(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: WM_NCCREATE lparam points to the CREATESTRUCTW for this creation call.
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        // SAFETY: Stores only the Box pointer passed to CreateWindowExW.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        return LRESULT(1);
    }
    // SAFETY: Retrieves only the pointer installed during WM_NCCREATE.
    let state_pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TrayState;
    if message == WM_NCDESTROY {
        // SAFETY: Prevents callbacks after destruction from observing application state.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        // SAFETY: Default non-client cleanup for this live window.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    if state_pointer.is_null() {
        // SAFETY: No application state is available, so default handling is required.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: The Box remains live for the complete message loop and callbacks are serialized.
    let state = unsafe { &mut *state_pointer };
    if message == state.taskbar_created {
        if let Err(error) = add_tray_icon(hwnd, state.icon, state.paused) {
            log::warn!("failed to restore tray icon after Explorer restart: {error}");
        }
        return LRESULT(0);
    }
    match message {
        TRAY_CALLBACK => {
            let notification = lparam.0 as u32;
            if notification == WM_LBUTTONDBLCLK && !state.paused {
                let _ = state.event_sender.try_send(TrayEvent::Capture);
            } else if notification == WM_RBUTTONUP || notification == WM_CONTEXTMENU {
                if let Err(error) = show_context_menu(hwnd, state.paused) {
                    log::warn!("failed to show tray menu: {error}");
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wparam.0 & 0xffff {
                COMMAND_CAPTURE => {
                    if !state.paused {
                        let _ = state.event_sender.try_send(TrayEvent::Capture);
                    }
                }
                COMMAND_PAUSE => {
                    state.paused = !state.paused;
                    if let Err(error) = modify_tray_tooltip(hwnd, state.paused) {
                        log::warn!("failed to update tray state: {error}");
                    }
                    let _ = state
                        .event_sender
                        .try_send(TrayEvent::PausedChanged(state.paused));
                }
                COMMAND_CONFIG => {
                    let _ = state.event_sender.try_send(TrayEvent::OpenConfig);
                }
                COMMAND_LOGS => {
                    let _ = state.event_sender.try_send(TrayEvent::OpenLogs);
                }
                COMMAND_EXIT => {
                    let _ = state.event_sender.try_send(TrayEvent::Exit);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: Ends only this tray thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            // SAFETY: Standard handling for messages not consumed by the hidden tray window.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

fn add_tray_icon(
    hwnd: HWND,
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
    paused: bool,
) -> Result<(), CaptureError> {
    let data = tray_data(hwnd, icon, paused, NIF_MESSAGE | NIF_ICON | NIF_TIP);
    // SAFETY: data is fully initialized and hwnd is the live tray notification owner.
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        return Err(last_error("add_tray_icon"));
    }
    Ok(())
}

fn modify_tray_tooltip(hwnd: HWND, paused: bool) -> Result<(), CaptureError> {
    let data = tray_data(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::HICON(0),
        paused,
        NIF_TIP,
    );
    // SAFETY: data identifies the existing icon and supplies a valid tooltip buffer.
    if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
        return Err(last_error("modify_tray_icon"));
    }
    Ok(())
}

fn delete_tray_icon(hwnd: HWND) {
    let data = tray_data(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::HICON(0),
        false,
        Default::default(),
    );
    // SAFETY: Best-effort removal of the notification icon owned by hwnd.
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

fn tray_data(
    hwnd: HWND,
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
    paused: bool,
    flags: windows::Win32::UI::Shell::NOTIFY_ICON_DATA_FLAGS,
) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: flags,
        uCallbackMessage: TRAY_CALLBACK,
        hIcon: icon,
        ..Default::default()
    };
    write_wide(&mut data.szTip, tray_tooltip(paused));
    data
}

fn show_context_menu(hwnd: HWND, paused: bool) -> Result<(), CaptureError> {
    // SAFETY: Creates an empty popup menu owned by this function.
    let menu =
        unsafe { CreatePopupMenu() }.map_err(|error| native_error("create_tray_menu", error))?;
    let capture_flags = if paused {
        MF_STRING | MF_GRAYED
    } else {
        MF_STRING
    };
    let result = (|| {
        // SAFETY: menu is live and each static label remains valid for the call.
        unsafe { AppendMenuW(menu, capture_flags, COMMAND_CAPTURE, w!("Capture")) }
            .map_err(|error| native_error("append_capture_menu", error))?;
        let pause_label = if paused { w!("Resume") } else { w!("Pause") };
        // SAFETY: menu and static label are live for the call.
        unsafe { AppendMenuW(menu, MF_STRING, COMMAND_PAUSE, pause_label) }
            .map_err(|error| native_error("append_pause_menu", error))?;
        // SAFETY: A separator ignores its command and text parameters.
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) }
            .map_err(|error| native_error("append_first_separator", error))?;
        // SAFETY: menu and static labels are live for each call.
        unsafe { AppendMenuW(menu, MF_STRING, COMMAND_CONFIG, w!("Open Config")) }
            .map_err(|error| native_error("append_config_menu", error))?;
        // SAFETY: menu and static labels are live for each call.
        unsafe { AppendMenuW(menu, MF_STRING, COMMAND_LOGS, w!("Open Logs")) }
            .map_err(|error| native_error("append_logs_menu", error))?;
        // SAFETY: A separator ignores its command and text parameters.
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) }
            .map_err(|error| native_error("append_second_separator", error))?;
        // SAFETY: menu and static label are live for the call.
        unsafe { AppendMenuW(menu, MF_STRING, COMMAND_EXIT, w!("Exit Captastic")) }
            .map_err(|error| native_error("append_exit_menu", error))?;
        let mut point = POINT::default();
        // SAFETY: point is writable storage for the current cursor position.
        unsafe { GetCursorPos(&mut point) }
            .map_err(|error| native_error("get_tray_cursor_position", error))?;
        // SAFETY: Required by TrackPopupMenu so dismissal messages route to the hidden owner.
        unsafe { SetForegroundWindow(hwnd) };
        // SAFETY: menu and hwnd remain live through this modal popup-menu call.
        unsafe {
            TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                point.x,
                point.y,
                0,
                hwnd,
                None,
            )
        }
        .map_err(|error| native_error("track_tray_menu", error))?;
        // SAFETY: Ensures correct menu dismissal ordering for the foreground owner.
        unsafe { PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0)) }
            .map_err(|error| native_error("dismiss_tray_menu", error))?;
        Ok(())
    })();
    // SAFETY: menu is no longer displayed or used after the closure completes.
    let _ = unsafe { DestroyMenu(menu) };
    result
}

fn tray_tooltip(paused: bool) -> &'static str {
    if paused {
        "Captastic - Paused"
    } else {
        "Captastic - Ready"
    }
}

fn write_wide<const N: usize>(destination: &mut [u16; N], value: &str) {
    destination.fill(0);
    for (output, input) in destination
        .iter_mut()
        .take(N.saturating_sub(1))
        .zip(value.encode_utf16())
    {
        *output = input;
    }
}

fn last_error(operation: &'static str) -> CaptureError {
    native_error(operation, WindowsError::from_win32())
}

fn native_error(operation: &'static str, error: WindowsError) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-tray",
        operation,
        message: error.to_string(),
        retryable: false,
        native_code: Some(i64::from(error.code().0)),
    }
}

fn tray_error(operation: &'static str, message: impl Into<String>) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-tray",
        operation,
        message: message.into(),
        retryable: false,
        native_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_tooltip_reports_operating_state() {
        assert_eq!(tray_tooltip(false), "Captastic - Ready");
        assert_eq!(tray_tooltip(true), "Captastic - Paused");
    }

    #[test]
    fn wide_text_is_terminated_and_truncated_to_capacity() {
        let mut output = [99_u16; 5];
        write_wide(&mut output, "abcdef");
        assert_eq!(output, ['a' as u16, 'b' as u16, 'c' as u16, 'd' as u16, 0]);
    }
}
