//! Window enumeration for the overlay's window chooser: which top-level windows are offered
//! as capture targets, and which display owns each. Pure Win32 queries against foreign
//! windows - nothing here touches overlay state, which is what makes this the cleanest seam
//! in the overlay and lets the chooser's eligibility rules be reasoned about in isolation.

use std::panic::{catch_unwind, AssertUnwindSafe};

use captastic_core::{CaptureError, CaptureErrorKind, DisplayId, DisplayInfo, Rect};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetLastActivePopup, GetShellWindow,
    GetWindowDisplayAffinity, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, GA_ROOTOWNER, GWL_EXSTYLE,
    WDA_NONE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use super::{intersect_with_source, overlay_error, NativeWindowHandle};

#[derive(Clone, Copy, Debug)]
pub(super) struct WindowCandidate {
    pub(super) handle: NativeWindowHandle,
}

pub(super) fn enumerate_visible_windows(
    source: Rect,
    target_display_id: &DisplayId,
    displays: Vec<DisplayInfo>,
) -> Result<Vec<WindowCandidate>, CaptureError> {
    let mut collector = WindowCollector {
        source,
        target_display_id: target_display_id.clone(),
        displays,
        windows: Vec::with_capacity(32),
        callback_failed: false,
    };
    // SAFETY: collector remains alive and uniquely borrowed for the synchronous enumeration.
    unsafe {
        EnumWindows(
            Some(enum_window),
            LPARAM((&mut collector as *mut WindowCollector) as isize),
        )
    }
    .map_err(|error| overlay_error("enumerate_windows", error))?;
    if collector.callback_failed {
        return Err(CaptureError {
            kind: CaptureErrorKind::NativeFailure,
            backend: "windows-overlay",
            operation: "enumerate_windows",
            message: "window enumeration callback failed".to_owned(),
            retryable: true,
            native_code: None,
        });
    }
    Ok(collector.windows)
}

struct WindowCollector {
    source: Rect,
    target_display_id: DisplayId,
    displays: Vec<DisplayInfo>,
    windows: Vec<WindowCandidate>,
    callback_failed: bool,
}

unsafe extern "system" fn enum_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: EnumWindows passes back the live WindowCollector pointer supplied by the caller.
    let collector = unsafe { &mut *(lparam.0 as *mut WindowCollector) };
    match catch_unwind(AssertUnwindSafe(|| collect_window(hwnd, collector))) {
        Ok(()) => BOOL(1),
        Err(_) => {
            collector.callback_failed = true;
            BOOL(0)
        }
    }
}

fn collect_window(hwnd: HWND, collector: &mut WindowCollector) {
    // SAFETY: Returns the shell desktop window for identity comparison only.
    if hwnd == unsafe { GetShellWindow() } {
        return;
    }
    // Reject Captastic's own windows before anything queries them. Captastic never offers them as
    // capture targets, and a title query against a same-process window on another thread is a
    // blocking WM_GETTEXT send: while it waits, this thread dispatches its incoming sent messages,
    // which can re-enter the window procedure and re-borrow the overlay state that enumeration
    // already holds mutably further down the stack.
    if is_own_process_window(hwnd) {
        return;
    }
    // Everything below this point is a foreign window, and every filter here reads window state
    // directly instead of sending a message, so none of them can block on another thread.
    // SAFETY: hwnd is a top-level handle supplied synchronously by EnumWindows.
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    // SAFETY: hwnd remains the same live enumerated top-level handle.
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    let class_name = window_class_name(hwnd);
    // SAFETY: Reads immutable extended-style bits from the enumerated window.
    let extended_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let forced_taskbar_window = extended_style & WS_EX_APPWINDOW.0 != 0;
    if !visible
        || minimized
        || is_cloaked_window(hwnd)
        || class_name.as_deref().is_some_and(is_shell_window_class)
        || !window_styles_allow_task_switcher(extended_style)
        || (!forced_taskbar_window && !is_root_owner_task_window(hwnd))
    {
        return;
    }
    // Only already-eligible foreign windows are titled. Across a process boundary GetWindowText
    // copies the cached title instead of sending WM_GETTEXT, so it cannot wait on that window's
    // thread; untitled windows stay excluded exactly as before.
    let Some(title) = window_text(hwnd) else {
        return;
    };
    let display_affinity = window_display_affinity(hwnd);
    if !display_affinity_allows_capture(display_affinity) {
        log_window_candidate(
            hwnd,
            "rejected-protected",
            &title,
            class_name.as_deref(),
            extended_style,
            display_affinity,
        );
        return;
    }
    let mut native = RECT::default();
    // SAFETY: native is writable storage and hwnd is the enumerated top-level window.
    if unsafe { GetWindowRect(hwnd, &mut native) }.is_err() {
        return;
    }
    if let Some(window_bounds) = rect_from_native(native) {
        let native_display_id = native_window_display_id(hwnd, &collector.displays);
        if owning_display_id(
            window_bounds,
            native_display_id.as_ref(),
            &collector.displays,
        ) != Some(&collector.target_display_id)
        {
            return;
        }
        let Some(visible_bounds) = intersect_with_source(native, collector.source) else {
            return;
        };
        if visible_bounds.width >= 16 && visible_bounds.height >= 16 {
            log_window_candidate(
                hwnd,
                "accepted",
                &title,
                class_name.as_deref(),
                extended_style,
                display_affinity,
            );
            collector.windows.push(WindowCandidate {
                handle: NativeWindowHandle(hwnd.0),
            });
        }
    }
}

fn owning_display_id<'a>(
    window_bounds: Rect,
    native_display_id: Option<&DisplayId>,
    displays: &'a [DisplayInfo],
) -> Option<&'a DisplayId> {
    let mut candidates: Vec<(&DisplayInfo, u64)> = displays
        .iter()
        .filter_map(|display| {
            display
                .bounds
                .intersection(window_bounds)
                .map(|intersection| (display, intersection.area()))
        })
        .collect();
    let maximum_area = candidates.iter().map(|(_, area)| *area).max()?;
    candidates.retain(|(_, area)| *area == maximum_area);
    if let Some(native_display_id) = native_display_id {
        if let Some((display, _)) = candidates
            .iter()
            .find(|(display, _)| display.id == *native_display_id)
        {
            return Some(&display.id);
        }
    }
    candidates
        .into_iter()
        .min_by(|(left, _), (right, _)| left.id.0.cmp(&right.id.0))
        .map(|(display, _)| &display.id)
}

fn native_window_display_id(hwnd: HWND, displays: &[DisplayInfo]) -> Option<DisplayId> {
    // SAFETY: hwnd is the live top-level window currently supplied by EnumWindows.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) };
    if monitor.0 == 0 {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..MONITORINFO::default()
    };
    // SAFETY: info has the required size and is writable for this synchronous monitor query.
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    let bounds = rect_from_native(info.rcMonitor)?;
    displays
        .iter()
        .find(|display| display.bounds == bounds)
        .map(|display| display.id.clone())
}

/// Reports whether `hwnd` belongs to this process, so window enumeration can skip it.
///
/// A handle that no longer resolves to a thread counts as ours: it is useless as a capture target
/// and must not be queried further.
fn is_own_process_window(hwnd: HWND) -> bool {
    let mut process_id = 0_u32;
    // SAFETY: process_id is writable storage and hwnd is inspected synchronously.
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if thread_id == 0 {
        return true;
    }
    // SAFETY: GetCurrentProcessId has no preconditions and cannot fail.
    process_id == unsafe { GetCurrentProcessId() }
}

fn is_cloaked_window(hwnd: HWND) -> bool {
    let mut cloaked = 0_u32;
    // SAFETY: cloaked is correctly sized writable storage used only for this synchronous query.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .is_ok()
        && cloaked != 0
}

fn window_text(hwnd: HWND) -> Option<String> {
    // SAFETY: Reads the current title length without retaining the enumerated handle.
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return None;
    }
    let mut title = vec![0_u16; length as usize + 1];
    // SAFETY: title is writable UTF-16 storage and hwnd is inspected synchronously.
    let copied = unsafe { GetWindowTextW(hwnd, &mut title) };
    (copied > 0).then(|| String::from_utf16_lossy(&title[..copied as usize]))
}

fn window_class_name(hwnd: HWND) -> Option<String> {
    let mut class_name = [0_u16; 256];
    // SAFETY: class_name is writable UTF-16 storage and hwnd is used synchronously.
    let length = unsafe { GetClassNameW(hwnd, &mut class_name) };
    if length <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&class_name[..length as usize]))
}

fn window_display_affinity(hwnd: HWND) -> Option<u32> {
    let mut affinity = WDA_NONE.0;
    // SAFETY: affinity is writable storage and hwnd is a live top-level enumeration candidate.
    unsafe { GetWindowDisplayAffinity(hwnd, &mut affinity) }
        .ok()
        .map(|_| affinity)
}

const fn display_affinity_allows_capture(affinity: Option<u32>) -> bool {
    match affinity {
        Some(value) => value == WDA_NONE.0,
        None => true,
    }
}

fn log_window_candidate(
    hwnd: HWND,
    decision: &str,
    title: &str,
    class_name: Option<&str>,
    extended_style: u32,
    display_affinity: Option<u32>,
) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }
    let process = window_process_name(hwnd);
    let affinity = display_affinity
        .map(|value| format!("0x{value:08X}"))
        .unwrap_or_else(|| "unknown".to_owned());
    log::debug!(
        "window chooser candidate decision={decision} handle=0x{:X} title={title:?} class={:?} process={:?} ex_style=0x{extended_style:08X} display_affinity={affinity}",
        hwnd.0,
        class_name.unwrap_or("<unknown>"),
        process.as_deref().unwrap_or("<unknown>"),
    );
}

fn window_process_name(hwnd: HWND) -> Option<String> {
    let mut process_id = 0_u32;
    // SAFETY: process_id is writable storage and hwnd is queried synchronously.
    if unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) } == 0 || process_id == 0 {
        return None;
    }
    // SAFETY: Opens a query-only handle to the owner process without inheriting it.
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let mut path = vec![0_u16; 32_768];
    let mut length = path.len() as u32;
    // SAFETY: path and length are writable storage and process remains live through the call.
    let queried = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    };
    // SAFETY: Closes the query-only process handle exactly once after its last use.
    let _ = unsafe { CloseHandle(process) };
    queried.ok()?;
    let path = String::from_utf16_lossy(&path[..length as usize]);
    std::path::Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

fn is_shell_window_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

fn window_styles_allow_task_switcher(extended_style: u32) -> bool {
    let is_app_window = extended_style & WS_EX_APPWINDOW.0 != 0;
    let is_tool_window = extended_style & WS_EX_TOOLWINDOW.0 != 0;
    let is_no_activate = extended_style & WS_EX_NOACTIVATE.0 != 0;
    !is_no_activate && (is_app_window || !is_tool_window)
}

fn is_root_owner_task_window(hwnd: HWND) -> bool {
    // SAFETY: The enumerated HWND is queried synchronously and no handle is retained.
    let mut candidate = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    if candidate.0 == 0 {
        return true;
    }
    for _ in 0..32 {
        // SAFETY: candidate is a live HWND returned by the preceding Win32 query.
        let popup = unsafe { GetLastActivePopup(candidate) };
        if popup == candidate {
            break;
        }
        // SAFETY: popup is inspected synchronously and not retained.
        if unsafe { IsWindowVisible(popup) }.as_bool() {
            break;
        }
        candidate = popup;
    }
    candidate == hwnd
}

pub(super) fn rect_from_native(native: RECT) -> Option<Rect> {
    let width = i64::from(native.right) - i64::from(native.left);
    let height = i64::from(native.bottom) - i64::from(native.top);
    (width > 0 && height > 0 && width <= i64::from(u32::MAX) && height <= i64::from(u32::MAX))
        .then_some(Rect {
            x: native.left,
            y: native.top,
            width: width as u32,
            height: height as u32,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR};

    #[test]
    fn task_switcher_filters_reject_shell_and_utility_windows() {
        assert!(is_shell_window_class("Progman"));
        assert!(is_shell_window_class("WorkerW"));
        assert!(!is_shell_window_class("Chrome_WidgetWin_1"));
        assert!(!window_styles_allow_task_switcher(WS_EX_TOOLWINDOW.0));
        assert!(!window_styles_allow_task_switcher(WS_EX_NOACTIVATE.0));
        assert!(window_styles_allow_task_switcher(
            WS_EX_TOOLWINDOW.0 | WS_EX_APPWINDOW.0
        ));
        assert!(window_styles_allow_task_switcher(0));
    }

    #[test]
    fn enumeration_rejects_this_process_before_querying_a_window() {
        // A handle that resolves to no thread is treated as ours, so nothing queries it further.
        assert!(is_own_process_window(HWND(0)));
        // SAFETY: Returns the shell desktop window handle for a process comparison only.
        let shell = unsafe { GetShellWindow() };
        if shell.0 != 0 {
            assert!(
                !is_own_process_window(shell),
                "the shell window belongs to another process"
            );
        }
    }

    #[test]
    fn protected_windows_are_not_offered_for_capture() {
        assert!(display_affinity_allows_capture(None));
        assert!(display_affinity_allows_capture(Some(WDA_NONE.0)));
        assert!(!display_affinity_allows_capture(Some(WDA_MONITOR.0)));
        assert!(!display_affinity_allows_capture(Some(
            WDA_EXCLUDEFROMCAPTURE.0
        )));
        assert!(!display_affinity_allows_capture(Some(u32::MAX)));
    }

    #[test]
    fn spanning_window_belongs_only_to_the_display_with_most_visible_area() {
        let displays = ownership_test_displays();
        let bounds = Rect {
            x: 1500,
            y: 100,
            width: 1800,
            height: 900,
        };
        assert_eq!(
            owning_display_id(bounds, Some(&displays[0].id), &displays),
            Some(&displays[1].id)
        );
    }

    #[test]
    fn exact_window_ownership_tie_prefers_native_monitor_then_stable_id() {
        let displays = ownership_test_displays();
        let bounds = Rect {
            x: 1720,
            y: 100,
            width: 400,
            height: 800,
        };
        assert_eq!(
            owning_display_id(bounds, Some(&displays[1].id), &displays),
            Some(&displays[1].id)
        );
        assert_eq!(
            owning_display_id(bounds, None, &displays),
            Some(&displays[0].id)
        );
    }

    fn ownership_test_displays() -> Vec<DisplayInfo> {
        vec![
            DisplayInfo {
                id: DisplayId("display-a".to_owned()),
                name: "Laptop".to_owned(),
                bounds: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                scale_factor: 1.0,
                rotation_degrees: 0,
                is_primary: true,
            },
            DisplayInfo {
                id: DisplayId("display-b".to_owned()),
                name: "External".to_owned(),
                bounds: Rect {
                    x: 1920,
                    y: 0,
                    width: 2560,
                    height: 1440,
                },
                scale_factor: 1.5,
                rotation_degrees: 0,
                is_primary: false,
            },
        ]
    }
}
