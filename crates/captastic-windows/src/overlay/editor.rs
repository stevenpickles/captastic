//! The editor: a window that crops and redacts a capture before it is published (ADR 0010,
//! Milestone 6 slice 2).
//!
//! Editors live on the `captastic-desktop-ui` thread beside the pins, any number at once. What
//! they edit is a [`captastic_core::Edit`]; this module is the window around it. Nothing an editor
//! shows has been published: **Done** renders the edit and hands it to whoever asked for the
//! editor, and **Cancel**, closing the window, or Captastic exiting hands back nothing at all.
//! Exactly one [`EditOutcome`] is reported for every editor, whichever way it ends.
//!
//! The capture is shown at its physical size when the window has room, and scaled down when it
//! does not. Redactions are shown as the opaque fill they will be published as, so what is on
//! screen is what will be sent.

use captastic_core::{
    paint_mark, CaptureError, CpuFrame, Edit, FrameAlpha, Mark, PixelFormat, Rect,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, BitBlt, EndPaint, GdiFlush, PAINTSTRUCT, SRCCOPY};
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, LoadCursorW, MessageBoxW,
    SetCursor, SetForegroundWindow, SetWindowPos, ShowWindow, HTCLIENT, IDC_ARROW, IDC_CROSS,
    IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDYES, MB_ICONWARNING,
    MB_YESNO, MINMAXINFO, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW, WM_CAPTURECHANGED, WM_CLOSE,
    WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCDESTROY, WM_PAINT, WM_SETCURSOR, WM_SIZE, WS_EX_APPWINDOW,
    WS_OVERLAPPEDWINDOW,
};

use super::desktop_ui::{desktop_error, forget_window, release_window_state, window_state};
use super::layout::{UiMetrics, UiRect};
use super::raster::{
    draw_outline_rect, draw_round_box, draw_surface_to_rect, draw_text, fill_device_rect, rgb,
    TextAlignment,
};
use super::shell::{
    capture_pointer, modifiers, query_display_environment, release_pointer_capture, FrozenSurface,
};

pub(super) const EDITOR_CLASS_NAME: PCWSTR = w!("CaptasticEditor-v1");
const EDITOR_WINDOW_NAME: PCWSTR = w!("Edit capture - Captastic");
/// Sent once the pointer leaves a window that asked with TrackMouseEvent. Defined by hand: the
/// windows crate puts it among the common controls, a feature nothing else here needs.
const WM_MOUSELEAVE: u32 = 0x02A3;

const TOOLBAR_HEIGHT_DIP: i32 = 52;
const TOOLBAR_PADDING_DIP: i32 = 8;
const BUTTON_HEIGHT_DIP: i32 = 36;
const BUTTON_GAP_DIP: i32 = 6;
const GROUP_GAP_DIP: i32 = 16;
const CANVAS_PADDING_DIP: i32 = 16;
/// How far from a crop edge a press still takes hold of it.
const GRIP_TOLERANCE_DIP: i32 = 8;
const HANDLE_SIZE_DIP: i32 = 8;
/// How far the pointer must travel before a press becomes a drag, so a click draws nothing.
const DRAG_THRESHOLD_DIP: i32 = 3;
const MINIMUM_CLIENT_DIP: (i32, i32) = (560, 320);
/// The share of the display's work area a new editor may take.
const MAXIMUM_WORK_AREA_SHARE: f64 = 0.9;

/// What is outside the crop keeps this share of its brightness, in fifths.
const DIM_FIFTHS: u16 = 2;
const CANVAS_RGB: (u8, u8, u8) = (24, 24, 27);
const ACCENT_RGB: (u8, u8, u8) = (45, 125, 246);

/// How an editor ended. Reported exactly once per editor.
#[derive(Debug)]
pub enum EditOutcome {
    /// **Done**: the edited image, ready to publish as the capture it came from.
    Done(Box<CpuFrame>),
    /// **Cancel**, Escape or the close button. Nothing is to be published.
    Cancelled,
    /// The editor was closed without an answer, because Captastic is shutting down. Nothing is to
    /// be published.
    Discarded,
    /// The editor could not be opened or could not render the edit. Nothing is to be published.
    Failed(String),
}

/// Called on the desktop UI thread when an editor ends.
pub type EditFinished = Box<dyn FnOnce(EditOutcome) + Send>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorTool {
    Crop,
    Redact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorButton {
    Crop,
    Redact,
    Undo,
    Redo,
    Cancel,
    Done,
}

impl EditorButton {
    const fn label(self) -> &'static str {
        match self {
            Self::Crop => "Crop",
            Self::Redact => "Redact",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::Cancel => "Cancel",
            Self::Done => "Done",
        }
    }

    const fn width_dip(self) -> i32 {
        match self {
            Self::Crop | Self::Undo | Self::Redo => 64,
            Self::Redact => 76,
            Self::Cancel | Self::Done => 84,
        }
    }
}

/// Where everything in an editor's client area is, for one client size and DPI.
#[derive(Clone, Copy, Debug, PartialEq)]
struct EditorLayout {
    client: UiRect,
    toolbar: UiRect,
    buttons: [(EditorButton, UiRect); 6],
    /// Between the editing buttons and Cancel, for the size of the crop.
    size_label: UiRect,
    canvas: UiRect,
    /// Where the capture is drawn.
    image: UiRect,
    /// Client pixels per capture pixel; never more than one, so a capture is never enlarged.
    scale: f64,
}

impl EditorLayout {
    fn new(client: UiRect, metrics: UiMetrics, image_width: u32, image_height: u32) -> Self {
        let toolbar = UiRect {
            left: client.left,
            top: client.top,
            right: client.right,
            bottom: client.top + metrics.px(TOOLBAR_HEIGHT_DIP),
        };
        let button_height = metrics.px(BUTTON_HEIGHT_DIP);
        let top = toolbar.top + (toolbar.bottom - toolbar.top - button_height) / 2;
        let bottom = top + button_height;
        let gap = metrics.px(BUTTON_GAP_DIP);
        let mut x = client.left + metrics.px(TOOLBAR_PADDING_DIP);
        let mut place_left = |button: EditorButton, extra_gap: i32| {
            x += extra_gap;
            let left = x;
            x += metrics.px(button.width_dip());
            let rect = UiRect {
                left,
                top,
                right: x,
                bottom,
            };
            x += gap;
            (button, rect)
        };
        let crop = place_left(EditorButton::Crop, 0);
        let redact = place_left(EditorButton::Redact, 0);
        let undo = place_left(EditorButton::Undo, metrics.px(GROUP_GAP_DIP) - gap);
        let redo = place_left(EditorButton::Redo, 0);
        let left_end = redo.1.right;
        let mut right = client.right - metrics.px(TOOLBAR_PADDING_DIP);
        let mut place_right = |button: EditorButton| {
            let rect = UiRect {
                left: right - metrics.px(button.width_dip()),
                top,
                right,
                bottom,
            };
            right = rect.left - gap;
            (button, rect)
        };
        let done = place_right(EditorButton::Done);
        let cancel = place_right(EditorButton::Cancel);
        let size_label = UiRect {
            left: left_end + metrics.px(GROUP_GAP_DIP),
            top,
            right: (cancel.1.left - metrics.px(GROUP_GAP_DIP))
                .max(left_end + metrics.px(GROUP_GAP_DIP)),
            bottom,
        };
        let canvas = UiRect {
            left: client.left,
            top: toolbar.bottom,
            right: client.right,
            bottom: client.bottom.max(toolbar.bottom),
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
            buttons: [crop, redact, undo, redo, cancel, done],
            size_label,
            canvas,
            image: UiRect {
                left,
                top: image_top,
                right: left + width,
                bottom: image_top + height,
            },
            scale,
        }
    }

    fn button_at(&self, point: POINT) -> Option<EditorButton> {
        self.buttons
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|(button, _)| *button)
    }

    /// The capture pixel position under a client point, unclamped and fractional.
    fn image_point(&self, point: POINT) -> (f64, f64) {
        (
            f64::from(point.x - self.image.left) / self.scale,
            f64::from(point.y - self.image.top) / self.scale,
        )
    }

    /// Where a rectangle of capture pixels is drawn.
    fn client_rect(&self, rect: Rect) -> UiRect {
        let at = |origin: i32, value: i64| origin + (value as f64 * self.scale).round() as i32;
        UiRect {
            left: at(self.image.left, i64::from(rect.x)),
            top: at(self.image.top, i64::from(rect.y)),
            right: at(self.image.left, rect.right()),
            bottom: at(self.image.top, rect.bottom()),
        }
    }
}

/// The capture pixels spanned by a drag between two fractional pixel positions: every pixel the
/// drag touches, so a drag over a single pixel covers it.
fn rect_between(a: (f64, f64), b: (f64, f64)) -> Option<Rect> {
    Rect::from_edges(
        a.0.min(b.0).floor() as i64,
        a.1.min(b.1).floor() as i64,
        a.0.max(b.0).ceil() as i64,
        a.1.max(b.1).ceil() as i64,
    )
}

/// What a press on the crop takes hold of.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Grip {
    Move,
    Resize {
        left: bool,
        top: bool,
        right: bool,
        bottom: bool,
    },
}

/// The part of the crop, drawn at `crop`, under `point`. Edges and corners win over the inside,
/// and an uncropped capture's inside is not offered for moving: dragging there draws a new crop,
/// which is what someone starting to crop means.
fn grip_at(crop: UiRect, point: POINT, tolerance: i32, cropped: bool) -> Option<Grip> {
    let within_x = point.x >= crop.left - tolerance && point.x <= crop.right + tolerance;
    let within_y = point.y >= crop.top - tolerance && point.y <= crop.bottom + tolerance;
    if !within_x || !within_y {
        return None;
    }
    let nearer = |low: i32, high: i32, value: i32| {
        let to_low = (value - low).abs();
        let to_high = (value - high).abs();
        let near_low = to_low <= tolerance && to_low <= to_high;
        let near_high = to_high <= tolerance && !near_low;
        (near_low, near_high)
    };
    let (left, right) = nearer(crop.left, crop.right, point.x);
    let (top, bottom) = nearer(crop.top, crop.bottom, point.y);
    if left || right || top || bottom {
        Some(Grip::Resize {
            left,
            top,
            right,
            bottom,
        })
    } else if cropped && crop.contains(point) {
        Some(Grip::Move)
    } else {
        None
    }
}

/// The crop `start` after its grip has been dragged by `(dx, dy)` capture pixels, kept within
/// `bounds` and at least one pixel in each direction.
fn adjust_crop(start: Rect, grip: Grip, dx: i64, dy: i64, bounds: Rect) -> Rect {
    match grip {
        Grip::Move => Rect {
            x: (i64::from(start.x) + dx).clamp(0, i64::from(bounds.width) - i64::from(start.width))
                as i32,
            y: (i64::from(start.y) + dy)
                .clamp(0, i64::from(bounds.height) - i64::from(start.height)) as i32,
            ..start
        },
        Grip::Resize {
            left,
            top,
            right,
            bottom,
        } => {
            let mut edges = (
                i64::from(start.x),
                i64::from(start.y),
                start.right(),
                start.bottom(),
            );
            if left {
                edges.0 = (edges.0 + dx).clamp(0, edges.2 - 1);
            }
            if right {
                edges.2 = (edges.2 + dx).clamp(edges.0 + 1, bounds.right());
            }
            if top {
                edges.1 = (edges.1 + dy).clamp(0, edges.3 - 1);
            }
            if bottom {
                edges.3 = (edges.3 + dy).clamp(edges.1 + 1, bounds.bottom());
            }
            Rect::from_edges(edges.0, edges.1, edges.2, edges.3).unwrap_or(start)
        }
    }
}

/// Darkens every pixel of `area` outside `keep`, in a tightly packed BGRA buffer `width` pixels
/// wide, so the crop reads as what will be kept.
fn dim_outside(pixels: &mut [u8], width: usize, area: UiRect, keep: UiRect) {
    let height = pixels.len() / (width * 4).max(1);
    let clamp_x = |value: i32| value.clamp(0, width as i32) as usize;
    let clamp_y = |value: i32| value.clamp(0, height as i32) as usize;
    for y in clamp_y(area.top)..clamp_y(area.bottom) {
        let inside_row = (y as i32) >= keep.top && (y as i32) < keep.bottom;
        for x in clamp_x(area.left)..clamp_x(area.right) {
            if inside_row && (x as i32) >= keep.left && (x as i32) < keep.right {
                continue;
            }
            let at = (y * width + x) * 4;
            for channel in &mut pixels[at..at + 3] {
                *channel = (u16::from(*channel) * DIM_FIFTHS / 5) as u8;
            }
        }
    }
}

/// The capture as the editor shows it: tightly packed, opaque, with a capture that carries alpha
/// laid over the canvas colour, and every mark painted as it will be published.
fn compose_preview(source: &CpuFrame, marks: &[Mark]) -> Vec<u8> {
    let (width, height) = (source.width() as usize, source.height() as usize);
    let stride = source.stride_bytes() as usize;
    let background = [CANVAS_RGB.2, CANVAS_RGB.1, CANVAS_RGB.0];
    let mut pixels = vec![0_u8; width * height * 4];
    for (row, target) in pixels.chunks_exact_mut(width * 4).enumerate() {
        let source_row = &source.pixels()[row * stride..row * stride + width * 4];
        for (to, from) in target.chunks_exact_mut(4).zip(source_row.chunks_exact(4)) {
            match source.alpha() {
                FrameAlpha::Opaque => to[..3].copy_from_slice(&from[..3]),
                FrameAlpha::Straight => {
                    let alpha = u32::from(from[3]);
                    for channel in 0..3 {
                        to[channel] = ((u32::from(from[channel]) * alpha
                            + u32::from(background[channel]) * (255 - alpha)
                            + 127)
                            / 255) as u8;
                    }
                }
            }
            to[3] = u8::MAX;
        }
    }
    let area = Rect {
        x: 0,
        y: 0,
        width: source.width(),
        height: source.height(),
    };
    for mark in marks {
        paint_mark(
            mark,
            &mut pixels,
            width * 4,
            area,
            PixelFormat::Bgra8Unorm,
            FrameAlpha::Opaque,
        );
    }
    pixels
}

/// A press on the canvas, and what it is doing.
#[derive(Clone, Copy, Debug)]
enum Drag {
    /// Drawing a redaction, or a crop from scratch.
    Draw {
        tool: EditorTool,
        anchor: POINT,
        current: POINT,
        moved: bool,
    },
    /// Moving or resizing the crop.
    Crop {
        grip: Grip,
        start: Rect,
        anchor: POINT,
        current: POINT,
    },
}

/// One editor's state, owned by its window.
struct EditorState {
    edit: Edit,
    metrics: UiMetrics,
    layout: EditorLayout,
    preview: FrozenSurface,
    back_buffer: Option<FrozenSurface>,
    tool: EditorTool,
    drag: Option<Drag>,
    pressed: Option<EditorButton>,
    hovered: Option<EditorButton>,
    tracking_leave: bool,
    on_finish: Option<EditFinished>,
}

impl EditorState {
    /// What a drag in progress would make, if it ended here: a rectangle and the tool it is for.
    fn pending(&self) -> Option<(EditorTool, Rect)> {
        let bounds = self.edit.bounds();
        match self.drag? {
            Drag::Draw {
                tool,
                anchor,
                current,
                moved,
            } => {
                if !moved {
                    return None;
                }
                let rect = rect_between(
                    self.layout.image_point(anchor),
                    self.layout.image_point(current),
                )?
                .intersection(bounds)?;
                Some((tool, rect))
            }
            Drag::Crop {
                grip,
                start,
                anchor,
                current,
            } => {
                let dx = (f64::from(current.x - anchor.x) / self.layout.scale).round() as i64;
                let dy = (f64::from(current.y - anchor.y) / self.layout.scale).round() as i64;
                Some((EditorTool::Crop, adjust_crop(start, grip, dx, dy, bounds)))
            }
        }
    }

    fn button_enabled(&self, button: EditorButton) -> bool {
        match button {
            EditorButton::Undo => self.edit.can_undo(),
            EditorButton::Redo => self.edit.can_redo(),
            _ => true,
        }
    }
}

/// Opens an editor on `frame`, centred on the display where it was captured at `at`.
///
/// The frame should already be [`CpuFrame::detached`]. An editor that cannot be opened reports
/// [`EditOutcome::Failed`] before this returns, so `on_finish` is always answered.
pub(super) fn create_editor(
    instance: HINSTANCE,
    frame: CpuFrame,
    at: Rect,
    on_finish: EditFinished,
) -> Option<HWND> {
    match open_editor(instance, frame, at) {
        Ok((hwnd, state_pointer)) => {
            // SAFETY: The window was just created on this thread with this state, and nothing has
            // run on it that could have ended it.
            unsafe { (*state_pointer).on_finish = Some(on_finish) };
            Some(hwnd)
        }
        Err(error) => {
            on_finish(EditOutcome::Failed(error.to_string()));
            None
        }
    }
}

fn open_editor(
    instance: HINSTANCE,
    frame: CpuFrame,
    at: Rect,
) -> Result<(HWND, *mut EditorState), CaptureError> {
    if frame.format() != PixelFormat::Bgra8Unorm {
        return Err(desktop_error(
            "edit_frame_format",
            format!("cannot edit a {:?} frame", frame.format()),
        ));
    }
    let environment = query_display_environment(at);
    let metrics = environment.metrics;
    let work = UiRect {
        left: environment.work_area.left + at.x,
        top: environment.work_area.top + at.y,
        right: environment.work_area.right + at.x,
        bottom: environment.work_area.bottom + at.y,
    };
    let (width, height) = (frame.width(), frame.height());
    let preview = FrozenSurface::new(width, height, &compose_preview(&frame, &[]))?;
    let padding = metrics.px(CANVAS_PADDING_DIP) * 2;
    let maximum_width = (f64::from(work.right - work.left) * MAXIMUM_WORK_AREA_SHARE) as i32;
    let maximum_height = (f64::from(work.bottom - work.top) * MAXIMUM_WORK_AREA_SHARE) as i32;
    let client_width = (width as i32)
        .saturating_add(padding)
        .clamp(metrics.px(MINIMUM_CLIENT_DIP.0), maximum_width.max(1));
    let client_height = (height as i32)
        .saturating_add(padding + metrics.px(TOOLBAR_HEIGHT_DIP))
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
    let state = Box::new(EditorState {
        layout: EditorLayout::new(client, metrics, width, height),
        edit: Edit::new(frame),
        metrics,
        preview,
        back_buffer: None,
        tool: EditorTool::Crop,
        drag: None,
        pressed: None,
        hovered: None,
        tracking_leave: false,
        on_finish: None,
    });
    let state_pointer = Box::into_raw(state);
    // SAFETY: A top-level window of a class registered on this thread; the state pointer is handed
    // to WM_NCCREATE and reclaimed in WM_NCDESTROY.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW,
            EDITOR_CLASS_NAME,
            EDITOR_WINDOW_NAME,
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
            "create_editor_window",
            windows::core::Error::from_win32().to_string(),
        ));
    }
    // SAFETY: hwnd is live. An editor is opened because the user asked for it, so it takes focus:
    // Enter and Escape must reach it straight away.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
    Ok((hwnd, state_pointer))
}

pub(super) unsafe extern "system" fn editor_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_pointer = window_state::<EditorState>(hwnd, message, lparam);
    if state_pointer.is_null() {
        // SAFETY: Forwards a message that arrived before WM_NCCREATE or after WM_NCDESTROY.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    match message {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            // SAFETY: The editor's state lives until WM_NCDESTROY on this thread, and painting
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
            // SAFETY: hwnd is this thread's live editor; the resulting WM_SIZE lays it out again.
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
            // SAFETY: As above. Released before acting: a button may end the editor.
            let pressed =
                unsafe { pointer_released(hwnd, &mut *state_pointer, client_point(lparam)) };
            if let Some(button) = pressed {
                press_button(hwnd, state_pointer, button);
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // SAFETY: As above. Losing the pointer mid-drag abandons the drag.
            let state = unsafe { &mut *state_pointer };
            if state.drag.take().is_some() || state.pressed.take().is_some() {
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            key_pressed(hwnd, state_pointer, wparam.0 as u16);
            LRESULT(0)
        }
        WM_CLOSE => {
            request_cancel(hwnd, state_pointer);
            LRESULT(0)
        }
        WM_DESTROY => {
            forget_window(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            if let Some(mut state) = release_window_state::<EditorState>(hwnd) {
                // Destroyed with no answer given: only shutdown closes an editor that way.
                if let Some(on_finish) = state.on_finish.take() {
                    on_finish(EditOutcome::Discarded);
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

fn relayout(hwnd: HWND, state: &mut EditorState) {
    let mut client = RECT::default();
    // SAFETY: hwnd is this thread's live editor and client is writable storage.
    if unsafe { GetClientRect(hwnd, &mut client) }.is_err() {
        return;
    }
    let source = state.edit.source();
    state.layout = EditorLayout::new(
        UiRect {
            left: client.left,
            top: client.top,
            right: client.right,
            bottom: client.bottom,
        },
        state.metrics,
        source.width(),
        source.height(),
    );
    invalidate(hwnd);
}

fn set_cursor(hwnd: HWND, state: &EditorState) {
    let mut point = POINT::default();
    // SAFETY: point is writable storage; the conversion is to this live window's client area.
    let located = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point).is_ok()
            && windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point).as_bool()
    };
    let cursor = if !located || !state.layout.canvas.contains(point) {
        IDC_ARROW
    } else {
        match state.drag.map(|drag| match drag {
            Drag::Crop { grip, .. } => Some(grip),
            Drag::Draw { .. } => None,
        }) {
            Some(Some(grip)) => grip_cursor(grip),
            Some(None) => IDC_CROSS,
            None => match state.tool {
                EditorTool::Redact => IDC_CROSS,
                EditorTool::Crop => grip_at(
                    state.layout.client_rect(state.edit.crop()),
                    point,
                    state.metrics.px(GRIP_TOLERANCE_DIP),
                    state.edit.crop() != state.edit.bounds(),
                )
                .map_or(IDC_CROSS, grip_cursor),
            },
        }
    };
    // SAFETY: Each is a shared system cursor that needs no destruction.
    if let Ok(cursor) = unsafe { LoadCursorW(None, cursor) } {
        // SAFETY: cursor is a live shared system cursor.
        unsafe { SetCursor(cursor) };
    }
}

fn grip_cursor(grip: Grip) -> PCWSTR {
    match grip {
        Grip::Move => IDC_SIZEALL,
        Grip::Resize {
            left,
            top,
            right,
            bottom,
        } => match ((left || right), (top || bottom)) {
            (true, true) if (left && top) || (right && bottom) => IDC_SIZENWSE,
            (true, true) => IDC_SIZENESW,
            (true, false) => IDC_SIZEWE,
            _ => IDC_SIZENS,
        },
    }
}

fn pointer_moved(hwnd: HWND, state: &mut EditorState, point: POINT) {
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
    let threshold = state.metrics.px(DRAG_THRESHOLD_DIP);
    match state.drag.as_mut() {
        Some(Drag::Draw {
            anchor,
            current,
            moved,
            ..
        }) => {
            *current = point;
            *moved |=
                (point.x - anchor.x).abs() >= threshold || (point.y - anchor.y).abs() >= threshold;
            changed = true;
        }
        Some(Drag::Crop { current, .. }) => {
            *current = point;
            changed = true;
        }
        None => {}
    }
    if changed {
        invalidate(hwnd);
    }
}

fn pointer_pressed(hwnd: HWND, state: &mut EditorState, point: POINT) {
    if let Some(button) = state.layout.button_at(point) {
        if state.button_enabled(button) {
            state.pressed = Some(button);
            capture_pointer(hwnd);
        }
        return;
    }
    if !state.layout.canvas.contains(point) {
        return;
    }
    let drag = match state.tool {
        EditorTool::Redact => Drag::Draw {
            tool: EditorTool::Redact,
            anchor: point,
            current: point,
            moved: false,
        },
        EditorTool::Crop => {
            let crop = state.edit.crop();
            match grip_at(
                state.layout.client_rect(crop),
                point,
                state.metrics.px(GRIP_TOLERANCE_DIP),
                crop != state.edit.bounds(),
            ) {
                Some(grip) => Drag::Crop {
                    grip,
                    start: crop,
                    anchor: point,
                    current: point,
                },
                None => Drag::Draw {
                    tool: EditorTool::Crop,
                    anchor: point,
                    current: point,
                    moved: false,
                },
            }
        }
    };
    state.drag = Some(drag);
    capture_pointer(hwnd);
}

/// Ends a press. Returns the toolbar button to act on, if the press was one and was released over
/// it; a drag is committed here.
fn pointer_released(hwnd: HWND, state: &mut EditorState, point: POINT) -> Option<EditorButton> {
    let pending = state.pending();
    let drag = state.drag.take();
    let pressed = state.pressed.take();
    // Taken before releasing: the release sends WM_CAPTURECHANGED, which would abandon both.
    release_pointer_capture();
    if drag.is_some() {
        match pending {
            Some((EditorTool::Redact, rect)) => {
                if state.edit.redact(rect) {
                    refresh_preview(state);
                }
            }
            Some((EditorTool::Crop, rect)) => {
                state.edit.set_crop(rect);
            }
            None => {}
        }
        invalidate(hwnd);
    }
    pressed.filter(|button| state.layout.button_at(point) == Some(*button))
}

fn press_button(hwnd: HWND, state_pointer: *mut EditorState, button: EditorButton) {
    {
        // SAFETY: The editor's state lives until WM_NCDESTROY on this thread; this borrow ends
        // before anything below can end the editor.
        let state = unsafe { &mut *state_pointer };
        match button {
            EditorButton::Crop => state.tool = EditorTool::Crop,
            EditorButton::Redact => state.tool = EditorTool::Redact,
            EditorButton::Undo => {
                if state.edit.undo() {
                    refresh_preview(state);
                }
            }
            EditorButton::Redo => {
                if state.edit.redo() {
                    refresh_preview(state);
                }
            }
            EditorButton::Cancel | EditorButton::Done => {}
        }
    }
    match button {
        EditorButton::Cancel => request_cancel(hwnd, state_pointer),
        EditorButton::Done => finish_done(hwnd, state_pointer),
        _ => invalidate(hwnd),
    }
}

fn key_pressed(hwnd: HWND, state_pointer: *mut EditorState, key: u16) {
    let held = modifiers();
    if key == VK_ESCAPE.0 {
        // SAFETY: The editor's state lives until WM_NCDESTROY on this thread; the borrow ends
        // before the cancel below.
        let dragging = unsafe { (*state_pointer).drag.take().is_some() };
        if dragging {
            // Escape mid-drag drops the drag, not the edit.
            release_pointer_capture();
            invalidate(hwnd);
        } else {
            request_cancel(hwnd, state_pointer);
        }
        return;
    }
    let button = match key {
        _ if key == VK_RETURN.0 => Some(EditorButton::Done),
        0x5A if held.ctrl && held.shift => Some(EditorButton::Redo),
        0x5A if held.ctrl => Some(EditorButton::Undo),
        0x59 if held.ctrl => Some(EditorButton::Redo),
        0x43 if !held.ctrl => Some(EditorButton::Crop),
        0x52 if !held.ctrl => Some(EditorButton::Redact),
        _ => None,
    };
    if let Some(button) = button {
        // SAFETY: As above.
        let dragging = unsafe { (*state_pointer).drag.is_some() };
        if !dragging {
            press_button(hwnd, state_pointer, button);
        }
    }
}

fn refresh_preview(state: &mut EditorState) {
    let pixels = compose_preview(state.edit.source(), state.edit.marks());
    if let Err(error) = state.preview.write_pixels(&pixels) {
        log::warn!("could not redraw the editor's preview: {error}");
    }
}

/// Cancels, after asking first when there are edits to lose.
fn request_cancel(hwnd: HWND, state_pointer: *mut EditorState) {
    // SAFETY: The editor's state lives until WM_NCDESTROY on this thread; read and released.
    let modified = unsafe { (*state_pointer).edit.is_modified() };
    if modified {
        // SAFETY: hwnd is this thread's live editor and both strings are static. The box runs a
        // modal loop; nothing in it can end this editor, since only it and shutdown close one,
        // and shutdown waits for this thread.
        let answer = unsafe {
            MessageBoxW(
                hwnd,
                w!("Discard your edits? Nothing from this capture has been copied or saved."),
                w!("Captastic"),
                MB_YESNO | MB_ICONWARNING,
            )
        };
        if answer != IDYES {
            return;
        }
    }
    finish(hwnd, state_pointer, EditOutcome::Cancelled);
}

fn finish_done(hwnd: HWND, state_pointer: *mut EditorState) {
    // SAFETY: The editor's state lives until WM_NCDESTROY on this thread; the borrow ends with
    // the render.
    let rendered = unsafe { (*state_pointer).edit.render() };
    let outcome = match rendered {
        Ok(frame) => EditOutcome::Done(Box::new(frame)),
        Err(error) => EditOutcome::Failed(format!("the edited capture could not be made: {error}")),
    };
    finish(hwnd, state_pointer, outcome);
}

fn finish(hwnd: HWND, state_pointer: *mut EditorState, outcome: EditOutcome) {
    // SAFETY: As above; the answer is taken out so it is given once.
    let on_finish = unsafe { (*state_pointer).on_finish.take() };
    if let Some(on_finish) = on_finish {
        on_finish(outcome);
    }
    // SAFETY: hwnd is this thread's live editor.
    let _ = unsafe { DestroyWindow(hwnd) };
}

fn paint(hwnd: HWND, state: &mut EditorState) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: hwnd is this thread's live editor, painting in response to WM_PAINT.
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
                log::warn!("could not allocate the editor's drawing surface: {error}");
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

fn compose(state: &EditorState, surface: &FrozenSurface) {
    let device = surface.device;
    let layout = state.layout;
    let metrics = state.metrics;
    let canvas = rgb(CANVAS_RGB.0, CANVAS_RGB.1, CANVAS_RGB.2);
    let accent = rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2);
    fill_device_rect(device, native(layout.client), canvas);
    draw_surface_to_rect(device, &state.preview, layout.image);

    let pending = state.pending();
    let crop = match pending {
        Some((EditorTool::Crop, rect)) => rect,
        _ => state.edit.crop(),
    };
    let crop_on_screen = layout.client_rect(crop);
    // SAFETY: Completes every queued GDI draw before the pixels are touched directly.
    let _ = unsafe { GdiFlush() };
    // SAFETY: The surface's DIB holds exactly byte_length writable bytes, owned by this thread.
    let pixels = unsafe { std::slice::from_raw_parts_mut(surface.bits, surface.byte_length) };
    dim_outside(pixels, surface.width as usize, layout.image, crop_on_screen);

    if let Some((EditorTool::Redact, rect)) = pending {
        let on_screen = layout.client_rect(rect);
        fill_device_rect(device, native(on_screen), rgb(0, 0, 0));
        draw_outline_rect(
            device,
            on_screen.left,
            on_screen.top,
            on_screen.right,
            on_screen.bottom,
            accent,
            1,
        );
    }
    let stroke = metrics.px(2).max(1);
    draw_outline_rect(
        device,
        crop_on_screen.left,
        crop_on_screen.top,
        crop_on_screen.right,
        crop_on_screen.bottom,
        accent,
        stroke,
    );
    if state.tool == EditorTool::Crop {
        draw_crop_handles(device, crop_on_screen, metrics, accent);
    }
    draw_toolbar(device, state, crop);
}

fn draw_crop_handles(
    device: windows::Win32::Graphics::Gdi::HDC,
    crop: UiRect,
    metrics: UiMetrics,
    accent: windows::Win32::Foundation::COLORREF,
) {
    let half = metrics.px(HANDLE_SIZE_DIP) / 2;
    let border = metrics.px(1).max(1);
    let middle_x = (crop.left + crop.right) / 2;
    let middle_y = (crop.top + crop.bottom) / 2;
    for (x, y) in [
        (crop.left, crop.top),
        (middle_x, crop.top),
        (crop.right, crop.top),
        (crop.right, middle_y),
        (crop.right, crop.bottom),
        (middle_x, crop.bottom),
        (crop.left, crop.bottom),
        (crop.left, middle_y),
    ] {
        let outer = UiRect {
            left: x - half,
            top: y - half,
            right: x + half,
            bottom: y + half,
        };
        fill_device_rect(device, native(outer), accent);
        let inner = UiRect {
            left: outer.left + border,
            top: outer.top + border,
            right: outer.right - border,
            bottom: outer.bottom - border,
        };
        fill_device_rect(device, native(inner), rgb(255, 255, 255));
    }
}

fn draw_toolbar(device: windows::Win32::Graphics::Gdi::HDC, state: &EditorState, crop: Rect) {
    let layout = state.layout;
    let tokens = state.metrics.toolbar_tokens();
    fill_device_rect(device, native(layout.toolbar), rgb(38, 38, 41));
    fill_device_rect(
        device,
        native(UiRect {
            top: layout.toolbar.bottom - 1,
            ..layout.toolbar
        }),
        rgb(60, 60, 65),
    );
    for (button, bounds) in layout.buttons {
        let enabled = state.button_enabled(button);
        let hovered = enabled && state.hovered == Some(button);
        let selected = matches!(
            (button, state.tool),
            (EditorButton::Crop, EditorTool::Crop) | (EditorButton::Redact, EditorTool::Redact)
        );
        let fill = match button {
            EditorButton::Done if hovered => Some(rgb(68, 145, 255)),
            EditorButton::Done => Some(rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2)),
            _ if selected => Some(rgb(76, 76, 82)),
            _ if hovered => Some(rgb(58, 58, 63)),
            _ => None,
        };
        if let Some(fill) = fill {
            draw_round_box(device, bounds, fill, fill, tokens.control_corner_radius);
        }
        let text = if enabled {
            rgb(245, 245, 247)
        } else {
            rgb(110, 110, 116)
        };
        draw_text(
            device,
            bounds,
            button.label(),
            text,
            TextAlignment::Center,
            tokens.font_height,
        );
    }
    draw_text(
        device,
        layout.size_label,
        &format!("{} \u{00d7} {}", crop.width, crop.height),
        rgb(170, 170, 176),
        TextAlignment::Center,
        tokens.font_height,
    );
}

const fn native(rect: UiRect) -> RECT {
    RECT {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use captastic_core::{
        CaptureId, CaptureMode, ColorSpace, DisplayId, FrameMetadata, FrameOrigin, TimingProvenance,
    };
    use std::sync::Arc;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn client(width: i32, height: i32) -> UiRect {
        UiRect {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        }
    }

    fn small_frame() -> CpuFrame {
        CpuFrame::new(
            Arc::from(vec![0x80_u8; 64]),
            4,
            4,
            16,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            FrameMetadata {
                capture_id: CaptureId(9),
                backend: "test".to_owned(),
                display_id: DisplayId::primary(),
                source_rect: rect(40, 40, 4, 4),
                rotation_degrees: 0,
                capture_mode: CaptureMode::Latest { max_age_ms: None },
                presentation_offset_ns: None,
                timing_provenance: TimingProvenance::Synthetic,
                native_ready_offset_ns: 0,
                cpu_ready_offset_ns: None,
                frame_age_ns: None,
                verified_current_offset_ns: None,
                frame_generation: None,
                copy_count: 1,
                pool_slot: None,
                cursor: None,
            },
        )
        .expect("a valid four-pixel frame")
    }

    #[test]
    #[ignore = "opens editor windows on the interactive desktop"]
    fn an_editor_publishes_only_what_done_hands_back_and_nothing_otherwise() {
        use super::super::DesktopUi;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

        let find_editor = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                // SAFETY: Looks a window up by its class name only.
                let found = unsafe { FindWindowW(EDITOR_CLASS_NAME, PCWSTR::null()) };
                if found.0 != 0 {
                    return found;
                }
                assert!(Instant::now() < deadline, "the editor never appeared");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let post = |hwnd: HWND, message: u32, wparam: usize, lparam: isize| {
            // SAFETY: Posting to a live editor window owned by the desktop UI thread.
            unsafe { PostMessageW(hwnd, message, WPARAM(wparam), LPARAM(lparam)) }
                .expect("post to the editor");
        };
        let at = |x: i32, y: i32| ((y as u32 as isize) << 16) | (x as u16 as isize);
        let host = DesktopUi::start().expect("the desktop UI thread starts");
        let frame = small_frame();
        let (sender, outcomes) = mpsc::channel();

        // Redact a drag across the whole canvas, then Done: the published image is the capture's
        // size, and every pixel of it is the opaque fill.
        let report = sender.clone();
        host.edit(
            frame.detached(),
            frame.metadata.source_rect,
            move |outcome| {
                let _ = report.send(outcome);
            },
        )
        .expect("the edit request is accepted");
        let editor = find_editor();
        let mut client = RECT::default();
        // SAFETY: editor is a live window and client is writable storage.
        unsafe { GetClientRect(editor, &mut client) }.expect("client size");
        // SAFETY: editor is a live window; the call has no other arguments.
        let metrics = UiMetrics::new(unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(editor) });
        let canvas_top = metrics.px(TOOLBAR_HEIGHT_DIP) + 1;
        post(editor, WM_KEYDOWN, usize::from(b'R'), 0);
        post(editor, WM_LBUTTONDOWN, 1, at(1, canvas_top));
        post(
            editor,
            WM_MOUSEMOVE,
            1,
            at(client.right / 2, client.bottom / 2),
        );
        post(
            editor,
            WM_MOUSEMOVE,
            1,
            at(client.right - 2, client.bottom - 2),
        );
        post(
            editor,
            WM_LBUTTONUP,
            0,
            at(client.right - 2, client.bottom - 2),
        );
        post(editor, WM_KEYDOWN, usize::from(VK_RETURN.0), 0);
        match outcomes
            .recv_timeout(Duration::from_secs(5))
            .expect("the editor answered")
        {
            EditOutcome::Done(edited) => {
                assert_eq!((edited.width(), edited.height()), (4, 4));
                assert_eq!(edited.metadata.capture_id, CaptureId(9));
                assert!(edited
                    .pixels()
                    .chunks_exact(4)
                    .all(|pixel| pixel == [0, 0, 0, 255]));
            }
            other => panic!("expected Done, got {other:?}"),
        }

        // The first editor answered before destroying itself; wait for it to go, so the next lookup
        // cannot find it.
        let deadline = Instant::now() + Duration::from_secs(5);
        // SAFETY: IsWindow accepts any handle, live or not.
        while unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(editor) }.as_bool() {
            assert!(
                Instant::now() < deadline,
                "the finished editor never closed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // Escape on an unedited capture cancels without asking.
        let report = sender.clone();
        host.edit(
            frame.detached(),
            frame.metadata.source_rect,
            move |outcome| {
                let _ = report.send(outcome);
            },
        )
        .expect("the edit request is accepted");
        post(find_editor(), WM_KEYDOWN, usize::from(VK_ESCAPE.0), 0);
        assert!(matches!(
            outcomes.recv_timeout(Duration::from_secs(5)),
            Ok(EditOutcome::Cancelled)
        ));

        // An editor still open when the thread stops is discarded, not published.
        host.edit(
            frame.detached(),
            frame.metadata.source_rect,
            move |outcome| {
                let _ = sender.send(outcome);
            },
        )
        .expect("the edit request is accepted");
        find_editor();
        assert!(host.stop(Instant::now() + Duration::from_secs(2)));
        assert!(matches!(
            outcomes.recv_timeout(Duration::from_secs(1)),
            Ok(EditOutcome::Discarded)
        ));
    }

    #[test]
    fn a_capture_that_fits_is_shown_at_its_physical_size_and_centred() {
        let metrics = UiMetrics::new(96);
        let layout = EditorLayout::new(client(800, 600), metrics, 200, 100);
        assert_eq!(layout.scale, 1.0);
        assert_eq!(layout.toolbar.bottom, 52);
        // Centred in the canvas below the toolbar.
        assert_eq!(layout.image.right - layout.image.left, 200);
        assert_eq!(layout.image.left, 300);
        assert_eq!(layout.image.top, 52 + (548 - 100) / 2);
    }

    #[test]
    fn a_capture_that_does_not_fit_is_scaled_down_never_up() {
        let metrics = UiMetrics::new(96);
        let layout = EditorLayout::new(client(632, 552), metrics, 1200, 400);
        // 600 pixels of room across, inside the 16 DIP padding, for 1200 capture pixels.
        assert_eq!(layout.scale, 0.5);
        assert_eq!(layout.image.right - layout.image.left, 600);
        assert_eq!(layout.image.bottom - layout.image.top, 200);
        // And a client point maps back to the capture pixel under it.
        let (x, y) = layout.image_point(POINT {
            x: layout.image.left + 100,
            y: layout.image.top + 50,
        });
        assert_eq!((x, y), (200.0, 100.0));
        assert_eq!(
            layout.client_rect(rect(200, 100, 2, 2)),
            UiRect {
                left: layout.image.left + 100,
                top: layout.image.top + 50,
                right: layout.image.left + 101,
                bottom: layout.image.top + 51,
            }
        );
    }

    #[test]
    fn the_toolbar_keeps_its_buttons_apart_at_the_minimum_size_and_every_dpi() {
        for dpi in [96, 120, 144, 192] {
            let metrics = UiMetrics::new(dpi);
            let layout = EditorLayout::new(
                client(
                    metrics.px(MINIMUM_CLIENT_DIP.0),
                    metrics.px(MINIMUM_CLIENT_DIP.1),
                ),
                metrics,
                10,
                10,
            );
            for pair in layout.buttons.windows(2) {
                assert!(
                    pair[0].1.right < pair[1].1.left,
                    "{:?} and {:?} overlap at {dpi} DPI",
                    pair[0].0,
                    pair[1].0
                );
            }
            assert!(layout.buttons[5].1.right <= layout.client.right);
            assert!(layout.size_label.left <= layout.size_label.right);
            assert_eq!(
                layout.button_at(POINT {
                    x: layout.buttons[5].1.left + 1,
                    y: layout.buttons[5].1.top + 1,
                }),
                Some(EditorButton::Done)
            );
        }
    }

    #[test]
    fn a_drag_covers_every_pixel_it_touches_and_a_still_press_covers_none() {
        assert_eq!(
            rect_between((1.5, 2.25), (0.25, 4.0)),
            Some(rect(0, 2, 2, 2))
        );
        assert_eq!(rect_between((3.0, 3.0), (3.0, 5.0)), None);
    }

    #[test]
    fn crop_edges_and_corners_are_taken_before_its_inside() {
        let crop = UiRect {
            left: 100,
            top: 100,
            right: 300,
            bottom: 200,
        };
        let at = |x, y| POINT { x, y };
        assert_eq!(
            grip_at(crop, at(103, 150), 8, true),
            Some(Grip::Resize {
                left: true,
                top: false,
                right: false,
                bottom: false
            })
        );
        assert_eq!(
            grip_at(crop, at(296, 205), 8, true),
            Some(Grip::Resize {
                left: false,
                top: false,
                right: true,
                bottom: true
            })
        );
        assert_eq!(grip_at(crop, at(200, 150), 8, true), Some(Grip::Move));
        // Uncropped, the inside starts a new crop instead.
        assert_eq!(grip_at(crop, at(200, 150), 8, false), None);
        assert_eq!(grip_at(crop, at(50, 150), 8, true), None);
        // A crop narrower than two tolerances still offers the nearer edge alone.
        let narrow = UiRect {
            left: 100,
            top: 100,
            right: 106,
            bottom: 200,
        };
        assert_eq!(
            grip_at(narrow, at(105, 150), 8, true),
            Some(Grip::Resize {
                left: false,
                top: false,
                right: true,
                bottom: false
            })
        );
    }

    #[test]
    fn a_dragged_crop_stays_inside_the_capture_and_never_inverts() {
        let bounds = rect(0, 0, 100, 50);
        let start = rect(10, 10, 20, 20);
        assert_eq!(
            adjust_crop(start, Grip::Move, 500, -500, bounds),
            rect(80, 0, 20, 20)
        );
        let left = Grip::Resize {
            left: true,
            top: false,
            right: false,
            bottom: false,
        };
        assert_eq!(
            adjust_crop(start, left, -50, 0, bounds),
            rect(0, 10, 30, 20)
        );
        assert_eq!(adjust_crop(start, left, 50, 0, bounds), rect(29, 10, 1, 20));
        let bottom_right = Grip::Resize {
            left: false,
            top: false,
            right: true,
            bottom: true,
        };
        assert_eq!(
            adjust_crop(start, bottom_right, 500, 500, bounds),
            rect(10, 10, 90, 40)
        );
    }

    #[test]
    fn only_what_is_outside_the_crop_is_dimmed() {
        let mut pixels = vec![200_u8; 4 * 4 * 4];
        let area = UiRect {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        };
        let keep = UiRect {
            left: 1,
            top: 1,
            right: 3,
            bottom: 3,
        };
        dim_outside(&mut pixels, 4, area, keep);
        let pixel = |x: usize, y: usize| pixels[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].to_vec();
        assert_eq!(pixel(1, 1), [200, 200, 200, 200]);
        assert_eq!(pixel(2, 2), [200, 200, 200, 200]);
        // Colour dimmed, alpha left alone.
        assert_eq!(pixel(0, 0), [80, 80, 80, 200]);
        assert_eq!(pixel(3, 1), [80, 80, 80, 200]);
    }

    #[test]
    fn the_preview_shows_redactions_as_the_fill_they_are_published_as() {
        let metadata = FrameMetadata {
            capture_id: CaptureId(1),
            backend: "test".to_owned(),
            display_id: DisplayId::primary(),
            source_rect: rect(0, 0, 2, 2),
            rotation_degrees: 0,
            capture_mode: CaptureMode::Latest { max_age_ms: None },
            presentation_offset_ns: None,
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: 0,
            cpu_ready_offset_ns: None,
            frame_age_ns: None,
            verified_current_offset_ns: None,
            frame_generation: None,
            copy_count: 1,
            pool_slot: None,
            cursor: None,
        };
        let frame = CpuFrame::new(
            Arc::from(vec![
                50_u8, 60, 70, 0, 50, 60, 70, 0, 50, 60, 70, 0, 50, 60, 70, 0,
            ]),
            2,
            2,
            8,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("frame");
        let preview = compose_preview(&frame, &[Mark::Redaction(rect(1, 0, 1, 2))]);
        // A desktop capture's alpha byte is meaningless, so the preview is opaque.
        assert_eq!(&preview[0..4], &[50, 60, 70, 255]);
        assert_eq!(&preview[4..8], &[0, 0, 0, 255]);
        assert_eq!(&preview[12..16], &[0, 0, 0, 255]);
    }
}
