use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use captastic_core::{
    CaptureError, CaptureErrorKind, CpuFrame, FrameMetadata, FrameOrigin, PixelFormat, Rect,
};
use windows::core::{w, Error as WindowsError, PCWSTR};
use windows::Win32::Foundation::{
    BOOL, COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
#[cfg(test)]
use windows::Win32::Graphics::Gdi::GetTextFaceW;
use windows::Win32::Graphics::Gdi::{
    AddFontMemResourceEx, AlphaBlend, BeginPaint, BitBlt, CreateBitmap, CreateCompatibleDC,
    CreateDIBSection, CreateFontW, CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW,
    Ellipse, EndPaint, FillRect, GetStockObject, GetTextExtentPoint32W, InvalidateRect, LineTo,
    MoveToEx, Rectangle, RemoveFontMemResourceEx, RoundRect, SelectObject, SetBkMode,
    SetStretchBltMode, SetTextColor, StretchBlt, UpdateWindow, AC_SRC_ALPHA, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_MEDIUM,
    HALFTONE, HBITMAP, HDC, HFONT, HGDIOBJ, NULL_BRUSH, PAINTSTRUCT, PS_SOLID, RGBQUAD, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyCursor, DestroyWindow,
    DispatchMessageW, EnumWindows, GetAncestor, GetClassNameW, GetForegroundWindow,
    GetLastActivePopup, GetMessageW, GetShellWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, IsIconic, IsWindow, IsWindowVisible, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetCursor, SetForegroundWindow, SetWindowLongPtrW, ShowWindow,
    TranslateMessage, UnregisterClassW, CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW,
    GA_ROOTOWNER, GWLP_USERDATA, GWL_EXSTYLE, HCURSOR, ICONINFO, IDC_ARROW, IDC_CROSS, IDC_SIZEALL,
    IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, MSG, SW_SHOW, WM_CLOSE, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WNDCLASSW, WS_EX_APPWINDOW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

#[cfg(test)]
use crate::window_capture::scaled_dimensions;
use crate::window_capture::{capture_window_thumbnail, capture_window_visual};

const CLASS_NAME: PCWSTR = w!("CaptasticFrozenSelectionOverlay");
const DRAG_THRESHOLD: i32 = 4;
const DIM_ALPHA: u8 = 128;
const HANDLE_HIT_RADIUS: i32 = 9;
const HANDLE_OUTER_RADIUS: i32 = 6;
const HANDLE_INNER_RADIUS: i32 = 3;
const MIN_REGION_SIZE: i64 = 8;
const DIMENSION_LABEL_HEIGHT: i32 = 34;
const REGION_CURSOR_SIZE: u32 = 64;
const REGION_CURSOR_CENTER: i32 = REGION_CURSOR_SIZE as i32 / 2;
const TOOLBAR_WIDTH: i32 = 600;
const TOOLBAR_HEIGHT: i32 = 82;
const TOOLBAR_BOTTOM_MARGIN: i32 = 36;
const TOOLBAR_CORNER_RADIUS: i32 = 18;
const WINDOW_THUMBNAIL_MAX_PIXELS: u64 = 1_200_000;
const UI_FONT_HEIGHT: i32 = 21;
const MENU_WIDTH: i32 = 320;
const MENU_HEIGHT: i32 = 164;
const NO_TOOLBAR_POSITION: u64 = u64::MAX;
const CAPTURE_TOOL_FULL_DISPLAY: u8 = 0;
const CAPTURE_TOOL_WINDOW: u8 = 1;
const CAPTURE_TOOL_REGION: u8 = 2;
const IOSKELEY_MONO_MEDIUM: &[u8] = include_bytes!("../assets/fonts/IoskeleyMono-Medium.ttf");

static LAST_TOOLBAR_POSITION: AtomicU64 = AtomicU64::new(NO_TOOLBAR_POSITION);
static TOOLBAR_POSITION_LOADED: AtomicBool = AtomicBool::new(false);
static CAPTURE_HISTORY_LOADED: AtomicBool = AtomicBool::new(false);
static LAST_CAPTURE_TOOL: AtomicU8 = AtomicU8::new(CAPTURE_TOOL_REGION);
static LAST_CAPTURED_REGION: Mutex<Option<Rect>> = Mutex::new(None);

thread_local! {
    static OVERLAY_RESOURCE_CACHE: RefCell<Option<OverlayResourceCache>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionKind {
    Display,
    Region,
    Window,
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
    pub selection_ns: u64,
    pub preparation_ns: u64,
    pub window_overview_ns: Option<u64>,
    pub window_preview_count: usize,
    pub window_preview_bytes: usize,
    pub(crate) window_frame: Option<CpuFrame>,
}

#[derive(Clone, Default)]
pub struct OverlayController {
    inner: Arc<OverlayControllerInner>,
}

#[derive(Default)]
struct OverlayControllerInner {
    hwnd: AtomicIsize,
    cancelled: AtomicBool,
}

impl OverlayController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        let hwnd = self.inner.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            // SAFETY: hwnd was published by the live overlay thread. Posting does not retain it.
            let _ = unsafe { PostMessageW(HWND(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        }
    }
}

pub fn select_from_frozen_frame(
    frame: &CpuFrame,
) -> Result<Option<OverlaySelection>, CaptureError> {
    select_from_frozen_frame_with_controller(frame, &OverlayController::new())
}

pub fn select_from_frozen_frame_with_controller(
    frame: &CpuFrame,
    controller: &OverlayController,
) -> Result<Option<OverlaySelection>, CaptureError> {
    let preparation_started = Instant::now();
    if controller.inner.cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    let _dpi_context = ThreadDpiContext::enter_per_monitor_v2()?;
    validate_frame(frame)?;
    let source = frame.metadata.source_rect;
    // SAFETY: Reads the current foreground window without retaining or mutating it.
    let previous_foreground = unsafe { GetForegroundWindow() };
    let pixels = tight_pixels(frame)?;
    let cached = take_overlay_resource_cache(frame.width, frame.height);
    let (
        surface,
        back_buffer,
        dimmer,
        blurred_background,
        cached_overview_surface,
        region_cursor,
        font_resource,
    ) = if let Some(cached) = cached {
        cached.surface.write_pixels(&pixels)?;
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
        (
            FrozenSurface::new(frame.width, frame.height, &pixels)?,
            FrozenSurface::empty(frame.width, frame.height)?,
            FrozenSurface::new(1, 1, &[0, 0, 0, 255])?,
            None,
            None,
            RegionCursor::create(),
            PrivateFontResource::register()?,
        )
    };
    let toolbar_position = remembered_toolbar_position(surface.width, surface.height)
        .unwrap_or_else(|| ToolbarLayout::default_origin(surface.width, surface.height));
    let last_region =
        Some(remembered_last_region(source).unwrap_or_else(|| default_region_for_source(source)));
    let tool = remembered_capture_tool();
    let (selection, selection_kind) = initial_selection(tool, last_region, source);
    let mut state = Box::new(OverlayState {
        source,
        surface,
        back_buffer,
        dimmer,
        blurred_background: None,
        cached_blurred_background: blurred_background,
        window_assets_ready: false,
        windows: None,
        hovered: None,
        anchor: None,
        dragging: false,
        selection,
        selection_kind,
        selected_window: None,
        selected_window_frame: None,
        hovered_handle: None,
        resizing: None,
        moving_region: None,
        last_region,
        tool,
        options_open: false,
        dim_background: true,
        hovered_control: None,
        toolbar_position,
        toolbar_drag: None,
        reference_metadata: frame.metadata.clone(),
        window_preview: None,
        window_thumbnails: Vec::new(),
        window_overview_cache: cached_overview_surface.map(|surface| WindowOverviewCache {
            surface,
            dim_background: false,
        }),
        region_cursor,
        _font_resource: font_resource,
        previous_foreground,
        result: None,
        started: Instant::now(),
        preparation_ns: duration_ns(preparation_started.elapsed()),
        window_overview_ns: None,
    });
    if tool == CaptureTool::Window {
        build_window_overview(&mut state);
    }
    run_overlay(state, controller)
}

struct ThreadDpiContext {
    previous: DPI_AWARENESS_CONTEXT,
}

impl ThreadDpiContext {
    fn enter_per_monitor_v2() -> Result<Self, CaptureError> {
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

struct OverlayState {
    source: Rect,
    surface: FrozenSurface,
    back_buffer: FrozenSurface,
    dimmer: FrozenSurface,
    blurred_background: Option<FrozenSurface>,
    cached_blurred_background: Option<FrozenSurface>,
    window_assets_ready: bool,
    windows: Option<Vec<WindowCandidate>>,
    hovered: Option<WindowCandidate>,
    anchor: Option<POINT>,
    dragging: bool,
    selection: Option<Rect>,
    selection_kind: Option<SelectionKind>,
    selected_window: Option<NativeWindowHandle>,
    selected_window_frame: Option<CpuFrame>,
    hovered_handle: Option<ResizeHandle>,
    resizing: Option<ResizeDrag>,
    moving_region: Option<MoveDrag>,
    last_region: Option<Rect>,
    tool: CaptureTool,
    options_open: bool,
    dim_background: bool,
    hovered_control: Option<ToolbarControl>,
    toolbar_position: POINT,
    toolbar_drag: Option<ToolbarDrag>,
    reference_metadata: FrameMetadata,
    window_preview: Option<WindowPreviewState>,
    window_thumbnails: Vec<WindowThumbnail>,
    window_overview_cache: Option<WindowOverviewCache>,
    region_cursor: RegionCursor,
    _font_resource: PrivateFontResource,
    previous_foreground: HWND,
    result: Option<OverlaySelection>,
    started: Instant,
    preparation_ns: u64,
    window_overview_ns: Option<u64>,
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

fn take_overlay_resource_cache(width: u32, height: u32) -> Option<OverlayResourceCache> {
    OVERLAY_RESOURCE_CACHE.with(|cache| {
        cache.borrow_mut().take().filter(|resources| {
            resources.surface.width == width as i32 && resources.surface.height == height as i32
        })
    })
}

pub fn clear_overlay_resource_cache() {
    OVERLAY_RESOURCE_CACHE.with(|cache| {
        cache.borrow_mut().take();
    });
}

fn cache_overlay_state(state: Box<OverlayState>) -> Option<OverlaySelection> {
    let OverlayState {
        result,
        surface,
        back_buffer,
        dimmer,
        blurred_background,
        cached_blurred_background,
        window_overview_cache,
        region_cursor,
        _font_resource,
        ..
    } = *state;
    let blurred_background = blurred_background.or(cached_blurred_background);
    let overview_surface = window_overview_cache.map(|cache| cache.surface);
    OVERLAY_RESOURCE_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(OverlayResourceCache {
            surface,
            back_buffer,
            dimmer,
            blurred_background,
            overview_surface,
            region_cursor,
            font_resource: _font_resource,
        });
    });
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureTool {
    FullDisplay,
    Window,
    Region,
}

impl CaptureTool {
    const fn from_selection_kind(kind: SelectionKind) -> Self {
        match kind {
            SelectionKind::Display => Self::FullDisplay,
            SelectionKind::Region => Self::Region,
            SelectionKind::Window => Self::Window,
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::FullDisplay => CAPTURE_TOOL_FULL_DISPLAY,
            Self::Window => CAPTURE_TOOL_WINDOW,
            Self::Region => CAPTURE_TOOL_REGION,
        }
    }

    const fn from_config(tool: captastic_config::CaptureTool) -> Self {
        match tool {
            captastic_config::CaptureTool::FullDisplay => Self::FullDisplay,
            captastic_config::CaptureTool::Window => Self::Window,
            captastic_config::CaptureTool::Region => Self::Region,
        }
    }

    const fn to_config(self) -> captastic_config::CaptureTool {
        match self {
            Self::FullDisplay => captastic_config::CaptureTool::FullDisplay,
            Self::Window => captastic_config::CaptureTool::Window,
            Self::Region => captastic_config::CaptureTool::Region,
        }
    }
}

#[derive(Clone, Copy)]
enum TextAlignment {
    Left,
    Center,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolbarControl {
    Background,
    FullDisplay,
    Window,
    Region,
    Options,
    Capture,
    DimBackground,
    ClipboardDestination,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UiRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl UiRect {
    fn contains(self, point: POINT) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }
}

#[derive(Clone, Copy)]
struct ToolbarLayout {
    bounds: UiRect,
    drag_handle: UiRect,
    full_display: UiRect,
    window: UiRect,
    region: UiRect,
    options: UiRect,
    capture: UiRect,
    menu: UiRect,
    dim_background: UiRect,
    clipboard_destination: UiRect,
    cancel: UiRect,
}

#[derive(Clone, Copy)]
struct ToolbarDrag {
    pointer_offset: POINT,
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

struct WindowThumbnail {
    handle: NativeWindowHandle,
    surface: FrozenSurface,
    corner_radius_px: f32,
}

struct WindowOverviewCache {
    surface: FrozenSurface,
    dim_background: bool,
}

struct RegionCursor {
    handle: HCURSOR,
    owned: bool,
}

impl RegionCursor {
    fn create() -> Self {
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

    fn create_high_contrast() -> Result<HCURSOR, CaptureError> {
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

    fn activate(&self) {
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

fn high_contrast_cursor_pixels() -> (Vec<u8>, Vec<u8>) {
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

struct PrivateFontResource(HANDLE);

impl PrivateFontResource {
    fn register() -> Result<Self, CaptureError> {
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

impl WindowPreviewState {
    fn handle(&self) -> NativeWindowHandle {
        match self {
            Self::Ready(preview) => preview.handle,
            Self::Unavailable(handle) => *handle,
        }
    }
}

struct FrozenSurface {
    device: HDC,
    bitmap: HBITMAP,
    previous_bitmap: HGDIOBJ,
    bits: *mut u8,
    byte_length: usize,
    width: i32,
    height: i32,
}

impl FrozenSurface {
    fn new(width: u32, height: u32, pixels: &[u8]) -> Result<Self, CaptureError> {
        Self::allocate(width, height, Some(pixels))
    }

    fn empty(width: u32, height: u32) -> Result<Self, CaptureError> {
        Self::allocate(width, height, None)
    }

    fn from_straight_alpha(width: u32, height: u32, pixels: &[u8]) -> Result<Self, CaptureError> {
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

    fn write_pixels(&self, pixels: &[u8]) -> Result<(), CaptureError> {
        if pixels.len() < self.byte_length {
            return Err(invalid_frame("overlay pixel buffer is too short"));
        }
        // SAFETY: bits addresses byte_length writable bytes owned by this surface's live DIB.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), self.bits, self.byte_length) };
        Ok(())
    }

    fn allocate(width: u32, height: u32, pixels: Option<&[u8]>) -> Result<Self, CaptureError> {
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
    fn pixel_bytes(&self) -> &[u8] {
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

#[derive(Clone, Copy)]
struct WindowCandidate {
    handle: NativeWindowHandle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizeHandle {
    NorthWest,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
}

#[derive(Clone, Copy)]
struct ResizeDrag {
    handle: ResizeHandle,
    original: Rect,
}

#[derive(Clone, Copy)]
struct MoveDrag {
    original: Rect,
    pointer_origin: POINT,
}

impl ToolbarLayout {
    fn default_origin(client_width: i32, client_height: i32) -> POINT {
        Self::clamp_origin(
            client_width,
            client_height,
            POINT {
                x: (client_width - TOOLBAR_WIDTH) / 2,
                y: client_height - TOOLBAR_HEIGHT - TOOLBAR_BOTTOM_MARGIN,
            },
        )
    }

    fn clamp_origin(client_width: i32, client_height: i32, origin: POINT) -> POINT {
        POINT {
            x: origin.x.clamp(8, (client_width - TOOLBAR_WIDTH - 8).max(8)),
            y: origin
                .y
                .clamp(8, (client_height - TOOLBAR_HEIGHT - 8).max(8)),
        }
    }

    fn new(client_width: i32, client_height: i32, origin: POINT) -> Self {
        let origin = Self::clamp_origin(client_width, client_height, origin);
        let left = origin.x;
        let top = origin.y;
        let bounds = UiRect {
            left,
            top,
            right: left + TOOLBAR_WIDTH,
            bottom: top + TOOLBAR_HEIGHT,
        };
        let menu_left =
            (bounds.right - MENU_WIDTH - 12).clamp(8, (client_width - MENU_WIDTH - 8).max(8));
        let menu_top = if top >= MENU_HEIGHT + 16 {
            top - MENU_HEIGHT - 10
        } else {
            bounds.bottom + 10
        };
        let menu = UiRect {
            left: menu_left,
            top: menu_top,
            right: menu_left + MENU_WIDTH,
            bottom: menu_top + MENU_HEIGHT,
        };
        Self {
            bounds,
            drag_handle: UiRect {
                left: left + 8,
                top: top + 10,
                right: left + 44,
                bottom: top + 72,
            },
            full_display: UiRect {
                left: left + 48,
                top: top + 10,
                right: left + 112,
                bottom: top + 72,
            },
            window: UiRect {
                left: left + 112,
                top: top + 10,
                right: left + 176,
                bottom: top + 72,
            },
            region: UiRect {
                left: left + 176,
                top: top + 10,
                right: left + 240,
                bottom: top + 72,
            },
            options: UiRect {
                left: left + 270,
                top: top + 10,
                right: left + 402,
                bottom: top + 72,
            },
            capture: UiRect {
                left: left + 418,
                top: top + 10,
                right: left + 584,
                bottom: top + 72,
            },
            dim_background: UiRect {
                left: menu.left + 6,
                top: menu.top + 6,
                right: menu.right - 6,
                bottom: menu.top + 56,
            },
            clipboard_destination: UiRect {
                left: menu.left + 6,
                top: menu.top + 56,
                right: menu.right - 6,
                bottom: menu.top + 106,
            },
            cancel: UiRect {
                left: menu.left + 6,
                top: menu.top + 106,
                right: menu.right - 6,
                bottom: menu.bottom - 6,
            },
            menu,
        }
    }

    fn hit_test(self, point: POINT, options_open: bool) -> Option<ToolbarControl> {
        if options_open && self.menu.contains(point) {
            if self.dim_background.contains(point) {
                return Some(ToolbarControl::DimBackground);
            }
            if self.clipboard_destination.contains(point) {
                return Some(ToolbarControl::ClipboardDestination);
            }
            if self.cancel.contains(point) {
                return Some(ToolbarControl::Cancel);
            }
            return Some(ToolbarControl::Background);
        }
        if !self.bounds.contains(point) {
            return None;
        }
        if self.full_display.contains(point) {
            Some(ToolbarControl::FullDisplay)
        } else if self.window.contains(point) {
            Some(ToolbarControl::Window)
        } else if self.region.contains(point) {
            Some(ToolbarControl::Region)
        } else if self.options.contains(point) {
            Some(ToolbarControl::Options)
        } else if self.capture.contains(point) {
            Some(ToolbarControl::Capture)
        } else {
            Some(ToolbarControl::Background)
        }
    }
}

fn remembered_toolbar_position(client_width: i32, client_height: i32) -> Option<POINT> {
    let mut packed = LAST_TOOLBAR_POSITION.load(Ordering::Acquire);
    if packed == NO_TOOLBAR_POSITION && !TOOLBAR_POSITION_LOADED.swap(true, Ordering::AcqRel) {
        if let Some(position) = load_toolbar_position_from_disk() {
            packed = pack_toolbar_position(position);
            LAST_TOOLBAR_POSITION.store(packed, Ordering::Release);
        }
    }
    (packed != NO_TOOLBAR_POSITION).then(|| {
        ToolbarLayout::clamp_origin(client_width, client_height, unpack_toolbar_position(packed))
    })
}

fn remembered_capture_tool() -> CaptureTool {
    ensure_capture_history_loaded();
    match LAST_CAPTURE_TOOL.load(Ordering::Acquire) {
        CAPTURE_TOOL_FULL_DISPLAY => CaptureTool::FullDisplay,
        CAPTURE_TOOL_WINDOW => CaptureTool::Window,
        _ => CaptureTool::Region,
    }
}

fn initial_selection(
    tool: CaptureTool,
    last_region: Option<Rect>,
    source: Rect,
) -> (Option<Rect>, Option<SelectionKind>) {
    match tool {
        CaptureTool::FullDisplay => (Some(source), Some(SelectionKind::Display)),
        CaptureTool::Window => (None, None),
        CaptureTool::Region => {
            let region = last_region
                .map(|region| fit_region_to_source(region, source))
                .unwrap_or_else(|| default_region_for_source(source));
            (Some(region), Some(SelectionKind::Region))
        }
    }
}

fn remembered_last_region(source: Rect) -> Option<Rect> {
    ensure_capture_history_loaded();
    LAST_CAPTURED_REGION
        .lock()
        .ok()
        .and_then(|region| region.as_ref().copied())
        .map(|region| fit_region_to_source(region, source))
}

fn remember_capture_history(kind: SelectionKind, confirmed_region: Option<Rect>) {
    let tool = CaptureTool::from_selection_kind(kind);
    LAST_CAPTURE_TOOL.store(tool.code(), Ordering::Release);
    if let Some(region) = confirmed_region {
        if let Ok(mut remembered) = LAST_CAPTURED_REGION.lock() {
            *remembered = Some(region);
        }
    }
    let persisted_region = confirmed_region.map(|region| captastic_config::CaptureRegion {
        x: region.x,
        y: region.y,
        width: region.width,
        height: region.height,
    });
    if let Err(error) = captastic_config::save_capture_history(tool.to_config(), persisted_region) {
        log::warn!("failed to save capture history: {error}");
    }
}

fn ensure_capture_history_loaded() {
    if CAPTURE_HISTORY_LOADED.swap(true, Ordering::AcqRel) {
        return;
    }
    match captastic_config::load_capture_history() {
        Ok(history) => {
            if let Some(tool) = history.tool {
                LAST_CAPTURE_TOOL.store(CaptureTool::from_config(tool).code(), Ordering::Release);
            }
            if let Some(region) = history.region {
                if let Ok(mut remembered) = LAST_CAPTURED_REGION.lock() {
                    *remembered = Some(Rect {
                        x: region.x,
                        y: region.y,
                        width: region.width,
                        height: region.height,
                    });
                }
            }
        }
        Err(error) => log::warn!("failed to load capture history: {error}"),
    }
}

fn default_region_for_source(source: Rect) -> Rect {
    let width = (source.width / 2).max(1);
    let height = (source.height / 2).max(1);
    Rect {
        x: source
            .x
            .saturating_add(((source.width - width) / 2).min(i32::MAX as u32) as i32),
        y: source
            .y
            .saturating_add(((source.height - height) / 2).min(i32::MAX as u32) as i32),
        width,
        height,
    }
}

fn fit_region_to_source(region: Rect, source: Rect) -> Rect {
    let width = region.width.min(source.width);
    let height = region.height.min(source.height);
    let source_right = i64::from(source.x) + i64::from(source.width);
    let source_bottom = i64::from(source.y) + i64::from(source.height);
    let maximum_x = source_right - i64::from(width);
    let maximum_y = source_bottom - i64::from(height);
    Rect {
        x: i64::from(region.x).clamp(i64::from(source.x), maximum_x) as i32,
        y: i64::from(region.y).clamp(i64::from(source.y), maximum_y) as i32,
        width,
        height,
    }
}

fn remember_toolbar_position(position: POINT) {
    TOOLBAR_POSITION_LOADED.store(true, Ordering::Release);
    LAST_TOOLBAR_POSITION.store(pack_toolbar_position(position), Ordering::Release);
    if let Err(error) = captastic_config::save_overlay_position(position.x, position.y) {
        log::warn!("failed to save overlay position: {error}");
    }
}

fn pack_toolbar_position(position: POINT) -> u64 {
    (u64::from(position.x as u32) << 32) | u64::from(position.y as u32)
}

fn unpack_toolbar_position(packed: u64) -> POINT {
    POINT {
        x: (packed >> 32) as u32 as i32,
        y: packed as u32 as i32,
    }
}

fn load_toolbar_position_from_disk() -> Option<POINT> {
    match captastic_config::load_overlay_position() {
        Ok(position) => position.map(|(x, y)| POINT { x, y }),
        Err(error) => {
            log::warn!("failed to load overlay position: {error}");
            None
        }
    }
}

fn run_overlay(
    state: Box<OverlayState>,
    controller: &OverlayController,
) -> Result<Option<OverlaySelection>, CaptureError> {
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
    let class_guard = ClassRegistration { instance };
    let source = state.source;
    let width = i32::try_from(source.width)
        .map_err(|_| invalid_frame("overlay width exceeds Win32 limits"))?;
    let height = i32::try_from(source.height)
        .map_err(|_| invalid_frame("overlay height exceeds Win32 limits"))?;
    let state_pointer = Box::into_raw(state);
    // SAFETY: The registered class and callback are valid. state_pointer remains allocated until
    // the message loop exits; WM_NCCREATE stores it as window user data.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
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
        // SAFETY: Window creation failed, so Win32 did not retain the state allocation.
        let state = unsafe { Box::from_raw(state_pointer) };
        let _ = cache_overlay_state(state);
        return Err(last_error("create_overlay_window"));
    }
    controller.inner.hwnd.store(hwnd.0, Ordering::Release);
    if controller.inner.cancelled.load(Ordering::Acquire) {
        // SAFETY: hwnd was just created on this thread and cancellation was requested.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
    // SAFETY: hwnd is the live overlay window on this thread.
    unsafe {
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
    }
    let mut message = MSG::default();
    loop {
        // SAFETY: message is writable storage and this thread owns the overlay message loop.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            // SAFETY: hwnd is still owned by this thread if message retrieval fails.
            let _ = unsafe { DestroyWindow(hwnd) };
            // SAFETY: The callback will no longer access state after DestroyWindow returns.
            let state = unsafe { Box::from_raw(state_pointer) };
            controller.inner.hwnd.store(0, Ordering::Release);
            let previous_foreground = state.previous_foreground;
            let _ = cache_overlay_state(state);
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
    controller.inner.hwnd.store(0, Ordering::Release);
    let previous_foreground = state.previous_foreground;
    let result = cache_overlay_state(state);
    restore_input_context(previous_foreground);
    drop(class_guard);
    Ok(result)
}

struct ClassRegistration {
    instance: HINSTANCE,
}

impl Drop for ClassRegistration {
    fn drop(&mut self) {
        // SAFETY: Balances this thread's successful RegisterClassW after all windows are gone.
        let _ = unsafe { UnregisterClassW(CLASS_NAME, self.instance) };
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
        // SAFETY: Stores the Box pointer passed to CreateWindowExW for later callbacks.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
        return LRESULT(1);
    }
    // SAFETY: Retrieves only the pointer installed during WM_NCCREATE.
    let state_pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut OverlayState;
    if message == WM_NCDESTROY {
        // SAFETY: Prevents any later callback from observing the state pointer.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        // SAFETY: Default non-client cleanup for this live window.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    if state_pointer.is_null() {
        // SAFETY: No application state is available, so default handling is required.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: The Box remains alive for the full message loop and callbacks are serialized here.
    let state = unsafe { &mut *state_pointer };
    match message {
        WM_MOUSEMOVE => {
            let point = screen_point(state.source, lparam);
            let local = local_point(state.source, point);
            let previous_hovered = state.hovered.map(|candidate| candidate.handle);
            let previous_control = state.hovered_control;
            if let Some(drag) = state.toolbar_drag {
                state.toolbar_position = ToolbarLayout::clamp_origin(
                    state.surface.width,
                    state.surface.height,
                    POINT {
                        x: local.x.saturating_sub(drag.pointer_offset.x),
                        y: local.y.saturating_sub(drag.pointer_offset.y),
                    },
                );
                state.hovered_control = Some(ToolbarControl::Background);
                state.hovered = None;
                state.hovered_handle = None;
                set_arrow_cursor();
                invalidate(hwnd);
                return LRESULT(0);
            }
            let layout = ToolbarLayout::new(
                state.surface.width,
                state.surface.height,
                state.toolbar_position,
            );
            let region_drag_active =
                state.resizing.is_some() || state.moving_region.is_some() || state.anchor.is_some();
            state.hovered_control = (!region_drag_active)
                .then(|| layout.hit_test(local, state.options_open))
                .flatten();
            if state.hovered_control.is_some() {
                state.hovered = None;
                set_arrow_cursor();
            } else if let Some(resize) = state.resizing {
                state.selection = Some(resize_region(
                    resize.original,
                    resize.handle,
                    point,
                    state.source,
                ));
                state.hovered_handle = Some(resize.handle);
                update_cursor(state.hovered_handle, &state.region_cursor);
            } else if let Some(moving) = state.moving_region {
                state.selection = Some(move_region(
                    moving.original,
                    moving.pointer_origin,
                    point,
                    state.source,
                ));
                state.hovered_handle = None;
                set_move_cursor();
            } else if let Some(anchor) = state.anchor {
                state.dragging |= (point.x - anchor.x).abs() >= DRAG_THRESHOLD
                    || (point.y - anchor.y).abs() >= DRAG_THRESHOLD;
                if state.dragging {
                    state.selection = rect_from_points(state.source, anchor, point);
                    state.selection_kind = Some(SelectionKind::Region);
                    state.selected_window = None;
                    state.hovered = None;
                }
                state.hovered_handle = None;
                update_cursor(None, &state.region_cursor);
            } else if state.tool == CaptureTool::Region
                && state.selection_kind == Some(SelectionKind::Region)
            {
                state.hovered_handle = state
                    .selection
                    .and_then(|selection| hit_test_resize_handle(selection, point));
                state.hovered = None;
                if state.hovered_handle.is_none()
                    && state
                        .selection
                        .is_some_and(|selection| contains(selection, point))
                {
                    set_move_cursor();
                } else {
                    update_cursor(state.hovered_handle, &state.region_cursor);
                }
            } else if state.tool == CaptureTool::Window {
                let hovered_handle = hit_test_window_thumbnail(state, local);
                state.hovered = hovered_handle.and_then(|handle| {
                    state.windows.as_ref().and_then(|windows| {
                        windows
                            .iter()
                            .copied()
                            .find(|candidate| candidate.handle == handle)
                    })
                });
                state.hovered_handle = None;
                set_arrow_cursor();
            } else {
                state.hovered = None;
                state.hovered_handle = None;
                if state.tool == CaptureTool::Region {
                    update_cursor(None, &state.region_cursor);
                } else {
                    set_arrow_cursor();
                }
            }
            if state.tool == CaptureTool::Window
                && previous_hovered == state.hovered.map(|candidate| candidate.handle)
                && previous_control == state.hovered_control
            {
                return LRESULT(0);
            }
            invalidate(hwnd);
            if state.tool == CaptureTool::Window {
                // SAFETY: Forces the now-cheap cached hover paint before this mouse message returns.
                let _ = unsafe { UpdateWindow(hwnd) };
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let point = screen_point(state.source, lparam);
            let local = local_point(state.source, point);
            let layout = ToolbarLayout::new(
                state.surface.width,
                state.surface.height,
                state.toolbar_position,
            );
            if let Some(control) = layout.hit_test(local, state.options_open) {
                state.anchor = None;
                state.dragging = false;
                state.resizing = None;
                state.moving_region = None;
                state.hovered_control = Some(control);
                match control {
                    ToolbarControl::Background if layout.bounds.contains(local) => {
                        state.options_open = false;
                        state.toolbar_drag = Some(ToolbarDrag {
                            pointer_offset: POINT {
                                x: local.x.saturating_sub(state.toolbar_position.x),
                                y: local.y.saturating_sub(state.toolbar_position.y),
                            },
                        });
                    }
                    ToolbarControl::Background => {}
                    ToolbarControl::FullDisplay => {
                        activate_tool(state, CaptureTool::FullDisplay);
                    }
                    ToolbarControl::Window => activate_tool(state, CaptureTool::Window),
                    ToolbarControl::Region => activate_tool(state, CaptureTool::Region),
                    ToolbarControl::Options => state.options_open = !state.options_open,
                    ToolbarControl::Capture => {
                        confirm_and_close(hwnd, state);
                        return LRESULT(0);
                    }
                    ToolbarControl::DimBackground => {
                        state.dim_background = !state.dim_background;
                    }
                    ToolbarControl::ClipboardDestination => {}
                    ToolbarControl::Cancel => {
                        cancel_and_close(hwnd);
                        return LRESULT(0);
                    }
                }
                set_arrow_cursor();
                invalidate(hwnd);
                return LRESULT(0);
            }
            state.hovered_control = None;
            state.options_open = false;
            if state.tool != CaptureTool::Region {
                if state.tool == CaptureTool::Window {
                    state.selected_window = hit_test_window_thumbnail(state, local);
                    update_window_preview(state, state.selected_window);
                    state.selected_window_frame =
                        ready_window_preview(state, state.selected_window)
                            .map(|preview| preview.frame.clone());
                    state.selection = state
                        .selected_window_frame
                        .as_ref()
                        .map(|frame| frame.metadata.source_rect);
                    state.selection_kind = state.selection.map(|_| SelectionKind::Window);
                    if state.selection.is_some() {
                        confirm_and_close(hwnd, state);
                        return LRESULT(0);
                    }
                    rebuild_window_overview_cache(state);
                }
                invalidate(hwnd);
                return LRESULT(0);
            }
            let existing_region = (state.selection_kind == Some(SelectionKind::Region))
                .then_some(state.selection)
                .flatten();
            let resize_handle =
                existing_region.and_then(|selection| hit_test_resize_handle(selection, point));
            if let (Some(original), Some(handle)) = (existing_region, resize_handle) {
                state.resizing = Some(ResizeDrag { handle, original });
                state.moving_region = None;
                state.hovered_handle = Some(handle);
                state.anchor = None;
                state.dragging = false;
                update_cursor(Some(handle), &state.region_cursor);
            } else if existing_region.is_some_and(|selection| contains(selection, point)) {
                state.moving_region = existing_region.map(|original| MoveDrag {
                    original,
                    pointer_origin: point,
                });
                state.anchor = None;
                state.dragging = false;
                state.hovered_handle = None;
                set_move_cursor();
            } else {
                state.anchor = Some(point);
                state.dragging = false;
                state.resizing = None;
                state.moving_region = None;
                state.selection = None;
                state.selection_kind = None;
                state.selected_window = None;
                state.hovered_handle = None;
                state.hovered = None;
                update_cursor(None, &state.region_cursor);
            }
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let point = screen_point(state.source, lparam);
            if state.toolbar_drag.take().is_some() {
                remember_toolbar_position(state.toolbar_position);
                state.hovered_control = Some(ToolbarControl::Background);
                set_arrow_cursor();
                invalidate(hwnd);
                return LRESULT(0);
            }
            if state.tool != CaptureTool::Region {
                return LRESULT(0);
            }
            if let Some(resize) = state.resizing.take() {
                state.selection = Some(resize_region(
                    resize.original,
                    resize.handle,
                    point,
                    state.source,
                ));
                state.selection_kind = Some(SelectionKind::Region);
                state.selected_window = None;
            } else if let Some(moving) = state.moving_region.take() {
                state.selection = Some(move_region(
                    moving.original,
                    moving.pointer_origin,
                    point,
                    state.source,
                ));
                state.selection_kind = Some(SelectionKind::Region);
                state.selected_window = None;
            } else if let Some(anchor) = state.anchor.take() {
                if state.dragging {
                    state.selection = rect_from_points(state.source, anchor, point);
                    state.selection_kind = Some(SelectionKind::Region);
                    state.selected_window = None;
                }
            }
            state.dragging = false;
            state.hovered_handle = if state.selection_kind == Some(SelectionKind::Region) {
                state
                    .selection
                    .and_then(|selection| hit_test_resize_handle(selection, point))
            } else {
                None
            };
            if state.hovered_handle.is_none()
                && state
                    .selection
                    .is_some_and(|selection| contains(selection, point))
            {
                set_move_cursor();
            } else {
                update_cursor(state.hovered_handle, &state.region_cursor);
            }
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let point = screen_point(state.source, lparam);
            let local = local_point(state.source, point);
            let layout = ToolbarLayout::new(
                state.surface.width,
                state.surface.height,
                state.toolbar_position,
            );
            if layout.hit_test(local, state.options_open).is_none() {
                confirm_and_close(hwnd, state);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_RETURN.0) => {
            confirm_and_close(hwnd, state);
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => {
            cancel_and_close(hwnd);
            LRESULT(0)
        }
        WM_RBUTTONDOWN | WM_CLOSE => {
            cancel_and_close(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            paint(hwnd, state);
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

fn confirm_and_close(hwnd: HWND, state: &mut OverlayState) {
    if let (Some(rect), Some(kind)) = (state.selection, state.selection_kind) {
        let window_frame = if kind == SelectionKind::Window {
            state.selected_window_frame.clone()
        } else {
            None
        };
        let rect = window_frame
            .as_ref()
            .map(|frame| frame.metadata.source_rect)
            .unwrap_or(rect);
        let confirmed_region = (kind == SelectionKind::Region).then_some(rect);
        remember_capture_history(kind, confirmed_region);
        if confirmed_region.is_some() {
            state.last_region = Some(rect);
        }
        state.result = Some(OverlaySelection {
            rect,
            kind,
            window: state.selected_window,
            selection_ns: duration_ns(state.started.elapsed()),
            preparation_ns: state.preparation_ns,
            window_overview_ns: state.window_overview_ns,
            window_preview_count: state.window_thumbnails.len(),
            window_preview_bytes: state
                .window_thumbnails
                .iter()
                .map(|thumbnail| thumbnail.surface.byte_length)
                .sum(),
            window_frame,
        });
        // SAFETY: hwnd is the live overlay window and this function runs on its owner thread.
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}

fn cancel_and_close(hwnd: HWND) {
    // SAFETY: hwnd is the live overlay window and this function runs on its owner thread.
    let _ = unsafe { DestroyWindow(hwnd) };
}

fn invalidate(hwnd: HWND) {
    // SAFETY: Invalidates the complete client area of the live overlay without erasing it first.
    let _ = unsafe { InvalidateRect(hwnd, None, false) };
}

fn paint(hwnd: HWND, state: &mut OverlayState) {
    let mut paint = PAINTSTRUCT::default();
    // SAFETY: paint is writable storage and EndPaint balances this call before return.
    let device = unsafe { BeginPaint(hwnd, &mut paint) };
    let width = state.surface.width;
    if state.tool == CaptureTool::Window {
        let cache_matches = state
            .window_overview_cache
            .as_ref()
            .is_some_and(|cache| cache.dim_background == state.dim_background);
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
    if state.tool != CaptureTool::Window && state.dim_background {
        let _ = apply_dim_wash(
            state.back_buffer.device,
            state.dimmer.device,
            width,
            state.surface.height,
            DIM_ALPHA,
        );
    }
    if state.tool != CaptureTool::Window {
        if let Some(rect) = state.selection {
            restore_highlight(state, rect);
            draw_outline(state.back_buffer.device, state.source, rect);
            if state.selection_kind == Some(SelectionKind::Region) {
                draw_resize_handles(state.back_buffer.device, state.source, rect);
                draw_region_dimensions(state.back_buffer.device, state.source, rect);
            }
        }
    }
    draw_toolbar(state);
    // SAFETY: The fully composed back buffer is copied to the live paint DC in one operation.
    let _ = unsafe {
        BitBlt(
            device,
            0,
            0,
            width,
            state.surface.height,
            state.back_buffer.device,
            0,
            0,
            SRCCOPY,
        )
    };
    // SAFETY: Balances BeginPaint for this exact hwnd/paint structure.
    unsafe { EndPaint(hwnd, &paint) };
}

fn apply_dim_wash(destination: HDC, dimmer: HDC, width: i32, height: i32, alpha: u8) -> bool {
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

fn update_window_preview(state: &mut OverlayState, target: Option<NativeWindowHandle>) {
    let Some(handle) = target else {
        return;
    };
    if state
        .window_preview
        .as_ref()
        .is_some_and(|preview| preview.handle() == handle)
    {
        return;
    }
    state.window_preview = match capture_window_visual(handle, &state.reference_metadata) {
        Ok(capture) => match FrozenSurface::from_straight_alpha(
            capture.frame.width,
            capture.frame.height,
            &capture.frame.pixels,
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

fn build_blurred_background(
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

fn ensure_window_mode_assets(state: &mut OverlayState) {
    if state.window_assets_ready {
        return;
    }
    if state.windows.is_none() {
        state.windows = Some(enumerate_visible_windows(state.source).unwrap_or_default());
    }
    let reusable = state.cached_blurred_background.take();
    state.blurred_background = build_blurred_background(&state.surface, 24, reusable).ok();
    state.window_assets_ready = true;
}

fn build_window_overview(state: &mut OverlayState) {
    let started = Instant::now();
    ensure_window_mode_assets(state);
    if state.window_thumbnails.is_empty() {
        let candidates = state.windows.clone().unwrap_or_default();
        for batch in candidates.chunks(2) {
            let rendered = thread::scope(|scope| {
                let workers: Vec<_> = batch
                    .iter()
                    .copied()
                    .map(|candidate| {
                        let metadata = &state.reference_metadata;
                        scope.spawn(move || {
                            (
                                candidate.handle,
                                capture_window_thumbnail(
                                    candidate.handle,
                                    metadata,
                                    WINDOW_THUMBNAIL_MAX_PIXELS,
                                ),
                            )
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .filter_map(|worker| worker.join().ok())
                    .collect::<Vec<_>>()
            });
            for (handle, capture) in rendered {
                let Ok(capture) = capture else {
                    continue;
                };
                let Ok(surface) = FrozenSurface::from_straight_alpha(
                    capture.frame.width,
                    capture.frame.height,
                    &capture.frame.pixels,
                ) else {
                    continue;
                };
                state.window_thumbnails.push(WindowThumbnail {
                    handle,
                    surface,
                    corner_radius_px: capture.corner_radius_px,
                });
            }
        }
    }
    rebuild_window_overview_cache(state);
    if state.window_overview_ns.is_none() {
        state.window_overview_ns = Some(duration_ns(started.elapsed()));
    }
}

fn rebuild_window_overview_cache(state: &mut OverlayState) {
    let reusable = state
        .window_overview_cache
        .take()
        .map(|cache| cache.surface);
    let surface = reusable
        .filter(|surface| {
            surface.width == state.surface.width && surface.height == state.surface.height
        })
        .map_or_else(
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
        dim_background: state.dim_background,
    });
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
    if state.dim_background {
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
        .map(|thumbnail| (thumbnail.surface.width, thumbnail.surface.height))
        .collect();
    layout_floating_windows(state.surface.width, state.surface.height, &dimensions)
}

fn layout_floating_windows(
    client_width: i32,
    client_height: i32,
    dimensions: &[(i32, i32)],
) -> Vec<UiRect> {
    if dimensions.is_empty() {
        return Vec::new();
    }
    let left = 70;
    let top = 64;
    let available_width = (client_width - left * 2).max(200);
    let available_height = (client_height - top - 160).max(160);
    let gap = 28;
    let columns = (1..=dimensions.len())
        .max_by_key(|&candidate| {
            minimum_row_height(
                available_width,
                available_height,
                gap,
                candidate,
                dimensions,
            )
        })
        .unwrap_or(1);
    layout_floating_windows_with_columns(
        left,
        top,
        available_width,
        available_height,
        gap,
        columns,
        dimensions,
    )
}

fn minimum_row_height(
    available_width: i32,
    available_height: i32,
    gap: i32,
    columns: usize,
    dimensions: &[(i32, i32)],
) -> i32 {
    let rows = dimensions.len().div_ceil(columns);
    let row_slot_height = ((available_height - gap * (rows as i32 - 1)) / rows as i32).max(60);
    dimensions
        .chunks(columns)
        .map(|row_dimensions| {
            let aspect_sum: f64 = row_dimensions
                .iter()
                .map(|&(width, height)| f64::from(width) / f64::from(height.max(1)))
                .sum();
            let width_without_gaps = available_width - gap * (row_dimensions.len() as i32 - 1);
            f64::from(row_slot_height)
                .min(f64::from(width_without_gaps.max(1)) / aspect_sum.max(0.01))
                .round() as i32
        })
        .min()
        .unwrap_or(0)
}

fn layout_floating_windows_with_columns(
    left: i32,
    top: i32,
    available_width: i32,
    available_height: i32,
    gap: i32,
    columns: usize,
    dimensions: &[(i32, i32)],
) -> Vec<UiRect> {
    let rows = dimensions.len().div_ceil(columns);
    let row_slot_height = ((available_height - gap * (rows as i32 - 1)) / rows as i32).max(60);
    let mut result = Vec::with_capacity(dimensions.len());
    for row in 0..rows {
        let start = row * columns;
        let end = (start + columns).min(dimensions.len());
        let row_dimensions = &dimensions[start..end];
        let aspect_sum: f64 = row_dimensions
            .iter()
            .map(|&(width, height)| f64::from(width) / f64::from(height.max(1)))
            .sum();
        let width_without_gaps = available_width - gap * (row_dimensions.len() as i32 - 1);
        let height_from_width = f64::from(width_without_gaps.max(1)) / aspect_sum.max(0.01);
        let window_height = f64::from(row_slot_height).min(height_from_width).max(40.0);
        let widths: Vec<i32> = row_dimensions
            .iter()
            .map(|&(width, height)| {
                (window_height * f64::from(width) / f64::from(height.max(1)))
                    .round()
                    .max(40.0) as i32
            })
            .collect();
        let row_width = widths.iter().sum::<i32>() + gap * (widths.len() as i32 - 1);
        let mut x = left + (available_width - row_width) / 2;
        let y = top
            + row as i32 * (row_slot_height + gap)
            + (row_slot_height - window_height.round() as i32) / 2;
        for width in widths {
            result.push(UiRect {
                left: x,
                top: y,
                right: x + width,
                bottom: y + window_height.round() as i32,
            });
            x += width + gap;
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
    let Some(visible) = intersect_rect(rect, state.source) else {
        return;
    };
    let x = visible.x - state.source.x;
    let y = visible.y - state.source.y;
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

fn draw_outline(device: windows::Win32::Graphics::Gdi::HDC, source: Rect, rect: Rect) {
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

fn draw_outline_layer(
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

fn draw_resize_handles(device: HDC, source: Rect, rect: Rect) {
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
        let outer_rect = centered_rect(x, y, HANDLE_OUTER_RADIUS);
        let inner_rect = centered_rect(x, y, HANDLE_INNER_RADIUS);
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

fn draw_region_dimensions(device: HDC, source: Rect, rect: Rect) {
    let value = format!("{} × {} px", rect.width, rect.height);
    let available_width = (source.width as i32 - 16).max(1);
    let label_width = ((value.chars().count() as i32 * 12) + 28)
        .max(150)
        .min(available_width);
    let selection_left = rect.x - source.x;
    let selection_top = rect.y - source.y;
    let selection_bottom = selection_top.saturating_add(rect.height as i32);
    let centered_left = selection_left
        .saturating_add((rect.width as i32 - label_width) / 2)
        .clamp(8, (source.width as i32 - label_width - 8).max(8));
    let preferred_top = if rect.height as i32 >= DIMENSION_LABEL_HEIGHT + 28 {
        selection_top + 14
    } else if selection_top >= DIMENSION_LABEL_HEIGHT + 12 {
        selection_top - DIMENSION_LABEL_HEIGHT - 10
    } else {
        selection_bottom + 10
    };
    let top = preferred_top.clamp(
        8,
        (source.height as i32 - DIMENSION_LABEL_HEIGHT - 8).max(8),
    );
    let bounds = UiRect {
        left: centered_left,
        top,
        right: centered_left + label_width,
        bottom: top + DIMENSION_LABEL_HEIGHT,
    };
    draw_round_box(device, bounds, rgb(31, 31, 34), rgb(104, 104, 110), 10);
    draw_text(
        device,
        UiRect {
            left: bounds.left + 10,
            top: bounds.top,
            right: bounds.right - 10,
            bottom: bounds.bottom,
        },
        &value,
        rgb(248, 248, 250),
        TextAlignment::Center,
    );
}

fn centered_rect(x: i32, y: i32, radius: i32) -> RECT {
    RECT {
        left: x.saturating_sub(radius),
        top: y.saturating_sub(radius),
        right: x.saturating_add(radius).saturating_add(1),
        bottom: y.saturating_add(radius).saturating_add(1),
    }
}

fn draw_window_overview_static(destination: &FrozenSurface, state: &OverlayState) {
    if state.window_thumbnails.is_empty() {
        draw_text(
            destination.device,
            UiRect {
                left: 70,
                top: 64,
                right: state.surface.width - 70,
                bottom: 118,
            },
            "No capturable application windows",
            rgb(220, 220, 224),
            TextAlignment::Center,
        );
        return;
    }
    let rects = window_overview_rects(state);
    for (thumbnail, rect) in state.window_thumbnails.iter().zip(rects) {
        let selected = state.selected_window == Some(thumbnail.handle);
        let preview = selected
            .then(|| ready_window_preview(state, Some(thumbnail.handle)))
            .flatten();
        let surface = preview.map_or(&thumbnail.surface, |preview| &preview.surface);
        draw_window_surface(destination, surface, rect);
    }
}

fn draw_window_overview_interactive(state: &OverlayState) {
    let rects = window_overview_rects(state);
    for (thumbnail, rect) in state.window_thumbnails.iter().zip(rects) {
        let selected = state.selected_window == Some(thumbnail.handle);
        let hovered = state.hovered.map(|candidate| candidate.handle) == Some(thumbnail.handle);
        if selected || hovered {
            let color = if selected {
                rgb(45, 125, 246)
            } else {
                rgb(52, 197, 218)
            };
            let preview = selected
                .then(|| ready_window_preview(state, Some(thumbnail.handle)))
                .flatten();
            let (surface, corner_radius_px) = preview.map_or(
                (&thumbnail.surface, thumbnail.corner_radius_px),
                |preview| (&preview.surface, preview.corner_radius_px),
            );
            let destination = fitted_surface_rect(surface, rect, true);
            let scaled_radius = scaled_corner_radius(surface, destination, corner_radius_px);
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

fn draw_window_surface(device: &FrozenSurface, surface: &FrozenSurface, bounds: UiRect) {
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

fn scale_premultiplied_surface(
    source: &FrozenSurface,
    width: u32,
    height: u32,
) -> Result<FrozenSurface, CaptureError> {
    let scaled = FrozenSurface::empty(width, height)?;
    // AlphaBlend's built-in stretch is low quality, while GDI HALFTONE discards the alpha byte.
    // Resample all four premultiplied channels together: area filtering for reduction and bilinear
    // filtering for enlargement. The exact-size result can then be composited 1:1.
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

fn area_scale_bgra(
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

fn area_contributors(source_length: usize, destination_length: usize) -> Vec<Vec<(usize, f32)>> {
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

fn bilinear_scale_bgra(
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

fn alpha_blend_surface(device: &FrozenSurface, surface: &FrozenSurface, destination: UiRect) {
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

fn blend_channel(foreground: u8, background: u8, coverage: u8) -> u8 {
    let coverage = u32::from(coverage);
    ((u32::from(foreground) * coverage + u32::from(background) * (255 - coverage) + 127) / 255)
        as u8
}

fn rounded_rect_coverage(width: i32, height: i32, radius: f32, x: i32, y: i32) -> u8 {
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

fn draw_antialiased_rounded_outline(
    surface: &FrozenSurface,
    rect: UiRect,
    color: COLORREF,
    stroke_width: i32,
    corner_radius: f32,
) {
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

fn blend_surface_pixel(surface: &FrozenSurface, x: i32, y: i32, foreground: [u8; 4], coverage: u8) {
    if x < 0 || y < 0 || x >= surface.width || y >= surface.height {
        return;
    }
    let offset = ((y * surface.width + x) * 4) as usize;
    // SAFETY: The checked coordinates address four writable bytes in the live off-screen DIB.
    unsafe {
        for (channel, foreground) in foreground.into_iter().enumerate() {
            let destination = surface.bits.add(offset + channel);
            *destination = blend_channel(foreground, *destination, coverage);
        }
    }
}

fn fitted_surface_rect(surface: &FrozenSurface, bounds: UiRect, allow_upscale: bool) -> UiRect {
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

fn scaled_corner_radius(surface: &FrozenSurface, destination: UiRect, source_radius: f32) -> f32 {
    if source_radius <= 0.0 || surface.width <= 0 || surface.height <= 0 {
        return 0.0;
    }
    let width_scale = (destination.right - destination.left).max(1) as f32 / surface.width as f32;
    let height_scale = (destination.bottom - destination.top).max(1) as f32 / surface.height as f32;
    source_radius * width_scale.min(height_scale)
}

fn draw_surface_to_rect(device: HDC, surface: &FrozenSurface, destination: UiRect) {
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

fn draw_toolbar(state: &OverlayState) {
    let device = state.back_buffer.device;
    let layout = ToolbarLayout::new(
        state.surface.width,
        state.surface.height,
        state.toolbar_position,
    );
    if state.options_open {
        draw_options_menu(device, state, layout);
    }
    draw_round_box(
        device,
        layout.bounds,
        rgb(38, 38, 41),
        rgb(92, 92, 98),
        TOOLBAR_CORNER_RADIUS,
    );
    for y in [
        layout.drag_handle.top + 17,
        layout.drag_handle.top + 31,
        layout.drag_handle.top + 45,
    ] {
        draw_filled_ellipse(
            device,
            layout.drag_handle.left + 13,
            y,
            layout.drag_handle.left + 18,
            y + 5,
            rgb(142, 142, 148),
        );
    }
    draw_tool_button(device, layout.full_display, CaptureTool::FullDisplay, state);
    draw_tool_button(device, layout.window, CaptureTool::Window, state);
    draw_tool_button(device, layout.region, CaptureTool::Region, state);
    draw_lines(
        device,
        &[
            (layout.bounds.left + 253, layout.bounds.top + 17),
            (layout.bounds.left + 253, layout.bounds.bottom - 17),
        ],
        rgb(92, 92, 98),
        1,
    );

    let options_hovered = state.hovered_control == Some(ToolbarControl::Options);
    if state.options_open || options_hovered {
        draw_round_box(device, layout.options, rgb(65, 65, 70), rgb(65, 65, 70), 8);
    }
    draw_text(
        device,
        UiRect {
            left: layout.options.left + 4,
            top: layout.options.top,
            right: layout.options.right - 34,
            bottom: layout.options.bottom,
        },
        "Options",
        rgb(245, 245, 247),
        TextAlignment::Center,
    );
    let chevron_y = layout.options.top + 31;
    draw_lines(
        device,
        &[
            (layout.options.right - 29, chevron_y - 3),
            (layout.options.right - 24, chevron_y + 2),
            (layout.options.right - 19, chevron_y - 3),
        ],
        rgb(220, 220, 224),
        2,
    );

    let capture_enabled = state.selection.is_some();
    let capture_color = if capture_enabled {
        if state.hovered_control == Some(ToolbarControl::Capture) {
            rgb(68, 145, 255)
        } else {
            rgb(45, 125, 246)
        }
    } else {
        rgb(82, 82, 87)
    };
    draw_round_box(device, layout.capture, capture_color, capture_color, 9);
    let capture_foreground = if capture_enabled {
        rgb(255, 255, 255)
    } else {
        rgb(180, 180, 185)
    };
    draw_camera_icon(
        device,
        layout.capture.left + 20,
        layout.capture.top + 18,
        capture_foreground,
    );
    draw_text(
        device,
        UiRect {
            left: layout.capture.left + 54,
            top: layout.capture.top,
            right: layout.capture.right - 8,
            bottom: layout.capture.bottom,
        },
        "Capture",
        capture_foreground,
        TextAlignment::Center,
    );
    draw_hover_tooltip(device, state, layout);
}

fn draw_tool_button(device: HDC, bounds: UiRect, tool: CaptureTool, state: &OverlayState) {
    let control = match tool {
        CaptureTool::FullDisplay => ToolbarControl::FullDisplay,
        CaptureTool::Window => ToolbarControl::Window,
        CaptureTool::Region => ToolbarControl::Region,
    };
    if state.tool == tool || state.hovered_control == Some(control) {
        let color = if state.tool == tool {
            rgb(76, 76, 82)
        } else {
            rgb(58, 58, 63)
        };
        draw_round_box(device, bounds, color, color, 8);
    }
    let color = rgb(245, 245, 247);
    match tool {
        CaptureTool::FullDisplay => draw_display_icon(device, bounds, color),
        CaptureTool::Window => draw_window_icon(device, bounds, color),
        CaptureTool::Region => draw_region_icon(device, bounds, color),
    }
}

fn draw_hover_tooltip(device: HDC, state: &OverlayState, layout: ToolbarLayout) {
    if state.options_open {
        return;
    }
    let (target, value) = match state.hovered_control {
        Some(ToolbarControl::FullDisplay) => {
            (layout.full_display, "Capture full display".to_owned())
        }
        Some(ToolbarControl::Window) => (layout.window, "Capture a window".to_owned()),
        Some(ToolbarControl::Region) => (layout.region, "Select a region".to_owned()),
        Some(ToolbarControl::Options) => (layout.options, "Capture options".to_owned()),
        Some(ToolbarControl::Capture) => {
            let value = if state.selection.is_some() {
                "Copy selection to clipboard"
            } else {
                "Select something to capture"
            };
            (layout.capture, value.to_owned())
        }
        _ => return,
    };
    let measured = measure_ui_text(device, &value);
    let width = (measured.cx + 36)
        .max(150)
        .min((state.surface.width - 16).max(1));
    let height = (measured.cy + 16).max(38);
    let left = ((target.left + target.right - width) / 2)
        .clamp(8, (state.surface.width - width - 8).max(8));
    let preferred_top = if layout.bounds.top >= height + 18 {
        layout.bounds.top - height - 10
    } else {
        layout.bounds.bottom + 10
    };
    let top = preferred_top.clamp(8, (state.surface.height - height - 8).max(8));
    let bounds = UiRect {
        left,
        top,
        right: left + width,
        bottom: top + height,
    };
    draw_round_box(device, bounds, rgb(31, 31, 34), rgb(104, 104, 110), 9);
    draw_text(
        device,
        UiRect {
            left: bounds.left + 16,
            top: bounds.top,
            right: bounds.right - 16,
            bottom: bounds.bottom,
        },
        &value,
        rgb(248, 248, 250),
        TextAlignment::Center,
    );
}

fn draw_options_menu(device: HDC, state: &OverlayState, layout: ToolbarLayout) {
    draw_round_box(device, layout.menu, rgb(43, 43, 47), rgb(101, 101, 107), 12);
    let rows = [
        (ToolbarControl::DimBackground, layout.dim_background),
        (
            ToolbarControl::ClipboardDestination,
            layout.clipboard_destination,
        ),
        (ToolbarControl::Cancel, layout.cancel),
    ];
    for (control, row) in rows {
        if state.hovered_control == Some(control) {
            draw_round_box(device, row, rgb(64, 64, 69), rgb(64, 64, 69), 6);
        }
    }
    if state.dim_background {
        draw_checkmark(
            device,
            layout.dim_background.left + 20,
            (layout.dim_background.top + layout.dim_background.bottom) / 2,
            rgb(86, 156, 255),
        );
    }
    draw_text(
        device,
        UiRect {
            left: layout.dim_background.left + 50,
            top: layout.dim_background.top,
            right: layout.dim_background.right - 12,
            bottom: layout.dim_background.bottom,
        },
        "Dim Background",
        rgb(245, 245, 247),
        TextAlignment::Left,
    );
    draw_checkmark(
        device,
        layout.clipboard_destination.left + 20,
        (layout.clipboard_destination.top + layout.clipboard_destination.bottom) / 2,
        rgb(86, 156, 255),
    );
    draw_text(
        device,
        UiRect {
            left: layout.clipboard_destination.left + 50,
            top: layout.clipboard_destination.top,
            right: layout.clipboard_destination.right - 12,
            bottom: layout.clipboard_destination.bottom,
        },
        "Copy to Clipboard",
        rgb(210, 210, 214),
        TextAlignment::Left,
    );
    draw_lines(
        device,
        &[
            (layout.menu.left + 12, layout.cancel.top),
            (layout.menu.right - 12, layout.cancel.top),
        ],
        rgb(76, 76, 81),
        1,
    );
    draw_text(
        device,
        UiRect {
            left: layout.cancel.left + 50,
            top: layout.cancel.top,
            right: layout.cancel.right - 12,
            bottom: layout.cancel.bottom,
        },
        "Cancel Capture",
        rgb(255, 105, 97),
        TextAlignment::Left,
    );
}

fn draw_display_icon(device: HDC, bounds: UiRect, color: COLORREF) {
    let left = bounds.left + 17;
    let top = bounds.top + 15;
    draw_outline_rect(device, left, top, left + 30, top + 21, color, 2);
    draw_lines(
        device,
        &[(left + 15, top + 21), (left + 15, top + 28)],
        color,
        2,
    );
    draw_lines(
        device,
        &[(left + 8, top + 28), (left + 22, top + 28)],
        color,
        2,
    );
}

fn draw_window_icon(device: HDC, bounds: UiRect, color: COLORREF) {
    let left = bounds.left + 15;
    let top = bounds.top + 14;
    draw_outline_rect(device, left + 7, top, left + 34, top + 22, color, 2);
    draw_outline_rect(device, left, top + 8, left + 27, top + 30, color, 2);
}

fn draw_region_icon(device: HDC, bounds: UiRect, color: COLORREF) {
    let left = bounds.left + 15;
    let top = bounds.top + 14;
    let right = left + 33;
    let bottom = top + 30;
    for points in [
        [(left, top + 9), (left, top), (left + 9, top)],
        [(right - 9, top), (right, top), (right, top + 9)],
        [(right, bottom - 9), (right, bottom), (right - 9, bottom)],
        [(left + 9, bottom), (left, bottom), (left, bottom - 9)],
    ] {
        draw_lines(device, &points, color, 2);
    }
}

fn draw_camera_icon(device: HDC, left: i32, top: i32, color: COLORREF) {
    draw_outline_rect(device, left, top + 6, left + 32, top + 28, color, 2);
    draw_lines(
        device,
        &[
            (left + 8, top + 6),
            (left + 12, top),
            (left + 20, top),
            (left + 24, top + 6),
        ],
        color,
        2,
    );
    draw_ellipse(device, left + 11, top + 10, left + 23, top + 22, color, 2);
}

fn draw_checkmark(device: HDC, x: i32, y: i32, color: COLORREF) {
    draw_lines(
        device,
        &[(x - 5, y), (x - 1, y + 4), (x + 6, y - 5)],
        color,
        2,
    );
}

fn draw_round_box(device: HDC, rect: UiRect, fill: COLORREF, border: COLORREF, radius: i32) {
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
            radius,
            radius,
        );
        SelectObject(device, old_pen);
        SelectObject(device, old_brush);
        DeleteObject(pen);
        DeleteObject(brush);
    }
}

fn draw_outline_rect(
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

fn draw_ellipse(
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

fn draw_filled_ellipse(device: HDC, left: i32, top: i32, right: i32, bottom: i32, color: COLORREF) {
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

fn draw_lines(device: HDC, points: &[(i32, i32)], color: COLORREF, width: i32) {
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

fn draw_text(device: HDC, bounds: UiRect, value: &str, color: COLORREF, alignment: TextAlignment) {
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
    let font = create_ui_font();
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

fn measure_ui_text(device: HDC, value: &str) -> SIZE {
    let text: Vec<u16> = value.encode_utf16().collect();
    let font = create_ui_font();
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
            cx: value.chars().count() as i32 * (UI_FONT_HEIGHT / 2),
            cy: UI_FONT_HEIGHT,
        }
    }
}

fn create_ui_font() -> HFONT {
    // SAFETY: Creates a ClearType font from the registered process-private IoskeleyMono face.
    unsafe {
        CreateFontW(
            -UI_FONT_HEIGHT,
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

const fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    COLORREF((red as u32) | ((green as u32) << 8) | ((blue as u32) << 16))
}

fn enumerate_visible_windows(source: Rect) -> Result<Vec<WindowCandidate>, CaptureError> {
    let mut collector = WindowCollector {
        source,
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
    // SAFETY: hwnd is a top-level handle supplied synchronously by EnumWindows.
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    // SAFETY: hwnd remains the same live enumerated top-level handle.
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    // SAFETY: Reading the title length does not retain the enumerated handle.
    let has_title = unsafe { GetWindowTextLengthW(hwnd) } != 0;
    // SAFETY: Reads immutable extended-style bits from the enumerated window.
    let extended_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let forced_taskbar_window = extended_style & WS_EX_APPWINDOW.0 != 0;
    if !visible
        || minimized
        || !has_title
        || is_cloaked_window(hwnd)
        || is_shell_surface(hwnd)
        || !window_styles_allow_task_switcher(extended_style)
        || (!forced_taskbar_window && !is_root_owner_task_window(hwnd))
    {
        return;
    }
    let mut native = RECT::default();
    // SAFETY: native is writable storage and hwnd is the enumerated top-level window.
    if unsafe { GetWindowRect(hwnd, &mut native) }.is_err() {
        return;
    }
    if rect_from_native(native).is_some() {
        let Some(visible_bounds) = intersect_with_source(native, collector.source) else {
            return;
        };
        if visible_bounds.width >= 16 && visible_bounds.height >= 16 {
            collector.windows.push(WindowCandidate {
                handle: NativeWindowHandle(hwnd.0),
            });
        }
    }
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

fn is_shell_surface(hwnd: HWND) -> bool {
    let mut class_name = [0_u16; 256];
    // SAFETY: class_name is writable UTF-16 storage and hwnd is used synchronously.
    let length = unsafe { GetClassNameW(hwnd, &mut class_name) };
    if length <= 0 {
        return false;
    }
    is_shell_window_class(&String::from_utf16_lossy(&class_name[..length as usize]))
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

fn intersect_with_source(native: RECT, source: Rect) -> Option<Rect> {
    let source_right = i64::from(source.x) + i64::from(source.width);
    let source_bottom = i64::from(source.y) + i64::from(source.height);
    let left = i64::from(native.left).max(i64::from(source.x));
    let top = i64::from(native.top).max(i64::from(source.y));
    let right = i64::from(native.right).min(source_right);
    let bottom = i64::from(native.bottom).min(source_bottom);
    (right > left && bottom > top).then_some(Rect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

fn intersect_rect(rect: Rect, source: Rect) -> Option<Rect> {
    let rect_right = i64::from(rect.x) + i64::from(rect.width);
    let rect_bottom = i64::from(rect.y) + i64::from(rect.height);
    let source_right = i64::from(source.x) + i64::from(source.width);
    let source_bottom = i64::from(source.y) + i64::from(source.height);
    let left = i64::from(rect.x).max(i64::from(source.x));
    let top = i64::from(rect.y).max(i64::from(source.y));
    let right = rect_right.min(source_right);
    let bottom = rect_bottom.min(source_bottom);
    (right > left && bottom > top).then_some(Rect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

fn screen_point(source: Rect, lparam: LPARAM) -> POINT {
    let packed = lparam.0 as u32;
    let x = (packed as u16) as i16 as i32;
    let y = ((packed >> 16) as u16) as i16 as i32;
    POINT {
        x: source.x.saturating_add(x),
        y: source.y.saturating_add(y),
    }
}

fn local_point(source: Rect, point: POINT) -> POINT {
    POINT {
        x: point.x.saturating_sub(source.x),
        y: point.y.saturating_sub(source.y),
    }
}

fn activate_tool(state: &mut OverlayState, tool: CaptureTool) {
    state.anchor = None;
    state.dragging = false;
    state.resizing = None;
    state.moving_region = None;
    let tool_changed = state.tool != tool;
    if tool_changed {
        state.selection = None;
        state.selection_kind = None;
        state.selected_window = None;
        state.selected_window_frame = None;
        state.hovered = None;
        state.hovered_handle = None;
    }
    state.tool = tool;
    state.options_open = false;
    match tool {
        CaptureTool::FullDisplay => {
            state.selection = Some(state.source);
            state.selection_kind = Some(SelectionKind::Display);
            state.selected_window = None;
            state.selected_window_frame = None;
        }
        CaptureTool::Window => build_window_overview(state),
        CaptureTool::Region if tool_changed => {
            (state.selection, state.selection_kind) =
                initial_selection(CaptureTool::Region, state.last_region, state.source);
        }
        CaptureTool::Region => {}
    }
}

fn rect_from_points(source: Rect, first: POINT, second: POINT) -> Option<Rect> {
    let source_right = i64::from(source.x) + i64::from(source.width);
    let source_bottom = i64::from(source.y) + i64::from(source.height);
    let left = i64::from(first.x.min(second.x)).clamp(i64::from(source.x), source_right);
    let top = i64::from(first.y.min(second.y)).clamp(i64::from(source.y), source_bottom);
    let right = i64::from(first.x.max(second.x)).clamp(i64::from(source.x), source_right);
    let bottom = i64::from(first.y.max(second.y)).clamp(i64::from(source.y), source_bottom);
    (right > left && bottom > top).then_some(Rect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

fn contains(rect: Rect, point: POINT) -> bool {
    let right = i64::from(rect.x) + i64::from(rect.width);
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    i64::from(point.x) >= i64::from(rect.x)
        && i64::from(point.y) >= i64::from(rect.y)
        && i64::from(point.x) < right
        && i64::from(point.y) < bottom
}

fn hit_test_resize_handle(rect: Rect, point: POINT) -> Option<ResizeHandle> {
    let left = i64::from(rect.x);
    let top = i64::from(rect.y);
    let right = left + i64::from(rect.width);
    let bottom = top + i64::from(rect.height);
    let x = i64::from(point.x);
    let y = i64::from(point.y);
    let radius = i64::from(HANDLE_HIT_RADIUS);
    let near_left = (x - left).abs() <= radius;
    let near_right = (x - right).abs() <= radius;
    let near_top = (y - top).abs() <= radius;
    let near_bottom = (y - bottom).abs() <= radius;
    if near_left && near_top {
        Some(ResizeHandle::NorthWest)
    } else if near_right && near_top {
        Some(ResizeHandle::NorthEast)
    } else if near_right && near_bottom {
        Some(ResizeHandle::SouthEast)
    } else if near_left && near_bottom {
        Some(ResizeHandle::SouthWest)
    } else if near_top && x >= left && x <= right {
        Some(ResizeHandle::North)
    } else if near_right && y >= top && y <= bottom {
        Some(ResizeHandle::East)
    } else if near_bottom && x >= left && x <= right {
        Some(ResizeHandle::South)
    } else if near_left && y >= top && y <= bottom {
        Some(ResizeHandle::West)
    } else {
        None
    }
}

fn resize_region(original: Rect, handle: ResizeHandle, point: POINT, source: Rect) -> Rect {
    let source_left = i64::from(source.x);
    let source_top = i64::from(source.y);
    let source_right = source_left + i64::from(source.width);
    let source_bottom = source_top + i64::from(source.height);
    let mut left = i64::from(original.x);
    let mut top = i64::from(original.y);
    let mut right = left + i64::from(original.width);
    let mut bottom = top + i64::from(original.height);
    let point_x = i64::from(point.x);
    let point_y = i64::from(point.y);

    if matches!(
        handle,
        ResizeHandle::NorthWest | ResizeHandle::West | ResizeHandle::SouthWest
    ) {
        let maximum_left = (right - MIN_REGION_SIZE).max(source_left);
        left = point_x.clamp(source_left, maximum_left);
    }
    if matches!(
        handle,
        ResizeHandle::NorthEast | ResizeHandle::East | ResizeHandle::SouthEast
    ) {
        let minimum_right = (left + MIN_REGION_SIZE).min(source_right);
        right = point_x.clamp(minimum_right, source_right);
    }
    if matches!(
        handle,
        ResizeHandle::NorthWest | ResizeHandle::North | ResizeHandle::NorthEast
    ) {
        let maximum_top = (bottom - MIN_REGION_SIZE).max(source_top);
        top = point_y.clamp(source_top, maximum_top);
    }
    if matches!(
        handle,
        ResizeHandle::SouthWest | ResizeHandle::South | ResizeHandle::SouthEast
    ) {
        let minimum_bottom = (top + MIN_REGION_SIZE).min(source_bottom);
        bottom = point_y.clamp(minimum_bottom, source_bottom);
    }

    Rect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    }
}

fn move_region(original: Rect, pointer_origin: POINT, point: POINT, source: Rect) -> Rect {
    let delta_x = i64::from(point.x) - i64::from(pointer_origin.x);
    let delta_y = i64::from(point.y) - i64::from(pointer_origin.y);
    let source_left = i64::from(source.x);
    let source_top = i64::from(source.y);
    let maximum_x = source_left + i64::from(source.width.saturating_sub(original.width));
    let maximum_y = source_top + i64::from(source.height.saturating_sub(original.height));
    Rect {
        x: (i64::from(original.x) + delta_x).clamp(source_left, maximum_x) as i32,
        y: (i64::from(original.y) + delta_y).clamp(source_top, maximum_y) as i32,
        width: original.width,
        height: original.height,
    }
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

fn set_arrow_cursor() {
    // SAFETY: IDC_ARROW is a shared system cursor that requires no explicit destruction.
    if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_ARROW) } {
        // SAFETY: cursor is a live shared system cursor.
        unsafe { SetCursor(cursor) };
    }
}

fn set_move_cursor() {
    // SAFETY: IDC_SIZEALL is a shared system cursor that requires no explicit destruction.
    if let Ok(cursor) = unsafe { LoadCursorW(None, IDC_SIZEALL) } {
        // SAFETY: cursor is a live shared system cursor.
        unsafe { SetCursor(cursor) };
    }
}

fn restore_input_context(previous_foreground: HWND) {
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

fn tight_pixels(frame: &CpuFrame) -> Result<Arc<[u8]>, CaptureError> {
    let tight_stride = frame
        .width
        .checked_mul(4)
        .ok_or_else(|| invalid_frame("overlay row size overflowed"))?
        as usize;
    if frame.stride_bytes as usize == tight_stride {
        return Ok(frame.pixels.clone());
    }
    let length = tight_stride
        .checked_mul(frame.height as usize)
        .ok_or_else(|| invalid_frame("overlay image size overflowed"))?;
    let mut pixels = vec![0_u8; length];
    for row in 0..frame.height as usize {
        let source_start = row * frame.stride_bytes as usize;
        let destination_start = row * tight_stride;
        pixels[destination_start..destination_start + tight_stride]
            .copy_from_slice(&frame.pixels[source_start..source_start + tight_stride]);
    }
    Ok(Arc::from(pixels))
}

fn top_down_bitmap_info(width: i32, height: i32) -> BITMAPINFO {
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

fn validate_frame(frame: &CpuFrame) -> Result<(), CaptureError> {
    if frame.format != PixelFormat::Bgra8Unorm || frame.origin != FrameOrigin::TopLeft {
        return Err(CaptureError {
            kind: CaptureErrorKind::Unsupported,
            backend: "windows-overlay",
            operation: "validate_frame",
            message: "selection overlay requires top-left BGRA8 pixels".to_owned(),
            retryable: false,
            native_code: None,
        });
    }
    if frame.width != frame.metadata.source_rect.width
        || frame.height != frame.metadata.source_rect.height
    {
        return Err(invalid_frame(
            "overlay frame dimensions do not match source bounds",
        ));
    }
    Ok(())
}

fn overlay_error(operation: &'static str, error: WindowsError) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-overlay",
        operation,
        message: error.to_string(),
        retryable: true,
        native_code: Some(i64::from(error.code().0)),
    }
}

fn last_error(operation: &'static str) -> CaptureError {
    overlay_error(operation, WindowsError::from_win32())
}

fn invalid_frame(message: impl Into<String>) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::InvalidFrame,
        backend: "windows-overlay",
        operation: "validate_frame",
        message: message.into(),
        retryable: false,
        native_code: None,
    }
}

fn duration_ns(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            intersect_rect(
                Rect {
                    x: -20,
                    y: 40,
                    width: 80,
                    height: 100,
                },
                Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ),
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
            hit_test_resize_handle(rect, POINT { x: 104, y: 204 }),
            Some(ResizeHandle::NorthWest)
        );
        assert_eq!(
            hit_test_resize_handle(rect, POINT { x: 250, y: 203 }),
            Some(ResizeHandle::North)
        );
        assert_eq!(
            hit_test_resize_handle(rect, POINT { x: 397, y: 300 }),
            Some(ResizeHandle::East)
        );
        assert_eq!(hit_test_resize_handle(rect, POINT { x: 250, y: 300 }), None);
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
    fn region_mode_restores_the_last_confirmed_rectangle() {
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
        let origin = ToolbarLayout::default_origin(1920, 1080);
        let layout = ToolbarLayout::new(1920, 1080, origin);
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
    fn dragged_toolbar_is_clamped_inside_the_display() {
        assert_eq!(
            ToolbarLayout::clamp_origin(1920, 1080, POINT { x: -400, y: 2000 }),
            POINT { x: 8, y: 990 }
        );
    }

    #[test]
    fn toolbar_position_round_trips_through_persistent_representation() {
        let position = POINT { x: 731, y: 944 };
        assert_eq!(
            unpack_toolbar_position(pack_toolbar_position(position)),
            position
        );
    }

    #[test]
    fn window_overview_arranges_independent_surfaces_in_centered_rows() {
        let dimensions = vec![(1600, 900); 5];
        let rectangles = layout_floating_windows(1920, 1080, &dimensions);
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
    fn overview_thumbnails_have_a_bounded_pixel_budget() {
        assert_eq!(scaled_dimensions(800, 600, 1_200_000), (800, 600));
        let (width, height) = scaled_dimensions(7_680, 4_320, 1_200_000);
        assert!(u64::from(width) * u64::from(height) <= 1_200_000);
        assert!((width as f64 / height as f64 - 16.0 / 9.0).abs() < 0.01);
    }

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
        let font = create_ui_font();
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
    fn overlay_resource_cache_reuses_matching_surfaces_and_rejects_other_sizes() {
        clear_overlay_resource_cache();
        let resources = OverlayResourceCache {
            surface: FrozenSurface::empty(8, 8).expect("surface"),
            back_buffer: FrozenSurface::empty(8, 8).expect("back buffer"),
            dimmer: FrozenSurface::new(1, 1, &[0, 0, 0, 255]).expect("dimmer"),
            blurred_background: None,
            overview_surface: None,
            region_cursor: RegionCursor::create(),
            font_resource: PrivateFontResource::register().expect("font"),
        };
        OVERLAY_RESOURCE_CACHE.with(|cache| *cache.borrow_mut() = Some(resources));
        let resources = take_overlay_resource_cache(8, 8).expect("matching cache");
        OVERLAY_RESOURCE_CACHE.with(|cache| *cache.borrow_mut() = Some(resources));
        assert!(take_overlay_resource_cache(16, 16).is_none());
        clear_overlay_resource_cache();
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
