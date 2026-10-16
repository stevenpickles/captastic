//! The editor: a window that crops, redacts and marks up a capture before it is published
//! (ADR 0010, Milestone 6 slices 2 to 4).
//!
//! Editors live on the `captastic-desktop-ui` thread beside the pins, any number at once. What
//! they edit is a [`captastic_core::Edit`]; this module is the window around it. Nothing an editor
//! shows has been published: **Done** renders the edit and hands it to whoever asked for the
//! editor, and **Cancel**, closing the window, or Captastic exiting hands back nothing at all.
//! Exactly one [`EditOutcome`] is reported for every editor, whichever way it ends.
//!
//! The capture is shown at its physical size when the window has room, and scaled down when it
//! does not. Every mark - a redaction, a box, an arrow, a pen, marker or highlighter stroke - is painted into
//! the preview by the same code that paints the published image, so what is on screen is what will
//! be sent. A mark being drawn is painted the same way as it grows, redrawing only the pixels it
//! covers, and so is text as it is typed.

use std::sync::atomic::{AtomicUsize, Ordering};

use captastic_core::{
    paint_mark, CaptureError, Color, CpuFrame, Edit, FrameAlpha, Mark, PixelFormat, Rect,
    StrokeStyle,
};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, EndPaint, GdiFlush, HDC, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_ADD, VK_ESCAPE, VK_OEM_MINUS, VK_OEM_PLUS,
    VK_RETURN, VK_SUBTRACT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, LoadCursorW, MessageBoxW,
    SetCursor, SetForegroundWindow, SetWindowPos, ShowWindow, HTCLIENT, IDC_ARROW, IDC_CROSS,
    IDC_IBEAM, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDYES,
    MB_ICONWARNING, MB_YESNO, MINMAXINFO, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW, WM_CAPTURECHANGED,
    WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCDESTROY, WM_PAINT, WM_SETCURSOR, WM_SIZE,
    WS_EX_APPWINDOW, WS_OVERLAPPEDWINDOW,
};

use super::desktop_ui::{desktop_error, forget_window, release_window_state, window_state};
use super::layout::{UiMetrics, UiRect};
use super::raster::{
    draw_lines, draw_outline_rect, draw_round_box, draw_surface_to_rect, draw_text,
    fill_device_rect, measure_ui_text, rgb, TextAlignment,
};
use super::shell::{
    capture_pointer, modifiers, query_display_environment, release_pointer_capture, FrozenSurface,
};
use super::text_mark::{layout_text, TextLayout};
use super::vector::{bgr, Bgr, Diagonal, Painter};

pub(super) const EDITOR_CLASS_NAME: PCWSTR = w!("CaptasticEditor-v1");
const EDITOR_WINDOW_NAME: PCWSTR = w!("Edit capture - Captastic");
/// Sent once the pointer leaves a window that asked with TrackMouseEvent. Defined by hand: the
/// windows crate puts it among the common controls, a feature nothing else here needs.
const WM_MOUSELEAVE: u32 = 0x02A3;

const TOOLBAR_HEIGHT_DIP: i32 = 52;
const TOOLBAR_PADDING_DIP: i32 = 8;
const BUTTON_HEIGHT_DIP: i32 = 36;
const ICON_BUTTON_WIDTH_DIP: i32 = 36;
const ICON_BUTTON_GAP_DIP: i32 = 4;
const SWATCH_WIDTH_DIP: i32 = 28;
const SWATCH_DIAMETER_DIP: i32 = 18;
const SWATCH_GAP_DIP: i32 = 2;
const TEXT_BUTTON_WIDTH_DIP: i32 = 84;
const BUTTON_GAP_DIP: i32 = 6;
const GROUP_GAP_DIP: i32 = 12;
const CANVAS_PADDING_DIP: i32 = 16;
/// How far from a crop edge a press still takes hold of it.
const GRIP_TOLERANCE_DIP: i32 = 8;
const HANDLE_SIZE_DIP: i32 = 8;
/// How far the pointer must travel before a press becomes a drag, so a click draws nothing.
const DRAG_THRESHOLD_DIP: i32 = 3;
/// How far, in capture pixels, the pointer must move before a stroke takes another point.
const STROKE_STEP: f32 = 0.75;
/// Wide enough for every button and swatch at every scaling.
const MINIMUM_CLIENT_DIP: (i32, i32) = (928, 320);
/// How far a box's corners are rounded: enough to soften them, not enough to read as a pill.
const BOX_RADIUS_DIP: f32 = 6.0;
/// The sizes text can be, in device-independent pixels to the em, smallest first.
const TEXT_SIZES_DIP: [i32; 5] = [14, 18, 24, 32, 48];
/// Where in [`TEXT_SIZES_DIP`] text starts: large enough to stand out from what it annotates.
const DEFAULT_TEXT_SIZE: usize = 2;
/// The widths an arrow can be, in device-independent pixels, thinnest first.
const ARROW_SIZES_DIP: [f32; 6] = [2.0, 3.0, 4.0, 6.0, 8.0, 12.0];
const DEFAULT_ARROW_SIZE: usize = 2;
/// What the last editor was left with, for the next one Captastic opens: the colour, as an index
/// into [`PALETTE`], and the arrow and text sizes, as indexes into theirs. Editors all live on the
/// one desktop UI thread; atomics only make the statics safe to share.
static REMEMBERED_COLOUR: AtomicUsize = AtomicUsize::new(RED);
static REMEMBERED_ARROW_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_ARROW_SIZE);
static REMEMBERED_TEXT_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_TEXT_SIZE);
/// The most one piece of text holds, in bytes, so a held key cannot grow it without end.
const MAXIMUM_TEXT_BYTES: usize = 4096;
/// The share of the display's work area a new editor may take.
const MAXIMUM_WORK_AREA_SHARE: f64 = 0.9;

/// What is outside the crop keeps this share of its brightness, in fifths.
const DIM_FIFTHS: u16 = 2;
const CANVAS_RGB: (u8, u8, u8) = (24, 24, 27);
const ACCENT_RGB: (u8, u8, u8) = (45, 125, 246);

/// The colours a mark can be drawn in, with the name each swatch's tooltip gives.
const PALETTE: [(&str, Color); 7] = [
    ("Red", Color::rgb(229, 57, 53)),
    ("Yellow", Color::rgb(253, 216, 53)),
    ("Green", Color::rgb(67, 160, 71)),
    ("Blue", Color::rgb(30, 136, 229)),
    ("Magenta", Color::rgb(216, 27, 186)),
    ("Black", Color::rgb(0, 0, 0)),
    ("White", Color::rgb(255, 255, 255)),
];
const RED: usize = 0;

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
    Box,
    Arrow,
    Text,
    Pen,
    Marker,
    Highlighter,
}

impl EditorTool {
    const ALL: [Self; 8] = [
        Self::Crop,
        Self::Redact,
        Self::Box,
        Self::Arrow,
        Self::Text,
        Self::Pen,
        Self::Marker,
        Self::Highlighter,
    ];

    /// Whether the tool draws in the colour in hand.
    const fn draws(self) -> bool {
        !matches!(self, Self::Crop | Self::Redact)
    }

    /// How wide the tool draws, in device-independent pixels of the display the capture came
    /// from, so a pen is the same weight on a capture from any scaling.
    const fn width_dip(self) -> f32 {
        match self {
            Self::Pen => 3.0,
            Self::Box | Self::Arrow => 4.0,
            Self::Marker => 10.0,
            Self::Highlighter => 22.0,
            Self::Crop | Self::Redact | Self::Text => 0.0,
        }
    }

    const fn tooltip(self) -> &'static str {
        match self {
            Self::Crop => "Crop (C)",
            Self::Redact => "Redact (R)",
            Self::Box => "Box (B)",
            Self::Arrow => "Arrow (A)",
            Self::Text => "Text (T)",
            Self::Pen => "Pen (P)",
            Self::Marker => "Marker (M)",
            Self::Highlighter => "Highlighter (H)",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorButton {
    Tool(EditorTool),
    /// A colour in [`PALETTE`], by index.
    Swatch(usize),
    /// Thinner arrows or smaller text.
    Smaller,
    /// Thicker arrows or larger text.
    Larger,
    Undo,
    Redo,
    Cancel,
    Done,
}

impl EditorButton {
    fn tooltip(self) -> Option<String> {
        match self {
            Self::Tool(tool) => Some(tool.tooltip().to_owned()),
            Self::Swatch(index) => Some(format!("{} ({})", PALETTE[index].0, index + 1)),
            Self::Smaller => Some("Smaller arrow or text (Ctrl+Minus)".to_owned()),
            Self::Larger => Some("Larger arrow or text (Ctrl+Plus)".to_owned()),
            Self::Undo => Some("Undo (Ctrl+Z)".to_owned()),
            Self::Redo => Some("Redo (Ctrl+Y)".to_owned()),
            Self::Cancel | Self::Done => None,
        }
    }
}

/// Where everything in an editor's client area is, for one client size and DPI.
#[derive(Clone, Debug, PartialEq)]
struct EditorLayout {
    client: UiRect,
    toolbar: UiRect,
    /// Left to right.
    buttons: Vec<(EditorButton, UiRect)>,
    /// Between Redo and Cancel, for the size of the crop; empty when there is no room.
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
        let mut buttons = Vec::with_capacity(16);
        let mut x = client.left + metrics.px(TOOLBAR_PADDING_DIP);
        let mut place = |button: EditorButton, width_dip: i32, gap_dip: i32| {
            let rect = UiRect {
                left: x,
                top,
                right: x + metrics.px(width_dip),
                bottom,
            };
            x = rect.right + metrics.px(gap_dip);
            buttons.push((button, rect));
        };
        // Each group ends with the gap to the next, so the closure is the only thing moving `x`.
        for (index, tool) in EditorTool::ALL.into_iter().enumerate() {
            let last = index + 1 == EditorTool::ALL.len();
            place(
                EditorButton::Tool(tool),
                ICON_BUTTON_WIDTH_DIP,
                if last {
                    GROUP_GAP_DIP
                } else {
                    ICON_BUTTON_GAP_DIP
                },
            );
        }
        for index in 0..PALETTE.len() {
            let last = index + 1 == PALETTE.len();
            place(
                EditorButton::Swatch(index),
                SWATCH_WIDTH_DIP,
                if last { GROUP_GAP_DIP } else { SWATCH_GAP_DIP },
            );
        }
        place(
            EditorButton::Smaller,
            ICON_BUTTON_WIDTH_DIP,
            ICON_BUTTON_GAP_DIP,
        );
        place(EditorButton::Larger, ICON_BUTTON_WIDTH_DIP, GROUP_GAP_DIP);
        place(
            EditorButton::Undo,
            ICON_BUTTON_WIDTH_DIP,
            ICON_BUTTON_GAP_DIP,
        );
        place(EditorButton::Redo, ICON_BUTTON_WIDTH_DIP, 0);
        let left_end = x;
        let done_left = client.right - metrics.px(TOOLBAR_PADDING_DIP + TEXT_BUTTON_WIDTH_DIP);
        let cancel_left = done_left - metrics.px(BUTTON_GAP_DIP + TEXT_BUTTON_WIDTH_DIP);
        for (button, left) in [
            (EditorButton::Cancel, cancel_left),
            (EditorButton::Done, done_left),
        ] {
            buttons.push((
                button,
                UiRect {
                    left,
                    top,
                    right: left + metrics.px(TEXT_BUTTON_WIDTH_DIP),
                    bottom,
                },
            ));
        }
        let label_left = left_end + metrics.px(GROUP_GAP_DIP);
        let size_label = UiRect {
            left: label_left,
            top,
            right: (cancel_left - metrics.px(GROUP_GAP_DIP)).max(label_left),
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
            buttons,
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

    fn button_rect(&self, wanted: EditorButton) -> Option<UiRect> {
        self.buttons
            .iter()
            .find(|(button, _)| *button == wanted)
            .map(|(_, rect)| *rect)
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
#[derive(Clone, Debug)]
enum Drag {
    /// Drawing between two points: a redaction, a box, an arrow, or a crop from scratch.
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
    /// A freehand stroke with the pen, marker or highlighter, in capture pixels.
    Stroke {
        tool: EditorTool,
        points: Vec<(f32, f32)>,
    },
    /// Moving one end of the arrow at `index` among the edit's marks.
    ArrowEnd {
        index: usize,
        /// The point, rather than the tail.
        tip: bool,
        current: POINT,
    },
}

/// Text being typed, not yet a mark: it is shown as the mark it would be, and becomes one on
/// Enter, on a click elsewhere, or on any toolbar button but a colour.
#[derive(Clone, Debug)]
struct TextEntry {
    /// The capture pixel clicked, where the first line starts and which it is centred on.
    anchor: (f64, f64),
    text: String,
    /// The first half of a character from beyond the Basic Multilingual Plane, such as an emoji,
    /// waiting for its second.
    high_surrogate: Option<u16>,
    layout: TextLayout,
    /// Pixels to the em, in the capture.
    size: i32,
    color: Color,
    /// The text mark this was opened from, which it replaces when finished.
    replacing: Option<usize>,
}

impl TextEntry {
    /// The first line's top-left, in capture pixels.
    fn origin(&self) -> (i32, i32) {
        (
            self.anchor.0.round() as i32,
            (self.anchor.1 - f64::from(self.layout.line_height) / 2.0).round() as i32,
        )
    }

    fn mark(&self) -> Option<Mark> {
        let mask = self.layout.mask.clone()?;
        let (x, y) = self.origin();
        Some(Mark::Text {
            at: (x + self.layout.mask_offset.0, y + self.layout.mask_offset.1),
            text: self.text.clone(),
            size: self.size.max(1) as u32,
            color: self.color,
            mask,
        })
    }

    /// Where the caret is, in capture pixels: one pixel wide and a line tall.
    fn caret(&self) -> Rect {
        let (x, y) = self.origin();
        let (left, top, height) = self.layout.caret;
        Rect {
            x: x + left,
            y: y + top,
            width: 1,
            height: height.max(1) as u32,
        }
    }
}

/// One editor's state, owned by its window.
struct EditorState {
    edit: Edit,
    metrics: UiMetrics,
    layout: EditorLayout,
    /// The capture with every committed mark, as shown; the preview is this plus the mark being
    /// drawn.
    committed: Vec<u8>,
    preview: FrozenSurface,
    /// The part of the preview the mark being drawn has changed, to restore from `committed`.
    pending_dirty: Option<Rect>,
    back_buffer: Option<FrozenSurface>,
    tool: EditorTool,
    /// The drawing tool a swatch picks up while Crop or Redact is chosen.
    last_drawing_tool: EditorTool,
    /// The palette index every drawing tool draws in.
    colour_index: usize,
    /// The text being typed, if any.
    typing: Option<TextEntry>,
    /// The size text is typed at, as an index into [`TEXT_SIZES_DIP`].
    text_size: usize,
    /// The width arrows are drawn at, as an index into [`ARROW_SIZES_DIP`].
    arrow_size: usize,
    /// Capture pixels per device-independent pixel on the display the capture came from.
    stroke_unit: f32,
    drag: Option<Drag>,
    pressed: Option<EditorButton>,
    hovered: Option<EditorButton>,
    tracking_leave: bool,
    on_finish: Option<EditFinished>,
}

impl EditorState {
    /// The crop a drag in progress would make, if it ended here.
    fn pending_crop(&self) -> Option<Rect> {
        let bounds = self.edit.bounds();
        match self.drag.as_ref()? {
            Drag::Draw {
                tool: EditorTool::Crop,
                anchor,
                current,
                moved: true,
            } => rect_between(
                self.layout.image_point(*anchor),
                self.layout.image_point(*current),
            )?
            .intersection(bounds),
            Drag::Crop {
                grip,
                start,
                anchor,
                current,
            } => {
                let dx = (f64::from(current.x - anchor.x) / self.layout.scale).round() as i64;
                let dy = (f64::from(current.y - anchor.y) / self.layout.scale).round() as i64;
                Some(adjust_crop(*start, *grip, dx, dy, bounds))
            }
            _ => None,
        }
    }

    /// The mark a drag in progress would make, if it ended here, or the text being typed.
    fn pending_mark(&self) -> Option<Mark> {
        let Some(drag) = self.drag.as_ref() else {
            return self.typing.as_ref().and_then(TextEntry::mark);
        };
        match drag {
            Drag::Draw {
                tool,
                anchor,
                current,
                moved: true,
            } => {
                let (from, to) = (
                    self.layout.image_point(*anchor),
                    self.layout.image_point(*current),
                );
                if *tool == EditorTool::Arrow {
                    return Some(Mark::Arrow {
                        from: (from.0 as f32, from.1 as f32),
                        to: (to.0 as f32, to.1 as f32),
                        color: self.colour(),
                        width: self.width(*tool),
                    });
                }
                let rect = rect_between(from, to)?;
                match tool {
                    EditorTool::Redact => Some(Mark::Redaction(rect)),
                    EditorTool::Box => Some(Mark::Rectangle {
                        rect,
                        color: self.colour(),
                        width: self.width(*tool).round().max(1.0) as u32,
                        radius: BOX_RADIUS_DIP * self.stroke_unit,
                    }),
                    _ => None,
                }
            }
            Drag::ArrowEnd {
                index,
                tip,
                current,
            } => {
                let Some(Mark::Arrow {
                    from,
                    to,
                    color,
                    width,
                }) = self.edit.marks().get(*index)
                else {
                    return None;
                };
                let (x, y) = self.layout.image_point(*current);
                let moved = (x as f32, y as f32);
                Some(Mark::Arrow {
                    from: if *tip { *from } else { moved },
                    to: if *tip { moved } else { *to },
                    color: *color,
                    width: *width,
                })
            }
            Drag::Stroke { tool, points } => Some(Mark::Stroke {
                points: points.clone(),
                color: self.colour(),
                width: self.width(*tool),
                style: if *tool == EditorTool::Highlighter {
                    StrokeStyle::Highlighter
                } else {
                    StrokeStyle::Ink
                },
            }),
            _ => None,
        }
    }

    fn colour(&self) -> Color {
        PALETTE[self.colour_index].1
    }

    fn width(&self, tool: EditorTool) -> f32 {
        let dip = if tool == EditorTool::Arrow {
            ARROW_SIZES_DIP[self.arrow_size]
        } else {
            tool.width_dip()
        };
        (dip * self.stroke_unit).max(1.0)
    }

    /// Where a step of `step` would take the size of the tool in hand, if it has a size and the
    /// step stays within its range.
    fn stepped_size(&self, step: i32) -> Option<usize> {
        let (current, count) = match self.tool {
            EditorTool::Arrow => (self.arrow_size, ARROW_SIZES_DIP.len()),
            EditorTool::Text => (self.text_size, TEXT_SIZES_DIP.len()),
            _ => return None,
        };
        let next = current as i32 + step;
        (0..count as i32).contains(&next).then_some(next as usize)
    }

    /// The size text is typed at, in capture pixels to the em.
    fn text_pixels(&self) -> i32 {
        ((TEXT_SIZES_DIP[self.text_size] as f32 * self.stroke_unit).round() as i32).max(1)
    }

    /// Whether there is anything to lose: an edit made, or text typed and not yet committed.
    fn has_changes(&self) -> bool {
        self.edit.is_modified()
            || self
                .typing
                .as_ref()
                .is_some_and(|entry| !entry.text.is_empty())
    }

    fn button_enabled(&self, button: EditorButton) -> bool {
        match button {
            EditorButton::Smaller => self.stepped_size(-1).is_some(),
            EditorButton::Larger => self.stepped_size(1).is_some(),
            EditorButton::Undo => self.edit.can_undo(),
            EditorButton::Redo => self.edit.can_redo(),
            _ => true,
        }
    }

    fn choose_tool(&mut self, tool: EditorTool) {
        self.tool = tool;
        if tool.draws() {
            self.last_drawing_tool = tool;
        }
    }

    /// A swatch sets the colour every drawing tool draws in, and recolours text being typed. With
    /// Crop or Redact in hand it picks up the last drawing tool, since choosing a colour means
    /// meaning to draw.
    fn choose_colour(&mut self, index: usize) {
        if !self.tool.draws() {
            self.choose_tool(self.last_drawing_tool);
        }
        self.colour_index = index;
        REMEMBERED_COLOUR.store(index, Ordering::Relaxed);
        if let Some(entry) = self.typing.as_mut() {
            entry.color = PALETTE[index].1;
        }
    }

    /// The end of an arrow under a client point, topmost first, and whether it is the point.
    fn arrow_end_at(&self, point: POINT) -> Option<(usize, bool)> {
        let reach = f64::from(self.metrics.px(GRIP_TOLERANCE_DIP));
        let near = |end: (f32, f32)| {
            let x = f64::from(self.layout.image.left) + f64::from(end.0) * self.layout.scale;
            let y = f64::from(self.layout.image.top) + f64::from(end.1) * self.layout.scale;
            (x - f64::from(point.x)).hypot(y - f64::from(point.y)) <= reach
        };
        self.edit
            .marks()
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, mark)| match mark {
                Mark::Arrow { to, .. } if near(*to) => Some((index, true)),
                Mark::Arrow { from, .. } if near(*from) => Some((index, false)),
                _ => None,
            })
    }

    /// The text mark under a capture pixel, topmost first.
    fn text_at(&self, at: (f64, f64)) -> Option<usize> {
        let (x, y) = (at.0.floor() as i64, at.1.floor() as i64);
        self.edit
            .marks()
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, mark)| {
                let bounds = match mark {
                    Mark::Text { .. } => mark.bounds()?,
                    _ => return None,
                };
                let inside = x >= i64::from(bounds.x)
                    && x < bounds.right()
                    && y >= i64::from(bounds.y)
                    && y < bounds.bottom();
                inside.then_some(index)
            })
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
    let committed = compose_preview(&frame, &[]);
    let preview = FrozenSurface::new(width, height, &committed)?;
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
        committed,
        preview,
        pending_dirty: None,
        back_buffer: None,
        tool: EditorTool::Crop,
        last_drawing_tool: EditorTool::Pen,
        // Red ink for the box, pen, marker and arrow; a highlighter is yellow.
        colour_index: REMEMBERED_COLOUR
            .load(Ordering::Relaxed)
            .min(PALETTE.len() - 1),
        arrow_size: REMEMBERED_ARROW_SIZE
            .load(Ordering::Relaxed)
            .min(ARROW_SIZES_DIP.len() - 1),
        typing: None,
        text_size: REMEMBERED_TEXT_SIZE
            .load(Ordering::Relaxed)
            .min(TEXT_SIZES_DIP.len() - 1),
        stroke_unit: metrics.dpi as f32 / UiMetrics::BASE_DPI as f32,
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
            let dropped = abandon_drag(state);
            if dropped || state.pressed.take().is_some() {
                show_pending(state);
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            key_pressed(hwnd, state_pointer, wparam.0 as u16);
            LRESULT(0)
        }
        WM_CHAR => {
            // SAFETY: As above.
            char_typed(hwnd, unsafe { &mut *state_pointer }, wparam.0 as u16);
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
        match &state.drag {
            Some(Drag::Crop { grip, .. }) => grip_cursor(*grip),
            Some(Drag::ArrowEnd { .. }) => IDC_SIZEALL,
            Some(_) => IDC_CROSS,
            None => match state.tool {
                EditorTool::Crop => grip_at(
                    state.layout.client_rect(state.edit.crop()),
                    point,
                    state.metrics.px(GRIP_TOLERANCE_DIP),
                    state.edit.crop() != state.edit.bounds(),
                )
                .map_or(IDC_CROSS, grip_cursor),
                EditorTool::Text => IDC_IBEAM,
                EditorTool::Arrow if state.arrow_end_at(point).is_some() => IDC_SIZEALL,
                _ => IDC_CROSS,
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
    let image_point = state.layout.image_point(point);
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
        Some(Drag::Crop { current, .. } | Drag::ArrowEnd { current, .. }) => {
            *current = point;
            changed = true;
        }
        Some(Drag::Stroke { points, .. }) => {
            let next = (image_point.0 as f32, image_point.1 as f32);
            let far_enough = points.last().is_none_or(|last| {
                (next.0 - last.0).abs() >= STROKE_STEP || (next.1 - last.1).abs() >= STROKE_STEP
            });
            if far_enough {
                points.push(next);
                changed = true;
            }
        }
        None => {}
    }
    if changed {
        if state.drag.is_some() {
            show_pending(state);
        }
        invalidate(hwnd);
    }
}

fn pointer_pressed(hwnd: HWND, state: &mut EditorState, point: POINT) {
    if let Some(button) = state.layout.button_at(point) {
        // A colour recolours the text being typed; anything else finishes it first, so Done
        // publishes it and Undo takes it back.
        if !matches!(button, EditorButton::Swatch(_)) {
            commit_typing(state);
        }
        if state.button_enabled(button) {
            state.pressed = Some(button);
            capture_pointer(hwnd);
        }
        return;
    }
    if !state.layout.canvas.contains(point) {
        return;
    }
    let draw = |tool| Drag::Draw {
        tool,
        anchor: point,
        current: point,
        moved: false,
    };
    if state.tool == EditorTool::Arrow {
        if let Some((index, tip)) = state.arrow_end_at(point) {
            state.drag = Some(Drag::ArrowEnd {
                index,
                tip,
                current: point,
            });
            leave_out_of_preview(state, index);
            show_pending(state);
            capture_pointer(hwnd);
            invalidate(hwnd);
            return;
        }
    }
    let drag = match state.tool {
        EditorTool::Text => {
            start_typing(state, state.layout.image_point(point));
            invalidate(hwnd);
            return;
        }
        EditorTool::Redact | EditorTool::Box | EditorTool::Arrow => draw(state.tool),
        EditorTool::Pen | EditorTool::Marker | EditorTool::Highlighter => {
            let (x, y) = state.layout.image_point(point);
            Drag::Stroke {
                tool: state.tool,
                points: vec![(x as f32, y as f32)],
            }
        }
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
                None => draw(EditorTool::Crop),
            }
        }
    };
    state.drag = Some(drag);
    show_pending(state);
    capture_pointer(hwnd);
    invalidate(hwnd);
}

/// Ends a press. Returns the toolbar button to act on, if the press was one and was released over
/// it; a drag is committed here.
fn pointer_released(hwnd: HWND, state: &mut EditorState, point: POINT) -> Option<EditorButton> {
    let crop = state.pending_crop();
    let mark = state.pending_mark();
    let drag = state.drag.take();
    let pressed = state.pressed.take();
    // Taken before releasing: the release sends WM_CAPTURECHANGED, which would abandon both.
    release_pointer_capture();
    if let Some(Drag::ArrowEnd { index, .. }) = drag {
        if let Some(mark) = mark {
            state.edit.replace(index, Some(mark));
        }
        refresh_preview(state);
        invalidate(hwnd);
    } else if drag.is_some() {
        if let Some(crop) = crop {
            state.edit.set_crop(crop);
        }
        if let Some(mark) = mark {
            commit_mark(state, mark);
        }
        show_pending(state);
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
            EditorButton::Tool(tool) => state.choose_tool(tool),
            EditorButton::Swatch(index) => state.choose_colour(index),
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
            EditorButton::Smaller => resize(state, -1),
            EditorButton::Larger => resize(state, 1),
            EditorButton::Cancel | EditorButton::Done => {}
        }
    }
    match button {
        EditorButton::Cancel => request_cancel(hwnd, state_pointer),
        EditorButton::Done => finish_done(hwnd, state_pointer),
        _ => invalidate(hwnd),
    }
}

/// The toolbar button a key stands for, without Ctrl unless it says so.
fn key_button(key: u16, ctrl: bool, shift: bool) -> Option<EditorButton> {
    let tool = |tool| Some(EditorButton::Tool(tool));
    match (key, ctrl) {
        _ if key == VK_RETURN.0 => Some(EditorButton::Done),
        (0x5A, true) if shift => Some(EditorButton::Redo),
        (0x5A, true) => Some(EditorButton::Undo),
        (0x59, true) => Some(EditorButton::Redo),
        (0x43, false) => tool(EditorTool::Crop),
        (0x52, false) => tool(EditorTool::Redact),
        (0x42, false) => tool(EditorTool::Box),
        (0x41, false) => tool(EditorTool::Arrow),
        (0x54, false) => tool(EditorTool::Text),
        (0x50, false) => tool(EditorTool::Pen),
        (0x4D, false) => tool(EditorTool::Marker),
        (0x48, false) => tool(EditorTool::Highlighter),
        // 1 to 7 on the main row or the number pad, one for each colour.
        (0x31..=0x37, false) => Some(EditorButton::Swatch(usize::from(key - 0x31))),
        (0x61..=0x67, false) => Some(EditorButton::Swatch(usize::from(key - 0x61))),
        _ => None,
    }
}

/// Ctrl with plus or minus, on the main row or the number pad: a step up or down in text size.
fn text_size_step(key: u16, ctrl: bool) -> Option<i32> {
    if !ctrl {
        None
    } else if key == VK_OEM_PLUS.0 || key == VK_ADD.0 {
        Some(1)
    } else if key == VK_OEM_MINUS.0 || key == VK_SUBTRACT.0 {
        Some(-1)
    } else {
        None
    }
}

fn key_pressed(hwnd: HWND, state_pointer: *mut EditorState, key: u16) {
    let held = modifiers();
    {
        // SAFETY: The editor's state lives until WM_NCDESTROY on this thread; the borrow ends
        // with this block.
        let state = unsafe { &mut *state_pointer };
        if let Some(step) = text_size_step(key, held.ctrl) {
            if state.drag.is_none() {
                resize(state, step);
                invalidate(hwnd);
            }
            return;
        }
        if state.typing.is_some() {
            // Keys type while text is open: none of them is a tool, a colour or a shortcut.
            if key == VK_ESCAPE.0 {
                discard_typing(state);
            } else if key == VK_RETURN.0 && held.shift {
                type_text(state, "\n");
            } else if key == VK_RETURN.0 {
                commit_typing(state);
            }
            invalidate(hwnd);
            return;
        }
    }
    if key == VK_ESCAPE.0 {
        // SAFETY: The editor's state lives until WM_NCDESTROY on this thread; the borrow ends
        // before the cancel below.
        let state = unsafe { &mut *state_pointer };
        if abandon_drag(state) {
            // Escape mid-drag drops the drag, not the edit.
            show_pending(state);
            release_pointer_capture();
            invalidate(hwnd);
        } else {
            request_cancel(hwnd, state_pointer);
        }
        return;
    }
    if let Some(button) = key_button(key, held.ctrl, held.shift) {
        // SAFETY: As above.
        let dragging = unsafe { (*state_pointer).drag.is_some() };
        if !dragging {
            press_button(hwnd, state_pointer, button);
        }
    }
}

/// The capture's whole area, which the preview buffers cover.
fn preview_area(state: &EditorState) -> Rect {
    state.edit.bounds()
}

/// Rebuilds the preview from every committed mark, after an undo or redo.
fn refresh_preview(state: &mut EditorState) {
    state.committed = compose_preview(state.edit.source(), state.edit.marks());
    state.pending_dirty = None;
    // SAFETY: Completes any queued GDI read of the preview before its pixels are replaced.
    let _ = unsafe { GdiFlush() };
    if let Err(error) = state.preview.write_pixels(&state.committed) {
        log::warn!("could not redraw the editor's preview: {error}");
    }
}

/// Adds a finished mark to the edit, and to the committed preview by painting just that mark.
fn commit_mark(state: &mut EditorState, mark: Mark) {
    if !state.edit.add(mark.clone()) {
        return;
    }
    let area = preview_area(state);
    paint_mark(
        &mark,
        &mut state.committed,
        area.width as usize * 4,
        area,
        PixelFormat::Bgra8Unorm,
        FrameAlpha::Opaque,
    );
    // Marked for the next show_pending to copy from `committed`, together with whatever was
    // already waiting, so the preview shows the mark exactly whether or not it was drawn as a
    // pending mark first.
    let reach = mark.bounds().and_then(|bounds| bounds.intersection(area));
    state.pending_dirty = match (state.pending_dirty, reach) {
        (Some(dirty), Some(reach)) => Some(union(dirty, reach)),
        (dirty, reach) => dirty.or(reach),
    };
}

/// The smallest rectangle holding both.
fn union(a: Rect, b: Rect) -> Rect {
    Rect::from_edges(
        i64::from(a.x.min(b.x)),
        i64::from(a.y.min(b.y)),
        a.right().max(b.right()),
        a.bottom().max(b.bottom()),
    )
    .unwrap_or(a)
}

/// Makes the preview show the committed marks plus the mark being drawn, if any, touching only the
/// pixels the last pending mark and this one cover.
fn show_pending(state: &mut EditorState) {
    let area = preview_area(state);
    let stride = area.width as usize * 4;
    // SAFETY: Completes any queued GDI read of the preview before its pixels change.
    let _ = unsafe { GdiFlush() };
    // SAFETY: The preview's DIB holds exactly byte_length writable bytes, owned by this thread.
    let pixels =
        unsafe { std::slice::from_raw_parts_mut(state.preview.bits, state.preview.byte_length) };
    if let Some(dirty) = state.pending_dirty.take() {
        for row in dirty.y..(dirty.bottom() as i32) {
            let start = row as usize * stride + dirty.x as usize * 4;
            let end = start + dirty.width as usize * 4;
            pixels[start..end].copy_from_slice(&state.committed[start..end]);
        }
    }
    if let Some(mark) = state.pending_mark() {
        paint_mark(
            &mark,
            pixels,
            stride,
            area,
            PixelFormat::Bgra8Unorm,
            FrameAlpha::Opaque,
        );
        state.pending_dirty = mark.bounds().and_then(|bounds| bounds.intersection(area));
    }
}

/// Makes the tool in hand a step larger or smaller. Text being typed takes the new size, and so
/// does an arrow when the last thing done was drawing or moving it, so what was just drawn can be
/// adjusted without drawing it again.
fn resize(state: &mut EditorState, step: i32) {
    let Some(size) = state.stepped_size(step) else {
        return;
    };
    match state.tool {
        EditorTool::Text => {
            state.text_size = size;
            REMEMBERED_TEXT_SIZE.store(size, Ordering::Relaxed);
            let pixels = state.text_pixels();
            if let Some(entry) = state.typing.as_mut() {
                entry.size = pixels;
            }
            retype(state);
        }
        EditorTool::Arrow => {
            state.arrow_size = size;
            REMEMBERED_ARROW_SIZE.store(size, Ordering::Relaxed);
            let width = state.width(EditorTool::Arrow);
            let last = state.edit.last_changed();
            if let Some((
                index,
                Some(Mark::Arrow {
                    from, to, color, ..
                }),
            )) = last.map(|index| (index, state.edit.marks().get(index).cloned()))
            {
                let thicker = Mark::Arrow {
                    from,
                    to,
                    color,
                    width,
                };
                if state.edit.replace(index, Some(thicker)) {
                    refresh_preview(state);
                }
            }
        }
        _ => {}
    }
}

/// Opens text at `at`, a capture pixel, finishing any text already open.
///
/// A click on text already there opens that text instead, with the caret at its end, to be
/// edited where it is, in its own size and colour.
fn start_typing(state: &mut EditorState, at: (f64, f64)) {
    commit_typing(state);
    if let Some(index) = state.text_at(at) {
        if let Some(Mark::Text {
            at,
            text,
            size,
            color,
            ..
        }) = state.edit.marks().get(index).cloned()
        {
            match layout_text(&text, size as i32) {
                Ok(layout) => {
                    // The inverse of TextEntry::origin and TextEntry::mark.
                    let origin = (at.0 - layout.mask_offset.0, at.1 - layout.mask_offset.1);
                    state.typing = Some(TextEntry {
                        anchor: (
                            f64::from(origin.0),
                            f64::from(origin.1) + f64::from(layout.line_height) / 2.0,
                        ),
                        text,
                        high_surrogate: None,
                        layout,
                        size: size as i32,
                        color,
                        replacing: Some(index),
                    });
                    leave_out_of_preview(state, index);
                    show_pending(state);
                }
                Err(error) => log::warn!("could not reopen text in the editor: {error}"),
            }
            return;
        }
    }
    let size = state.text_pixels();
    match layout_text("", size) {
        Ok(layout) => {
            state.typing = Some(TextEntry {
                anchor: at,
                text: String::new(),
                high_surrogate: None,
                layout,
                size,
                color: state.colour(),
                replacing: None,
            });
        }
        Err(error) => log::warn!("could not start typing in the editor: {error}"),
    }
}

/// Makes the text being typed a mark, if it shows anything. Reopened text replaces the mark it
/// came from, and text emptied out removes it.
fn commit_typing(state: &mut EditorState) {
    let Some(entry) = state.typing.take() else {
        return;
    };
    let mark = entry.mark();
    match entry.replacing {
        Some(index) => {
            state.edit.replace(index, mark);
            refresh_preview(state);
        }
        None => {
            if let Some(mark) = mark {
                commit_mark(state, mark);
            }
            show_pending(state);
        }
    }
}

/// Throws away the text being typed; reopened text goes back to how it was.
fn discard_typing(state: &mut EditorState) {
    if let Some(entry) = state.typing.take() {
        if entry.replacing.is_some() {
            refresh_preview(state);
        } else {
            show_pending(state);
        }
    }
}

/// Drops the drag in progress, if any, putting back a mark it had taken out of the preview.
/// Returns whether there was one.
fn abandon_drag(state: &mut EditorState) -> bool {
    match state.drag.take() {
        Some(Drag::ArrowEnd { .. }) => {
            refresh_preview(state);
            true
        }
        Some(_) => true,
        None => false,
    }
}

/// Shows every committed mark but the one at `index`, which is being changed and is drawn as the
/// pending mark meanwhile, over the others.
fn leave_out_of_preview(state: &mut EditorState, index: usize) {
    let others: Vec<Mark> = state
        .edit
        .marks()
        .iter()
        .enumerate()
        .filter(|(at, _)| *at != index)
        .map(|(_, mark)| mark.clone())
        .collect();
    state.committed = compose_preview(state.edit.source(), &others);
    state.pending_dirty = None;
    // SAFETY: Completes any queued GDI read of the preview before its pixels are replaced.
    let _ = unsafe { GdiFlush() };
    if let Err(error) = state.preview.write_pixels(&state.committed) {
        log::warn!("could not redraw the editor's preview: {error}");
    }
}

/// Lays the text being typed out again, after it or its size changed, and shows it.
fn retype(state: &mut EditorState) {
    if let Some(entry) = state.typing.as_mut() {
        match layout_text(&entry.text, entry.size) {
            Ok(layout) => entry.layout = layout,
            Err(error) => log::warn!("could not lay out the editor's text: {error}"),
        }
    }
    show_pending(state);
}

fn type_text(state: &mut EditorState, value: &str) {
    if let Some(entry) = state.typing.as_mut() {
        if entry.text.len() + value.len() <= MAXIMUM_TEXT_BYTES {
            entry.text.push_str(value);
        }
    }
    retype(state);
}

/// A character typed, one UTF-16 unit at a time, as WM_CHAR delivers it.
fn char_typed(hwnd: HWND, state: &mut EditorState, unit: u16) {
    let Some(entry) = state.typing.as_mut() else {
        return;
    };
    match unit {
        0x08 => {
            entry.high_surrogate = None;
            entry.text.pop();
            retype(state);
        }
        0xD800..=0xDBFF => entry.high_surrogate = Some(unit),
        0xDC00..=0xDFFF => {
            let Some(high) = entry.high_surrogate.take() else {
                return;
            };
            if let Some(Ok(character)) = char::decode_utf16([high, unit]).next() {
                type_text(state, character.encode_utf8(&mut [0; 4]));
            }
        }
        // Enter, Tab, Escape and Ctrl with a letter arrive as control characters: they are keys,
        // handled as keys, not text.
        _ if unit < 0x20 || unit == 0x7F => return,
        _ => {
            entry.high_surrogate = None;
            if let Some(character) = char::from_u32(u32::from(unit)) {
                type_text(state, character.encode_utf8(&mut [0; 4]));
            }
        }
    }
    invalidate(hwnd);
}

/// Cancels, after asking first when there are edits to lose.
fn request_cancel(hwnd: HWND, state_pointer: *mut EditorState) {
    // SAFETY: The editor's state lives until WM_NCDESTROY on this thread; read and released.
    let modified = unsafe { (*state_pointer).has_changes() };
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
    let rendered = unsafe {
        let state = &mut *state_pointer;
        commit_typing(state);
        state.edit.render()
    };
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
    let layout = &state.layout;
    let metrics = state.metrics;
    let canvas = rgb(CANVAS_RGB.0, CANVAS_RGB.1, CANVAS_RGB.2);
    let accent = rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2);
    fill_device_rect(device, native(layout.client), canvas);
    draw_surface_to_rect(device, &state.preview, layout.image);

    let crop = state.pending_crop().unwrap_or_else(|| state.edit.crop());
    let crop_on_screen = layout.client_rect(crop);
    // SAFETY: Completes every queued GDI draw before the pixels are touched directly.
    let _ = unsafe { GdiFlush() };
    // SAFETY: The surface's DIB holds exactly byte_length writable bytes, owned by this thread.
    let pixels = unsafe { std::slice::from_raw_parts_mut(surface.bits, surface.byte_length) };
    dim_outside(pixels, surface.width as usize, layout.image, crop_on_screen);

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
    if state.tool == EditorTool::Arrow {
        draw_arrow_handles(device, state, accent);
    }
    if let Some(entry) = &state.typing {
        // A frame around the text being typed, so it is plain which text is open.
        let caret = entry.caret();
        let extent = entry
            .mark()
            .and_then(|mark| mark.bounds())
            .map_or(caret, |bounds| union(bounds, caret));
        let frame = layout.client_rect(extent);
        let margin = metrics.px(4);
        draw_outline_rect(
            device,
            frame.left - margin,
            frame.top - margin,
            frame.right + margin,
            frame.bottom + margin,
            accent,
            metrics.px(1).max(1),
        );
        draw_typing_hint(
            device,
            state,
            UiRect {
                left: frame.left - margin,
                top: frame.top - margin,
                right: frame.right + margin,
                bottom: frame.bottom + margin,
            },
        );
        let caret = layout.client_rect(caret);
        let colour = entry.color;
        fill_device_rect(
            device,
            native(UiRect {
                right: caret.left + metrics.px(2).max(1),
                bottom: caret.bottom.max(caret.top + 1),
                ..caret
            }),
            rgb(colour.red, colour.green, colour.blue),
        );
    }
    draw_toolbar(device, state, crop);
    paint_icons(state, surface);
    draw_tooltip(device, state);
}

/// A handle on each end of every arrow, so it is plain they can be taken hold of. The arrow
/// being changed shows its ends where they are going.
fn draw_arrow_handles(device: HDC, state: &EditorState, accent: COLORREF) {
    let metrics = state.metrics;
    let layout = &state.layout;
    let half = metrics.px(HANDLE_SIZE_DIP) / 2;
    let border = metrics.px(1).max(1);
    let moving = match &state.drag {
        Some(Drag::ArrowEnd { index, .. }) => Some(*index),
        _ => None,
    };
    let pending = moving.and_then(|_| state.pending_mark());
    let marks = state.edit.marks().iter().enumerate();
    for (index, mark) in marks {
        let shown = if moving == Some(index) {
            pending.as_ref()
        } else {
            Some(mark)
        };
        let Some(Mark::Arrow { from, to, .. }) = shown else {
            continue;
        };
        for end in [*from, *to] {
            let x = layout.image.left + (f64::from(end.0) * layout.scale).round() as i32;
            let y = layout.image.top + (f64::from(end.1) * layout.scale).round() as i32;
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
}

fn draw_crop_handles(device: HDC, crop: UiRect, metrics: UiMetrics, accent: COLORREF) {
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

fn draw_toolbar(device: HDC, state: &EditorState, crop: Rect) {
    let layout = &state.layout;
    let metrics = state.metrics;
    let tokens = metrics.toolbar_tokens();
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
        let (button, bounds) = (*button, *bounds);
        let enabled = state.button_enabled(button);
        let hovered = enabled && state.hovered == Some(button);
        if let EditorButton::Swatch(_) = button {
            // Painted antialiased after GDI is done: see paint_icons.
            continue;
        }
        let selected = button == EditorButton::Tool(state.tool);
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
        let ink = if enabled {
            rgb(245, 245, 247)
        } else {
            rgb(110, 110, 116)
        };
        match button {
            EditorButton::Tool(tool) => draw_tool_icon(device, bounds, tool, ink, metrics),
            EditorButton::Undo => draw_history_icon(device, bounds, false, ink, metrics),
            EditorButton::Redo => draw_history_icon(device, bounds, true, ink, metrics),
            EditorButton::Smaller => draw_size_icon(device, bounds, false, ink, metrics),
            EditorButton::Larger => draw_size_icon(device, bounds, true, ink, metrics),
            EditorButton::Cancel | EditorButton::Done => draw_text(
                device,
                bounds,
                if button == EditorButton::Done {
                    "Done"
                } else {
                    "Cancel"
                },
                ink,
                TextAlignment::Center,
                tokens.font_height,
            ),
            EditorButton::Swatch(_) => {}
        }
    }
    if layout.size_label.right - layout.size_label.left >= metrics.px(72) {
        draw_text(
            device,
            layout.size_label,
            &format!("{} \u{00d7} {}", crop.width, crop.height),
            rgb(170, 170, 176),
            TextAlignment::Center,
            tokens.font_height,
        );
    }
}

/// The toolbar's antialiased icons: the marker and highlighter, each showing the colour it draws
/// in, and the colour swatches. Painted into the back buffer's pixels once GDI has finished with
/// it, and before the tooltip, which may sit over them.
fn paint_icons(state: &EditorState, surface: &FrozenSurface) {
    // SAFETY: Completes every queued GDI draw before the pixels are touched directly.
    let _ = unsafe { GdiFlush() };
    // SAFETY: The surface's DIB holds exactly byte_length writable bytes, owned by this thread.
    let pixels = unsafe { std::slice::from_raw_parts_mut(surface.bits, surface.byte_length) };
    let mut painter = Painter::new(pixels, surface.width as usize);
    let metrics = state.metrics;
    let chosen_colour = Some(state.colour_index);
    let ink = bgr(245, 245, 247);
    for (button, bounds) in &state.layout.buttons {
        let hovered = state.hovered == Some(*button);
        match *button {
            EditorButton::Swatch(index) => paint_swatch(
                &mut painter,
                *bounds,
                index,
                chosen_colour == Some(index),
                hovered,
                metrics,
            ),
            EditorButton::Tool(EditorTool::Pen) => paint_pen(
                &mut painter,
                *bounds,
                ink,
                colour_bgr(state.colour()),
                metrics,
            ),
            EditorButton::Tool(EditorTool::Marker) => paint_marker(
                &mut painter,
                *bounds,
                ink,
                colour_bgr(state.colour()),
                metrics,
            ),
            EditorButton::Tool(EditorTool::Highlighter) => paint_highlighter(
                &mut painter,
                *bounds,
                ink,
                colour_bgr(state.colour()),
                metrics,
            ),
            EditorButton::Tool(EditorTool::Arrow) => {
                paint_arrow(&mut painter, *bounds, ink, metrics)
            }
            EditorButton::Tool(EditorTool::Text) => {
                paint_text_icon(&mut painter, *bounds, ink, metrics)
            }
            _ => {}
        }
    }
}

const fn colour_bgr(colour: Color) -> Bgr {
    bgr(colour.red, colour.green, colour.blue)
}

fn icon_frame(bounds: UiRect, metrics: UiMetrics) -> (f32, f32, f32) {
    let (left, top, size) = icon_box(bounds, metrics);
    (left as f32, top as f32, size as f32)
}

/// A slim pen on the diagonal with a conical nib, its point in the colour it draws.
fn paint_pen(painter: &mut Painter, bounds: UiRect, ink: Bgr, point: Bgr, metrics: UiMetrics) {
    let (left, top, size) = icon_frame(bounds, metrics);
    let pen = Diagonal::new(left, top, size, 0.06);
    let half = 0.11;
    // The nib: a cone in the body colour, its point in the ink.
    painter.polygon(
        &[pen.at(0.0, 0.0), pen.at(0.30, -half), pen.at(0.30, half)],
        ink,
        1.0,
    );
    painter.polygon(
        &[pen.at(0.0, 0.0), pen.at(0.13, -0.05), pen.at(0.13, 0.05)],
        point,
        1.0,
    );
    painter.polygon(
        &pen.quad([(0.30, -half), (0.38, -half), (0.38, half), (0.30, half)]),
        bgr(160, 160, 166),
        1.0,
    );
    painter.polygon(
        &pen.quad([(0.38, -half), (0.86, -half), (0.86, half), (0.38, half)]),
        ink,
        1.0,
    );
    painter.capsule(pen.at(0.86, 0.0), pen.at(0.90, 0.0), half * size, ink, 1.0);
}

/// A chisel-tipped marker on the diagonal, its tip in the colour it draws.
fn paint_marker(painter: &mut Painter, bounds: UiRect, ink: Bgr, tip: Bgr, metrics: UiMetrics) {
    let (left, top, size) = icon_frame(bounds, metrics);
    let pen = Diagonal::new(left, top, size, 0.06);
    let half = 0.17;
    // A rim of the body colour around the tip, so a black or dark tip still reads on the toolbar.
    painter.polygon(
        &pen.quad([(-0.03, -0.12), (0.30, -0.15), (0.30, 0.15), (0.12, 0.15)]),
        ink,
        1.0,
    );
    painter.polygon(
        &pen.quad([(0.02, -0.08), (0.28, -0.11), (0.28, 0.11), (0.14, 0.11)]),
        tip,
        1.0,
    );
    // The collar, then the barrel with a rounded end.
    painter.polygon(
        &pen.quad([(0.28, -half), (0.38, -half), (0.38, half), (0.28, half)]),
        bgr(160, 160, 166),
        1.0,
    );
    painter.polygon(
        &pen.quad([(0.38, -half), (0.80, -half), (0.80, half), (0.38, half)]),
        ink,
        1.0,
    );
    painter.capsule(pen.at(0.80, 0.0), pen.at(0.86, 0.0), half * size, ink, 1.0);
    // A cap line across the barrel.
    painter.polygon(
        &pen.quad([(0.66, -half), (0.69, -half), (0.69, half), (0.66, half)]),
        bgr(160, 160, 166),
        1.0,
    );
}

/// A broad highlighter laying down a translucent stroke of its colour.
fn paint_highlighter(
    painter: &mut Painter,
    bounds: UiRect,
    ink: Bgr,
    colour: Bgr,
    metrics: UiMetrics,
) {
    let (left, top, size) = icon_frame(bounds, metrics);
    // The stroke it has drawn, across the bottom.
    let stroke_y = top + size * 0.86;
    painter.capsule(
        (left + size * 0.08, stroke_y),
        (left + size * 0.92, stroke_y),
        size * 0.12,
        colour,
        0.7,
    );
    // The highlighter, its broad flat tip resting on the end of the stroke.
    let pen = Diagonal::new(left + size * 0.12, top - size * 0.10, size, 0.10);
    let half = 0.2;
    painter.polygon(
        &pen.quad([(0.0, -0.16), (0.22, -half), (0.22, half), (0.0, 0.16)]),
        ink,
        1.0,
    );
    painter.polygon(
        &pen.quad([(0.03, -0.12), (0.20, -0.15), (0.20, 0.15), (0.03, 0.12)]),
        colour,
        1.0,
    );
    painter.polygon(
        &pen.quad([(0.22, -half), (0.32, -half), (0.32, half), (0.22, half)]),
        bgr(160, 160, 166),
        1.0,
    );
    painter.polygon(
        &pen.quad([(0.32, -half), (0.76, -half), (0.76, half), (0.32, half)]),
        ink,
        1.0,
    );
    painter.capsule(pen.at(0.76, 0.0), pen.at(0.80, 0.0), half * size, ink, 1.0);
}

/// An arrow on the diagonal, pointing up and to the right.
fn paint_arrow(painter: &mut Painter, bounds: UiRect, ink: Bgr, metrics: UiMetrics) {
    let (left, top, size) = icon_frame(bounds, metrics);
    let line = metrics.toolbar_tokens().icon_stroke as f32;
    let tip = (left + size * 0.9, top + size * 0.1);
    let base = (left + size * 0.58, top + size * 0.42);
    painter.capsule(
        (left + size * 0.12, top + size * 0.88),
        base,
        line * 0.75,
        ink,
        1.0,
    );
    // The head: its point at the tip, its base square to the shaft.
    let spread = size * 0.2 * std::f32::consts::FRAC_1_SQRT_2;
    painter.polygon(
        &[
            tip,
            (base.0 - spread, base.1 - spread),
            (base.0 + spread, base.1 + spread),
        ],
        ink,
        1.0,
    );
}

/// A capital T.
fn paint_text_icon(painter: &mut Painter, bounds: UiRect, ink: Bgr, metrics: UiMetrics) {
    let (left, top, size) = icon_frame(bounds, metrics);
    let line = metrics.toolbar_tokens().icon_stroke as f32 * 1.25;
    let (bar_top, bar_left, bar_right) =
        (top + size * 0.12, left + size * 0.14, left + size * 0.86);
    painter.polygon(
        &[
            (bar_left, bar_top),
            (bar_right, bar_top),
            (bar_right, bar_top + line),
            (bar_left, bar_top + line),
        ],
        ink,
        1.0,
    );
    let middle = left + size / 2.0;
    painter.polygon(
        &[
            (middle - line / 2.0, bar_top),
            (middle + line / 2.0, bar_top),
            (middle + line / 2.0, top + size * 0.9),
            (middle - line / 2.0, top + size * 0.9),
        ],
        ink,
        1.0,
    );
}

/// A colour swatch: a disc with a quiet rim, so black and white both read on the toolbar, and a
/// ring when it is the colour in use or under the pointer.
fn paint_swatch(
    painter: &mut Painter,
    bounds: UiRect,
    index: usize,
    chosen: bool,
    hovered: bool,
    metrics: UiMetrics,
) {
    let colour = colour_bgr(PALETTE[index].1);
    let centre = (
        (bounds.left + bounds.right) as f32 / 2.0,
        (bounds.top + bounds.bottom) as f32 / 2.0,
    );
    let radius = metrics.px(SWATCH_DIAMETER_DIP) as f32 / 2.0;
    let line = (metrics.dpi as f32 / UiMetrics::BASE_DPI as f32).max(1.0);
    painter.circle(centre, radius, colour, 1.0);
    painter.ring(centre, radius, line, bgr(130, 130, 136), 0.9);
    if chosen || hovered {
        let ring = if chosen {
            bgr(245, 245, 247)
        } else {
            bgr(140, 140, 146)
        };
        painter.ring(centre, radius + line * 3.5, line * 2.0, ring, 1.0);
    }
}

fn icon_box(bounds: UiRect, metrics: UiMetrics) -> (i32, i32, i32) {
    let size = metrics.toolbar_tokens().icon_size;
    (
        (bounds.left + bounds.right - size) / 2,
        (bounds.top + bounds.bottom - size) / 2,
        size,
    )
}

fn draw_tool_icon(
    device: HDC,
    bounds: UiRect,
    tool: EditorTool,
    ink: COLORREF,
    metrics: UiMetrics,
) {
    let stroke = metrics.toolbar_tokens().icon_stroke;
    let (left, top, size) = icon_box(bounds, metrics);
    let (right, bottom) = (left + size, top + size);
    let quarter = size / 4;
    match tool {
        EditorTool::Crop => {
            draw_lines(
                device,
                &[
                    (left + quarter, top),
                    (left + quarter, bottom - quarter),
                    (right, bottom - quarter),
                ],
                ink,
                stroke,
            );
            draw_lines(
                device,
                &[
                    (left, top + quarter),
                    (right - quarter, top + quarter),
                    (right - quarter, bottom),
                ],
                ink,
                stroke,
            );
        }
        EditorTool::Redact => {
            draw_outline_rect(
                device,
                left,
                top + quarter / 2,
                right,
                bottom - quarter / 2,
                ink,
                stroke,
            );
            fill_device_rect(
                device,
                RECT {
                    left: left + stroke * 2,
                    top: top + size / 2 - stroke,
                    right: right - stroke * 2,
                    bottom: top + size / 2 + stroke * 2,
                },
                ink,
            );
        }
        EditorTool::Box => {
            draw_outline_rect(
                device,
                left,
                top + quarter / 2,
                right,
                bottom - quarter / 2,
                ink,
                stroke,
            );
        }
        // Painted antialiased after GDI is done: see paint_icons.
        EditorTool::Arrow
        | EditorTool::Text
        | EditorTool::Pen
        | EditorTool::Marker
        | EditorTool::Highlighter => {}
    }
}

/// A minus for Smaller, a plus for Larger.
fn draw_size_icon(device: HDC, bounds: UiRect, larger: bool, ink: COLORREF, metrics: UiMetrics) {
    let stroke = metrics.toolbar_tokens().icon_stroke;
    let (left, top, size) = icon_box(bounds, metrics);
    let inset = size / 5;
    let middle = (left + size / 2, top + size / 2);
    draw_lines(
        device,
        &[(left + inset, middle.1), (left + size - inset, middle.1)],
        ink,
        stroke,
    );
    if larger {
        draw_lines(
            device,
            &[(middle.0, top + inset), (middle.0, top + size - inset)],
            ink,
            stroke,
        );
    }
}

/// A hooked arrow: back for Undo, forward for Redo.
fn draw_history_icon(
    device: HDC,
    bounds: UiRect,
    forward: bool,
    ink: COLORREF,
    metrics: UiMetrics,
) {
    let stroke = metrics.toolbar_tokens().icon_stroke;
    let (left, top, size) = icon_box(bounds, metrics);
    let mirror = |x: i32| if forward { left + size - (x - left) } else { x };
    let middle = top + size * 2 / 5;
    let head = size / 4;
    draw_lines(
        device,
        &[
            (mirror(left + size), top + size - 1),
            (mirror(left + size), middle),
            (mirror(left), middle),
        ],
        ink,
        stroke,
    );
    draw_lines(
        device,
        &[
            (mirror(left + head), middle - head),
            (mirror(left), middle),
            (mirror(left + head), middle + head),
        ],
        ink,
        stroke,
    );
}

/// How to finish the text being typed, in a label just below its frame, or above it when the
/// frame is near the bottom of the canvas.
fn draw_typing_hint(device: HDC, state: &EditorState, frame: UiRect) {
    let metrics = state.metrics;
    let tokens = metrics.toolbar_tokens();
    let text = "Enter to finish \u{00b7} Shift+Enter new line \u{00b7} Esc to discard";
    let measured = measure_ui_text(device, text, tokens.font_height);
    let width = measured.cx + tokens.tooltip_padding_x * 2;
    let height = (measured.cy + tokens.tooltip_padding_y * 2).max(metrics.px(28));
    let canvas = state.layout.canvas;
    let gap = metrics.px(6);
    let top = if frame.bottom + gap + height <= canvas.bottom {
        frame.bottom + gap
    } else {
        (frame.top - gap - height).max(canvas.top)
    };
    let left = frame
        .left
        .clamp(canvas.left, (canvas.right - width).max(canvas.left));
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
        rgb(ACCENT_RGB.0, ACCENT_RGB.1, ACCENT_RGB.2),
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

/// The hovered icon button's name and key, below the toolbar.
fn draw_tooltip(device: HDC, state: &EditorState) {
    let Some(button) = state.hovered else { return };
    let (Some(text), Some(target)) = (button.tooltip(), state.layout.button_rect(button)) else {
        return;
    };
    let metrics = state.metrics;
    let tokens = metrics.toolbar_tokens();
    let measured = measure_ui_text(device, &text, tokens.font_height);
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
        &text,
        rgb(248, 248, 250),
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
        // Read the editor's geometry in its own terms: a thread that is not per-monitor aware is
        // handed coordinates scaled for it, which miss a four-pixel capture entirely.
        let _dpi = super::super::shell::ThreadDpiContext::enter_per_monitor_v2()
            .expect("per-monitor DPI awareness for the test thread");
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

        // A pen stroke across the middle, then Done: the stroke is in the published image, in
        // the pen's colour.
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
        let middle_y = (canvas_top + client.bottom) / 2;
        post(editor, WM_KEYDOWN, usize::from(b'P'), 0);
        post(editor, WM_LBUTTONDOWN, 1, at(1, middle_y));
        post(editor, WM_MOUSEMOVE, 1, at(client.right / 2, middle_y));
        post(editor, WM_MOUSEMOVE, 1, at(client.right - 2, middle_y));
        post(editor, WM_LBUTTONUP, 0, at(client.right - 2, middle_y));
        post(editor, WM_KEYDOWN, usize::from(VK_RETURN.0), 0);
        match outcomes
            .recv_timeout(Duration::from_secs(5))
            .expect("the editor answered")
        {
            EditOutcome::Done(edited) => {
                let red = PALETTE[RED].1;
                let ink = [red.blue, red.green, red.red, 255];
                assert!(
                    edited.pixels().chunks_exact(4).any(|pixel| pixel == ink),
                    "no pixel was inked: {:?}",
                    edited.pixels()
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        // SAFETY: IsWindow accepts any handle, live or not.
        while unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(editor) }.as_bool() {
            assert!(
                Instant::now() < deadline,
                "the finished editor never closed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // Blue chosen, then the pen, then the arrow: the colour stays blue. An arrow dragged across
        // the middle, then its point dragged elsewhere, then Done: it is blue in the image.
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
        post(editor, WM_KEYDOWN, usize::from(b'4'), 0);
        post(editor, WM_KEYDOWN, usize::from(b'P'), 0);
        post(editor, WM_KEYDOWN, usize::from(b'A'), 0);
        post(editor, WM_LBUTTONDOWN, 1, at(1, middle_y));
        post(editor, WM_MOUSEMOVE, 1, at(client.right - 2, middle_y));
        post(editor, WM_LBUTTONUP, 0, at(client.right - 2, middle_y));
        post(
            editor,
            WM_LBUTTONDOWN,
            1,
            at(client.right - 3, middle_y + 1),
        );
        post(editor, WM_MOUSEMOVE, 1, at(client.right - 3, middle_y + 9));
        post(editor, WM_LBUTTONUP, 0, at(client.right - 3, middle_y + 9));
        post(editor, WM_KEYDOWN, usize::from(VK_RETURN.0), 0);
        match outcomes
            .recv_timeout(Duration::from_secs(5))
            .expect("the editor answered")
        {
            EditOutcome::Done(edited) => {
                let blue = PALETTE[3].1;
                let ink = [blue.blue, blue.green, blue.red, 255];
                assert!(
                    edited.pixels().chunks_exact(4).any(|pixel| pixel == ink),
                    "no pixel was inked: {:?}",
                    edited.pixels()
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        // SAFETY: IsWindow accepts any handle, live or not.
        while unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(editor) }.as_bool() {
            assert!(
                Instant::now() < deadline,
                "the finished editor never closed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // Text typed in the middle of the capture, committed with Enter, then Done with Enter: the
        // text is in the published image. Typing T as text must not switch tools.
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
        post(editor, WM_KEYDOWN, usize::from(b'T'), 0);
        post(
            editor,
            WM_LBUTTONDOWN,
            1,
            at(client.right / 2 - 6, middle_y),
        );
        post(editor, WM_LBUTTONUP, 0, at(client.right / 2 - 6, middle_y));
        for character in "MMT".encode_utf16() {
            post(editor, WM_CHAR, usize::from(character), 0);
        }
        post(editor, WM_KEYDOWN, usize::from(VK_RETURN.0), 0);
        post(editor, WM_KEYDOWN, usize::from(VK_RETURN.0), 0);
        match outcomes
            .recv_timeout(Duration::from_secs(5))
            .expect("the editor answered")
        {
            EditOutcome::Done(edited) => {
                // The capture is mid-grey. The blue chosen in the editor before this one is
                // remembered, so the text pulls some pixel well toward blue.
                assert!(
                    edited
                        .pixels()
                        .chunks_exact(4)
                        .any(|pixel| pixel[0] > pixel[1].saturating_add(40)),
                    "no pixel was inked: {:?}",
                    edited.pixels()
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        // SAFETY: IsWindow accepts any handle, live or not.
        while unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(editor) }.as_bool() {
            assert!(
                Instant::now() < deadline,
                "the finished editor never closed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // Text typed and finished, then clicked again, emptied and finished: the text is gone, so
        // nothing is left to lose and Escape cancels without asking.
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
        let spot = at(client.right / 2 - 6, middle_y);
        post(editor, WM_KEYDOWN, usize::from(b'T'), 0);
        post(editor, WM_LBUTTONDOWN, 1, spot);
        post(editor, WM_LBUTTONUP, 0, spot);
        post(editor, WM_CHAR, usize::from(b'M'), 0);
        post(editor, WM_CHAR, usize::from(b'M'), 0);
        post(editor, WM_KEYDOWN, usize::from(VK_RETURN.0), 0);
        post(editor, WM_LBUTTONDOWN, 1, spot);
        post(editor, WM_LBUTTONUP, 0, spot);
        post(editor, WM_CHAR, 0x08, 0);
        post(editor, WM_CHAR, 0x08, 0);
        post(editor, WM_KEYDOWN, usize::from(VK_RETURN.0), 0);
        post(editor, WM_KEYDOWN, usize::from(VK_ESCAPE.0), 0);
        assert!(matches!(
            outcomes.recv_timeout(Duration::from_secs(5)),
            Ok(EditOutcome::Cancelled)
        ));
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
            let (last, done) = *layout.buttons.last().expect("buttons");
            assert_eq!(last, EditorButton::Done);
            assert!(done.right <= layout.client.right);
            assert!(layout.size_label.left <= layout.size_label.right);
            assert_eq!(
                layout.button_at(POINT {
                    x: done.left + 1,
                    y: done.top + 1,
                }),
                Some(EditorButton::Done)
            );
            // Every tool and every colour has a button.
            for tool in EditorTool::ALL {
                assert!(layout.button_rect(EditorButton::Tool(tool)).is_some());
            }
            for index in 0..PALETTE.len() {
                assert!(layout.button_rect(EditorButton::Swatch(index)).is_some());
            }
        }
    }

    #[test]
    fn keys_choose_tools_and_colours_and_ctrl_keeps_them_for_undo_and_redo() {
        assert_eq!(
            key_button(u16::from(b'P'), false, false),
            Some(EditorButton::Tool(EditorTool::Pen))
        );
        assert_eq!(
            key_button(u16::from(b'H'), false, false),
            Some(EditorButton::Tool(EditorTool::Highlighter))
        );
        assert_eq!(
            key_button(u16::from(b'B'), false, false),
            Some(EditorButton::Tool(EditorTool::Box))
        );
        assert_eq!(
            key_button(u16::from(b'A'), false, false),
            Some(EditorButton::Tool(EditorTool::Arrow))
        );
        assert_eq!(
            key_button(u16::from(b'T'), false, false),
            Some(EditorButton::Tool(EditorTool::Text))
        );
        assert_eq!(text_size_step(VK_OEM_PLUS.0, true), Some(1));
        assert_eq!(text_size_step(VK_SUBTRACT.0, true), Some(-1));
        assert_eq!(text_size_step(VK_OEM_PLUS.0, false), None);
        assert_eq!(
            key_button(u16::from(b'M'), false, false),
            Some(EditorButton::Tool(EditorTool::Marker))
        );
        assert_eq!(
            key_button(u16::from(b'1'), false, false),
            Some(EditorButton::Swatch(0))
        );
        assert_eq!(
            key_button(u16::from(b'6'), false, false),
            Some(EditorButton::Swatch(5))
        );
        assert_eq!(
            key_button(0x61, false, false),
            Some(EditorButton::Swatch(0))
        );
        assert_eq!(
            key_button(u16::from(b'7'), false, false),
            Some(EditorButton::Swatch(6))
        );
        assert_eq!(key_button(u16::from(b'8'), false, false), None);
        assert_eq!(
            key_button(u16::from(b'Z'), true, false),
            Some(EditorButton::Undo)
        );
        assert_eq!(
            key_button(u16::from(b'Z'), true, true),
            Some(EditorButton::Redo)
        );
        assert_eq!(
            key_button(u16::from(b'Y'), true, false),
            Some(EditorButton::Redo)
        );
        // Ctrl with a tool key is not that tool: Ctrl+C must not switch to cropping.
        assert_eq!(key_button(u16::from(b'C'), true, false), None);
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
