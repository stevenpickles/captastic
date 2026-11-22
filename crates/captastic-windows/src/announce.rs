//! A notification-area icon that exists for one announcement (ADR 0014).
//!
//! When Captastic is not running, a command-line capture is still announced, by a short-lived
//! `captastic.exe` that shows this icon, raises one notification, shows the file in its folder if
//! the notification is clicked, and removes the icon after a few seconds. It has no menu and takes
//! no other input: it is not Captastic running, only the receipt for a capture.

use std::cell::RefCell;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use captastic_core::CaptureError;
use windows::core::{w, Error as WindowsError, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
    LoadIconW, PostQuitMessage, RegisterClassW, SetTimer, TranslateMessage, UnregisterClassW,
    HMENU, MSG, WM_APP, WM_DESTROY, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

const CLASS_NAME: PCWSTR = w!("CaptasticAnnouncementWindow-v1");
const CALLBACK: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;
const APPLICATION_ICON_RESOURCE_ID: usize = 1;
const LINGER_TIMER: usize = 1;
/// `WM_USER + 5`, as in the tray: the person clicked the notification.
const NIN_BALLOONUSERCLICK: u32 = 0x0405;
/// How long the icon stays after a click, so Explorer has taken the request before it goes.
const AFTER_CLICK: Duration = Duration::from_millis(500);

thread_local! {
    /// The file a click shows. One announcement per thread, set before the window exists.
    static REVEAL: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Starts `desktop` (`captastic.exe`) to announce `file`, and returns without waiting for it.
///
/// It inherits no handles. `std::process::Command` lets a child inherit every inheritable handle,
/// and the caller's standard output is one when a shell or an agent reads it through a pipe: the
/// pipe would then stay open, and the reader waiting, for as long as the announcement lingers.
pub fn start_announcer(desktop: &Path, file: &Path) -> Result<(), CaptureError> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        CreateProcessW, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };
    let application: Vec<u16> = desktop
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // Neither path can contain a quote, which Windows forbids in file names, so quoting each
    // whole is enough. A path never ends in a backslash here: both name files.
    let mut command_line: Vec<u16> = format!(
        "\"{}\" --announce-command-line-capture \"{}\"",
        desktop.display(),
        file.display()
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: Both strings are null-terminated and outlive the call; the command line buffer is
    // writable, as CreateProcessW requires. No handles are inherited.
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            windows::core::PWSTR(command_line.as_mut_ptr()),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            None,
            PCWSTR::null(),
            &startup,
            &mut process,
        )
    }
    .map_err(|error| native("start_announcer", error))?;
    // SAFETY: Closes the handles CreateProcessW returned; the process runs on without them.
    unsafe {
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
    }
    Ok(())
}

/// Shows `title` and `message` from an icon of its own, for `linger`, and shows `file` in its
/// folder if the notification is clicked. Returns when the icon has been removed.
pub fn announce_capture(
    title: &str,
    message: &str,
    file: &Path,
    linger: Duration,
) -> Result<(), CaptureError> {
    REVEAL.with(|reveal| *reveal.borrow_mut() = Some(file.to_path_buf()));
    // SAFETY: No module name asks for the current executable's module.
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| native("get_announcement_module", error))?;
    let instance = HINSTANCE(module.0);
    // SAFETY: Resource ID 1 is the application icon embedded in every Captastic executable.
    let icon = unsafe { LoadIconW(instance, PCWSTR(APPLICATION_ICON_RESOURCE_ID as *const u16)) }
        .map_err(|error| native("load_announcement_icon", error))?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hIcon: icon,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: The class is fully initialised and its procedure and name are static.
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(native(
            "register_announcement_class",
            WindowsError::from_win32(),
        ));
    }
    // SAFETY: The class was just registered. A hidden top-level window owns the icon.
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            CLASS_NAME,
            w!("Captastic"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            HWND(0),
            HMENU(0),
            instance,
            None,
        )
    };
    let result = if hwnd.0 == 0 {
        Err(native(
            "create_announcement_window",
            WindowsError::from_win32(),
        ))
    } else {
        show_and_wait(hwnd, icon, title, message, linger)
    };
    // SAFETY: Unregisters the class this function registered; its window is gone by now.
    let _ = unsafe { UnregisterClassW(CLASS_NAME, instance) };
    REVEAL.with(|reveal| *reveal.borrow_mut() = None);
    result
}

fn show_and_wait(
    hwnd: HWND,
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
    title: &str,
    message: &str,
    linger: Duration,
) -> Result<(), CaptureError> {
    let mut data = icon_data(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_INFO;
    data.hIcon = icon;
    write_wide(&mut data.szTip, "Captastic");
    write_wide(&mut data.szInfoTitle, title);
    write_wide(&mut data.szInfo, message);
    data.dwInfoFlags = NIIF_INFO;
    // SAFETY: data is fully initialised and names the live window that owns the icon.
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        let error = native("add_announcement_icon", WindowsError::from_win32());
        // SAFETY: The window was created by this thread and has no icon to remove.
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(error);
    }
    // SAFETY: The window is live and owned by this thread.
    unsafe { SetTimer(hwnd, LINGER_TIMER, millis(linger), None) };
    let mut message = MSG::default();
    // SAFETY: The loop runs on the thread that owns the window, and ends with WM_QUIT.
    while unsafe { GetMessageW(&mut message, HWND(0), 0, 0) }.0 > 0 {
        // SAFETY: Translates and dispatches a message this thread's queue returned.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        CALLBACK if lparam.0 as u32 == NIN_BALLOONUSERCLICK => {
            if let Some(file) = REVEAL.with(|reveal| reveal.borrow().clone()) {
                if let Err(error) = crate::reveal_in_folder(&file) {
                    log::warn!("the announced capture could not be shown in its folder: {error}");
                }
            }
            // Clicked and answered: nothing is left for the icon to do.
            // SAFETY: Replaces this window's own timer.
            unsafe { SetTimer(hwnd, LINGER_TIMER, millis(AFTER_CLICK), None) };
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == LINGER_TIMER => {
            // SAFETY: The window and its timer belong to this thread.
            unsafe {
                let _ = KillTimer(hwnd, LINGER_TIMER);
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let data = icon_data(hwnd);
            // SAFETY: Best-effort removal of the icon this window owns.
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
            // SAFETY: Ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: Default handling for every other message, with the arguments as received.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        uCallbackMessage: CALLBACK,
        ..Default::default()
    }
}

fn millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis())
        .unwrap_or(u32::MAX)
        .max(1)
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

fn native(operation: &'static str, error: WindowsError) -> CaptureError {
    CaptureError {
        kind: captastic_core::CaptureErrorKind::NativeFailure,
        backend: "windows-announcement",
        operation,
        message: error.to_string(),
        retryable: false,
        native_code: Some(i64::from(error.code().0)),
    }
}
