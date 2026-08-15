//! The overlay's software rasterizer: pure pixel math, surface-level composition, GDI
//! primitives, and the vector icon glyphs. Nothing here reads overlay state - every function
//! takes surfaces, device contexts, geometry, and colors - which is what makes this layer
//! reusable by any future window (pinned captures included) and testable without one.

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse,
    FillRect, GdiFlush, GetStockObject, GetTextExtentPoint32W, LineTo, MoveToEx, Rectangle,
    RoundRect, SelectObject, SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, AC_SRC_ALPHA,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, DEFAULT_CHARSET,
    DEFAULT_PITCH, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_MEDIUM, HALFTONE,
    HDC, HFONT, NULL_BRUSH, PS_SOLID, RGBQUAD, SRCCOPY, TRANSPARENT,
};

use captastic_core::{CaptureError, Rect};

use super::layout::{UiMetrics, UiRect};
use super::shell::{REGION_CURSOR_CENTER, REGION_CURSOR_SIZE};
use super::{FrozenSurface, LIVE_UNDRAWN_ALPHA};

#[derive(Clone, Copy)]
pub(super) enum TextAlignment {
    Left,
    Center,
}

pub(super) fn blend_channel(foreground: u8, background: u8, coverage: u8) -> u8 {
    let coverage = u32::from(coverage);
    ((u32::from(foreground) * coverage + u32::from(background) * (255 - coverage) + 127) / 255)
        as u8
}

pub(super) fn rounded_rect_coverage(width: i32, height: i32, radius: f32, x: i32, y: i32) -> u8 {
    if x < 0 || y < 0 || x >= width || y >= height {
        return 0;
    }
    let radius = radius.clamp(0.0, width.min(height) as f32 / 2.0);
    if radius <= 0.0 {
        return 255;
    }
    let left = x as f32 + 1.0 <= radius;
    let right = x as f32 >= width as f32 - radius;
    let top = y as f32 + 1.0 <= radius;
    let bottom = y as f32 >= height as f32 - radius;
    if !(left || right) || !(top || bottom) {
        return 255;
    }
    let center_x = if left { radius } else { width as f32 - radius };
    let center_y = if top { radius } else { height as f32 - radius };
    let radius_squared = radius * radius;
    let mut inside = 0_u32;
    const SAMPLES: i32 = 8;
    for sample_y in 0..SAMPLES {
        for sample_x in 0..SAMPLES {
            let sample_x = x as f32 + (sample_x as f32 + 0.5) / SAMPLES as f32;
            let sample_y = y as f32 + (sample_y as f32 + 0.5) / SAMPLES as f32;
            let delta_x = sample_x - center_x;
            let delta_y = sample_y - center_y;
            if delta_x * delta_x + delta_y * delta_y <= radius_squared {
                inside += 1;
            }
        }
    }
    ((inside * 255 + 32) / 64) as u8
}

pub(super) fn area_contributors(
    source_length: usize,
    destination_length: usize,
) -> Vec<Vec<(usize, f32)>> {
    let scale = source_length as f32 / destination_length as f32;
    (0..destination_length)
        .map(|destination| {
            let start = destination as f32 * scale;
            let end = (destination + 1) as f32 * scale;
            let first = start.floor() as usize;
            let last = end.ceil().min(source_length as f32) as usize;
            (first..last)
                .filter_map(|source| {
                    let overlap = end.min((source + 1) as f32) - start.max(source as f32);
                    (overlap > 0.0).then_some((source, overlap / scale))
                })
                .collect()
        })
        .collect()
}

pub(super) fn area_scale_bgra(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    destination: &mut [u8],
    destination_width: usize,
    destination_height: usize,
) {
    let horizontal = area_contributors(source_width, destination_width);
    let vertical = area_contributors(source_height, destination_height);
    let mut intermediate = vec![0_f32; destination_width * source_height * 4];
    for source_y in 0..source_height {
        for (destination_x, contributors) in horizontal.iter().enumerate() {
            let output = (source_y * destination_width + destination_x) * 4;
            for &(source_x, weight) in contributors {
                let input = (source_y * source_width + source_x) * 4;
                for channel in 0..4 {
                    intermediate[output + channel] += f32::from(source[input + channel]) * weight;
                }
            }
        }
    }
    for (destination_y, contributors) in vertical.iter().enumerate() {
        for destination_x in 0..destination_width {
            let output = (destination_y * destination_width + destination_x) * 4;
            for channel in 0..4 {
                let mut value = 0_f32;
                for &(source_y, weight) in contributors {
                    value += intermediate
                        [(source_y * destination_width + destination_x) * 4 + channel]
                        * weight;
                }
                destination[output + channel] = value.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

pub(super) fn bilinear_scale_bgra(
    source: &[u8],
    source_width: usize,
    source_height: usize,
    destination: &mut [u8],
    destination_width: usize,
    destination_height: usize,
) {
    let scale_x = source_width as f32 / destination_width as f32;
    let scale_y = source_height as f32 / destination_height as f32;
    for destination_y in 0..destination_height {
        let source_y =
            ((destination_y as f32 + 0.5) * scale_y - 0.5).clamp(0.0, (source_height - 1) as f32);
        let y0 = source_y.floor() as usize;
        let y1 = (y0 + 1).min(source_height - 1);
        let fy = source_y - y0 as f32;
        for destination_x in 0..destination_width {
            let source_x = ((destination_x as f32 + 0.5) * scale_x - 0.5)
                .clamp(0.0, (source_width - 1) as f32);
            let x0 = source_x.floor() as usize;
            let x1 = (x0 + 1).min(source_width - 1);
            let fx = source_x - x0 as f32;
            let output = (destination_y * destination_width + destination_x) * 4;
            for channel in 0..4 {
                let top = f32::from(source[(y0 * source_width + x0) * 4 + channel]) * (1.0 - fx)
                    + f32::from(source[(y0 * source_width + x1) * 4 + channel]) * fx;
                let bottom = f32::from(source[(y1 * source_width + x0) * 4 + channel]) * (1.0 - fx)
                    + f32::from(source[(y1 * source_width + x1) * 4 + channel]) * fx;
                destination[output + channel] =
                    (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

pub(super) fn fitted_surface_rect(
    surface: &FrozenSurface,
    bounds: UiRect,
    allow_upscale: bool,
) -> UiRect {
    let available_width = (bounds.right - bounds.left).max(1);
    let available_height = (bounds.bottom - bounds.top).max(1);
    let (mut width, mut height) = if i64::from(surface.width) * i64::from(available_height)
        > i64::from(surface.height) * i64::from(available_width)
    {
        (
            available_width,
            (i64::from(surface.height) * i64::from(available_width) / i64::from(surface.width))
                as i32,
        )
    } else {
        (
            (i64::from(surface.width) * i64::from(available_height) / i64::from(surface.height))
                as i32,
            available_height,
        )
    };
    if !allow_upscale && surface.width <= available_width && surface.height <= available_height {
        width = surface.width;
        height = surface.height;
    }
    let x = bounds.left + (available_width - width) / 2;
    let y = bounds.top + (available_height - height) / 2;
    UiRect {
        left: x,
        top: y,
        right: x + width.max(1),
        bottom: y + height.max(1),
    }
}

pub(super) fn scaled_corner_radius(
    surface: &FrozenSurface,
    destination: UiRect,
    source_radius: f32,
) -> f32 {
    if source_radius <= 0.0 || surface.width <= 0 || surface.height <= 0 {
        return 0.0;
    }
    let width_scale = (destination.right - destination.left).max(1) as f32 / surface.width as f32;
    let height_scale = (destination.bottom - destination.top).max(1) as f32 / surface.height as f32;
    source_radius * width_scale.min(height_scale)
}

pub(super) fn centered_rect(x: i32, y: i32, radius: i32) -> RECT {
    RECT {
        left: x.saturating_sub(radius),
        top: y.saturating_sub(radius),
        right: x.saturating_add(radius).saturating_add(1),
        bottom: y.saturating_add(radius).saturating_add(1),
    }
}

pub(super) const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF((red as u32) | ((green as u32) << 8) | ((blue as u32) << 16))
}

pub(super) fn high_contrast_cursor_pixels() -> (Vec<u8>, Vec<u8>) {
    let size = REGION_CURSOR_SIZE as usize;
    let mask_stride = size.div_ceil(16) * 2;
    let mut pixels = vec![0_u8; size * size * 4];
    let mut mask = vec![0xff_u8; mask_stride * size];
    for y in 0..size {
        for x in 0..size {
            let delta_x = (x as i32 - REGION_CURSOR_CENTER).abs();
            let delta_y = (y as i32 - REGION_CURSOR_CENTER).abs();
            let horizontal = delta_y <= 3 && (6..=29).contains(&delta_x);
            let vertical = delta_x <= 3 && (6..=29).contains(&delta_y);
            let center_mark = delta_x <= 1 && delta_y <= 1;
            if !horizontal && !vertical && !center_mark {
                continue;
            }
            let inner = (delta_y <= 1 && horizontal)
                || (delta_x <= 1 && vertical)
                || (delta_x == 0 && delta_y == 0);
            let offset = (y * size + x) * 4;
            let channel = if inner { 255 } else { 0 };
            pixels[offset..offset + 3].fill(channel);
            pixels[offset + 3] = 255;
            let mask_byte = y * mask_stride + x / 8;
            mask[mask_byte] &= !(0x80 >> (x % 8));
        }
    }
    (pixels, mask)
}

pub(super) fn top_down_bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: (width as u32)
                .saturating_mul(height as u32)
                .saturating_mul(4),
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    }
}

pub(super) fn blend_surface_pixel(
    surface: &FrozenSurface,
    x: i32,
    y: i32,
    foreground: [u8; 4],
    coverage: u8,
) {
    if x < 0 || y < 0 || x >= surface.width || y >= surface.height {
        return;
    }
    let offset = (i64::from(y) * i64::from(surface.width) + i64::from(x)) as usize * 4;
    // SAFETY: The checked coordinates address four writable bytes in the live off-screen DIB.
    unsafe {
        for (channel, foreground) in foreground.into_iter().enumerate() {
            let destination = surface.bits.add(offset + channel);
            *destination = blend_channel(foreground, *destination, coverage);
        }
    }
}

/// Strokes `rect` inward by `stroke_width` with an antialiased rounded outer edge.
///
/// The stroke region is the true erosion of the rounded rect: the inner boundary is a
/// concentric arc of radius `corner_radius - stroke_width`, degenerating to a square corner
/// when the radius does not exceed the stroke - exactly like a square frame, whose corners
/// also measure thicker diagonally. Callers wanting a constant-width stroke along the whole
/// arc therefore pass `corner_radius >= stroke_width` (the window-tile caller derives the
/// radius as `scaled_radius + stroke_width`, which guarantees it).
pub(super) fn draw_antialiased_rounded_outline(
    surface: &FrozenSurface,
    rect: UiRect,
    color: COLORREF,
    stroke_width: i32,
    corner_radius: f32,
) {
    // SAFETY: Flushes preceding GDI composition before blend_surface_pixel reads and writes the
    // same DIB section through its CPU pointer.
    let _ = unsafe { GdiFlush() };
    let width = (rect.right - rect.left).max(1);
    let height = (rect.bottom - rect.top).max(1);
    let radius = corner_radius.clamp(0.0, width.min(height) as f32 / 2.0);
    let stroke_width = stroke_width.clamp(1, width.min(height).max(1) / 2);
    let color = [
        ((color.0 >> 16) & 0xff) as u8,
        ((color.0 >> 8) & 0xff) as u8,
        (color.0 & 0xff) as u8,
        255,
    ];
    let corner_band = radius.ceil() as i32 + stroke_width + 1;
    for y in 0..height {
        for x in 0..width {
            let near_edge = x < stroke_width
                || y < stroke_width
                || x >= width - stroke_width
                || y >= height - stroke_width;
            let near_corner = (x < corner_band || x >= width - corner_band)
                && (y < corner_band || y >= height - corner_band);
            if !near_edge && !near_corner {
                continue;
            }
            let outer = rounded_rect_coverage(width, height, radius, x, y);
            let inner = if x >= stroke_width
                && y >= stroke_width
                && x < width - stroke_width
                && y < height - stroke_width
            {
                rounded_rect_coverage(
                    width - stroke_width * 2,
                    height - stroke_width * 2,
                    (radius - stroke_width as f32).max(0.0),
                    x - stroke_width,
                    y - stroke_width,
                )
            } else {
                0
            };
            let coverage = ((u32::from(outer) * u32::from(255 - inner) + 127) / 255) as u8;
            if coverage == 0 {
                continue;
            }
            blend_surface_pixel(surface, rect.left + x, rect.top + y, color, coverage);
        }
    }
}

pub(super) fn alpha_blend_surface(
    device: &FrozenSurface,
    surface: &FrozenSurface,
    destination: UiRect,
) {
    let width = (destination.right - destination.left).max(1);
    let height = (destination.bottom - destination.top).max(1);
    // SAFETY: Both DCs contain live 32-bit DIBs. The source surface stores premultiplied BGRA,
    // which is the format required by AC_SRC_ALPHA. Normal operation uses a same-size source and
    // destination; the stretch fallback is retained only for allocation/scale failures.
    let _ = unsafe {
        AlphaBlend(
            device.device,
            destination.left,
            destination.top,
            width,
            height,
            surface.device,
            0,
            0,
            surface.width,
            surface.height,
            BLENDFUNCTION {
                BlendOp: 0,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            },
        )
    };
}

pub(super) fn scale_premultiplied_surface(
    source: &FrozenSurface,
    width: u32,
    height: u32,
) -> Result<FrozenSurface, CaptureError> {
    let scaled = FrozenSurface::empty(width, height)?;
    // AlphaBlend's built-in stretch is low quality, while GDI HALFTONE discards the alpha byte.
    // Resample all four premultiplied channels together: area filtering for reduction and bilinear
    // filtering for enlargement. The exact-size result can then be composited 1:1.
    // SAFETY: Flushes this thread's queued GDI drawing before CPU reads the DIB section.
    let _ = unsafe { GdiFlush() };
    // SAFETY: Both slices cover their live DIB allocations for the duration of this call.
    let source_pixels = unsafe { std::slice::from_raw_parts(source.bits, source.byte_length) };
    // SAFETY: `scaled` uniquely owns this writable DIB and no GDI operation runs concurrently.
    let destination_pixels =
        unsafe { std::slice::from_raw_parts_mut(scaled.bits, scaled.byte_length) };
    if width as i32 <= source.width && height as i32 <= source.height {
        area_scale_bgra(
            source_pixels,
            source.width as usize,
            source.height as usize,
            destination_pixels,
            width as usize,
            height as usize,
        );
    } else {
        bilinear_scale_bgra(
            source_pixels,
            source.width as usize,
            source.height as usize,
            destination_pixels,
            width as usize,
            height as usize,
        );
    }
    Ok(scaled)
}

pub(super) fn draw_window_surface(device: &FrozenSurface, surface: &FrozenSurface, bounds: UiRect) {
    let destination = fitted_surface_rect(surface, bounds, true);
    let width = (destination.right - destination.left).max(1);
    let height = (destination.bottom - destination.top).max(1);
    if width != surface.width || height != surface.height {
        if let Ok(scaled) = scale_premultiplied_surface(surface, width as u32, height as u32) {
            alpha_blend_surface(device, &scaled, destination);
            return;
        }
    }
    alpha_blend_surface(device, surface, destination);
}

pub(super) fn draw_surface_to_rect(device: HDC, surface: &FrozenSurface, destination: UiRect) {
    let width = (destination.right - destination.left).max(1);
    let height = (destination.bottom - destination.top).max(1);
    // SAFETY: Both DCs own live DIBs. Destination bounds are positive and GDI performs scaling.
    unsafe {
        SetStretchBltMode(device, HALFTONE);
        StretchBlt(
            device,
            destination.left,
            destination.top,
            width,
            height,
            surface.device,
            0,
            0,
            surface.width,
            surface.height,
            SRCCOPY,
        );
    }
}

pub(super) fn build_blurred_background(
    source: &FrozenSurface,
    block_size: u32,
    reusable: Option<FrozenSurface>,
) -> Result<FrozenSurface, CaptureError> {
    let block_size = block_size.max(1);
    let width = (source.width as u32).div_ceil(block_size);
    let height = (source.height as u32).div_ceil(block_size);
    let destination = reusable
        .filter(|surface| surface.width == width as i32 && surface.height == height as i32)
        .map_or_else(|| FrozenSurface::empty(width, height), Ok)?;
    draw_surface_to_rect(
        destination.device,
        source,
        UiRect {
            left: 0,
            top: 0,
            right: destination.width,
            bottom: destination.height,
        },
    );
    Ok(destination)
}

pub(super) fn apply_dim_wash(
    destination: HDC,
    dimmer: HDC,
    width: i32,
    height: i32,
    alpha: u8,
) -> bool {
    // SAFETY: Callers provide live memory DCs; dimmer has a selected one-pixel black DIB and the
    // destination has a writable DIB at least width by height.
    unsafe {
        AlphaBlend(
            destination,
            0,
            0,
            width,
            height,
            dimmer,
            0,
            0,
            1,
            1,
            BLENDFUNCTION {
                BlendOp: 0,
                BlendFlags: 0,
                SourceConstantAlpha: alpha,
                AlphaFormat: 0,
            },
        )
    }
    .as_bool()
}

pub(super) fn fill_device_rect(device: HDC, rect: RECT, color: COLORREF) {
    // SAFETY: The brush is selected only by FillRect for this call and is deleted exactly once.
    let brush = unsafe { CreateSolidBrush(color) };
    if brush.0 == 0 {
        return;
    }
    // SAFETY: device is a live memory DC and rect is bounded by the caller's paint surface.
    let _ = unsafe { FillRect(device, &rect, brush) };
    // SAFETY: brush is process-owned and no longer selected after FillRect returns.
    unsafe { DeleteObject(brush) };
}

/// Fills the whole surface with pure black carrying [`LIVE_UNDRAWN_ALPHA`] in the alpha byte.
/// Every GDI chrome draw that follows zeroes the alpha of the pixels it touches, which is what
/// lets the present pass recognize black chrome as chrome instead of background.
pub(super) fn fill_live_background(surface: &FrozenSurface) {
    // SAFETY: The surface uniquely owns this writable DIB on the overlay thread.
    let pixels = unsafe { std::slice::from_raw_parts_mut(surface.bits, surface.byte_length) };
    for pixel in pixels.chunks_exact_mut(4) {
        pixel[0] = 0;
        pixel[1] = 0;
        pixel[2] = 0;
        pixel[3] = LIVE_UNDRAWN_ALPHA;
    }
}

pub(super) fn draw_outline(device: windows::Win32::Graphics::Gdi::HDC, source: Rect, rect: Rect) {
    // SAFETY: NULL_BRUSH is a valid stock object that must not be deleted.
    let brush = unsafe { GetStockObject(NULL_BRUSH) };
    // SAFETY: Selects the stock hollow brush for an outline-only rectangle.
    let old_brush = unsafe { SelectObject(device, brush) };
    let left = rect.x - source.x;
    let top = rect.y - source.y;
    let right = left.saturating_add(rect.width as i32);
    let bottom = top.saturating_add(rect.height as i32);
    draw_outline_layer(device, left, top, right, bottom, 7, COLORREF(0x0000_0000));
    draw_outline_layer(device, left, top, right, bottom, 3, COLORREF(0x00ff_ff00));
    // SAFETY: Restores the brush that was selected before the outline layers.
    unsafe { SelectObject(device, old_brush) };
}

pub(super) fn draw_outline_layer(
    device: HDC,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    width: i32,
    color: COLORREF,
) {
    // SAFETY: Creates one process-owned pen used only for this off-screen paint operation.
    let pen = unsafe { CreatePen(PS_SOLID, width, color) };
    // SAFETY: device and pen are live GDI handles and the prior object is restored before delete.
    let old_pen = unsafe { SelectObject(device, pen) };
    // SAFETY: Coordinates may be clipped by GDI but do not address memory directly.
    unsafe {
        Rectangle(device, left, top, right, bottom);
        SelectObject(device, old_pen);
        DeleteObject(pen);
    }
}

pub(super) fn draw_resize_handles(device: HDC, source: Rect, rect: Rect, metrics: UiMetrics) {
    let tokens = metrics.region_tokens();
    let left = rect.x - source.x;
    let top = rect.y - source.y;
    let right = left.saturating_add(rect.width as i32);
    let bottom = top.saturating_add(rect.height as i32);
    let middle_x = left.saturating_add((right - left) / 2);
    let middle_y = top.saturating_add((bottom - top) / 2);
    let points = [
        (left, top),
        (middle_x, top),
        (right, top),
        (right, middle_y),
        (right, bottom),
        (middle_x, bottom),
        (left, bottom),
        (left, middle_y),
    ];
    // SAFETY: Creates two process-owned brushes used only on the off-screen composition.
    let (outer, inner) = unsafe {
        (
            CreateSolidBrush(COLORREF(0x0000_0000)),
            CreateSolidBrush(COLORREF(0x00ff_ff00)),
        )
    };
    for (x, y) in points {
        let outer_rect = centered_rect(x, y, tokens.handle_outer_radius);
        let inner_rect = centered_rect(x, y, tokens.handle_inner_radius);
        // SAFETY: The brushes and memory DC are live; GDI clips handles at display edges.
        unsafe {
            FillRect(device, &outer_rect, outer);
            FillRect(device, &inner_rect, inner);
        }
    }
    // SAFETY: The brushes are no longer selected or used after these calls.
    unsafe {
        DeleteObject(outer);
        DeleteObject(inner);
    }
}

pub(super) fn draw_round_box(
    device: HDC,
    rect: UiRect,
    fill: COLORREF,
    border: COLORREF,
    radius: i32,
) {
    // RoundRect's last two parameters are the corner ellipse's WIDTH and HEIGHT - a diameter,
    // not a radius. Callers pass *_corner_radius tokens, so double them here; the CPU
    // rasterizer path (draw_antialiased_rounded_outline) already treats the same tokens as
    // true radii, and the two paths must agree.
    let diameter = radius.saturating_mul(2);
    // SAFETY: Creates temporary process-owned GDI objects for this off-screen paint operation.
    let (brush, pen) = unsafe { (CreateSolidBrush(fill), CreatePen(PS_SOLID, 1, border)) };
    // SAFETY: Objects and DC are live. Prior objects are restored before deletion.
    unsafe {
        let old_brush = SelectObject(device, brush);
        let old_pen = SelectObject(device, pen);
        RoundRect(
            device,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            diameter,
            diameter,
        );
        SelectObject(device, old_pen);
        SelectObject(device, old_brush);
        DeleteObject(pen);
        DeleteObject(brush);
    }
}

pub(super) fn draw_outline_rect(
    device: HDC,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    color: COLORREF,
    width: i32,
) {
    // SAFETY: NULL_BRUSH is a shared stock object; the pen is process-owned for this draw call.
    let (brush, pen) = unsafe {
        (
            GetStockObject(NULL_BRUSH),
            CreatePen(PS_SOLID, width, color),
        )
    };
    // SAFETY: Objects and DC are live. Prior objects are restored before deleting the pen.
    unsafe {
        let old_brush = SelectObject(device, brush);
        let old_pen = SelectObject(device, pen);
        Rectangle(device, left, top, right, bottom);
        SelectObject(device, old_pen);
        SelectObject(device, old_brush);
        DeleteObject(pen);
    }
}

pub(super) fn draw_ellipse(
    device: HDC,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    color: COLORREF,
    width: i32,
) {
    // SAFETY: NULL_BRUSH is shared; the temporary pen is restored and deleted below.
    let (brush, pen) = unsafe {
        (
            GetStockObject(NULL_BRUSH),
            CreatePen(PS_SOLID, width, color),
        )
    };
    // SAFETY: Objects and DC are live for the duration of this off-screen draw operation.
    unsafe {
        let old_brush = SelectObject(device, brush);
        let old_pen = SelectObject(device, pen);
        Ellipse(device, left, top, right, bottom);
        SelectObject(device, old_pen);
        SelectObject(device, old_brush);
        DeleteObject(pen);
    }
}

pub(super) fn draw_filled_ellipse(
    device: HDC,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    color: COLORREF,
) {
    // SAFETY: Creates temporary process-owned objects for this off-screen paint operation.
    let (brush, pen) = unsafe { (CreateSolidBrush(color), CreatePen(PS_SOLID, 1, color)) };
    // SAFETY: Objects and DC are live. Prior objects are restored before deletion.
    unsafe {
        let old_brush = SelectObject(device, brush);
        let old_pen = SelectObject(device, pen);
        Ellipse(device, left, top, right, bottom);
        SelectObject(device, old_pen);
        SelectObject(device, old_brush);
        DeleteObject(pen);
        DeleteObject(brush);
    }
}

pub(super) fn draw_lines(device: HDC, points: &[(i32, i32)], color: COLORREF, width: i32) {
    let Some(&(first_x, first_y)) = points.first() else {
        return;
    };
    // SAFETY: Creates a temporary process-owned pen for this off-screen paint operation.
    let pen = unsafe { CreatePen(PS_SOLID, width, color) };
    // SAFETY: DC and pen are live. GDI clips coordinates and the prior pen is restored.
    unsafe {
        let old_pen = SelectObject(device, pen);
        MoveToEx(device, first_x, first_y, None);
        for &(x, y) in &points[1..] {
            LineTo(device, x, y);
        }
        SelectObject(device, old_pen);
        DeleteObject(pen);
    }
}

pub(super) fn draw_text(
    device: HDC,
    bounds: UiRect,
    value: &str,
    color: COLORREF,
    alignment: TextAlignment,
    font_height: i32,
) {
    let mut text: Vec<u16> = value.encode_utf16().collect();
    let mut native_bounds = RECT {
        left: bounds.left,
        top: bounds.top,
        right: bounds.right,
        bottom: bounds.bottom,
    };
    let horizontal = match alignment {
        TextAlignment::Left => DT_LEFT,
        TextAlignment::Center => DT_CENTER,
    };
    let format = windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT(
        DT_SINGLELINE.0 | DT_VCENTER.0 | DT_NOPREFIX.0 | horizontal.0,
    );
    let font = create_ui_font(font_height);
    // SAFETY: The DC/font and bounds are live and text is valid writable UTF-16 storage.
    unsafe {
        let old_font = SelectObject(device, font);
        SetBkMode(device, TRANSPARENT);
        SetTextColor(device, color);
        DrawTextW(device, &mut text, &mut native_bounds, format);
        SelectObject(device, old_font);
        DeleteObject(font);
    }
}

pub(super) fn measure_ui_text(device: HDC, value: &str, font_height: i32) -> SIZE {
    let text: Vec<u16> = value.encode_utf16().collect();
    let font = create_ui_font(font_height);
    let mut measured = SIZE::default();
    // SAFETY: device/font are live, text is valid UTF-16, and measured is writable storage.
    let succeeded = unsafe {
        let old_font = SelectObject(device, font);
        let succeeded = GetTextExtentPoint32W(device, &text, &mut measured).as_bool();
        SelectObject(device, old_font);
        DeleteObject(font);
        succeeded
    };
    if succeeded {
        measured
    } else {
        SIZE {
            cx: value.chars().count() as i32 * (font_height / 2),
            cy: font_height,
        }
    }
}

pub(super) fn create_ui_font(font_height: i32) -> HFONT {
    // SAFETY: Creates a ClearType font from the registered process-private IoskeleyMono face.
    unsafe {
        CreateFontW(
            -font_height,
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            u32::from(DEFAULT_CHARSET.0),
            0,
            0,
            u32::from(CLEARTYPE_QUALITY.0),
            u32::from(DEFAULT_PITCH.0),
            w!("Ioskeley Mono Medium"),
        )
    }
}

pub(super) fn draw_display_icon(device: HDC, bounds: UiRect, color: COLORREF, metrics: UiMetrics) {
    let tokens = metrics.toolbar_tokens();
    let left = (bounds.left + bounds.right - tokens.icon_size) / 2;
    let top = (bounds.top + bounds.bottom - tokens.icon_size) / 2;
    let body_height = tokens.icon_size * 2 / 3;
    let center_x = left + tokens.icon_size / 2;
    draw_outline_rect(
        device,
        left,
        top,
        left + tokens.icon_size,
        top + body_height,
        color,
        tokens.icon_stroke,
    );
    draw_lines(
        device,
        &[
            (center_x, top + body_height),
            (center_x, top + tokens.icon_size),
        ],
        color,
        tokens.icon_stroke,
    );
    draw_lines(
        device,
        &[
            (center_x - tokens.icon_size / 4, top + tokens.icon_size),
            (center_x + tokens.icon_size / 4, top + tokens.icon_size),
        ],
        color,
        tokens.icon_stroke,
    );
}

pub(super) fn draw_window_icon(device: HDC, bounds: UiRect, color: COLORREF, metrics: UiMetrics) {
    let tokens = metrics.toolbar_tokens();
    let left = (bounds.left + bounds.right - tokens.icon_size) / 2;
    let top = (bounds.top + bounds.bottom - tokens.icon_size) / 2;
    let offset = tokens.icon_size / 4;
    draw_outline_rect(
        device,
        left + offset,
        top,
        left + tokens.icon_size,
        top + tokens.icon_size - offset,
        color,
        tokens.icon_stroke,
    );
    draw_outline_rect(
        device,
        left,
        top + offset,
        left + tokens.icon_size - offset,
        top + tokens.icon_size,
        color,
        tokens.icon_stroke,
    );
}

pub(super) fn draw_region_icon(device: HDC, bounds: UiRect, color: COLORREF, metrics: UiMetrics) {
    let tokens = metrics.toolbar_tokens();
    let left = (bounds.left + bounds.right - tokens.icon_size) / 2;
    let top = (bounds.top + bounds.bottom - tokens.icon_size) / 2;
    let right = left + tokens.icon_size;
    let bottom = top + tokens.icon_size;
    let corner = tokens.icon_size / 3;
    for points in [
        [(left, top + corner), (left, top), (left + corner, top)],
        [(right - corner, top), (right, top), (right, top + corner)],
        [
            (right, bottom - corner),
            (right, bottom),
            (right - corner, bottom),
        ],
        [
            (left + corner, bottom),
            (left, bottom),
            (left, bottom - corner),
        ],
    ] {
        draw_lines(device, &points, color, tokens.icon_stroke);
    }
}

pub(super) fn draw_camera_icon(
    device: HDC,
    left: i32,
    top: i32,
    color: COLORREF,
    metrics: UiMetrics,
) {
    let tokens = metrics.toolbar_tokens();
    let body_top = top + tokens.icon_size / 4;
    draw_outline_rect(
        device,
        left,
        body_top,
        left + tokens.icon_size,
        top + tokens.icon_size,
        color,
        tokens.icon_stroke,
    );
    draw_lines(
        device,
        &[
            (left + tokens.icon_size / 4, body_top),
            (left + tokens.icon_size * 2 / 5, top),
            (left + tokens.icon_size * 3 / 5, top),
            (left + tokens.icon_size * 3 / 4, body_top),
        ],
        color,
        tokens.icon_stroke,
    );
    draw_ellipse(
        device,
        left + tokens.icon_size / 3,
        top + tokens.icon_size / 3,
        left + tokens.icon_size * 2 / 3,
        top + tokens.icon_size * 2 / 3,
        color,
        tokens.icon_stroke,
    );
}

pub(super) fn draw_checkmark(device: HDC, x: i32, y: i32, color: COLORREF, metrics: UiMetrics) {
    draw_lines(
        device,
        &[
            (x - metrics.px(5), y),
            (x - metrics.px(1), y + metrics.px(4)),
            (x + metrics.px(6), y - metrics.px(5)),
        ],
        color,
        metrics.px(2).max(1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_corner_coverage_has_antialiased_edge_pixels() {
        assert_eq!(rounded_rect_coverage(100, 60, 11.0, 0, 0), 0);
        assert!((0..11).any(|y| {
            (0..11).any(|x| {
                let coverage = rounded_rect_coverage(100, 60, 11.0, x, y);
                coverage > 0 && coverage < 255
            })
        }));
        assert_eq!(rounded_rect_coverage(100, 60, 11.0, 11, 0), 255);
        assert_eq!(rounded_rect_coverage(100, 60, 0.0, 0, 0), 255);
        assert!((149..=151).contains(&blend_channel(200, 100, 128)));
    }

    #[test]
    fn window_paint_alpha_blends_edges_without_black_fringes() {
        let background =
            FrozenSurface::new(2, 1, &[10, 20, 30, 255, 10, 20, 30, 255]).expect("background");
        let window =
            FrozenSurface::from_straight_alpha(2, 1, &[200, 100, 50, 0, 100, 50, 200, 128])
                .expect("window surface");
        draw_window_surface(
            &background,
            &window,
            UiRect {
                left: 0,
                top: 0,
                right: 2,
                bottom: 1,
            },
        );
        let pixels = background.pixel_bytes();
        assert_eq!(&pixels[..4], &[10, 20, 30, 255]);
        assert!((54..=56).contains(&pixels[4]));
        assert!((34..=36).contains(&pixels[5]));
        assert!((114..=116).contains(&pixels[6]));
    }

    #[test]
    fn preview_scaling_preserves_premultiplied_alpha() {
        let source = FrozenSurface::from_straight_alpha(
            2,
            2,
            &[
                200, 100, 50, 0, 100, 50, 200, 255, 200, 100, 50, 0, 100, 50, 200, 255,
            ],
        )
        .expect("source");
        let scaled = scale_premultiplied_surface(&source, 12, 12).expect("scaled surface");
        let pixels = scaled.pixel_bytes();
        let left_alpha = pixels[3];
        let right_alpha = pixels[(11 * 4) + 3];
        assert!(left_alpha < 32, "left alpha was {left_alpha}");
        assert!(right_alpha > 223, "right alpha was {right_alpha}");
        assert!(pixels.chunks_exact(4).any(|pixel| {
            pixel[3] > 0
                && pixel[3] < 255
                && u16::from(pixel[0]) <= u16::from(pixel[3])
                && u16::from(pixel[1]) <= u16::from(pixel[3])
                && u16::from(pixel[2]) <= u16::from(pixel[3])
        }));
    }

    #[test]
    fn preview_downscaling_area_filters_color_and_alpha_together() {
        let source = FrozenSurface::from_straight_alpha(
            4,
            1,
            &[0, 0, 0, 0, 0, 0, 0, 0, 200, 100, 50, 255, 200, 100, 50, 255],
        )
        .expect("source");
        let scaled = scale_premultiplied_surface(&source, 1, 1).expect("scaled surface");
        let pixel = scaled.pixel_bytes();
        assert!((99..=101).contains(&pixel[0]));
        assert!((49..=51).contains(&pixel[1]));
        assert!((24..=26).contains(&pixel[2]));
        assert!((127..=128).contains(&pixel[3]));
    }

    #[test]
    fn native_crosshair_has_high_contrast_arms_and_a_precise_hotspot() {
        let (pixels, mask) = high_contrast_cursor_pixels();
        let pixel = |x: usize, y: usize| {
            let offset = (y * REGION_CURSOR_SIZE as usize + x) * 4;
            &pixels[offset..offset + 4]
        };
        assert_eq!(pixel(0, 0), &[0, 0, 0, 0]);
        assert_eq!(
            pixel(10, REGION_CURSOR_CENTER as usize),
            &[255, 255, 255, 255]
        );
        assert_eq!(
            pixel(10, REGION_CURSOR_CENTER as usize + 3),
            &[0, 0, 0, 255]
        );
        assert_eq!(
            pixel(REGION_CURSOR_CENTER as usize, REGION_CURSOR_CENTER as usize),
            &[255, 255, 255, 255]
        );
        assert_ne!(mask[0] & 0x80, 0);
        let center_mask = REGION_CURSOR_CENTER as usize * 8 + REGION_CURSOR_CENTER as usize / 8;
        assert_eq!(
            mask[center_mask] & (0x80 >> (REGION_CURSOR_CENTER as usize % 8)),
            0
        );
    }

    #[test]
    fn spotlight_preserves_native_size_when_the_window_fits() {
        let surface = FrozenSurface::empty(800, 600).expect("test preview surface");
        let destination = fitted_surface_rect(
            &surface,
            UiRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1000,
            },
            false,
        );
        assert_eq!(destination.right - destination.left, 800);
        assert_eq!(destination.bottom - destination.top, 600);
    }
}
