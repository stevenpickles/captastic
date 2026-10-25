//! The recording indicator: a red border around the area being recorded, and a bar beside it with
//! the elapsed time and a **Stop** button (ADR 0011).
//!
//! One layered window holds both, on the `captastic-desktop-ui` thread. It is excluded from capture
//! (`WDA_EXCLUDEFROMCAPTURE`), which DXGI duplication honours, so it is on the screen for the whole
//! recording and never in it. Everything but the bar is fully transparent, so clicks go through the
//! border and the recorded area to whatever is beneath; the bar takes clicks for its button. It
//! never takes focus: a recording of a window being typed into must not steal the typing.
//!
//! A window recorded through Windows Graphics Capture is recorded wherever it goes, so its
//! indicator follows it (ADR 0013): the window's frame is read again at the recording's rate and the
//! indicator laid out afresh around it whenever it has moved or changed size.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use captastic_core::{CaptureError, Rect};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{GdiFlush, GetDC, ReleaseDC, AC_SRC_ALPHA, BLENDFUNCTION};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, KillTimer, LoadCursorW, SetCursor, SetTimer,
    SetWindowDisplayAffinity, ShowWindow, UpdateLayeredWindow, HTCLIENT, HTTRANSPARENT, IDC_ARROW,
    IDC_HAND, SW_SHOWNOACTIVATE, ULW_ALPHA, WDA_EXCLUDEFROMCAPTURE, WM_DESTROY, WM_LBUTTONDOWN,
    WM_MOUSEMOVE, WM_NCDESTROY, WM_NCHITTEST, WM_SETCURSOR, WM_TIMER, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use super::desktop_ui::{desktop_error, forget_window, release_window_state, window_state};
use super::layout::{UiMetrics, UiRect};
use super::raster::{draw_round_box, draw_text, rgb, TextAlignment};
use super::shell::{query_display_environment, FrozenSurface};
use super::vector::{bgr, Painter};
use crate::window_capture::visible_window_frame;
use crate::NativeWindowHandle;

pub(super) const RECORDING_FRAME_CLASS_NAME: PCWSTR = w!("CaptasticRecordingFrame-v1");
const RECORDING_FRAME_WINDOW_NAME: PCWSTR = w!("Captastic recording");
/// Sent once the pointer leaves a window that asked with TrackMouseEvent.
const WM_MOUSELEAVE: u32 = 0x02A3;

const BORDER_DIP: i32 = 3;
const BAR_SIZE_DIP: (i32, i32) = (256, 36);
const BAR_GAP_DIP: i32 = 8;
const BUTTON_SIZE_DIP: (i32, i32) = (72, 28);
const TICK_ID: usize = 1;
const TICK_MS: u32 = 250;
const FOLLOW_ID: usize = 2;
/// How often a followed window's frame is read: the faster of the two recording rates.
const FOLLOW_MS: u32 = 33;
/// The border and the dot: a recording red, not the accent blue of a selection.
const RECORD_RED: (u8, u8, u8) = (232, 64, 56);

/// Where the border and the bar go, in the window's own coordinates, and where the window goes on
/// the desktop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameGeometry {
    /// The window, on the desktop.
    window: Rect,
    /// The recorded area, inside the window.
    area: UiRect,
    /// The border's outer edge, inside the window.
    ring: UiRect,
    bar: UiRect,
    stop: UiRect,
}

/// Lays the indicator out around `area`, on the work area `work` (both in desktop coordinates).
/// The bar goes below the area if there is room, above it if not, and inside its bottom edge if
/// neither, so **Stop** is always on the screen.
fn frame_geometry(area: Rect, work: UiRect, metrics: UiMetrics) -> FrameGeometry {
    let border = metrics.px(BORDER_DIP).max(1);
    let (bar_width, bar_height) = (metrics.px(BAR_SIZE_DIP.0), metrics.px(BAR_SIZE_DIP.1));
    let gap = metrics.px(BAR_GAP_DIP);
    let area_right = area.right() as i32;
    let area_bottom = area.bottom() as i32;
    let ring = UiRect {
        left: area.x - border,
        top: area.y - border,
        right: area_right + border,
        bottom: area_bottom + border,
    };
    let bar_left = ((area.x + area_right - bar_width) / 2)
        .clamp(work.left, (work.right - bar_width).max(work.left));
    let below = ring.bottom + gap;
    let above = ring.top - gap - bar_height;
    let bar_top = if below + bar_height <= work.bottom {
        below
    } else if above >= work.top {
        above
    } else {
        (area_bottom - gap - bar_height).max(work.top)
    };
    let bar = UiRect {
        left: bar_left,
        top: bar_top,
        right: bar_left + bar_width,
        bottom: bar_top + bar_height,
    };
    let window = UiRect {
        left: ring.left.min(bar.left),
        top: ring.top.min(bar.top),
        right: ring.right.max(bar.right),
        bottom: ring.bottom.max(bar.bottom),
    };
    let local = |rect: UiRect| UiRect {
        left: rect.left - window.left,
        top: rect.top - window.top,
        right: rect.right - window.left,
        bottom: rect.bottom - window.top,
    };
    let bar = local(bar);
    let (button_width, button_height) =
        (metrics.px(BUTTON_SIZE_DIP.0), metrics.px(BUTTON_SIZE_DIP.1));
    let button_left = bar.right - metrics.px(4) - button_width;
    let button_top = bar.top + (bar.bottom - bar.top - button_height) / 2;
    FrameGeometry {
        window: Rect {
            x: window.left,
            y: window.top,
            width: (window.right - window.left) as u32,
            height: (window.bottom - window.top) as u32,
        },
        area: local(UiRect {
            left: area.x,
            top: area.y,
            right: area_right,
            bottom: area_bottom,
        }),
        ring: local(ring),
        bar,
        stop: UiRect {
            left: button_left,
            top: button_top,
            right: button_left + button_width,
            bottom: button_top + button_height,
        },
    }
}

/// `elapsed / limit` as minutes and seconds.
fn elapsed_label(elapsed: Duration, limit: Duration) -> String {
    let clock = |duration: Duration| {
        let seconds = duration.as_secs();
        format!("{}:{:02}", seconds / 60, seconds % 60)
    };
    format!("{} / {}", clock(elapsed.min(limit)), clock(limit))
}

struct FrameState {
    /// The area the indicator is around, in desktop coordinates.
    area: Rect,
    /// The window the indicator follows, once the recording records the window itself.
    follow: Option<NativeWindowHandle>,
    geometry: FrameGeometry,
    metrics: UiMetrics,
    surface: FrozenSurface,
    started: Instant,
    limit: Duration,
    stop: Arc<AtomicBool>,
    hovered: bool,
    tracking_leave: bool,
}

/// Shows the indicator around `area`. `stop` is set when **Stop** is pressed; the recording ends
/// itself and then asks for the indicator to be closed.
pub(super) fn create_recording_frame(
    instance: HINSTANCE,
    area: Rect,
    stop: Arc<AtomicBool>,
    limit: Duration,
) -> Result<HWND, CaptureError> {
    let (geometry, metrics) = layout(area);
    let surface = FrozenSurface::empty(geometry.window.width, geometry.window.height)?;
    let state = Box::new(FrameState {
        area,
        follow: None,
        geometry,
        metrics,
        surface,
        started: Instant::now(),
        limit,
        stop,
        hovered: false,
        tracking_leave: false,
    });
    let state_pointer = Box::into_raw(state);
    // SAFETY: A layered top-level window of a class registered on this thread; the state pointer
    // is handed to WM_NCCREATE and reclaimed in WM_NCDESTROY.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            RECORDING_FRAME_CLASS_NAME,
            RECORDING_FRAME_WINDOW_NAME,
            WS_POPUP,
            geometry.window.x,
            geometry.window.y,
            geometry.window.width as i32,
            geometry.window.height as i32,
            None,
            None,
            instance,
            Some(state_pointer.cast()),
        )
    };
    if hwnd.0 == 0 {
        // SAFETY: Creation failed before WM_NCCREATE could take ownership of the state.
        drop(unsafe { Box::from_raw(state_pointer) });
        return Err(desktop_error(
            "create_recording_frame",
            windows::core::Error::from_win32().to_string(),
        ));
    }
    // SAFETY: hwnd is this thread's live window. Excluding it is what keeps the indicator out of
    // the very recording it indicates; a system that refuses leaves it visible in the recording,
    // which is worth a warning but not worth refusing to record.
    if let Err(error) = unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) } {
        log::warn!("the recording border could not be excluded from capture and may appear in the recording: {error}");
    }
    // SAFETY: The window was just created on this thread with this state.
    present(hwnd, unsafe { &*state_pointer });
    // SAFETY: hwnd is live; a null callback posts WM_TIMER to its procedure.
    unsafe {
        SetTimer(hwnd, TICK_ID, TICK_MS, None);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    Ok(hwnd)
}

/// The indicator's layout around `area`, on the display it is on.
fn layout(area: Rect) -> (FrameGeometry, UiMetrics) {
    let environment = query_display_environment(area);
    let metrics = environment.metrics;
    let work = UiRect {
        left: environment.work_area.left + area.x,
        top: environment.work_area.top + area.y,
        right: environment.work_area.right + area.x,
        bottom: environment.work_area.bottom + area.y,
    };
    (frame_geometry(area, work, metrics), metrics)
}

/// Makes the indicator `frame` follow `window` from now on.
pub(super) fn follow_window(frame: HWND, window: NativeWindowHandle) {
    let state_pointer = window_state::<FrameState>(frame, 0, LPARAM(0));
    if state_pointer.is_null() {
        return;
    }
    // SAFETY: frame is this thread's live indicator, whose state lives until WM_NCDESTROY, and
    // nothing here dispatches a message.
    unsafe { (*state_pointer).follow = Some(window) };
    // SAFETY: frame is live; a null callback posts WM_TIMER to its procedure.
    unsafe { SetTimer(frame, FOLLOW_ID, FOLLOW_MS, None) };
}

/// Lays the indicator out around where its window is now, when that has changed. A minimised
/// window leaves it where it was, so **Stop** stays on the screen.
fn follow(hwnd: HWND, state: &mut FrameState) {
    let Some(area) = state.follow.and_then(visible_window_frame) else {
        return;
    };
    if area == state.area {
        return;
    }
    let (geometry, metrics) = layout(area);
    if (geometry.window.width, geometry.window.height)
        != (state.geometry.window.width, state.geometry.window.height)
    {
        match FrozenSurface::empty(geometry.window.width, geometry.window.height) {
            Ok(surface) => state.surface = surface,
            Err(error) => {
                log::warn!("the recording border could not follow its window: {error}");
                return;
            }
        }
    }
    state.area = area;
    state.geometry = geometry;
    state.metrics = metrics;
    present(hwnd, state);
}

pub(super) unsafe extern "system" fn recording_frame_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_pointer = window_state::<FrameState>(hwnd, message, lparam);
    if state_pointer.is_null() {
        // SAFETY: Forwards a message that arrived before WM_NCCREATE or after WM_NCDESTROY.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: The state lives until WM_NCDESTROY on this thread, and nothing below dispatches a
    // message that could reach it while this borrow is held.
    let state = unsafe { &mut *state_pointer };
    match message {
        WM_NCHITTEST => {
            let point = screen_to_local(state, lparam);
            if state.geometry.bar.contains(point) {
                LRESULT(HTCLIENT as isize)
            } else {
                // The border and the area belong to whatever is beneath them.
                LRESULT(HTTRANSPARENT as isize)
            }
        }
        WM_SETCURSOR => {
            let cursor = if state.hovered && !state.stop.load(Ordering::Acquire) {
                IDC_HAND
            } else {
                IDC_ARROW
            };
            // SAFETY: Shared system cursors need no destruction.
            if let Ok(cursor) = unsafe { LoadCursorW(None, cursor) } {
                // SAFETY: cursor is a live shared system cursor.
                unsafe { SetCursor(cursor) };
            }
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            if !state.tracking_leave {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: track is initialized with its size and names this live window.
                state.tracking_leave = unsafe { TrackMouseEvent(&mut track) }.is_ok();
            }
            let hovered = state.geometry.stop.contains(client_point(lparam));
            if hovered != state.hovered {
                state.hovered = hovered;
                present(hwnd, state);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            state.tracking_leave = false;
            if std::mem::take(&mut state.hovered) {
                present(hwnd, state);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if state.geometry.stop.contains(client_point(lparam)) {
                state.stop.store(true, Ordering::Release);
                present(hwnd, state);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == TICK_ID => {
            present(hwnd, state);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == FOLLOW_ID => {
            follow(hwnd, state);
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: hwnd is this thread's live window; a missing timer is a harmless failure.
            let _ = unsafe { KillTimer(hwnd, TICK_ID) };
            // SAFETY: As above.
            let _ = unsafe { KillTimer(hwnd, FOLLOW_ID) };
            forget_window(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            drop(release_window_state::<FrameState>(hwnd));
            LRESULT(0)
        }
        // SAFETY: Forwards an unhandled message with the original arguments.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn client_point(lparam: LPARAM) -> POINT {
    let packed = lparam.0 as u32;
    POINT {
        x: i32::from(packed as u16 as i16),
        y: i32::from((packed >> 16) as u16 as i16),
    }
}

fn screen_to_local(state: &FrameState, lparam: LPARAM) -> POINT {
    let screen = client_point(lparam);
    POINT {
        x: screen.x - state.geometry.window.x,
        y: screen.y - state.geometry.window.y,
    }
}

/// Draws the indicator as it stands and puts it on the screen.
fn present(hwnd: HWND, state: &FrameState) {
    compose(state);
    let size = SIZE {
        cx: state.surface.width,
        cy: state.surface.height,
    };
    let origin = POINT {
        x: state.geometry.window.x,
        y: state.geometry.window.y,
    };
    let source = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: 0,
        BlendFlags: 0,
        SourceConstantAlpha: u8::MAX,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // SAFETY: A null HWND obtains the screen DC, released below on this thread.
    let screen = unsafe { GetDC(None) };
    // SAFETY: hwnd is this thread's live layered window, and every pointer argument outlives the
    // call.
    let result = unsafe {
        UpdateLayeredWindow(
            hwnd,
            screen,
            Some(&origin),
            Some(&size),
            state.surface.device,
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    };
    // SAFETY: Balances the GetDC above.
    unsafe { ReleaseDC(None, screen) };
    if let Err(error) = result {
        log::warn!("could not draw the recording border: {error}");
    }
}

/// Paints the indicator into the surface: transparent everywhere but the border and the bar.
fn compose(state: &FrameState) {
    let surface = &state.surface;
    let geometry = state.geometry;
    let metrics = state.metrics;
    let tokens = metrics.toolbar_tokens();
    // SAFETY: The surface's DIB holds exactly byte_length writable bytes, owned by this thread.
    let pixels = unsafe { std::slice::from_raw_parts_mut(surface.bits, surface.byte_length) };
    // Every pixel starts black with full alpha. GDI writes zero into the alpha byte of every pixel
    // it draws, so once it is done, full alpha means "untouched" and is made transparent, and zero
    // means "drawn" and is made opaque.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0, 0, 0, u8::MAX]);
    }
    let stopping = state.stop.load(Ordering::Acquire);
    draw_round_box(
        surface.device,
        geometry.bar,
        rgb(31, 31, 34),
        rgb(92, 92, 98),
        tokens.control_corner_radius,
    );
    let dot = metrics.px(10);
    let text = UiRect {
        left: geometry.bar.left + metrics.px(12) + dot + metrics.px(8),
        top: geometry.bar.top,
        right: geometry.stop.left - metrics.px(4),
        bottom: geometry.bar.bottom,
    };
    let label = if stopping {
        "Saving\u{2026}".to_owned()
    } else {
        elapsed_label(state.started.elapsed(), state.limit)
    };
    draw_text(
        surface.device,
        text,
        &label,
        rgb(245, 245, 247),
        TextAlignment::Left,
        tokens.font_height,
    );
    if !stopping {
        let fill = if state.hovered {
            rgb(232, 64, 56)
        } else {
            rgb(64, 64, 70)
        };
        draw_round_box(
            surface.device,
            geometry.stop,
            fill,
            fill,
            tokens.control_corner_radius,
        );
        draw_text(
            surface.device,
            UiRect {
                left: geometry.stop.left + metrics.px(14),
                ..geometry.stop
            },
            "Stop",
            rgb(245, 245, 247),
            TextAlignment::Center,
            tokens.font_height,
        );
    }
    // SAFETY: Completes every queued GDI draw before the pixels are touched directly.
    let _ = unsafe { GdiFlush() };
    for pixel in pixels.chunks_exact_mut(4) {
        if pixel[3] == u8::MAX {
            pixel.copy_from_slice(&[0, 0, 0, 0]);
        } else {
            pixel[3] = u8::MAX;
        }
    }
    let width = surface.width as usize;
    let red = [RECORD_RED.2, RECORD_RED.1, RECORD_RED.0, u8::MAX];
    // The border: the ring between its outer edge and the recorded area.
    for y in geometry.ring.top.max(0)..geometry.ring.bottom.min(surface.height) {
        for x in geometry.ring.left.max(0)..geometry.ring.right.min(surface.width) {
            let inside = x >= geometry.area.left
                && x < geometry.area.right
                && y >= geometry.area.top
                && y < geometry.area.bottom;
            if !inside {
                let at = (y as usize * width + x as usize) * 4;
                pixels[at..at + 4].copy_from_slice(&red);
            }
        }
    }
    let mut painter = Painter::new(pixels, width);
    // The recording dot, steady while recording and grey while the file is finished.
    let centre = (
        (geometry.bar.left + metrics.px(12)) as f32 + dot as f32 / 2.0,
        (geometry.bar.top + geometry.bar.bottom) as f32 / 2.0,
    );
    let dot_colour = if stopping {
        bgr(140, 140, 146)
    } else {
        bgr(RECORD_RED.0, RECORD_RED.1, RECORD_RED.2)
    };
    painter.circle(centre, dot as f32 / 2.0, dot_colour, 1.0);
    if !stopping {
        // A stop square before the button's label.
        let side = metrics.px(9) as f32;
        let left = (geometry.stop.left + metrics.px(10)) as f32;
        let top = (geometry.stop.top + geometry.stop.bottom) as f32 / 2.0 - side / 2.0;
        painter.polygon(
            &[
                (left, top),
                (left + side, top),
                (left + side, top + side),
                (left, top + side),
            ],
            bgr(245, 245, 247),
            1.0,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    const WORK: UiRect = UiRect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };

    #[test]
    fn the_bar_sits_below_the_area_when_there_is_room() {
        let metrics = UiMetrics::new(96);
        let geometry = frame_geometry(rect(400, 300, 800, 400), WORK, metrics);
        // The border surrounds the area exactly, three pixels wide.
        assert_eq!(geometry.area.left - geometry.ring.left, 3);
        assert_eq!(geometry.ring.bottom - geometry.area.bottom, 3);
        assert!(geometry.bar.top >= geometry.ring.bottom);
        // Centred under the area, and the window holds both.
        let bar_centre = geometry.window.x + (geometry.bar.left + geometry.bar.right) / 2;
        assert_eq!(bar_centre, 800);
        assert!(geometry.bar.right <= geometry.window.width as i32);
        assert!(geometry.bar.bottom <= geometry.window.height as i32);
        // Stop is inside the bar.
        assert!(
            geometry.stop.left >= geometry.bar.left && geometry.stop.right <= geometry.bar.right
        );
    }

    #[test]
    fn the_bar_moves_above_or_inside_when_the_area_reaches_the_screen_edge() {
        let metrics = UiMetrics::new(96);
        let near_bottom = frame_geometry(rect(400, 700, 800, 330), WORK, metrics);
        assert!(near_bottom.bar.bottom <= near_bottom.ring.top);
        let whole = frame_geometry(rect(0, 0, 1920, 1040), WORK, metrics);
        // The whole work area: the bar goes inside it, still on the screen.
        let bar_top = whole.window.y + whole.bar.top;
        let bar_bottom = whole.window.y + whole.bar.bottom;
        assert!(
            bar_top >= WORK.top && bar_bottom <= WORK.bottom,
            "{whole:?}"
        );
    }

    #[test]
    fn the_bar_scales_with_the_display() {
        let small = frame_geometry(rect(100, 100, 400, 300), WORK, UiMetrics::new(96));
        let large = frame_geometry(rect(100, 100, 400, 300), WORK, UiMetrics::new(192));
        assert_eq!(
            (large.bar.right - large.bar.left),
            (small.bar.right - small.bar.left) * 2
        );
    }

    #[test]
    fn elapsed_time_reads_as_minutes_and_seconds_against_the_limit() {
        assert_eq!(
            elapsed_label(Duration::from_secs(12), Duration::from_secs(60)),
            "0:12 / 1:00"
        );
        assert_eq!(
            elapsed_label(Duration::from_secs(605), Duration::from_secs(600)),
            "10:00 / 10:00"
        );
    }
}
