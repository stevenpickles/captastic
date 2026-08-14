use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use captastic_core::{CaptureError, CaptureErrorKind};
use windows::core::{w, Error as WindowsError, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Shutdown::{ShutdownBlockReasonCreate, ShutdownBlockReasonDestroy};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, LoadIconW, PostMessageW,
    PostQuitMessage, PostThreadMessageW, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, UnregisterClassW,
    CREATESTRUCTW, GWLP_USERDATA, HMENU, MB_ICONERROR, MB_OK, MF_CHECKED, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MSG, SPI_SETLOGICALDPIOVERRIDE, SPI_SETWORKAREA, SW_SHOWNORMAL, TPM_BOTTOMALIGN,
    TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_DISPLAYCHANGE,
    WM_ENDSESSION, WM_LBUTTONDBLCLK, WM_NCCREATE, WM_NCDESTROY, WM_NULL, WM_QUERYENDSESSION,
    WM_QUIT, WM_RBUTTONUP, WM_SETTINGCHANGE, WNDCLASSW, WS_OVERLAPPED,
};

const CLASS_NAME: PCWSTR = w!("CaptasticTrayWindow-v1");
const WINDOW_NAME: PCWSTR = w!("Captastic");
const TASKBAR_CREATED_NAME: PCWSTR = w!("TaskbarCreated");
const TRAY_CALLBACK: u32 = WM_APP + 1;
const TRAY_SET_STARTUP: u32 = WM_APP + 2;
const TRAY_SHOW_ERROR: u32 = WM_APP + 3;
const TRAY_ICON_ID: u32 = 1;
const APPLICATION_ICON_RESOURCE_ID: usize = 1;
const COMMAND_CAPTURE: usize = 1_001;
const COMMAND_PAUSE: usize = 1_002;
const COMMAND_CONFIG: usize = 1_003;
const COMMAND_LOGS: usize = 1_004;
const COMMAND_STARTUP: usize = 1_005;
const COMMAND_EXIT: usize = 1_006;
const SESSION_DRAIN_TIMEOUT: Duration = Duration::from_secs(4);
const TRAY_STOP_TIMEOUT: Duration = Duration::from_secs(1);
const TRAY_STOP_POLL: Duration = Duration::from_millis(5);
const TRAY_ADD_ATTEMPTS: u32 = 4;
const TRAY_ADD_RETRY_DELAY: Duration = Duration::from_millis(250);
/// `HRESULT_FROM_WIN32(ERROR_TIMEOUT)`: the notification area server was too busy to answer.
const HRESULT_ERROR_TIMEOUT: i32 = 0x8007_05B4_u32 as i32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    Capture,
    PausedChanged(bool),
    OpenConfig,
    OpenLogs,
    ToggleStartup,
    Exit,
}

pub struct TrayIcon {
    receiver: Receiver<TrayEvent>,
    notification_sender: SyncSender<TrayNotification>,
    thread_id: u32,
    hwnd: isize,
    session_shutdown_requested: Arc<AtomicBool>,
    session_drain_completed: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl TrayIcon {
    pub fn start(startup_enabled: bool) -> Result<Self, CaptureError> {
        let (event_sender, receiver) = mpsc::sync_channel(8);
        let (notification_sender, notification_receiver) = mpsc::sync_channel(8);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let session_shutdown_requested = Arc::new(AtomicBool::new(false));
        let session_drain_completed = Arc::new(AtomicBool::new(false));
        let tray_shutdown_requested = session_shutdown_requested.clone();
        let tray_drain_completed = session_drain_completed.clone();
        let join = thread::Builder::new()
            .name("captastic-tray".to_owned())
            .spawn(move || {
                if let Err(error) = run_tray(
                    event_sender,
                    notification_receiver,
                    startup_enabled,
                    tray_shutdown_requested,
                    tray_drain_completed,
                    &ready_sender,
                ) {
                    let _ = ready_sender.send(Err(error.clone()));
                    log::error!("native tray stopped with an error: {error}");
                }
            })
            .map_err(|error| tray_error("spawn_tray_thread", error.to_string()))?;
        match ready_receiver.recv() {
            Ok(Ok((thread_id, hwnd))) => Ok(Self {
                receiver,
                notification_sender,
                thread_id,
                hwnd,
                session_shutdown_requested,
                session_drain_completed,
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

    pub fn session_shutdown_requested(&self) -> bool {
        self.session_shutdown_requested.load(Ordering::Acquire)
    }

    pub fn signal_session_drained(&self) {
        self.session_drain_completed.store(true, Ordering::Release);
    }

    pub fn set_startup_enabled(&self, enabled: bool) -> Result<(), CaptureError> {
        // SAFETY: hwnd identifies the live hidden tray window owned by the tray thread.
        unsafe {
            PostMessageW(
                HWND(self.hwnd),
                TRAY_SET_STARTUP,
                WPARAM(usize::from(enabled)),
                LPARAM(0),
            )
        }
        .map_err(|error| native_error("update_startup_menu", error))
    }

    pub fn show_error(&self, message: impl Into<String>) -> Result<(), CaptureError> {
        self.show_error_with_title("Captastic capture failed", message)
    }

    pub fn show_error_with_title(
        &self,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<(), CaptureError> {
        self.notification_sender
            .try_send(TrayNotification {
                title: title.into(),
                message: message.into(),
            })
            .map_err(|error| tray_error("queue_tray_error", error.to_string()))?;
        // SAFETY: hwnd identifies the live hidden tray window owned by the tray thread.
        unsafe { PostMessageW(HWND(self.hwnd), TRAY_SHOW_ERROR, WPARAM(0), LPARAM(0)) }
            .map_err(|error| native_error("show_tray_error", error))
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
            if !join_tray_worker(join, TRAY_STOP_TIMEOUT)? {
                log::error!(
                    "tray worker did not stop within {} ms; detaching it so shutdown can continue",
                    TRAY_STOP_TIMEOUT.as_millis()
                );
            }
        }
        Ok(())
    }
}

fn join_tray_worker(join: JoinHandle<()>, timeout: Duration) -> Result<bool, CaptureError> {
    let started = Instant::now();
    while !join.is_finished() && started.elapsed() < timeout {
        thread::sleep(TRAY_STOP_POLL);
    }
    if join.is_finished() {
        join.join()
            .map_err(|_| tray_error("join_tray_thread", "tray thread panicked"))?;
        Ok(true)
    } else {
        Ok(false)
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

pub fn show_error_dialog(message: &str) {
    let text: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: text is null-terminated for the modal call and the title is a static wide string.
    let _ = unsafe {
        windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
            HWND(0),
            PCWSTR(text.as_ptr()),
            w!("Captastic"),
            MB_OK | MB_ICONERROR,
        )
    };
}

fn run_tray(
    event_sender: SyncSender<TrayEvent>,
    notification_receiver: Receiver<TrayNotification>,
    startup_enabled: bool,
    session_shutdown_requested: Arc<AtomicBool>,
    session_drain_completed: Arc<AtomicBool>,
    ready_sender: &SyncSender<Result<(u32, isize), CaptureError>>,
) -> Result<(), CaptureError> {
    // SAFETY: No module name requests the current executable module.
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| native_error("get_tray_module", error))?;
    let instance = HINSTANCE(module.0);
    // SAFETY: Resource ID 1 is embedded into each Captastic executable at build time.
    let icon = unsafe { LoadIconW(instance, PCWSTR(APPLICATION_ICON_RESOURCE_ID as *const u16)) }
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
        notification_receiver,
        icon,
        taskbar_created,
        session_shutdown_requested,
        session_drain_completed,
        model: machine::TrayModel::new(startup_enabled),
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
            // A hidden top-level window receives system broadcasts and the registered
            // TaskbarCreated message. Message-only windows are excluded from both.
            HWND(0),
            HMENU(0),
            instance,
            Some(state_pointer.cast::<c_void>()),
        )
    };
    if hwnd.0 == 0 {
        let creation_error = last_error("create_tray_window");
        // SAFETY: Window creation failed, so no callback retains the allocation and there is no
        // window or icon to destroy.
        unsafe { tear_down_tray_window(None, state_pointer, instance) };
        return Err(creation_error);
    }
    let startup_icon_added = match add_tray_icon_with_retry(hwnd, icon, false) {
        Ok(()) => true,
        Err(error) => {
            // Destroying the window here would also destroy the only listener for the
            // TaskbarCreated broadcast, so a single failed add would cost the icon for the rest
            // of the session. The daemon already runs without a tray, so keep the window and its
            // message loop alive in a degraded state and let the existing TaskbarCreated handler
            // restore the icon later.
            log::error!(
                "failed to register the tray icon; running without it until the shell broadcasts TaskbarCreated: {error}"
            );
            false
        }
    };
    {
        // SAFETY: The window is live and no other borrow of the state exists on this thread.
        let state = unsafe { &mut *state_pointer };
        // IconAddCompleted only records the icon state; it never emits effects, so the empty
        // effect list is dropped rather than routed through the applier.
        let _ = machine::transition(
            &mut state.model,
            machine::TrayInput::IconAddCompleted {
                restored: startup_icon_added,
            },
        );
    }
    // SAFETY: Called on the tray thread after its message queue and hidden window exist.
    let thread_id = unsafe { GetCurrentThreadId() };
    if ready_sender.send(Ok((thread_id, hwnd.0))).is_err() {
        // SAFETY: hwnd is the live hidden window on this thread and the state Box has no
        // outstanding borrows once the send has failed.
        unsafe { tear_down_tray_window(Some(hwnd), state_pointer, instance) };
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

    // SAFETY: The message loop has ended, so no callback is executing and the state Box has no
    // outstanding borrows; hwnd belongs to this thread.
    unsafe { tear_down_tray_window(Some(hwnd), state_pointer, instance) };
    loop_result
}

/// Tears down the tray window's native resources in their one required order: the notification
/// icon before its owning window, the window before the state allocation its callbacks dereference,
/// and the class only after no window of the class remains. Every `run_tray` exit path funnels
/// through here so the ordering cannot drift between them.
///
/// # Safety
///
/// Must run on the tray thread. `state_pointer` must be the live allocation created by `run_tray`
/// with no outstanding borrows, and is freed here. `hwnd`, when present, must be the tray window
/// owning that allocation; when `None`, window creation failed and no icon or window exists.
unsafe fn tear_down_tray_window(
    hwnd: Option<HWND>,
    state_pointer: *mut TrayState,
    instance: HINSTANCE,
) {
    if let Some(hwnd) = hwnd {
        delete_tray_icon(hwnd);
        // SAFETY: hwnd is the live hidden window on this thread; destruction runs its callbacks
        // to completion and clears the stored state pointer.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
    // SAFETY: Either window destruction completed every callback or no window ever existed, so
    // nothing can dereference the allocation again.
    let _ = unsafe { Box::from_raw(state_pointer) };
    // SAFETY: No window remains for this registered class.
    let _ = unsafe { UnregisterClassW(CLASS_NAME, instance) };
}

/// Runtime owned by the tray thread: the Win32 handles and channels the effects need, plus the
/// pure [`machine::TrayModel`] that owns every product decision. The window procedure only ever
/// borrows this long enough to translate a message and run one transition; all Win32 calls happen
/// after that borrow ends.
struct TrayState {
    event_sender: SyncSender<TrayEvent>,
    notification_receiver: Receiver<TrayNotification>,
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
    taskbar_created: u32,
    session_shutdown_requested: Arc<AtomicBool>,
    session_drain_completed: Arc<AtomicBool>,
    model: machine::TrayModel,
}

/// The tray window's decision core: the window procedure translates each Win32 message into a
/// [`machine::TrayInput`], runs it through [`machine::transition`], and only then performs the
/// returned [`machine::TrayEffect`]s. The model borrow always ends before the first effect
/// executes, so a `Shell_NotifyIconW` or `TrackPopupMenu` call that synchronously re-enters the
/// window procedure can never alias the state — the structure enforces what scoped-borrow
/// discipline previously had to guarantee by hand at every call site.
mod machine {
    use super::TrayEvent;

    /// Product state of the tray window, free of every Win32 handle and channel so each
    /// transition is testable without a shell.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct TrayModel {
        pub(super) paused: bool,
        pub(super) startup_enabled: bool,
        pub(super) icon: IconState,
        pub(super) session: SessionPhase,
        /// Latched on the first exit request (menu Exit or `WM_CLOSE`). No transition consults
        /// it yet: it records the "user already asked to leave" fact explicitly so a later change
        /// can stop offering captures afterwards, without silently altering today's tolerance for
        /// duplicate Exit events.
        pub(super) exit_requested: bool,
    }

    impl TrayModel {
        pub(super) fn new(startup_enabled: bool) -> Self {
            Self {
                paused: false,
                startup_enabled,
                icon: IconState::Absent,
                session: SessionPhase::Idle,
                exit_requested: false,
            }
        }
    }

    /// Whether the notification-area icon is currently believed to exist. Previously this was
    /// only implied by which log lines had been emitted.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum IconState {
        /// No add attempt has completed yet.
        Absent,
        Present,
        /// The last add exhausted its bounded retries; the hidden window stays alive so a later
        /// `TaskbarCreated` broadcast can restore the icon.
        Degraded,
    }

    /// Where the window sits in the session-shutdown handshake. `QueryReceived` means a shutdown
    /// block reason was requested (attempted, not necessarily created — the destroy must balance
    /// the attempt either way, mirroring the previous `shutdown_block_active` flag).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum SessionPhase {
        Idle,
        QueryReceived,
    }

    /// A Win32 message reduced to its product meaning.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum TrayInput {
        TaskbarCreated,
        DisplayChanged,
        DisplaySettingChanged,
        IconDoubleClick,
        IconContextMenu,
        Menu(TrayMenuCommand),
        StartupStateChanged(bool),
        NotificationsPosted,
        QueryEndSession,
        EndSession {
            committed: bool,
        },
        CloseRequested,
        Destroyed,
        /// Outcome of an [`TrayEffect::AddIcon`] effect, fed back by the effect runner.
        IconAddCompleted {
            restored: bool,
        },
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum TrayMenuCommand {
        Capture,
        Pause,
        OpenConfig,
        OpenLogs,
        ToggleStartup,
        Exit,
    }

    /// A side effect the transition asks the runtime to perform, in order, after the model
    /// borrow has ended. Payloads are snapshots taken at transition time: a reentrant message
    /// that mutates the model mid-effect (a pause toggle while the menu is open, say) does not
    /// rewrite an effect already handed out.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum TrayEffect {
        /// Re-add the notification icon (bounded retry); the runner reports the outcome back
        /// through [`TrayInput::IconAddCompleted`].
        AddIcon {
            paused: bool,
        },
        ModifyTooltip {
            paused: bool,
        },
        ShowMenu {
            paused: bool,
            startup_enabled: bool,
        },
        /// Drain the queued notifications into balloons.
        DrainNotifications,
        SendEvent(TrayEvent),
        CreateShutdownBlockReason,
        DestroyShutdownBlockReason,
        /// Block (bounded) until the daemon drains for a committed session shutdown.
        WaitForSessionDrain,
        MarkDisplayConfigurationChanged(&'static str),
        PostQuit,
    }

    /// What the window procedure should answer Windows once the effects have run.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum MessageDisposition {
        /// `LRESULT(0)`: the message was fully handled.
        Handled,
        /// `LRESULT(1)` for `WM_QUERYENDSESSION`: allow the session to end. Veto is a product
        /// decision this machine could make, but today it always allows.
        AllowSessionEnd,
    }

    pub(super) fn transition(
        model: &mut TrayModel,
        input: TrayInput,
    ) -> (Vec<TrayEffect>, MessageDisposition) {
        use MessageDisposition::Handled;
        match input {
            TrayInput::TaskbarCreated => (
                vec![TrayEffect::AddIcon {
                    paused: model.paused,
                }],
                Handled,
            ),
            TrayInput::IconAddCompleted { restored } => {
                model.icon = if restored {
                    IconState::Present
                } else {
                    IconState::Degraded
                };
                (Vec::new(), Handled)
            }
            TrayInput::DisplayChanged => (
                vec![TrayEffect::MarkDisplayConfigurationChanged(
                    "tray_display_changed",
                )],
                Handled,
            ),
            TrayInput::DisplaySettingChanged => (
                vec![TrayEffect::MarkDisplayConfigurationChanged(
                    "tray_display_setting_changed",
                )],
                Handled,
            ),
            TrayInput::IconDoubleClick | TrayInput::Menu(TrayMenuCommand::Capture) => {
                if model.paused {
                    (Vec::new(), Handled)
                } else {
                    (vec![TrayEffect::SendEvent(TrayEvent::Capture)], Handled)
                }
            }
            TrayInput::IconContextMenu => (
                vec![TrayEffect::ShowMenu {
                    paused: model.paused,
                    startup_enabled: model.startup_enabled,
                }],
                Handled,
            ),
            TrayInput::Menu(TrayMenuCommand::Pause) => {
                model.paused = !model.paused;
                (
                    vec![
                        TrayEffect::ModifyTooltip {
                            paused: model.paused,
                        },
                        TrayEffect::SendEvent(TrayEvent::PausedChanged(model.paused)),
                    ],
                    Handled,
                )
            }
            TrayInput::Menu(TrayMenuCommand::OpenConfig) => {
                (vec![TrayEffect::SendEvent(TrayEvent::OpenConfig)], Handled)
            }
            TrayInput::Menu(TrayMenuCommand::OpenLogs) => {
                (vec![TrayEffect::SendEvent(TrayEvent::OpenLogs)], Handled)
            }
            TrayInput::Menu(TrayMenuCommand::ToggleStartup) => (
                vec![TrayEffect::SendEvent(TrayEvent::ToggleStartup)],
                Handled,
            ),
            TrayInput::Menu(TrayMenuCommand::Exit) | TrayInput::CloseRequested => {
                model.exit_requested = true;
                (vec![TrayEffect::SendEvent(TrayEvent::Exit)], Handled)
            }
            TrayInput::StartupStateChanged(enabled) => {
                model.startup_enabled = enabled;
                (Vec::new(), Handled)
            }
            TrayInput::NotificationsPosted => (vec![TrayEffect::DrainNotifications], Handled),
            TrayInput::QueryEndSession => {
                let effects = if model.session == SessionPhase::Idle {
                    model.session = SessionPhase::QueryReceived;
                    vec![TrayEffect::CreateShutdownBlockReason]
                } else {
                    // Windows may query more than once per session end; the block reason is
                    // created exactly once and stays owed until WM_ENDSESSION or window
                    // destruction settles it.
                    Vec::new()
                };
                (effects, MessageDisposition::AllowSessionEnd)
            }
            TrayInput::EndSession { committed } => {
                let mut effects = Vec::new();
                if committed {
                    effects.push(TrayEffect::WaitForSessionDrain);
                }
                if model.session == SessionPhase::QueryReceived {
                    effects.push(TrayEffect::DestroyShutdownBlockReason);
                }
                // A canceled shutdown returns to Idle so a later query starts a fresh block; a
                // committed one is followed by process death, and Idle is equally correct if
                // Windows ever changes its mind.
                model.session = SessionPhase::Idle;
                (effects, Handled)
            }
            TrayInput::Destroyed => {
                let mut effects = Vec::new();
                if model.session == SessionPhase::QueryReceived {
                    // The window is dying outside the WM_ENDSESSION handshake (daemon stop, menu
                    // Exit mid-handshake, panic recovery). Every destruction path dispatches
                    // WM_DESTROY through this transition while the handle is still valid, so the
                    // block reason that WM_ENDSESSION never got to release is settled here
                    // instead of leaking with the window.
                    effects.push(TrayEffect::DestroyShutdownBlockReason);
                    model.session = SessionPhase::Idle;
                }
                effects.push(TrayEffect::PostQuit);
                (effects, Handled)
            }
        }
    }
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
            // SAFETY: Best-effort recovery reads the pointer installed for this live window. The
            // callback boundary caught the panic before unwinding through Win32.
            let state_pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TrayState;
            if !state_pointer.is_null() {
                send_tray_event(state_pointer, TrayEvent::Exit);
            }
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
    let translated = {
        // SAFETY: The Box remains live for the message loop; this borrow ends before any Win32
        // call an effect might make.
        let state = unsafe { &*state_pointer };
        translate_tray_message(state.taskbar_created, message, wparam, lparam)
    };
    match translated {
        TranslatedMessage::Input(input) => run_tray_input(hwnd, state_pointer, input),
        TranslatedMessage::Consumed => LRESULT(0),
        // SAFETY: Standard handling for messages not consumed by the hidden tray window.
        TranslatedMessage::Unhandled => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// The product meaning of one Win32 message, or how to answer it when it has none.
#[derive(Debug, PartialEq, Eq)]
enum TranslatedMessage {
    Input(machine::TrayInput),
    /// Handled with `LRESULT(0)` but carrying no product decision (an ignored tray-callback
    /// notification, an unrecognized menu command id).
    Consumed,
    /// Forwarded to `DefWindowProcW`.
    Unhandled,
}

fn translate_tray_message(
    taskbar_created: u32,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> TranslatedMessage {
    use machine::{TrayInput, TrayMenuCommand};
    if message == taskbar_created {
        return TranslatedMessage::Input(TrayInput::TaskbarCreated);
    }
    match message {
        WM_DISPLAYCHANGE => TranslatedMessage::Input(TrayInput::DisplayChanged),
        WM_SETTINGCHANGE
            if wparam.0 == SPI_SETWORKAREA.0 as usize
                || wparam.0 == SPI_SETLOGICALDPIOVERRIDE.0 as usize =>
        {
            TranslatedMessage::Input(TrayInput::DisplaySettingChanged)
        }
        TRAY_CALLBACK => match lparam.0 as u32 {
            WM_LBUTTONDBLCLK => TranslatedMessage::Input(TrayInput::IconDoubleClick),
            WM_RBUTTONUP | WM_CONTEXTMENU => TranslatedMessage::Input(TrayInput::IconContextMenu),
            _ => TranslatedMessage::Consumed,
        },
        WM_COMMAND => match wparam.0 & 0xffff {
            COMMAND_CAPTURE => TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::Capture)),
            COMMAND_PAUSE => TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::Pause)),
            COMMAND_CONFIG => {
                TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::OpenConfig))
            }
            COMMAND_LOGS => TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::OpenLogs)),
            COMMAND_STARTUP => {
                TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::ToggleStartup))
            }
            COMMAND_EXIT => TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::Exit)),
            _ => TranslatedMessage::Consumed,
        },
        TRAY_SET_STARTUP => TranslatedMessage::Input(TrayInput::StartupStateChanged(wparam.0 != 0)),
        TRAY_SHOW_ERROR => TranslatedMessage::Input(TrayInput::NotificationsPosted),
        WM_QUERYENDSESSION => TranslatedMessage::Input(TrayInput::QueryEndSession),
        WM_ENDSESSION => TranslatedMessage::Input(TrayInput::EndSession {
            committed: session_end_is_committed(wparam.0),
        }),
        WM_CLOSE => {
            log::warn!("tray window received WM_CLOSE; requesting daemon shutdown");
            TranslatedMessage::Input(TrayInput::CloseRequested)
        }
        WM_DESTROY => TranslatedMessage::Input(TrayInput::Destroyed),
        _ => TranslatedMessage::Unhandled,
    }
}

/// Runs one input through the pure transition, then applies the returned effects. The exclusive
/// model borrow ends before the first effect executes, so effects that synchronously re-enter the
/// window procedure (`Shell_NotifyIconW`, `TrackPopupMenu`) recurse through this same path against
/// a released borrow instead of an aliased one.
fn run_tray_input(hwnd: HWND, state_pointer: *mut TrayState, input: machine::TrayInput) -> LRESULT {
    let (effects, disposition) = {
        // SAFETY: The Box remains live for the message loop; this exclusive borrow ends here,
        // before any effect runs.
        let state = unsafe { &mut *state_pointer };
        machine::transition(&mut state.model, input)
    };
    for effect in effects {
        apply_tray_effect(hwnd, state_pointer, effect);
    }
    match disposition {
        machine::MessageDisposition::Handled => LRESULT(0),
        machine::MessageDisposition::AllowSessionEnd => LRESULT(1),
    }
}

fn apply_tray_effect(hwnd: HWND, state_pointer: *mut TrayState, effect: machine::TrayEffect) {
    use machine::TrayEffect;
    match effect {
        TrayEffect::AddIcon { paused } => {
            let icon = {
                // SAFETY: Short copy of the icon handle; the borrow ends before Shell_NotifyIconW.
                unsafe { (*state_pointer).icon }
            };
            // A restarting shell is exactly the load that makes NIM_ADD time out, so this retries
            // on the tray thread. The worst-case stall is bounded and this thread has no other
            // work.
            let restored = match add_tray_icon_with_retry(hwnd, icon, paused) {
                Ok(()) => true,
                Err(error) => {
                    log::warn!("failed to restore tray icon after Explorer restart: {error}");
                    false
                }
            };
            let _ = run_tray_input(
                hwnd,
                state_pointer,
                machine::TrayInput::IconAddCompleted { restored },
            );
        }
        TrayEffect::ModifyTooltip { paused } => {
            if let Err(error) = modify_tray_tooltip(hwnd, paused) {
                log::warn!("failed to update tray state: {error}");
            }
        }
        TrayEffect::ShowMenu {
            paused,
            startup_enabled,
        } => {
            if let Err(error) = show_context_menu(hwnd, paused, startup_enabled) {
                log::warn!("failed to show tray menu: {error}");
            }
        }
        TrayEffect::DrainNotifications => loop {
            let message = {
                // SAFETY: The receive borrow ends before Shell_NotifyIconW can re-enter.
                unsafe { (*state_pointer).notification_receiver.try_recv() }
            };
            let Ok(message) = message else { break };
            if let Err(error) = show_error_notification(hwnd, &message) {
                log::warn!("failed to show tray error notification: {error}");
            }
        },
        TrayEffect::SendEvent(event) => send_tray_event(state_pointer, event),
        TrayEffect::CreateShutdownBlockReason => {
            // SAFETY: hwnd is this live top-level window and the reason is static UTF-16.
            if let Err(error) = unsafe {
                ShutdownBlockReasonCreate(hwnd, w!("Saving Captastic capture preferences"))
            } {
                log::warn!("failed to register session-shutdown drain reason: {error}");
            }
        }
        TrayEffect::DestroyShutdownBlockReason => {
            // SAFETY: Balances the create attempted when the session query arrived.
            let _ = unsafe { ShutdownBlockReasonDestroy(hwnd) };
        }
        TrayEffect::WaitForSessionDrain => wait_for_session_drain(state_pointer),
        TrayEffect::MarkDisplayConfigurationChanged(reason) => {
            crate::dxgi::mark_display_configuration_changed(reason);
        }
        TrayEffect::PostQuit => {
            // SAFETY: Ends only this tray thread's message loop.
            unsafe { PostQuitMessage(0) };
        }
    }
}

fn session_end_is_committed(end_session: usize) -> bool {
    end_session != 0
}

fn wait_for_session_drain(state_pointer: *mut TrayState) {
    // SAFETY: Clone the atomics from the live state, then release the borrow before waiting.
    let (requested, drained) = unsafe {
        (
            (&*state_pointer).session_shutdown_requested.clone(),
            (&*state_pointer).session_drain_completed.clone(),
        )
    };
    if !wait_for_drain_signal(&requested, &drained, SESSION_DRAIN_TIMEOUT) {
        log::warn!(
            "daemon did not finish its session-shutdown drain within {} ms",
            SESSION_DRAIN_TIMEOUT.as_millis()
        );
    }
}

fn wait_for_drain_signal(requested: &AtomicBool, drained: &AtomicBool, timeout: Duration) -> bool {
    requested.store(true, Ordering::Release);
    let started = Instant::now();
    while !drained.load(Ordering::Acquire) && started.elapsed() < timeout {
        thread::sleep(TRAY_STOP_POLL);
    }
    drained.load(Ordering::Acquire)
}

fn send_tray_event(state_pointer: *mut TrayState, event: TrayEvent) {
    // SAFETY: Callers invoke this only while processing a message for the live tray window. The
    // cloned sender outlives the short borrow and sending cannot alias TrayState on reentry.
    let sender = unsafe { (&*state_pointer).event_sender.clone() };
    dispatch_tray_event(&sender, event);
}

/// Sends a tray event without ever blocking the window procedure, logging when the bounded
/// channel drops it instead of silently discarding it.
///
/// `std::sync::mpsc::SyncSender` offers no way to pop or replace a queued item from the sender
/// side, so a full channel always drops the newest event (the one being sent here) rather than
/// an older one already queued; there is no drop-oldest option available with std mpsc. `Exit` is
/// the one command that must never vanish without a trace, so a dropped `Exit` is logged at
/// `error`; other dropped events only warn. A disconnected channel means the main thread has
/// already stopped reading, which is expected noise during shutdown, so it is logged at `debug`.
fn dispatch_tray_event(sender: &SyncSender<TrayEvent>, event: TrayEvent) {
    match sender.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(dropped)) => {
            if matches!(dropped, TrayEvent::Exit) {
                log::error!("tray event channel is full; dropped {dropped:?}");
            } else {
                log::warn!("tray event channel is full; dropped {dropped:?}");
            }
        }
        Err(TrySendError::Disconnected(dropped)) => {
            log::debug!(
                "tray event channel is disconnected; dropped {dropped:?} (expected during shutdown)"
            );
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
        return Err(shell_notify_error(
            "add_tray_icon",
            WindowsError::from_win32(),
        ));
    }
    Ok(())
}

fn add_tray_icon_with_retry(
    hwnd: HWND,
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
    paused: bool,
) -> Result<(), CaptureError> {
    retry_tray_icon_add(|| add_tray_icon(hwnd, icon, paused), thread::sleep)
}

/// Retries a timed-out `NIM_ADD` a bounded number of times.
///
/// Shell_NotifyIcon is documented to fail with `ERROR_TIMEOUT` when the notification area server
/// is still starting up - the exact condition a daemon launched at logon runs into - and Microsoft
/// recommends sleeping briefly and retrying rather than treating the failure as final. Only errors
/// classified as retryable are repeated; a genuine failure (bad window, bad icon) fails at once.
fn retry_tray_icon_add(
    mut add: impl FnMut() -> Result<(), CaptureError>,
    mut wait: impl FnMut(Duration),
) -> Result<(), CaptureError> {
    let mut attempt = 1_u32;
    loop {
        match add() {
            Err(error) if error.retryable && attempt < TRAY_ADD_ATTEMPTS => {
                log::warn!(
                    "the notification area was busy on tray icon attempt {attempt} of {TRAY_ADD_ATTEMPTS}; retrying: {error}"
                );
                attempt = attempt.saturating_add(1);
                wait(TRAY_ADD_RETRY_DELAY);
            }
            result => return result,
        }
    }
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

#[derive(Debug)]
struct TrayNotification {
    title: String,
    message: String,
}

fn show_error_notification(
    hwnd: HWND,
    notification: &TrayNotification,
) -> Result<(), CaptureError> {
    let mut data = tray_data(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::HICON(0),
        false,
        NIF_INFO,
    );
    write_wide(&mut data.szInfoTitle, &notification.title);
    write_wide(&mut data.szInfo, &notification.message);
    data.dwInfoFlags = NIIF_ERROR;
    // SAFETY: data identifies the existing icon and contains terminated notification buffers.
    if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
        return Err(last_error("show_tray_error_notification"));
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

fn show_context_menu(hwnd: HWND, paused: bool, startup_enabled: bool) -> Result<(), CaptureError> {
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
        let startup_flags = if startup_enabled {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        // SAFETY: menu and static label are live for the call.
        unsafe {
            AppendMenuW(
                menu,
                startup_flags,
                COMMAND_STARTUP,
                w!("Start with Windows"),
            )
        }
        .map_err(|error| native_error("append_startup_menu", error))?;
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

/// Classifies a Shell_NotifyIconW failure, keeping the `retryable` flag truthful: a busy
/// notification-area server reports `ERROR_TIMEOUT` and is worth another attempt, while every
/// other failure is a permanent misuse of the API and must not be retried.
fn shell_notify_error(operation: &'static str, error: WindowsError) -> CaptureError {
    if error.code().0 == HRESULT_ERROR_TIMEOUT {
        return CaptureError {
            kind: CaptureErrorKind::Timeout,
            backend: "windows-tray",
            operation,
            message: format!("the notification area did not respond in time: {error}"),
            retryable: true,
            native_code: Some(i64::from(error.code().0)),
        };
    }
    native_error(operation, error)
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
    use windows::core::HRESULT;

    #[test]
    fn session_shutdown_is_routed_only_after_windows_commits_it() {
        assert!(!session_end_is_committed(0));
        assert!(session_end_is_committed(1));
    }

    #[test]
    fn session_shutdown_handoff_uses_a_dedicated_nonblocking_signal() {
        let requested = Arc::new(AtomicBool::new(false));
        let drained = Arc::new(AtomicBool::new(false));
        let daemon_requested = requested.clone();
        let daemon_drained = drained.clone();
        let daemon = thread::spawn(move || {
            while !daemon_requested.load(Ordering::Acquire) {
                thread::yield_now();
            }
            daemon_drained.store(true, Ordering::Release);
        });

        assert!(wait_for_drain_signal(
            &requested,
            &drained,
            Duration::from_secs(1)
        ));
        daemon.join().expect("join scripted daemon acknowledgement");
    }

    #[test]
    fn session_shutdown_handoff_obeys_its_deadline_without_an_acknowledgement() {
        let requested = AtomicBool::new(false);
        let drained = AtomicBool::new(false);
        let started = Instant::now();

        assert!(!wait_for_drain_signal(
            &requested,
            &drained,
            Duration::from_millis(10)
        ));
        assert!(requested.load(Ordering::Acquire));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn tray_worker_join_detaches_at_its_deadline() {
        let (release_sender, release_receiver) = mpsc::channel();
        let join = thread::spawn(move || {
            let _ = release_receiver.recv();
        });

        let started = Instant::now();
        assert!(!join_tray_worker(join, Duration::from_millis(10)).expect("bounded join"));
        assert!(started.elapsed() < Duration::from_secs(1));
        release_sender
            .send(())
            .expect("release detached tray test worker");
    }

    #[test]
    fn dispatch_tray_event_drops_without_blocking_when_full_or_disconnected() {
        let (sender, receiver) = mpsc::sync_channel(1);
        dispatch_tray_event(&sender, TrayEvent::Capture);
        // The channel is now full; the drop-newest send below must return immediately rather
        // than block the caller, and must leave the already-queued event untouched.
        dispatch_tray_event(&sender, TrayEvent::Exit);
        assert!(matches!(receiver.try_recv(), Ok(TrayEvent::Capture)));
        assert!(receiver.try_recv().is_err());

        drop(receiver);
        // A disconnected receiver must not panic or block either.
        dispatch_tray_event(&sender, TrayEvent::Exit);
    }

    #[test]
    fn a_busy_notification_area_is_classified_as_retryable() {
        let busy = shell_notify_error(
            "add_tray_icon",
            WindowsError::from(HRESULT(HRESULT_ERROR_TIMEOUT)),
        );
        assert_eq!(busy.kind, CaptureErrorKind::Timeout);
        assert!(busy.retryable);
        assert_eq!(busy.native_code, Some(i64::from(HRESULT_ERROR_TIMEOUT)));

        let denied = shell_notify_error(
            "add_tray_icon",
            WindowsError::from(HRESULT(0x8007_0005_u32 as i32)),
        );
        assert_eq!(denied.kind, CaptureErrorKind::NativeFailure);
        assert!(!denied.retryable);
    }

    #[test]
    fn a_timed_out_icon_add_is_retried_to_its_bounded_limit() {
        let mut attempts = 0_u32;
        let mut waits = Vec::new();
        let result = retry_tray_icon_add(
            || {
                attempts = attempts.saturating_add(1);
                Err(shell_notify_error(
                    "add_tray_icon",
                    WindowsError::from(HRESULT(HRESULT_ERROR_TIMEOUT)),
                ))
            },
            |delay| waits.push(delay),
        );

        assert!(result.is_err());
        assert_eq!(attempts, TRAY_ADD_ATTEMPTS);
        assert_eq!(
            waits,
            vec![TRAY_ADD_RETRY_DELAY; TRAY_ADD_ATTEMPTS as usize - 1]
        );
    }

    #[test]
    fn a_transient_icon_add_failure_still_produces_an_icon() {
        let mut attempts = 0_u32;
        let result = retry_tray_icon_add(
            || {
                attempts = attempts.saturating_add(1);
                if attempts < 3 {
                    Err(shell_notify_error(
                        "add_tray_icon",
                        WindowsError::from(HRESULT(HRESULT_ERROR_TIMEOUT)),
                    ))
                } else {
                    Ok(())
                }
            },
            |_| {},
        );

        assert!(result.is_ok());
        assert_eq!(attempts, 3);
    }

    #[test]
    fn a_permanent_icon_add_failure_is_not_retried() {
        let mut attempts = 0_u32;
        let result = retry_tray_icon_add(
            || {
                attempts = attempts.saturating_add(1);
                Err(shell_notify_error(
                    "add_tray_icon",
                    WindowsError::from(HRESULT(0x8007_0006_u32 as i32)),
                ))
            },
            |_| panic!("a permanent failure must not wait"),
        );

        assert!(result.is_err());
        assert_eq!(attempts, 1);
    }

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

    use super::machine::{
        transition, IconState, MessageDisposition, SessionPhase, TrayEffect, TrayInput,
        TrayMenuCommand, TrayModel,
    };

    #[test]
    fn win32_messages_translate_to_their_product_inputs() {
        // Registered TaskbarCreated ids live in 0xC000..=0xFFFF, so this cannot collide with any
        // constant the translator matches on.
        let taskbar_created = 0xC123_u32;
        let cases: &[(u32, usize, isize, TranslatedMessage)] = &[
            (
                taskbar_created,
                0,
                0,
                TranslatedMessage::Input(TrayInput::TaskbarCreated),
            ),
            (
                WM_DISPLAYCHANGE,
                0,
                0,
                TranslatedMessage::Input(TrayInput::DisplayChanged),
            ),
            (
                WM_SETTINGCHANGE,
                SPI_SETWORKAREA.0 as usize,
                0,
                TranslatedMessage::Input(TrayInput::DisplaySettingChanged),
            ),
            (
                WM_SETTINGCHANGE,
                SPI_SETLOGICALDPIOVERRIDE.0 as usize,
                0,
                TranslatedMessage::Input(TrayInput::DisplaySettingChanged),
            ),
            // Any other setting change is none of the tray's business.
            (WM_SETTINGCHANGE, 0x9999, 0, TranslatedMessage::Unhandled),
            (
                TRAY_CALLBACK,
                0,
                WM_LBUTTONDBLCLK as isize,
                TranslatedMessage::Input(TrayInput::IconDoubleClick),
            ),
            (
                TRAY_CALLBACK,
                0,
                WM_RBUTTONUP as isize,
                TranslatedMessage::Input(TrayInput::IconContextMenu),
            ),
            (
                TRAY_CALLBACK,
                0,
                WM_CONTEXTMENU as isize,
                TranslatedMessage::Input(TrayInput::IconContextMenu),
            ),
            // Hover and move notifications are acknowledged without a product decision.
            (TRAY_CALLBACK, 0, 0x0200, TranslatedMessage::Consumed),
            (
                WM_COMMAND,
                COMMAND_CAPTURE,
                0,
                TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::Capture)),
            ),
            (
                WM_COMMAND,
                COMMAND_PAUSE,
                0,
                TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::Pause)),
            ),
            (
                WM_COMMAND,
                COMMAND_CONFIG,
                0,
                TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::OpenConfig)),
            ),
            (
                WM_COMMAND,
                COMMAND_LOGS,
                0,
                TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::OpenLogs)),
            ),
            (
                WM_COMMAND,
                COMMAND_STARTUP,
                0,
                TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::ToggleStartup)),
            ),
            (
                WM_COMMAND,
                COMMAND_EXIT,
                0,
                TranslatedMessage::Input(TrayInput::Menu(TrayMenuCommand::Exit)),
            ),
            (WM_COMMAND, 9_999, 0, TranslatedMessage::Consumed),
            (
                TRAY_SET_STARTUP,
                1,
                0,
                TranslatedMessage::Input(TrayInput::StartupStateChanged(true)),
            ),
            (
                TRAY_SET_STARTUP,
                0,
                0,
                TranslatedMessage::Input(TrayInput::StartupStateChanged(false)),
            ),
            (
                TRAY_SHOW_ERROR,
                0,
                0,
                TranslatedMessage::Input(TrayInput::NotificationsPosted),
            ),
            (
                WM_QUERYENDSESSION,
                0,
                0,
                TranslatedMessage::Input(TrayInput::QueryEndSession),
            ),
            (
                WM_ENDSESSION,
                1,
                0,
                TranslatedMessage::Input(TrayInput::EndSession { committed: true }),
            ),
            (
                WM_ENDSESSION,
                0,
                0,
                TranslatedMessage::Input(TrayInput::EndSession { committed: false }),
            ),
            (
                WM_CLOSE,
                0,
                0,
                TranslatedMessage::Input(TrayInput::CloseRequested),
            ),
            (
                WM_DESTROY,
                0,
                0,
                TranslatedMessage::Input(TrayInput::Destroyed),
            ),
            (WM_APP + 7, 0, 0, TranslatedMessage::Unhandled),
        ];
        for (message, wparam, lparam, expected) in cases {
            assert_eq!(
                translate_tray_message(taskbar_created, *message, WPARAM(*wparam), LPARAM(*lparam)),
                *expected,
                "message {message:#06x} wparam {wparam:#x} lparam {lparam:#x}"
            );
        }
    }

    #[test]
    fn menu_commands_map_to_their_daemon_events() {
        let cases = [
            (TrayMenuCommand::OpenConfig, TrayEvent::OpenConfig),
            (TrayMenuCommand::OpenLogs, TrayEvent::OpenLogs),
            (TrayMenuCommand::ToggleStartup, TrayEvent::ToggleStartup),
            (TrayMenuCommand::Exit, TrayEvent::Exit),
        ];
        for (command, event) in cases {
            let mut model = TrayModel::new(false);
            let (effects, disposition) = transition(&mut model, TrayInput::Menu(command));
            assert_eq!(effects, vec![TrayEffect::SendEvent(event)], "{command:?}");
            assert_eq!(disposition, MessageDisposition::Handled);
        }
    }

    #[test]
    fn capture_requests_are_swallowed_while_paused() {
        for input in [
            TrayInput::IconDoubleClick,
            TrayInput::Menu(TrayMenuCommand::Capture),
        ] {
            let mut model = TrayModel::new(false);
            let (effects, _) = transition(&mut model, input.clone());
            assert_eq!(effects, vec![TrayEffect::SendEvent(TrayEvent::Capture)]);

            model.paused = true;
            let (effects, _) = transition(&mut model, input.clone());
            assert!(effects.is_empty(), "{input:?} while paused");
        }
    }

    #[test]
    fn pausing_updates_the_tooltip_before_the_daemon_hears_about_it() {
        let mut model = TrayModel::new(false);
        let (effects, _) = transition(&mut model, TrayInput::Menu(TrayMenuCommand::Pause));
        assert!(model.paused);
        assert_eq!(
            effects,
            vec![
                TrayEffect::ModifyTooltip { paused: true },
                TrayEffect::SendEvent(TrayEvent::PausedChanged(true)),
            ]
        );

        let (effects, _) = transition(&mut model, TrayInput::Menu(TrayMenuCommand::Pause));
        assert!(!model.paused);
        assert_eq!(
            effects,
            vec![
                TrayEffect::ModifyTooltip { paused: false },
                TrayEffect::SendEvent(TrayEvent::PausedChanged(false)),
            ]
        );
    }

    #[test]
    fn repeated_session_queries_create_one_block_reason() {
        let mut model = TrayModel::new(false);
        let (effects, disposition) = transition(&mut model, TrayInput::QueryEndSession);
        assert_eq!(effects, vec![TrayEffect::CreateShutdownBlockReason]);
        assert_eq!(disposition, MessageDisposition::AllowSessionEnd);

        let (effects, disposition) = transition(&mut model, TrayInput::QueryEndSession);
        assert!(effects.is_empty());
        assert_eq!(disposition, MessageDisposition::AllowSessionEnd);
        assert_eq!(model.session, SessionPhase::QueryReceived);
    }

    #[test]
    fn a_canceled_session_shutdown_releases_the_block_without_draining() {
        let mut model = TrayModel::new(false);
        let _ = transition(&mut model, TrayInput::QueryEndSession);
        let (effects, _) = transition(&mut model, TrayInput::EndSession { committed: false });
        // No WaitForSessionDrain: the daemon never hears about a shutdown Windows abandoned.
        assert_eq!(effects, vec![TrayEffect::DestroyShutdownBlockReason]);
        assert_eq!(model.session, SessionPhase::Idle);

        // A later query starts a fresh block, exactly as if the canceled attempt never happened.
        let (effects, _) = transition(&mut model, TrayInput::QueryEndSession);
        assert_eq!(effects, vec![TrayEffect::CreateShutdownBlockReason]);
    }

    #[test]
    fn a_committed_session_shutdown_drains_before_releasing_the_block() {
        let mut model = TrayModel::new(false);
        let _ = transition(&mut model, TrayInput::QueryEndSession);
        let (effects, _) = transition(&mut model, TrayInput::EndSession { committed: true });
        assert_eq!(
            effects,
            vec![
                TrayEffect::WaitForSessionDrain,
                TrayEffect::DestroyShutdownBlockReason,
            ]
        );
        assert_eq!(model.session, SessionPhase::Idle);
    }

    #[test]
    fn session_end_without_a_query_still_drains_but_owes_no_block_reason() {
        let mut model = TrayModel::new(false);
        let (effects, _) = transition(&mut model, TrayInput::EndSession { committed: true });
        assert_eq!(effects, vec![TrayEffect::WaitForSessionDrain]);

        let (effects, _) = transition(&mut model, TrayInput::EndSession { committed: false });
        assert!(effects.is_empty());
    }

    #[test]
    fn menu_snapshots_track_state_mutated_while_the_menu_was_open() {
        let mut model = TrayModel::new(true);
        let (effects, _) = transition(&mut model, TrayInput::IconContextMenu);
        assert_eq!(
            effects,
            vec![TrayEffect::ShowMenu {
                paused: false,
                startup_enabled: true,
            }]
        );

        // The modal menu loop dispatches messages reentrantly; a pause toggle and a startup
        // update arriving mid-menu mutate the model without touching the snapshot already handed
        // out, and the next menu reflects them.
        let _ = transition(&mut model, TrayInput::Menu(TrayMenuCommand::Pause));
        let _ = transition(&mut model, TrayInput::StartupStateChanged(false));
        let (effects, _) = transition(&mut model, TrayInput::IconContextMenu);
        assert_eq!(
            effects,
            vec![TrayEffect::ShowMenu {
                paused: true,
                startup_enabled: false,
            }]
        );
    }

    #[test]
    fn taskbar_rebirth_readds_the_icon_from_both_present_and_degraded_states() {
        let cases = [
            (IconState::Present, true, IconState::Present),
            (IconState::Present, false, IconState::Degraded),
            (IconState::Degraded, true, IconState::Present),
            (IconState::Degraded, false, IconState::Degraded),
        ];
        for (initial, restored, expected) in cases {
            let mut model = TrayModel::new(false);
            model.icon = initial;
            model.paused = true;
            let (effects, _) = transition(&mut model, TrayInput::TaskbarCreated);
            assert_eq!(effects, vec![TrayEffect::AddIcon { paused: true }]);

            let (effects, _) = transition(&mut model, TrayInput::IconAddCompleted { restored });
            assert!(effects.is_empty());
            assert_eq!(model.icon, expected, "{initial:?} restored={restored}");
        }
    }

    #[test]
    fn close_requests_exit_without_destroying_the_window() {
        let mut model = TrayModel::new(false);
        let (effects, disposition) = transition(&mut model, TrayInput::CloseRequested);
        assert_eq!(effects, vec![TrayEffect::SendEvent(TrayEvent::Exit)]);
        assert_eq!(disposition, MessageDisposition::Handled);
        assert!(model.exit_requested);

        // Only actual destruction ends the message loop; the daemon owns the WM_QUIT decision.
        let (effects, _) = transition(&mut model, TrayInput::Destroyed);
        assert_eq!(effects, vec![TrayEffect::PostQuit]);
    }

    #[test]
    fn a_dying_window_settles_the_block_reason_it_still_owes() {
        // Destroyed outside the handshake owes nothing.
        let mut model = TrayModel::new(false);
        let (effects, _) = transition(&mut model, TrayInput::Destroyed);
        assert_eq!(effects, vec![TrayEffect::PostQuit]);

        // Destroyed mid-handshake (daemon stop, menu Exit, panic recovery): the destroy that
        // WM_ENDSESSION never got to perform runs before the quit, while the handle is valid.
        let mut model = TrayModel::new(false);
        let _ = transition(&mut model, TrayInput::QueryEndSession);
        let (effects, _) = transition(&mut model, TrayInput::Destroyed);
        assert_eq!(
            effects,
            vec![TrayEffect::DestroyShutdownBlockReason, TrayEffect::PostQuit]
        );
        assert_eq!(model.session, SessionPhase::Idle);
    }

    #[test]
    fn a_settled_session_handshake_is_not_double_destroyed_at_teardown() {
        for committed in [false, true] {
            let mut model = TrayModel::new(false);
            let _ = transition(&mut model, TrayInput::QueryEndSession);
            let _ = transition(&mut model, TrayInput::EndSession { committed });
            let (effects, _) = transition(&mut model, TrayInput::Destroyed);
            assert_eq!(effects, vec![TrayEffect::PostQuit], "committed={committed}");
        }
    }

    #[test]
    fn display_reconfiguration_messages_invalidate_the_capture_topology() {
        let mut model = TrayModel::new(false);
        let (effects, _) = transition(&mut model, TrayInput::DisplayChanged);
        assert_eq!(
            effects,
            vec![TrayEffect::MarkDisplayConfigurationChanged(
                "tray_display_changed"
            )]
        );

        let (effects, _) = transition(&mut model, TrayInput::DisplaySettingChanged);
        assert_eq!(
            effects,
            vec![TrayEffect::MarkDisplayConfigurationChanged(
                "tray_display_setting_changed"
            )]
        );
    }
}
