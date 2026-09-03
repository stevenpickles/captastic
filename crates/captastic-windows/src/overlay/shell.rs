//! Win32 window-shell utilities the overlay is built on: DIB surfaces, DPI scope, cursor and
//! font resources, class registration, message-queue hygiene, and error plumbing. Nothing here
//! reads overlay state; a second window on the same thread (a future pinned capture, for
//! instance) can reuse every piece.

use captastic_core::{CaptureError, CaptureErrorKind, Rect};
use windows::core::{Error as WindowsError, PCWSTR};
use windows::Win32::Foundation::{BOOL, HANDLE, HINSTANCE, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{
    AddFontMemResourceEx, CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC,
    DeleteObject, GetMonitorInfoW, InvalidateRect, MonitorFromRect, RemoveFontMemResourceEx,
    SelectObject, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyCursor, IsWindow, LoadCursorW, PeekMessageW, SetCursor,
    SetForegroundWindow, UnregisterClassW, HCURSOR, ICONINFO, IDC_ARROW, IDC_CROSS, IDC_SIZEALL,
    MSG, PM_REMOVE, WM_QUIT,
};

use super::layout::{DisplayEnvironment, UiMetrics, UiRect};
use super::raster::{high_contrast_cursor_pixels, top_down_bitmap_info};

pub(super) const REGION_CURSOR_SIZE: u32 = 64;
pub(super) const REGION_CURSOR_CENTER: i32 = REGION_CURSOR_SIZE as i32 / 2;
pub(super) const IOSKELEY_MONO_MEDIUM: &[u8] =
    include_bytes!("../../assets/fonts/IoskeleyMono-Medium.ttf");

pub(super) struct ThreadDpiContext {
    pub(super) previous: DPI_AWARENESS_CONTEXT,
}

impl ThreadDpiContext {
    pub(super) fn enter_per_monitor_v2() -> Result<Self, CaptureError> {
        // SAFETY: Changes DPI virtualization only for the current overlay worker thread.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.0 == 0 {
            return Err(last_error("set_overlay_dpi_awareness"));
        }
        Ok(Self { previous })
    }
}

impl Drop for ThreadDpiContext {
    fn drop(&mut self) {
        // SAFETY: Restores the exact thread context returned by the successful enter call.
        let _ = unsafe { SetThreadDpiAwarenessContext(self.previous) };
    }
}

pub(super) fn query_display_environment(source: Rect) -> DisplayEnvironment {
    let full_area = UiRect {
        left: 0,
        top: 0,
        right: i32::try_from(source.width).unwrap_or(i32::MAX),
        bottom: i32::try_from(source.height).unwrap_or(i32::MAX),
    };
    let native_source = RECT {
        left: source.x,
        top: source.y,
        right: source
            .right()
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        bottom: source
            .bottom()
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
    };
    // SAFETY: native_source is a valid physical desktop rectangle and the returned monitor handle
    // is used only for synchronous information queries.
    let monitor = unsafe { MonitorFromRect(&native_source, MONITOR_DEFAULTTONEAREST) };
    if monitor.0 == 0 {
        log::warn!(
            "monitor work area unavailable for display bounds={}x{}{:+}{:+}; using full display at 96 DPI",
            source.width,
            source.height,
            source.x,
            source.y
        );
        return DisplayEnvironment {
            work_area: full_area,
            metrics: UiMetrics::new(UiMetrics::BASE_DPI),
        };
    }

    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..MONITORINFO::default()
    };
    // SAFETY: info is initialized with the required size and remains writable for the call.
    let work_area = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        UiRect {
            left: info.rcWork.left.saturating_sub(source.x),
            top: info.rcWork.top.saturating_sub(source.y),
            right: info.rcWork.right.saturating_sub(source.x),
            bottom: info.rcWork.bottom.saturating_sub(source.y),
        }
    } else {
        log::warn!(
            "monitor work area query failed for display bounds={}x{}{:+}{:+}; using full display",
            source.width,
            source.height,
            source.x,
            source.y
        );
        full_area
    };

    let mut dpi_x = UiMetrics::BASE_DPI;
    let mut dpi_y = UiMetrics::BASE_DPI;
    // SAFETY: dpi_x and dpi_y are writable values and monitor is a live handle from the query.
    let dpi_result =
        unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if let Err(error) = dpi_result {
        log::warn!("effective monitor DPI query failed: {error}; using 96 DPI");
        dpi_x = UiMetrics::BASE_DPI;
    } else if dpi_x != dpi_y {
        log::debug!("monitor reports asymmetric DPI x={dpi_x} y={dpi_y}; using x-axis DPI");
    }

    DisplayEnvironment {
        work_area,
        metrics: UiMetrics::new(dpi_x),
    }
}

pub fn flush_desktop_composition() -> Result<(), CaptureError> {
    // SAFETY: DwmFlush has no pointer arguments and synchronizes this process's queued changes.
    unsafe { DwmFlush() }.map_err(|error| overlay_error("flush_overlay_composition", error))
}

pub(super) struct RegionCursor {
    pub(super) handle: HCURSOR,
    pub(super) owned: bool,
}

impl RegionCursor {
    pub(super) fn create() -> Self {
        match Self::create_high_contrast() {
            Ok(handle) => Self {
                handle,
                owned: true,
            },
            Err(_) => {
                // SAFETY: IDC_CROSS is a shared system cursor and remains valid process-wide.
                let handle = unsafe { LoadCursorW(None, IDC_CROSS) }.unwrap_or_default();
                Self {
                    handle,
                    owned: false,
                }
            }
        }
    }

    pub(super) fn create_high_contrast() -> Result<HCURSOR, CaptureError> {
        let (pixels, mask_bits) = high_contrast_cursor_pixels();
        let color = FrozenSurface::new(REGION_CURSOR_SIZE, REGION_CURSOR_SIZE, &pixels)?;
        // SAFETY: mask_bits contains exactly one 1-bit scanline per cursor row.
        let mask = unsafe {
            CreateBitmap(
                REGION_CURSOR_SIZE as i32,
                REGION_CURSOR_SIZE as i32,
                1,
                1,
                Some(mask_bits.as_ptr().cast()),
            )
        };
        if mask.0 == 0 {
            return Err(last_error("create_region_cursor_mask"));
        }
        let info = ICONINFO {
            fIcon: BOOL(0),
            xHotspot: REGION_CURSOR_CENTER as u32,
            yHotspot: REGION_CURSOR_CENTER as u32,
            hbmMask: mask,
            hbmColor: color.bitmap,
        };
        // SAFETY: info references two live, correctly sized bitmaps for the duration of the call.
        let result = unsafe { CreateIconIndirect(&info) };
        // SAFETY: CreateIconIndirect copies both bitmaps; this temporary mask is no longer needed.
        unsafe { DeleteObject(mask) };
        result
            .map(|icon| HCURSOR(icon.0))
            .map_err(|error| overlay_error("create_region_cursor", error))
    }

    pub(super) fn activate(&self) {
        if self.handle.0 != 0 {
            // SAFETY: handle is either this object's live cursor or a shared system cursor.
            unsafe { SetCursor(self.handle) };
        }
    }
}

impl Drop for RegionCursor {
    fn drop(&mut self) {
        if self.owned && self.handle.0 != 0 {
            // SAFETY: Only the cursor created and uniquely owned by this object is destroyed.
            let _ = unsafe { DestroyCursor(self.handle) };
        }
    }
}

pub(super) struct PrivateFontResource(HANDLE);

impl PrivateFontResource {
    pub(super) fn register() -> Result<Self, CaptureError> {
        let byte_length = u32::try_from(IOSKELEY_MONO_MEDIUM.len())
            .map_err(|_| invalid_frame("embedded UI font exceeds Win32 resource limits"))?;
        let mut font_count = 0_u32;
        let font_count_pointer = std::ptr::addr_of_mut!(font_count).cast_const();
        // SAFETY: The embedded font bytes have static lifetime; font_count is writable storage.
        let handle = unsafe {
            AddFontMemResourceEx(
                IOSKELEY_MONO_MEDIUM.as_ptr().cast(),
                byte_length,
                None,
                font_count_pointer,
            )
        };
        if handle.0 == 0 || font_count == 0 {
            return Err(last_error("register_embedded_ui_font"));
        }
        Ok(Self(handle))
    }
}

impl Drop for PrivateFontResource {
    fn drop(&mut self) {
        // SAFETY: This exact handle was returned by AddFontMemResourceEx and is removed once.
        let _ = unsafe { RemoveFontMemResourceEx(self.0) };
    }
}

pub(super) struct FrozenSurface {
    pub(super) device: HDC,
    pub(super) bitmap: HBITMAP,
    pub(super) previous_bitmap: HGDIOBJ,
    pub(super) bits: *mut u8,
    pub(super) byte_length: usize,
    pub(super) width: i32,
    pub(super) height: i32,
}

impl FrozenSurface {
    pub(super) fn new(width: u32, height: u32, pixels: &[u8]) -> Result<Self, CaptureError> {
        Self::allocate(width, height, Some(pixels))
    }

    pub(super) fn empty(width: u32, height: u32) -> Result<Self, CaptureError> {
        Self::allocate(width, height, None)
    }

    pub(super) fn from_straight_alpha(
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<Self, CaptureError> {
        let surface = Self::empty(width, height)?;
        if pixels.len() < surface.byte_length {
            return Err(invalid_frame("overlay alpha buffer is too short"));
        }
        // GDI AlphaBlend requires premultiplied BGRA. Window frames stay straight-alpha for PNG
        // clipboard publication; only this private paint surface is converted.
        for (source, offset) in pixels[..surface.byte_length]
            .chunks_exact(4)
            .zip((0..surface.byte_length).step_by(4))
        {
            let alpha = u32::from(source[3]);
            // SAFETY: `offset` advances by complete pixels inside this surface's DIB allocation.
            unsafe {
                *surface.bits.add(offset) = ((u32::from(source[0]) * alpha + 127) / 255) as u8;
                *surface.bits.add(offset + 1) = ((u32::from(source[1]) * alpha + 127) / 255) as u8;
                *surface.bits.add(offset + 2) = ((u32::from(source[2]) * alpha + 127) / 255) as u8;
                *surface.bits.add(offset + 3) = source[3];
            }
        }
        Ok(surface)
    }

    pub(super) fn write_pixels(&self, pixels: &[u8]) -> Result<(), CaptureError> {
        if pixels.len() < self.byte_length {
            return Err(invalid_frame("overlay pixel buffer is too short"));
        }
        // SAFETY: bits addresses byte_length writable bytes owned by this surface's live DIB.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), self.bits, self.byte_length) };
        Ok(())
    }

    pub(super) fn clear(&self) {
        // SAFETY: bits addresses byte_length writable bytes owned by this surface's live DIB.
        unsafe { std::ptr::write_bytes(self.bits, 0, self.byte_length) };
    }

    pub(super) fn allocate(
        width: u32,
        height: u32,
        pixels: Option<&[u8]>,
    ) -> Result<Self, CaptureError> {
        let width_i32 = i32::try_from(width)
            .map_err(|_| invalid_frame("overlay width exceeds Win32 limits"))?;
        let height_i32 = i32::try_from(height)
            .map_err(|_| invalid_frame("overlay height exceeds Win32 limits"))?;
        let byte_length = usize::try_from(width)
            .ok()
            .and_then(|value| value.checked_mul(height as usize))
            .and_then(|value| value.checked_mul(4))
            .ok_or_else(|| invalid_frame("overlay bitmap size overflowed"))?;
        if pixels.is_some_and(|pixels| pixels.len() < byte_length) {
            return Err(invalid_frame("overlay pixel buffer is too short"));
        }
        // SAFETY: A null compatible DC creates a memory DC compatible with the current screen.
        let device = unsafe { CreateCompatibleDC(None) };
        if device.0 == 0 {
            return Err(last_error("create_overlay_memory_dc"));
        }
        let bitmap_info = top_down_bitmap_info(width_i32, height_i32);
        let mut bitmap_bits = std::ptr::null_mut();
        // SAFETY: bitmap_info and bitmap_bits are valid for the duration of the call. A null
        // section handle requests process-owned storage for the DIB.
        let bitmap = match unsafe {
            CreateDIBSection(
                device,
                &bitmap_info,
                DIB_RGB_COLORS,
                &mut bitmap_bits,
                None,
                0,
            )
        } {
            Ok(bitmap) => bitmap,
            Err(error) => {
                // SAFETY: device was created by CreateCompatibleDC and owns no selected bitmap.
                unsafe { DeleteDC(device) };
                return Err(overlay_error("create_overlay_dib", error));
            }
        };
        if bitmap_bits.is_null() {
            // SAFETY: bitmap and device were created above and have not been selected/retained.
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            return Err(invalid_frame("overlay DIB returned no writable pixels"));
        }
        if let Some(pixels) = pixels {
            // SAFETY: CreateDIBSection allocated byte_length writable bytes for this 32-bit bitmap.
            unsafe {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), bitmap_bits.cast(), byte_length)
            };
        }
        // SAFETY: Selects the newly created bitmap into its owning memory DC.
        let previous_bitmap = unsafe { SelectObject(device, bitmap) };
        if previous_bitmap.0 == 0 || previous_bitmap.0 == -1 {
            // SAFETY: selection failed, so both newly created handles can be released directly.
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(device);
            }
            return Err(last_error("select_overlay_dib"));
        }
        Ok(Self {
            device,
            bitmap,
            previous_bitmap,
            bits: bitmap_bits.cast(),
            byte_length,
            width: width_i32,
            height: height_i32,
        })
    }

    #[cfg(test)]
    pub(super) fn pixel_bytes(&self) -> &[u8] {
        // SAFETY: bits points to byte_length bytes owned by bitmap for this surface's lifetime.
        unsafe { std::slice::from_raw_parts(self.bits, self.byte_length) }
    }
}

impl Drop for FrozenSurface {
    fn drop(&mut self) {
        debug_assert!(!self.bits.is_null());
        debug_assert!(self.byte_length > 0);
        // SAFETY: Restores the original object before releasing the process-owned bitmap and DC.
        unsafe {
            SelectObject(self.device, self.previous_bitmap);
            DeleteObject(self.bitmap);
            DeleteDC(self.device);
        }
    }
}

/// Removes a pending `WM_QUIT` from this thread's queue, if one is waiting.
///
/// `DestroyWindow` dispatches `WM_DESTROY` synchronously, and the overlay window procedure answers
/// it with `PostQuitMessage`. When an overlay tears down without running its message loop to the
/// end, that quit stays latched on the thread as per-thread state and would end the very next
/// overlay run before it pumped a single message. The filter matches only `WM_QUIT`, so no
/// window message can be discarded by mistake.
pub(super) fn drain_pending_quit() {
    let mut message = MSG::default();
    // SAFETY: message is writable storage and this thread owns the queue being peeked.
    let _ = unsafe { PeekMessageW(&mut message, None, WM_QUIT, WM_QUIT, PM_REMOVE) };
}

pub(super) fn invalidate(hwnd: HWND) {
    // SAFETY: Invalidates the complete client area of the live overlay without erasing it first.
    let _ = unsafe { InvalidateRect(hwnd, None, false) };
}

pub(super) fn capture_pointer(hwnd: HWND) {
    // SAFETY: Captures mouse input to the live overlay for the duration of an active drag.
    let _ = unsafe { SetCapture(hwnd) };
}

pub(super) fn release_pointer_capture() {
    // SAFETY: Best-effort release on the overlay thread after the matching button-up message.
    let _ = unsafe { ReleaseCapture() };
}

pub(super) fn consume_self_initiated_capture_change(releasing_pointer_capture: &mut bool) -> bool {
    std::mem::take(releasing_pointer_capture)
}

pub(super) fn screen_point(source: Rect, lparam: LPARAM) -> POINT {
    let packed = lparam.0 as u32;
    let x = (packed as u16) as i16 as i32;
    let y = ((packed >> 16) as u16) as i16 as i32;
    POINT {
        x: source.x.saturating_add(x),
        y: source.y.saturating_add(y),
    }
}

pub(super) fn set_arrow_cursor() {
    // SAFETY: IDC_ARROW is a shared system cursor that requires no explicit destruction.
    if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_ARROW) } {
        // SAFETY: cursor is a live shared system cursor.
        unsafe { SetCursor(cursor) };
    }
}

pub(super) fn set_move_cursor() {
    // SAFETY: IDC_SIZEALL is a shared system cursor that requires no explicit destruction.
    if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_SIZEALL) } {
        // SAFETY: cursor is a live shared system cursor.
        unsafe { SetCursor(cursor) };
    }
}

pub(super) fn restore_input_context(previous_foreground: HWND) {
    // SAFETY: IDC_ARROW is a shared system cursor that does not require destruction.
    if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_ARROW) } {
        // SAFETY: Restores the ordinary pointer after Captastic's directional overlay cursors.
        unsafe { SetCursor(cursor) };
    }
    if previous_foreground.0 != 0 {
        // SAFETY: The handle was observed immediately before the overlay opened. IsWindow checks
        // that it still represents a live window before Captastic returns foreground focus.
        if unsafe { IsWindow(previous_foreground) }.as_bool() {
            // SAFETY: This is a best-effort restoration from the foreground overlay thread.
            unsafe { SetForegroundWindow(previous_foreground) };
        }
    }
}

pub(super) fn overlay_error(operation: &'static str, error: WindowsError) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-overlay",
        operation,
        message: error.to_string(),
        retryable: true,
        native_code: Some(i64::from(error.code().0)),
    }
}

pub(super) fn last_error(operation: &'static str) -> CaptureError {
    overlay_error(operation, WindowsError::from_win32())
}

pub(super) fn invalid_frame(message: impl Into<String>) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::InvalidFrame,
        backend: "windows-overlay",
        operation: "validate_frame",
        message: message.into(),
        retryable: false,
        native_code: None,
    }
}

pub(super) fn duration_ns(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

pub(super) struct ClassRegistration {
    pub(super) class_name: PCWSTR,
    pub(super) instance: HINSTANCE,
}

impl Drop for ClassRegistration {
    fn drop(&mut self) {
        // SAFETY: Balances this thread's successful RegisterClassW after all windows are gone.
        let _ = unsafe { UnregisterClassW(self.class_name, self.instance) };
    }
}

#[cfg(test)]
mod tests {
    use super::super::raster::create_ui_font;
    use super::super::UI_FONT_HEIGHT;
    use super::*;
    use windows::Win32::Graphics::Gdi::GetTextFaceW;

    #[test]
    fn window_paint_surface_premultiplies_straight_alpha() {
        let surface =
            FrozenSurface::from_straight_alpha(2, 1, &[100, 50, 200, 128, 11, 22, 33, 255])
                .expect("premultiplied surface");
        assert_eq!(surface.pixel_bytes(), &[50, 25, 100, 128, 11, 22, 33, 255]);
    }

    #[test]
    fn embedded_ioskeley_font_registers_process_privately() {
        let _font_resource =
            PrivateFontResource::register().expect("register embedded IoskeleyMono font");
        let surface = FrozenSurface::empty(8, 8).expect("font test surface");
        let font = create_ui_font(UI_FONT_HEIGHT);
        assert_ne!(font.0, 0);
        let mut face_name = [0_u16; 64];
        // SAFETY: The font and DC are live; the previous object is restored before deletion.
        let copied = unsafe {
            let previous = SelectObject(surface.device, font);
            let copied = GetTextFaceW(surface.device, Some(&mut face_name));
            SelectObject(surface.device, previous);
            DeleteObject(font);
            copied
        };
        assert!(copied > 0);
        let length = face_name
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(face_name.len());
        assert_eq!(
            String::from_utf16_lossy(&face_name[..length]),
            "Ioskeley Mono Medium"
        );
    }
}
