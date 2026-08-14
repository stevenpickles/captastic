use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use captastic_core::{CaptureError, CaptureErrorKind, CpuFrame, FrameAlpha, FrameMetadata, Rect};
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
static DETACHED_WINDOW_RENDERS: AtomicUsize = AtomicUsize::new(0);
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
    let budget = WindowRenderBudget::acquire()?;
    let permit = WindowRenderPermit::acquire()?;
    let metadata = reference_metadata.clone();
    run_bounded_window_render(permit, budget, WINDOW_RENDER_TIMEOUT, move || {
        // Capture geometry must use physical pixels. Thread DPI awareness is not inherited
        // reliably by newly spawned workers, and mixing virtualized GetWindowRect values with
        // physical DWM bounds leaves asymmetric one- or two-pixel frame artifacts.
        // SAFETY: Changes DPI virtualization only for this short-lived render worker.
        let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        capture_window_inner(handle, &metadata, max_pixels)
    })
}

fn run_bounded_window_render<T>(
    permit: WindowRenderPermit<'static>,
    budget: WindowRenderBudget<'static>,
    timeout: Duration,
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
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The detached worker may remain blocked inside a foreign window procedure. Its
            // lease expires at the caller's deadline so unrelated windows can still be rendered;
            // release is idempotent when the worker eventually exits.
            timeout_permit.release();
            if mark_render_detached(&render_state, &DETACHED_WINDOW_RENDERS) {
                log::warn!(
                    "window render exceeded its deadline; {} detached native render worker(s) remain",
                    DETACHED_WINDOW_RENDERS.load(Ordering::Acquire)
                );
            }
            Err(capture_error(
                CaptureErrorKind::Timeout,
                "print_selected_window",
                format!(
                    "the selected window did not render within {} ms",
                    timeout.as_millis()
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
        complete_render(&self.0, &DETACHED_WINDOW_RENDERS);
    }
}

fn mark_render_detached(state: &AtomicU8, detached_count: &AtomicUsize) -> bool {
    if state
        .compare_exchange(
            RENDER_ACTIVE,
            RENDER_DETACHED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
    {
        detached_count.fetch_add(1, Ordering::AcqRel);
        true
    } else {
        false
    }
}

fn complete_render(state: &AtomicU8, detached_count: &AtomicUsize) {
    if state.swap(RENDER_COMPLETED, Ordering::AcqRel) == RENDER_DETACHED {
        detached_count.fetch_sub(1, Ordering::AcqRel);
    }
}

fn capture_window_inner(
    handle: NativeWindowHandle,
    reference_metadata: &FrameMetadata,
    max_pixels: Option<u64>,
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
    let visible_frame = visible_frame_bounds(hwnd, capture_bounds);
    let visible_bounds = visible_frame.bounds;
    let probe_wgc_error = if !window_is_responsive(hwnd) {
        log::debug!(
            "window handle=0x{:X} did not answer the responsiveness probe; using Windows Graphics Capture",
            handle.raw()
        );
        match capture_window_with_wgc(hwnd, reference_metadata, max_pixels, &visible_frame) {
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
        return capture_window_with_wgc(hwnd, reference_metadata, max_pixels, &visible_frame);
    }
    if region_is_blank(surface.as_bytes(), capture_bounds, visible_bounds)? {
        if probe_wgc_error.is_none() {
            log::debug!(
                "window handle=0x{:X} PrintWindow reported success but rendered a blank frame; trying Windows Graphics Capture",
                handle.raw()
            );
            match capture_window_with_wgc(hwnd, reference_metadata, max_pixels, &visible_frame) {
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
    let inner_corner_radius = (window_corner_radius(hwnd) - visible_frame.border_thickness as f32)
        .max(0.0)
        * radius_scale;
    let border_width = scaled_border_width(visible_frame.border_thickness, radius_scale);
    let (pixels, output_width, output_height) = add_clean_window_border(
        pixels,
        content_width,
        content_height,
        border_width,
        inner_corner_radius,
    )?;
    let corner_radius = inner_corner_radius + border_width as f32;
    let mut metadata = reference_metadata.clone();
    metadata.backend = "windows-print-window".to_owned();
    metadata.source_rect = Rect {
        x: visible_bounds
            .x
            .saturating_sub(visible_frame.border_thickness as i32),
        y: visible_bounds
            .y
            .saturating_sub(visible_frame.border_thickness as i32),
        width: output_width,
        height: output_height,
    };
    metadata.copy_count = metadata.copy_count.saturating_add(1);
    metadata.pool_slot = None;
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
    Ok(CapturedWindow {
        frame,
        corner_radius_px: corner_radius,
    })
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
    visible_frame: &VisibleFrame,
) -> Result<CapturedWindow, CaptureError> {
    let raw = capture_window_wgc_frame(hwnd)?;
    let source_bounds = Rect {
        x: 0,
        y: 0,
        width: raw.width,
        height: raw.height,
    };
    let border_thickness = visible_frame
        .border_thickness
        .min(raw.width / 4)
        .min(raw.height / 4);
    let content_bounds = inset_rect(source_bounds, border_thickness).unwrap_or(source_bounds);
    let content = normalize_wgc_content(crop_bgra(&raw.pixels, source_bounds, content_bounds)?);
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
    let inner_corner_radius =
        (window_corner_radius(hwnd) - border_thickness as f32).max(0.0) * radius_scale;
    let border_width = scaled_border_width(border_thickness, radius_scale);
    let (pixels, output_width, output_height) = add_clean_window_border(
        pixels,
        content_width,
        content_height,
        border_width,
        inner_corner_radius,
    )?;
    let corner_radius_px = inner_corner_radius + border_width as f32;
    let mut metadata = reference_metadata.clone();
    metadata.backend = "windows-graphics-capture".to_owned();
    metadata.source_rect = Rect {
        x: visible_frame
            .bounds
            .x
            .saturating_sub(visible_frame.border_thickness as i32),
        y: visible_frame
            .bounds
            .y
            .saturating_sub(visible_frame.border_thickness as i32),
        width: output_width,
        height: output_height,
    };
    metadata.copy_count = metadata.copy_count.saturating_add(2);
    metadata.pool_slot = None;
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
    Ok(CapturedWindow {
        frame,
        corner_radius_px,
    })
}

fn normalize_wgc_content(mut pixels: Vec<u8>) -> Vec<u8> {
    // WGC publishes premultiplied BGRA. Convert before any GDI scaling because StretchBlt does
    // not preserve alpha; waiting until after the blit would interpret every pixel as transparent
    // and erase its RGB channels.
    unpremultiply_bgra(&mut pixels);
    pixels
}

fn scale_bgra_with_dib(
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

#[derive(Debug)]
struct WindowRenderBudget<'a> {
    workers: &'a AtomicUsize,
}

impl WindowRenderBudget<'static> {
    fn acquire() -> Result<Self, CaptureError> {
        Self::acquire_from(&WINDOW_RENDER_WORKERS, MAX_WINDOW_RENDER_WORKERS)
    }
}

impl<'a> WindowRenderBudget<'a> {
    fn acquire_from(workers: &'a AtomicUsize, limit: usize) -> Result<Self, CaptureError> {
        let mut current = workers.load(Ordering::Acquire);
        loop {
            if current >= limit {
                return Err(capture_error(
                    CaptureErrorKind::BufferExhausted,
                    "start_window_render",
                    format!(
                        "{current} native window-render workers are still running; refusing to create an unbounded thread backlog"
                    ),
                    true,
                    None,
                ));
            }
            match workers.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self { workers }),
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

struct VisibleFrame {
    bounds: Rect,
    border_thickness: u32,
}

fn visible_frame_bounds(hwnd: HWND, capture_bounds: Rect) -> VisibleFrame {
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
        return VisibleFrame {
            bounds: capture_bounds,
            border_thickness: 0,
        };
    };
    let frame_bounds = intersect_rect(capture_bounds, dwm_bounds).unwrap_or(capture_bounds);
    let border_thickness = visible_frame_border_thickness(hwnd)
        .min(frame_bounds.width / 4)
        .min(frame_bounds.height / 4);
    let bounds = inset_rect(frame_bounds, border_thickness).unwrap_or(frame_bounds);
    VisibleFrame {
        bounds,
        border_thickness,
    }
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
    let x = u32::try_from(crop.x - source_bounds.x)
        .map_err(|_| invalid_frame("visible frame starts outside the rendered window"))?;
    let y = u32::try_from(crop.y - source_bounds.y)
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

fn window_corner_radius(hwnd: HWND) -> f32 {
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

fn apply_window_alpha(pixels: &mut [u8], width: u32, height: u32, radius: f32) {
    let stride = width as usize * 4;
    for y in 0..height {
        for x in 0..width {
            let alpha = rounded_rect_coverage(x, y, width, height, radius);
            let offset = y as usize * stride + x as usize * 4;
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
    apply_window_alpha(&mut framed, width, height, outer_corner_radius);

    for content_y in 0..content_height {
        for content_x in 0..content_width {
            let content_alpha = u32::from(rounded_rect_coverage(
                content_x,
                content_y,
                content_width,
                content_height,
                inner_corner_radius,
            ));
            if content_alpha == 0 {
                continue;
            }
            let source_offset = content_y as usize * content_stride + content_x as usize * 4;
            let output_x = content_x + border_width;
            let output_y = content_y + border_width;
            let output_offset = output_y as usize * stride + output_x as usize * 4;
            if content_alpha == 255 {
                framed[output_offset..output_offset + 3]
                    .copy_from_slice(&content[source_offset..source_offset + 3]);
                framed[output_offset + 3] = 255;
                continue;
            }

            let background_alpha = u32::from(framed[output_offset + 3]);
            let remaining = 255 - content_alpha;
            let retained_background_alpha = (background_alpha * remaining + 127) / 255;
            let output_alpha = content_alpha + retained_background_alpha;
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
    capture_error(
        CaptureErrorKind::NativeFailure,
        operation,
        error.to_string(),
        retryable,
        Some(i64::from(error.code().0)),
    )
}

fn capture_error(
    kind: CaptureErrorKind,
    operation: &'static str,
    message: impl Into<String>,
    retryable: bool,
    native_code: Option<i64>,
) -> CaptureError {
    CaptureError {
        kind,
        backend: "windows-window-capture",
        operation,
        message: message.into(),
        retryable,
        native_code,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use captastic_core::{
        CaptureId, CaptureMode, ColorSpace, DisplayId, FrameOrigin, PixelFormat, TimingProvenance,
    };

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
        let permit = WindowRenderPermit::acquire_from(active, 1).expect("render permit");
        let budget = WindowRenderBudget::acquire_from(workers, 1).expect("worker budget");
        let (release_sender, release_receiver) = mpsc::sync_channel(0);

        let error = run_bounded_window_render(permit, budget, Duration::ZERO, move || {
            release_receiver.recv().expect("release late worker");
            Ok(7_u8)
        })
        .expect_err("worker must exceed the immediate deadline");

        assert_eq!(error.kind, CaptureErrorKind::Timeout);
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert_eq!(workers.load(Ordering::Acquire), 1);
        assert!(WindowRenderBudget::acquire_from(workers, 1).is_err());
        let replacement =
            WindowRenderPermit::acquire_from(active, 1).expect("deadline reclaimed capacity");
        release_sender.send(()).expect("release detached worker");
        drop(replacement);
        assert_eq!(active.load(Ordering::Acquire), 0);
        for _ in 0..100 {
            if workers.load(Ordering::Acquire) == 0 {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(workers.load(Ordering::Acquire), 0);
    }

    #[test]
    fn bounded_render_worker_panic_is_a_non_retryable_native_failure() {
        let active = Box::leak(Box::new(AtomicUsize::new(0)));
        let workers = Box::leak(Box::new(AtomicUsize::new(0)));
        let permit = WindowRenderPermit::acquire_from(active, 1).expect("render permit");
        let budget = WindowRenderBudget::acquire_from(workers, 1).expect("worker budget");

        let error = run_bounded_window_render::<u8>(permit, budget, Duration::from_secs(1), || {
            panic!("scripted render panic")
        })
        .expect_err("panic disconnects the bounded worker");

        assert_eq!(error.kind, CaptureErrorKind::NativeFailure);
        assert!(!error.retryable);
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert_eq!(workers.load(Ordering::Acquire), 0);
    }

    #[test]
    fn native_render_worker_budget_is_a_strict_hard_cap() {
        let workers = AtomicUsize::new(0);
        let first = WindowRenderBudget::acquire_from(&workers, 2).expect("first worker");
        let second = WindowRenderBudget::acquire_from(&workers, 2).expect("second worker");

        let error = WindowRenderBudget::acquire_from(&workers, 2).expect_err("worker cap");
        assert_eq!(error.kind, CaptureErrorKind::BufferExhausted);
        assert!(error.message.contains("unbounded thread backlog"));

        drop(first);
        let replacement = WindowRenderBudget::acquire_from(&workers, 2).expect("released slot");
        drop(second);
        drop(replacement);
        assert_eq!(workers.load(Ordering::Acquire), 0);
    }

    #[test]
    fn detached_render_telemetry_handles_completion_races() {
        let detached = AtomicUsize::new(0);
        let timed_out = AtomicU8::new(RENDER_ACTIVE);
        assert!(mark_render_detached(&timed_out, &detached));
        assert_eq!(detached.load(Ordering::Acquire), 1);
        complete_render(&timed_out, &detached);
        assert_eq!(detached.load(Ordering::Acquire), 0);

        let completed_first = AtomicU8::new(RENDER_ACTIVE);
        complete_render(&completed_first, &detached);
        assert!(!mark_render_detached(&completed_first, &detached));
        assert_eq!(detached.load(Ordering::Acquire), 0);
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
        apply_window_alpha(&mut pixels, 16, 16, 8.0);
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
            add_clean_window_border(content, 2, 1, 1, 0.0).expect("bordered frame");
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
            add_clean_window_border(content, 8, 8, 1, 4.0).expect("rounded bordered frame");
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
    fn normalized_wgc_rgb_survives_a_scaler_that_discards_alpha() {
        let mut pixels = normalize_wgc_content(vec![25, 50, 100, 128]);
        pixels[3] = 0;
        assert_eq!(&pixels[..3], &[50, 100, 199]);
    }

    #[test]
    fn normalized_wgc_rgb_survives_native_dib_thumbnail_round_trip() {
        let premultiplied_pixel = [25, 50, 100, 128];
        let pixels = normalize_wgc_content(premultiplied_pixel.repeat(4));

        let scaled = scale_bgra_with_dib(&pixels, 2, 2, 1, 1, "test_wgc_dib_round_trip")
            .expect("scale normalized WGC pixels through native DIBs");

        assert_eq!(&scaled[..3], &[50, 100, 199]);
        assert_ne!(&scaled[..3], &[0, 0, 0]);
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
        assert!(Arc::ptr_eq(&desktop.pixels, &selected.pixels));
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
        assert!(Arc::ptr_eq(&preview.pixels, &selected.pixels));
        assert!(Arc::ptr_eq(
            &preview.pixels,
            &captured_window_frame(&OverlaySelection {
                rect: preview.metadata.source_rect,
                kind: SelectionKind::Window,
                window: None,
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
            .pixels
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
        let visible_frame = visible_frame_bounds(hwnd, outer);
        let border = visible_frame.border_thickness;
        assert_eq!(
            captured.frame.width,
            visible_frame.bounds.width + border * 2
        );
        assert_eq!(
            captured.frame.height,
            visible_frame.bounds.height + border * 2
        );
        assert_eq!(
            captured.frame.metadata.source_rect.x,
            visible_frame.bounds.x - border as i32
        );
        assert_eq!(
            captured.frame.metadata.source_rect.y,
            visible_frame.bounds.y - border as i32
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
            frame_generation: Some(1),
            copy_count: 0,
            pool_slot: None,
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
