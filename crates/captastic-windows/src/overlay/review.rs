//! The review window: a finished recording, played back and trimmed before it is saved
//! (ADR 0012).
//!
//! Like the editor, a review lives on the `captastic-desktop-ui` thread and answers exactly once,
//! with a [`ReviewOutcome`]. It reads the recording back from its draft file through a
//! [`RecordingFrames`] reader, one frame at a time, so a long recording costs no more memory than
//! a short one. Nothing here writes a file: **Save** hands back the [`RecordingEdit`] to save
//! with, and the reader is closed before it does, so the draft can be moved or replaced.
//!
//! Trimming is to the millisecond. The timeline under the picture has a handle for each end of
//! the span kept and a playhead; the keyboard moves the playhead by a frame interval, a second,
//! or to the next change on screen, and **I** and **O** put the start or the end where it is.

use std::path::Path;
use std::time::{Duration, Instant};

use captastic_config::RecordingFormat;
use captastic_core::{
    CaptureError, GifReader, PlaybackError, RecordingEdit, RecordingFrames, Rect, Timeline,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, BitBlt, EndPaint, PAINTSTRUCT, SRCCOPY};
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN,
    VK_RIGHT, VK_SPACE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, KillTimer, LoadCursorW,
    MessageBoxW, SetCursor, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, HTCLIENT,
    IDC_ARROW, IDC_SIZEWE, IDOK, MB_ICONWARNING, MB_OKCANCEL, MINMAXINFO, SWP_NOACTIVATE,
    SWP_NOZORDER, SW_SHOW, WM_CAPTURECHANGED, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND,
    WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCDESTROY,
    WM_PAINT, WM_SETCURSOR, WM_SIZE, WM_TIMER, WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW,
};

use super::desktop_ui::{desktop_error, forget_window, release_window_state, window_state};
use super::layout::{UiMetrics, UiRect};
use super::raster::{
    draw_round_box, draw_surface_to_rect, draw_text, fill_device_rect, measure_ui_text, rgb,
    TextAlignment,
};
use super::shell::{
    capture_pointer, modifiers, query_display_environment, release_pointer_capture, FrozenSurface,
};

pub(super) const REVIEW_CLASS_NAME: PCWSTR = w!("CaptasticReview-v1");
const REVIEW_WINDOW_NAME: PCWSTR = w!("Review recording - Captastic");
/// Sent once the pointer leaves a window that asked with TrackMouseEvent.
const WM_MOUSELEAVE: u32 = 0x02A3;

const TOOLBAR_HEIGHT_DIP: i32 = 52;
const TOOLBAR_PADDING_DIP: i32 = 8;
const BUTTON_HEIGHT_DIP: i32 = 36;
const TEXT_BUTTON_WIDTH_DIP: i32 = 84;
const BUTTON_GAP_DIP: i32 = 6;
const GROUP_GAP_DIP: i32 = 12;
const TIME_LABEL_WIDTH_DIP: i32 = 190;
const CANVAS_PADDING_DIP: i32 = 16;
const TIMELINE_HEIGHT_DIP: i32 = 84;
const TRACK_INSET_DIP: i32 = 20;
const TRACK_TOP_DIP: i32 = 14;
const TRACK_HEIGHT_DIP: i32 = 30;
const HANDLE_WIDTH_DIP: i32 = 8;
/// How far from a handle a press still takes hold of it.
const GRIP_TOLERANCE_DIP: i32 = 8;
/// Ticks for the changes on screen closer together than this are left out.
const TICK_SPACING_DIP: i32 = 3;
const MINIMUM_CLIENT_DIP: (i32, i32) = (760, 420);
const MAXIMUM_WORK_AREA_SHARE: f64 = 0.9;
/// The shortest span a trim keeps.
const MINIMUM_SPAN: Duration = Duration::from_millis(10);
const PLAYBACK_TIMER: usize = 1;
const PLAYBACK_TICK_MS: u32 = 10;

const CANVAS_RGB: (u8, u8, u8) = (24, 24, 27);
const ACCENT_RGB: (u8, u8, u8) = (45, 125, 246);

/// How a review ended. Reported exactly once per review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// **Save**: save the recording with this edit, which may change nothing.
    Save(RecordingEdit),
    /// **Discard**, confirmed: delete the recording.
    Discard,
    /// Closed with no answer because Captastic is shutting down: keep the recording as it is.
    Closed,
    /// The recording could not be opened for review: keep it as it is.
    Failed(String),
}

/// Called on the desktop UI thread when a review ends.
pub type ReviewFinished = Box<dyn FnOnce(ReviewOutcome) + Send>;

/// The trim, the playhead, and how the keys move them: everything about a review but its window.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Trim {
    duration: Duration,
    start: Duration,
    end: Duration,
    playhead: Duration,
    /// One frame interval at the rate the recording was made at.
    interval: Duration,
}

/// How far a key moves the playhead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Frame,
    Second,
    Change,
}

impl Trim {
    fn new(duration: Duration, frame_rate: u32) -> Self {
        Self {
            duration,
            start: Duration::ZERO,
            end: duration,
            playhead: Duration::ZERO,
            interval: Duration::from_secs(1) / frame_rate.max(1),
        }
    }

    fn set_playhead(&mut self, at: Duration) {
        self.playhead = at.min(self.duration);
    }

    fn step(&mut self, forward: bool, step: Step, timeline: &Timeline) {
        let at = self.playhead;
        let next = match (step, forward) {
            (Step::Frame, true) => at + self.interval,
            (Step::Frame, false) => at.saturating_sub(self.interval),
            (Step::Second, true) => at + Duration::from_secs(1),
            (Step::Second, false) => at.saturating_sub(Duration::from_secs(1)),
            (Step::Change, true) => timeline.next_change(at),
            (Step::Change, false) => timeline.previous_change(at),
        };
        self.set_playhead(next);
    }

    /// Moves the start of the span kept to `at`, leaving at least [`MINIMUM_SPAN`] before the end.
    fn set_start(&mut self, at: Duration) {
        self.start = at.min(self.end.saturating_sub(MINIMUM_SPAN));
    }

    /// Moves the end of the span kept to `at`, leaving at least [`MINIMUM_SPAN`] after the start.
    fn set_end(&mut self, at: Duration) {
        self.end = at.clamp(self.start + MINIMUM_SPAN, self.duration);
    }

    fn edit(&self) -> RecordingEdit {
        RecordingEdit {
            start: self.start,
            end: self.end,
            crop: None,
        }
    }
}

/// `m:ss.mmm`: to the millisecond, which is how finely a trim can be placed.
fn clock(at: Duration) -> String {
    let millis = at.as_millis();
    format!(
        "{}:{:02}.{:03}",
        millis / 60_000,
        (millis / 1_000) % 60,
        millis % 1_000
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReviewButton {
    Play,
    Discard,
    Save,
}

impl ReviewButton {
    const fn tooltip(self) -> &'static str {
        match self {
            Self::Play => "Play or pause (Space)",
            Self::Discard => "Delete this recording (Esc)",
            Self::Save => "Save the span kept (Enter)",
        }
    }
}

/// Where everything in a review's client area is, for one client size and DPI.
#[derive(Clone, Debug, PartialEq)]
struct ReviewLayout {
    client: UiRect,
    toolbar: UiRect,
    buttons: [(ReviewButton, UiRect); 3],
    time_label: UiRect,
    span_label: UiRect,
    image: UiRect,
    timeline: UiRect,
    track: UiRect,
    hint: UiRect,
}

impl ReviewLayout {
    fn new(client: UiRect, metrics: UiMetrics, image_width: u32, image_height: u32) -> Self {
        let toolbar = UiRect {
            bottom: client.top + metrics.px(TOOLBAR_HEIGHT_DIP),
            ..client
        };
        let button_height = metrics.px(BUTTON_HEIGHT_DIP);
        let top = toolbar.top + (toolbar.bottom - toolbar.top - button_height) / 2;
        let bottom = top + button_height;
        let button = |left: i32| UiRect {
            left,
            top,
            right: left + metrics.px(TEXT_BUTTON_WIDTH_DIP),
            bottom,
        };
        let play = button(client.left + metrics.px(TOOLBAR_PADDING_DIP));
        let save = button(client.right - metrics.px(TOOLBAR_PADDING_DIP + TEXT_BUTTON_WIDTH_DIP));
        let discard = button(save.left - metrics.px(BUTTON_GAP_DIP + TEXT_BUTTON_WIDTH_DIP));
        let time_label = UiRect {
            left: play.right + metrics.px(GROUP_GAP_DIP),
            top,
            right: play.right + metrics.px(GROUP_GAP_DIP + TIME_LABEL_WIDTH_DIP),
            bottom,
        };
        let span_label = UiRect {
            left: time_label.right + metrics.px(GROUP_GAP_DIP),
            top,
            right: (discard.left - metrics.px(GROUP_GAP_DIP))
                .max(time_label.right + metrics.px(GROUP_GAP_DIP)),
            bottom,
        };
        let timeline = UiRect {
            top: (client.bottom - metrics.px(TIMELINE_HEIGHT_DIP)).max(toolbar.bottom),
            ..client
        };
        let inset = metrics.px(TRACK_INSET_DIP);
        let track = UiRect {
            left: timeline.left + inset,
            top: timeline.top + metrics.px(TRACK_TOP_DIP),
            right: (timeline.right - inset).max(timeline.left + inset + 1),
            bottom: timeline.top + metrics.px(TRACK_TOP_DIP + TRACK_HEIGHT_DIP),
        };
        let hint = UiRect {
            left: track.left,
            top: track.bottom + metrics.px(6),
            right: track.right,
            bottom: timeline.bottom - metrics.px(4),
        };
        let canvas = UiRect {
            left: client.left,
            top: toolbar.bottom,
            right: client.right,
            bottom: timeline.top,
        };
        let padding = metrics.px(CANVAS_PADDING_DIP);
        let available_width = f64::from((canvas.right - canvas.left - padding * 2).max(1));
        let available_height = f64::from((canvas.bottom - canvas.top - padding * 2).max(1));
        let scale = (available_width / f64::from(image_width.max(1)))
            .min(available_height / f64::from(image_height.max(1)))
            .min(1.0);
        let width = ((f64::from(image_width) * scale).round() as i32).max(1);
        let height = ((f64::from(image_height) * scale).round() as i32).max(1);
        let left = canvas.left + (canvas.right - canvas.left - width) / 2;
        let image_top = canvas.top + (canvas.bottom - canvas.top - height) / 2;
        Self {
            client,
            toolbar,
            buttons: [
                (ReviewButton::Play, play),
                (ReviewButton::Discard, discard),
                (ReviewButton::Save, save),
            ],
            time_label,
            span_label,
            image: UiRect {
                left,
                top: image_top,
                right: left + width,
                bottom: image_top + height,
            },
            timeline,
            track,
            hint,
        }
    }

    fn button_at(&self, point: POINT) -> Option<ReviewButton> {
        self.buttons
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|(button, _)| *button)
    }

    fn button_rect(&self, wanted: ReviewButton) -> UiRect {
        self.buttons
            .iter()
            .find(|(button, _)| *button == wanted)
            .map_or(self.toolbar, |(_, rect)| *rect)
    }

    /// Where on the track a time is.
    fn x_at(&self, at: Duration, duration: Duration) -> i32 {
        let share = at.as_secs_f64() / duration.as_secs_f64().max(f64::EPSILON);
        self.track.left + (share.clamp(0.0, 1.0) * f64::from(self.track.width())).round() as i32
    }

    /// The time at a point on the track, to the millisecond.
    fn time_at(&self, x: i32, duration: Duration) -> Duration {
        let share = f64::from(x - self.track.left) / f64::from(self.track.width().max(1));
        let millis = (share.clamp(0.0, 1.0) * duration.as_secs_f64() * 1_000.0).round();
        Duration::from_millis(millis as u64).min(duration)
    }
}

/// What a press on the timeline took hold of.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Drag {
    Start,
    End,
    Playhead,
}

/// The part of the timeline under `point`: a handle when the press is near enough to one, the
/// nearer when both are, and otherwise the playhead.
fn grip_at(layout: &ReviewLayout, trim: &Trim, point: POINT, tolerance: i32) -> Option<Drag> {
    let area = UiRect {
        top: layout.track.top - tolerance,
        bottom: layout.track.bottom + tolerance,
        left: layout.track.left - tolerance,
        right: layout.track.right + tolerance,
    };
    if !area.contains(point) {
        return None;
    }
    let start = (point.x - layout.x_at(trim.start, trim.duration)).abs();
    let end = (point.x - layout.x_at(trim.end, trim.duration)).abs();
    if start.min(end) > tolerance {
        Some(Drag::Playhead)
    } else if start <= end {
        Some(Drag::Start)
    } else {
        Some(Drag::End)
    }
}

/// One review's state, owned by its window.
struct ReviewState {
    /// Taken out and dropped before the answer is given, so the draft is free to move.
    reader: Option<Box<dyn RecordingFrames>>,
    timeline: Timeline,
    size: (u32, u32),
    trim: Trim,
    metrics: UiMetrics,
    layout: ReviewLayout,
    picture: FrozenSurface,
    shown: Option<usize>,
    back_buffer: Option<FrozenSurface>,
    /// Since when, and from where, the recording is playing.
    playing: Option<(Instant, Duration)>,
    drag: Option<Drag>,
    pressed: Option<ReviewButton>,
    hovered: Option<ReviewButton>,
    tracking_leave: bool,
    on_finish: Option<ReviewFinished>,
}

impl ReviewState {
    /// Puts the frame on screen at the playhead into the picture, if it is not there already.
    fn show_playhead(&mut self) {
        let index = self.timeline.index_at(self.trim.playhead);
        if self.shown == Some(index) {
            return;
        }
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        match reader.frame(index) {
            Ok(pixels) => {
                if let Err(error) = self.picture.write_pixels(pixels) {
                    log::warn!("could not show frame {index} of the recording: {error}");
                }
            }
            Err(error) => log::warn!("could not read frame {index} of the recording: {error}"),
        }
        self.shown = Some(index);
    }
}

fn open_reader(
    path: &Path,
    format: RecordingFormat,
) -> Result<Box<dyn RecordingFrames>, PlaybackError> {
    match format {
        RecordingFormat::Gif => {
            let bytes = std::fs::read(path)
                .map_err(|error| PlaybackError(format!("{}: {error}", path.display())))?;
            Ok(Box::new(GifReader::new(&bytes)?))
        }
        RecordingFormat::Mp4 => Ok(Box::new(crate::Mp4Reader::open(path)?)),
    }
}

/// Opens a review of the recording at `path`, centred on the display around `at`. A review that
/// cannot be opened reports [`ReviewOutcome::Failed`] before this returns, so `on_finish` is
/// always answered.
pub(super) fn create_review(
    instance: HINSTANCE,
    path: &Path,
    format: RecordingFormat,
    at: Rect,
    on_finish: ReviewFinished,
) -> Option<HWND> {
    match open_review(instance, path, format, at) {
        Ok((hwnd, state_pointer)) => {
            // SAFETY: The window was just created on this thread with this state, and nothing has
            // run on it that could have ended it.
            unsafe { (*state_pointer).on_finish = Some(on_finish) };
            Some(hwnd)
        }
        Err(error) => {
            on_finish(ReviewOutcome::Failed(error.to_string()));
            None
        }
    }
}

fn open_review(
    instance: HINSTANCE,
    path: &Path,
    format: RecordingFormat,
    at: Rect,
) -> Result<(HWND, *mut ReviewState), CaptureError> {
    let reader = open_reader(path, format)
        .map_err(|error| desktop_error("open_recording_for_review", error.to_string()))?;
    let (width, height) = reader.size();
    let timeline = reader.timeline().clone();
    let environment = query_display_environment(at);
    let metrics = environment.metrics;
    let work = UiRect {
        left: environment.work_area.left + at.x,
        top: environment.work_area.top + at.y,
        right: environment.work_area.right + at.x,
        bottom: environment.work_area.bottom + at.y,
    };
    let picture = FrozenSurface::empty(width, height)?;
    let padding = metrics.px(CANVAS_PADDING_DIP) * 2;
    let maximum_width = (f64::from(work.right - work.left) * MAXIMUM_WORK_AREA_SHARE) as i32;
    let maximum_height = (f64::from(work.bottom - work.top) * MAXIMUM_WORK_AREA_SHARE) as i32;
    let client_width = (width as i32)
        .saturating_add(padding)
        .clamp(metrics.px(MINIMUM_CLIENT_DIP.0), maximum_width.max(1));
    let client_height = (height as i32)
        .saturating_add(padding + metrics.px(TOOLBAR_HEIGHT_DIP + TIMELINE_HEIGHT_DIP))
        .clamp(metrics.px(MINIMUM_CLIENT_DIP.1), maximum_height.max(1));
    let mut frame_rect = RECT {
        left: 0,
        top: 0,
        right: client_width,
        bottom: client_height,
    };
    // SAFETY: frame_rect is writable storage for the call. On failure the client size is used as
    // the window size, which only makes the window a little small.
    let _ = unsafe {
        AdjustWindowRectExForDpi(
            &mut frame_rect,
            WS_OVERLAPPEDWINDOW,
            BOOL(0),
            WS_EX_APPWINDOW,
            metrics.dpi,
        )
    };
    let window_width = frame_rect.right - frame_rect.left;
    let window_height = frame_rect.bottom - frame_rect.top;
    let x = (work.left + (work.right - work.left - window_width) / 2).max(work.left);
    let y = (work.top + (work.bottom - work.top - window_height) / 2).max(work.top);
    let client = UiRect {
        left: 0,
        top: 0,
        right: client_width,
        bottom: client_height,
    };
    let mut state = Box::new(ReviewState {
        trim: Trim::new(timeline.duration(), format.frame_rate()),
        timeline,
        size: (width, height),
        reader: Some(reader),
        layout: ReviewLayout::new(client, metrics, width, height),
        metrics,
        picture,
        shown: None,
        back_buffer: None,
        playing: None,
        drag: None,
        pressed: None,
        hovered: None,
        tracking_leave: false,
        on_finish: None,
    });
    state.show_playhead();
    let state_pointer = Box::into_raw(state);
    // SAFETY: A top-level window of a class registered on this thread; the state pointer is handed
    // to WM_NCCREATE and reclaimed in WM_NCDESTROY.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            REVIEW_CLASS_NAME,
            REVIEW_WINDOW_NAME,
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            window_width,
            window_height,
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
            "create_review_window",
            windows::core::Error::from_win32().to_string(),
        ));
    }
    // SAFETY: hwnd is live. A review opens because a recording the user made has ended, so it
    // takes focus: Space, Enter and Escape must reach it straight away.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
    Ok((hwnd, state_pointer))
}

pub(super) unsafe extern "system" fn review_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_pointer = window_state::<ReviewState>(hwnd, message, lparam);
    if state_pointer.is_null() {
        // SAFETY: Forwards a message that arrived before WM_NCCREATE or after WM_NCDESTROY.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    match message {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            // SAFETY: The review's state lives until WM_NCDESTROY on this thread, and painting
            // dispatches nothing that could reach it.
            paint(hwnd, unsafe { &mut *state_pointer });
            LRESULT(0)
        }
        WM_SIZE => {
            // SAFETY: As above.
            relayout(hwnd, unsafe { &mut *state_pointer });
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // SAFETY: As above.
            let state = unsafe { &mut *state_pointer };
            state.metrics = UiMetrics::new((wparam.0 & 0xffff) as u32);
            // SAFETY: WM_DPICHANGED's lparam is the suggested window rectangle for the new DPI.
            let suggested = unsafe { *(lparam.0 as *const RECT) };
            // SAFETY: hwnd is this thread's live review; the resulting WM_SIZE lays it out again.
            let _ = unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            relayout(hwnd, state);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            // SAFETY: As above; read only for the DPI.
            let metrics = unsafe { (*state_pointer).metrics };
            // SAFETY: WM_GETMINMAXINFO's lparam points to a writable MINMAXINFO.
            let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            info.ptMinTrackSize.x = metrics.px(MINIMUM_CLIENT_DIP.0);
            info.ptMinTrackSize.y = metrics.px(MINIMUM_CLIENT_DIP.1);
            LRESULT(0)
        }
        WM_SETCURSOR if (lparam.0 & 0xffff) as u32 == HTCLIENT => {
            // SAFETY: As above.
            set_cursor(hwnd, unsafe { &*state_pointer });
            LRESULT(1)
        }
        WM_MOUSEMOVE => {
            // SAFETY: As above.
            pointer_moved(hwnd, unsafe { &mut *state_pointer }, client_point(lparam));
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            // SAFETY: As above.
            let state = unsafe { &mut *state_pointer };
            state.tracking_leave = false;
            if state.hovered.take().is_some() {
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // SAFETY: As above.
            pointer_pressed(hwnd, unsafe { &mut *state_pointer }, client_point(lparam));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // SAFETY: As above. Released before acting: a button may end the review.
            let pressed = pointer_released(unsafe { &mut *state_pointer }, client_point(lparam));
            invalidate(hwnd);
            if let Some(button) = pressed {
                press_button(hwnd, state_pointer, button);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // SAFETY: As above. Losing the pointer mid-drag ends the drag where it is.
            let state = unsafe { &mut *state_pointer };
            if state.drag.take().is_some() || state.pressed.take().is_some() {
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == PLAYBACK_TIMER => {
            // SAFETY: As above.
            play_on(hwnd, unsafe { &mut *state_pointer });
            LRESULT(0)
        }
        WM_KEYDOWN => {
            key_pressed(hwnd, state_pointer, wparam.0 as u16);
            LRESULT(0)
        }
        WM_CLOSE => {
            request_discard(hwnd, state_pointer);
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: hwnd is this thread's window; a timer it never set is refused harmlessly.
            let _ = unsafe { KillTimer(hwnd, PLAYBACK_TIMER) };
            forget_window(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            if let Some(mut state) = release_window_state::<ReviewState>(hwnd) {
                drop(state.reader.take());
                // Destroyed with no answer given: only shutdown closes a review that way, and a
                // recording is kept when Captastic exits.
                if let Some(on_finish) = state.on_finish.take() {
                    on_finish(ReviewOutcome::Closed);
                }
            }
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

fn invalidate(hwnd: HWND) {
    super::shell::invalidate(hwnd);
}

fn relayout(hwnd: HWND, state: &mut ReviewState) {
    let mut client = RECT::default();
    // SAFETY: hwnd is this thread's live review and client is writable storage.
    if unsafe { GetClientRect(hwnd, &mut client) }.is_err() {
        return;
    }
    state.layout = ReviewLayout::new(
        UiRect {
            left: client.left,
            top: client.top,
            right: client.right,
            bottom: client.bottom,
        },
        state.metrics,
        state.size.0,
        state.size.1,
    );
    invalidate(hwnd);
}

fn set_cursor(hwnd: HWND, state: &ReviewState) {
    let mut point = POINT::default();
    // SAFETY: point is writable storage; the conversion is to this live window's client area.
    let located = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point).is_ok()
            && windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point).as_bool()
    };
    let grip = match state.drag {
        Some(drag) => Some(drag),
        None if located => grip_at(
            &state.layout,
            &state.trim,
            point,
            state.metrics.px(GRIP_TOLERANCE_DIP),
        ),
        None => None,
    };
    let cursor = match grip {
        Some(Drag::Start | Drag::End) => IDC_SIZEWE,
        _ => IDC_ARROW,
    };
    // SAFETY: Each is a shared system cursor that needs no destruction.
    if let Ok(cursor) = unsafe { LoadCursorW(None, cursor) } {
        // SAFETY: cursor is a live shared system cursor.
        unsafe { SetCursor(cursor) };
    }
}

fn pointer_moved(hwnd: HWND, state: &mut ReviewState, point: POINT) {
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
    let hovered = state.layout.button_at(point);
    let mut changed = hovered != state.hovered;
    state.hovered = hovered;
    if let Some(drag) = state.drag {
        drag_to(state, drag, point.x);
        changed = true;
    }
    if changed {
        invalidate(hwnd);
    }
}

/// Moves what `drag` holds to the time at `x`. The picture follows: dragging an end shows the
/// frame that will be the first or the last one kept.
fn drag_to(state: &mut ReviewState, drag: Drag, x: i32) {
    let at = state.layout.time_at(x, state.trim.duration);
    match drag {
        Drag::Start => {
            state.trim.set_start(at);
            state.trim.set_playhead(state.trim.start);
        }
        Drag::End => {
            state.trim.set_end(at);
            state
                .trim
                .set_playhead(state.trim.end.saturating_sub(Duration::from_millis(1)));
        }
        Drag::Playhead => state.trim.set_playhead(at),
    }
    state.show_playhead();
}

fn pointer_pressed(hwnd: HWND, state: &mut ReviewState, point: POINT) {
    if let Some(button) = state.layout.button_at(point) {
        state.pressed = Some(button);
        capture_pointer(hwnd);
        return;
    }
    let tolerance = state.metrics.px(GRIP_TOLERANCE_DIP);
    if let Some(drag) = grip_at(&state.layout, &state.trim, point, tolerance) {
        pause(hwnd, state);
        state.drag = Some(drag);
        drag_to(state, drag, point.x);
        capture_pointer(hwnd);
        invalidate(hwnd);
    }
}

/// Ends a press. Returns the toolbar button to act on, if the press was one and was released over
/// it.
fn pointer_released(state: &mut ReviewState, point: POINT) -> Option<ReviewButton> {
    let pressed = state.pressed.take();
    state.drag = None;
    // Taken before releasing: the release sends WM_CAPTURECHANGED.
    release_pointer_capture();
    pressed.filter(|button| state.layout.button_at(point) == Some(*button))
}

fn press_button(hwnd: HWND, state_pointer: *mut ReviewState, button: ReviewButton) {
    match button {
        ReviewButton::Play => {
            // SAFETY: The review's state lives until WM_NCDESTROY on this thread.
            toggle_playing(hwnd, unsafe { &mut *state_pointer });
        }
        ReviewButton::Discard => request_discard(hwnd, state_pointer),
        ReviewButton::Save => {
            // SAFETY: As above; read and released before the review ends.
            let edit = unsafe { (*state_pointer).trim.edit() };
            finish(hwnd, state_pointer, ReviewOutcome::Save(edit));
        }
    }
}

fn toggle_playing(hwnd: HWND, state: &mut ReviewState) {
    if state.playing.is_some() {
        pause(hwnd, state);
        return;
    }
    // Played from the playhead, or from the start when the playhead is outside the span kept.
    if state.trim.playhead < state.trim.start || state.trim.playhead >= state.trim.end {
        state.trim.set_playhead(state.trim.start);
    }
    state.playing = Some((Instant::now(), state.trim.playhead));
    // SAFETY: hwnd is this thread's live review; the timer is killed when it pauses or closes.
    unsafe { SetTimer(hwnd, PLAYBACK_TIMER, PLAYBACK_TICK_MS, None) };
    invalidate(hwnd);
}

fn pause(hwnd: HWND, state: &mut ReviewState) {
    if state.playing.take().is_some() {
        // SAFETY: hwnd is this thread's live review.
        let _ = unsafe { KillTimer(hwnd, PLAYBACK_TIMER) };
        invalidate(hwnd);
    }
}

/// Advances playback to now, going round to the start of the span kept at its end.
fn play_on(hwnd: HWND, state: &mut ReviewState) {
    let Some((since, from)) = state.playing else {
        return;
    };
    let mut at = from + since.elapsed();
    if at >= state.trim.end {
        at = state.trim.start;
        state.playing = Some((Instant::now(), at));
    }
    state.trim.set_playhead(at);
    state.show_playhead();
    invalidate(hwnd);
}

fn key_pressed(hwnd: HWND, state_pointer: *mut ReviewState, key: u16) {
    let held = modifiers();
    if key == VK_ESCAPE.0 {
        // SAFETY: The review's state lives until WM_NCDESTROY on this thread; the borrow ends
        // before the discard below.
        let dragging = unsafe { (*state_pointer).drag.take().is_some() };
        if dragging {
            release_pointer_capture();
            invalidate(hwnd);
        } else {
            request_discard(hwnd, state_pointer);
        }
        return;
    }
    if key == VK_RETURN.0 {
        press_button(hwnd, state_pointer, ReviewButton::Save);
        return;
    }
    // SAFETY: As above; nothing below ends the review.
    let state = unsafe { &mut *state_pointer };
    if state.drag.is_some() {
        return;
    }
    let step = if held.ctrl {
        Step::Change
    } else if held.shift {
        Step::Second
    } else {
        Step::Frame
    };
    match key {
        _ if key == VK_SPACE.0 => {
            toggle_playing(hwnd, state);
            return;
        }
        _ if key == VK_LEFT.0 => state.trim.step(false, step, &state.timeline),
        _ if key == VK_RIGHT.0 => state.trim.step(true, step, &state.timeline),
        _ if key == VK_HOME.0 => state.trim.set_playhead(Duration::ZERO),
        _ if key == VK_END.0 => state.trim.set_playhead(state.trim.duration),
        0x49 => state.trim.set_start(state.trim.playhead),
        0x4F => state.trim.set_end(state.trim.playhead),
        _ => return,
    }
    pause(hwnd, state);
    state.show_playhead();
    invalidate(hwnd);
}

fn request_discard(hwnd: HWND, state_pointer: *mut ReviewState) {
    // SAFETY: The review's state lives until WM_NCDESTROY on this thread.
    pause(hwnd, unsafe { &mut *state_pointer });
    // SAFETY: hwnd is this thread's live review and both strings are static. The box runs a modal
    // loop; nothing in it can end this review, since only it and shutdown close one, and shutdown
    // waits for this thread.
    let answer = unsafe {
        MessageBoxW(
            hwnd,
            w!("Discard this recording? It will be deleted."),
            w!("Captastic"),
            MB_OKCANCEL | MB_ICONWARNING,
        )
    };
    if answer == IDOK {
        finish(hwnd, state_pointer, ReviewOutcome::Discard);
    }
}

fn finish(hwnd: HWND, state_pointer: *mut ReviewState, outcome: ReviewOutcome) {
    // SAFETY: As above. The reader is closed first, so the draft can be moved or deleted by
    // whoever is told; the answer is taken out so it is given once.
    let on_finish = unsafe {
        drop((*state_pointer).reader.take());
        (*state_pointer).on_finish.take()
    };
    // The window goes before the answer, so whoever is told finds it gone.
    // SAFETY: hwnd is this thread's live review; its state is freed here and not touched after.
    let _ = unsafe { DestroyWindow(hwnd) };
    if let Some(on_finish) = on_finish {
        on_finish(outcome);
    }
}

fn paint(hwnd: HWND, state: &mut ReviewState) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: hwnd is this thread's live review, painting in response to WM_PAINT.
    let device = unsafe { BeginPaint(hwnd, &mut paint) };
    let client = state.layout.client;
    let (width, height) = (client.width(), client.height());
    let reusable = state
        .back_buffer
        .take()
        .filter(|surface| surface.width == width && surface.height == height);
    let back_buffer =
        match reusable.map_or_else(|| FrozenSurface::empty(width as u32, height as u32), Ok) {
            Ok(surface) => surface,
            Err(error) => {
                log::warn!("could not allocate the review's drawing surface: {error}");
                // SAFETY: Balances BeginPaint.
                let _ = unsafe { EndPaint(hwnd, &paint) };
                return;
            }
        };
    compose(state, &back_buffer);
    // SAFETY: Both DCs are live; the back buffer covers the whole client area.
    let _ = unsafe {
        BitBlt(
            device,
            0,
            0,
            width,
            height,
            back_buffer.device,
            0,
            0,
            SRCCOPY,
        )
    };
    // SAFETY: Balances BeginPaint.
    let _ = unsafe { EndPaint(hwnd, &paint) };
    state.back_buffer = Some(back_buffer);
}

const fn native(rect: UiRect) -> RECT {
    RECT {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

fn compose(state: &ReviewState, surface: &FrozenSurface) {
    let device = surface.device;
    let layout = &state.layout;
    let metrics = state.metrics;
    let tokens = metrics.toolbar_tokens();
    fill_device_rect(
        device,
        native(layout.client),
        rgb(CANVAS_RGB.0, CANVAS_RGB.1, CANVAS_RGB.2),
    );
    draw_surface_to_rect(device, &state.picture, layout.image);

    // The toolbar: Play, the playhead's time, the span kept, Discard and Save.
    fill_device_rect(device, native(layout.toolbar), rgb(38, 38, 41));
    fill_device_rect(
        device,
        native(UiRect {
            top: layout.toolbar.bottom - 1,
            ..layout.toolbar
        }),
        rgb(60, 60, 65),
    );
    for (button, bounds) in &layout.buttons {
        let hovered = state.hovered == Some(*button);
        let fill = match button {
            ReviewButton::Save if hovered => Some(rgb(68, 145, 255)),
            ReviewButton::Save => Some(rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2)),
            ReviewButton::Play if state.playing.is_some() => Some(rgb(76, 76, 82)),
            _ if hovered => Some(rgb(58, 58, 63)),
            _ => None,
        };
        if let Some(fill) = fill {
            draw_round_box(device, *bounds, fill, fill, tokens.control_corner_radius);
        }
        let label = match button {
            ReviewButton::Play if state.playing.is_some() => "Pause",
            ReviewButton::Play => "Play",
            ReviewButton::Discard => "Discard",
            ReviewButton::Save => "Save",
        };
        draw_text(
            device,
            *bounds,
            label,
            rgb(245, 245, 247),
            TextAlignment::Center,
            tokens.font_height,
        );
    }
    let trim = &state.trim;
    draw_text(
        device,
        layout.time_label,
        &format!("{} / {}", clock(trim.playhead), clock(trim.duration)),
        rgb(245, 245, 247),
        TextAlignment::Center,
        tokens.font_height,
    );
    // The span kept, as fully as there is room for.
    let length = format_seconds(trim.end - trim.start);
    let room = layout.span_label.right - layout.span_label.left;
    let span = [
        format!(
            "Keep {} \u{2013} {} ({length} s)",
            clock(trim.start),
            clock(trim.end)
        ),
        format!("{} \u{2013} {}", clock(trim.start), clock(trim.end)),
        format!("Keep {length} s"),
    ]
    .into_iter()
    .find(|text| measure_ui_text(device, text, tokens.font_height).cx <= room);
    if let Some(span) = span {
        draw_text(
            device,
            layout.span_label,
            &span,
            rgb(170, 170, 176),
            TextAlignment::Center,
            tokens.font_height,
        );
    }

    draw_timeline(device, state);
    draw_tooltip(device, state);
}

fn format_seconds(span: Duration) -> String {
    let millis = span.as_millis();
    format!("{}.{:03}", millis / 1_000, millis % 1_000)
}

fn draw_timeline(device: windows::Win32::Graphics::Gdi::HDC, state: &ReviewState) {
    let layout = &state.layout;
    let metrics = state.metrics;
    let trim = &state.trim;
    let track = layout.track;
    let duration = trim.duration;
    fill_device_rect(device, native(layout.timeline), rgb(32, 32, 35));
    fill_device_rect(
        device,
        native(UiRect {
            bottom: layout.timeline.top + 1,
            ..layout.timeline
        }),
        rgb(60, 60, 65),
    );
    // The whole recording, with the span kept lit.
    fill_device_rect(device, native(track), rgb(52, 52, 57));
    let kept = UiRect {
        left: layout.x_at(trim.start, duration),
        right: layout.x_at(trim.end, duration),
        ..track
    };
    fill_device_rect(device, native(kept), rgb(36, 74, 140));
    // A tick where the picture changes, where they are far enough apart to be told apart.
    let spacing = metrics.px(TICK_SPACING_DIP).max(2);
    let mut last = track.left;
    let tick_top = track.bottom - (track.bottom - track.top) / 3;
    for index in 1..state.timeline.len() {
        let x = layout.x_at(state.timeline.start(index), duration);
        if x - last < spacing {
            continue;
        }
        last = x;
        fill_device_rect(
            device,
            native(UiRect {
                left: x,
                top: tick_top,
                right: x + 1,
                bottom: track.bottom,
            }),
            rgb(120, 120, 128),
        );
    }
    // The ends of the span kept.
    let handle_half = metrics.px(HANDLE_WIDTH_DIP) / 2;
    let rise = metrics.px(4);
    for x in [kept.left, kept.right] {
        let handle = UiRect {
            left: x - handle_half,
            top: track.top - rise,
            right: x + handle_half,
            bottom: track.bottom + rise,
        };
        draw_round_box(
            device,
            handle,
            rgb(245, 245, 247),
            rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2),
            metrics.px(2),
        );
    }
    // The playhead.
    let x = layout.x_at(trim.playhead, duration);
    let line = metrics.px(2).max(1);
    fill_device_rect(
        device,
        native(UiRect {
            left: x - line / 2,
            top: track.top - rise * 2,
            right: x - line / 2 + line,
            bottom: track.bottom + rise * 2,
        }),
        rgb(253, 216, 53),
    );
    let hint =
        "\u{2190} \u{2192} frame   Shift second   Ctrl change   I / O set start / end   Space play";
    let font = (metrics.toolbar_tokens().font_height * 9 / 10).max(1);
    if measure_ui_text(device, hint, font).cx <= layout.hint.right - layout.hint.left {
        draw_text(
            device,
            layout.hint,
            hint,
            rgb(140, 140, 148),
            TextAlignment::Center,
            font,
        );
    }
}

fn draw_tooltip(device: windows::Win32::Graphics::Gdi::HDC, state: &ReviewState) {
    let Some(button) = state.hovered else { return };
    let text = button.tooltip();
    let target = state.layout.button_rect(button);
    let metrics = state.metrics;
    let tokens = metrics.toolbar_tokens();
    let measured = measure_ui_text(device, text, tokens.font_height);
    let width = measured.cx + tokens.tooltip_padding_x * 2;
    let height = (measured.cy + tokens.tooltip_padding_y * 2).max(metrics.px(28));
    let client = state.layout.client;
    let left = ((target.left + target.right - width) / 2).clamp(
        client.left + metrics.px(4),
        (client.right - width - metrics.px(4)).max(0),
    );
    let top = state.layout.toolbar.bottom + metrics.px(6);
    let bounds = UiRect {
        left,
        top,
        right: left + width,
        bottom: top + height,
    };
    draw_round_box(
        device,
        bounds,
        rgb(31, 31, 34),
        rgb(104, 104, 110),
        tokens.tooltip_corner_radius,
    );
    draw_text(
        device,
        bounds,
        text,
        rgb(248, 248, 250),
        TextAlignment::Center,
        tokens.font_height,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn timeline() -> Timeline {
        Timeline::new(vec![ms(0), ms(400), ms(2_500)], ms(4_000)).expect("timeline")
    }

    #[test]
    fn the_playhead_steps_by_a_frame_a_second_or_a_change_and_stays_inside_the_recording() {
        let timeline = timeline();
        let mut trim = Trim::new(timeline.duration(), 15);
        trim.step(true, Step::Frame, &timeline);
        assert_eq!(trim.playhead, Duration::from_secs(1) / 15);
        trim.step(false, Step::Frame, &timeline);
        trim.step(false, Step::Frame, &timeline);
        assert_eq!(trim.playhead, Duration::ZERO);
        trim.step(true, Step::Change, &timeline);
        assert_eq!(trim.playhead, ms(400));
        trim.step(true, Step::Change, &timeline);
        assert_eq!(trim.playhead, ms(2_500));
        trim.step(true, Step::Second, &timeline);
        assert_eq!(trim.playhead, ms(3_500));
        trim.step(true, Step::Second, &timeline);
        assert_eq!(trim.playhead, ms(4_000));
        trim.step(false, Step::Change, &timeline);
        assert_eq!(trim.playhead, ms(2_500));
    }

    #[test]
    fn the_ends_of_the_span_never_cross() {
        let mut trim = Trim::new(ms(4_000), 30);
        trim.set_playhead(ms(1_234));
        trim.set_start(trim.playhead);
        assert_eq!(trim.start, ms(1_234));
        trim.set_end(ms(500));
        assert_eq!(trim.end, ms(1_244));
        trim.set_start(ms(3_000));
        assert_eq!(trim.start, ms(1_234));
        trim.set_end(ms(9_000));
        assert_eq!(trim.end, ms(4_000));
        assert_eq!(
            trim.edit(),
            RecordingEdit {
                start: ms(1_234),
                end: ms(4_000),
                crop: None
            }
        );
    }

    #[test]
    fn times_read_to_the_millisecond() {
        assert_eq!(clock(ms(0)), "0:00.000");
        assert_eq!(clock(ms(61_005)), "1:01.005");
        assert_eq!(format_seconds(ms(10_800)), "10.800");
    }

    fn layout() -> ReviewLayout {
        ReviewLayout::new(
            UiRect {
                left: 0,
                top: 0,
                right: 1_000,
                bottom: 600,
            },
            UiMetrics::new(UiMetrics::BASE_DPI),
            640,
            360,
        )
    }

    #[test]
    fn the_track_maps_time_both_ways() {
        let layout = layout();
        let duration = ms(10_000);
        assert_eq!(layout.x_at(Duration::ZERO, duration), layout.track.left);
        assert_eq!(layout.x_at(duration, duration), layout.track.right);
        let middle = layout.x_at(ms(5_000), duration);
        assert_eq!(layout.time_at(middle, duration).as_millis() / 10, 500);
        assert_eq!(
            layout.time_at(layout.track.left - 50, duration),
            Duration::ZERO
        );
        assert_eq!(layout.time_at(layout.track.right + 50, duration), duration);
    }

    #[test]
    fn a_press_takes_the_nearer_handle_or_else_the_playhead() {
        let layout = layout();
        let mut trim = Trim::new(ms(10_000), 15);
        trim.set_start(ms(2_000));
        trim.set_end(ms(8_000));
        let y = (layout.track.top + layout.track.bottom) / 2;
        let at = |millis| POINT {
            x: layout.x_at(ms(millis), trim.duration),
            y,
        };
        assert_eq!(grip_at(&layout, &trim, at(2_000), 8), Some(Drag::Start));
        assert_eq!(grip_at(&layout, &trim, at(8_000), 8), Some(Drag::End));
        assert_eq!(grip_at(&layout, &trim, at(5_000), 8), Some(Drag::Playhead));
        let above = POINT {
            y: layout.track.top - 40,
            ..at(5_000)
        };
        assert_eq!(grip_at(&layout, &trim, above, 8), None);
    }

    /// A GIF of `seconds` at 15 frames a second whose picture moves a white bar two pixels a frame.
    fn moving_gif(seconds: u64) -> std::path::PathBuf {
        let (width, height) = (320_u32, 180_u32);
        let mut writer = captastic_core::GifWriter::new(Vec::new(), width, height).expect("writer");
        for step in 0..seconds * 15 {
            let mut pixels = vec![0_u8; (width * height * 4) as usize];
            for y in 0..height {
                for x in 0..width {
                    let at = ((y * width + x) * 4) as usize;
                    let bar = x == (step as u32 * 2) % width;
                    let value = if bar {
                        255
                    } else {
                        60 + (y * 100 / height) as u8
                    };
                    pixels[at..at + 4].copy_from_slice(&[value, value, value, 255]);
                }
            }
            writer
                .push(&pixels, Duration::from_micros(step * 66_667))
                .expect("push");
        }
        let (bytes, _) = writer.finish(Duration::from_secs(seconds)).expect("finish");
        let path = std::env::temp_dir().join(format!(
            "captastic-review-{}-{seconds}.gif",
            std::process::id()
        ));
        std::fs::write(&path, bytes).expect("write the GIF");
        path
    }

    #[test]
    #[ignore = "opens review windows on the interactive desktop"]
    fn a_review_saves_the_span_kept_discards_only_when_asked_and_keeps_on_shutdown() {
        use super::super::DesktopUi;
        use std::sync::mpsc;
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_COMMAND};

        let find = |class: PCWSTR, title: PCWSTR| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                // SAFETY: Looks a window up by its class and title only.
                let found = unsafe { FindWindowW(class, title) };
                if found.0 != 0 {
                    return found;
                }
                assert!(
                    Instant::now() < deadline,
                    "the window never appeared: {}",
                    // SAFETY: Both are static wide strings or null.
                    unsafe { title.to_string() }.unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let post = |hwnd: HWND, message: u32, wparam: usize| {
            // SAFETY: Posting to a live window owned by the desktop UI thread.
            unsafe { PostMessageW(hwnd, message, WPARAM(wparam), LPARAM(0)) }
                .expect("post to the window");
        };
        let _dpi = super::super::shell::ThreadDpiContext::enter_per_monitor_v2()
            .expect("per-monitor DPI awareness for the test thread");
        let path = moving_gif(3);
        let area = Rect {
            x: 0,
            y: 0,
            width: 320,
            height: 180,
        };
        let host = DesktopUi::start().expect("the desktop UI thread starts");
        let (sender, outcomes) = mpsc::channel();

        // Five frames in, I; the end, three frames back, O; Enter saves exactly that span.
        let report = sender.clone();
        host.review(path.clone(), RecordingFormat::Gif, area, move |outcome| {
            let _ = report.send(outcome);
        })
        .expect("the review request is accepted");
        let review = find(REVIEW_CLASS_NAME, PCWSTR::null());
        for _ in 0..5 {
            post(review, WM_KEYDOWN, usize::from(VK_RIGHT.0));
        }
        post(review, WM_KEYDOWN, 0x49);
        post(review, WM_KEYDOWN, usize::from(VK_END.0));
        for _ in 0..3 {
            post(review, WM_KEYDOWN, usize::from(VK_LEFT.0));
        }
        post(review, WM_KEYDOWN, 0x4F);
        post(review, WM_KEYDOWN, usize::from(VK_RETURN.0));
        let interval = Duration::from_secs(1) / 15;
        match outcomes
            .recv_timeout(Duration::from_secs(5))
            .expect("the review answered")
        {
            ReviewOutcome::Save(edit) => {
                assert_eq!(edit.start, interval * 5);
                assert_eq!(edit.end, Duration::from_secs(3) - interval * 3);
                assert_eq!(edit.crop, None);
            }
            other => panic!("expected Save, got {other:?}"),
        }

        // The close button asks; Cancel keeps the review open, OK discards.
        let report = sender.clone();
        host.review(path.clone(), RecordingFormat::Gif, area, move |outcome| {
            let _ = report.send(outcome);
        })
        .expect("the review request is accepted");
        let review = find(REVIEW_CLASS_NAME, PCWSTR::null());
        post(review, WM_CLOSE, 0);
        let question = find(w!("#32770"), w!("Captastic"));
        post(question, WM_COMMAND, 2);
        assert!(outcomes.recv_timeout(Duration::from_millis(500)).is_err());
        post(review, WM_CLOSE, 0);
        let question = find(w!("#32770"), w!("Captastic"));
        post(question, WM_COMMAND, 1);
        assert_eq!(
            outcomes.recv_timeout(Duration::from_secs(5)),
            Ok(ReviewOutcome::Discard)
        );

        // Shut down with a review open: it answers Closed, and the recording is kept.
        let report = sender.clone();
        host.review(path.clone(), RecordingFormat::Gif, area, move |outcome| {
            let _ = report.send(outcome);
        })
        .expect("the review request is accepted");
        find(REVIEW_CLASS_NAME, PCWSTR::null());
        assert!(host.stop(Instant::now() + Duration::from_secs(5)));
        assert_eq!(
            outcomes.recv_timeout(Duration::from_secs(5)),
            Ok(ReviewOutcome::Closed)
        );

        // A recording that cannot be read is reported, and kept.
        let host = DesktopUi::start().expect("the desktop UI thread starts again");
        let missing = path.with_extension("missing.gif");
        host.review(missing, RecordingFormat::Gif, area, move |outcome| {
            let _ = sender.send(outcome);
        })
        .expect("the review request is accepted");
        assert!(matches!(
            outcomes.recv_timeout(Duration::from_secs(5)),
            Ok(ReviewOutcome::Failed(_))
        ));
        drop(host);
        let _ = std::fs::remove_file(&path);
    }
}
