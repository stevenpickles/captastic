use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use captastic_core::{
    process_detach_ledger, CaptureError, CaptureErrorKind, CpuFrame, CursorAbsence, CursorCapture,
    DetachCount, DetachKind, DetachLedger, FrameAlpha, FrameMetadata, Rect,
};
use windows::core::Error as WindowsError;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_VISIBLE_FRAME_BORDER_THICKNESS,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DWMWCP_ROUND,
    DWMWCP_ROUNDSMALL, DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, SelectObject,
    SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE,
    HBITMAP, HDC, HGDIOBJ, RGBQUAD, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, GetWindowRect, IsZoomed, SendMessageTimeoutW, GWL_STYLE,
    PW_RENDERFULLCONTENT, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_NULL, WS_CAPTION,
};

use crate::window_capture_wgc::capture_window as capture_window_wgc_frame;
use crate::{NativeWindowHandle, OverlaySelection, SelectionKind};

const WINDOW_RENDER_TIMEOUT: Duration = Duration::from_millis(700);
const MAX_IN_FLIGHT_WINDOW_RENDERS: usize = 2;
const MAX_WINDOW_RENDER_WORKERS: usize = 8;
const WINDOW_RESPONSIVENESS_TIMEOUT_MS: u32 = 50;
pub(crate) const WINDOW_THUMBNAIL_RENDER_BATCH: usize = MAX_IN_FLIGHT_WINDOW_RENDERS - 1;
const _: () = assert!(MAX_IN_FLIGHT_WINDOW_RENDERS > 1);
const WINDOW_BORDER_BGRA: [u8; 3] = [188, 180, 176];
static IN_FLIGHT_WINDOW_RENDERS: AtomicUsize = AtomicUsize::new(0);
static WINDOW_RENDER_WORKERS: AtomicUsize = AtomicUsize::new(0);
// The worker budget is what actually enforces the detach ceiling, because a detached render holds
// its slot until it exits. The ledger only documents the number, so the two have to agree or the
// documented ceiling describes a limit nothing imposes.
const _: () = assert!(MAX_WINDOW_RENDER_WORKERS == DetachKind::WindowRender.ceiling());
static ACTIVE_WINDOW_RENDER_TARGETS: Mutex<BTreeSet<isize>> = Mutex::new(BTreeSet::new());
const RENDER_ACTIVE: u8 = 0;
const RENDER_DETACHED: u8 = 1;
const RENDER_COMPLETED: u8 = 2;

pub fn materialize_selection(
    frozen_desktop: &CpuFrame,
    selection: &OverlaySelection,
) -> Result<CpuFrame, CaptureError> {
    match selection.kind {
        SelectionKind::Display => Ok(frozen_desktop.clone()),
        SelectionKind::Region => frozen_desktop.crop(selection.rect).map_err(|error| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "crop_region",
                error.to_string(),
                false,
                None,
            )
        }),
        SelectionKind::Window => {
            if let Some(frame) = &selection.window_frame {
                return Ok(frame.clone());
            }
            let handle = selection.window.ok_or_else(|| {
                capture_error(
                    CaptureErrorKind::SourceUnavailable,
                    "resolve_selected_window",
                    "window selection did not retain a native window handle",
                    false,
                    None,
                )
            })?;
            capture_window(handle, &frozen_desktop.metadata)
        }
    }
}

/// Returns the native frame captured while a window selection was confirmed.
///
/// Live display and region selections intentionally defer pixel acquisition until after the
/// overlay closes. A window selection is different: confirming it already renders that native
/// window independently of the desktop, so callers should not acquire an unrelated display frame.
pub fn captured_window_frame(selection: &OverlaySelection) -> Option<CpuFrame> {
    (selection.kind == SelectionKind::Window)
        .then(|| selection.window_frame.clone())
        .flatten()
}

pub(crate) fn capture_window(
    handle: NativeWindowHandle,
    reference_metadata: &FrameMetadata,
) -> Result<CpuFrame, CaptureError> {
    capture_window_visual(handle, reference_metadata).map(|capture| capture.frame)
}

pub(crate) fn capture_window_visual(
    handle: NativeWindowHandle,
    reference_metadata: &FrameMetadata,
) -> Result<CapturedWindow, CaptureError> {
    capture_window_bounded(handle, reference_metadata, None)
}

/// What a window capture can say about the cursor, which is only ever that it has none.
///
/// `PrintWindow` renders a window's own drawing, with no compositor and no pointer; Windows
/// Graphics Capture can draw one, but the compositor positions it and we could neither clip it to
/// the window nor compare a cursor-on capture against a cursor-off one. Reported rather than
/// silently dropped, so a user whose configuration asks for a cursor can see which captures could
/// not have one.
fn window_cursor_outcome(metadata: &FrameMetadata) -> CursorCapture {
    match metadata.cursor {
        Some(CursorCapture::Excluded) | None => CursorCapture::Excluded,
        _ => CursorCapture::Absent {
            reason: CursorAbsence::SourceCannotCompose,
        },
    }
}

pub(crate) fn capture_window_thumbnail(
    handle: NativeWindowHandle,
    reference_metadata: &FrameMetadata,
    max_pixels: u64,
) -> Result<CapturedWindow, CaptureError> {
    capture_window_bounded(handle, reference_metadata, Some(max_pixels))
}

pub(crate) struct CapturedWindow {
    pub frame: CpuFrame,
    pub corner_radius_px: f32,
}

fn capture_window_bounded(
    handle: NativeWindowHandle,
    reference_metadata: &FrameMetadata,
    max_pixels: Option<u64>,
) -> Result<CapturedWindow, CaptureError> {
    let budget = WindowRenderBudget::acquire(handle.raw())?;
    let permit = WindowRenderPermit::acquire()?;
    let metadata = reference_metadata.clone();
    // One deadline, shared: the caller stops waiting at `deadline`, so every backend the worker
    // drives has to stop waiting by then too.
    let deadline = RenderDeadline::starting_now(WINDOW_RENDER_TIMEOUT);
    run_bounded_window_render(permit, budget, deadline, move || {
        // Capture geometry must use physical pixels. Thread DPI awareness is not inherited
        // reliably by newly spawned workers, and mixing virtualized GetWindowRect values with
        // physical DWM bounds leaves asymmetric one- or two-pixel frame artifacts.
        // SAFETY: Changes DPI virtualization only for this short-lived render worker.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        capture_window_inner(handle, &metadata, max_pixels, deadline)
    })
}

/// The wall-clock instant a bounded window render stops being useful.
///
/// The worker is abandoned once the deadline passes and anything it produces afterwards is thrown
/// away, so a backend that waits internally has to know when waiting stops paying. Sizing an inner
/// budget independently of this one is what let a legitimately slow WGC capture spend the caller's
/// entire deadline and still be discarded — having burned a worker-lifetime slot to produce a
/// frame nobody read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RenderDeadline {
    expires_at: Instant,
}

impl RenderDeadline {
    pub(crate) fn starting_now(budget: Duration) -> Self {
        Self {
            expires_at: Instant::now() + budget,
        }
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// Clamps a backend's own timeout to the time the caller can still use.
    ///
    /// `reserve` is the work that must still happen *after* the wait for its result to reach the
    /// caller — without it a backend can wait right up to the deadline and then miss it while
    /// publishing what it waited for.
    pub(crate) fn bounded_wait(&self, preferred: Duration, reserve: Duration) -> Duration {
        bounded_wait(preferred, self.remaining(), reserve)
    }
}

/// The arithmetic behind [`RenderDeadline::bounded_wait`], separated so it can be tabulated.
fn bounded_wait(preferred: Duration, remaining: Duration, reserve: Duration) -> Duration {
    preferred.min(remaining.saturating_sub(reserve))
}

fn run_bounded_window_render<T>(
    permit: WindowRenderPermit<'static>,
    budget: WindowRenderBudget<'static>,
    deadline: RenderDeadline,
    render: impl FnOnce() -> Result<T, CaptureError> + Send + 'static,
) -> Result<T, CaptureError>
where
    T: Send + 'static,
{
    let timeout_permit = permit.clone();
    let render_state = Arc::new(AtomicU8::new(RENDER_ACTIVE));
    let worker_render_state = render_state.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("captastic-window-render".to_owned())
        .spawn(move || {
            let _budget = budget;
            let _permit = permit;
            let _completion = RenderCompletion(worker_render_state);
            let _ = sender.send(render());
        })
        .map_err(|error| {
            capture_error(
                CaptureErrorKind::NativeFailure,
                "spawn_window_render",
                error.to_string(),
                true,
                None,
            )
        })?;
    // The worker inherits the same deadline, and it started counting a spawn ago — so it gives up
    // marginally before this wait does, which is the safe direction.
    match receiver.recv_timeout(deadline.remaining()) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The detached worker may remain blocked inside a foreign window procedure. Its
            // lease expires at the caller's deadline so unrelated windows can still be rendered;
            // release is idempotent when the worker eventually exits.
            timeout_permit.release();
            if let Some(detached) = mark_render_detached(&render_state, process_detach_ledger()) {
                let at_ceiling = detached.at_ceiling(DetachKind::WindowRender);
                // At the ceiling every worker slot is held by a render that never came back, so
                // the next window capture is refused outright rather than merely slow. That is a
                // different thing to report than one wedged window, and it is reported louder.
                log::log!(
                    if at_ceiling {
                        log::Level::Error
                    } else {
                        log::Level::Warn
                    },
                    "window render exceeded its deadline; {} detached render worker(s) still running of {} ceiling, {} detached in total{}",
                    detached.live,
                    DetachKind::WindowRender.ceiling(),
                    detached.total,
                    if at_ceiling {
                        " — further window renders will be refused until one returns"
                    } else {
                        ""
                    }
                );
            }
            Err(capture_error(
                CaptureErrorKind::Timeout,
                "print_selected_window",
                format!(
                    "the selected window did not render within {} ms",
                    WINDOW_RENDER_TIMEOUT.as_millis()
                ),
                true,
                None,
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            timeout_permit.release();
            Err(capture_error(
                CaptureErrorKind::NativeFailure,
                "print_selected_window",
                "the window-render worker stopped without returning a frame",
                false,
                None,
            ))
        }
    }
}

struct RenderCompletion(Arc<AtomicU8>);

impl Drop for RenderCompletion {
    fn drop(&mut self) {
        complete_render(&self.0, process_detach_ledger());
    }
}

/// Marks a render detached, returning the ledger's counts if this call is the one that detached it.
///
/// `None` means the worker had already finished — the caller and the worker raced at the deadline,
/// and the worker won.
fn mark_render_detached(state: &AtomicU8, ledger: &DetachLedger) -> Option<DetachCount> {
    if state
        .compare_exchange(
            RENDER_ACTIVE,
            RENDER_DETACHED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        Some(ledger.detached(DetachKind::WindowRender))
    } else {
        None
    }
}

fn complete_render(state: &AtomicU8, ledger: &DetachLedger) {
    if state.swap(RENDER_COMPLETED, Ordering::AcqRel) == RENDER_DETACHED {
        ledger.rejoined(DetachKind::WindowRender);
    }
}

fn capture_window_inner(
    handle: NativeWindowHandle,
    reference_metadata: &FrameMetadata,
    max_pixels: Option<u64>,
    deadline: RenderDeadline,
) -> Result<CapturedWindow, CaptureError> {
    let hwnd = HWND(handle.raw());
    let mut native_bounds = RECT::default();
    // SAFETY: The handle came from the immediately preceding EnumWindows selection pass.
    unsafe { GetWindowRect(hwnd, &mut native_bounds) }
        .map_err(|error| windows_error("get_selected_window_bounds", error, true))?;
    let capture_bounds = rect_from_native(native_bounds).ok_or_else(|| {
        capture_error(
            CaptureErrorKind::SourceUnavailable,
            "get_selected_window_bounds",
            "selected window has invalid or empty bounds",
            true,
            None,
        )
    })?;
    let geometry = window_geometry(hwnd, capture_bounds);
    let visible_bounds = geometry.content;
    let probe_wgc_error = if !window_is_responsive(hwnd) {
        log::debug!(
            "window handle=0x{:X} did not answer the responsiveness probe; using Windows Graphics Capture",
            handle.raw()
        );
        match capture_window_with_wgc(hwnd, reference_metadata, max_pixels, &geometry, deadline) {
            Ok(capture) => return Ok(capture),
            Err(error) => {
                log::debug!(
                    "window handle=0x{:X} could not use Windows Graphics Capture after the probe; trying bounded PrintWindow: {error}",
                    handle.raw()
                );
                Some(error)
            }
        }
    } else {
        None
    };
    let mut surface = DibSurface::new(capture_bounds.width, capture_bounds.height)?;
    surface.clear();
    // Re-probe immediately before the blocking call. Everything since the first probe — the DWM
    // queries, the DIB allocation, and possibly a full WGC attempt — is time the target had to
    // stop answering, and `PrintWindow` is a synchronous call into its window procedure with no
    // timeout of its own. A worker that enters it against a hung window never returns, and the
    // slot it holds is only reclaimed when it does.
    if !window_is_responsive(hwnd) {
        log::debug!(
            "window handle=0x{:X} stopped answering between the responsiveness probe and PrintWindow; refusing to block a render worker on it",
            handle.raw()
        );
        return match probe_wgc_error {
            // Windows Graphics Capture already failed for this window during the first probe;
            // there is nothing left to try that will not wedge the worker.
            Some(error) => Err(error),
            None => {
                capture_window_with_wgc(hwnd, reference_metadata, max_pixels, &geometry, deadline)
            }
        };
    }
    // SAFETY: hwnd is the selected live top-level window and surface.device contains a selected
    // writable DIB sized to the complete window bounds. PrintWindow asks the owning application
    // to render itself, so desktop occlusion is not copied into the result.
    let rendered = unsafe {
        PrintWindow(
            hwnd,
            surface.device,
            PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT),
        )
    };
    if !rendered.as_bool() {
        if let Some(error) = probe_wgc_error {
            return Err(error);
        }
        log::debug!(
            "window handle=0x{:X} rejected PrintWindow; trying Windows Graphics Capture",
            handle.raw()
        );
        return capture_window_with_wgc(hwnd, reference_metadata, max_pixels, &geometry, deadline);
    }
    if region_is_blank(surface.as_bytes(), capture_bounds, visible_bounds)? {
        if probe_wgc_error.is_none() {
            log::debug!(
                "window handle=0x{:X} PrintWindow reported success but rendered a blank frame; trying Windows Graphics Capture",
                handle.raw()
            );
            match capture_window_with_wgc(hwnd, reference_metadata, max_pixels, &geometry, deadline)
            {
                Ok(capture) => return Ok(capture),
                Err(error) => {
                    log::debug!(
                        "window handle=0x{:X} Windows Graphics Capture also failed after a blank PrintWindow render; keeping the blank PrintWindow frame: {error}",
                        handle.raw()
                    );
                }
            }
        } else {
            log::debug!(
                "window handle=0x{:X} PrintWindow reported success but rendered a blank frame; Windows Graphics Capture already failed during the responsiveness probe, keeping the blank PrintWindow frame",
                handle.raw()
            );
        }
    }
    let (pixels, content_width, content_height, radius_scale) = if let Some(max_pixels) = max_pixels
    {
        let (width, height) =
            scaled_dimensions(visible_bounds.width, visible_bounds.height, max_pixels);
        if width != visible_bounds.width || height != visible_bounds.height {
            let scaled = DibSurface::new(width, height)?;
            let source_x = visible_bounds.x.saturating_sub(capture_bounds.x);
            let source_y = visible_bounds.y.saturating_sub(capture_bounds.y);
            // SAFETY: Both DIBs are live. Source coordinates are the checked intersection of
            // window and visible-frame bounds, and the destination dimensions are positive.
            let scaled_rendered = unsafe {
                SetStretchBltMode(scaled.device, HALFTONE);
                StretchBlt(
                    scaled.device,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    surface.device,
                    source_x,
                    source_y,
                    visible_bounds.width as i32,
                    visible_bounds.height as i32,
                    SRCCOPY,
                )
            };
            if !scaled_rendered.as_bool() {
                return Err(last_error("scale_window_thumbnail"));
            }
            (
                scaled.copy_pixels(),
                width,
                height,
                width as f32 / visible_bounds.width as f32,
            )
        } else {
            (
                crop_bgra(&surface.copy_pixels(), capture_bounds, visible_bounds)?,
                width,
                height,
                1.0,
            )
        }
    } else {
        (
            crop_bgra(&surface.copy_pixels(), capture_bounds, visible_bounds)?,
            visible_bounds.width,
            visible_bounds.height,
            1.0,
        )
    };
    let inner_corner_radius =
        (window_corner_radius(hwnd) - geometry.border_thickness as f32).max(0.0) * radius_scale;
    let border_width = scaled_border_width(geometry.border_thickness, radius_scale);
    let (pixels, output_width, output_height) = add_clean_window_border(
        pixels,
        content_width,
        content_height,
        border_width,
        inner_corner_radius,
        // GDI never writes the alpha byte, so whatever PrintWindow left behind is meaningless.
        AlphaSource::Coverage,
    )?;
    let corner_radius = inner_corner_radius + border_width as f32;
    let (origin_x, origin_y) = geometry.published_origin(geometry.border_thickness);
    let mut metadata = reference_metadata.clone();
    metadata.backend = "windows-print-window".to_owned();
    metadata.source_rect = Rect {
        x: origin_x,
        y: origin_y,
        width: output_width,
        height: output_height,
    };
    metadata.copy_count = metadata.copy_count.saturating_add(1);
    metadata.pool_slot = None;
    metadata.cursor = Some(window_cursor_outcome(&metadata));
    let frame = CpuFrame::new(
        Arc::from(pixels),
        output_width,
        output_height,
        output_width.saturating_mul(4),
        frozen_format(),
        captastic_core::FrameOrigin::TopLeft,
        captastic_core::ColorSpace::Srgb,
        metadata,
    )
    .map(|frame| frame.with_alpha(FrameAlpha::Straight))
    .map_err(|error| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "build_window_frame",
            error.to_string(),
            false,
            None,
        )
    })?;
    let capture = CapturedWindow {
        frame,
        corner_radius_px: corner_radius,
    };
    log_published_window_frame(handle.raw(), &capture, max_pixels.is_some());
    Ok(capture)
}

/// Records which backend produced a frame that is about to be published.
///
/// The backend name travels with the frame in `FrameMetadata`, but that only helps whoever is
/// holding the frame. The two paths differ in how they crop, how they treat alpha, and how much
/// they copy, and either one can run for any given window depending on how it answered a probe
/// milliseconds earlier — so a capture that comes out misaligned or oddly bright has to be
/// traceable to the path that produced it from the log alone.
fn log_published_window_frame(handle: isize, capture: &CapturedWindow, thumbnail: bool) {
    let metadata = &capture.frame.metadata;
    log::debug!(
        "window handle=0x{handle:X} published a {kind} frame from {backend}: {width}x{height} at ({x},{y}), {alpha:?} alpha, corner radius {radius:.1}px, {copies} copies",
        kind = if thumbnail { "thumbnail" } else { "full-size" },
        backend = metadata.backend,
        width = capture.frame.width(),
        height = capture.frame.height(),
        x = metadata.source_rect.x,
        y = metadata.source_rect.y,
        alpha = capture.frame.alpha(),
        radius = capture.corner_radius_px,
        copies = metadata.copy_count,
    );
}

fn unpremultiply_bgra(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
}

fn window_is_responsive(hwnd: HWND) -> bool {
    let mut ignored_result = 0_usize;
    // SAFETY: WM_NULL carries no pointers. The timeout bounds synchronous work in the target
    // process, and ignored_result remains writable for the duration of the call.
    unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            WINDOW_RESPONSIVENESS_TIMEOUT_MS,
            Some(&mut ignored_result),
        )
    }
    .0 != 0
}

fn capture_window_with_wgc(
    hwnd: HWND,
    reference_metadata: &FrameMetadata,
    max_pixels: Option<u64>,
    geometry: &WindowGeometry,
    deadline: RenderDeadline,
) -> Result<CapturedWindow, CaptureError> {
    let raw = capture_window_wgc_frame(hwnd, deadline)?;
    let source_bounds = Rect {
        x: 0,
        y: 0,
        width: raw.width,
        height: raw.height,
    };
    let WgcCrop {
        crop: content_bounds,
        border_thickness,
        aligned,
    } = wgc_content_crop(raw.width, raw.height, geometry);
    if !aligned {
        log::debug!(
            "window handle=0x{:X} Windows Graphics Capture surface is {}x{}, matching neither the window rect {}x{} nor the visible frame {}x{}; cropping a symmetric {border_thickness}px inset instead",
            hwnd.0,
            raw.width,
            raw.height,
            geometry.window.width,
            geometry.window.height,
            geometry.frame.width,
            geometry.frame.height,
        );
    }
    let content = crop_bgra(&raw.pixels, source_bounds, content_bounds)?;
    let (pixels, content_width, content_height, radius_scale) = if let Some(max_pixels) = max_pixels
    {
        let (width, height) =
            scaled_dimensions(content_bounds.width, content_bounds.height, max_pixels);
        if width != content_bounds.width || height != content_bounds.height {
            (
                scale_bgra_with_dib(
                    &content,
                    content_bounds.width,
                    content_bounds.height,
                    width,
                    height,
                    "scale_wgc_window_thumbnail",
                )?,
                width,
                height,
                width as f32 / content_bounds.width as f32,
            )
        } else {
            (content, width, height, 1.0)
        }
    } else {
        (content, content_bounds.width, content_bounds.height, 1.0)
    };
    let pixels = normalize_wgc_content(pixels);
    let inner_corner_radius =
        (window_corner_radius(hwnd) - border_thickness as f32).max(0.0) * radius_scale;
    let border_width = scaled_border_width(border_thickness, radius_scale);
    let (pixels, output_width, output_height) = add_clean_window_border(
        pixels,
        content_width,
        content_height,
        border_width,
        inner_corner_radius,
        // WGC hands back the composited window surface, so an acrylic or Mica backdrop arrives
        // genuinely translucent. Overwriting that with corner coverage published it opaque.
        AlphaSource::Content,
    )?;
    let corner_radius_px = inner_corner_radius + border_width as f32;
    let (origin_x, origin_y) = geometry.published_origin(border_thickness);
    let mut metadata = reference_metadata.clone();
    metadata.backend = "windows-graphics-capture".to_owned();
    metadata.source_rect = Rect {
        x: origin_x,
        y: origin_y,
        width: output_width,
        height: output_height,
    };
    metadata.copy_count = metadata.copy_count.saturating_add(2);
    metadata.pool_slot = None;
    metadata.cursor = Some(window_cursor_outcome(&metadata));
    let frame = CpuFrame::new(
        Arc::from(pixels),
        output_width,
        output_height,
        output_width.saturating_mul(4),
        frozen_format(),
        captastic_core::FrameOrigin::TopLeft,
        captastic_core::ColorSpace::Srgb,
        metadata,
    )
    .map(|frame| frame.with_alpha(FrameAlpha::Straight))
    .map_err(|error| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "build_wgc_window_frame",
            error.to_string(),
            false,
            None,
        )
    })?;
    let capture = CapturedWindow {
        frame,
        corner_radius_px,
    };
    log_published_window_frame(hwnd.0, &capture, max_pixels.is_some());
    Ok(capture)
}

fn normalize_wgc_content(mut pixels: Vec<u8>) -> Vec<u8> {
    // WGC publishes premultiplied BGRA and Captastic publishes straight alpha. Convert *after* any
    // scaling: premultiplied is the only representation a resampling filter may average directly,
    // and unpremultiplying first zeroes the colour of fully transparent pixels, which a downscale
    // would then bleed as a dark fringe into the rounded corners.
    unpremultiply_bgra(&mut pixels);
    pixels
}

/// Scales a BGRA buffer through GDI, keeping the alpha channel intact.
///
/// `StretchBlt` treats the fourth byte of a 32-bit DIB as padding rather than alpha and is free to
/// discard it — with `HALFTONE` it does. The alpha channel is therefore resampled separately, as a
/// grayscale image pushed through the identical filter, and merged back afterwards so the colour
/// and alpha planes stay aligned.
fn scale_bgra_with_dib(
    pixels: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
    operation: &'static str,
) -> Result<Vec<u8>, CaptureError> {
    let mut scaled = stretch_bgra_with_dib(
        pixels,
        source_width,
        source_height,
        target_width,
        target_height,
        operation,
    )?;
    let alpha_plane: Vec<u8> = pixels
        .chunks_exact(4)
        .flat_map(|pixel| [pixel[3], pixel[3], pixel[3], 255])
        .collect();
    let scaled_alpha = stretch_bgra_with_dib(
        &alpha_plane,
        source_width,
        source_height,
        target_width,
        target_height,
        operation,
    )?;
    for (pixel, alpha) in scaled.chunks_exact_mut(4).zip(scaled_alpha.chunks_exact(4)) {
        pixel[3] = alpha[0];
    }
    Ok(scaled)
}

fn stretch_bgra_with_dib(
    pixels: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
    operation: &'static str,
) -> Result<Vec<u8>, CaptureError> {
    let mut source = DibSurface::new(source_width, source_height)?;
    source.write_pixels(pixels)?;
    let scaled = DibSurface::new(target_width, target_height)?;
    // SAFETY: Both DIBs are live and the source/destination rectangles cover them exactly.
    let scaled_rendered = unsafe {
        SetStretchBltMode(scaled.device, HALFTONE);
        StretchBlt(
            scaled.device,
            0,
            0,
            target_width as i32,
            target_height as i32,
            source.device,
            0,
            0,
            source_width as i32,
            source_height as i32,
            SRCCOPY,
        )
    };
    if !scaled_rendered.as_bool() {
        return Err(last_error(operation));
    }
    Ok(scaled.copy_pixels())
}

pub(crate) fn scaled_dimensions(width: u32, height: u32, max_pixels: u64) -> (u32, u32) {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if pixels <= max_pixels || pixels == 0 || max_pixels == 0 {
        return (width, height);
    }
    let scale = (max_pixels as f64 / pixels as f64).sqrt();
    (
        ((width as f64 * scale).floor() as u32).max(1),
        ((height as f64 * scale).floor() as u32).max(1),
    )
}

#[derive(Debug)]
struct WindowRenderPermit<'a> {
    active: &'a AtomicUsize,
    released: Arc<AtomicBool>,
}

/// A worker-lifetime lease: one of the process's render-worker slots, held for one window.
///
/// Unlike the in-flight permit (reclaimed at the caller's timeout deadline), this budget is only
/// released when the spawned thread itself exits, and a worker blocked inside an unresponsive
/// foreign window procedure may never exit. Pairing the slot with an exclusive claim on the target
/// window is what stops that from being fatal: a wedged window pins one slot forever, but it can
/// never take a second, so eight attempts at one bad window no longer disable window capture for
/// the life of the process.
#[derive(Debug)]
struct WindowRenderBudget<'a> {
    workers: &'a AtomicUsize,
    // Dropped after the slot is returned; ordering between the two does not matter, only that both
    // happen when the worker exits.
    _target: WindowRenderTarget<'a>,
}

/// An exclusive claim on rendering one specific window.
#[derive(Debug)]
struct WindowRenderTarget<'a> {
    claimed: &'a Mutex<BTreeSet<isize>>,
    target: isize,
}

impl WindowRenderBudget<'static> {
    fn acquire(target: isize) -> Result<Self, CaptureError> {
        Self::acquire_from(
            &WINDOW_RENDER_WORKERS,
            &ACTIVE_WINDOW_RENDER_TARGETS,
            MAX_WINDOW_RENDER_WORKERS,
            target,
        )
    }
}

impl<'a> WindowRenderBudget<'a> {
    fn acquire_from(
        workers: &'a AtomicUsize,
        claimed: &'a Mutex<BTreeSet<isize>>,
        limit: usize,
        target: isize,
    ) -> Result<Self, CaptureError> {
        // Claim the window before the slot. Refusing a duplicate costs nothing, and it must happen
        // first or a repeatedly retried hung window still burns a slot per attempt on its way to
        // being rejected. If the slot below is unavailable the claim drops again on the `?`.
        let target = WindowRenderTarget::claim(claimed, target)?;
        let mut current = workers.load(Ordering::Acquire);
        loop {
            if current >= limit {
                // Every slot is taken by a worker that has not exited. Some of them may be wedged
                // in a foreign window procedure and never will, so an immediate retry cannot
                // succeed — this is a distinct condition from transient buffer pressure, and the
                // kind says so rather than inviting a retry that is guaranteed to fail.
                return Err(capture_error(
                    CaptureErrorKind::WorkersExhausted,
                    "start_window_render",
                    format!(
                        "{current} native window-render workers are still running; refusing to create an unbounded thread backlog"
                    ),
                    false,
                    None,
                ));
            }
            match workers.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(Self {
                        workers,
                        _target: target,
                    })
                }
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for WindowRenderBudget<'_> {
    fn drop(&mut self) {
        self.workers.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<'a> WindowRenderTarget<'a> {
    fn claim(claimed: &'a Mutex<BTreeSet<isize>>, target: isize) -> Result<Self, CaptureError> {
        if !lock_render_targets(claimed).insert(target) {
            return Err(capture_error(
                CaptureErrorKind::BufferExhausted,
                "start_window_render",
                format!(
                    "a native render for window handle=0x{target:X} is already running; one window may occupy only one render worker"
                ),
                // Unlike the exhausted worker budget, this really can clear on its own: the
                // render already in flight is usually a different caller's, moments from done.
                true,
                None,
            ));
        }
        Ok(Self { claimed, target })
    }
}

impl Drop for WindowRenderTarget<'_> {
    fn drop(&mut self) {
        lock_render_targets(self.claimed).remove(&self.target);
    }
}

/// A poisoned claim set is still a correct claim set: the guarded `BTreeSet` has no invariant a
/// panicking holder could have broken, and refusing to render anything ever again is worse than
/// whatever the panic was.
fn lock_render_targets(claimed: &Mutex<BTreeSet<isize>>) -> MutexGuard<'_, BTreeSet<isize>> {
    claimed.lock().unwrap_or_else(PoisonError::into_inner)
}

impl WindowRenderPermit<'static> {
    fn acquire() -> Result<Self, CaptureError> {
        Self::acquire_from(&IN_FLIGHT_WINDOW_RENDERS, MAX_IN_FLIGHT_WINDOW_RENDERS)
    }
}

impl<'a> WindowRenderPermit<'a> {
    fn acquire_from(active_count: &'a AtomicUsize, limit: usize) -> Result<Self, CaptureError> {
        let mut active = active_count.load(Ordering::Acquire);
        loop {
            if active >= limit {
                return Err(capture_error(
                    CaptureErrorKind::BufferExhausted,
                    "start_window_render",
                    "too many nonresponsive window renders are still in flight",
                    true,
                    None,
                ));
            }
            match active_count.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(Self {
                        active: active_count,
                        released: Arc::new(AtomicBool::new(false)),
                    });
                }
                Err(current) => active = current,
            }
        }
    }

    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.active.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Clone for WindowRenderPermit<'_> {
    fn clone(&self) -> Self {
        Self {
            active: self.active,
            released: self.released.clone(),
        }
    }
}

impl Drop for WindowRenderPermit<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// The desktop-space rectangles every window capture is derived from.
///
/// Both backends must crop the same `content` rectangle and rebuild the same border ring around
/// it, or the same window comes out with different margins depending on which one ran, and the
/// overlay preview lands a border width away from the frame that is finally published.
struct WindowGeometry {
    /// `GetWindowRect`: the whole window, including the invisible resize frame around it.
    window: Rect,
    /// The DWM extended frame bounds clipped to `window`: the window as the user sees it.
    frame: Rect,
    /// `frame` inset by `border_thickness`: the pixels copied out of a render.
    content: Rect,
    /// The width of the visible DWM border, rebuilt synthetically around `content`.
    border_thickness: u32,
}

impl WindowGeometry {
    /// The desktop-space origin of a published frame — the outer edge of the rebuilt border ring.
    ///
    /// `border_thickness` is a parameter rather than the field because a capture whose surface did
    /// not line up with either known rectangle rebuilds a clamped ring instead.
    fn published_origin(&self, border_thickness: u32) -> (i32, i32) {
        let border = i32::try_from(border_thickness).unwrap_or(i32::MAX);
        (
            self.content.x.saturating_sub(border),
            self.content.y.saturating_sub(border),
        )
    }
}

fn window_geometry(hwnd: HWND, window: Rect) -> WindowGeometry {
    let mut native = RECT::default();
    // SAFETY: hwnd is a live top-level window and native is writable for the exact RECT size.
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut native as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    };
    let Some(dwm_bounds) = result.ok().and_then(|()| rect_from_native(native)) else {
        return WindowGeometry {
            window,
            frame: window,
            content: window,
            border_thickness: 0,
        };
    };
    let frame = intersect_rect(window, dwm_bounds).unwrap_or(window);
    let border_thickness = visible_frame_border_thickness(hwnd)
        .min(frame.width / 4)
        .min(frame.height / 4);
    let content = inset_rect(frame, border_thickness).unwrap_or(frame);
    WindowGeometry {
        window,
        frame,
        content,
        border_thickness,
    }
}

/// Where the shared `content` crop lands inside a raw Windows Graphics Capture surface.
#[derive(Debug, Eq, PartialEq)]
struct WgcCrop {
    /// The crop rectangle in the surface's own coordinates.
    crop: Rect,
    /// The border ring thickness to rebuild around `crop`.
    border_thickness: u32,
    /// Whether the surface lined up with a window rectangle Captastic had already measured.
    ///
    /// False means the crop is a best-effort symmetric inset instead of the geometry PrintWindow
    /// would have produced, which is worth saying out loud in the log.
    aligned: bool,
}

/// Maps the visible-frame crop into the coordinate space of a raw WGC surface.
///
/// WGC hands back a surface sized like one of the two rectangles already measured for the window:
/// the full window rect for windows whose surface includes the invisible resize frame, or the DWM
/// visible frame for those whose surface does not. Matching the size identifies the surface's
/// desktop-space origin, and therefore where the crop PrintWindow performs lands inside it — which
/// is the whole point: the invisible resize-frame delta has to come off both backends or it comes
/// off neither.
fn wgc_content_crop(surface_width: u32, surface_height: u32, geometry: &WindowGeometry) -> WgcCrop {
    let surface = Rect {
        x: 0,
        y: 0,
        width: surface_width,
        height: surface_height,
    };
    for origin in [geometry.window, geometry.frame] {
        if origin.width != surface_width || origin.height != surface_height {
            continue;
        }
        let Some(crop) =
            translate_rect(geometry.content, -i64::from(origin.x), -i64::from(origin.y))
        else {
            continue;
        };
        if rect_contains(surface, crop) {
            return WgcCrop {
                crop,
                border_thickness: geometry.border_thickness,
                aligned: true,
            };
        }
    }
    // The surface matches neither rectangle: the window resized while the capture was in flight,
    // or the compositor handed back a size Captastic cannot place. Nothing maps desktop
    // coordinates onto it, so fall back to a symmetric inset of the surface itself.
    let border_thickness = geometry
        .border_thickness
        .min(surface_width / 4)
        .min(surface_height / 4);
    WgcCrop {
        crop: inset_rect(surface, border_thickness).unwrap_or(surface),
        border_thickness,
        aligned: false,
    }
}

fn translate_rect(rect: Rect, dx: i64, dy: i64) -> Option<Rect> {
    Some(Rect {
        x: i32::try_from(i64::from(rect.x) + dx).ok()?,
        y: i32::try_from(i64::from(rect.y) + dy).ok()?,
        width: rect.width,
        height: rect.height,
    })
}

fn rect_contains(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.right() <= outer.right()
        && inner.bottom() <= outer.bottom()
}

fn visible_frame_border_thickness(hwnd: HWND) -> u32 {
    let mut thickness = 0_u32;
    // SAFETY: thickness is writable storage for the documented UINT attribute and hwnd is live.
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_VISIBLE_FRAME_BORDER_THICKNESS,
            (&mut thickness as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    result.ok().map_or(0, |()| thickness)
}

fn inset_rect(rect: Rect, inset: u32) -> Option<Rect> {
    if inset == 0 {
        return Some(rect);
    }
    let doubled = inset.checked_mul(2)?;
    let width = rect.width.checked_sub(doubled)?;
    let height = rect.height.checked_sub(doubled)?;
    (width != 0 && height != 0).then_some(Rect {
        x: rect.x.checked_add(i32::try_from(inset).ok()?)?,
        y: rect.y.checked_add(i32::try_from(inset).ok()?)?,
        width,
        height,
    })
}

fn intersect_rect(first: Rect, second: Rect) -> Option<Rect> {
    let left = i64::from(first.x).max(i64::from(second.x));
    let top = i64::from(first.y).max(i64::from(second.y));
    let right = (i64::from(first.x) + i64::from(first.width))
        .min(i64::from(second.x) + i64::from(second.width));
    let bottom = (i64::from(first.y) + i64::from(first.height))
        .min(i64::from(second.y) + i64::from(second.height));
    (right > left && bottom > top).then_some(Rect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

fn crop_bgra(source: &[u8], source_bounds: Rect, crop: Rect) -> Result<Vec<u8>, CaptureError> {
    if crop.x < source_bounds.x
        || crop.y < source_bounds.y
        || crop.right() > source_bounds.right()
        || crop.bottom() > source_bounds.bottom()
    {
        return Err(invalid_frame(
            "visible frame crop rect is not contained within the rendered window",
        ));
    }
    let x = u32::try_from(i64::from(crop.x) - i64::from(source_bounds.x))
        .map_err(|_| invalid_frame("visible frame starts outside the rendered window"))?;
    let y = u32::try_from(i64::from(crop.y) - i64::from(source_bounds.y))
        .map_err(|_| invalid_frame("visible frame starts outside the rendered window"))?;
    let source_stride = source_bounds
        .width
        .checked_mul(4)
        .ok_or_else(|| invalid_frame("rendered window stride overflowed"))?
        as usize;
    let destination_stride =
        crop.width
            .checked_mul(4)
            .ok_or_else(|| invalid_frame("visible window stride overflowed"))? as usize;
    let destination_len = destination_stride
        .checked_mul(crop.height as usize)
        .ok_or_else(|| invalid_frame("visible window buffer size overflowed"))?;
    let mut destination = vec![0_u8; destination_len];
    for row in 0..crop.height as usize {
        let source_start = (y as usize + row)
            .checked_mul(source_stride)
            .and_then(|offset| offset.checked_add(x as usize * 4))
            .ok_or_else(|| invalid_frame("visible window crop offset overflowed"))?;
        let destination_start = row * destination_stride;
        destination[destination_start..destination_start + destination_stride]
            .copy_from_slice(&source[source_start..source_start + destination_stride]);
    }
    Ok(destination)
}

/// Returns true when every byte (including alpha) in `region` of a BGRA `source` buffer is zero.
///
/// `source` is expected to have been pre-cleared to zero before the render that produced it, so
/// an all-zero region means the render painted nothing there. A genuinely all-black paint is
/// indistinguishable from this by bytes alone (GDI never sets alpha), but treating it as blank is
/// harmless: falling back to another capture path for a truly black window still yields black
/// pixels. This scans the full region with an early exit per row, which is fine because the check
/// is memory-bandwidth bound rather than CPU bound.
fn region_is_blank(source: &[u8], source_bounds: Rect, region: Rect) -> Result<bool, CaptureError> {
    let x = u32::try_from(region.x - source_bounds.x)
        .map_err(|_| invalid_frame("visible frame starts outside the rendered window"))?;
    let y = u32::try_from(region.y - source_bounds.y)
        .map_err(|_| invalid_frame("visible frame starts outside the rendered window"))?;
    let source_stride = source_bounds
        .width
        .checked_mul(4)
        .ok_or_else(|| invalid_frame("rendered window stride overflowed"))?
        as usize;
    let row_len = region
        .width
        .checked_mul(4)
        .ok_or_else(|| invalid_frame("visible window stride overflowed"))?
        as usize;
    for row in 0..region.height as usize {
        let start = (y as usize + row)
            .checked_mul(source_stride)
            .and_then(|offset| offset.checked_add(x as usize * 4))
            .ok_or_else(|| invalid_frame("visible window crop offset overflowed"))?;
        if source[start..start + row_len].iter().any(|&byte| byte != 0) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The corner radius DWM rounds a window to, in physical source pixels.
///
/// Public to the crate so the chooser can shape a tile's selection outline without capturing the
/// window: this reads DWM attributes and nothing else.
pub(crate) fn window_corner_radius(hwnd: HWND) -> f32 {
    // Maximized windows meet the monitor edges and DWM does not round them.
    // SAFETY: hwnd is the same live top-level window validated for the capture operation.
    if unsafe { IsZoomed(hwnd) }.as_bool() {
        return 0.0;
    }
    let mut preference = DWM_WINDOW_CORNER_PREFERENCE::default();
    // A failed query indicates a pre-Windows 11 DWM or an ineligible/custom window. In both
    // cases, preserving a rectangular shape is safer than inventing transparent corners.
    // SAFETY: preference is writable for the exact type size and hwnd remains live during capture.
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&mut preference as *mut DWM_WINDOW_CORNER_PREFERENCE).cast(),
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    };
    if result.is_err() || preference == DWMWCP_DONOTROUND {
        return 0.0;
    }
    // Default rounding is only inferred for conventionally captioned DWM windows. Borderless
    // and custom-shaped windows must opt in explicitly or their real corner pixels are retained.
    // SAFETY: Reads immutable style bits from the same live top-level window.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
    let has_standard_caption = style & WS_CAPTION.0 != 0;
    let logical_radius = logical_corner_radius(preference, has_standard_caption);
    // SAFETY: hwnd remains a live top-level window for the duration of this capture.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    logical_radius * dpi as f32 / 96.0
}

fn logical_corner_radius(
    preference: DWM_WINDOW_CORNER_PREFERENCE,
    has_standard_caption: bool,
) -> f32 {
    if preference == DWMWCP_ROUNDSMALL {
        4.0
    } else if preference == DWMWCP_ROUND || (preference == DWMWCP_DEFAULT && has_standard_caption) {
        8.0
    } else {
        0.0
    }
}

/// Whether the alpha channel of a rendered window buffer carries information worth keeping.
///
/// The two capture backends disagree, and publishing both through the same mask is what made
/// translucent windows opaque: geometric corner coverage is the *whole* answer for one and only
/// half of it for the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AlphaSource {
    /// The buffer has no meaningful alpha of its own, so the corner mask becomes the alpha.
    ///
    /// GDI never writes the alpha byte, so a `PrintWindow` render leaves whatever the DIB was
    /// cleared to (zero) behind every opaque pixel it painted. The synthetic border ring is
    /// likewise opaque by construction.
    Coverage,
    /// The buffer carries real per-pixel alpha, so the corner mask modulates it.
    ///
    /// Windows Graphics Capture publishes the composited window surface, where an acrylic or Mica
    /// backdrop is genuinely translucent. Replacing that alpha with coverage publishes the
    /// backdrop as if it were opaque, which reads as over-bright against whatever it is composited
    /// onto later.
    Content,
}

impl AlphaSource {
    /// Combines the alpha already stored in a pixel with the geometric corner `coverage`.
    fn combine(self, stored: u8, coverage: u8) -> u8 {
        match self {
            Self::Coverage => coverage,
            // Rounded so a fully opaque source pixel reproduces `coverage` exactly, which keeps
            // the `Coverage` and `Content` masks identical for opaque content.
            Self::Content => ((u32::from(stored) * u32::from(coverage) + 127) / 255) as u8,
        }
    }
}

fn apply_window_alpha(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    radius: f32,
    alpha_source: AlphaSource,
) {
    let stride = width as usize * 4;
    for y in 0..height {
        for x in 0..width {
            let coverage = rounded_rect_coverage(x, y, width, height, radius);
            let offset = y as usize * stride + x as usize * 4;
            let alpha = alpha_source.combine(pixels[offset + 3], coverage);
            pixels[offset + 3] = alpha;
            if alpha == 0 {
                pixels[offset..offset + 3].fill(0);
            }
        }
    }
}

fn scaled_border_width(border_width: u32, scale: f32) -> u32 {
    if border_width == 0 {
        0
    } else {
        (border_width as f32 * scale).round().max(1.0) as u32
    }
}

fn add_clean_window_border(
    mut content: Vec<u8>,
    content_width: u32,
    content_height: u32,
    border_width: u32,
    inner_corner_radius: f32,
    alpha_source: AlphaSource,
) -> Result<(Vec<u8>, u32, u32), CaptureError> {
    let content_stride = content_width
        .checked_mul(4)
        .ok_or_else(|| invalid_frame("window content stride overflowed"))?
        as usize;
    let expected_length = content_stride
        .checked_mul(content_height as usize)
        .ok_or_else(|| invalid_frame("window content buffer size overflowed"))?;
    if content.len() != expected_length {
        return Err(invalid_frame("window content buffer length is invalid"));
    }
    if border_width == 0 {
        apply_window_alpha(
            &mut content,
            content_width,
            content_height,
            inner_corner_radius,
            alpha_source,
        );
        return Ok((content, content_width, content_height));
    }

    let doubled_border = border_width
        .checked_mul(2)
        .ok_or_else(|| invalid_frame("window border width overflowed"))?;
    let width = content_width
        .checked_add(doubled_border)
        .ok_or_else(|| invalid_frame("bordered window width overflowed"))?;
    let height = content_height
        .checked_add(doubled_border)
        .ok_or_else(|| invalid_frame("bordered window height overflowed"))?;
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| invalid_frame("bordered window stride overflowed"))?
        as usize;
    let length = stride
        .checked_mul(height as usize)
        .ok_or_else(|| invalid_frame("bordered window buffer size overflowed"))?;
    let mut framed = vec![0_u8; length];
    let outer_corner_radius = inner_corner_radius + border_width as f32;
    for pixel in framed.chunks_exact_mut(4) {
        pixel[..3].copy_from_slice(&WINDOW_BORDER_BGRA);
        pixel[3] = 255;
    }
    // The ring is synthetic and opaque, so its own mask never has content alpha to preserve.
    apply_window_alpha(
        &mut framed,
        width,
        height,
        outer_corner_radius,
        AlphaSource::Coverage,
    );

    for content_y in 0..content_height {
        for content_x in 0..content_width {
            // `coverage` is *area*: how much of this output pixel the rounded content rectangle
            // occupies, with the border ring occupying the rest. It is deliberately kept separate
            // from the content's own transparency below, because the two combine differently —
            // area splits the pixel between content and ring, transparency does not.
            let coverage = rounded_rect_coverage(
                content_x,
                content_y,
                content_width,
                content_height,
                inner_corner_radius,
            );
            if coverage == 0 {
                continue;
            }
            let source_offset = content_y as usize * content_stride + content_x as usize * 4;
            let output_x = content_x + border_width;
            let output_y = content_y + border_width;
            let output_offset = output_y as usize * stride + output_x as usize * 4;
            let source_alpha = alpha_source.combine(content[source_offset + 3], 255);
            if coverage == 255 {
                framed[output_offset..output_offset + 3]
                    .copy_from_slice(&content[source_offset..source_offset + 3]);
                framed[output_offset + 3] = source_alpha;
                continue;
            }

            let content_alpha = u32::from(alpha_source.combine(source_alpha, coverage));
            let background_alpha = u32::from(framed[output_offset + 3]);
            let uncovered = 255 - u32::from(coverage);
            let retained_background_alpha = (background_alpha * uncovered + 127) / 255;
            let output_alpha = content_alpha + retained_background_alpha;
            if output_alpha == 0 {
                framed[output_offset..output_offset + 4].fill(0);
                continue;
            }
            for channel in 0..3 {
                let premultiplied = u32::from(content[source_offset + channel]) * content_alpha
                    + u32::from(framed[output_offset + channel]) * retained_background_alpha;
                framed[output_offset + channel] =
                    ((premultiplied + output_alpha / 2) / output_alpha) as u8;
            }
            framed[output_offset + 3] = output_alpha as u8;
        }
    }
    Ok((framed, width, height))
}

fn rounded_rect_coverage(x: u32, y: u32, width: u32, height: u32, radius: f32) -> u8 {
    if radius <= 0.0 {
        return 255;
    }
    let radius = radius.min(width as f32 / 2.0).min(height as f32 / 2.0);
    let in_corner_x = x as f32 + 1.0 <= radius || x as f32 >= width as f32 - radius;
    let in_corner_y = y as f32 + 1.0 <= radius || y as f32 >= height as f32 - radius;
    if !in_corner_x || !in_corner_y {
        return 255;
    }
    let center_x = if x as f32 + 1.0 <= radius {
        radius
    } else {
        width as f32 - radius
    };
    let center_y = if y as f32 + 1.0 <= radius {
        radius
    } else {
        height as f32 - radius
    };
    let mut covered = 0_u8;
    const SAMPLES: u8 = 8;
    for sample_y in 0..SAMPLES {
        for sample_x in 0..SAMPLES {
            let px = x as f32 + (sample_x as f32 + 0.5) / SAMPLES as f32;
            let py = y as f32 + (sample_y as f32 + 0.5) / SAMPLES as f32;
            let dx = px - center_x;
            let dy = py - center_y;
            if dx * dx + dy * dy <= radius * radius {
                covered += 1;
            }
        }
    }
    ((u16::from(covered) * 255 + 32) / 64) as u8
}

const fn frozen_format() -> captastic_core::PixelFormat {
    captastic_core::PixelFormat::Bgra8Unorm
}

struct DibSurface {
    device: HDC,
    bitmap: HBITMAP,
    previous_bitmap: HGDIOBJ,
    bits: *mut u8,
    byte_length: usize,
}

impl DibSurface {
    fn new(width: u32, height: u32) -> Result<Self, CaptureError> {
        let width_i32 = i32::try_from(width)
            .map_err(|_| invalid_frame("selected window width exceeds Win32 bitmap limits"))?;
        let height_i32 = i32::try_from(height)
            .map_err(|_| invalid_frame("selected window height exceeds Win32 bitmap limits"))?;
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| invalid_frame("selected window row size overflowed"))?;
        let byte_length = usize::try_from(stride)
            .ok()
            .and_then(|value| value.checked_mul(height as usize))
            .ok_or_else(|| invalid_frame("selected window bitmap size overflowed"))?;
        // SAFETY: A null source requests a screen-compatible memory device context.
        let device = unsafe { CreateCompatibleDC(None) };
        if device.0 == 0 {
            return Err(last_error("create_window_capture_dc"));
        }
        let bitmap_info = top_down_bitmap_info(width_i32, height_i32, stride, height);
        let mut bits = std::ptr::null_mut();
        // SAFETY: bitmap_info and the out pointer are valid; a null section requests owned memory.
        let bitmap = match unsafe {
            CreateDIBSection(device, &bitmap_info, DIB_RGB_COLORS, &mut bits, None, 0)
        } {
            Ok(bitmap) => bitmap,
            Err(error) => {
                // SAFETY: Releases the memory DC created immediately above.
                unsafe { DeleteDC(device) };
                return Err(windows_error("create_window_capture_dib", error, true));
            }
        };
        if bits.is_null() {
            // SAFETY: Neither object has escaped or been selected into another context.
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            return Err(invalid_frame(
                "selected window DIB returned no writable pixels",
            ));
        }
        // SAFETY: Selects the new DIB into the memory DC used by PrintWindow.
        let previous_bitmap = unsafe { SelectObject(device, bitmap) };
        if previous_bitmap.0 == 0 || previous_bitmap.0 == -1 {
            // SAFETY: Selection failed, so the newly created objects can be released directly.
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            return Err(last_error("select_window_capture_dib"));
        }
        Ok(Self {
            device,
            bitmap,
            previous_bitmap,
            bits: bits.cast(),
            byte_length,
        })
    }

    fn clear(&mut self) {
        // SAFETY: bits addresses byte_length writable bytes owned by the selected DIB.
        unsafe { std::ptr::write_bytes(self.bits, 0, self.byte_length) };
    }

    fn write_pixels(&mut self, pixels: &[u8]) -> Result<(), CaptureError> {
        if pixels.len() != self.byte_length {
            return Err(invalid_frame("window DIB pixel buffer length is invalid"));
        }
        // SAFETY: bits addresses exactly byte_length writable bytes owned by this selected DIB.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), self.bits, self.byte_length) };
        Ok(())
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: Flushes this thread's queued GDI drawing before CPU reads the DIB section.
        let _ = unsafe { GdiFlush() };
        // SAFETY: The DIB owns byte_length initialized bytes and stays alive for the lifetime of &self.
        unsafe { std::slice::from_raw_parts(self.bits, self.byte_length) }
    }

    fn copy_pixels(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        // SAFETY: Restores the original bitmap before deleting the selected bitmap and memory DC.
        unsafe {
            SelectObject(self.device, self.previous_bitmap);
            DeleteObject(self.bitmap);
            DeleteDC(self.device);
        }
    }
}

fn top_down_bitmap_info(width: i32, height: i32, stride: u32, rows: u32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: stride.saturating_mul(rows),
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    }
}

fn rect_from_native(native: RECT) -> Option<Rect> {
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

fn invalid_frame(message: impl Into<String>) -> CaptureError {
    capture_error(
        CaptureErrorKind::InvalidFrame,
        "validate_window_capture",
        message,
        false,
        None,
    )
}

fn last_error(operation: &'static str) -> CaptureError {
    windows_error(operation, WindowsError::from_win32(), true)
}

fn windows_error(operation: &'static str, error: WindowsError, retryable: bool) -> CaptureError {
    session_explained_error(operation, error, retryable, crate::session::desktop_state)
}

/// Maps a window-capture failure, asking the session about it only when it was denied.
///
/// The same check `window_capture_wgc` does, for the same reason: this backend had none anywhere,
/// so a secure desktop refusing `GetWindowRect` on the selected window, or refusing the
/// screen-compatible device context `PrintWindow` renders into, produced `NativeFailure in
/// windows-window-capture/<operation>: Access is denied.` and said nothing about the desktop. Both
/// window backends are reached from the same overlay selection, so explaining only one of them
/// would leave the bare denial reachable by whichever route the fallback happened to take.
///
/// Everything the gate in [`crate::session::denied_by_session`] guarantees applies here too, and
/// one consequence is worth naming: the GDI scaling operations in this file also map through this
/// function, and they are untouched by it, because a `StretchBlt` between two memory DIBs does not
/// fail with `E_ACCESSDENIED` and the session is never asked about a failure that is not a denial.
/// A denial the session cannot account for keeps its kind, message, native code and `retryable`
/// flag — which is what keeps the ordinary refusals this backend already expects, such as
/// `PrintWindow` across the normal-to-elevated integrity boundary, reported as themselves.
fn session_explained_error(
    operation: &'static str,
    error: WindowsError,
    retryable: bool,
    probe_session: impl FnOnce() -> crate::session::DesktopState,
) -> CaptureError {
    crate::session::denied_by_session(
        WINDOW_CAPTURE_BACKEND,
        operation,
        error.code().0,
        probe_session,
    )
    .unwrap_or_else(|| {
        capture_error(
            CaptureErrorKind::NativeFailure,
            operation,
            error.to_string(),
            retryable,
            Some(i64::from(error.code().0)),
        )
    })
}

/// The backend name every window-capture failure reports itself under, in the log and in these
/// tests. A session-explained denial keeps it, so the operation a reader greps for does not move
/// when the explanation is added.
const WINDOW_CAPTURE_BACKEND: &str = "windows-window-capture";

fn capture_error(
    kind: CaptureErrorKind,
    operation: &'static str,
    message: impl Into<String>,
    retryable: bool,
    native_code: Option<i64>,
) -> CaptureError {
    CaptureError {
        kind,
        backend: WINDOW_CAPTURE_BACKEND,
        operation,
        message: message.into(),
        retryable,
        native_code,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;
    use crate::session::{DesktopState, HRESULT_ACCESS_DENIED};
    use captastic_core::{
        CaptureId, CaptureMode, ColorSpace, DisplayId, FrameOrigin, PixelFormat, TimingProvenance,
    };
    use windows::core::HRESULT;

    /// The refusal a secure desktop gives a window query, as Windows reports it.
    fn access_denied() -> WindowsError {
        WindowsError::from(HRESULT(HRESULT_ACCESS_DENIED))
    }

    /// A lock refusing the selected window's bounds has to say so.
    ///
    /// `GetWindowRect` is the first thing this backend asks about the window the user picked, and a
    /// desktop this process cannot read refuses it. Before this it reported `NativeFailure in
    /// windows-window-capture/get_selected_window_bounds: Access is denied.`, with nothing to say
    /// the desktop was the reason. The operation and backend are what a log reader greps for and
    /// neither moves.
    #[test]
    fn a_locked_session_explains_a_refused_window_query() {
        let denied =
            session_explained_error("get_selected_window_bounds", access_denied(), true, || {
                DesktopState::Locked {
                    desktop: Some("Winlogon".to_owned()),
                }
            });
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
        assert!(
            denied.message.contains("locked"),
            "the whole point is that the message says so: {denied}"
        );
        assert!(denied.message.contains("Winlogon"), "{denied}");
        assert_eq!(denied.backend, WINDOW_CAPTURE_BACKEND);
        assert_eq!(denied.operation, "get_selected_window_bounds");
        assert!(denied.retryable);
    }

    /// Every other temporary session state explains it too, because every one of them refuses.
    #[test]
    fn any_session_that_owns_no_desktop_explains_a_refused_window_query() {
        for state in [
            DesktopState::Locked { desktop: None },
            DesktopState::NotOurs {
                desktop: Some("Screen-saver".to_owned()),
            },
            DesktopState::Detached {
                connect_state: "disconnected",
            },
            DesktopState::Remote {
                protocol: "Remote Desktop",
            },
        ] {
            let denied =
                session_explained_error("create_window_capture_dc", access_denied(), true, || {
                    state.clone()
                });
            assert_eq!(
                denied.kind,
                CaptureErrorKind::DesktopUnavailable,
                "{state:?} should explain the refusal"
            );
            assert_eq!(denied.message, state.to_string());
            assert_eq!(denied.operation, "create_window_capture_dc");
        }
    }

    /// A refusal the session cannot account for stays exactly what it was.
    ///
    /// Window capture is refused for ordinary reasons all the time — a window owned by a
    /// higher-integrity process is the reason the Windows Graphics Capture fallback exists — and
    /// those refusals have to keep saying what they are. The caller's own `retryable` choice
    /// survives with the rest of the error.
    #[test]
    fn an_interactive_session_leaves_a_refused_window_query_alone() {
        for state in [
            DesktopState::Interactive,
            DesktopState::Unknown {
                detail: "the input desktop could not be named".to_owned(),
            },
        ] {
            let denied = session_explained_error(
                "get_selected_window_bounds",
                access_denied(),
                false,
                || state.clone(),
            );
            assert_eq!(
                denied.kind,
                CaptureErrorKind::NativeFailure,
                "{state:?} does not explain a refusal and must not hide one"
            );
            assert_eq!(denied.message, access_denied().to_string());
            assert_eq!(denied.native_code, Some(i64::from(HRESULT_ACCESS_DENIED)));
            assert_eq!(denied.backend, WINDOW_CAPTURE_BACKEND);
            assert_eq!(denied.operation, "get_selected_window_bounds");
            assert!(!denied.retryable, "the call site's own choice, preserved");
        }
    }

    /// The session probe costs four syscalls, and this mapper also serves the GDI scaling path. It
    /// may only be paid on the one failure it can explain.
    ///
    /// `scale_window_thumbnail` and the DIB stretches behind it fail between two blocks of this
    /// process's own memory. There is nothing for a session to say about one, and the counter here
    /// is what proves it is never asked.
    #[test]
    fn only_a_refused_window_query_pays_for_the_session_probe() {
        let probes = std::cell::Cell::new(0_u32);
        let probe = || {
            probes.set(probes.get() + 1);
            DesktopState::Locked { desktop: None }
        };
        let out_of_memory = WindowsError::from(HRESULT(0x8007_000E_u32 as i32));
        let failed =
            session_explained_error("scale_window_thumbnail", out_of_memory.clone(), true, probe);
        assert_eq!(probes.get(), 0, "a non-denial must not ask the session");
        assert_eq!(failed.kind, CaptureErrorKind::NativeFailure);
        assert_eq!(failed.message, out_of_memory.to_string());
        assert_eq!(failed.native_code, Some(i64::from(0x8007_000E_u32 as i32)));

        let denied =
            session_explained_error("get_selected_window_bounds", access_denied(), true, || {
                probes.set(probes.get() + 1);
                DesktopState::Locked { desktop: None }
            });
        assert_eq!(probes.get(), 1, "a denial asks the session exactly once");
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
    }

    #[test]
    fn native_window_bounds_are_normalized() {
        assert_eq!(
            rect_from_native(RECT {
                left: -100,
                top: 20,
                right: 500,
                bottom: 420,
            }),
            Some(Rect {
                x: -100,
                y: 20,
                width: 600,
                height: 400,
            })
        );
    }

    #[test]
    fn inner_waits_are_bounded_by_what_the_caller_can_still_use() {
        let ms = Duration::from_millis;
        // (preferred, remaining, reserve) -> effective wait
        let cases = [
            // Plenty of deadline left: the backend keeps its own preferred bound.
            ((ms(300), ms(700), ms(200)), ms(300)),
            ((ms(300), ms(500), ms(200)), ms(300)),
            // Squeezed: the caller's remaining time wins, minus what publishing still costs.
            ((ms(300), ms(400), ms(200)), ms(200)),
            ((ms(250), ms(300), ms(100)), ms(200)),
            // Exactly enough to publish and no more, and past that point.
            ((ms(300), ms(200), ms(200)), Duration::ZERO),
            ((ms(300), ms(50), ms(200)), Duration::ZERO),
            ((ms(300), Duration::ZERO, ms(200)), Duration::ZERO),
        ];
        for ((preferred, remaining, reserve), expected) in cases {
            assert_eq!(
                bounded_wait(preferred, remaining, reserve),
                expected,
                "preferred={preferred:?} remaining={remaining:?} reserve={reserve:?}"
            );
        }
    }

    #[test]
    fn the_whole_wgc_budget_fits_inside_the_outer_render_deadline() {
        // The failure this guards: 300 ms first-frame wait plus 250 ms map retry, sized without
        // reference to the 700 ms outer deadline, could outlast a device creation slow enough to
        // matter — and the frame it finally produced was thrown away, having consumed a
        // worker-lifetime slot for nothing.
        let first_frame = Duration::from_millis(300);
        let map = Duration::from_millis(250);
        let readback_reserve = Duration::from_millis(200);
        let publish_reserve = Duration::from_millis(100);
        for device_creation_ms in [0, 50, 100, 200, 400, 600, 700, 900] {
            let after_device =
                WINDOW_RENDER_TIMEOUT.saturating_sub(Duration::from_millis(device_creation_ms));
            let first_frame_wait = bounded_wait(first_frame, after_device, readback_reserve);
            let after_first_frame = after_device.saturating_sub(first_frame_wait);
            let map_wait = bounded_wait(map, after_first_frame, publish_reserve);
            let waited = first_frame_wait + map_wait;
            assert!(
                waited <= after_device,
                "device creation of {device_creation_ms} ms left {after_device:?} but the inner waits still asked for {waited:?}"
            );
            // ...and whenever the path runs at all, the readback and the border rebuild that
            // follow the last wait still have room before the caller stops listening.
            assert!(
                first_frame_wait.is_zero() || after_device - waited >= publish_reserve,
                "device creation of {device_creation_ms} ms left no room to publish"
            );
        }
    }

    #[test]
    fn an_expired_deadline_offers_no_wait_at_all() {
        let expired = RenderDeadline::starting_now(Duration::ZERO);
        assert_eq!(expired.remaining(), Duration::ZERO);
        assert_eq!(
            expired.bounded_wait(Duration::from_millis(300), Duration::from_millis(200)),
            Duration::ZERO
        );

        let fresh = RenderDeadline::starting_now(WINDOW_RENDER_TIMEOUT);
        assert!(fresh.remaining() > Duration::ZERO);
        assert!(fresh.remaining() <= WINDOW_RENDER_TIMEOUT);
    }

    #[test]
    fn render_permit_deadline_reclaims_capacity_idempotently() {
        let active = AtomicUsize::new(0);
        let first = WindowRenderPermit::acquire_from(&active, 2).expect("first permit");
        let first_worker = first.clone();
        let second = WindowRenderPermit::acquire_from(&active, 2).expect("second permit");
        let exhausted = WindowRenderPermit::acquire_from(&active, 2).expect_err("capacity bound");
        assert_eq!(exhausted.kind, CaptureErrorKind::BufferExhausted);
        assert_eq!(active.load(Ordering::Acquire), 2);

        first.release();
        assert_eq!(active.load(Ordering::Acquire), 1);
        let replacement =
            WindowRenderPermit::acquire_from(&active, 2).expect("deadline reclaimed permit");
        assert_eq!(active.load(Ordering::Acquire), 2);

        drop(first);
        drop(first_worker);
        assert_eq!(
            active.load(Ordering::Acquire),
            2,
            "release must be idempotent"
        );
        drop(second);
        drop(replacement);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn bounded_render_timeout_reclaims_capacity_before_late_completion() {
        let active = Box::leak(Box::new(AtomicUsize::new(0)));
        let workers = Box::leak(Box::new(AtomicUsize::new(0)));
        let claimed = Box::leak(Box::new(Mutex::new(BTreeSet::new())));
        let permit = WindowRenderPermit::acquire_from(active, 1).expect("render permit");
        let budget =
            WindowRenderBudget::acquire_from(workers, claimed, 1, 0x11).expect("worker budget");
        let (release_sender, release_receiver) = mpsc::sync_channel(0);

        let error = run_bounded_window_render(
            permit,
            budget,
            RenderDeadline::starting_now(Duration::ZERO),
            move || {
                release_receiver.recv().expect("release late worker");
                Ok(7_u8)
            },
        )
        .expect_err("worker must exceed the immediate deadline");

        assert_eq!(error.kind, CaptureErrorKind::Timeout);
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert_eq!(workers.load(Ordering::Acquire), 1);
        assert!(WindowRenderBudget::acquire_from(workers, claimed, 1, 0x22).is_err());
        let replacement =
            WindowRenderPermit::acquire_from(active, 1).expect("deadline reclaimed capacity");
        release_sender.send(()).expect("release detached worker");
        drop(replacement);
        assert_eq!(active.load(Ordering::Acquire), 0);
        // The released worker still has to be scheduled before it drops its budget lease. Wait on
        // wall-clock time, not a fixed yield count, so a preempted worker on a loaded CI runner
        // cannot outlive the wait; the deadline only bounds a genuinely stuck release.
        let release_deadline = Instant::now() + Duration::from_secs(10);
        while workers.load(Ordering::Acquire) != 0 && Instant::now() < release_deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(workers.load(Ordering::Acquire), 0);
    }

    #[test]
    fn bounded_render_worker_panic_is_a_non_retryable_native_failure() {
        let active = Box::leak(Box::new(AtomicUsize::new(0)));
        let workers = Box::leak(Box::new(AtomicUsize::new(0)));
        let claimed = Box::leak(Box::new(Mutex::new(BTreeSet::new())));
        let permit = WindowRenderPermit::acquire_from(active, 1).expect("render permit");
        let budget =
            WindowRenderBudget::acquire_from(workers, claimed, 1, 0x11).expect("worker budget");

        // The worker terminates promptly by panicking, so this timeout only bounds a broken
        // harness. Keep it generous: a 1-second budget lost the race to thread spawn plus unwind
        // on coverage-instrumented CI runners and misreported the panic as a Timeout.
        let error = run_bounded_window_render::<u8>(
            permit,
            budget,
            RenderDeadline::starting_now(Duration::from_secs(60)),
            || panic!("scripted render panic"),
        )
        .expect_err("panic disconnects the bounded worker");

        assert_eq!(error.kind, CaptureErrorKind::NativeFailure);
        assert!(!error.retryable);
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert_eq!(workers.load(Ordering::Acquire), 0);
    }

    #[test]
    fn native_render_worker_budget_is_a_strict_hard_cap() {
        let workers = AtomicUsize::new(0);
        let claimed = Mutex::new(BTreeSet::new());
        let first =
            WindowRenderBudget::acquire_from(&workers, &claimed, 2, 0x11).expect("first worker");
        let second =
            WindowRenderBudget::acquire_from(&workers, &claimed, 2, 0x22).expect("second worker");

        let error =
            WindowRenderBudget::acquire_from(&workers, &claimed, 2, 0x33).expect_err("worker cap");
        assert_eq!(error.kind, CaptureErrorKind::WorkersExhausted);
        assert!(error.message.contains("unbounded thread backlog"));
        // The budget is only reclaimed when a worker thread exits, so an immediate retry cannot
        // succeed while the cap is held; unlike the in-flight permit, this must not be retryable.
        assert!(!error.retryable);
        // The rejected attempt must not leave its window claimed behind, or one exhausted moment
        // would lock that window out until the process restarts.
        drop(error);
        drop(first);
        let replacement = WindowRenderBudget::acquire_from(&workers, &claimed, 2, 0x33)
            .expect("released slot, and 0x33 was never left claimed");
        drop(second);
        drop(replacement);
        assert_eq!(workers.load(Ordering::Acquire), 0);
        assert!(lock_render_targets(&claimed).is_empty());
    }

    #[test]
    fn one_window_can_never_occupy_more_than_one_render_worker() {
        // A window wedged inside its own window procedure holds its slot until the process exits.
        // Eight retries against that one window used to consume the entire worker budget and
        // disable window capture process-wide; now the second attempt is refused for free.
        let workers = AtomicUsize::new(0);
        let claimed = Mutex::new(BTreeSet::new());
        let wedged = WindowRenderBudget::acquire_from(&workers, &claimed, 8, 0xBAD)
            .expect("first render of the wedged window");

        for attempt in 0..8 {
            let error = WindowRenderBudget::acquire_from(&workers, &claimed, 8, 0xBAD)
                .expect_err("a repeat attempt must not take a second slot");
            assert_eq!(error.kind, CaptureErrorKind::BufferExhausted);
            // The render already in flight is often another caller's and moments from finishing,
            // so this one really can clear on its own.
            assert!(error.retryable, "attempt {attempt}");
            assert_eq!(
                workers.load(Ordering::Acquire),
                1,
                "attempt {attempt} consumed a worker slot"
            );
        }

        // Every other window is unaffected, which is the whole point.
        let healthy = WindowRenderBudget::acquire_from(&workers, &claimed, 8, 0x600D)
            .expect("an unrelated window still renders");
        assert_eq!(workers.load(Ordering::Acquire), 2);

        drop(healthy);
        drop(wedged);
        assert_eq!(workers.load(Ordering::Acquire), 0);
        assert!(lock_render_targets(&claimed).is_empty());
    }

    #[test]
    fn a_finished_render_releases_its_window_for_the_next_one() {
        let workers = AtomicUsize::new(0);
        let claimed = Mutex::new(BTreeSet::new());
        let first = WindowRenderBudget::acquire_from(&workers, &claimed, 8, 0x11).expect("first");
        drop(first);
        let second = WindowRenderBudget::acquire_from(&workers, &claimed, 8, 0x11)
            .expect("the same window renders again once its worker has exited");
        drop(second);
        assert!(lock_render_targets(&claimed).is_empty());
    }

    #[test]
    fn detached_render_telemetry_handles_completion_races() {
        let ledger = DetachLedger::new();
        let timed_out = AtomicU8::new(RENDER_ACTIVE);
        assert_eq!(
            mark_render_detached(&timed_out, &ledger),
            Some(DetachCount { live: 1, total: 1 })
        );
        complete_render(&timed_out, &ledger);
        assert_eq!(
            ledger.count(DetachKind::WindowRender),
            DetachCount { live: 0, total: 1 }
        );

        // The worker finished a moment before the caller gave up on it. Nothing was detached, so
        // the total must not move either - this is the count that says how often detaching really
        // happens, and a race the worker won is not an instance of it.
        let completed_first = AtomicU8::new(RENDER_ACTIVE);
        complete_render(&completed_first, &ledger);
        assert!(mark_render_detached(&completed_first, &ledger).is_none());
        assert_eq!(
            ledger.count(DetachKind::WindowRender),
            DetachCount { live: 0, total: 1 }
        );
    }

    #[test]
    fn visible_frame_crop_removes_the_outer_window_margins() {
        let source_bounds = Rect {
            x: 10,
            y: 20,
            width: 4,
            height: 3,
        };
        let mut source = Vec::new();
        for pixel in 0_u8..12 {
            source.extend_from_slice(&[pixel, pixel, pixel, 0]);
        }
        let cropped = crop_bgra(
            &source,
            source_bounds,
            Rect {
                x: 11,
                y: 21,
                width: 2,
                height: 2,
            },
        )
        .expect("valid visible bounds");
        assert_eq!(cropped, [5, 5, 5, 0, 6, 6, 6, 0, 9, 9, 9, 0, 10, 10, 10, 0]);
    }

    #[test]
    fn crop_touching_the_source_edges_exactly_succeeds() {
        let source_bounds = Rect {
            x: 10,
            y: 20,
            width: 4,
            height: 3,
        };
        let source = vec![0_u8; 4 * 3 * 4];
        let cropped = crop_bgra(&source, source_bounds, source_bounds)
            .expect("a crop equal to the full source bounds is contained");
        assert_eq!(cropped.len(), source.len());
    }

    #[test]
    fn crop_exceeding_the_source_width_by_one_is_rejected() {
        let source_bounds = Rect {
            x: 10,
            y: 20,
            width: 4,
            height: 3,
        };
        let source = vec![0_u8; 4 * 3 * 4];
        let crop = Rect {
            x: 11,
            width: 4,
            ..source_bounds
        };
        let error = crop_bgra(&source, source_bounds, crop)
            .expect_err("a crop reaching past the right edge must not panic on a bad slice");
        assert!(!error.retryable);
    }

    #[test]
    fn crop_exceeding_the_source_height_by_one_is_rejected() {
        let source_bounds = Rect {
            x: 10,
            y: 20,
            width: 4,
            height: 3,
        };
        let source = vec![0_u8; 4 * 3 * 4];
        let crop = Rect {
            y: 21,
            height: 3,
            ..source_bounds
        };
        let error = crop_bgra(&source, source_bounds, crop)
            .expect_err("a crop reaching past the bottom edge must not panic on a bad slice");
        assert!(!error.retryable);
    }

    #[test]
    fn crop_starting_before_the_source_origin_is_rejected() {
        let source_bounds = Rect {
            x: 10,
            y: 20,
            width: 4,
            height: 3,
        };
        let source = vec![0_u8; 4 * 3 * 4];
        let crop = Rect {
            x: 9,
            ..source_bounds
        };
        crop_bgra(&source, source_bounds, crop)
            .expect_err("a crop starting outside the source bounds must be rejected");
    }

    #[test]
    fn zero_area_crop_returns_an_empty_buffer_without_panicking() {
        let source_bounds = Rect {
            x: 10,
            y: 20,
            width: 4,
            height: 3,
        };
        let source = vec![0_u8; 4 * 3 * 4];
        let crop = Rect {
            x: 11,
            y: 21,
            width: 0,
            height: 0,
        };
        let cropped =
            crop_bgra(&source, source_bounds, crop).expect("a zero-area crop is still contained");
        assert!(cropped.is_empty());
    }

    #[test]
    fn blank_region_detection_ignores_bytes_outside_the_visible_crop() {
        let source_bounds = Rect {
            x: 10,
            y: 20,
            width: 4,
            height: 3,
        };
        // A window whose only non-zero pixel sits in the outer margin that visible_bounds crops
        // away: the inner 2x2 region PrintWindow was supposed to paint stayed untouched.
        let mut source = vec![0_u8; 4 * 3 * 4];
        source[0] = 7;
        let region = Rect {
            x: 11,
            y: 21,
            width: 2,
            height: 2,
        };
        assert!(region_is_blank(&source, source_bounds, region).expect("valid region"));
    }

    #[test]
    fn blank_region_detection_flags_any_painted_pixel_in_the_crop() {
        let source_bounds = Rect {
            x: 10,
            y: 20,
            width: 4,
            height: 3,
        };
        let mut source = vec![0_u8; 4 * 3 * 4];
        // Non-zero alpha (no color) still counts as painted content, not a blank render.
        let (row, column) = (1_usize, 1_usize);
        let painted_offset = (row * 4 + column) * 4 + 3;
        source[painted_offset] = 255;
        let region = Rect {
            x: 11,
            y: 21,
            width: 2,
            height: 2,
        };
        assert!(!region_is_blank(&source, source_bounds, region).expect("valid region"));
    }

    /// A resizable window: `GetWindowRect` reports an 8px invisible resize frame around the
    /// visible DWM frame, which itself carries a 2px visible border.
    fn resizable_window_geometry() -> WindowGeometry {
        let window = Rect {
            x: 100,
            y: 200,
            width: 816,
            height: 616,
        };
        let frame = Rect {
            x: 108,
            y: 200,
            width: 800,
            height: 608,
        };
        WindowGeometry {
            window,
            frame,
            content: inset_rect(frame, 2).expect("the visible border fits inside the frame"),
            border_thickness: 2,
        }
    }

    #[test]
    fn wgc_surface_matching_the_window_rect_crops_the_invisible_resize_frame() {
        let geometry = resizable_window_geometry();
        // Same crop PrintWindow performs, expressed in the surface's own coordinates: 8px of
        // invisible resize frame on the left plus the 2px visible border.
        assert_eq!(
            wgc_content_crop(geometry.window.width, geometry.window.height, &geometry),
            WgcCrop {
                crop: Rect {
                    x: 10,
                    y: 2,
                    width: 796,
                    height: 604,
                },
                border_thickness: 2,
                aligned: true,
            }
        );
    }

    #[test]
    fn wgc_surface_matching_the_visible_frame_crops_only_the_border() {
        let geometry = resizable_window_geometry();
        // A surface that already excludes the invisible resize frame must not have it removed
        // twice; only the visible border comes off.
        assert_eq!(
            wgc_content_crop(geometry.frame.width, geometry.frame.height, &geometry),
            WgcCrop {
                crop: Rect {
                    x: 2,
                    y: 2,
                    width: 796,
                    height: 604,
                },
                border_thickness: 2,
                aligned: true,
            }
        );
    }

    #[test]
    fn both_backends_publish_the_same_origin_and_size_for_one_window() {
        let geometry = resizable_window_geometry();
        for (surface_width, surface_height) in [
            (geometry.window.width, geometry.window.height),
            (geometry.frame.width, geometry.frame.height),
        ] {
            let wgc = wgc_content_crop(surface_width, surface_height, &geometry);
            // The rebuilt border ring restores exactly what each backend cropped away, so the
            // published frame is the visible DWM frame either way.
            assert_eq!(
                (
                    wgc.crop.width + wgc.border_thickness * 2,
                    wgc.crop.height + wgc.border_thickness * 2
                ),
                (
                    geometry.content.width + geometry.border_thickness * 2,
                    geometry.content.height + geometry.border_thickness * 2
                ),
                "surface {surface_width}x{surface_height}"
            );
            assert_eq!(
                geometry.published_origin(wgc.border_thickness),
                geometry.published_origin(geometry.border_thickness),
                "surface {surface_width}x{surface_height}"
            );
            assert_eq!(
                geometry.published_origin(wgc.border_thickness),
                (geometry.frame.x, geometry.frame.y),
                "surface {surface_width}x{surface_height}"
            );
        }
    }

    #[test]
    fn an_unplaceable_wgc_surface_falls_back_to_a_symmetric_inset() {
        let geometry = resizable_window_geometry();
        // The window resized mid-capture, so neither measured rectangle describes the surface.
        let unaligned = wgc_content_crop(640, 480, &geometry);
        assert!(!unaligned.aligned);
        assert_eq!(
            unaligned.crop,
            Rect {
                x: 2,
                y: 2,
                width: 636,
                height: 476,
            }
        );
        assert_eq!(unaligned.border_thickness, 2);
    }

    #[test]
    fn a_tiny_wgc_surface_clamps_the_border_it_rebuilds() {
        let geometry = WindowGeometry {
            window: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 400,
            },
            frame: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 400,
            },
            content: Rect {
                x: 20,
                y: 20,
                width: 360,
                height: 360,
            },
            border_thickness: 20,
        };
        let clamped = wgc_content_crop(8, 8, &geometry);
        assert!(!clamped.aligned);
        assert_eq!(clamped.border_thickness, 2);
        assert_eq!(
            clamped.crop,
            Rect {
                x: 2,
                y: 2,
                width: 4,
                height: 4,
            }
        );
    }

    #[test]
    fn a_borderless_window_publishes_its_own_origin() {
        let bounds = Rect {
            x: -40,
            y: 15,
            width: 300,
            height: 200,
        };
        let geometry = WindowGeometry {
            window: bounds,
            frame: bounds,
            content: bounds,
            border_thickness: 0,
        };
        let wgc = wgc_content_crop(bounds.width, bounds.height, &geometry);
        assert!(wgc.aligned);
        assert_eq!(
            wgc.crop,
            Rect {
                x: 0,
                y: 0,
                ..bounds
            }
        );
        assert_eq!(geometry.published_origin(0), (-40, 15));
    }

    #[test]
    fn visible_dwm_border_is_inset_symmetrically() {
        assert_eq!(
            inset_rect(
                Rect {
                    x: 66,
                    y: 468,
                    width: 2_618,
                    height: 1_511,
                },
                2,
            ),
            Some(Rect {
                x: 68,
                y: 470,
                width: 2_614,
                height: 1_507,
            })
        );
        assert_eq!(
            inset_rect(
                Rect {
                    x: 0,
                    y: 0,
                    width: 3,
                    height: 3
                },
                2
            ),
            None
        );
    }

    #[test]
    fn rounded_window_mask_has_transparent_antialiased_corners() {
        let mut pixels = vec![128_u8; 16 * 16 * 4];
        apply_window_alpha(&mut pixels, 16, 16, 8.0, AlphaSource::Coverage);
        assert_eq!(pixels[3], 0);
        assert_eq!(pixels[(8 * 16 + 8) * 4 + 3], 255);
        assert!(pixels
            .chunks_exact(4)
            .any(|pixel| (1..255).contains(&pixel[3])));
        assert_eq!(&pixels[..3], &[0, 0, 0]);
    }

    #[test]
    fn clean_window_border_expands_the_frame_without_changing_content() {
        let content = vec![10, 20, 30, 0, 40, 50, 60, 0];
        let (framed, width, height) =
            add_clean_window_border(content, 2, 1, 1, 0.0, AlphaSource::Coverage)
                .expect("bordered frame");
        assert_eq!((width, height), (4, 3));
        let pixel = |x: u32, y: u32| {
            let offset = (y * width + x) as usize * 4;
            &framed[offset..offset + 4]
        };
        assert_eq!(pixel(1, 1), &[10, 20, 30, 255]);
        assert_eq!(pixel(2, 1), &[40, 50, 60, 255]);
        assert_eq!(pixel(2, 0), &[188, 180, 176, 255]);
    }

    #[test]
    fn clean_window_border_preserves_transparent_rounded_corners() {
        let content = vec![200_u8; 8 * 8 * 4];
        let (framed, width, height) =
            add_clean_window_border(content, 8, 8, 1, 4.0, AlphaSource::Coverage)
                .expect("rounded bordered frame");
        assert_eq!((width, height), (10, 10));
        assert_eq!(&framed[..4], &[0, 0, 0, 0]);
        assert!(framed
            .chunks_exact(4)
            .any(|pixel| (1..255).contains(&pixel[3])));
        let top_center = (5 * 4) as usize;
        assert_eq!(&framed[top_center..top_center + 3], &[188, 180, 176]);
        assert!(framed[top_center + 3] > 200);
    }

    #[test]
    fn corner_coverage_replaces_gdi_alpha_but_only_scales_wgc_alpha() {
        // (stored alpha, corner coverage) -> (PrintWindow alpha, WGC alpha)
        let cases = [
            ((0_u8, 255_u8), (255_u8, 0_u8)),
            ((128, 255), (255, 128)),
            ((255, 255), (255, 255)),
            ((255, 128), (128, 128)),
            ((128, 128), (128, 64)),
            ((200, 0), (0, 0)),
            ((0, 0), (0, 0)),
        ];
        for ((stored, coverage), (expected_coverage_alpha, expected_content_alpha)) in cases {
            assert_eq!(
                AlphaSource::Coverage.combine(stored, coverage),
                expected_coverage_alpha,
                "coverage mask for stored={stored} coverage={coverage}"
            );
            assert_eq!(
                AlphaSource::Content.combine(stored, coverage),
                expected_content_alpha,
                "content mask for stored={stored} coverage={coverage}"
            );
        }
    }

    #[test]
    fn opaque_content_masks_identically_under_both_alpha_sources() {
        // The WGC fix must be a no-op for opaque windows: an alpha-255 source multiplied by
        // coverage has to reproduce coverage exactly, or every square window changes shape.
        for coverage in 0..=255_u8 {
            assert_eq!(
                AlphaSource::Content.combine(255, coverage),
                AlphaSource::Coverage.combine(255, coverage),
                "coverage={coverage}"
            );
        }
    }

    #[test]
    fn wgc_translucent_content_keeps_its_alpha_through_the_border_frame() {
        // A uniformly half-transparent acrylic backdrop, big enough that the centre pixel is well
        // clear of the rounded corners.
        let content = [90_u8, 80, 70, 128].repeat(8 * 8);
        let (framed, width, _) =
            add_clean_window_border(content.clone(), 8, 8, 1, 4.0, AlphaSource::Content)
                .expect("translucent bordered frame");
        let centre = ((4 * width) + 4) as usize * 4;
        assert_eq!(&framed[centre..centre + 4], &[90, 80, 70, 128]);

        // The same buffer down the PrintWindow path is published opaque, because GDI alpha is
        // garbage there and coverage is the only signal available.
        let (opaque, _, _) = add_clean_window_border(content, 8, 8, 1, 4.0, AlphaSource::Coverage)
            .expect("opaque bordered frame");
        assert_eq!(&opaque[centre..centre + 4], &[90, 80, 70, 255]);
    }

    #[test]
    fn wgc_fully_transparent_content_publishes_nothing_but_the_border() {
        // A fully transparent WGC surface must not be resurrected as opaque black, and the
        // compositor must not divide by the zero alpha it produces.
        let content = vec![0_u8; 8 * 8 * 4];
        let (framed, width, height) =
            add_clean_window_border(content, 8, 8, 1, 4.0, AlphaSource::Content)
                .expect("transparent bordered frame");
        assert_eq!(framed.len(), (width * height) as usize * 4);

        let centre = ((4 * width) + 4) as usize * 4;
        assert_eq!(&framed[centre..centre + 4], &[0, 0, 0, 0]);
        for content_y in 0..8_u32 {
            for content_x in 0..8_u32 {
                let offset = (((content_y + 1) * width) + content_x + 1) as usize * 4;
                assert!(
                    framed[offset + 3] < 255,
                    "content pixel ({content_x},{content_y}) was published opaque"
                );
            }
        }
        // The synthetic ring itself is unaffected by the content's transparency.
        let ring_top_centre = (width / 2) as usize * 4;
        assert_eq!(
            &framed[ring_top_centre..ring_top_centre + 3],
            &WINDOW_BORDER_BGRA
        );
        assert!(framed[ring_top_centre + 3] > 200);
    }

    #[test]
    fn wgc_translucent_content_survives_thumbnail_scaling() {
        // StretchBlt drops the alpha byte, so a downscaled acrylic thumbnail used to arrive fully
        // transparent (or, before this fix, was papered over by the coverage mask).
        let pixels = [90_u8, 80, 70, 128].repeat(4 * 4);
        let scaled = scale_bgra_with_dib(&pixels, 4, 4, 2, 2, "test_wgc_alpha_round_trip")
            .expect("scale translucent WGC pixels through native DIBs");
        assert_eq!(scaled.len(), 2 * 2 * 4);
        for pixel in scaled.chunks_exact(4) {
            assert_eq!(pixel[3], 128, "alpha must survive the GDI stretch");
        }
    }

    #[test]
    fn reported_border_remains_visible_after_thumbnail_scaling() {
        assert_eq!(scaled_border_width(0, 0.25), 0);
        assert_eq!(scaled_border_width(2, 1.0), 2);
        assert_eq!(scaled_border_width(2, 0.25), 1);
    }

    #[test]
    fn wgc_colors_are_unpremultiplied_before_straight_alpha_publication() {
        let pixels = normalize_wgc_content(vec![25, 50, 100, 128, 9, 8, 7, 0, 10, 20, 30, 255]);
        assert_eq!(pixels, [50, 100, 199, 128, 0, 0, 0, 0, 10, 20, 30, 255]);
    }

    #[test]
    fn wgc_rgb_survives_the_native_dib_thumbnail_round_trip() {
        let premultiplied_pixel = [25_u8, 50, 100, 128];

        let scaled = scale_bgra_with_dib(
            &premultiplied_pixel.repeat(4),
            2,
            2,
            1,
            1,
            "test_wgc_dib_round_trip",
        )
        .expect("scale premultiplied WGC pixels through native DIBs");
        let pixels = normalize_wgc_content(scaled);

        assert_eq!(&pixels[..3], &[50, 100, 199]);
        assert_eq!(pixels[3], 128);
    }

    #[test]
    fn scaling_before_unpremultiplying_keeps_transparent_pixels_from_darkening_the_result() {
        // Half opaque white, half fully transparent. Unpremultiplying first zeroes the transparent
        // half's colour, so the downscale averages a mid-grey into the survivor — a dark fringe
        // around every translucent or rounded edge. Averaging in premultiplied space carries no
        // colour at all out of a zero-alpha pixel.
        let mut source = Vec::new();
        source.extend_from_slice(&[255, 255, 255, 255]);
        source.extend_from_slice(&[0, 0, 0, 0]);
        source.extend_from_slice(&[255, 255, 255, 255]);
        source.extend_from_slice(&[0, 0, 0, 0]);

        let scaled = scale_bgra_with_dib(&source, 2, 2, 1, 1, "test_wgc_premultiplied_downscale")
            .expect("downscale premultiplied WGC pixels");
        let straight = normalize_wgc_content(scaled);

        assert!(
            straight[..3].iter().all(|&channel| channel >= 250),
            "half-covered white must stay white, got {:?}",
            &straight[..3]
        );
        assert!(
            (120..=136).contains(&straight[3]),
            "half-covered white must be half transparent, got {}",
            straight[3]
        );
    }

    #[test]
    fn default_corner_rounding_does_not_cut_borderless_windows() {
        assert_eq!(logical_corner_radius(DWMWCP_DEFAULT, false), 0.0);
        assert_eq!(logical_corner_radius(DWMWCP_DEFAULT, true), 8.0);
        assert_eq!(logical_corner_radius(DWMWCP_ROUND, false), 8.0);
    }

    #[test]
    fn display_selection_reuses_the_frozen_frame() {
        let desktop = test_desktop();
        let selected = materialize_selection(
            &desktop,
            &OverlaySelection {
                rect: desktop.metadata.source_rect,
                kind: SelectionKind::Display,
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
                window_frame: None,
            },
        )
        .expect("display selection should be materialized");
        assert!(Arc::ptr_eq(
            desktop.pixels_shared(),
            selected.pixels_shared()
        ));
        assert_eq!(desktop.metadata.source_rect, selected.metadata.source_rect);
    }

    #[test]
    fn window_selection_never_falls_back_to_a_desktop_crop() {
        let desktop = test_desktop();
        let error = materialize_selection(
            &desktop,
            &OverlaySelection {
                rect: desktop.metadata.source_rect,
                kind: SelectionKind::Window,
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
                window_frame: None,
            },
        )
        .expect_err("a missing HWND must not produce the desktop crop");
        assert_eq!(error.kind, CaptureErrorKind::SourceUnavailable);
        assert_eq!(error.operation, "resolve_selected_window");
    }

    #[test]
    fn window_selection_reuses_the_frame_shown_in_the_overlay() {
        let desktop = test_desktop();
        let preview = test_desktop();
        let selected = materialize_selection(
            &desktop,
            &OverlaySelection {
                rect: preview.metadata.source_rect,
                kind: SelectionKind::Window,
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
                window_frame: Some(preview.clone()),
            },
        )
        .expect("previewed window frame should be materialized without another native render");
        assert!(Arc::ptr_eq(
            preview.pixels_shared(),
            selected.pixels_shared()
        ));
        assert!(Arc::ptr_eq(
            preview.pixels_shared(),
            captured_window_frame(&OverlaySelection {
                rect: preview.metadata.source_rect,
                kind: SelectionKind::Window,
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
                window_frame: Some(preview.clone()),
            })
            .expect("window confirmation should expose its native frame")
            .pixels_shared()
        ));
    }

    #[test]
    #[ignore = "requires CAPTASTIC_TEST_WINDOW_HANDLE naming a live interactive window"]
    fn native_window_capture_reconstructs_the_reported_dwm_border() {
        // SAFETY: Changes DPI virtualization only for this short-lived integration-test thread.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let raw = std::env::var("CAPTASTIC_TEST_WINDOW_HANDLE")
            .expect("set CAPTASTIC_TEST_WINDOW_HANDLE")
            .parse::<isize>()
            .expect("numeric window handle");
        let captured =
            capture_window_visual(NativeWindowHandle::from_raw(raw), &test_desktop().metadata)
                .expect("native window capture");
        let hwnd = HWND(raw);
        let mut bounds = RECT::default();
        // SAFETY: The test environment supplies a live window handle and writable RECT storage.
        unsafe { GetWindowRect(hwnd, &mut bounds) }.expect("window bounds");
        let outer = rect_from_native(bounds).expect("valid bounds");
        let geometry = window_geometry(hwnd, outer);
        let border = geometry.border_thickness;
        assert_eq!(captured.frame.width(), geometry.content.width + border * 2);
        assert_eq!(
            captured.frame.height(),
            geometry.content.height + border * 2
        );
        assert_eq!(
            (
                captured.frame.metadata.source_rect.x,
                captured.frame.metadata.source_rect.y
            ),
            geometry.published_origin(border)
        );
    }

    /// Verifies on a real desktop what the table tests can only assert about arithmetic: that the
    /// two backends, run against the *same live window*, publish the same rectangle — and reports
    /// how much translucency each one kept.
    ///
    /// Run it against a standard resizable window with:
    ///
    /// CAPTASTIC_TEST_WINDOW_HANDLE=<hwnd> cargo test --locked -p captastic-windows \
    ///     -- --ignored --nocapture both_backends_capture_a_live_window_identically
    #[test]
    #[ignore = "requires CAPTASTIC_TEST_WINDOW_HANDLE naming a live interactive window"]
    fn both_backends_capture_a_live_window_identically() {
        // SAFETY: Changes DPI virtualization only for this short-lived integration-test thread.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let raw = std::env::var("CAPTASTIC_TEST_WINDOW_HANDLE")
            .expect("set CAPTASTIC_TEST_WINDOW_HANDLE")
            .parse::<isize>()
            .expect("numeric window handle");
        let hwnd = HWND(raw);
        let metadata = test_desktop().metadata;

        let mut native = RECT::default();
        // SAFETY: The test environment supplies a live window handle and writable RECT storage.
        unsafe { GetWindowRect(hwnd, &mut native) }.expect("window bounds");
        let geometry = window_geometry(hwnd, rect_from_native(native).expect("valid bounds"));
        println!(
            "window rect {}x{} at ({},{}); visible frame {}x{} at ({},{}); border {}px",
            geometry.window.width,
            geometry.window.height,
            geometry.window.x,
            geometry.window.y,
            geometry.frame.width,
            geometry.frame.height,
            geometry.frame.x,
            geometry.frame.y,
            geometry.border_thickness,
        );

        // A responsive window takes the PrintWindow path; the WGC path is invoked directly so both
        // can be compared for one window in one run.
        let printed = capture_window_visual(NativeWindowHandle::from_raw(raw), &metadata)
            .expect("PrintWindow capture");
        assert_eq!(
            printed.frame.metadata.backend, "windows-print-window",
            "the target window must be responsive for this comparison to mean anything"
        );
        let captured = capture_window_with_wgc(
            hwnd,
            &metadata,
            None,
            &geometry,
            RenderDeadline::starting_now(Duration::from_secs(30)),
        )
        .expect("Windows Graphics Capture");

        assert_eq!(
            (printed.frame.width(), printed.frame.height()),
            (captured.frame.width(), captured.frame.height()),
            "the backends cropped the window to different sizes"
        );
        assert_eq!(
            printed.frame.metadata.source_rect, captured.frame.metadata.source_rect,
            "the backends disagree about where the captured frame sits on the desktop"
        );
        assert_eq!(printed.corner_radius_px, captured.corner_radius_px);
        // The visible DWM frame is exactly what both are expected to have published.
        assert_eq!(
            (printed.frame.width(), printed.frame.height()),
            (geometry.frame.width, geometry.frame.height)
        );
        assert_eq!(
            (
                printed.frame.metadata.source_rect.x,
                printed.frame.metadata.source_rect.y
            ),
            (geometry.frame.x, geometry.frame.y)
        );

        let translucent = |frame: &CpuFrame| {
            frame
                .pixels()
                .chunks_exact(4)
                .filter(|pixel| (1..255).contains(&pixel[3]))
                .count()
        };
        println!(
            "published {}x{} at ({},{}) from both backends; partially transparent pixels: PrintWindow {}, WGC {}",
            printed.frame.width(),
            printed.frame.height(),
            printed.frame.metadata.source_rect.x,
            printed.frame.metadata.source_rect.y,
            translucent(&printed.frame),
            translucent(&captured.frame),
        );
    }

    /// A throwaway top-level window whose composited surface has genuine per-pixel alpha.
    ///
    /// Acrylic and Mica are the motivating cases for keeping WGC's alpha, but they cannot be
    /// summoned on demand — a layered window updated with `AC_SRC_ALPHA` reaches DWM the same way
    /// and is entirely under this test's control.
    struct TranslucentTestWindow {
        hwnd: HWND,
    }

    /// The probe has no behaviour of its own; every message takes the default handling.
    unsafe extern "system" fn translucent_probe_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        // SAFETY: Win32 supplied these arguments and DefWindowProcW is their intended handler.
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }

    impl TranslucentTestWindow {
        fn show(width: u32, height: u32, alpha: u8) -> Result<Self, CaptureError> {
            let class = windows::core::w!("CaptasticTranslucentCaptureProbe");
            // SAFETY: A null module name asks for the running executable's handle.
            let instance = unsafe {
                windows::Win32::System::LibraryLoader::GetModuleHandleW(
                    windows::core::PCWSTR::null(),
                )
            }
            .map_err(|error| windows_error("probe_module_handle", error, false))?;
            let descriptor = windows::Win32::UI::WindowsAndMessaging::WNDCLASSW {
                lpfnWndProc: Some(translucent_probe_proc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // A repeated registration fails harmlessly when several tests share the process.
            // SAFETY: descriptor and its borrowed class name outlive the call.
            unsafe { windows::Win32::UI::WindowsAndMessaging::RegisterClassW(&descriptor) };

            // SAFETY: The class is registered and no creation parameter is passed.
            let hwnd = unsafe {
                windows::Win32::UI::WindowsAndMessaging::CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WS_EX_LAYERED
                        | windows::Win32::UI::WindowsAndMessaging::WS_EX_TOOLWINDOW
                        | windows::Win32::UI::WindowsAndMessaging::WS_EX_TOPMOST,
                    class,
                    windows::core::w!("Captastic translucency probe"),
                    windows::Win32::UI::WindowsAndMessaging::WS_POPUP,
                    64,
                    64,
                    width as i32,
                    height as i32,
                    None,
                    None,
                    instance,
                    None,
                )
            };
            if hwnd.0 == 0 {
                return Err(last_error("create_translucency_probe"));
            }
            let window = Self { hwnd };

            // Pure red premultiplied by `alpha`, which is the representation UpdateLayeredWindow
            // and Windows Graphics Capture both work in.
            let mut surface = DibSurface::new(width, height)?;
            let premultiplied_red = ((u32::from(alpha) * 255 + 127) / 255) as u8;
            surface.write_pixels(
                &[0, 0, premultiplied_red, alpha].repeat((width * height) as usize),
            )?;

            let destination = windows::Win32::Foundation::POINT { x: 64, y: 64 };
            let origin = windows::Win32::Foundation::POINT { x: 0, y: 0 };
            let size = windows::Win32::Foundation::SIZE {
                cx: width as i32,
                cy: height as i32,
            };
            let blend = windows::Win32::Graphics::Gdi::BLENDFUNCTION {
                BlendOp: 0,
                BlendFlags: 0,
                SourceConstantAlpha: u8::MAX,
                AlphaFormat: windows::Win32::Graphics::Gdi::AC_SRC_ALPHA as u8,
            };
            // SAFETY: A null HWND obtains the desktop DC, released immediately below.
            let screen = unsafe { windows::Win32::Graphics::Gdi::GetDC(None) };
            // SAFETY: Every borrowed structure and the selected DIB outlive the call.
            let updated = unsafe {
                windows::Win32::UI::WindowsAndMessaging::UpdateLayeredWindow(
                    hwnd,
                    screen,
                    Some(&destination),
                    Some(&size),
                    surface.device,
                    Some(&origin),
                    windows::Win32::Foundation::COLORREF(0),
                    Some(&blend),
                    windows::Win32::UI::WindowsAndMessaging::ULW_ALPHA,
                )
            };
            // SAFETY: Balances the GetDC(None) above on this thread.
            unsafe { windows::Win32::Graphics::Gdi::ReleaseDC(None, screen) };
            updated.map_err(|error| windows_error("present_translucency_probe", error, false))?;

            // SAFETY: hwnd is this process's live top-level window.
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
                )
            };
            // DWM needs a compose pass before Windows Graphics Capture has anything to hand back.
            thread::sleep(Duration::from_millis(250));
            Ok(window)
        }
    }

    impl Drop for TranslucentTestWindow {
        fn drop(&mut self) {
            // SAFETY: The window was created on this thread and has not been destroyed yet.
            let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd) };
        }
    }

    /// The real-desktop half of the alpha fix: WGC has to *hand back* translucency for the
    /// publication path to have any to keep.
    ///
    /// cargo test --locked -p captastic-windows -- --ignored --nocapture \
    ///     wgc_publishes_a_translucent_window_translucently
    #[test]
    #[ignore = "briefly shows a real translucent window on the interactive desktop"]
    fn wgc_publishes_a_translucent_window_translucently() {
        const PROBE_ALPHA: u8 = 128;
        // SAFETY: Changes DPI virtualization only for this short-lived integration-test thread.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let window = TranslucentTestWindow::show(240, 160, PROBE_ALPHA).expect("translucent probe");

        let mut native = RECT::default();
        // SAFETY: The probe window is live and native is writable for the exact RECT size.
        unsafe { GetWindowRect(window.hwnd, &mut native) }.expect("probe bounds");
        let geometry =
            window_geometry(window.hwnd, rect_from_native(native).expect("valid bounds"));
        let captured = capture_window_with_wgc(
            window.hwnd,
            &test_desktop().metadata,
            None,
            &geometry,
            RenderDeadline::starting_now(Duration::from_secs(30)),
        )
        .expect("Windows Graphics Capture of the translucent probe");

        let translucent = captured
            .frame
            .pixels()
            .chunks_exact(4)
            .filter(|pixel| (PROBE_ALPHA - 16..=PROBE_ALPHA + 16).contains(&pixel[3]))
            .count();
        let total = (captured.frame.width() * captured.frame.height()) as usize;
        println!(
            "probe published {}x{}: {translucent}/{total} pixels at ~{PROBE_ALPHA} alpha",
            captured.frame.width(),
            captured.frame.height(),
        );
        assert!(
            translucent * 2 > total,
            "a half-transparent window was published with only {translucent} of {total} pixels translucent"
        );
        // The colour must survive unpremultiplication rather than staying at the premultiplied
        // value it arrived as.
        let centre = ((captured.frame.height() / 2) * captured.frame.width()
            + captured.frame.width() / 2) as usize
            * 4;
        assert!(
            captured.frame.pixels()[centre + 2] > 240,
            "the probe's red channel came back at {} instead of ~255",
            captured.frame.pixels()[centre + 2]
        );
    }

    /// Measures what opening the window chooser costs, split between the two mechanisms.
    ///
    /// The frozen inventory renders every window through PrintWindow or WGC and scales it; the
    /// live previews ask DWM to composite surfaces it already holds. Both currently run when live
    /// previews are enabled, and this is the number that says whether that matters.
    ///
    /// CAPTASTIC_TEST_WINDOW_HANDLES=1234,5678 cargo test --locked -p captastic-windows --release
    ///     -- --ignored --nocapture window_overview_frozen_versus_live_preview_cost
    #[test]
    #[ignore = "requires CAPTASTIC_TEST_WINDOW_HANDLES listing live interactive windows"]
    fn window_overview_frozen_versus_live_preview_cost() {
        use crate::dwm_thumbnail::DwmThumbnail;
        use std::time::Instant;

        // The chooser's own budget, so the numbers describe what it actually does.
        const THUMBNAIL_MAX_PIXELS: u64 = 1_200_000;

        // SAFETY: Changes DPI virtualization only for this short-lived integration-test thread.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let handles: Vec<isize> = std::env::var("CAPTASTIC_TEST_WINDOW_HANDLES")
            .expect("set CAPTASTIC_TEST_WINDOW_HANDLES")
            .split(',')
            .filter_map(|raw| raw.trim().parse::<isize>().ok())
            .collect();
        assert!(!handles.is_empty(), "no window handles supplied");
        let metadata = test_desktop().metadata;
        // A hidden destination for the DWM registrations; nothing is ever shown on screen.
        let destination = TranslucentTestWindow::show(16, 16, 255).expect("destination window");

        let mut frozen_total = Duration::ZERO;
        let mut live_total = Duration::ZERO;
        let mut seed_total = Duration::ZERO;
        let mut frozen_failures = 0_usize;
        let mut live_failures = 0_usize;
        let mut needs_render = 0_usize;

        println!(
            "{:>12}  {:>12}  {:>12}  {:>12}  window",
            "frozen (ms)", "live (ms)", "seed (ms)", "ratio"
        );
        for raw in &handles {
            let handle = NativeWindowHandle::from_raw(*raw);

            // The frozen inventory: a real native render, scaled to the chooser's budget.
            let started = Instant::now();
            let frozen = capture_window_thumbnail(handle, &metadata, THUMBNAIL_MAX_PIXELS);
            let frozen_elapsed = started.elapsed();

            // The live preview: register with DWM, ask for the source size, place it. This is
            // every per-window call the chooser makes on that path.
            let started = Instant::now();
            let live = DwmThumbnail::register(destination.hwnd, HWND(*raw)).and_then(|thumbnail| {
                let size = thumbnail.source_size()?;
                thumbnail.show(
                    RECT {
                        left: 0,
                        top: 0,
                        right: 160,
                        bottom: 90,
                    },
                    255,
                )?;
                Ok(size)
            });
            let live_elapsed = started.elapsed();

            // What the chooser now does per window before deciding whether to render it: ask DWM
            // for the source size and read the corner radius, both attribute reads.
            let started = Instant::now();
            let seeded = DwmThumbnail::register(destination.hwnd, HWND(*raw))
                .and_then(|thumbnail| thumbnail.source_size());
            let radius = window_corner_radius(HWND(*raw));
            let seed_elapsed = started.elapsed();
            let _ = radius;
            if seeded.is_err() {
                needs_render += 1;
            }
            seed_total += seed_elapsed;

            if frozen.is_err() {
                frozen_failures += 1;
            }
            if live.is_err() {
                live_failures += 1;
            }
            frozen_total += frozen_elapsed;
            live_total += live_elapsed;

            let frozen_ms = frozen_elapsed.as_secs_f64() * 1000.0;
            let live_ms = live_elapsed.as_secs_f64() * 1000.0;
            println!(
                "{frozen_ms:>12.2}  {live_ms:>12.2}  {:>12.2}  {:>12}  0x{raw:X}{}{}",
                seed_elapsed.as_secs_f64() * 1000.0,
                if live_ms > 0.0 {
                    format!("{:.0}x", frozen_ms / live_ms)
                } else {
                    "-".to_owned()
                },
                if frozen.is_err() {
                    " [frozen failed]"
                } else {
                    ""
                },
                if live.is_err() { " [live failed]" } else { "" },
            );
        }

        println!(
            "
{} window(s): frozen inventory {:.1} ms total ({} failed), live previews {:.1} ms total ({} failed)",
            handles.len(),
            frozen_total.as_secs_f64() * 1000.0,
            frozen_failures,
            live_total.as_secs_f64() * 1000.0,
            live_failures,
        );
        println!(
            "seeding tiles from DWM instead: {:.1} ms, with {needs_render} window(s) still needing a render",
            seed_total.as_secs_f64() * 1000.0,
        );
        println!(
            "rendering every window costs {:.1} ms; seeding and rendering only what DWM cannot draw costs {:.1} ms",
            frozen_total.as_secs_f64() * 1000.0,
            seed_total.as_secs_f64() * 1000.0
                + if needs_render == 0 {
                    0.0
                } else {
                    // Charge the unrenderable windows at the measured average.
                    frozen_total.as_secs_f64() * 1000.0 / handles.len() as f64
                        * needs_render as f64
                },
        );
    }

    fn test_desktop() -> CpuFrame {
        let metadata = FrameMetadata {
            capture_id: CaptureId(1),
            backend: "test".to_owned(),
            display_id: DisplayId::primary(),
            source_rect: Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            rotation_degrees: 0,
            capture_mode: CaptureMode::Latest { max_age_ms: None },
            presentation_offset_ns: None,
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: 0,
            cpu_ready_offset_ns: Some(0),
            frame_age_ns: Some(0),
            verified_current_offset_ns: None,
            frame_generation: Some(1),
            copy_count: 0,
            pool_slot: None,
            cursor: None,
        };
        CpuFrame::new(
            Arc::from(vec![255_u8; 16]),
            2,
            2,
            8,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("test frame is valid")
    }
}
