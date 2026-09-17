use std::collections::{BTreeMap, VecDeque};
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

mod layout;
mod machine;
mod raster;
mod shell;
mod snap;
mod window_enumeration;

use layout::{
    layout_dimension_label, layout_loupe, loupe_edge_lines, DimensionLabelPlacement,
    DisplayEnvironment, LoupeLayout, OverviewLayoutTokens, ToolbarControl, ToolbarLayout,
    UiMetrics, UiRect, UiSize, LOUPE_SOURCE_HALF, LOUPE_SOURCE_SPAN,
};
use raster::{
    apply_dim_wash, build_blurred_background, draw_antialiased_rounded_outline, draw_camera_icon,
    draw_checkmark, draw_display_icon, draw_filled_ellipse, draw_lines, draw_outline,
    draw_outline_rect, draw_region_icon, draw_resize_handles, draw_round_box, draw_surface_nearest,
    draw_text, draw_window_icon, draw_window_surface, fill_device_rect, fill_live_background,
    fitted_surface_rect, measure_ui_text, rgb, scaled_corner_radius, TextAlignment,
};
pub use shell::flush_desktop_composition;
use shell::{
    capture_pointer, consume_self_initiated_capture_change, drain_pending_quit, duration_ns,
    invalid_frame, invalidate, key_is_autorepeat, last_error, loupe_key_after_focus_change,
    message_time_ms, modifiers, overlay_error, query_display_environment, release_pointer_capture,
    restore_input_context, screen_point, set_arrow_cursor, set_move_cursor, start_loupe_timer,
    stop_loupe_timer, ClassRegistration, FrozenSurface, PrivateFontResource, RegionCursor,
    ThreadDpiContext, LOUPE_TIMER_ID, REGION_CURSOR_CENTER,
};
use snap::{SnapAxis, SnapTarget, SnapTargetKind, SnapTargets, NO_SNAPS};
use window_enumeration::{enumerate_visible_windows, WindowCandidate};

#[cfg(test)]
use machine::{
    contains, hit_test_resize_handle, latest_interaction_region, move_region, rect_from_points,
    resize_region,
};
use machine::{
    default_region_for_source, fit_region_to_source, initial_selection, local_point, transition,
    CaptureTool, CloseOutcome, CursorIntent, OverlayEffect, OverlayInput, OverlayModel,
    ResizeHandle,
};

use crate::dwm_thumbnail::{fit_source_in_bounds, DwmThumbnail};
use captastic_core::{
    CaptureError, CaptureErrorKind, CpuFrame, DisplayId, DisplayInfo, FrameMetadata, FrameOrigin,
    PixelEncoding, Rect,
};
#[cfg(test)]
use captastic_core::{CaptureId, CaptureMode, ColorSpace, TimingProvenance};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, EndPaint, GdiFlush, GetDC, ReleaseDC, SetStretchBltMode, StretchBlt,
    UpdateWindow, AC_SRC_ALPHA, BLENDFUNCTION, CAPTUREBLT, HALFTONE, HDC, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, VK_DOWN, VK_ESCAPE, VK_F, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClassNameW,
    GetForegroundWindow, GetMessageW, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId,
    IsWindow, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, SetCursor,
    SetForegroundWindow, SetWindowDisplayAffinity, SetWindowLongPtrW, ShowWindow, TranslateMessage,
    UpdateLayeredWindow, CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
    IDC_CROSS, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, MSG, SPI_SETLOGICALDPIOVERRIDE,
    SPI_SETWORKAREA, SW_SHOW, ULW_ALPHA, ULW_OPAQUE, WDA_EXCLUDEFROMCAPTURE, WM_APP,
    WM_CAPTURECHANGED, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND,
    WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WM_SETFOCUS,
    WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};
#[cfg(test)]
use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, PM_NOREMOVE, WM_QUIT};

#[cfg(test)]
use crate::window_capture::scaled_dimensions;
use crate::window_capture::{
    capture_window_thumbnail, capture_window_visual, WINDOW_THUMBNAIL_RENDER_BATCH,
};

const CLASS_NAME: PCWSTR = w!("CaptasticFrozenSelectionOverlay");
/// Posted by the incremental window-overview build: each delivery renders one batch of
/// thumbnails and posts the next, so the message pump stays live for paint, Escape, and
/// cancellation between batches. `wparam` carries the build generation.
const WM_OVERVIEW_RENDER_BATCH: u32 = WM_APP + 1;
const DIM_ALPHA: u8 = 128;
const LIVE_HIT_TEST_ALPHA: u8 = 1;
const _: () = assert!(LIVE_HIT_TEST_ALPHA > 0);
/// Alpha byte the live background fill stamps on every pixel before the chrome pass. GDI
/// drawing writes 0 into a 32bpp DIB's reserved byte, so after the chrome is drawn the alpha
/// byte is an exact per-pixel coverage signal: 0 means some GDI operation painted the pixel,
/// this sentinel means untouched background. Color cannot carry that signal - the fill is pure
/// black and so is the outline's contrast halo (M19). The present pass rewrites every alpha
/// byte, so the sentinel itself never reaches the screen.
const LIVE_UNDRAWN_ALPHA: u8 = u8::MAX;
const _: () = assert!(LIVE_UNDRAWN_ALPHA != 0);
const WINDOW_THUMBNAIL_MAX_PIXELS: u64 = 1_200_000;
const UI_FONT_HEIGHT: i32 = 21;
/// The Options row for edge snapping. Named once so the glyph-fit test measures the same string
/// the menu paints — an Options row whose label overruns its row is the failure this guards.
///
/// "Snap to Window Edges" would be plainer but measures 200 px against the 192 px a menu row has
/// for text at 96 DPI, and widening the menu for one row is a worse trade than the shorter name.
const SNAP_TO_WINDOWS_LABEL: &str = "Snap to Edges";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionKind {
    Display,
    Region,
    Window,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialSelectionTool {
    Remembered,
    Region,
    Window,
}

/// Which pixels the overlay is showing the user *right now*.
///
/// Not the same question as which presenter is on screen. One layered window serves both views:
/// [`PreviewView::Live`] lets the real desktop through and keeps changing, [`PreviewView::Frozen`]
/// paints the snapshot taken at the hotkey press. The view is what the user sees, and therefore
/// what the confirmation must materialize from; the presenter is only how the window gets there.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PreviewView {
    /// The desktop as it is, seen through the overlay. Confirmation captures fresh pixels.
    #[default]
    Live,
    /// The frame captured at the hotkey press. Confirmation crops that frame.
    Frozen,
}

impl PreviewView {
    /// The name this view goes by in logs and JSON.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Frozen => "frozen",
        }
    }

    /// The Options row's label, which states the view that is showing rather than the action.
    ///
    /// "View: Frozen at hotkey" would say *when* the picture stopped, which is the part that
    /// matters, but it measures 220 px against the 192 px a menu row has for text at 96 DPI - and
    /// widening the whole menu for one row is the same trade the snap row already declined. The
    /// tag on screen carries the full sentence; this row only has to name the state.
    pub(crate) const fn menu_label(self) -> &'static str {
        match self {
            Self::Live => "View: Live",
            Self::Frozen => "View: Frozen",
        }
    }
}

/// How the overlay window gets its pixels onto the screen.
///
/// `Layered` is the only presenter that can show the live desktop, and it shows the frozen
/// snapshot just as well (with `ULW_OPAQUE` and no alpha pass). `Opaque` is the in-process
/// fallback for a machine where establishing the layered presenter fails: it can only ever show
/// the frozen view, which is why falling back also locks the view and hides the toggle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Presenter {
    Layered,
    Opaque,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeWindowHandle(isize);

impl NativeWindowHandle {
    #[cfg(test)]
    pub(crate) const fn from_raw(raw: isize) -> Self {
        Self(raw)
    }

    pub(crate) const fn raw(self) -> isize {
        self.0
    }
}

#[derive(Clone, Debug)]
pub struct OverlaySelection {
    pub rect: Rect,
    pub kind: SelectionKind,
    pub window: Option<NativeWindowHandle>,
    /// The captured window's title, for naming the capture after it. Set by whichever application
    /// owns the window, so every consumer treats it as untrusted text.
    pub window_title: Option<String>,
    /// The file stem of the executable owning the captured window, where it could be read.
    pub window_application: Option<String>,
    pub selection_ns: u64,
    pub preparation_ns: u64,
    pub window_overview_ns: Option<u64>,
    pub window_preview_count: usize,
    pub window_live_preview_count: usize,
    pub window_frozen_preview_count: usize,
    pub window_preview_bytes: usize,
    /// The view that was on screen when the user confirmed. The caller materializes from this and
    /// nothing else: it is the only record of which pixels the user was actually looking at.
    pub view: PreviewView,
    /// Whether the user changed the view at least once during the run. Reported rather than
    /// inferred, because a run that ends in the view it opened in is indistinguishable from one
    /// that was switched and switched back.
    pub view_switched: bool,
    /// Why the layered presenter could not be established, when it could not. `None` on every
    /// machine where it worked, which is expected to be all of them.
    pub presenter_fallback_reason: Option<String>,
    pub(crate) window_frame: Option<CpuFrame>,
}

impl OverlaySelection {
    /// A selection shaped exactly as an overlay run reports one, for tests in dependent crates
    /// that have to exercise materialization without a desktop to draw on.
    ///
    /// Not part of the product surface - nothing in Captastic builds one of these outside
    /// `build_overlay_selection` - but the alternative was making the captured window frame
    /// public, and a crate that cannot construct the value it is handed cannot test what it does
    /// with it.
    #[doc(hidden)]
    pub fn for_test(kind: SelectionKind, rect: Rect, view: PreviewView) -> Self {
        Self {
            rect,
            kind,
            window: None,
            window_title: None,
            window_application: None,
            selection_ns: 0,
            preparation_ns: 0,
            window_overview_ns: None,
            window_preview_count: 0,
            window_live_preview_count: 0,
            window_frozen_preview_count: 0,
            window_preview_bytes: 0,
            view,
            view_switched: false,
            presenter_fallback_reason: None,
            window_frame: None,
        }
    }
}

/// The close reason an overlay reports for `WM_DISPLAYCHANGE`: the monitors themselves changed —
/// one arrived, one left, or the arrangement moved.
///
/// Public because it is the one reason of the three that means *the display layout changed*. The
/// other two — a DPI change, and a work-area change such as a taskbar resizing, auto-hide being
/// toggled, or an appbar registering itself — abandon the overlay's geometry just as surely and
/// are not a layout change at all. A caller telling the user why their press was lost has to say
/// which of those happened, and sharing this spelling is better than matching on it.
pub const DISPLAY_CHANGE_MONITORS: &str = "overlay_display_changed";

/// How a selection run ended.
///
/// A run that produced no selection used to be `Ok(None)`, which made an Escape and a display
/// change under an open overlay the same event to everything downstream. They are not the same
/// event. The first is the user saying no; the second is the user's press being taken away from
/// them mid-draw, because the overlay's geometry belongs to an arrangement that has stopped
/// existing and it closes rather than keep drawing over a desktop that moved. A caller that
/// cannot tell them apart logs "selection cancelled" at info and says nothing to the user, which
/// is precisely what the first hotkey press after a dock switch looked like.
#[derive(Debug)]
pub enum SelectionOutcome {
    /// The user confirmed a region or a window. Boxed because a confirmed window selection
    /// carries its captured frame and would otherwise make every outcome that size.
    Selected(Box<OverlaySelection>),
    /// Escape, right-click, or a cancel requested from outside the overlay.
    Cancelled,
    /// The display configuration changed while the overlay was open, so the run closed without a
    /// selection and without persisting geometry from a configuration that is no longer current.
    /// The press is lost; pressing the hotkey again is the only remedy.
    ///
    /// The payload is the reason the window procedure closed for, and it is carried rather than
    /// flattened because these are not one event. A monitor arriving, leaving or moving is a
    /// display layout change. A DPI change or a work-area change — a taskbar resized, auto-hide
    /// toggled, an appbar registering itself — is not, and a caller that told the user their
    /// displays had moved because their taskbar grew would be explaining a real loss wrongly.
    DisplayConfigurationChanged(&'static str),
}

impl SelectionOutcome {
    /// The outcome a finished run reports, from the selection it produced and the reason a display
    /// change closed it, if one did.
    ///
    /// Separate from the window procedure that sets those two fields so the mapping can be tested
    /// on its own: nothing else in this file's teardown path can run without a message pump.
    fn from_run(
        selection: Option<OverlaySelection>,
        display_change_reason: Option<&'static str>,
    ) -> Self {
        match (selection, display_change_reason) {
            // Defensive rather than observed: confirmation and a display change both destroy the
            // window synchronously, so the procedure cannot reach this state. If it ever could,
            // a selection in hand is one the user made against the arrangement they were looking
            // at, and the confirmation capture validates the geometry it gets back.
            (Some(selection), _) => Self::Selected(Box::new(selection)),
            (None, Some(reason)) => Self::DisplayConfigurationChanged(reason),
            (None, None) => Self::Cancelled,
        }
    }
}

#[derive(Clone, Debug)]
pub enum OverlayUiUpdate {
    Interaction {
        display_id: String,
        tool: captastic_config::CaptureTool,
        region: Option<captastic_config::CaptureRegion>,
        source: Option<captastic_config::CaptureRegionSource>,
    },
    ToolbarCenter {
        display_id: String,
        center_x: f64,
        center_y: f64,
    },
    ConfirmedRegion {
        display_id: String,
        region: captastic_config::CaptureRegion,
        source: captastic_config::CaptureRegionSource,
    },
    /// Whether the region tool snaps to window edges. Carries no display id: it is one answer for
    /// the whole installation, and a toggle made on one monitor applies on the next.
    SnapToWindows { enabled: bool },
    /// When the region tool's magnifier appears. Global, for the same reason.
    RegionZoom { mode: captastic_config::RegionZoom },
}

#[derive(Clone, Default)]
pub struct OverlayController {
    inner: Arc<OverlayControllerInner>,
}

#[derive(Default)]
struct OverlayControllerInner {
    hwnd: AtomicIsize,
    cancelled: AtomicBool,
    ui_updates: Option<OverlayUiSink>,
}

#[derive(Clone)]
struct OverlayUiSink {
    sender: Sender<OverlayUiUpdate>,
    live_ui: Arc<Mutex<LiveUiState>>,
}

/// What this process has learned about remembered UI state since it started, so a second overlay
/// opened before the persistence worker has written anything still sees the first one's choices.
///
/// Split into per-display entries and the handful of preferences that are not about a display at
/// all: a global kept in the map would only be visible on monitors this process had already drawn
/// on, which is exactly the bug it exists to prevent.
#[derive(Default)]
struct LiveUiState {
    displays: BTreeMap<String, captastic_config::DisplayUiState>,
    snap_to_windows: Option<bool>,
    region_zoom: Option<captastic_config::RegionZoom>,
}

impl OverlayController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_ui_updates(ui_updates: Sender<OverlayUiUpdate>) -> Self {
        Self {
            inner: Arc::new(OverlayControllerInner {
                ui_updates: Some(OverlayUiSink {
                    sender: ui_updates,
                    live_ui: Arc::new(Mutex::new(LiveUiState::default())),
                }),
                ..OverlayControllerInner::default()
            }),
        }
    }

    pub fn remembered_ui(
        &self,
        display_id: &str,
        fallback: captastic_config::DisplayUiState,
    ) -> captastic_config::DisplayUiState {
        let Some(sink) = self.inner.ui_updates.as_ref() else {
            return fallback;
        };
        let mut live_ui = sink
            .live_ui
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let snap_to_windows = live_ui.snap_to_windows;
        let region_zoom = live_ui.region_zoom;
        let mut remembered = *live_ui
            .displays
            .entry(display_id.to_owned())
            .or_insert(fallback);
        // A global answers for every display, including one whose stored entry predates it.
        if let Some(enabled) = snap_to_windows {
            remembered.snap_to_windows = Some(enabled);
        }
        if let Some(mode) = region_zoom {
            remembered.region_zoom = Some(mode);
        }
        remembered
    }

    pub fn submit_ui_update(&self, update: OverlayUiUpdate) {
        if let Some(sink) = self.inner.ui_updates.as_ref() {
            sink.submit(update);
        }
    }

    pub fn cancel(&self) {
        // Sequential consistency prevents the cancel thread and overlay thread from both missing
        // the other's publication in the store-then-load rendezvous.
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let hwnd = self.inner.hwnd.load(Ordering::SeqCst);
        // The overlay thread clears the published handle inside WM_NCDESTROY - before the value
        // can be recycled - so a non-zero load is almost always a live overlay. The load-to-post
        // gap that remains is narrowed by verifying the handle still names this process's
        // overlay class. The sub-microsecond TOCTOU that survives both layers would require the
        // exact handle value to be recycled in that instant, and could misdeliver only a
        // WM_CLOSE - a soft close request - which is an accepted residual risk.
        if hwnd != 0 && overlay_window_is_current(HWND(hwnd)) {
            // SAFETY: hwnd was published by the live overlay thread. Posting does not retain it.
            let _ = unsafe { PostMessageW(HWND(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        }
    }
}

/// True when the handle still names this process's overlay window. The publisher may have been
/// destroyed - and the value recycled by any process - between publication and this call, so
/// the check pins both the owning process and the window class before a message is posted.
fn overlay_window_is_current(handle: HWND) -> bool {
    let mut process_id = 0_u32;
    // SAFETY: Queries the owning thread and process without retaining the handle.
    let thread_id = unsafe { GetWindowThreadProcessId(handle, Some(&mut process_id)) };
    // SAFETY: Reads this process's own identity.
    if thread_id == 0 || process_id != unsafe { GetCurrentProcessId() } {
        return false;
    }
    let mut class_name = [0_u16; 64];
    // SAFETY: class_name is writable storage for the null-terminated class query.
    let length = unsafe { GetClassNameW(handle, &mut class_name) }.max(0) as usize;
    // SAFETY: CLASS_NAME is a static null-terminated wide string.
    let expected = unsafe { CLASS_NAME.as_wide() };
    class_name.get(..length) == Some(expected)
}

pub fn select_from_frozen_frame(
    frame: &CpuFrame,
    resources: &mut OverlayResources,
) -> Result<SelectionOutcome, CaptureError> {
    select_from_frozen_frame_with_controller(frame, &OverlayController::new(), resources)
}

pub fn select_from_frozen_frame_with_controller(
    frame: &CpuFrame,
    controller: &OverlayController,
    resources: &mut OverlayResources,
) -> Result<SelectionOutcome, CaptureError> {
    select_from_frozen_frame_with_initial_tool(
        frame,
        controller,
        InitialSelectionTool::Remembered,
        resources,
    )
}

pub fn select_from_frozen_frame_with_initial_tool(
    frame: &CpuFrame,
    controller: &OverlayController,
    initial_tool: InitialSelectionTool,
    resources: &mut OverlayResources,
) -> Result<SelectionOutcome, CaptureError> {
    select_from_frozen_frame_with_initial_tool_and_ui(
        frame,
        controller,
        initial_tool,
        None,
        resources,
    )
}

pub fn select_from_frozen_frame_with_initial_tool_and_ui(
    frame: &CpuFrame,
    controller: &OverlayController,
    initial_tool: InitialSelectionTool,
    remembered_ui: Option<captastic_config::DisplayUiState>,
    resources: &mut OverlayResources,
) -> Result<SelectionOutcome, CaptureError> {
    select_from_preview_source_with_initial_tool_and_ui(
        SelectionPreviewSource::frozen(frame),
        controller,
        initial_tool,
        remembered_ui,
        resources,
    )
}

/// What an overlay run has to present from, and which view it opens in.
///
/// A run with a `snapshot` can show either view and lets the user switch between them; a run
/// without one can only ever show the live desktop.
#[derive(Clone, Copy, Debug)]
pub struct SelectionPreviewSource<'a> {
    metadata: &'a FrameMetadata,
    /// The frame captured at the hotkey press, when there is one.
    snapshot: Option<&'a CpuFrame>,
    /// The view the overlay opens in. Only honoured when a snapshot backs it: without one,
    /// `Frozen` has nothing to paint.
    initial_view: PreviewView,
    /// Whether the layered presenter is a requirement rather than a preference. Set when the
    /// caller asked for the live view explicitly, where an opaque window showing trigger-time
    /// pixels would be the wrong answer rather than a degraded one.
    require_layered: bool,
}

impl<'a> SelectionPreviewSource<'a> {
    /// A run backed by the trigger-time snapshot, opening in `initial_view`.
    pub fn snapshot(frame: &'a CpuFrame, initial_view: PreviewView, require_layered: bool) -> Self {
        Self {
            metadata: &frame.metadata,
            snapshot: Some(frame),
            initial_view,
            require_layered,
        }
    }

    /// A run backed by a snapshot and opening on it. The layered presenter is preferred but not
    /// required: an opaque window shows exactly these pixels.
    pub fn frozen(frame: &'a CpuFrame) -> Self {
        Self::snapshot(frame, PreviewView::Frozen, false)
    }

    /// A run with no pixels of its own, which can only present the live desktop.
    pub fn live(metadata: &'a FrameMetadata) -> Self {
        Self {
            metadata,
            snapshot: None,
            initial_view: PreviewView::Live,
            require_layered: true,
        }
    }

    pub fn metadata(self) -> &'a FrameMetadata {
        self.metadata
    }

    /// The view this run will actually open in: `Frozen` needs a snapshot to paint.
    fn resolved_initial_view(self) -> PreviewView {
        if self.snapshot.is_some() {
            self.initial_view
        } else {
            PreviewView::Live
        }
    }
}

pub fn select_from_preview_source_with_initial_tool_and_ui(
    preview_source: SelectionPreviewSource<'_>,
    controller: &OverlayController,
    initial_tool: InitialSelectionTool,
    remembered_ui: Option<captastic_config::DisplayUiState>,
    resources: &mut OverlayResources,
) -> Result<SelectionOutcome, CaptureError> {
    let preparation_started = Instant::now();
    if controller.inner.cancelled.load(Ordering::SeqCst) {
        return Ok(SelectionOutcome::Cancelled);
    }
    let _dpi_context = ThreadDpiContext::enter_per_monitor_v2()?;
    if let Some(frame) = preview_source.snapshot {
        validate_frame(frame)?;
    }
    let metadata = preview_source.metadata;
    let source = metadata.source_rect;
    if source.width == 0 || source.height == 0 {
        return Err(invalid_frame(
            "selection source dimensions must be non-zero",
        ));
    }
    // SAFETY: Reads the current foreground window without retaining or mutating it.
    let previous_foreground = unsafe { GetForegroundWindow() };
    let pixels = preview_source.snapshot.map(tight_pixels).transpose()?;
    let cached = resources.take_matching(source.width, source.height);
    let (
        surface,
        back_buffer,
        dimmer,
        blurred_background,
        cached_overview_surface,
        region_cursor,
        font_resource,
    ) = if let Some(cached) = cached {
        if let Some(pixels) = pixels.as_deref() {
            cached.surface.write_pixels(pixels)?;
        } else {
            cached.surface.clear();
        }
        (
            cached.surface,
            cached.back_buffer,
            cached.dimmer,
            cached.blurred_background,
            cached.overview_surface,
            cached.region_cursor,
            cached.font_resource,
        )
    } else {
        let surface = if let Some(pixels) = pixels.as_deref() {
            FrozenSurface::new(source.width, source.height, pixels)?
        } else {
            FrozenSurface::empty(source.width, source.height)?
        };
        (
            surface,
            FrozenSurface::empty(source.width, source.height)?,
            FrozenSurface::new(1, 1, &[0, 0, 0, 255])?,
            None,
            None,
            RegionCursor::create(),
            PrivateFontResource::register()?,
        )
    };
    if preview_source.snapshot.is_none() {
        // No snapshot was written into `surface`, so it holds nothing at all. Retain one
        // pre-overlay desktop image so switching to the window chooser has real pixels from
        // which to build its blurred backdrop. A run that *does* have a snapshot already has
        // better pixels there — taken at the press, by the capture engine — and re-reading the
        // desktop over them would both cost a full-screen BitBlt and overwrite the one image
        // the frozen view exists to show.
        if let Err(error) = capture_live_window_backdrop(&surface, source) {
            log::warn!("live window backdrop capture failed: {error}; using a neutral background");
            fill_device_rect(
                surface.device,
                RECT {
                    left: 0,
                    top: 0,
                    right: surface.width,
                    bottom: surface.height,
                },
                rgb(28, 28, 31),
            );
        }
    }
    let display_environment = query_display_environment(source);
    let remembered_ui = remembered_ui.unwrap_or_default();
    let toolbar_position = remembered_toolbar_position(
        remembered_ui.overlay_center,
        remembered_ui.overlay_position,
        display_environment,
    )
    .unwrap_or_else(|| ToolbarLayout::default_origin(display_environment));
    let last_region = Some(
        remembered_last_region(
            remembered_ui.region,
            remembered_ui.region_source,
            remembered_ui.region_is_display_local,
            source,
            metadata.rotation_degrees,
        )
        .unwrap_or_else(|| default_region_for_source(source)),
    );
    let remembered_tool = remembered_ui
        .tool
        .map(CaptureTool::from_config)
        .unwrap_or(CaptureTool::Region);
    let tool = match initial_tool {
        InitialSelectionTool::Remembered => remembered_tool,
        InitialSelectionTool::Region => CaptureTool::Region,
        InitialSelectionTool::Window => CaptureTool::Window,
    };
    let (selection, selection_kind) = initial_selection(tool, last_region, source);
    let initial_view = preview_source.resolved_initial_view();
    let state = Box::new(OverlayState {
        model: OverlayModel {
            source,
            view: initial_view,
            initial_view,
            // Both views need pixels to switch between: the snapshot for the frozen one and a
            // layered window for the live one. A run with a snapshot has the first; the second
            // is retracted below if the layered presenter has to be abandoned.
            view_toggle_available: preview_source.snapshot.is_some(),
            display_environment,
            tool,
            selection,
            selection_kind,
            selected_window: None,
            anchor: None,
            dragging: false,
            resizing: None,
            moving_region: None,
            hovered_handle: None,
            last_region,
            toolbar_position,
            toolbar_drag: None,
            options_open: false,
            dim_background: true,
            hovered_control: None,
            pointer_local: None,
            hovered: None,
            snap_to_windows: remembered_ui.snap_to_windows.unwrap_or(true),
            snap_targets: None,
            loupe: machine::LoupeState::new(
                remembered_ui
                    .region_zoom
                    .map_or_else(Default::default, machine::LoupeMode::from_config),
            ),
            active_snaps: NO_SNAPS,
        },
        overlay_hwnd: HWND(0),
        presenter: Presenter::Layered,
        presenter_fallback_reason: None,
        require_layered: preview_source.require_layered,
        snapshot_present: preview_source.snapshot.is_some(),
        view_switched: false,
        surface,
        back_buffer,
        dimmer,
        blurred_background: None,
        cached_blurred_background: blurred_background,
        window_assets_ready: false,
        windows: None,
        dimension_label_placement: None,
        loupe_sample: None,
        loupe_layout: None,
        selected_window_frame: None,
        releasing_pointer_capture: false,
        controller: controller.clone(),
        reference_metadata: metadata.clone(),
        window_preview: None,
        window_thumbnails: Vec::new(),
        live_window_thumbnails: Vec::new(),
        // A previous run's chooser surface is only ever an allocation to draw into, never
        // presentable content: the cache starts empty so the first Window-mode compose is
        // forced to rebuild, and the rebuild reclaims the spare buffer below.
        window_overview_cache: None,
        window_overview_build: None,
        overview_build_generation: 0,
        spare_overview_surface: cached_overview_surface,
        region_cursor,
        _font_resource: font_resource,
        previous_foreground,
        result: None,
        display_change_reason: None,
        started: Instant::now(),
        preparation_ns: duration_ns(preparation_started.elapsed()),
        window_overview_ns: None,
        ui_updates: controller.inner.ui_updates.clone(),
    });
    // A remembered Window tool no longer builds the chooser synchronously before the window
    // exists: run_overlay starts the incremental build right after creation, so the overlay
    // appears at once and the thumbnails stream in - strictly better than a frozen launch.
    run_overlay(state, controller, resources)
}

struct OverlayState {
    /// The pure product-state machine; every transition-owned field lives here.
    model: OverlayModel,
    overlay_hwnd: HWND,
    /// How this run is getting its pixels onto the screen. Always starts `Layered`; only the
    /// first-present fallback in [`run_overlay`] ever changes it.
    presenter: Presenter,
    /// Why the layered presenter was abandoned, when it was.
    presenter_fallback_reason: Option<String>,
    /// Whether the caller demanded the layered presenter. `true` makes a first-present failure
    /// an error rather than a fallback.
    require_layered: bool,
    /// Whether `surface` holds the trigger-time snapshot. An opaque window has nothing else to
    /// show, so this is half of what makes the presenter fallback possible at all.
    snapshot_present: bool,
    /// Whether the user switched the view during this run. Shell-side because it is telemetry
    /// about the run, not a product decision the machine makes anything of.
    view_switched: bool,
    surface: FrozenSurface,
    back_buffer: FrozenSurface,
    dimmer: FrozenSurface,
    blurred_background: Option<FrozenSurface>,
    cached_blurred_background: Option<FrozenSurface>,
    window_assets_ready: bool,
    windows: Option<Vec<WindowCandidate>>,
    dimension_label_placement: Option<DimensionLabelPlacement>,
    /// The 31x31 square of real pixels the magnifier is showing, resampled every paint. A single
    /// allocation for the life of the run; the pixels in it are replaced, never appended to.
    loupe_sample: Option<FrozenSurface>,
    /// Where the magnifier is this paint, or `None` when it is not showing. Computed before the
    /// dimension badge is placed, so the badge can treat it as an obstacle, and consumed again by
    /// the live presenter's alpha pass.
    loupe_layout: Option<LoupeLayout>,
    selected_window_frame: Option<CpuFrame>,
    /// Shell-side protocol flag distinguishing a self-initiated `ReleaseCapture` round trip from
    /// an externally stolen pointer capture. Never part of the model.
    releasing_pointer_capture: bool,
    /// The cross-thread controller this run published its HWND to; WM_NCDESTROY clears that
    /// publication before the handle value can be recycled.
    controller: OverlayController,
    reference_metadata: FrameMetadata,
    window_preview: Option<WindowPreviewState>,
    window_thumbnails: Vec<WindowThumbnail>,
    live_window_thumbnails: Vec<LiveWindowThumbnail>,
    window_overview_cache: Option<WindowOverviewCache>,
    /// The in-flight incremental thumbnail build, if any. Shell inventory work, not product
    /// state: it survives a tool switch away (frozen; its posted messages are ignored) and
    /// resumes when the Window tool is re-activated.
    window_overview_build: Option<WindowOverviewBuild>,
    /// Monotonic stamp for overview builds; a posted batch message whose generation does not
    /// match the current build is stale and ignored.
    overview_build_generation: usize,
    /// A previous run's chooser surface, held purely as a reusable allocation for
    /// [`rebuild_window_overview_cache`]. Its pixels are stale by definition and are always
    /// overdrawn before the surface can reach the screen.
    spare_overview_surface: Option<FrozenSurface>,
    region_cursor: RegionCursor,
    _font_resource: PrivateFontResource,
    previous_foreground: HWND,
    result: Option<OverlaySelection>,
    /// Set when the run is ending because the display configuration changed under the overlay,
    /// to the reason the window procedure closed for, so the outcome can name what happened
    /// instead of reporting a cancellation the user never made.
    display_change_reason: Option<&'static str>,
    started: Instant,
    preparation_ns: u64,
    window_overview_ns: Option<u64>,
    ui_updates: Option<OverlayUiSink>,
}

struct OverlayResourceCache {
    surface: FrozenSurface,
    back_buffer: FrozenSurface,
    dimmer: FrozenSurface,
    blurred_background: Option<FrozenSurface>,
    overview_surface: Option<FrozenSurface>,
    region_cursor: RegionCursor,
    font_resource: PrivateFontResource,
}

/// Reusable GDI-backed overlay resources: the frozen and composition surfaces, the blur and
/// chooser caches, the region cursor, and the process-private font registration - roughly three
/// full-display DIB allocations when warm. The caller that runs overlays owns one instance and
/// passes it into every run; dropping or [`clear`](Self::clear)ing it releases everything at
/// once. Thread-affine: every handle inside was created on the owning thread and the `Drop`
/// impls must run there, which the `!Send` marker enforces.
#[derive(Default)]
pub struct OverlayResources {
    cache: Option<OverlayResourceCache>,
    _thread_affine: PhantomData<Rc<()>>,
}

impl OverlayResources {
    pub fn new() -> Self {
        Self::default()
    }

    /// Releases every cached surface and resource immediately.
    pub fn clear(&mut self) {
        self.cache = None;
    }

    /// Hands out the cached resources only when they match the requested dimensions exactly;
    /// a resolution change drops the stale allocation instead of resizing it.
    fn take_matching(&mut self, width: u32, height: u32) -> Option<OverlayResourceCache> {
        self.cache.take().filter(|resources| {
            resources.surface.width == width as i32 && resources.surface.height == height as i32
        })
    }
}

impl OverlayState {
    /// Whether the pixels in `surface` are what the user is looking at.
    ///
    /// The one place the magnifier's sampling decision is made. In the frozen view the overlay is
    /// painting a still image and that image is authoritative; in the live view the layered
    /// window is showing the real desktop through itself and `surface` holds trigger-time pixels
    /// that are *not* what will be captured — stale the moment anything on screen moves.
    /// Magnifying those would show the user pixels that are not there.
    ///
    /// Keyed on the view rather than the presenter for exactly that reason: a run that has a
    /// snapshot in `surface` and is showing the live desktop over it must still sample the
    /// desktop.
    fn showing_frozen_pixels(&self) -> bool {
        self.model.view == PreviewView::Frozen
    }
}

fn cache_overlay_state(
    state: Box<OverlayState>,
    resources: &mut OverlayResources,
) -> SelectionOutcome {
    let OverlayState {
        result,
        display_change_reason,
        surface,
        back_buffer,
        dimmer,
        blurred_background,
        cached_blurred_background,
        window_overview_cache,
        spare_overview_surface,
        region_cursor,
        _font_resource,
        ..
    } = *state;
    let blurred_background = blurred_background.or(cached_blurred_background);
    // A run that never entered Window mode passes the unused spare allocation along.
    let overview_surface = window_overview_cache
        .map(|cache| cache.surface)
        .or(spare_overview_surface);
    resources.cache = Some(OverlayResourceCache {
        surface,
        back_buffer,
        dimmer,
        blurred_background,
        overview_surface,
        region_cursor,
        font_resource: _font_resource,
    });
    SelectionOutcome::from_run(result, display_change_reason)
}

enum WindowPreviewState {
    Ready(Box<WindowPreview>),
    Unavailable(NativeWindowHandle),
}

struct WindowPreview {
    handle: NativeWindowHandle,
    frame: CpuFrame,
    surface: FrozenSurface,
    corner_radius_px: f32,
}

/// One tile in the window chooser.
///
/// A tile always knows the shape of the window behind it, because that is what the layout and the
/// selection outline are built from. It does not always have pixels: when DWM draws the tile as a
/// live preview, rendering the window ourselves would cost ~60 ms to produce something the
/// compositor immediately covers.
struct WindowThumbnail {
    handle: NativeWindowHandle,
    /// The source window's size, for layout and outline geometry.
    source_width: i32,
    source_height: i32,
    corner_radius_px: f32,
    /// Rendered pixels, present only when this tile is drawn by Captastic rather than by DWM.
    surface: Option<FrozenSurface>,
}

struct LiveWindowThumbnail {
    handle: NativeWindowHandle,
    thumbnail: DwmThumbnail,
}

struct WindowOverviewCache {
    surface: FrozenSurface,
    dim_background: bool,
}

impl WindowPreviewState {
    /// Whether this cached entry lets a click on `handle` skip another capture attempt.
    ///
    /// Only a ready preview of the same window qualifies. A previous failure is deliberately not
    /// sticky: window captures fail transiently by design (the bounded render reports a retryable
    /// timeout after 700 ms), so a click on a window that failed before tries again instead of
    /// silently doing nothing. Retrying once per click is bounded by that timeout, and every click
    /// is an explicit user action.
    fn satisfies(&self, handle: NativeWindowHandle) -> bool {
        match self {
            Self::Ready(preview) => preview.handle == handle,
            Self::Unavailable(_) => false,
        }
    }
}

fn remembered_toolbar_position(
    normalized_center: Option<(f64, f64)>,
    position: Option<(i32, i32)>,
    environment: DisplayEnvironment,
) -> Option<POINT> {
    normalized_center
        .map(|(center_x, center_y)| {
            let work_area = environment.work_area;
            let metrics = environment.metrics;
            POINT {
                x: work_area.left
                    + (center_x.clamp(0.0, 1.0) * f64::from(work_area.width())).round() as i32
                    - metrics.toolbar_width() / 2,
                y: work_area.top
                    + (center_y.clamp(0.0, 1.0) * f64::from(work_area.height())).round() as i32
                    - metrics.toolbar_height() / 2,
            }
        })
        .or_else(|| position.map(|(x, y)| POINT { x, y }))
        .map(|origin| ToolbarLayout::clamp_origin(environment, origin))
}

fn remembered_last_region(
    region: Option<captastic_config::CaptureRegion>,
    previous_source: Option<captastic_config::CaptureRegionSource>,
    display_local: bool,
    source: Rect,
    current_rotation_degrees: u16,
) -> Option<Rect> {
    region.map(|region| {
        if display_local {
            if let Some(previous_source) = previous_source {
                return restore_region_for_display_change(
                    region,
                    previous_source,
                    source,
                    current_rotation_degrees,
                );
            }
        }
        let x = if display_local {
            source.x.saturating_add(region.x)
        } else {
            region.x
        };
        let y = if display_local {
            source.y.saturating_add(region.y)
        } else {
            region.y
        };
        fit_region_to_source(
            Rect {
                x,
                y,
                width: region.width,
                height: region.height,
            },
            source,
        )
    })
}

fn restore_region_for_display_change(
    region: captastic_config::CaptureRegion,
    previous_source: captastic_config::CaptureRegionSource,
    source: Rect,
    current_rotation_degrees: u16,
) -> Rect {
    let center_x = (f64::from(region.x) + f64::from(region.width) / 2.0)
        / f64::from(previous_source.width.max(1));
    let center_y = (f64::from(region.y) + f64::from(region.height) / 2.0)
        / f64::from(previous_source.height.max(1));
    let rotation_delta = (u32::from(current_rotation_degrees) + 360
        - u32::from(previous_source.rotation_degrees))
        % 360;
    let (center_x, center_y, width, height) = match rotation_delta {
        90 => (1.0 - center_y, center_x, region.height, region.width),
        180 => (1.0 - center_x, 1.0 - center_y, region.width, region.height),
        270 => (center_y, 1.0 - center_x, region.height, region.width),
        _ => (center_x, center_y, region.width, region.height),
    };
    let width = width.min(source.width);
    let height = height.min(source.height);
    let local_x =
        (center_x.clamp(0.0, 1.0) * f64::from(source.width) - f64::from(width) / 2.0).round();
    let local_y =
        (center_y.clamp(0.0, 1.0) * f64::from(source.height) - f64::from(height) / 2.0).round();
    fit_region_to_source(
        Rect {
            x: source.x.saturating_add(local_x as i32),
            y: source.y.saturating_add(local_y as i32),
            width,
            height,
        },
        source,
    )
}

fn remember_overlay_interaction(
    ui_updates: Option<&OverlayUiSink>,
    display_id: &DisplayId,
    source: Rect,
    tool: CaptureTool,
    region: Option<Rect>,
    rotation_degrees: u16,
) {
    let persisted_region = region.map(|region| captastic_config::CaptureRegion {
        x: region.x.saturating_sub(source.x),
        y: region.y.saturating_sub(source.y),
        width: region.width,
        height: region.height,
    });
    let update = OverlayUiUpdate::Interaction {
        display_id: display_id.0.clone(),
        tool: tool.to_config(),
        region: persisted_region,
        source: region.map(|_| captastic_config::CaptureRegionSource {
            width: source.width,
            height: source.height,
            rotation_degrees,
        }),
    };
    if let Some(sink) = ui_updates {
        sink.submit(update);
    }
}

fn remember_toolbar_position(
    ui_updates: Option<&OverlayUiSink>,
    display_id: &DisplayId,
    position: POINT,
    environment: DisplayEnvironment,
) {
    let metrics = environment.metrics;
    let work_area = environment.work_area;
    let center_x = f64::from(position.x + metrics.toolbar_width() / 2 - work_area.left)
        / f64::from(work_area.width());
    let center_y = f64::from(position.y + metrics.toolbar_height() / 2 - work_area.top)
        / f64::from(work_area.height());
    let update = OverlayUiUpdate::ToolbarCenter {
        display_id: display_id.0.clone(),
        center_x: center_x.clamp(0.0, 1.0),
        center_y: center_y.clamp(0.0, 1.0),
    };
    if let Some(sink) = ui_updates {
        sink.submit(update);
    }
}

impl OverlayUiSink {
    fn submit(&self, update: OverlayUiUpdate) {
        {
            let mut live_ui = self
                .live_ui
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            apply_overlay_ui_update(&mut live_ui, &update);
        }
        if self.sender.send(update).is_err() {
            log::warn!("UI-state persistence worker is unavailable; update was not saved to disk");
        }
    }
}

fn apply_overlay_ui_update(state: &mut LiveUiState, update: &OverlayUiUpdate) {
    match update {
        OverlayUiUpdate::SnapToWindows { enabled } => state.snap_to_windows = Some(*enabled),
        OverlayUiUpdate::RegionZoom { mode } => state.region_zoom = Some(*mode),
        OverlayUiUpdate::Interaction {
            display_id,
            tool,
            region,
            source,
        } => {
            let display = state.displays.entry(display_id.clone()).or_default();
            display.tool = Some(*tool);
            if let Some(region) = region {
                display.region = Some(*region);
                display.region_source = *source;
                display.region_is_display_local = true;
            }
        }
        OverlayUiUpdate::ToolbarCenter {
            display_id,
            center_x,
            center_y,
        } => {
            state
                .displays
                .entry(display_id.clone())
                .or_default()
                .overlay_center = Some((*center_x, *center_y));
        }
        OverlayUiUpdate::ConfirmedRegion {
            display_id,
            region,
            source,
        } => {
            state
                .displays
                .entry(display_id.clone())
                .or_default()
                .confirmed_region = Some(captastic_config::ConfirmedRegion {
                region: *region,
                source: *source,
            });
        }
    }
}

fn run_overlay(
    state: Box<OverlayState>,
    controller: &OverlayController,
    resources: &mut OverlayResources,
) -> Result<SelectionOutcome, CaptureError> {
    // Overlays run back to back on one long-lived selection thread. Start from a queue that cannot
    // already be quitting, so an earlier run's teardown can never end this loop before it pumps.
    drain_pending_quit();
    // SAFETY: No module name requests the current executable module.
    let module = unsafe { GetModuleHandleW(None) }
        .map_err(|error| overlay_error("get_module_handle", error))?;
    let instance = HINSTANCE(module.0);
    // SAFETY: IDC_CROSS is a valid system cursor resource.
    let cursor = unsafe { LoadCursorW(None, IDC_CROSS) }
        .map_err(|error| overlay_error("load_overlay_cursor", error))?;
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(overlay_window_proc),
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: class is fully initialized and the callback/class name live for the registration.
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        return Err(last_error("register_overlay_class"));
    }
    let class_guard = ClassRegistration {
        class_name: CLASS_NAME,
        instance,
    };
    let source = state.model.source;
    let width = i32::try_from(source.width)
        .map_err(|_| invalid_frame("overlay width exceeds Win32 limits"))?;
    let height = i32::try_from(source.height)
        .map_err(|_| invalid_frame("overlay height exceeds Win32 limits"))?;
    let state_pointer = Box::into_raw(state);
    let hwnd = match establish_presenter(state_pointer, instance, source, width, height) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            // SAFETY: Presenter setup left no live window holding the state allocation.
            let state = unsafe { Box::from_raw(state_pointer) };
            let _ = cache_overlay_state(state, resources);
            return Err(error);
        }
    };
    // SAFETY: state_pointer remains exclusively owned by this overlay thread. The chooser build
    // starts only once the presenter has settled, so a fallback re-creation cannot strand a
    // build posting messages at a window that no longer exists.
    unsafe {
        if (*state_pointer).model.tool == CaptureTool::Window {
            start_window_overview_build(hwnd, &mut *state_pointer);
        }
    }
    controller.inner.hwnd.store(hwnd.0, Ordering::SeqCst);
    if controller.inner.cancelled.load(Ordering::SeqCst) {
        // A cancel raced construction: destroy immediately and fall through to the message
        // loop, which consumes the WM_QUIT that WM_DESTROY just latched and exits through
        // the normal teardown (state reclaim, resource cache, controller clear). The handle
        // is dead past this point and must not be shown or focused.
        // SAFETY: hwnd was just created on this thread and cancellation was requested.
        let _ = unsafe { DestroyWindow(hwnd) };
    } else {
        // SAFETY: hwnd is the live overlay window on this thread.
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            SetForegroundWindow(hwnd);
            SetFocus(hwnd);
        }
    }
    let mut message = MSG::default();
    loop {
        // SAFETY: message is writable storage and this thread owns the overlay message loop.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            // SAFETY: hwnd is still owned by this thread if message retrieval fails.
            let _ = unsafe { DestroyWindow(hwnd) };
            // This loop is abandoning, so nothing will consume the quit that WM_DESTROY posted.
            drain_pending_quit();
            // SAFETY: The callback will no longer access state after DestroyWindow returns.
            let state = unsafe { Box::from_raw(state_pointer) };
            controller.inner.hwnd.store(0, Ordering::SeqCst);
            let previous_foreground = state.previous_foreground;
            let _ = cache_overlay_state(state, resources);
            restore_input_context(previous_foreground);
            return Err(last_error("overlay_message_loop"));
        }
        if result.0 == 0 {
            break;
        }
        // SAFETY: message was populated by GetMessageW.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    // SAFETY: WM_NCDESTROY cleared the window user-data pointer and the loop has ended.
    let state = unsafe { Box::from_raw(state_pointer) };
    controller.inner.hwnd.store(0, Ordering::SeqCst);
    let previous_foreground = state.previous_foreground;
    let outcome = cache_overlay_state(state, resources);
    restore_input_context(previous_foreground);
    drop(class_guard);
    Ok(outcome)
}

/// Creates the overlay window and gets its first frame onto the layer, falling back to an opaque
/// window when the layered presenter cannot be established and the run can live without it.
///
/// One layered window serves both views, so this runs for every run rather than only live ones:
/// the first `UpdateLayeredWindow` is what proves the presenter works, and doing it before the
/// window is shown is what makes a fallback invisible rather than a flash of a broken overlay.
///
/// The fallback is only available to a run that has a snapshot to paint and did not demand the
/// layered presenter. It destroys the window, drains the quit that `WM_DESTROY` latched — leaving
/// it queued would end the replacement run's message loop immediately — locks the view to
/// `Frozen`, and re-creates without `WS_EX_LAYERED`. An explicit `live` request fails hard
/// instead: an opaque window showing trigger-time pixels is the wrong answer to it, not a
/// degraded one.
///
/// # Safety-relevant ownership
///
/// `state_pointer` is owned by the caller for the whole call: on the error return no window holds
/// it, and on success exactly one live window does.
fn establish_presenter(
    state_pointer: *mut OverlayState,
    instance: HINSTANCE,
    source: Rect,
    width: i32,
    height: i32,
) -> Result<HWND, CaptureError> {
    loop {
        // SAFETY: state_pointer is exclusively owned by this overlay thread; no window is live
        // at this point in the loop, so no callback can be reading it.
        let layered = unsafe { (*state_pointer).presenter } == Presenter::Layered;
        // SAFETY: The registered class and callback are valid. state_pointer remains allocated
        // until the message loop exits; WM_NCCREATE stores it as window user data.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | if layered {
                        WS_EX_LAYERED
                    } else {
                        Default::default()
                    },
                CLASS_NAME,
                w!("Captastic Selection"),
                WS_POPUP,
                source.x,
                source.y,
                width,
                height,
                None,
                None,
                instance,
                Some(state_pointer.cast()),
            )
        };
        if hwnd.0 == 0 {
            return Err(last_error("create_overlay_window"));
        }
        // SAFETY: state_pointer remains exclusively owned by this overlay thread. Recording the
        // HWND lets tool transitions register compositor previews against this destination.
        unsafe { (*state_pointer).overlay_hwnd = hwnd };
        // Every overlay, including the opaque fallback. Capture exclusion is defence in depth -
        // confirmation still destroys this window before asking the capture owner for pixels -
        // but a window that can be showing the live desktop one keystroke from now has no
        // business appearing in somebody else's screen recording, and the fallback window is
        // only locked to the frozen view by a decision this code made, not by anything Windows
        // knows.
        // SAFETY: hwnd is a live top-level window owned by this process.
        let excluded = unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) };
        if let Err(error) = excluded {
            log::warn!("selection overlay could not be excluded from capture: {error}");
        }
        if !layered {
            return Ok(hwnd);
        }
        // Build the first layer before showing the window: this both establishes the layered
        // presenter and validates it early enough to fall back invisibly.
        // SAFETY: state_pointer remains exclusively owned by this overlay thread.
        let state = unsafe { &mut *state_pointer };
        compose_overlay_state(state);
        let present = present_layer(hwnd, state);
        let fallback_allowed = !state.require_layered && state.snapshot_present;
        match present {
            Ok(()) => return Ok(hwnd),
            Err(error) => {
                // SAFETY: hwnd and state_pointer were created on this thread and are not
                // published to any other.
                let _ = unsafe { DestroyWindow(hwnd) };
                // The synchronous WM_DESTROY posted a quit that no message loop will consume.
                drain_pending_quit();
                // SAFETY: no callback can access the state after DestroyWindow returns.
                let state = unsafe { &mut *state_pointer };
                state.overlay_hwnd = HWND(0);
                if !fallback_allowed {
                    return Err(error);
                }
                log::warn!(
                    "layered selection presenter failed; showing the frozen view in an opaque window: {error}"
                );
                state.presenter = Presenter::Opaque;
                state.presenter_fallback_reason = Some(error.to_string());
                // An opaque window can only ever show the snapshot, so the view is not a choice
                // any more and offering to switch it would be offering something that cannot
                // happen.
                state.model.view = PreviewView::Frozen;
                state.model.initial_view = PreviewView::Frozen;
                state.model.view_toggle_available = false;
            }
        }
    }
}

unsafe extern "system" fn overlay_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| {
        overlay_window_proc_inner(hwnd, message, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(_) => {
            // SAFETY: hwnd belongs to this callback. Destroying it terminates the overlay safely.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
    }
}

fn overlay_window_proc_inner(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: WM_NCCREATE lparam points to a valid CREATESTRUCTW for this call.
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        // SAFETY: Stores the Box pointer passed to CreateWindowExW for later callbacks. This
        // must run first and unconditionally so every later message can reach the state.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        // Delegate to DefWindowProcW instead of answering TRUE directly: its WM_NCCREATE
        // handling stores the window text passed to CreateWindowExW (without it,
        // GetWindowTextW on the overlay returns nothing) and returns TRUE on success, so
        // creation proceeds exactly as before - and honestly aborts if default non-client
        // setup ever fails.
        // SAFETY: Default non-client creation for this live window.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: Retrieves only the pointer installed during WM_NCCREATE.
    let state_pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut OverlayState;
    if message == WM_NCDESTROY {
        if !state_pointer.is_null() {
            // SAFETY: WM_NCDESTROY runs on the owning overlay thread while the state allocation is
            // still alive. Unregister compositor relationships before the destination is gone.
            let state = unsafe { &mut *state_pointer };
            state.live_window_thumbnails.clear();
            state.overlay_hwnd = HWND(0);
            // Retract the published handle while the window still exists: after WM_NCDESTROY
            // returns the value can be recycled, and cancel() must not be able to load it.
            // run_overlay's later store(0) remains as belt and braces for non-window paths.
            state.controller.inner.hwnd.store(0, Ordering::SeqCst);
        }
        // SAFETY: Prevents any later callback from observing the state pointer.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        // SAFETY: Default non-client cleanup for this live window.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    if state_pointer.is_null() {
        // SAFETY: No application state is available, so default handling is required.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    match message {
        WM_DPICHANGED => run_machine(
            hwnd,
            state_pointer,
            OverlayInput::DisplayConfigurationInvalidated {
                reason: "overlay_dpi_changed",
            },
        ),
        WM_DISPLAYCHANGE => run_machine(
            hwnd,
            state_pointer,
            OverlayInput::DisplayConfigurationInvalidated {
                reason: DISPLAY_CHANGE_MONITORS,
            },
        ),
        WM_SETTINGCHANGE
            if wparam.0 == SPI_SETWORKAREA.0 as usize
                || wparam.0 == SPI_SETLOGICALDPIOVERRIDE.0 as usize =>
        {
            run_machine(
                hwnd,
                state_pointer,
                OverlayInput::DisplayConfigurationInvalidated {
                    reason: "overlay_display_setting_changed",
                },
            )
        }
        WM_MOUSEMOVE => {
            let (point, window_pass) = {
                // SAFETY: The borrow ends with this block, before run_machine derives its own.
                let state = unsafe { &*state_pointer };
                (
                    screen_point(state.model.source, lparam),
                    state.model.tool == CaptureTool::Window && state.model.toolbar_drag.is_none(),
                )
            };
            if !window_pass {
                return run_machine(
                    hwnd,
                    state_pointer,
                    OverlayInput::PointerMoved {
                        point,
                        window_hover: None,
                        modifiers: modifiers(),
                        time_ms: message_time_ms(),
                    },
                );
            }
            // Window-tool hover pass: the shell resolves the thumbnail under the pointer (it
            // owns the inventory) and owns the paint policy - skip the repaint entirely when
            // nothing changed, otherwise force the now-cheap cached hover paint synchronously.
            let (input, previous) = {
                // SAFETY: The borrow ends with this block, before run_machine derives its own.
                let state = unsafe { &*state_pointer };
                let local = local_point(state.model.source, point);
                let window_hover = hit_test_window_thumbnail(state, local).and_then(|handle| {
                    state.windows.as_ref().and_then(|windows| {
                        windows
                            .iter()
                            .copied()
                            .find(|candidate| candidate.handle == handle)
                    })
                });
                (
                    OverlayInput::PointerMoved {
                        point,
                        window_hover,
                        modifiers: modifiers(),
                        time_ms: message_time_ms(),
                    },
                    (
                        state.model.hovered.map(|candidate| candidate.handle),
                        state.model.hovered_control,
                    ),
                )
            };
            run_machine(hwnd, state_pointer, input);
            let unchanged = {
                // SAFETY: run_machine has returned; the borrow ends with this block.
                let state = unsafe { &*state_pointer };
                previous
                    == (
                        state.model.hovered.map(|candidate| candidate.handle),
                        state.model.hovered_control,
                    )
            };
            if unchanged {
                return LRESULT(0);
            }
            invalidate(hwnd);
            // SAFETY: Forces the now-cheap cached hover paint before this mouse message returns.
            let _ = unsafe { UpdateWindow(hwnd) };
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let input = {
                // SAFETY: The borrow ends with this block, before run_machine derives its own.
                let state = unsafe { &*state_pointer };
                let point = screen_point(state.model.source, lparam);
                let local = local_point(state.model.source, point);
                let window_slot = (state.model.tool == CaptureTool::Window)
                    .then(|| hit_test_window_thumbnail(state, local))
                    .flatten();
                OverlayInput::PointerDown {
                    point,
                    window_slot,
                    time_ms: message_time_ms(),
                }
            };
            run_machine(hwnd, state_pointer, input)
        }
        WM_LBUTTONUP => {
            let point = {
                // SAFETY: This borrow ends before run_machine derives its own.
                let state = unsafe { &*state_pointer };
                screen_point(state.model.source, lparam)
            };
            run_machine(
                hwnd,
                state_pointer,
                OverlayInput::PointerUp {
                    point,
                    modifiers: modifiers(),
                },
            )
        }
        WM_LBUTTONDBLCLK => {
            let point = {
                // SAFETY: This borrow ends before run_machine derives its own.
                let state = unsafe { &*state_pointer };
                screen_point(state.model.source, lparam)
            };
            run_machine(hwnd, state_pointer, OverlayInput::DoubleClicked { point })
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_RETURN.0) => {
            run_machine(hwnd, state_pointer, OverlayInput::ConfirmRequested)
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => {
            run_machine(hwnd, state_pointer, OverlayInput::CancelRequested)
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_Z.0) => {
            if key_is_autorepeat(lparam) {
                // The key is already down and the machine already knows. Restating it thirty
                // times a second would only churn the visibility comparison.
                return LRESULT(0);
            }
            run_machine(
                hwnd,
                state_pointer,
                OverlayInput::LoupeKeyChanged { held: true },
            )
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_F.0) => {
            if key_is_autorepeat(lparam) {
                // Holding F is not a request to flicker between the two views thirty times a
                // second. Only the transition from up to down means anything here.
                return LRESULT(0);
            }
            run_machine(hwnd, state_pointer, OverlayInput::ToggleView)
        }
        WM_KEYUP if wparam.0 == usize::from(VK_Z.0) => run_machine(
            hwnd,
            state_pointer,
            OverlayInput::LoupeKeyChanged { held: false },
        ),
        WM_SETFOCUS | WM_KILLFOCUS => {
            // Losing focus disarms the magnifier key, because a key released while another window
            // has focus is delivered to that window and would otherwise stay held forever.
            // Gaining it re-reads the keyboard: after an Alt+Tab away and back, Windows sends
            // only autorepeat WM_KEYDOWNs for a key that was already down and those are discarded
            // as repeats, so without this Z could never re-arm. The same read is what makes a Z
            // already held when the overlay opens work at all - SetFocus in run_overlay sends
            // this message synchronously, after the state pointer is installed.
            run_machine(
                hwnd,
                state_pointer,
                OverlayInput::LoupeKeyChanged {
                    held: loupe_key_after_focus_change(message == WM_SETFOCUS),
                },
            );
            // Focus notifications are the system's, not ours: consuming them would deny the
            // default procedure its caret and accessibility bookkeeping.
            // SAFETY: Default focus handling for this live window.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_TIMER if wparam.0 == LOUPE_TIMER_ID => run_machine(
            hwnd,
            state_pointer,
            OverlayInput::Tick {
                time_ms: message_time_ms(),
            },
        ),
        WM_KEYDOWN if arrow_direction(wparam).is_some() => {
            let (x, y) = arrow_direction(wparam).unwrap_or((0, 0));
            let modifiers = modifiers();
            // Autorepeat is not filtered here: holding an arrow down is exactly how a user walks
            // an edge across a few pixels, and Windows' own repeat rate is the right cadence for
            // it. Ctrl turns the same keys into a resize of the right and bottom edges.
            let step = modifiers.nudge_step();
            run_machine(
                hwnd,
                state_pointer,
                OverlayInput::Nudge {
                    dx: x * step,
                    dy: y * step,
                    resize: modifiers.ctrl,
                },
            )
        }
        WM_CAPTURECHANGED => {
            // A self-initiated ReleaseCapture round trip is protocol, not product state: consume
            // the shell's flag and never involve the machine.
            // SAFETY: This single-field borrow ends before run_machine derives its own.
            if consume_self_initiated_capture_change(unsafe {
                &mut (*state_pointer).releasing_pointer_capture
            }) {
                return LRESULT(0);
            }
            // A real loss ends the drag, so the rest timer has nothing left to watch.
            stop_loupe_timer(hwnd);
            run_machine(hwnd, state_pointer, OverlayInput::PointerCaptureLost)
        }
        WM_RBUTTONDOWN | WM_CLOSE => {
            run_machine(hwnd, state_pointer, OverlayInput::CancelRequested)
        }
        WM_OVERVIEW_RENDER_BATCH => {
            // SAFETY: The Box remains alive for the message loop; the borrow ends before this
            // arm returns, and rendering spawns only scoped workers that never message us.
            render_overview_batch(hwnd, unsafe { &mut *state_pointer }, wparam.0);
            LRESULT(0)
        }
        WM_PAINT => {
            paint(hwnd, state_pointer);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DESTROY => {
            // SAFETY: Ends only this overlay thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => {
            // SAFETY: Standard handling for messages Captastic does not consume.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

/// The unit step an arrow key asks for, or `None` for any other key.
const fn arrow_direction(wparam: WPARAM) -> Option<(i32, i32)> {
    let key = wparam.0 as u16;
    if key == VK_LEFT.0 {
        Some((-1, 0))
    } else if key == VK_RIGHT.0 {
        Some((1, 0))
    } else if key == VK_UP.0 {
        Some((0, -1))
    } else if key == VK_DOWN.0 {
        Some((0, 1))
    } else {
        None
    }
}

fn build_overlay_selection(
    state: &OverlayState,
    rect: Rect,
    kind: SelectionKind,
    window: Option<NativeWindowHandle>,
    window_frame: Option<CpuFrame>,
) -> OverlaySelection {
    // Preview counts and bytes are run-scoped cost telemetry, like window_overview_ns: they
    // describe the chooser inventory this run built and held, whatever kind was confirmed.
    // The live/frozen split, however, describes presentation at confirm time - leaving the
    // Window tool hides every live registration, so a non-Window confirm has zero live
    // previews and the whole inventory counts as frozen.
    let (live_previews, frozen_previews) = confirm_preview_split(
        kind,
        state.live_window_thumbnails.len(),
        state.window_thumbnails.len(),
    );
    // Resolved here rather than after the overlay closes: the window is certainly alive at the
    // moment it is confirmed, and may not be by the time the capture is delivered.
    let (window_title, window_application) = window
        .map(|handle| super::overlay::window_enumeration::describe_window(HWND(handle.raw())))
        .unwrap_or((None, None));
    OverlaySelection {
        rect,
        kind,
        window,
        window_title,
        window_application,
        selection_ns: duration_ns(state.started.elapsed()),
        preparation_ns: state.preparation_ns,
        window_overview_ns: state.window_overview_ns,
        window_preview_count: state.window_thumbnails.len(),
        window_live_preview_count: live_previews,
        window_frozen_preview_count: frozen_previews,
        // Only tiles Captastic rendered hold pixels; a DWM-drawn tile costs none, which is the
        // point of this number falling.
        window_preview_bytes: state
            .window_thumbnails
            .iter()
            .filter_map(|thumbnail| thumbnail.surface.as_ref())
            .map(|surface| surface.byte_length)
            .sum(),
        // The view that is on screen at this instant, not the one the run opened in: the whole
        // point of the toggle is that the user confirms what they are looking at.
        view: state.model.view,
        view_switched: state.view_switched,
        presenter_fallback_reason: state.presenter_fallback_reason.clone(),
        window_frame,
    }
}

/// Splits the chooser inventory into (live, frozen) counts for a confirm of the given kind.
/// Only a Window confirm can have live DWM previews on screen; every other kind hid them on
/// the way out of the Window tool.
const fn confirm_preview_split(kind: SelectionKind, live: usize, total: usize) -> (usize, usize) {
    let live = if matches!(kind, SelectionKind::Window) {
        live
    } else {
        0
    };
    (live, total.saturating_sub(live))
}

/// Runs one machine transition and applies the returned effects. The model borrow taken for the
/// transition ends before any effect executes, so effects that reenter this window procedure
/// (`ReleaseCapture`, `DestroyWindow`) can never observe a live `&mut` of the state.
fn run_machine(hwnd: HWND, state_pointer: *mut OverlayState, input: OverlayInput) -> LRESULT {
    // SAFETY: Single-field read; the borrow ends on this line.
    let view_before = unsafe { (*state_pointer).model.view };
    // SAFETY: The Box remains alive for the message loop; this borrow ends at the semicolon.
    let effects = transition(unsafe { &mut (*state_pointer).model }, input);
    // Observed here rather than announced by an effect, and latched rather than compared at the
    // end: whether the *user* changed the view is telemetry about the run, not a decision the
    // machine makes anything of, and a run switched twice ends where it started while still
    // having been switched. One place covers both the `F` key and the Options row.
    // SAFETY: This takes a whole-struct `&mut` to read `model.view` and write `view_switched`.
    // The Box remains alive for the message loop, `transition` has already returned so no other
    // borrow of the state is live, and nothing inside this block can reenter the window
    // procedure: it is two field accesses and no call. The borrow ends at the closing brace,
    // before `apply_overlay_effects` derives its own.
    unsafe {
        let state = &mut *state_pointer;
        state.view_switched = latch_view_switch(state.view_switched, view_before, state.model.view);
    }
    apply_overlay_effects(hwnd, state_pointer, effects);
    LRESULT(0)
}

/// Whether the user has changed the view at any point in this run, given what it was before an
/// input and what it is after.
///
/// Latched, never recomputed: a run switched to the frozen view and back ends in the view it
/// opened in, and comparing the two at the end would report it as never switched - which is
/// exactly the case the flag exists to tell apart.
const fn latch_view_switch(switched: bool, before: PreviewView, after: PreviewView) -> bool {
    // `PreviewView` is not `const`-comparable through `PartialEq`, so the match spells it out.
    switched
        || !matches!(
            (before, after),
            (PreviewView::Live, PreviewView::Live) | (PreviewView::Frozen, PreviewView::Frozen)
        )
}

fn apply_overlay_effects(
    hwnd: HWND,
    state_pointer: *mut OverlayState,
    effects: Vec<OverlayEffect>,
) {
    for effect in effects {
        match effect {
            OverlayEffect::SetCursor(intent) => {
                // SAFETY: Shared borrow for the cursor resources; released at the block's end.
                let state = unsafe { &*state_pointer };
                match intent {
                    CursorIntent::Arrow => set_arrow_cursor(),
                    CursorIntent::Move => set_move_cursor(),
                    CursorIntent::Crosshair => update_cursor(None, &state.region_cursor),
                    CursorIntent::Resize(handle) => {
                        update_cursor(Some(handle), &state.region_cursor)
                    }
                }
            }
            OverlayEffect::Invalidate => invalidate(hwnd),
            OverlayEffect::CapturePointer => {
                capture_pointer(hwnd);
                // The timer's lifetime is exactly the drag's, which is exactly the pointer
                // capture's: started here, stopped on release and on a real capture loss. There
                // is no path that takes capture without this effect.
                // SAFETY: Shared borrow released at the block's end.
                if machine::loupe_timer_wanted(unsafe { &(*state_pointer).model }) {
                    start_loupe_timer(hwnd);
                }
            }
            OverlayEffect::ReleasePointer => {
                // ReleaseCapture synchronously sends WM_CAPTURECHANGED back to this window. Arm
                // the protocol flag first so that reentrant notification is recognized as
                // self-initiated; the model transition has already committed its work, so the
                // reentrant callback can never erase state the caller still needs.
                stop_loupe_timer(hwnd);
                // SAFETY: This single-field mutation ends before ReleaseCapture can re-enter.
                unsafe { (*state_pointer).releasing_pointer_capture = true };
                release_pointer_capture();
                // Clear a stale marker if the platform did not deliver WM_CAPTURECHANGED.
                // SAFETY: ReleaseCapture has returned, so no borrow spans its callback.
                unsafe { (*state_pointer).releasing_pointer_capture = false };
            }
            OverlayEffect::ClearDimensionLabel => {
                // SAFETY: Local mutation on the owning thread; nothing is dispatched.
                unsafe { (*state_pointer).dimension_label_placement = None };
            }
            OverlayEffect::ClearSelectedWindowFrame => {
                // SAFETY: Local mutation on the owning thread; nothing is dispatched.
                unsafe { (*state_pointer).selected_window_frame = None };
            }
            OverlayEffect::ClearLiveThumbnails => {
                // SAFETY: The borrow ends before the next effect; unregistering DWM thumbnails
                // does not dispatch messages into this window procedure.
                unsafe { (*state_pointer).live_window_thumbnails.clear() };
            }
            OverlayEffect::HideLiveThumbnails => {
                // SAFETY: Shared borrow released at the block's end.
                let state = unsafe { &*state_pointer };
                hide_live_window_thumbnails(state);
            }
            OverlayEffect::RebuildOverviewCache => {
                // SAFETY: The borrow ends before the next effect.
                let state = unsafe { &mut *state_pointer };
                rebuild_window_overview_cache(state);
            }
            OverlayEffect::BuildWindowOverview => {
                // Deliberately synchronous: the chooser blocks the pump while thumbnails render
                // (M21), and this stage preserves that timing exactly.
                // SAFETY: The borrow ends before the next effect.
                let state = unsafe { &mut *state_pointer };
                start_window_overview_build(hwnd, state);
            }
            OverlayEffect::BuildSnapTargets => {
                // Enumeration happens here, in an effect handler, and never inside the machine:
                // the model borrow taken for the transition has already ended, so the Win32 calls
                // below cannot observe a live `&mut` of the state. The same shape as
                // UpdateWindowPreview -> WindowPreviewResolved.
                let targets = {
                    // SAFETY: The borrow ends before the feedback transition derives its own.
                    let state = unsafe { &mut *state_pointer };
                    build_snap_targets(state)
                };
                // SAFETY: The Box remains alive; this borrow ends at the semicolon.
                let effects = transition(
                    unsafe { &mut (*state_pointer).model },
                    OverlayInput::SnapTargetsReady { targets },
                );
                apply_overlay_effects(hwnd, state_pointer, effects);
            }
            OverlayEffect::UpdateWindowPreview { window } => {
                let rect = {
                    // The 700 ms blocking capture attempt stays synchronous here (M21); the
                    // non-sticky Unavailable retry lives in update_window_preview's cache.
                    // SAFETY: The borrow ends before the feedback transition derives its own.
                    let state = unsafe { &mut *state_pointer };
                    update_window_preview(state, window);
                    state.selected_window_frame =
                        ready_window_preview(state, window).map(|preview| preview.frame.clone());
                    state
                        .selected_window_frame
                        .as_ref()
                        .map(|frame| frame.metadata.source_rect)
                };
                // Feed the outcome back so the machine decides confirm versus stay-in-chooser.
                // SAFETY: The Box remains alive; this borrow ends at the semicolon.
                let effects = transition(
                    unsafe { &mut (*state_pointer).model },
                    OverlayInput::WindowPreviewResolved { rect },
                );
                apply_overlay_effects(hwnd, state_pointer, effects);
            }
            OverlayEffect::RefreshWindowChrome => {
                // SAFETY: The borrow ends before the next effect. DWM thumbnail registration
                // does not dispatch messages into this window procedure.
                let state = unsafe { &mut *state_pointer };
                refresh_live_window_thumbnails(state);
                rebuild_window_overview_cache(state);
                // Moving chrome can uncover a tile DWM was drawing, which then needs pixels.
                ensure_overview_batch_posted(hwnd, state);
            }
            OverlayEffect::PersistToolbarCenter { position } => {
                // SAFETY: Shared borrow released at the block's end.
                let state = unsafe { &*state_pointer };
                remember_toolbar_position(
                    state.ui_updates.as_ref(),
                    &state.reference_metadata.display_id,
                    position,
                    state.model.display_environment,
                );
            }
            OverlayEffect::PersistSnapPreference { enabled } => {
                // SAFETY: Shared borrow released at the block's end.
                let state = unsafe { &*state_pointer };
                if let Some(sink) = state.ui_updates.as_ref() {
                    sink.submit(OverlayUiUpdate::SnapToWindows { enabled });
                }
            }
            OverlayEffect::PersistRegionZoom { mode } => {
                // SAFETY: Shared borrow released at the block's end.
                let state = unsafe { &*state_pointer };
                if let Some(sink) = state.ui_updates.as_ref() {
                    sink.submit(OverlayUiUpdate::RegionZoom {
                        mode: mode.to_config(),
                    });
                }
            }
            OverlayEffect::PersistInteraction { region } => {
                // SAFETY: Shared borrow released at the block's end.
                let state = unsafe { &*state_pointer };
                remember_overlay_interaction(
                    state.ui_updates.as_ref(),
                    &state.reference_metadata.display_id,
                    state.model.source,
                    state.model.tool,
                    region,
                    state.reference_metadata.rotation_degrees,
                );
            }
            OverlayEffect::Close(outcome) => {
                match outcome {
                    CloseOutcome::Confirmed { rect, kind, window } => {
                        // SAFETY: The borrow ends before close_overlay dispatches destruction.
                        let state = unsafe { &mut *state_pointer };
                        let window_frame = (kind == SelectionKind::Window)
                            .then(|| state.selected_window_frame.clone())
                            .flatten();
                        let rect = window_frame
                            .as_ref()
                            .map(|frame| frame.metadata.source_rect)
                            .unwrap_or(rect);
                        let selection =
                            build_overlay_selection(state, rect, kind, window, window_frame);
                        state.result = Some(selection);
                        close_overlay(hwnd);
                    }
                    CloseOutcome::Cancelled => close_overlay(hwnd),
                    CloseOutcome::DisplayConfigurationInvalidated { reason } => {
                        // Recorded before the window goes away, because it is the only difference
                        // between this close and the Escape above: both leave `result` empty, and
                        // a caller that cannot tell them apart reports a cancellation the user
                        // never made.
                        // SAFETY: The borrow ends before close dispatches destruction.
                        let state = unsafe { &mut *state_pointer };
                        state.display_change_reason = Some(reason);
                        display_configuration_changed_and_close(hwnd, reason);
                    }
                }
                // Destruction has been dispatched; nothing may run after it.
                return;
            }
        }
    }
}

fn close_overlay(hwnd: HWND) {
    // SAFETY: hwnd is the live overlay window and this function runs on its owner thread.
    let _ = unsafe { DestroyWindow(hwnd) };
}

fn display_configuration_changed_and_close(hwnd: HWND, reason: &'static str) {
    crate::dxgi::mark_display_configuration_changed(reason);
    // Do not persist geometry from a display configuration that is no longer current.
    // SAFETY: hwnd is the live overlay window and this function runs on its owner thread.
    let _ = unsafe { DestroyWindow(hwnd) };
}

fn paint(hwnd: HWND, state_pointer: *mut OverlayState) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: paint is writable storage and EndPaint balances this call before return.
    let device = unsafe { BeginPaint(hwnd, &mut paint) };
    {
        // SAFETY: The Box remains alive for the message loop. BeginPaint has completed before this
        // borrow is created, and the borrow ends before EndPaint can synchronously send messages.
        let state = unsafe { &mut *state_pointer };
        compose_overlay_state(state);
        match state.presenter {
            Presenter::Layered => {
                if let Err(error) = present_layer(hwnd, state) {
                    log::error!("overlay presentation failed: {error}");
                }
            }
            Presenter::Opaque => copy_overlay_to_paint_device(device, state),
        }
    }
    // SAFETY: Balances BeginPaint for this exact hwnd/paint structure after releasing state.
    unsafe { EndPaint(hwnd, &paint) };
}

/// Removes every chooser entry whose source window no longer exists and returns the dead
/// handles. The tiles, live DWM registrations, candidates, and any preview pixels disappear
/// together, so the layout closes the gap and a destroyed window is no longer clickable.
/// Windows that are still alive keep their entries even when their live thumbnail or preview
/// has failed - failure stays retryable, only destruction prunes.
fn prune_dead_window_sources(state: &mut OverlayState) -> Vec<NativeWindowHandle> {
    let dead: Vec<NativeWindowHandle> = state
        .window_thumbnails
        .iter()
        .map(|thumbnail| thumbnail.handle)
        // SAFETY: IsWindow validates a handle without retaining or dereferencing it.
        .filter(|handle| !unsafe { IsWindow(HWND(handle.raw())) }.as_bool())
        .collect();
    if dead.is_empty() {
        return dead;
    }
    for handle in &dead {
        log::debug!(
            "pruning chooser slot for destroyed window handle=0x{:X}",
            handle.raw()
        );
    }
    state
        .window_thumbnails
        .retain(|thumbnail| !dead.contains(&thumbnail.handle));
    // Dropping a live registration unregisters its DWM thumbnail.
    state
        .live_window_thumbnails
        .retain(|preview| !dead.contains(&preview.handle));
    if let Some(windows) = state.windows.as_mut() {
        windows.retain(|candidate| !dead.contains(&candidate.handle));
    }
    let preview_died = match &state.window_preview {
        Some(WindowPreviewState::Ready(preview)) => dead.contains(&preview.handle),
        Some(WindowPreviewState::Unavailable(handle)) => dead.contains(handle),
        None => false,
    };
    if preview_died {
        state.window_preview = None;
    }
    dead
}

fn compose_overlay_state(state: &mut OverlayState) {
    let width = state.surface.width;
    if state.model.tool == CaptureTool::Window {
        // Destroyed sources are pruned once per paint. The overlay's only timer is the region
        // tool's rest timer, which never runs under the Window tool, so a paint is still the one
        // and only moment this inventory is revisited. Any model state pointing at a pruned
        // window is cleared before the chooser surface is rebuilt.
        let dead = prune_dead_window_sources(state);
        if !dead.is_empty() {
            for effect in machine::window_sources_pruned(&mut state.model, &dead) {
                debug_assert!(matches!(effect, OverlayEffect::ClearSelectedWindowFrame));
                state.selected_window_frame = None;
            }
            rebuild_window_overview_cache(state);
        }
        let cache_matches = state
            .window_overview_cache
            .as_ref()
            .is_some_and(|cache| cache.dim_background == state.model.dim_background);
        if !cache_matches {
            rebuild_window_overview_cache(state);
        }
        if let Some(cache) = &state.window_overview_cache {
            // SAFETY: Both surfaces are same-sized live DIBs. This is the complete static chooser.
            let _ = unsafe {
                BitBlt(
                    state.back_buffer.device,
                    0,
                    0,
                    width,
                    state.surface.height,
                    cache.surface.device,
                    0,
                    0,
                    SRCCOPY,
                )
            };
        } else {
            compose_window_overview_background(&state.back_buffer, state);
            draw_window_overview_static(&state.back_buffer, state);
        }
        draw_window_overview_interactive(state);
    } else if state.model.view == PreviewView::Live {
        paint_live_selection_background(state);
    } else {
        // SAFETY: Both memory contexts are live, compatible GDI DCs with selected DIBs. Compose
        // the entire visual off-screen so the visible overlay never sees a partial update.
        let _ = unsafe {
            BitBlt(
                state.back_buffer.device,
                0,
                0,
                width,
                state.surface.height,
                state.surface.device,
                0,
                0,
                SRCCOPY,
            )
        };
    }
    // Window mode's dim wash is already part of its static overview cache. Other tools compose it
    // here so the selected region can subsequently restore its original frozen pixels.
    if state.model.tool != CaptureTool::Window
        && state.model.dim_background
        && state.model.view == PreviewView::Frozen
    {
        let _ = apply_dim_wash(
            state.back_buffer.device,
            state.dimmer.device,
            width,
            state.surface.height,
            DIM_ALPHA,
        );
    }
    // Laid out *and sampled* before anything is drawn, so the dimension badge can avoid it, and
    // painted after the selection chrome so it sits on top of the outline it is magnifying. The
    // sampling has to happen on this side of the badge: a sample that fails cancels the
    // magnifier, and a badge that had already reserved space for one would sit off to the side
    // avoiding nothing.
    prepare_loupe(state);
    if state.model.tool != CaptureTool::Window {
        if let Some(rect) = state.model.selection {
            if state.model.view == PreviewView::Frozen {
                restore_highlight(state, rect);
            }
            draw_outline(state.back_buffer.device, state.model.source, rect);
            draw_snap_guides(state);
            if state.model.selection_kind == Some(SelectionKind::Region) {
                draw_resize_handles(
                    state.back_buffer.device,
                    state.model.source,
                    rect,
                    state.model.display_environment.metrics,
                );
                draw_region_dimensions(state, rect);
            }
        }
    }
    draw_loupe(state);
    draw_toolbar(state);
}

/// Decides where the magnifier goes this paint and gets its pixels, or settles that there is no
/// magnifier this paint.
///
/// Both halves belong together, and ahead of every draw. `loupe_layout` is what the dimension
/// badge treats as an obstacle, so it has to be final before the badge is placed — a sample that
/// fails cancels the magnifier, and a badge that had already moved aside for one would be
/// avoiding nothing.
fn prepare_loupe(state: &mut OverlayState) {
    state.loupe_layout = plan_loupe(state);
    let Some(layout) = state.loupe_layout else {
        return;
    };
    if !sample_loupe_source(state, layout) {
        // Without pixels there is nothing honest to show, and the live presenter's alpha pass
        // must not carve an opaque hole where a magnifier is not.
        state.loupe_layout = None;
    }
}

/// Where the magnifier goes this paint, or `None` when it is not showing.
fn plan_loupe(state: &OverlayState) -> Option<LoupeLayout> {
    let pointer = state.model.pointer_local?;
    machine::loupe_visible(&state.model).then(|| {
        layout_loupe(
            UiRect {
                left: 0,
                top: 0,
                right: state.surface.width,
                bottom: state.surface.height,
            },
            pointer,
            state.model.display_environment.metrics.loupe_tokens(),
        )
    })
}

/// Draws the guide line for each snapped axis: a hairline in the accent colour running the full
/// length of the target edge the selection landed on, so it is obvious *what* it snapped to.
///
/// A trailing guide is painted at `position - 1`. `position` is the selection's exclusive right or
/// bottom edge, so the last pixel it actually covers is the one before it; drawing at `position`
/// would put the line outside the very selection it is explaining. `draw_lines` uses `LineTo`,
/// which excludes its end point, so the span is painted half-open exactly as it is stored.
fn draw_snap_guides(state: &OverlayState) {
    let source = state.model.source;
    for guide in state.model.active_snaps.iter().flatten() {
        let position = guide.position - if guide.trailing { 1 } else { 0 };
        let points = match guide.axis {
            SnapAxis::X => {
                let x = position.saturating_sub(source.x);
                [
                    (x, guide.span.0.saturating_sub(source.y)),
                    (x, guide.span.1.saturating_sub(source.y)),
                ]
            }
            SnapAxis::Y => {
                let y = position.saturating_sub(source.y);
                [
                    (guide.span.0.saturating_sub(source.x), y),
                    (guide.span.1.saturating_sub(source.x), y),
                ]
            }
        };
        draw_lines(state.back_buffer.device, &points, rgb(86, 156, 255), 1);
    }
}

fn copy_overlay_to_paint_device(device: HDC, state: &OverlayState) {
    // SAFETY: The fully composed back buffer is copied to the live paint DC in one operation.
    let _ = unsafe {
        BitBlt(
            device,
            0,
            0,
            state.surface.width,
            state.surface.height,
            state.back_buffer.device,
            0,
            0,
            SRCCOPY,
        )
    };
}

fn paint_live_selection_background(state: &OverlayState) {
    // SAFETY: Flushes queued GDI work so none of it can land on top of the CPU fill below.
    let _ = unsafe { GdiFlush() };
    fill_live_background(&state.back_buffer);
}

/// Puts the composed back buffer on the screen through the layered window.
///
/// Both views present through this one path. The live view needs per-pixel alpha, so it runs the
/// alpha pass and asks for `ULW_ALPHA`. The frozen view is a still image with nothing behind it
/// worth seeing: it asks for `ULW_OPAQUE` and skips the alpha pass entirely, which is both
/// correct — every pixel of a frozen overlay is Captastic's — and a full-screen per-pixel loop
/// saved on every paint.
fn present_layer(hwnd: HWND, state: &OverlayState) -> Result<(), CaptureError> {
    let live = state.model.view == PreviewView::Live;
    if live {
        prepare_live_layer_pixels(state);
    } else {
        // The frozen view skips the alpha pass, and with it the `GdiFlush` that pass performs
        // before touching the DIB bytes. The flush is still required: `UpdateLayeredWindow` reads
        // the back buffer's bits directly, and GDI drawing this thread has queued but not yet
        // executed would otherwise land after the compositor had taken its copy.
        // SAFETY: Flushes the calling thread's own GDI batch. It takes no arguments, touches no
        // handle this function owns, and cannot fail in a way that matters here.
        let _ = unsafe { GdiFlush() };
    }
    let destination = POINT {
        x: state.model.source.x,
        y: state.model.source.y,
    };
    let source = POINT { x: 0, y: 0 };
    let size = SIZE {
        cx: state.back_buffer.width,
        cy: state.back_buffer.height,
    };
    // The opaque blend is stated rather than left to `ULW_OPAQUE` ignoring it: were the flag ever
    // honoured together with a blend, this one still composes the layer exactly as drawn.
    let blend = BLENDFUNCTION {
        BlendOp: 0,
        BlendFlags: 0,
        SourceConstantAlpha: u8::MAX,
        AlphaFormat: if live { AC_SRC_ALPHA as u8 } else { 0 },
    };
    // SAFETY: A null HWND obtains the desktop DC used only for palette matching during this call.
    let screen = unsafe { GetDC(None) };
    if screen.0 == 0 {
        return Err(last_error("get_live_overlay_screen_dc"));
    }
    // SAFETY: hwnd is this process's live layered top-level window. The destination, size, source,
    // blend, and selected back-buffer DIB remain valid for the duration of the call.
    let result = unsafe {
        UpdateLayeredWindow(
            hwnd,
            screen,
            Some(&destination),
            Some(&size),
            state.back_buffer.device,
            Some(&source),
            COLORREF(0),
            Some(&blend),
            if live { ULW_ALPHA } else { ULW_OPAQUE },
        )
    };
    // SAFETY: Balances the successful GetDC(None) above on this thread.
    unsafe { ReleaseDC(None, screen) };
    result.map_err(|error| overlay_error("present_overlay_layer", error))
}

fn prepare_live_layer_pixels(state: &OverlayState) {
    // SAFETY: Flushes this thread's queued GDI drawing before the CPU updates the DIB alpha bytes.
    let _ = unsafe { GdiFlush() };
    let width = state.back_buffer.width.max(0) as usize;
    let height = state.back_buffer.height.max(0) as usize;
    let selection = state
        .model
        .selection
        .and_then(|rect| rect.intersection(state.model.source))
        .map(|rect| RECT {
            left: rect.x.saturating_sub(state.model.source.x),
            top: rect.y.saturating_sub(state.model.source.y),
            right: i32::try_from(rect.right().saturating_sub(i64::from(state.model.source.x)))
                .unwrap_or(state.back_buffer.width),
            bottom: i32::try_from(
                rect.bottom()
                    .saturating_sub(i64::from(state.model.source.y)),
            )
            .unwrap_or(state.back_buffer.height),
        });
    // The view, not the whole box. Only the magnified pixels arrive by StretchBlt; every other
    // part of the magnifier - the round box, the grid, the edges, the label - is GDI-drawn and
    // is already opaque through the coverage sentinel. Forcing the bounding box opaque painted
    // the four corner wedges *outside* the rounded border solid black.
    let loupe = state.loupe_layout.map(|layout| layout.view);
    // SAFETY: The back buffer uniquely owns this writable DIB on the overlay thread.
    let pixels = unsafe {
        std::slice::from_raw_parts_mut(state.back_buffer.bits, state.back_buffer.byte_length)
    };
    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 4;
            // GDI zeroed the alpha byte of every pixel the chrome pass painted; the background
            // fill's sentinel survives everywhere else. This is coverage, not color, so the
            // pure-black contrast halo and handle rings classify as chrome (M19).
            let drawn = pixels[offset + 3] == 0;
            let in_selection = selection.is_some_and(|rect| {
                x >= rect.left.max(0) as usize
                    && x < rect.right.max(0) as usize
                    && y >= rect.top.max(0) as usize
                    && y < rect.bottom.max(0) as usize
            });
            pixels[offset + 3] = live_pixel_alpha(
                state.model.tool,
                state.model.dim_background,
                in_selection,
                drawn,
                loupe.is_some_and(|bounds| {
                    x >= bounds.left.max(0) as usize
                        && x < bounds.right.max(0) as usize
                        && y >= bounds.top.max(0) as usize
                        && y < bounds.bottom.max(0) as usize
                }),
            );
        }
    }
}

const fn live_pixel_alpha(
    tool: CaptureTool,
    dim_background: bool,
    in_selection: bool,
    drawn: bool,
    in_loupe: bool,
) -> u8 {
    // The magnified pixels are opaque by fiat rather than by coverage. They arrive by StretchBlt,
    // which copies the source's alpha byte rather than zeroing it the way GDI's drawing calls do,
    // so the coverage sentinel cannot classify them - and a see-through magnifier over the very
    // pixels it is magnifying would be worse than none. `in_loupe` is deliberately the magnified
    // view alone: the chrome around it is drawn, and the rounded box's corner wedges are not part
    // of the magnifier at all.
    if in_loupe || matches!(tool, CaptureTool::Window) || drawn {
        u8::MAX
    } else if in_selection || !dim_background {
        LIVE_HIT_TEST_ALPHA
    } else {
        DIM_ALPHA
    }
}

fn update_window_preview(state: &mut OverlayState, target: Option<NativeWindowHandle>) {
    let Some(handle) = target else {
        return;
    };
    if let Some(cached) = &state.window_preview {
        if cached.satisfies(handle) {
            return;
        }
        if matches!(cached, WindowPreviewState::Unavailable(previous) if *previous == handle) {
            log::debug!(
                "retrying window preview for handle {:#x} after an earlier capture failure",
                handle.raw()
            );
        }
    }
    // A fresh attempt always replaces the cached entry: success stores Ready, another failure
    // refreshes Unavailable, and no other overlay state is touched either way.
    state.window_preview = match capture_window_visual(handle, &state.reference_metadata) {
        Ok(capture) => match FrozenSurface::from_straight_alpha(
            capture.frame.width(),
            capture.frame.height(),
            capture.frame.pixels(),
        ) {
            Ok(surface) => Some(WindowPreviewState::Ready(Box::new(WindowPreview {
                handle,
                frame: capture.frame,
                surface,
                corner_radius_px: capture.corner_radius_px,
            }))),
            Err(_) => Some(WindowPreviewState::Unavailable(handle)),
        },
        Err(_) => Some(WindowPreviewState::Unavailable(handle)),
    };
}

fn capture_live_window_backdrop(
    destination: &FrozenSurface,
    source: Rect,
) -> Result<(), CaptureError> {
    // Capture before the overlay HWND is created, ensuring the visual-only backdrop cannot contain
    // Captastic itself. CAPTUREBLT includes layered application windows in the desktop snapshot.
    // SAFETY: A null HWND obtains the desktop DC, which remains live until the matching ReleaseDC.
    let screen = unsafe { GetDC(None) };
    if screen.0 == 0 {
        return Err(last_error("get_live_window_backdrop_dc"));
    }
    // SAFETY: The destination DIB is at least source.width by source.height and both DCs remain
    // valid for this synchronous copy. Physical desktop coordinates may legitimately be negative.
    let copied = unsafe {
        BitBlt(
            destination.device,
            0,
            0,
            destination.width,
            destination.height,
            screen,
            source.x,
            source.y,
            SRCCOPY | CAPTUREBLT,
        )
    };
    // SAFETY: Balances the successful GetDC(None) above on this thread.
    unsafe { ReleaseDC(None, screen) };
    copied.map_err(|error| overlay_error("capture_live_window_backdrop", error))
}

fn ensure_window_mode_assets(state: &mut OverlayState) {
    if state.window_assets_ready {
        return;
    }
    ensure_window_inventory(state);
    let reusable = state.cached_blurred_background.take();
    state.blurred_background = build_blurred_background(&state.surface, 24, reusable).ok();
    state.window_assets_ready = true;
}

/// Enumerates the capturable windows on this display, once per run.
///
/// Split out of [`ensure_window_mode_assets`] because the region tool needs the same inventory
/// for edge snapping and emphatically does not need the blurred chooser backdrop that used to
/// come with it — building that costs a full-display downscale the region tool would never look
/// at. Whichever tool asks first pays; the other then finds the list already there.
fn ensure_window_inventory(state: &mut OverlayState) {
    if state.windows.is_some() {
        return;
    }
    {
        let displays = match crate::dxgi::enumerate_displays() {
            Ok(displays)
                if displays
                    .iter()
                    .any(|display| display.id == state.reference_metadata.display_id) =>
            {
                displays
            }
            Ok(_) => {
                log::warn!(
                    "captured display {} is absent from the window-mode topology snapshot; using captured bounds only",
                    state.reference_metadata.display_id.0
                );
                vec![fallback_display_info(state)]
            }
            Err(error) => {
                log::warn!(
                    "window-mode display discovery failed: {error}; using captured bounds only"
                );
                vec![fallback_display_info(state)]
            }
        };
        state.windows = Some(
            match enumerate_visible_windows(
                state.model.source,
                &state.reference_metadata.display_id,
                displays,
            ) {
                Ok(windows) => windows,
                Err(error) => {
                    log::warn!(
                        "window-mode enumeration failed for display {}: {error}",
                        state.reference_metadata.display_id.0
                    );
                    Vec::new()
                }
            },
        );
    }
}

/// The ordered edge inventory the region tool snaps against, built once per run.
///
/// Windows come first, in the front-to-back order `EnumWindows` reports, so the topmost window
/// wins a distance tie; then the work area (the taskbar's inner edge, which is where a region
/// meant to exclude the taskbar belongs); then the display itself, last, so it can only ever win
/// when nothing else is in reach. Every frame is clipped to the captured display, because an edge
/// off the side of it is not an edge the user can put a region on.
fn build_snap_targets(state: &mut OverlayState) -> SnapTargets {
    let started = Instant::now();
    ensure_window_inventory(state);
    let source = state.model.source;
    // The work area arrives overlay-local (it is measured against the captured display's origin);
    // every other target here is in desktop coordinates.
    let work_area = state.model.display_environment.work_area;
    let work_area = Rect::from_edges(
        i64::from(work_area.left) + i64::from(source.x),
        i64::from(work_area.top) + i64::from(source.y),
        i64::from(work_area.right) + i64::from(source.x),
        i64::from(work_area.bottom) + i64::from(source.y),
    );
    let targets = snap_targets_from(
        state
            .windows
            .iter()
            .flatten()
            .map(|candidate| candidate.frame),
        work_area,
        source,
    );
    log::debug!(
        "region snap inventory for display {}: {} targets in {} ns",
        state.reference_metadata.display_id.0,
        targets.len(),
        duration_ns(started.elapsed()),
    );
    if log::log_enabled!(log::Level::Trace) {
        for target in targets.iter() {
            log::trace!(
                "snap target kind={:?} rect={}x{}{:+}{:+}",
                target.kind,
                target.rect.width,
                target.rect.height,
                target.rect.x,
                target.rect.y,
            );
        }
    }
    targets
}

/// Assembles the inventory from geometry alone, so the ordering rule that the whole tie-break
/// rests on is testable without a desktop.
fn snap_targets_from(
    frames: impl IntoIterator<Item = Rect>,
    work_area: Option<Rect>,
    source: Rect,
) -> SnapTargets {
    let mut targets: Vec<SnapTarget> = frames
        .into_iter()
        .filter_map(|frame| frame.intersection(source))
        .map(|rect| SnapTarget {
            rect,
            kind: SnapTargetKind::Window,
        })
        .collect();
    // A work area equal to the display carries no edge the display does not already carry, and
    // listing it twice would only make a duplicate line appear under the guide.
    if let Some(rect) = work_area
        .and_then(|rect| rect.intersection(source))
        .filter(|rect| *rect != source)
    {
        targets.push(SnapTarget {
            rect,
            kind: SnapTargetKind::WorkArea,
        });
    }
    targets.push(SnapTarget {
        rect: source,
        kind: SnapTargetKind::Display,
    });
    SnapTargets::new(targets)
}

fn fallback_display_info(state: &OverlayState) -> DisplayInfo {
    DisplayInfo {
        id: state.reference_metadata.display_id.clone(),
        name: state.reference_metadata.display_id.0.clone(),
        bounds: state.model.source,
        scale_factor: state.model.display_environment.metrics.dpi as f32
            / UiMetrics::BASE_DPI as f32,
        rotation_degrees: state.reference_metadata.rotation_degrees,
        is_primary: false,
    }
}

/// The pure scheduling core of the incremental overview build: an ordered render queue of
/// (handle, attempt) pairs stamped with a generation. Retryable failures re-queue exactly once,
/// at the back - the same ordering the old synchronous two-pass loop produced (first-pass
/// successes in enumeration order, retry successes after them).
struct OverviewBuildQueue {
    generation: usize,
    pending: VecDeque<(NativeWindowHandle, u32)>,
}

impl OverviewBuildQueue {
    fn new(generation: usize, handles: impl IntoIterator<Item = NativeWindowHandle>) -> Self {
        Self {
            generation,
            pending: handles.into_iter().map(|handle| (handle, 0)).collect(),
        }
    }

    /// True when a posted batch message belongs to this build.
    fn accepts(&self, generation: usize) -> bool {
        self.generation == generation
    }

    fn take_batch(&mut self, size: usize) -> Vec<(NativeWindowHandle, u32)> {
        let size = size.max(1).min(self.pending.len());
        self.pending.drain(..size).collect()
    }

    /// Re-queues a retryable failure once; a second failure (or a non-retryable one, which the
    /// caller never passes here) drops the window from the overview.
    fn requeue(&mut self, handle: NativeWindowHandle, attempt: u32) -> bool {
        if attempt == 0 {
            self.pending.push_back((handle, 1));
            return true;
        }
        false
    }

    /// Adds a window that turned out to need a render after all, unless it is already queued.
    ///
    /// A tile can lose its live preview at any time — the toolbar moves over it, or a DWM
    /// registration stops working — and then it needs pixels it was never going to be given.
    fn enqueue(&mut self, handle: NativeWindowHandle) -> bool {
        if self.pending.iter().any(|(queued, _)| *queued == handle) {
            return false;
        }
        self.pending.push_back((handle, 0));
        true
    }

    fn is_done(&self) -> bool {
        self.pending.is_empty()
    }
}

struct WindowOverviewBuild {
    queue: OverviewBuildQueue,
    started: Instant,
}

fn post_overview_batch(hwnd: HWND, generation: usize) {
    // SAFETY: hwnd is the live overlay window; the message is consumed by this thread's pump.
    let _ = unsafe {
        PostMessageW(
            hwnd,
            WM_OVERVIEW_RENDER_BATCH,
            WPARAM(generation),
            LPARAM(0),
        )
    };
}

/// Starts (or resumes) the incremental chooser build. The chooser chrome appears immediately -
/// thumbnails stream in batch by batch via [`WM_OVERVIEW_RENDER_BATCH`], keeping the message
/// pump live for paint, Escape, and cancellation between batches (M21). Re-activating the
/// Window tool mid-build resumes the existing queue rather than restarting it.
fn start_window_overview_build(hwnd: HWND, state: &mut OverlayState) {
    let started = Instant::now();
    ensure_window_mode_assets(state);
    if state.window_overview_build.is_none() && state.window_thumbnails.is_empty() {
        let handles: Vec<NativeWindowHandle> = state
            .windows
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|candidate| candidate.handle)
            .collect();
        // Every window gets a tile immediately, shaped from what DWM already knows. Only the
        // windows DWM cannot draw are queued for a render, because rendering one it *can* draw
        // costs tens of milliseconds to produce pixels the compositor covers a moment later.
        let needs_render = seed_window_tiles(state, &handles);
        if !needs_render.is_empty() {
            state.overview_build_generation = state.overview_build_generation.wrapping_add(1);
            state.window_overview_build = Some(WindowOverviewBuild {
                queue: OverviewBuildQueue::new(state.overview_build_generation, needs_render),
                started,
            });
        }
    }
    refresh_live_window_thumbnails(state);
    rebuild_window_overview_cache(state);
    invalidate(hwnd);
    match &state.window_overview_build {
        Some(build) => post_overview_batch(hwnd, build.queue.generation),
        // Nothing to render (no candidates, or thumbnails already built): the build is complete
        // as of this call, matching the old synchronous telemetry for the trivial cases.
        None => {
            if state.window_overview_ns.is_none() {
                state.window_overview_ns = Some(duration_ns(started.elapsed()));
            }
        }
    }
}

/// Renders one batch of the in-flight build. Stale deliveries - after a tool switch away, after
/// completion, or from a superseded build - are ignored via the tool and generation guards.
fn render_overview_batch(hwnd: HWND, state: &mut OverlayState, generation: usize) {
    if state.model.tool != CaptureTool::Window {
        return;
    }
    let Some(build) = state.window_overview_build.as_mut() else {
        return;
    };
    if !build.queue.accepts(generation) {
        return;
    }
    let batch = build.queue.take_batch(WINDOW_THUMBNAIL_RENDER_BATCH);
    let rendered = thread::scope(|scope| {
        let workers: Vec<_> = batch
            .iter()
            .copied()
            .map(|(handle, attempt)| {
                let metadata = &state.reference_metadata;
                scope.spawn(move || {
                    (
                        handle,
                        attempt,
                        capture_window_thumbnail(handle, metadata, WINDOW_THUMBNAIL_MAX_PIXELS),
                    )
                })
            })
            .collect();
        workers
            .into_iter()
            .filter_map(|worker| worker.join().ok())
            .collect::<Vec<_>>()
    });
    let build = state
        .window_overview_build
        .as_mut()
        .expect("the build is untouched while its batch renders");
    let mut new_thumbnails = Vec::new();
    for (handle, attempt, capture) in rendered {
        let capture = match capture {
            Ok(capture) => capture,
            Err(error) if error.retryable && build.queue.requeue(handle, attempt) => continue,
            Err(error) => {
                log::debug!(
                    "window handle=0x{:X} omitted from overview after render failure: {error}",
                    handle.raw()
                );
                continue;
            }
        };
        let Ok(surface) = FrozenSurface::from_straight_alpha(
            capture.frame.width(),
            capture.frame.height(),
            capture.frame.pixels(),
        ) else {
            continue;
        };
        new_thumbnails.push(WindowThumbnail {
            handle,
            source_width: surface.width,
            source_height: surface.height,
            corner_radius_px: capture.corner_radius_px,
            surface: Some(surface),
        });
    }
    let finished = build.queue.is_done();
    let started = build.started;
    // A render either fills in a tile DWM could not draw, or adds one in frozen mode. Replacing
    // in place keeps the chooser's ordering stable while tiles arrive.
    for rendered in new_thumbnails {
        match state
            .window_thumbnails
            .iter_mut()
            .find(|tile| tile.handle == rendered.handle)
        {
            Some(tile) => *tile = rendered,
            None => state.window_thumbnails.push(rendered),
        }
    }
    if finished {
        state.window_overview_build = None;
        refresh_live_window_thumbnails(state);
        rebuild_window_overview_cache(state);
        if state.window_overview_ns.is_none() {
            state.window_overview_ns = Some(duration_ns(started.elapsed()));
        }
        // The refresh above may have found a tile that still needs rendering.
        ensure_overview_batch_posted(hwnd, state);
    } else {
        rebuild_window_overview_cache(state);
        post_overview_batch(hwnd, generation);
    }
    invalidate(hwnd);
}

/// Builds a tile for each window, and reports which of them still need pixels.
///
/// Under the layered presenter a tile needs only the shape of its window, which
/// `DwmQueryThumbnailSourceSize` answers in about a millisecond without capturing anything.
/// Without it there is no compositor drawing tiles for us, so every window needs a render.
///
/// Keyed on the presenter, not the view: DWM composes its thumbnails against the destination
/// window regardless of which pixels Captastic is painting underneath, and the Window tool is
/// unaffected by the view in any case — clicking a tile always renders the window fresh.
fn seed_window_tiles(
    state: &mut OverlayState,
    handles: &[NativeWindowHandle],
) -> Vec<NativeWindowHandle> {
    let mut needs_render = Vec::new();
    let layered = state.presenter == Presenter::Layered;
    for handle in handles {
        let source = layered
            .then(|| dwm_source_size(state.overlay_hwnd, *handle))
            .flatten()
            .or_else(|| native_window_size(*handle));
        let Some((source_width, source_height)) = source else {
            // Nothing can say how big this window is, so it cannot be laid out at all.
            log::debug!(
                "window handle=0x{:X} reported no size; omitted from the chooser",
                handle.raw()
            );
            continue;
        };
        // Without DWM every tile is drawn here, so every tile needs pixels.
        if !layered {
            needs_render.push(*handle);
        }
        state.window_thumbnails.push(WindowThumbnail {
            handle: *handle,
            source_width,
            source_height,
            // Read from DWM attributes rather than derived from a capture, so a tile has its
            // outline shape before — or without — ever being rendered.
            corner_radius_px: crate::window_capture::window_corner_radius(HWND(handle.raw())),
            surface: None,
        });
    }
    needs_render
}

/// The size DWM holds for a window, without capturing it.
///
/// Registered and dropped immediately: the registration exists only to ask the question, and the
/// real one that draws the tile is made later, once the layout is known.
fn dwm_source_size(destination: HWND, handle: NativeWindowHandle) -> Option<(i32, i32)> {
    if destination.0 == 0 {
        return None;
    }
    let thumbnail = DwmThumbnail::register(destination, HWND(handle.raw())).ok()?;
    let size = thumbnail.source_size().ok()?;
    (size.cx > 0 && size.cy > 0).then_some((size.cx, size.cy))
}

/// The window's own bounds, for anything DWM will not describe.
fn native_window_size(handle: NativeWindowHandle) -> Option<(i32, i32)> {
    let mut bounds = RECT::default();
    // SAFETY: The handle came from the enumeration pass that populated this chooser.
    unsafe { GetWindowRect(HWND(handle.raw()), &mut bounds) }.ok()?;
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    (width > 0 && height > 0).then_some((width, height))
}

/// Drives a build that was created outside `start_window_overview_build`.
///
/// `queue_uncovered_window_tiles` can start one at any time — a toolbar drag is enough — and a
/// queue nothing posts a batch for never renders anything.
fn ensure_overview_batch_posted(hwnd: HWND, state: &OverlayState) {
    if let Some(build) = state.window_overview_build.as_ref() {
        post_overview_batch(hwnd, build.queue.generation);
    }
}

fn refresh_live_window_thumbnails(state: &mut OverlayState) {
    state.live_window_thumbnails.clear();
    if state.presenter != Presenter::Layered
        || state.model.tool != CaptureTool::Window
        || state.overlay_hwnd.0 == 0
    {
        return;
    }

    let toolbar = ToolbarLayout::new(
        state.model.display_environment,
        state.model.toolbar_position,
    );
    let rects = window_overview_rects(state);
    for (window, bounds) in state.window_thumbnails.iter().zip(rects) {
        let thumbnail = match DwmThumbnail::register(state.overlay_hwnd, HWND(window.handle.raw()))
        {
            Ok(thumbnail) => thumbnail,
            Err(error) => {
                log::debug!(
                    "DWM preview registration failed for window handle=0x{:X}; using frozen fallback: {error}",
                    window.handle.raw()
                );
                continue;
            }
        };
        let source_size = match thumbnail.source_size() {
            Ok(size) => size,
            Err(error) => {
                log::debug!(
                    "DWM preview size query failed for window handle=0x{:X}; using frozen fallback: {error}",
                    window.handle.raw()
                );
                continue;
            }
        };
        let destination = fit_source_in_bounds(
            source_size,
            RECT {
                left: bounds.left,
                top: bounds.top,
                right: bounds.right,
                bottom: bounds.bottom,
            },
        );
        if live_thumbnail_overlaps_chrome(
            destination,
            toolbar,
            state.model.options_open,
            state.model.display_environment,
        ) {
            // DWM thumbnails are composed above the destination window's own pixels. Keep this
            // preview in the frozen overview instead so the toolbar and menu remain unobscured.
            continue;
        }
        if let Err(error) = thumbnail.show(destination, u8::MAX) {
            log::debug!(
                "DWM preview update failed for window handle=0x{:X}; using frozen fallback: {error}",
                window.handle.raw()
            );
            continue;
        }
        state.live_window_thumbnails.push(LiveWindowThumbnail {
            handle: window.handle,
            thumbnail,
        });
    }
    queue_uncovered_window_tiles(state);
}

/// Queues a render for any tile that ends up with neither a live preview nor pixels.
///
/// A tile can lose its live preview at any moment and for reasons that have nothing to do with
/// how it was built: the toolbar is dragged over it, a menu opens above it, or DWM stops drawing
/// it. Seeding decided which tiles needed rendering *at the time*; this is what keeps that
/// decision honest afterwards.
fn queue_uncovered_window_tiles(state: &mut OverlayState) {
    let uncovered: Vec<NativeWindowHandle> = state
        .window_thumbnails
        .iter()
        .filter(|tile| tile.surface.is_none())
        .map(|tile| tile.handle)
        .filter(|handle| {
            !state
                .live_window_thumbnails
                .iter()
                .any(|preview| preview.handle == *handle)
        })
        .collect();
    if uncovered.is_empty() {
        return;
    }
    match state.window_overview_build.as_mut() {
        Some(build) => {
            for handle in uncovered {
                build.queue.enqueue(handle);
            }
        }
        None => {
            state.overview_build_generation = state.overview_build_generation.wrapping_add(1);
            state.window_overview_build = Some(WindowOverviewBuild {
                queue: OverviewBuildQueue::new(state.overview_build_generation, uncovered),
                started: Instant::now(),
            });
        }
    }
}

fn live_thumbnail_overlaps_chrome(
    destination: RECT,
    toolbar: ToolbarLayout,
    options_open: bool,
    environment: DisplayEnvironment,
) -> bool {
    rect_overlaps_ui(destination, toolbar.bounds)
        || rect_overlaps_ui(destination, tooltip_band(environment, toolbar))
        || (options_open && rect_overlaps_ui(destination, toolbar.menu))
}

/// The strip a hover tooltip can occupy: directly above the toolbar when there is room
/// (mirroring draw_hover_tooltip's placement rule), otherwise directly below, spanning the
/// work area horizontally. The band is reserved whenever the toolbar is present - not only
/// while a tooltip is showing - so live thumbnails and the dimension label never flicker
/// between states as the hover comes and goes. DWM thumbnails composite above our pixels, so
/// anything in this band must yield or the tooltip would be occluded.
fn tooltip_band(environment: DisplayEnvironment, toolbar: ToolbarLayout) -> UiRect {
    let metrics = environment.metrics;
    let work_area = environment.work_area;
    // draw_hover_tooltip's single-line height: text plus padding, floored at px(32).
    let height = metrics.px(32);
    let gap = metrics.px(8);
    let (top, bottom) = if toolbar.bounds.top >= work_area.top + height + metrics.px(16) {
        (toolbar.bounds.top - height - gap, toolbar.bounds.top)
    } else {
        (toolbar.bounds.bottom, toolbar.bounds.bottom + gap + height)
    };
    UiRect {
        left: work_area.left,
        top,
        right: work_area.right,
        bottom,
    }
}

fn rect_overlaps_ui(rect: RECT, ui: UiRect) -> bool {
    rect.left < ui.right && rect.right > ui.left && rect.top < ui.bottom && rect.bottom > ui.top
}

fn hide_live_window_thumbnails(state: &OverlayState) {
    for preview in &state.live_window_thumbnails {
        if let Err(error) = preview.thumbnail.hide() {
            log::debug!(
                "DWM preview hide failed for window handle=0x{:X}: {error}",
                preview.handle.raw()
            );
        }
    }
}

fn has_live_window_thumbnail(state: &OverlayState, handle: NativeWindowHandle) -> bool {
    state
        .live_window_thumbnails
        .iter()
        .any(|preview| preview.handle == handle)
}

fn rebuild_window_overview_cache(state: &mut OverlayState) {
    let reusable = reusable_overview_surface(
        state
            .window_overview_cache
            .take()
            .map(|cache| cache.surface),
        &mut state.spare_overview_surface,
        state.surface.width,
        state.surface.height,
    );
    let surface = reusable.map_or_else(
        || FrozenSurface::empty(state.surface.width as u32, state.surface.height as u32),
        Ok,
    );
    let Ok(surface) = surface else {
        state.window_overview_cache = None;
        return;
    };
    compose_window_overview_background(&surface, state);
    draw_window_overview_static(&surface, state);
    state.window_overview_cache = Some(WindowOverviewCache {
        surface,
        dim_background: state.model.dim_background,
    });
}

/// Picks the allocation the chooser rebuild will draw into: the current cache's own surface
/// first, else the spare carried over from a previous run. Either is only a buffer - the caller
/// overdraws it completely - and a dimension mismatch consumes the candidate instead of handing
/// out a wrong-sized surface.
fn reusable_overview_surface(
    current: Option<FrozenSurface>,
    spare: &mut Option<FrozenSurface>,
    width: i32,
    height: i32,
) -> Option<FrozenSurface> {
    current
        .or_else(|| spare.take())
        .filter(|surface| surface.width == width && surface.height == height)
}

fn compose_window_overview_background(destination: &FrozenSurface, state: &OverlayState) {
    let source = state.blurred_background.as_ref().unwrap_or(&state.surface);
    // SAFETY: Both DCs own live DIBs. Enlarging the block-averaged desktop creates the soft base.
    unsafe {
        SetStretchBltMode(destination.device, HALFTONE);
        StretchBlt(
            destination.device,
            0,
            0,
            destination.width,
            destination.height,
            source.device,
            0,
            0,
            source.width,
            source.height,
            SRCCOPY,
        );
    }
    if state.model.dim_background {
        let _ = apply_dim_wash(
            destination.device,
            state.dimmer.device,
            destination.width,
            destination.height,
            64,
        );
    }
}

fn window_overview_rects(state: &OverlayState) -> Vec<UiRect> {
    let dimensions: Vec<(i32, i32)> = state
        .window_thumbnails
        .iter()
        .map(|thumbnail| (thumbnail.source_width, thumbnail.source_height))
        .collect();
    layout_floating_windows(
        state.surface.width,
        state.surface.height,
        state.model.display_environment.metrics,
        &dimensions,
    )
}

fn layout_floating_windows(
    client_width: i32,
    client_height: i32,
    metrics: UiMetrics,
    dimensions: &[(i32, i32)],
) -> Vec<UiRect> {
    if dimensions.is_empty() {
        return Vec::new();
    }
    let tokens = metrics.overview_tokens();
    let available_width = (client_width - tokens.margin_x * 2).max(tokens.min_available_width);
    let available_height =
        (client_height - tokens.top - tokens.bottom_reserve).max(tokens.min_available_height);
    let columns = (1..=dimensions.len())
        .max_by_key(|&candidate| {
            minimum_row_height(
                available_width,
                available_height,
                tokens,
                candidate,
                dimensions,
            )
        })
        .unwrap_or(1);
    layout_floating_windows_with_columns(
        tokens,
        available_width,
        available_height,
        columns,
        dimensions,
    )
}

fn minimum_row_height(
    available_width: i32,
    available_height: i32,
    tokens: OverviewLayoutTokens,
    columns: usize,
    dimensions: &[(i32, i32)],
) -> i32 {
    let rows = dimensions.len().div_ceil(columns);
    let row_slot_height = ((available_height - tokens.gap * (rows as i32 - 1)) / rows as i32)
        .max(tokens.min_row_height);
    dimensions
        .chunks(columns)
        .map(|row_dimensions| {
            let aspect_sum: f64 = row_dimensions
                .iter()
                .map(|&(width, height)| f64::from(width) / f64::from(height.max(1)))
                .sum();
            let width_without_gaps =
                available_width - tokens.gap * (row_dimensions.len() as i32 - 1);
            f64::from(row_slot_height)
                .min(f64::from(width_without_gaps.max(1)) / aspect_sum.max(0.01))
                .round() as i32
        })
        .min()
        .unwrap_or(0)
}

fn layout_floating_windows_with_columns(
    tokens: OverviewLayoutTokens,
    available_width: i32,
    available_height: i32,
    columns: usize,
    dimensions: &[(i32, i32)],
) -> Vec<UiRect> {
    let rows = dimensions.len().div_ceil(columns);
    let row_slot_height = ((available_height - tokens.gap * (rows as i32 - 1)) / rows as i32)
        .max(tokens.min_row_height);
    let minimum_window = f64::from(tokens.min_window_size);
    let mut result = Vec::with_capacity(dimensions.len());
    for row in 0..rows {
        let start = row * columns;
        let end = (start + columns).min(dimensions.len());
        let row_dimensions = &dimensions[start..end];
        let aspect_sum: f64 = row_dimensions
            .iter()
            .map(|&(width, height)| f64::from(width) / f64::from(height.max(1)))
            .sum();
        let width_without_gaps = available_width - tokens.gap * (row_dimensions.len() as i32 - 1);
        let height_from_width = f64::from(width_without_gaps.max(1)) / aspect_sum.max(0.01);
        let window_height = f64::from(row_slot_height)
            .min(height_from_width)
            .max(minimum_window);
        let widths: Vec<i32> = row_dimensions
            .iter()
            .map(|&(width, height)| {
                (window_height * f64::from(width) / f64::from(height.max(1)))
                    .round()
                    .max(minimum_window) as i32
            })
            .collect();
        let row_width = widths.iter().sum::<i32>() + tokens.gap * (widths.len() as i32 - 1);
        let mut x = tokens.margin_x + (available_width - row_width) / 2;
        let y = tokens.top
            + row as i32 * (row_slot_height + tokens.gap)
            + (row_slot_height - window_height.round() as i32) / 2;
        for width in widths {
            result.push(UiRect {
                left: x,
                top: y,
                right: x + width,
                bottom: y + window_height.round() as i32,
            });
            x += width + tokens.gap;
        }
    }
    result
}

fn hit_test_window_thumbnail(state: &OverlayState, point: POINT) -> Option<NativeWindowHandle> {
    let rects = window_overview_rects(state);
    state
        .window_thumbnails
        .iter()
        .zip(rects)
        .find_map(|(thumbnail, rect)| rect.contains(point).then_some(thumbnail.handle))
}

fn ready_window_preview(
    state: &OverlayState,
    target: Option<NativeWindowHandle>,
) -> Option<&WindowPreview> {
    let handle = target?;
    match &state.window_preview {
        Some(WindowPreviewState::Ready(preview)) if preview.handle == handle => Some(preview),
        _ => None,
    }
}

fn restore_highlight(state: &OverlayState, rect: Rect) {
    let Some(visible) = rect.intersection(state.model.source) else {
        return;
    };
    let x = visible.x - state.model.source.x;
    let y = visible.y - state.model.source.y;
    let width = visible.width as i32;
    let height = visible.height as i32;
    // SAFETY: The intersection is contained in both same-sized memory DIBs. Copying from the
    // immutable frozen frame removes the dim wash only inside the active selection.
    let _ = unsafe {
        BitBlt(
            state.back_buffer.device,
            x,
            y,
            width,
            height,
            state.surface.device,
            x,
            y,
            SRCCOPY,
        )
    };
}

fn draw_region_dimensions(state: &mut OverlayState, rect: Rect) {
    let device = state.back_buffer.device;
    let source = state.model.source;
    let metrics = state.model.display_environment.metrics;
    let tokens = metrics.region_tokens();
    // Region width and height remain the exact physical-pixel values from the selection rectangle.
    let value = format!("{} × {} px", rect.width, rect.height);
    let measured = measure_ui_text(device, &value, tokens.label_font_height);
    let label_size = UiSize {
        width: measured
            .cx
            .saturating_add(tokens.label_padding_x.saturating_mul(2)),
        height: measured
            .cy
            .max(tokens.label_font_height)
            .saturating_add(tokens.label_padding_y.saturating_mul(2)),
    };
    let selection_left = rect.x.saturating_sub(source.x);
    let selection_top = rect.y.saturating_sub(source.y);
    let selection = UiRect {
        left: selection_left,
        top: selection_top,
        right: selection_left.saturating_add(rect.width as i32),
        bottom: selection_top.saturating_add(rect.height as i32),
    };
    let toolbar = ToolbarLayout::new(
        state.model.display_environment,
        state.model.toolbar_position,
    );
    let mut reserved = vec![toolbar.bounds];
    // The magnifier is drawn after the badge and would simply cover it; reserving it here moves
    // the badge instead, which is the only one of the two that has anywhere else to go. Read
    // rather than computed, because `prepare_loupe` has already settled whether there will be
    // one at all - a magnifier whose sample failed is not an obstacle.
    reserved.extend(loupe_obstacle(state.loupe_layout));
    // The tooltip paints after the label and would occlude it; reserve its whole potential
    // band so the label's placement stays stable across hover changes.
    reserved.push(tooltip_band(state.model.display_environment, toolbar));
    if state.model.options_open {
        reserved.push(toolbar.menu);
    }
    let pointer_exclusion = state.model.pointer_local.map(|pointer| UiRect {
        left: pointer.x.saturating_sub(REGION_CURSOR_CENTER),
        top: pointer.y.saturating_sub(REGION_CURSOR_CENTER),
        right: pointer
            .x
            .saturating_add(REGION_CURSOR_CENTER)
            .saturating_add(1),
        bottom: pointer
            .y
            .saturating_add(REGION_CURSOR_CENTER)
            .saturating_add(1),
    });
    let layout = layout_dimension_label(
        UiRect {
            left: 0,
            top: 0,
            right: state.surface.width,
            bottom: state.surface.height,
        },
        selection,
        label_size,
        &reserved,
        pointer_exclusion,
        state.dimension_label_placement,
        tokens,
    );
    state.dimension_label_placement = Some(layout.placement);
    draw_round_box(
        device,
        layout.bounds,
        rgb(31, 31, 34),
        rgb(104, 104, 110),
        tokens.label_corner_radius,
    );
    draw_text(
        device,
        UiRect {
            left: layout.bounds.left.saturating_add(tokens.label_padding_x),
            top: layout.bounds.top,
            right: layout.bounds.right.saturating_sub(tokens.label_padding_x),
            bottom: layout.bounds.bottom,
        },
        &value,
        rgb(248, 248, 250),
        TextAlignment::Center,
        tokens.label_font_height,
    );
}

/// Paints the magnifier over the pixels [`prepare_loupe`] sampled.
///
/// Takes a shared borrow on purpose: by this point `loupe_layout` is settled, the badge has
/// already been placed against it, and nothing here is allowed to change its mind.
fn draw_loupe(state: &OverlayState) {
    let Some(layout) = state.loupe_layout else {
        return;
    };
    let device = state.back_buffer.device;
    let tokens = layout.tokens;
    draw_round_box(
        device,
        layout.bounds,
        rgb(24, 24, 27),
        rgb(104, 104, 110),
        tokens.corner_radius,
    );
    if let Some(sample) = state.loupe_sample.as_ref() {
        draw_surface_nearest(device, sample, layout.view);
    }
    draw_loupe_grid(device, layout);
    draw_loupe_selection_edges(device, state, layout);
    draw_loupe_centre_cell(device, layout);
    // The desktop coordinate of the pixel under the pointer: the number the user is aiming for,
    // stated plainly rather than left to be counted off the badge.
    if let Some(pointer) = state.model.pointer_local {
        draw_text(
            device,
            layout.label,
            &format!(
                "{}, {}",
                pointer.x.saturating_add(state.model.source.x),
                pointer.y.saturating_add(state.model.source.y)
            ),
            rgb(220, 220, 224),
            TextAlignment::Center,
            tokens.label_font_height,
        );
    }
}

/// A hairline between every magnified pixel, so the blocks read as pixels rather than as a blur.
/// Skipped when the zoom is too small for a grid to be anything but noise.
fn draw_loupe_grid(device: HDC, layout: LoupeLayout) {
    let zoom = layout.tokens.zoom;
    if zoom < 4 {
        return;
    }
    let grid = rgb(64, 64, 70);
    for step in 1..LOUPE_SOURCE_SPAN {
        let x = layout.view.left + step * zoom;
        draw_lines(
            device,
            &[(x, layout.view.top), (x, layout.view.bottom)],
            grid,
            1,
        );
        let y = layout.view.top + step * zoom;
        draw_lines(
            device,
            &[(layout.view.left, y), (layout.view.right, y)],
            grid,
            1,
        );
    }
}

/// The selection's edges, drawn synthetically at the exact pixel boundaries they occupy.
///
/// Not sampled: the live view's sample is the bare desktop with no overlay in it, and the frozen
/// view's is the untouched capture, so the outline being dragged is in neither. Drawing it here
/// is also what makes it exact — a right edge at `right` is the boundary *before* the pixel at
/// `right`, which is the one question the magnifier exists to answer.
fn draw_loupe_selection_edges(device: HDC, state: &OverlayState, layout: LoupeLayout) {
    let Some(rect) = state.model.selection else {
        return;
    };
    if state.model.selection_kind != Some(SelectionKind::Region) {
        return;
    }
    let source = state.model.source;
    let selection = UiRect {
        left: rect.x.saturating_sub(source.x),
        top: rect.y.saturating_sub(source.y),
        right: i32::try_from(rect.right().saturating_sub(i64::from(source.x))).unwrap_or(i32::MAX),
        bottom: i32::try_from(rect.bottom().saturating_sub(i64::from(source.y)))
            .unwrap_or(i32::MAX),
    };
    let accent = rgb(86, 156, 255);
    for line in loupe_edge_lines(selection, layout) {
        let points = if line.vertical {
            [
                (line.position, layout.view.top),
                (line.position, layout.view.bottom),
            ]
        } else {
            [
                (layout.view.left, line.position),
                (layout.view.right, line.position),
            ]
        };
        draw_lines(device, &points, accent, layout.tokens.edge_stroke);
    }
}

/// Outlines the one magnified cell that is the pixel under the pointer.
fn draw_loupe_centre_cell(device: HDC, layout: LoupeLayout) {
    let zoom = layout.tokens.zoom;
    let left = layout.view.left + LOUPE_SOURCE_HALF * zoom;
    let top = layout.view.top + LOUPE_SOURCE_HALF * zoom;
    draw_outline_rect(
        device,
        left,
        top,
        left + zoom,
        top + zoom,
        rgb(255, 255, 255),
        1,
    );
}

/// Copies the 31x31 square of real pixels centred on the pointer into `loupe_sample`.
///
/// Returns false when no pixels could be obtained, which is the only honest reason not to draw.
fn sample_loupe_source(state: &mut OverlayState, layout: LoupeLayout) -> bool {
    if state.loupe_sample.is_none() {
        match FrozenSurface::empty(LOUPE_SOURCE_SPAN as u32, LOUPE_SOURCE_SPAN as u32) {
            Ok(surface) => state.loupe_sample = Some(surface),
            Err(error) => {
                log::warn!("magnifier sample surface could not be allocated: {error}");
                return false;
            }
        }
    }
    let Some(sample) = state.loupe_sample.as_ref() else {
        return false;
    };
    // A square that runs off the edge of the display must not smear the last row across the gap.
    sample.clear();
    let source = state.model.source;
    if state.showing_frozen_pixels() {
        // The composed still image is what the user is looking at, and `surface` holds it
        // untouched by the overlay's own chrome - which is exactly what should be magnified.
        // SAFETY: Both are live memory DIBs owned by this thread. GDI clips a source rectangle
        // that runs past the edge of the frozen surface rather than reading out of bounds.
        let copied = unsafe {
            BitBlt(
                sample.device,
                0,
                0,
                LOUPE_SOURCE_SPAN,
                LOUPE_SOURCE_SPAN,
                state.surface.device,
                layout.source.left,
                layout.source.top,
                SRCCOPY,
            )
        };
        return copied.is_ok();
    }
    // Live: read the desktop itself. `surface` in this mode holds a backdrop captured before the
    // overlay existed, kept only so the window chooser has something to blur; it is stale the
    // moment anything on screen moves, and magnifying it would show pixels that are not there.
    //
    // No CAPTUREBLT: it would pull layered windows into the copy, and this overlay is one. The
    // overlay is also marked WDA_EXCLUDEFROMCAPTURE, so neither route should put it in the
    // sample; the synthetic selection edges mean correctness does not depend on that holding.
    // SAFETY: A null HWND obtains the desktop DC, released immediately after the copy.
    let screen = unsafe { GetDC(None) };
    if screen.0 == 0 {
        log::debug!("magnifier could not obtain a desktop device context");
        return false;
    }
    // SAFETY: The sample DIB is exactly LOUPE_SOURCE_SPAN square and both DCs stay live for this
    // synchronous copy. Physical desktop coordinates may legitimately be negative.
    let copied = unsafe {
        BitBlt(
            sample.device,
            0,
            0,
            LOUPE_SOURCE_SPAN,
            LOUPE_SOURCE_SPAN,
            screen,
            source.x.saturating_add(layout.source.left),
            source.y.saturating_add(layout.source.top),
            SRCCOPY,
        )
    };
    // SAFETY: Balances the successful GetDC(None) above on this thread.
    unsafe { ReleaseDC(None, screen) };
    copied.is_ok()
}

/// What the dimension badge must avoid on account of the magnifier: its box, or nothing at all.
///
/// A named function so the "or nothing at all" half is testable. The badge is placed once per
/// paint and cannot be moved afterwards, so reserving space for a magnifier that turns out not to
/// be drawn pushes the badge aside for nothing - which is why this reads the settled layout
/// rather than recomputing whether one was wanted.
fn loupe_obstacle(layout: Option<LoupeLayout>) -> Option<UiRect> {
    layout.map(|layout| layout.bounds)
}

fn draw_window_overview_static(destination: &FrozenSurface, state: &OverlayState) {
    if state.window_thumbnails.is_empty() {
        // While a build is in flight the empty inventory is a transient, not a verdict: show
        // only the chooser background until the first tile lands.
        if state.window_overview_build.is_some() {
            return;
        }
        let tokens = state.model.display_environment.metrics.overview_tokens();
        draw_text(
            destination.device,
            UiRect {
                left: tokens.margin_x,
                top: tokens.top,
                right: state.surface.width - tokens.margin_x,
                bottom: tokens.empty_state_bottom,
            },
            "No capturable application windows",
            rgb(220, 220, 224),
            TextAlignment::Center,
            state.model.display_environment.metrics.px(UI_FONT_HEIGHT),
        );
        return;
    }
    let rects = window_overview_rects(state);
    for (thumbnail, rect) in state.window_thumbnails.iter().zip(rects) {
        if has_live_window_thumbnail(state, thumbnail.handle) {
            continue;
        }
        let selected = state.model.selected_window == Some(thumbnail.handle);
        let preview = selected
            .then(|| ready_window_preview(state, Some(thumbnail.handle)))
            .flatten();
        let surface = match preview {
            Some(preview) => &preview.surface,
            // A tile with neither a live preview nor a render is one whose frozen render is
            // still queued; the chooser background stands in until it lands.
            None => match thumbnail.surface.as_ref() {
                Some(surface) => surface,
                None => continue,
            },
        };
        draw_window_surface(destination, surface, rect);
    }
}

fn draw_window_overview_interactive(state: &OverlayState) {
    let rects = window_overview_rects(state);
    for (thumbnail, rect) in state.window_thumbnails.iter().zip(rects) {
        let selected = state.model.selected_window == Some(thumbnail.handle);
        let hovered =
            state.model.hovered.map(|candidate| candidate.handle) == Some(thumbnail.handle);
        if selected || hovered {
            let color = if selected {
                rgb(45, 125, 246)
            } else {
                rgb(52, 197, 218)
            };
            let preview = selected
                .then(|| ready_window_preview(state, Some(thumbnail.handle)))
                .flatten();
            let (source_width, source_height, corner_radius_px) = preview.map_or(
                (
                    thumbnail.source_width,
                    thumbnail.source_height,
                    thumbnail.corner_radius_px,
                ),
                |preview| {
                    (
                        preview.surface.width,
                        preview.surface.height,
                        preview.corner_radius_px,
                    )
                },
            );
            let destination = fitted_surface_rect(source_width, source_height, rect, true);
            let scaled_radius =
                scaled_corner_radius(source_width, source_height, destination, corner_radius_px);
            const OUTLINE_WIDTH: i32 = 3;
            draw_antialiased_rounded_outline(
                &state.back_buffer,
                UiRect {
                    left: destination.left - OUTLINE_WIDTH,
                    top: destination.top - OUTLINE_WIDTH,
                    right: destination.right + OUTLINE_WIDTH,
                    bottom: destination.bottom + OUTLINE_WIDTH,
                },
                color,
                OUTLINE_WIDTH,
                scaled_radius + OUTLINE_WIDTH as f32,
            );
        }
    }
}

fn draw_toolbar(state: &OverlayState) {
    let device = state.back_buffer.device;
    let metrics = state.model.display_environment.metrics;
    let tokens = metrics.toolbar_tokens();
    let layout = ToolbarLayout::new(
        state.model.display_environment,
        state.model.toolbar_position,
    );
    // First, so the Options menu paints over it when both want the space above the toolbar: an
    // open menu is what the user is looking at, and the tag is still readable the moment it
    // closes.
    draw_view_tag(state, layout);
    if state.model.options_open {
        draw_options_menu(device, state, layout);
    }
    draw_round_box(
        device,
        layout.bounds,
        rgb(38, 38, 41),
        rgb(92, 92, 98),
        tokens.toolbar_corner_radius,
    );
    let grip_center_x = (layout.drag_handle.left + layout.drag_handle.right) / 2;
    let grip_center_y = (layout.drag_handle.top + layout.drag_handle.bottom) / 2;
    for offset in [-tokens.grip_dot_gap, 0, tokens.grip_dot_gap] {
        let left = grip_center_x - tokens.grip_dot_size / 2;
        let top = grip_center_y + offset - tokens.grip_dot_size / 2;
        draw_filled_ellipse(
            device,
            left,
            top,
            left + tokens.grip_dot_size,
            top + tokens.grip_dot_size,
            rgb(142, 142, 148),
        );
    }
    draw_tool_button(
        device,
        layout.full_display,
        CaptureTool::FullDisplay,
        state,
        metrics,
    );
    draw_tool_button(device, layout.window, CaptureTool::Window, state, metrics);
    draw_tool_button(device, layout.region, CaptureTool::Region, state, metrics);
    draw_lines(
        device,
        &[
            (
                (layout.region.right + layout.options.left) / 2,
                layout.options.top + metrics.px(8),
            ),
            (
                (layout.region.right + layout.options.left) / 2,
                layout.options.bottom - metrics.px(8),
            ),
        ],
        rgb(92, 92, 98),
        1,
    );

    let options_hovered = state.model.hovered_control == Some(ToolbarControl::Options);
    if state.model.options_open || options_hovered {
        draw_round_box(
            device,
            layout.options,
            rgb(65, 65, 70),
            rgb(65, 65, 70),
            tokens.control_corner_radius,
        );
    }
    draw_text(
        device,
        UiRect {
            left: layout.options.left + tokens.text_padding,
            top: layout.options.top,
            right: layout.options.right - tokens.icon_size,
            bottom: layout.options.bottom,
        },
        "Options",
        rgb(245, 245, 247),
        TextAlignment::Center,
        tokens.font_height,
    );
    let chevron_y = (layout.options.top + layout.options.bottom) / 2;
    draw_lines(
        device,
        &[
            (
                layout.options.right - metrics.px(17),
                chevron_y - metrics.px(3),
            ),
            (
                layout.options.right - metrics.px(13),
                chevron_y + metrics.px(2),
            ),
            (
                layout.options.right - metrics.px(9),
                chevron_y - metrics.px(3),
            ),
        ],
        rgb(220, 220, 224),
        tokens.icon_stroke,
    );

    let capture_enabled = state.model.selection.is_some();
    let capture_color = if capture_enabled {
        if state.model.hovered_control == Some(ToolbarControl::Capture) {
            rgb(68, 145, 255)
        } else {
            rgb(45, 125, 246)
        }
    } else {
        rgb(82, 82, 87)
    };
    draw_round_box(
        device,
        layout.capture,
        capture_color,
        capture_color,
        tokens.control_corner_radius,
    );
    let capture_foreground = if capture_enabled {
        rgb(255, 255, 255)
    } else {
        rgb(180, 180, 185)
    };
    draw_camera_icon(
        device,
        layout.capture.left + metrics.px(10),
        (layout.capture.top + layout.capture.bottom - tokens.icon_size) / 2,
        capture_foreground,
        metrics,
    );
    draw_text(
        device,
        UiRect {
            left: layout.capture.left + tokens.icon_size + metrics.px(18),
            top: layout.capture.top,
            right: layout.capture.right - tokens.text_padding,
            bottom: layout.capture.bottom,
        },
        "Capture",
        capture_foreground,
        TextAlignment::Center,
        tokens.font_height,
    );
    draw_hover_tooltip(device, state, layout);
}

fn draw_tool_button(
    device: HDC,
    bounds: UiRect,
    tool: CaptureTool,
    state: &OverlayState,
    metrics: UiMetrics,
) {
    let tokens = metrics.toolbar_tokens();
    let control = match tool {
        CaptureTool::FullDisplay => ToolbarControl::FullDisplay,
        CaptureTool::Window => ToolbarControl::Window,
        CaptureTool::Region => ToolbarControl::Region,
    };
    if state.model.tool == tool || state.model.hovered_control == Some(control) {
        let color = if state.model.tool == tool {
            rgb(76, 76, 82)
        } else {
            rgb(58, 58, 63)
        };
        draw_round_box(device, bounds, color, color, tokens.control_corner_radius);
    }
    let color = rgb(245, 245, 247);
    match tool {
        CaptureTool::FullDisplay => draw_display_icon(device, bounds, color, metrics),
        CaptureTool::Window => draw_window_icon(device, bounds, color, metrics),
        CaptureTool::Region => draw_region_icon(device, bounds, color, metrics),
    }
}

fn draw_hover_tooltip(device: HDC, state: &OverlayState, layout: ToolbarLayout) {
    if state.model.options_open {
        return;
    }
    let (target, value) = match state.model.hovered_control {
        Some(ToolbarControl::FullDisplay) => {
            (layout.full_display, "Capture full display".to_owned())
        }
        Some(ToolbarControl::Window) => (layout.window, "Capture a window".to_owned()),
        Some(ToolbarControl::Region) => (layout.region, "Select a region".to_owned()),
        Some(ToolbarControl::Options) => (layout.options, "Capture options".to_owned()),
        Some(ToolbarControl::Capture) => {
            let value = if state.model.selection.is_some() {
                "Copy selection to clipboard"
            } else {
                "Select something to capture"
            };
            (layout.capture, value.to_owned())
        }
        _ => return,
    };
    let metrics = state.model.display_environment.metrics;
    let tokens = metrics.toolbar_tokens();
    let font_height = tokens.font_height;
    let measured = measure_ui_text(device, &value, font_height);
    let width = measured
        .cx
        .saturating_add(tokens.tooltip_padding_x.saturating_mul(2))
        .max(metrics.px(120))
        .min((state.model.display_environment.work_area.width() - metrics.px(16)).max(1));
    let height = (measured.cy + tokens.tooltip_padding_y * 2).max(metrics.px(32));
    let left = ((target.left + target.right - width) / 2).clamp(
        state.model.display_environment.work_area.left + metrics.px(8),
        (state.model.display_environment.work_area.right - width - metrics.px(8))
            .max(state.model.display_environment.work_area.left + metrics.px(8)),
    );
    let preferred_top = if layout.bounds.top
        >= state.model.display_environment.work_area.top + height + metrics.px(16)
    {
        layout.bounds.top - height - metrics.px(8)
    } else {
        layout.bounds.bottom + metrics.px(8)
    };
    let top = preferred_top.clamp(
        state.model.display_environment.work_area.top + metrics.px(8),
        (state.model.display_environment.work_area.bottom - height - metrics.px(8))
            .max(state.model.display_environment.work_area.top + metrics.px(8)),
    );
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
        UiRect {
            left: bounds.left + tokens.tooltip_padding_x,
            top: bounds.top,
            right: bounds.right - tokens.tooltip_padding_x,
            bottom: bounds.bottom,
        },
        &value,
        rgb(248, 248, 250),
        TextAlignment::Center,
        font_height,
    );
}

/// The label on the tag that shows while the frozen view is on screen.
///
/// Named once so the glyph-fit test measures the string the overlay paints, and worded to answer
/// the question the user will actually be asking - not "which mode is this" but "why has the
/// video stopped".
const FROZEN_VIEW_TAG: &str = "FROZEN · pixels from hotkey press";

/// Draws the tag that says the overlay is showing the snapshot rather than the desktop.
///
/// Only while the frozen view is up, only outside the Window tool where the view means nothing,
/// and only while the pointer is off the toolbar - the tooltips claim the same band and, being
/// painted later, would sit on top of this. The live view carries no tag at all: an overlay
/// showing what is really there is the unremarkable case, and labelling it would put a permanent
/// pill on every capture.
fn draw_view_tag(state: &OverlayState, layout: ToolbarLayout) {
    if !machine::view_tag_visible(&state.model) {
        return;
    }
    let device = state.back_buffer.device;
    let metrics = state.model.display_environment.metrics;
    let work_area = state.model.display_environment.work_area;
    let tokens = metrics.toolbar_tokens();
    let font_height = tokens.font_height;
    let measured = measure_ui_text(device, FROZEN_VIEW_TAG, font_height);
    let width = measured
        .cx
        .saturating_add(tokens.tooltip_padding_x.saturating_mul(2))
        .min((work_area.width() - metrics.px(16)).max(1));
    let height = (measured.cy + tokens.tooltip_padding_y * 2).max(metrics.px(32));
    let left = ((layout.bounds.left + layout.bounds.right - width) / 2).clamp(
        work_area.left + metrics.px(8),
        (work_area.right - width - metrics.px(8)).max(work_area.left + metrics.px(8)),
    );
    // Above the toolbar, which is where the eye already is, and below it only when there is no
    // room above - the same rule the tooltips follow, so the two never argue about which side of
    // the toolbar its chrome belongs on.
    let preferred_top = if layout.bounds.top >= work_area.top + height + metrics.px(16) {
        layout.bounds.top - height - metrics.px(8)
    } else {
        layout.bounds.bottom + metrics.px(8)
    };
    let top = preferred_top.clamp(
        work_area.top + metrics.px(8),
        (work_area.bottom - height - metrics.px(8)).max(work_area.top + metrics.px(8)),
    );
    let bounds = UiRect {
        left,
        top,
        right: left + width,
        bottom: top + height,
    };
    // The accent border rather than the toolbar's grey: this is a statement about the pixels, not
    // another control, and it has to be noticed without being alarming.
    draw_round_box(
        device,
        bounds,
        rgb(28, 42, 66),
        rgb(86, 156, 255),
        tokens.tooltip_corner_radius,
    );
    draw_text(
        device,
        UiRect {
            left: bounds.left + tokens.tooltip_padding_x,
            top: bounds.top,
            right: bounds.right - tokens.tooltip_padding_x,
            bottom: bounds.bottom,
        },
        FROZEN_VIEW_TAG,
        rgb(198, 220, 255),
        TextAlignment::Center,
        font_height,
    );
}

fn draw_options_menu(device: HDC, state: &OverlayState, layout: ToolbarLayout) {
    let metrics = state.model.display_environment.metrics;
    let tokens = metrics.toolbar_tokens();
    draw_round_box(
        device,
        layout.menu,
        rgb(43, 43, 47),
        rgb(101, 101, 107),
        tokens.menu_corner_radius,
    );
    let rows = [
        (ToolbarControl::DimBackground, layout.dim_background),
        (ToolbarControl::SnapToWindows, layout.snap_to_windows),
        (ToolbarControl::RegionZoom, layout.region_zoom),
        (ToolbarControl::PreviewView, layout.preview_view),
        (
            ToolbarControl::ClipboardDestination,
            layout.clipboard_destination,
        ),
        (ToolbarControl::Cancel, layout.cancel),
    ];
    for (control, row) in rows {
        if state.model.hovered_control == Some(control) {
            draw_round_box(
                device,
                row,
                rgb(64, 64, 69),
                rgb(64, 64, 69),
                tokens.control_corner_radius,
            );
        }
    }
    if state.model.dim_background {
        draw_checkmark(
            device,
            layout.dim_background.left + tokens.menu_check_offset,
            (layout.dim_background.top + layout.dim_background.bottom) / 2,
            rgb(86, 156, 255),
            metrics,
        );
    }
    draw_text(
        device,
        UiRect {
            left: layout.dim_background.left + tokens.menu_text_offset,
            top: layout.dim_background.top,
            right: layout.dim_background.right - tokens.text_padding,
            bottom: layout.dim_background.bottom,
        },
        "Dim Background",
        rgb(245, 245, 247),
        TextAlignment::Left,
        tokens.font_height,
    );
    if state.model.snap_to_windows {
        draw_checkmark(
            device,
            layout.snap_to_windows.left + tokens.menu_check_offset,
            (layout.snap_to_windows.top + layout.snap_to_windows.bottom) / 2,
            rgb(86, 156, 255),
            metrics,
        );
    }
    draw_text(
        device,
        UiRect {
            left: layout.snap_to_windows.left + tokens.menu_text_offset,
            top: layout.snap_to_windows.top,
            right: layout.snap_to_windows.right - tokens.text_padding,
            bottom: layout.snap_to_windows.bottom,
        },
        SNAP_TO_WINDOWS_LABEL,
        rgb(245, 245, 247),
        TextAlignment::Left,
        tokens.font_height,
    );
    // No checkmark: this row cycles through three states rather than being on or off, so its
    // label states the current one and a tick beside it would only be ambiguous.
    draw_text(
        device,
        UiRect {
            left: layout.region_zoom.left + tokens.menu_text_offset,
            top: layout.region_zoom.top,
            right: layout.region_zoom.right - tokens.text_padding,
            bottom: layout.region_zoom.bottom,
        },
        state.model.loupe.mode.label(),
        rgb(245, 245, 247),
        TextAlignment::Left,
        tokens.font_height,
    );
    // No checkmark here either: two views cycle rather than one setting being on or off, and the
    // label states which one is showing. Greyed - the same grey the clipboard row uses for a
    // destination the user cannot turn off - when the run has nothing to switch to, or under the
    // Window tool, where a click renders its window fresh whichever view is behind it.
    draw_text(
        device,
        UiRect {
            left: layout.preview_view.left + tokens.menu_text_offset,
            top: layout.preview_view.top,
            right: layout.preview_view.right - tokens.text_padding,
            bottom: layout.preview_view.bottom,
        },
        state.model.view.menu_label(),
        if machine::view_toggle_enabled(&state.model) {
            rgb(245, 245, 247)
        } else {
            rgb(130, 130, 136)
        },
        TextAlignment::Left,
        tokens.font_height,
    );
    draw_checkmark(
        device,
        layout.clipboard_destination.left + tokens.menu_check_offset,
        (layout.clipboard_destination.top + layout.clipboard_destination.bottom) / 2,
        rgb(86, 156, 255),
        metrics,
    );
    draw_text(
        device,
        UiRect {
            left: layout.clipboard_destination.left + tokens.menu_text_offset,
            top: layout.clipboard_destination.top,
            right: layout.clipboard_destination.right - tokens.text_padding,
            bottom: layout.clipboard_destination.bottom,
        },
        "Copy to Clipboard",
        rgb(210, 210, 214),
        TextAlignment::Left,
        tokens.font_height,
    );
    draw_lines(
        device,
        &[
            (layout.menu.left + tokens.text_padding, layout.cancel.top),
            (layout.menu.right - tokens.text_padding, layout.cancel.top),
        ],
        rgb(76, 76, 81),
        1,
    );
    draw_text(
        device,
        UiRect {
            left: layout.cancel.left + tokens.menu_text_offset,
            top: layout.cancel.top,
            right: layout.cancel.right - tokens.text_padding,
            bottom: layout.cancel.bottom,
        },
        "Cancel Capture",
        rgb(255, 105, 97),
        TextAlignment::Left,
        tokens.font_height,
    );
}

/// The part of a native window rectangle that lies on the captured display.
///
/// A thin adapter over [`Rect::intersection`], which is the single implementation of half-open
/// rectangle overlap in the workspace and is property-tested against `Rect::contains` there. The
/// two hand-rolled copies that used to live here agreed with it by inspection only.
fn intersect_with_source(native: RECT, source: Rect) -> Option<Rect> {
    window_enumeration::rect_from_native(native)?.intersection(source)
}

fn update_cursor(handle: Option<ResizeHandle>, region_cursor: &RegionCursor) {
    if handle.is_none() {
        region_cursor.activate();
        return;
    }
    let resource = match handle {
        Some(ResizeHandle::NorthWest | ResizeHandle::SouthEast) => IDC_SIZENWSE,
        Some(ResizeHandle::NorthEast | ResizeHandle::SouthWest) => IDC_SIZENESW,
        Some(ResizeHandle::North | ResizeHandle::South) => IDC_SIZENS,
        Some(ResizeHandle::East | ResizeHandle::West) => IDC_SIZEWE,
        None => unreachable!("the native region cursor is handled above"),
    };
    // SAFETY: All resources are predefined system cursors and require no explicit destruction.
    if let Ok(cursor) = unsafe { LoadCursorW(None, resource) } {
        // SAFETY: cursor is a live shared system cursor.
        unsafe { SetCursor(cursor) };
    }
}

fn tight_pixels(frame: &CpuFrame) -> Result<Arc<[u8]>, CaptureError> {
    let tight_stride = frame
        .width()
        .checked_mul(4)
        .ok_or_else(|| invalid_frame("overlay row size overflowed"))?
        as usize;
    if frame.stride_bytes() as usize == tight_stride {
        return Ok(frame.pixels_shared().clone());
    }
    let length = tight_stride
        .checked_mul(frame.height() as usize)
        .ok_or_else(|| invalid_frame("overlay image size overflowed"))?;
    let mut pixels = vec![0_u8; length];
    for row in 0..frame.height() as usize {
        let source_start = row * frame.stride_bytes() as usize;
        let destination_start = row * tight_stride;
        pixels[destination_start..destination_start + tight_stride]
            .copy_from_slice(&frame.pixels()[source_start..source_start + tight_stride]);
    }
    Ok(Arc::from(pixels))
}

fn validate_frame(frame: &CpuFrame) -> Result<(), CaptureError> {
    let is_bgra8 = matches!(
        frame.format().encoding(),
        PixelEncoding::EightBitRgba { blue_first: true }
    );
    if !is_bgra8 || frame.origin != FrameOrigin::TopLeft {
        return Err(CaptureError {
            kind: CaptureErrorKind::Unsupported,
            backend: "windows-overlay",
            operation: "validate_frame",
            message: "selection overlay requires top-left BGRA8 pixels".to_owned(),
            retryable: false,
            native_code: None,
        });
    }
    if frame.width() != frame.metadata.source_rect.width
        || frame.height() != frame.metadata.source_rect.height
    {
        return Err(invalid_frame(
            "overlay frame dimensions do not match source bounds",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use captastic_core::PixelFormat;

    use super::*;

    fn selection_for_tests() -> OverlaySelection {
        OverlaySelection {
            rect: Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            kind: SelectionKind::Region,
            window: None,
            window_title: None,
            window_application: None,
            selection_ns: 0,
            preparation_ns: 0,
            window_overview_ns: None,
            window_preview_count: 0,
            window_live_preview_count: 0,
            window_frozen_preview_count: 0,
            window_preview_bytes: 0,
            view: PreviewView::Frozen,
            view_switched: false,
            presenter_fallback_reason: None,
            window_frame: None,
        }
    }

    #[test]
    fn switching_the_view_and_switching_back_still_counts_as_switched() {
        // The whole reason this is latched in the shell rather than compared at the end. A run
        // that ends in the view it opened in is otherwise indistinguishable from one the user
        // never touched, and `view_switched` exists precisely to tell those two apart.
        let mut switched = false;
        switched = latch_view_switch(switched, PreviewView::Live, PreviewView::Live);
        assert!(
            !switched,
            "an input that leaves the view alone changes nothing"
        );

        switched = latch_view_switch(switched, PreviewView::Live, PreviewView::Frozen);
        assert!(switched);

        switched = latch_view_switch(switched, PreviewView::Frozen, PreviewView::Live);
        assert!(
            switched,
            "switching back does not unsay that it was switched"
        );

        // And every later input keeps it set, whatever it does.
        for (before, after) in [
            (PreviewView::Live, PreviewView::Live),
            (PreviewView::Frozen, PreviewView::Frozen),
        ] {
            assert!(latch_view_switch(switched, before, after));
        }
    }

    #[test]
    fn a_preview_source_without_a_snapshot_can_only_open_live() {
        let mut metadata = frame_metadata_for_tests();
        metadata.source_rect = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let frame = CpuFrame::new(
            Arc::from(vec![0_u8; 4]),
            1,
            1,
            4,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("a valid one-pixel frame");

        // A snapshot opens in whichever view it was asked for, and does not insist on the
        // layered presenter: an opaque window shows exactly those pixels.
        assert_eq!(
            SelectionPreviewSource::snapshot(&frame, PreviewView::Live, false)
                .resolved_initial_view(),
            PreviewView::Live
        );
        assert_eq!(
            SelectionPreviewSource::frozen(&frame).resolved_initial_view(),
            PreviewView::Frozen
        );
        assert!(!SelectionPreviewSource::frozen(&frame).require_layered);
        // Without a snapshot there is nothing to paint a frozen view from, and the layered
        // presenter is the only way to show anything at all.
        let live = SelectionPreviewSource::live(&frame.metadata);
        assert_eq!(live.resolved_initial_view(), PreviewView::Live);
        assert!(live.require_layered);
    }

    #[test]
    fn a_display_change_under_the_overlay_is_not_reported_as_a_cancellation() {
        // What the first hotkey press after a dock switch did: WM_DISPLAYCHANGE closed the
        // overlay, the run produced no selection, and every caller downstream read that as the
        // user pressing Escape - info-level "selection cancelled", no balloon, nothing to
        // distinguish a press that was taken away from one that was declined.
        assert!(matches!(
            SelectionOutcome::from_run(None, Some(DISPLAY_CHANGE_MONITORS)),
            SelectionOutcome::DisplayConfigurationChanged(DISPLAY_CHANGE_MONITORS)
        ));
        assert!(matches!(
            SelectionOutcome::from_run(None, None),
            SelectionOutcome::Cancelled
        ));
    }

    #[test]
    fn the_reason_an_overlay_abandoned_its_geometry_reaches_the_caller() {
        // A DPI change and a work-area change close the overlay exactly as a monitor change does,
        // and they are not a monitor change. The caller words the user's explanation from this,
        // so the reason has to survive the trip rather than being flattened into one outcome.
        for reason in ["overlay_dpi_changed", "overlay_display_setting_changed"] {
            assert!(matches!(
                SelectionOutcome::from_run(None, Some(reason)),
                SelectionOutcome::DisplayConfigurationChanged(carried) if carried == reason
            ));
        }
    }

    #[test]
    fn a_selection_in_hand_outranks_a_display_change() {
        // Defensive, not observed: confirmation and a display change each destroy the window
        // synchronously, so the window procedure cannot produce both. This pins what the mapping
        // does if that ever stops being true - a selection the user made is not thrown away -
        // rather than claiming the sequence happens.
        assert!(matches!(
            SelectionOutcome::from_run(Some(selection_for_tests()), Some(DISPLAY_CHANGE_MONITORS)),
            SelectionOutcome::Selected(_)
        ));
    }

    /// The overlay presenter blits the frozen frame into a 32-bit top-down DIB and hit-tests
    /// against it at four bytes per pixel. A half-float frame reaching that code would be drawn as
    /// a quarter of the desktop stretched over the whole screen, which is a far worse outcome than
    /// declining to open the overlay - and the fallback for declining already exists.
    #[test]
    fn the_selection_overlay_refuses_pixels_it_cannot_present() {
        let mut metadata = frame_metadata_for_tests();
        metadata.source_rect = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        let frame = CpuFrame::new(
            Arc::from(vec![0_u8; 8]),
            1,
            1,
            8,
            PixelFormat::Rgba16Float,
            FrameOrigin::TopLeft,
            ColorSpace::ScRgb,
            metadata,
        )
        .expect("a valid half-float frame");

        let error = validate_frame(&frame).expect_err("a half-float frame cannot be presented");
        assert_eq!(error.kind, CaptureErrorKind::Unsupported);
    }

    #[test]
    fn draining_a_pending_quit_clears_the_thread_quit_flag() {
        // The quit flag is per-thread state, so exercise it on a thread of its own.
        thread::spawn(|| {
            // SAFETY: Sets the quit flag on this test thread's own message queue.
            unsafe { PostQuitMessage(0) };
            let mut message = MSG::default();
            // SAFETY: message is writable storage and the peek leaves the queue untouched.
            let latched =
                unsafe { PeekMessageW(&mut message, None, WM_QUIT, WM_QUIT, PM_NOREMOVE) };
            assert!(latched.as_bool(), "expected a latched quit to drain");

            drain_pending_quit();

            // SAFETY: message is writable storage and the peek leaves the queue untouched.
            let remaining =
                unsafe { PeekMessageW(&mut message, None, WM_QUIT, WM_QUIT, PM_NOREMOVE) };
            assert!(!remaining.as_bool(), "quit survived the drain");
        })
        .join()
        .expect("quit drain thread panicked");
    }

    #[test]
    fn self_initiated_capture_change_preserves_drag_completion_state() {
        let mut releasing_pointer_capture = true;
        assert!(consume_self_initiated_capture_change(
            &mut releasing_pointer_capture
        ));
        assert!(!releasing_pointer_capture);

        assert!(!consume_self_initiated_capture_change(
            &mut releasing_pointer_capture
        ));
    }

    #[test]
    fn controller_updates_live_ui_before_persistence_receives_the_event() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let controller = OverlayController::with_ui_updates(sender);
        let fallback = captastic_config::DisplayUiState {
            overlay_position: Some((10, 20)),
            ..captastic_config::DisplayUiState::default()
        };
        assert_eq!(controller.remembered_ui("display-1", fallback), fallback);

        controller.submit_ui_update(OverlayUiUpdate::ToolbarCenter {
            display_id: "display-1".to_owned(),
            center_x: 0.25,
            center_y: 0.75,
        });

        let remembered = controller.remembered_ui("display-1", Default::default());
        assert_eq!(remembered.overlay_center, Some((0.25, 0.75)));
        assert_eq!(remembered.overlay_position, Some((10, 20)));
        assert!(matches!(
            receiver.try_recv(),
            Ok(OverlayUiUpdate::ToolbarCenter { .. })
        ));
    }

    #[test]
    fn the_snap_preference_is_live_on_a_display_this_process_has_never_drawn_on() {
        // The reason the global is not kept in the per-display map: a toggle made on the laptop
        // screen has to apply on the external monitor too, before the persistence worker has
        // written anything and without that monitor ever having had an entry.
        let (sender, receiver) = std::sync::mpsc::channel();
        let controller = OverlayController::with_ui_updates(sender);
        assert_eq!(
            controller
                .remembered_ui("display-1", Default::default())
                .snap_to_windows,
            None
        );

        controller.submit_ui_update(OverlayUiUpdate::SnapToWindows { enabled: false });

        for display in ["display-1", "a-monitor-never-seen-before"] {
            assert_eq!(
                controller
                    .remembered_ui(display, Default::default())
                    .snap_to_windows,
                Some(false),
                "{display}"
            );
        }
        // The live answer overrides a stale fallback read off disk before the toggle happened.
        let stale = captastic_config::DisplayUiState {
            snap_to_windows: Some(true),
            ..Default::default()
        };
        assert_eq!(
            controller.remembered_ui("display-2", stale).snap_to_windows,
            Some(false)
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(OverlayUiUpdate::SnapToWindows { enabled: false })
        ));

        // The zoom mode is the same kind of answer and takes the same route.
        controller.submit_ui_update(OverlayUiUpdate::RegionZoom {
            mode: captastic_config::RegionZoom::Key,
        });
        assert_eq!(
            controller
                .remembered_ui("another-monitor-never-seen-before", Default::default())
                .region_zoom,
            Some(captastic_config::RegionZoom::Key)
        );
        assert!(matches!(
            receiver.try_recv(),
            Ok(OverlayUiUpdate::RegionZoom {
                mode: captastic_config::RegionZoom::Key
            })
        ));
    }

    #[test]
    fn region_from_points_is_normalized_and_clamped() {
        assert_eq!(
            rect_from_points(
                Rect {
                    x: 100,
                    y: 200,
                    width: 300,
                    height: 200,
                },
                POINT { x: 350, y: 350 },
                POINT { x: 50, y: 150 },
            ),
            Some(Rect {
                x: 100,
                y: 200,
                width: 250,
                height: 150,
            })
        );
    }

    #[test]
    fn window_intersection_returns_only_visible_portion() {
        assert_eq!(
            intersect_with_source(
                RECT {
                    left: -10,
                    top: 20,
                    right: 100,
                    bottom: 80,
                },
                Rect {
                    x: 0,
                    y: 0,
                    width: 50,
                    height: 50,
                },
            ),
            Some(Rect {
                x: 0,
                y: 20,
                width: 50,
                height: 30,
            })
        );
    }

    #[test]
    fn the_snap_inventory_is_clipped_and_ordered_windows_work_area_display() {
        let source = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let on_display = Rect {
            x: -1800,
            y: 100,
            width: 400,
            height: 300,
        };
        let straddling = Rect {
            x: -200,
            y: 100,
            width: 400,
            height: 300,
        };
        let elsewhere = Rect {
            x: 200,
            y: 100,
            width: 400,
            height: 300,
        };
        let work_area = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1040,
        };
        let targets =
            snap_targets_from([on_display, straddling, elsewhere], Some(work_area), source);
        let listed: Vec<_> = targets
            .iter()
            .map(|target| (target.kind, target.rect))
            .collect();
        assert_eq!(
            listed,
            vec![
                // Enumeration order is z-order, and the inventory preserves it.
                (SnapTargetKind::Window, on_display),
                // Only the part on this display: an edge off the side of it is not an edge a
                // region can be put on.
                (
                    SnapTargetKind::Window,
                    Rect {
                        x: -200,
                        y: 100,
                        width: 200,
                        height: 300,
                    }
                ),
                // `elsewhere` is on the other monitor entirely and is dropped.
                (SnapTargetKind::WorkArea, work_area),
                (SnapTargetKind::Display, source),
            ]
        );

        // A work area identical to the display adds nothing but a duplicate guide.
        let without = snap_targets_from([], Some(source), source);
        assert_eq!(
            without.iter().map(|target| target.kind).collect::<Vec<_>>(),
            vec![SnapTargetKind::Display]
        );
        // The display is always present, so a run with no windows still snaps to the screen edge.
        assert_eq!(snap_targets_from([], None, source).len(), 1);
    }

    #[test]
    fn point_containment_uses_exclusive_bottom_right_edge() {
        let rect = Rect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        };
        assert!(contains(rect, POINT { x: 10, y: 20 }));
        assert!(!contains(rect, POINT { x: 40, y: 60 }));
    }

    #[test]
    fn highlight_is_clipped_to_the_captured_display() {
        assert_eq!(
            Rect {
                x: -20,
                y: 40,
                width: 80,
                height: 100,
            }
            .intersection(Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }),
            Some(Rect {
                x: 0,
                y: 40,
                width: 60,
                height: 60,
            })
        );
    }

    #[test]
    fn dim_wash_preserves_the_restored_highlight() {
        let original = FrozenSurface::new(
            2,
            1,
            &[
                0, 0, 200, 255, // red selected pixel
                0, 200, 0, 255, // green surrounding pixel
            ],
        )
        .expect("original test surface");
        let destination = FrozenSurface::empty(2, 1).expect("destination test surface");
        let dimmer = FrozenSurface::new(1, 1, &[0, 0, 0, 255]).expect("dimmer test surface");
        // SAFETY: All test surfaces are live, compatible memory DCs with selected DIBs.
        unsafe {
            BitBlt(
                destination.device,
                0,
                0,
                2,
                1,
                original.device,
                0,
                0,
                SRCCOPY,
            )
        }
        .expect("copy original test pixels");
        assert!(apply_dim_wash(
            destination.device,
            dimmer.device,
            2,
            1,
            DIM_ALPHA
        ));
        // SAFETY: Restores the first pixel exactly as the overlay restores an active rectangle.
        unsafe {
            BitBlt(
                destination.device,
                0,
                0,
                1,
                1,
                original.device,
                0,
                0,
                SRCCOPY,
            )
        }
        .expect("restore selected test pixel");
        let pixels = destination.pixel_bytes();
        assert_eq!(&pixels[0..3], &[0, 0, 200]);
        assert_eq!(pixels[4], 0);
        assert!((99..=101).contains(&pixels[5]));
        assert_eq!(pixels[6], 0);
    }

    #[test]
    fn resize_hit_testing_prioritizes_corners_and_accepts_full_edges() {
        let rect = Rect {
            x: 100,
            y: 200,
            width: 300,
            height: 200,
        };
        assert_eq!(
            hit_test_resize_handle(rect, POINT { x: 104, y: 204 }, UiMetrics::new(96)),
            Some(ResizeHandle::NorthWest)
        );
        assert_eq!(
            hit_test_resize_handle(rect, POINT { x: 250, y: 203 }, UiMetrics::new(96)),
            Some(ResizeHandle::North)
        );
        assert_eq!(
            hit_test_resize_handle(rect, POINT { x: 397, y: 300 }, UiMetrics::new(96)),
            Some(ResizeHandle::East)
        );
        assert_eq!(
            hit_test_resize_handle(rect, POINT { x: 250, y: 300 }, UiMetrics::new(96)),
            None
        );
    }

    #[test]
    fn resize_handle_hit_target_scales_with_monitor_dpi() {
        let rect = Rect {
            x: 100,
            y: 200,
            width: 300,
            height: 200,
        };
        let point = POINT { x: 250, y: 215 };
        assert_eq!(
            hit_test_resize_handle(rect, point, UiMetrics::new(96)),
            None
        );
        assert_eq!(
            hit_test_resize_handle(rect, point, UiMetrics::new(192)),
            Some(ResizeHandle::North)
        );
    }

    #[test]
    fn corner_resize_moves_two_edges_and_preserves_the_opposite_corner() {
        assert_eq!(
            resize_region(
                Rect {
                    x: 100,
                    y: 100,
                    width: 200,
                    height: 100,
                },
                ResizeHandle::NorthWest,
                POINT { x: 50, y: 60 },
                Rect {
                    x: 0,
                    y: 0,
                    width: 500,
                    height: 400,
                },
            ),
            Rect {
                x: 50,
                y: 60,
                width: 250,
                height: 140,
            }
        );
    }

    #[test]
    fn resize_is_clamped_to_source_and_minimum_size() {
        let source = Rect {
            x: 0,
            y: 0,
            width: 500,
            height: 400,
        };
        let original = Rect {
            x: 100,
            y: 100,
            width: 200,
            height: 100,
        };
        assert_eq!(
            resize_region(
                original,
                ResizeHandle::East,
                POINT { x: 900, y: 100 },
                source,
            ),
            Rect {
                x: 100,
                y: 100,
                width: 400,
                height: 100,
            }
        );
        assert_eq!(
            resize_region(
                original,
                ResizeHandle::West,
                POINT { x: 299, y: 100 },
                source,
            ),
            Rect {
                x: 292,
                y: 100,
                width: 8,
                height: 100,
            }
        );
    }

    #[test]
    fn moving_region_preserves_dimensions() {
        assert_eq!(
            move_region(
                Rect {
                    x: 100,
                    y: 80,
                    width: 320,
                    height: 180,
                },
                POINT { x: 200, y: 150 },
                POINT { x: 275, y: 210 },
                Rect {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                },
            ),
            Rect {
                x: 175,
                y: 140,
                width: 320,
                height: 180,
            }
        );
    }

    #[test]
    fn moving_region_clamps_without_stretching() {
        let source = Rect {
            x: -100,
            y: 20,
            width: 500,
            height: 400,
        };
        let original = Rect {
            x: 50,
            y: 100,
            width: 200,
            height: 120,
        };
        assert_eq!(
            move_region(
                original,
                POINT { x: 75, y: 125 },
                POINT { x: 900, y: 900 },
                source,
            ),
            Rect {
                x: 200,
                y: 300,
                width: 200,
                height: 120,
            }
        );
    }

    #[test]
    fn live_selection_pixels_remain_hit_testable() {
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, true, true, false, false),
            LIVE_HIT_TEST_ALPHA
        );
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, true, false, false, false),
            DIM_ALPHA
        );
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, false, false, false, false),
            LIVE_HIT_TEST_ALPHA
        );
    }

    #[test]
    fn a_magnifier_that_was_not_drawn_is_not_an_obstacle_for_the_badge() {
        // The badge is placed once per paint and cannot be moved afterwards. Sampling used to
        // happen inside the draw, after the badge had already reserved space, so a sample that
        // failed left the badge pushed aside to avoid a magnifier that was never painted.
        assert_eq!(loupe_obstacle(None), None);
        let layout = layout_loupe(
            UiRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            POINT { x: 900, y: 500 },
            UiMetrics::new(96).loupe_tokens(),
        );
        assert_eq!(loupe_obstacle(Some(layout)), Some(layout.bounds));
    }

    #[test]
    fn the_magnifier_chrome_obeys_the_coverage_sentinel_like_every_other_control() {
        // The rounded box, grid, selection edges and label are GDI draws, so the sentinel has
        // already classified them as chrome. The pixels in the box's corner wedges are outside
        // the rounded border and were never drawn, so they must dim like their surroundings -
        // forcing the whole bounding box opaque painted four black corners around the magnifier.
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, true, false, true, false),
            u8::MAX,
            "drawn chrome is opaque without any magnifier override"
        );
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, true, false, false, false),
            DIM_ALPHA,
            "an undrawn corner wedge dims like the desktop around it"
        );
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, true, true, false, false),
            LIVE_HIT_TEST_ALPHA,
            "and inside the selection it stays see-through"
        );
    }

    #[test]
    fn the_magnifier_is_opaque_over_pixels_that_would_otherwise_show_through() {
        // Its magnified pixels arrive by StretchBlt, which copies the source's alpha byte instead
        // of zeroing it the way GDI's drawing calls do, so the coverage sentinel cannot classify
        // them. Without the explicit override a magnifier inside the selection would be
        // see-through over the very pixels it exists to magnify.
        for (in_selection, dim) in [(true, true), (false, true), (false, false)] {
            assert_eq!(
                live_pixel_alpha(CaptureTool::Region, dim, in_selection, false, true),
                u8::MAX,
                "in_selection={in_selection} dim={dim}"
            );
        }
    }

    #[test]
    fn confirm_telemetry_reports_live_previews_only_for_window_confirms() {
        // A Window confirm reports the actual live registrations; the rest of the inventory
        // is frozen.
        assert_eq!(confirm_preview_split(SelectionKind::Window, 3, 8), (3, 5));
        // A confirm that merely passed through Window mode hid every live preview on the way
        // out: the inventory still counts (run-scoped cost), but nothing is live.
        assert_eq!(confirm_preview_split(SelectionKind::Region, 3, 8), (0, 8));
        assert_eq!(confirm_preview_split(SelectionKind::Display, 3, 8), (0, 8));
        // A run that never entered Window mode has no inventory at all.
        assert_eq!(confirm_preview_split(SelectionKind::Region, 0, 0), (0, 0));
    }

    #[test]
    fn black_chrome_pixels_are_opaque_in_live_region_mode() {
        // Reproduce the live pipeline on a real DIB: sentinel-alpha fill, then pure-black GDI
        // chrome the way draw_outline's contrast halo and draw_resize_handles' rings paint it.
        let surface = FrozenSurface::empty(32, 32).expect("surface");
        fill_live_background(&surface);
        fill_device_rect(
            surface.device,
            RECT {
                left: 8,
                top: 8,
                right: 24,
                bottom: 24,
            },
            COLORREF(0),
        );
        // SAFETY: Flushes the queued fill before the CPU reads the DIB bytes.
        let _ = unsafe { GdiFlush() };
        // SAFETY: The surface uniquely owns this readable DIB on the test thread.
        let pixels = unsafe { std::slice::from_raw_parts(surface.bits, surface.byte_length) };
        let alpha_at = |x: usize, y: usize| pixels[(y * 32 + x) * 4 + 3];

        // GDI zeroed the alpha byte of the drawn black pixels; untouched background keeps the
        // sentinel. Coverage therefore separates the two even though both are pure black.
        assert_eq!(alpha_at(16, 16), 0, "drawn black pixel reads as chrome");
        assert_eq!(
            alpha_at(2, 2),
            LIVE_UNDRAWN_ALPHA,
            "untouched background keeps the sentinel"
        );

        // And the present-pass classification: drawn black chrome becomes opaque where the old
        // color heuristic left it at background alpha; genuinely undrawn areas keep the
        // reserved dim and hit-test values.
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, true, false, true, false),
            u8::MAX
        );
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, true, false, false, false),
            DIM_ALPHA
        );
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, false, true, false, false),
            LIVE_HIT_TEST_ALPHA
        );
    }

    #[test]
    fn live_window_chooser_and_drawn_controls_are_opaque() {
        assert_eq!(
            live_pixel_alpha(CaptureTool::Window, true, false, false, false),
            u8::MAX
        );
        assert_eq!(
            live_pixel_alpha(CaptureTool::Region, true, true, true, false),
            u8::MAX
        );
    }

    #[test]
    fn last_region_is_repositioned_without_resizing_when_it_fits() {
        assert_eq!(
            fit_region_to_source(
                Rect {
                    x: 900,
                    y: 700,
                    width: 320,
                    height: 180,
                },
                Rect {
                    x: 0,
                    y: 0,
                    width: 1000,
                    height: 800,
                },
            ),
            Rect {
                x: 680,
                y: 620,
                width: 320,
                height: 180,
            }
        );
    }

    #[test]
    fn capture_tools_match_confirmed_selection_kinds() {
        assert_eq!(
            CaptureTool::from_selection_kind(SelectionKind::Display),
            CaptureTool::FullDisplay
        );
        assert_eq!(
            CaptureTool::from_selection_kind(SelectionKind::Region),
            CaptureTool::Region
        );
        assert_eq!(
            CaptureTool::from_selection_kind(SelectionKind::Window),
            CaptureTool::Window
        );
        assert_eq!(
            CaptureTool::from_config(captastic_config::CaptureTool::FullDisplay),
            CaptureTool::FullDisplay
        );
        assert_eq!(
            CaptureTool::Window.to_config(),
            captastic_config::CaptureTool::Window
        );
    }

    #[test]
    fn tool_switching_restores_the_latest_interaction_region() {
        let source = Rect {
            x: 0,
            y: 0,
            width: 1_920,
            height: 1_080,
        };
        let previously_captured = Rect {
            x: 120,
            y: 80,
            width: 640,
            height: 360,
        };
        let moved_without_capture = Rect {
            x: 420,
            y: 260,
            width: 800,
            height: 450,
        };
        let latest = latest_interaction_region(
            CaptureTool::Region,
            Some(moved_without_capture),
            Some(SelectionKind::Region),
            Some(previously_captured),
        );
        assert_eq!(latest, Some(moved_without_capture));
        assert_eq!(
            latest_interaction_region(CaptureTool::Window, None, None, latest),
            Some(moved_without_capture)
        );
        assert_eq!(
            initial_selection(CaptureTool::Region, latest, source),
            (Some(moved_without_capture), Some(SelectionKind::Region))
        );
    }

    #[test]
    fn region_mode_restores_the_last_adjusted_rectangle() {
        let source = Rect {
            x: 0,
            y: 0,
            width: 1_920,
            height: 1_080,
        };
        let region = Rect {
            x: 120,
            y: 80,
            width: 640,
            height: 360,
        };
        assert_eq!(
            initial_selection(CaptureTool::Region, Some(region), source),
            (Some(region), Some(SelectionKind::Region))
        );
    }

    #[test]
    fn region_mode_defaults_to_a_centered_half_display_rectangle() {
        let source = Rect {
            x: -1_920,
            y: 100,
            width: 1_920,
            height: 1_080,
        };
        let expected = Rect {
            x: -1_440,
            y: 370,
            width: 960,
            height: 540,
        };
        assert_eq!(default_region_for_source(source), expected);
        assert_eq!(
            initial_selection(CaptureTool::Region, None, source),
            (Some(expected), Some(SelectionKind::Region))
        );
    }

    #[test]
    fn window_mode_starts_without_a_stale_rectangle() {
        let source = Rect {
            x: 0,
            y: 0,
            width: 1_920,
            height: 1_080,
        };
        let region = Rect {
            x: 120,
            y: 80,
            width: 640,
            height: 360,
        };
        assert_eq!(
            initial_selection(CaptureTool::Window, Some(region), source),
            (None, None)
        );
    }

    #[test]
    fn toolbar_controls_have_stable_hit_targets() {
        let environment = test_display_environment(1920, 1080, 1080, 96);
        let origin = ToolbarLayout::default_origin(environment);
        let layout = ToolbarLayout::new(environment, origin);
        assert_eq!(
            layout.hit_test(
                POINT {
                    x: layout.options.left + 4,
                    y: layout.options.top + 4,
                },
                false,
            ),
            Some(ToolbarControl::Options)
        );
        assert_eq!(
            layout.hit_test(
                POINT {
                    x: layout.full_display.left + 4,
                    y: layout.full_display.top + 4,
                },
                false,
            ),
            Some(ToolbarControl::FullDisplay)
        );
        assert_eq!(
            layout.hit_test(
                POINT {
                    x: layout.capture.left + 4,
                    y: layout.capture.top + 4,
                },
                false,
            ),
            Some(ToolbarControl::Capture)
        );
        assert_eq!(
            layout.hit_test(
                POINT {
                    x: layout.dim_background.left + 4,
                    y: layout.dim_background.top + 4,
                },
                true,
            ),
            Some(ToolbarControl::DimBackground)
        );
        assert_eq!(layout.hit_test(POINT { x: 10, y: 10 }, false), None);
    }

    #[test]
    fn live_thumbnails_yield_to_visible_overlay_chrome() {
        let environment = test_display_environment(1920, 1080, 1080, 96);
        let layout = ToolbarLayout::new(environment, ToolbarLayout::default_origin(environment));
        let menu_overlap = RECT {
            left: layout.menu.left + 1,
            top: layout.menu.top + 1,
            right: layout.menu.right - 1,
            bottom: layout.menu.bottom - 1,
        };
        let toolbar_overlap = RECT {
            left: layout.bounds.left + 1,
            top: layout.bounds.top + 1,
            right: layout.bounds.right - 1,
            bottom: layout.bounds.bottom - 1,
        };

        assert!(live_thumbnail_overlaps_chrome(
            menu_overlap,
            layout,
            true,
            environment
        ));
        // With the options menu closed the menu rect is no longer chrome, but for a bottom
        // toolbar it lies inside the always-reserved tooltip band, so it still yields.
        assert!(live_thumbnail_overlaps_chrome(
            menu_overlap,
            layout,
            false,
            environment
        ));
        assert!(live_thumbnail_overlaps_chrome(
            toolbar_overlap,
            layout,
            false,
            environment
        ));
        assert!(!live_thumbnail_overlaps_chrome(
            RECT {
                left: 8,
                top: 8,
                right: 64,
                bottom: 64,
            },
            layout,
            true,
            environment
        ));

        // The default-origin toolbar sits near the bottom, so the tooltip band lies directly
        // above it; a thumbnail there must yield even while no tooltip is showing.
        let band = tooltip_band(environment, layout);
        assert_eq!(band.bottom, layout.bounds.top);
        assert!(band.top < layout.bounds.top);
        let band_overlap = RECT {
            left: layout.bounds.left,
            top: band.top + 1,
            right: layout.bounds.left + 10,
            bottom: band.top + 2,
        };
        assert!(live_thumbnail_overlaps_chrome(
            band_overlap,
            layout,
            false,
            environment
        ));
    }

    #[test]
    fn dragged_toolbar_is_clamped_inside_the_display() {
        let environment = test_display_environment(1920, 1080, 1080, 96);
        assert_eq!(
            ToolbarLayout::clamp_origin(environment, POINT { x: -400, y: 2000 }),
            POINT { x: 8, y: 1016 }
        );
    }

    #[test]
    fn toolbar_position_restores_from_normalized_work_area_coordinates() {
        let environment = test_display_environment(2560, 1440, 1400, 144);
        let layout = ToolbarLayout::new(environment, ToolbarLayout::default_origin(environment));
        assert_eq!(layout.bounds.width(), 627);
        assert_eq!(layout.bounds.height(), 84);
        assert!(layout.bounds.bottom <= environment.work_area.bottom);
        assert_eq!(
            remembered_toolbar_position(Some((0.5, 0.5)), None, environment),
            Some(POINT { x: 967, y: 658 })
        );
        let smaller = test_display_environment(1920, 1080, 1040, 96);
        assert_eq!(
            remembered_toolbar_position(Some((0.5, 0.5)), None, smaller),
            Some(POINT { x: 751, y: 492 })
        );
    }

    #[test]
    fn legacy_toolbar_pixels_are_clamped_to_the_monitor_work_area() {
        let environment = test_display_environment(1920, 1080, 1040, 96);
        assert_eq!(
            remembered_toolbar_position(None, Some((3700, 2000)), environment),
            Some(POINT { x: 1494, y: 976 })
        );
    }

    fn test_display_environment(
        width: i32,
        height: i32,
        work_bottom: i32,
        dpi: u32,
    ) -> DisplayEnvironment {
        DisplayEnvironment {
            work_area: UiRect {
                left: 0,
                top: 0,
                right: width,
                bottom: work_bottom.min(height),
            },
            metrics: UiMetrics::new(dpi),
        }
    }

    #[test]
    fn monitor_local_region_restores_against_a_negative_display_origin() {
        let source = Rect {
            x: -1920,
            y: -200,
            width: 1920,
            height: 1200,
        };
        assert_eq!(
            remembered_last_region(
                Some(captastic_config::CaptureRegion {
                    x: 200,
                    y: 100,
                    width: 800,
                    height: 600,
                }),
                None,
                true,
                source,
                0,
            ),
            Some(Rect {
                x: -1720,
                y: -100,
                width: 800,
                height: 600,
            })
        );
    }

    #[test]
    fn saved_region_keeps_pixel_size_and_relative_center_after_resolution_change() {
        let restored = restore_region_for_display_change(
            captastic_config::CaptureRegion {
                x: 560,
                y: 240,
                width: 800,
                height: 600,
            },
            captastic_config::CaptureRegionSource {
                width: 1920,
                height: 1080,
                rotation_degrees: 0,
            },
            Rect {
                x: -3840,
                y: 100,
                width: 3840,
                height: 2160,
            },
            0,
        );
        assert_eq!(
            restored,
            Rect {
                x: -2320,
                y: 880,
                width: 800,
                height: 600,
            }
        );
    }

    #[test]
    fn saved_region_rotates_its_center_and_dimensions_with_the_display() {
        let restored = restore_region_for_display_change(
            captastic_config::CaptureRegion {
                x: 100,
                y: 200,
                width: 400,
                height: 300,
            },
            captastic_config::CaptureRegionSource {
                width: 1920,
                height: 1080,
                rotation_degrees: 0,
            },
            Rect {
                x: 1920,
                y: -400,
                width: 1080,
                height: 1920,
            },
            90,
        );
        assert_eq!(
            restored,
            Rect {
                x: 2500,
                y: -300,
                width: 300,
                height: 400,
            }
        );
    }

    #[test]
    fn window_overview_arranges_independent_surfaces_in_centered_rows() {
        let dimensions = vec![(1600, 900); 5];
        let rectangles =
            layout_floating_windows(1920, 1080, UiMetrics::new(UiMetrics::BASE_DPI), &dimensions);
        assert_eq!(rectangles.len(), dimensions.len());
        assert!(rectangles[0].right < rectangles[1].left);
        assert_eq!(
            rectangles
                .iter()
                .take_while(|rectangle| rectangle.top == rectangles[0].top)
                .count(),
            3
        );
        assert!(rectangles[0].right - rectangles[0].left > 500);
        assert!(rectangles[3].top > rectangles[0].top);
        assert!(rectangles.iter().all(|rectangle| {
            rectangle.left >= 0
                && rectangle.top >= 0
                && rectangle.right <= 1920
                && rectangle.bottom <= 1080
        }));
        assert!(rectangles.iter().all(|rectangle| {
            let width = rectangle.right - rectangle.left;
            let height = rectangle.bottom - rectangle.top;
            (width * 9 - height * 16).abs() <= 16
        }));
    }

    #[test]
    fn overview_layout_scales_with_monitor_dpi_and_clears_the_toolbar_band() {
        let dimensions = vec![(1600, 900); 5];
        // 200% scaling on a 4K display: the margins must scale with the toolbar's metrics so
        // the bottom thumbnail row cannot collide with the (equally scaled) toolbar band.
        let metrics = UiMetrics::new(192);
        let tokens = metrics.overview_tokens();
        assert_eq!(tokens.margin_x, 140);
        assert_eq!(tokens.top, 128);
        assert_eq!(tokens.bottom_reserve, 320);
        let rectangles = layout_floating_windows(3840, 2160, metrics, &dimensions);
        assert_eq!(rectangles.len(), dimensions.len());
        assert!(rectangles.iter().all(|rectangle| {
            rectangle.left >= tokens.margin_x
                && rectangle.top >= tokens.top
                && rectangle.right <= 3840 - tokens.margin_x
                && rectangle.bottom <= 2160 - tokens.bottom_reserve
        }));

        // Base DPI is the identity, so default-DPI layouts do not shift: the scaled geometry
        // at 200% on a doubled client matches the 100% layout doubled, rounding aside.
        let base =
            layout_floating_windows(1920, 1080, UiMetrics::new(UiMetrics::BASE_DPI), &dimensions);
        // Per-rect rounding (window height, per-column width, centering) can drift a few
        // pixels as columns accumulate; the bound stays far below one gap width, so any
        // structural divergence (different column count) would still fail loudly.
        for (scaled, base) in rectangles.iter().zip(&base) {
            assert!((scaled.left - base.left * 2).abs() <= 6);
            assert!((scaled.top - base.top * 2).abs() <= 6);
            assert!((scaled.right - base.right * 2).abs() <= 6);
            assert!((scaled.bottom - base.bottom * 2).abs() <= 6);
        }
    }

    #[test]
    fn overview_thumbnails_have_a_bounded_pixel_budget() {
        assert_eq!(scaled_dimensions(800, 600, 1_200_000), (800, 600));
        let (width, height) = scaled_dimensions(7_680, 4_320, 1_200_000);
        assert!(u64::from(width) * u64::from(height) <= 1_200_000);
        assert!((width as f64 / height as f64 - 16.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn compact_control_text_fits_at_supported_dpi_levels() {
        let _font_resource =
            PrivateFontResource::register().expect("register embedded IoskeleyMono font");
        let surface = FrozenSurface::empty(8, 8).expect("text measurement surface");
        for dpi in [96, 120, 144, 192] {
            let metrics = UiMetrics::new(dpi);
            let tokens = metrics.toolbar_tokens();
            let environment = DisplayEnvironment {
                work_area: UiRect {
                    left: 0,
                    top: 0,
                    right: 3840,
                    bottom: 2160,
                },
                metrics,
            };
            let layout =
                ToolbarLayout::new(environment, ToolbarLayout::default_origin(environment));
            let labels = [
                (
                    "Options",
                    layout.options.width() - tokens.text_padding - tokens.icon_size,
                    layout.options.height(),
                ),
                (
                    "Capture",
                    layout.capture.width()
                        - tokens.icon_size
                        - metrics.px(18)
                        - tokens.text_padding,
                    layout.capture.height(),
                ),
                (
                    "Dim Background",
                    layout.dim_background.width() - tokens.menu_text_offset - tokens.text_padding,
                    layout.dim_background.height(),
                ),
                (
                    SNAP_TO_WINDOWS_LABEL,
                    layout.snap_to_windows.width() - tokens.menu_text_offset - tokens.text_padding,
                    layout.snap_to_windows.height(),
                ),
                (
                    machine::LoupeMode::Auto.label(),
                    layout.region_zoom.width() - tokens.menu_text_offset - tokens.text_padding,
                    layout.region_zoom.height(),
                ),
                (
                    machine::LoupeMode::Key.label(),
                    layout.region_zoom.width() - tokens.menu_text_offset - tokens.text_padding,
                    layout.region_zoom.height(),
                ),
                (
                    machine::LoupeMode::Off.label(),
                    layout.region_zoom.width() - tokens.menu_text_offset - tokens.text_padding,
                    layout.region_zoom.height(),
                ),
                (
                    PreviewView::Live.menu_label(),
                    layout.preview_view.width() - tokens.menu_text_offset - tokens.text_padding,
                    layout.preview_view.height(),
                ),
                (
                    PreviewView::Frozen.menu_label(),
                    layout.preview_view.width() - tokens.menu_text_offset - tokens.text_padding,
                    layout.preview_view.height(),
                ),
                (
                    "Copy to Clipboard",
                    layout.clipboard_destination.width()
                        - tokens.menu_text_offset
                        - tokens.text_padding,
                    layout.clipboard_destination.height(),
                ),
                (
                    "Cancel Capture",
                    layout.cancel.width() - tokens.menu_text_offset - tokens.text_padding,
                    layout.cancel.height(),
                ),
            ];
            for (label, available_width, available_height) in labels {
                let measured = measure_ui_text(surface.device, label, tokens.font_height);
                assert!(
                    measured.cx <= available_width,
                    "{label} is {measured:?} at {dpi} DPI but only {available_width} px are available"
                );
                assert!(
                    measured.cy <= available_height,
                    "{label} is {measured:?} at {dpi} DPI but only {available_height} px are available"
                );
            }
            // The view tag sizes its pill around the measurement rather than fitting inside a
            // fixed row, so the constraint is the display: a tag wider than the work area is
            // clipped, and a clipped explanation of why the desktop has stopped moving is worse
            // than none. 1280 DIPs is the narrowest configuration the verification doc covers.
            let tag = measure_ui_text(surface.device, FROZEN_VIEW_TAG, tokens.font_height);
            let narrowest = metrics.px(1280) - metrics.px(16);
            assert!(
                tag.cx + tokens.tooltip_padding_x * 2 <= narrowest,
                "the frozen view tag is {tag:?} at {dpi} DPI but only {narrowest} px are available"
            );
        }
    }

    #[test]
    fn overlay_resources_reuse_matching_surfaces_and_reject_other_sizes() {
        let mut resources = OverlayResources::new();
        resources.cache = Some(OverlayResourceCache {
            surface: FrozenSurface::empty(8, 8).expect("surface"),
            back_buffer: FrozenSurface::empty(8, 8).expect("back buffer"),
            dimmer: FrozenSurface::new(1, 1, &[0, 0, 0, 255]).expect("dimmer"),
            blurred_background: None,
            overview_surface: None,
            region_cursor: RegionCursor::create(),
            font_resource: PrivateFontResource::register().expect("font"),
        });
        let cached = resources.take_matching(8, 8).expect("matching cache");
        resources.cache = Some(cached);
        // A resolution change must drop the stale allocation rather than hand it out.
        assert!(resources.take_matching(16, 16).is_none());
        assert!(resources.cache.is_none(), "the mismatch consumed the cache");
        resources.cache = Some(OverlayResourceCache {
            surface: FrozenSurface::empty(8, 8).expect("surface"),
            back_buffer: FrozenSurface::empty(8, 8).expect("back buffer"),
            dimmer: FrozenSurface::new(1, 1, &[0, 0, 0, 255]).expect("dimmer"),
            blurred_background: None,
            overview_surface: None,
            region_cursor: RegionCursor::create(),
            font_resource: PrivateFontResource::register().expect("font"),
        });
        resources.clear();
        assert!(resources.take_matching(8, 8).is_none());
    }

    #[test]
    fn overview_build_queue_retries_once_in_order_and_rejects_stale_generations() {
        let first = NativeWindowHandle::from_raw(1);
        let second = NativeWindowHandle::from_raw(2);
        let third = NativeWindowHandle::from_raw(3);
        let mut queue = OverviewBuildQueue::new(7, [first, second, third]);

        // A batch message stamped with another build's generation is stale.
        assert!(queue.accepts(7));
        assert!(!queue.accepts(6));
        assert!(!queue.accepts(8));

        // Batches drain in enumeration order; a retryable first failure re-queues at the back,
        // so retried windows land after every first-pass window - the old two-pass ordering.
        assert_eq!(queue.take_batch(1), vec![(first, 0)]);
        assert!(queue.requeue(first, 0));
        assert_eq!(queue.take_batch(1), vec![(second, 0)]);
        assert_eq!(queue.take_batch(2), vec![(third, 0), (first, 1)]);

        // The second failure of the same window is dropped instead of re-queued.
        assert!(!queue.requeue(first, 1));
        assert!(queue.is_done());

        // A zero-size batch request still makes progress instead of looping forever.
        let mut queue = OverviewBuildQueue::new(1, [first]);
        assert_eq!(queue.take_batch(0), vec![(first, 0)]);
        assert!(queue.is_done());
    }

    #[test]
    fn a_tile_that_loses_its_live_preview_is_queued_once() {
        // A tile can stop being drawn by DWM at any moment — the toolbar moves over it, a menu
        // opens above it — and then it needs pixels it was deliberately never given. Repeated
        // chrome movement must not queue the same window again and again behind itself.
        let uncovered = NativeWindowHandle::from_raw(1);
        let other = NativeWindowHandle::from_raw(2);
        let mut queue = OverviewBuildQueue::new(3, [other]);

        assert!(queue.enqueue(uncovered), "a new window is queued");
        assert!(
            !queue.enqueue(uncovered),
            "a window already waiting must not be queued twice"
        );
        assert!(!queue.enqueue(other), "nor one from the original build");

        assert_eq!(queue.take_batch(2), vec![(other, 0), (uncovered, 0)]);
        assert!(queue.is_done());

        // Once drained, the same window can be queued again: it may have been covered and
        // uncovered a second time.
        assert!(queue.enqueue(uncovered));
        assert!(!queue.is_done());
    }

    #[test]
    fn the_confirm_split_counts_rendered_tiles_as_frozen() {
        // With live previews the inventory is no longer all-rendered, so the split now means
        // "drawn by DWM" against "drawn by us" — which is what it was always trying to say.
        assert_eq!(confirm_preview_split(SelectionKind::Window, 8, 10), (8, 2));
        // Leaving the Window tool hides every registration, so nothing is live at confirm time.
        assert_eq!(confirm_preview_split(SelectionKind::Region, 8, 10), (0, 10));
        assert_eq!(
            confirm_preview_split(SelectionKind::Window, 10, 10),
            (10, 0)
        );
    }

    #[test]
    fn a_previous_runs_overview_surface_is_an_allocation_never_content() {
        // The constructor no longer manufactures a WindowOverviewCache from a previous run's
        // surface, so its stale pixels cannot be blitted as current content; the only way the
        // spare reaches the screen is through this reuse seam, whose caller overdraws it.
        let spare_surface = FrozenSurface::empty(8, 8).expect("surface");
        let spare_device = spare_surface.device.0;
        let mut spare = Some(spare_surface);
        let reused = reusable_overview_surface(None, &mut spare, 8, 8).expect("matching spare");
        assert_eq!(
            reused.device.0, spare_device,
            "the allocation itself is reused"
        );
        assert!(spare.is_none(), "the spare was consumed");

        // The current cache's own surface wins; the spare stays put for later.
        let current = FrozenSurface::empty(8, 8).expect("surface");
        let current_device = current.device.0;
        let mut spare = Some(FrozenSurface::empty(8, 8).expect("surface"));
        let reused =
            reusable_overview_surface(Some(current), &mut spare, 8, 8).expect("current allocation");
        assert_eq!(reused.device.0, current_device);
        assert!(spare.is_some());

        // A resolution change consumes the mismatched candidate rather than handing it out.
        let mut spare = Some(FrozenSurface::empty(8, 8).expect("surface"));
        assert!(reusable_overview_surface(None, &mut spare, 16, 16).is_none());
        assert!(spare.is_none());
    }

    fn frame_metadata_for_tests() -> FrameMetadata {
        FrameMetadata {
            capture_id: CaptureId(1),
            backend: "test".to_owned(),
            display_id: DisplayId::primary(),
            source_rect: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            rotation_degrees: 0,
            capture_mode: CaptureMode::Latest { max_age_ms: None },
            presentation_offset_ns: Some(0),
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: 0,
            cpu_ready_offset_ns: Some(0),
            frame_age_ns: Some(0),
            verified_current_offset_ns: None,
            frame_generation: Some(1),
            copy_count: 1,
            pool_slot: Some(0),
            cursor: None,
        }
    }

    fn ready_preview_state(handle: NativeWindowHandle) -> WindowPreviewState {
        let metadata = FrameMetadata {
            capture_id: CaptureId(1),
            backend: "test".to_owned(),
            display_id: DisplayId::primary(),
            source_rect: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            rotation_degrees: 0,
            capture_mode: CaptureMode::Latest { max_age_ms: None },
            presentation_offset_ns: Some(0),
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: 0,
            cpu_ready_offset_ns: Some(0),
            frame_age_ns: Some(0),
            verified_current_offset_ns: None,
            frame_generation: Some(1),
            copy_count: 1,
            pool_slot: Some(0),
            cursor: None,
        };
        let frame = CpuFrame::new(
            Arc::from(vec![0_u8; 4]),
            1,
            1,
            4,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("test preview frame");
        WindowPreviewState::Ready(Box::new(WindowPreview {
            handle,
            frame,
            surface: FrozenSurface::empty(1, 1).expect("test preview surface"),
            corner_radius_px: 0.0,
        }))
    }

    #[test]
    fn an_unavailable_window_preview_is_retried_on_the_next_click() {
        let target = NativeWindowHandle::from_raw(11);
        let other = NativeWindowHandle::from_raw(22);

        let failed = WindowPreviewState::Unavailable(target);
        assert!(
            !failed.satisfies(target),
            "a transient capture failure must not suppress the next attempt"
        );
        assert!(!failed.satisfies(other));

        let ready = ready_preview_state(target);
        assert!(
            ready.satisfies(target),
            "a ready preview is reused without recapturing"
        );
        assert!(!ready.satisfies(other));
    }
}
