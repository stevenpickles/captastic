use std::sync::mpsc;
use std::time::{Duration, Instant};

use captastic_core::{CaptureError, CaptureErrorKind};

use crate::window_capture::RenderDeadline;
use windows::core::{factory, ComInterface, Error as WindowsError, IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
    IDirect3D11CaptureFramePoolStatics2, IGraphicsCaptureSessionStatics,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, DXGI_ERROR_WAS_STILL_DRAWING};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_millis(300);
const GPU_MAP_TIMEOUT: Duration = Duration::from_millis(250);
const GPU_MAP_RETRY_DELAY: Duration = Duration::from_millis(1);
/// Time held back from the caller's deadline before waiting for the first frame, covering the
/// readback that has to follow it: staging-texture creation, the GPU copy, the map retry loop and
/// the row copy out of the mapping.
const READBACK_RESERVE: Duration = Duration::from_millis(200);
/// Time held back from the caller's deadline before waiting on the GPU map, covering what remains
/// after it: copying the rows out, then the caller's own crop, rescale and border reconstruction.
const PUBLISH_RESERVE: Duration = Duration::from_millis(100);

pub(crate) struct WgcWindowFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub(crate) fn capture_window(
    hwnd: HWND,
    deadline: RenderDeadline,
) -> Result<WgcWindowFrame, CaptureError> {
    let _apartment = WinRtApartment::initialize()?;
    // Building a device and a capture session costs real time. If there is not enough deadline
    // left to wait for a frame afterwards, say so now instead of spending the rest of it and
    // handing back a result the caller has already stopped listening for.
    if deadline
        .bounded_wait(FIRST_FRAME_TIMEOUT, READBACK_RESERVE)
        .is_zero()
    {
        return Err(deadline_exhausted("start_window_capture", deadline));
    }
    if !wgc_session_is_supported().map_err(|error| windows_error("check_support", error, false))? {
        return Err(capture_error(
            CaptureErrorKind::Unsupported,
            "check_support",
            "Windows Graphics Capture is not supported on this system",
            false,
            None,
        ));
    }

    let (device, context, winrt_device) = create_device()?;
    let item_factory = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|error| windows_error("get_item_factory", error, false))?;
    // SAFETY: hwnd is a live top-level window selected by the immediately preceding enumeration.
    let item: GraphicsCaptureItem = unsafe { item_factory.CreateForWindow(hwnd) }
        .map_err(|error| windows_error("create_item_for_window", error, false))?;
    let size = item
        .Size()
        .map_err(|error| windows_error("get_capture_item_size", error, true))?;
    if size.Width <= 0 || size.Height <= 0 {
        return Err(capture_error(
            CaptureErrorKind::SourceUnavailable,
            "get_capture_item_size",
            "Windows Graphics Capture returned empty window dimensions",
            true,
            None,
        ));
    }

    let pool = wgc_create_free_threaded_frame_pool(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )
    .map_err(|error| windows_error("create_frame_pool", error, true))?;
    let mut resources = WgcCaptureResources {
        pool,
        session: None,
    };
    let session = resources
        .pool
        .CreateCaptureSession(&item)
        .map_err(|error| windows_error("create_capture_session", error, true))?;
    resources.session = Some(session.clone());
    let _ = session.SetIsCursorCaptureEnabled(false);
    let _ = session.SetIsBorderRequired(false);

    let (sender, receiver) = mpsc::sync_channel(1);
    let handler =
        TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
            let _ = sender.try_send(());
            Ok(())
        });
    let token = resources
        .pool
        .FrameArrived(&handler)
        .map_err(|error| windows_error("subscribe_frame_arrived", error, true))?;
    let result = (|| {
        session
            .StartCapture()
            .map_err(|error| windows_error("start_capture", error, true))?;
        let first_frame_wait = deadline.bounded_wait(FIRST_FRAME_TIMEOUT, READBACK_RESERVE);
        if first_frame_wait.is_zero() {
            return Err(deadline_exhausted("wait_for_first_frame", deadline));
        }
        receiver.recv_timeout(first_frame_wait).map_err(|error| {
            let message = match error {
                mpsc::RecvTimeoutError::Timeout => format!(
                    "no window frame arrived within {} ms",
                    first_frame_wait.as_millis()
                ),
                mpsc::RecvTimeoutError::Disconnected => {
                    "the window frame notification channel disconnected".to_owned()
                }
            };
            capture_error(
                CaptureErrorKind::Timeout,
                "wait_for_first_frame",
                message,
                true,
                None,
            )
        })?;
        let frame = resources
            .pool
            .TryGetNextFrame()
            .map_err(|error| windows_error("get_next_frame", error, true))?;
        readback_frame(&device, &context, &frame, deadline)
    })();

    let _ = resources.pool.RemoveFrameArrived(token);
    result
}

// The generated class-static convenience methods (GraphicsCaptureSession::IsSupported,
// Direct3D11CaptureFramePool::CreateFreeThreaded) resolve their activation factory through
// windows_core::imp::FactoryCache, which stores agile factories in process-wide statics and
// never revalidates them. Captastic runs WGC on short-lived worker threads whose WinRtApartment
// guard calls RoUninitialize on exit, which may unload the WinRT DLL that backs those cached
// factories; a later worker then calls through a vtable pointing into unloaded code. The helpers
// below therefore acquire an owned, uncached factory via windows::core::factory() for every call
// and drop it before the apartment guard runs, so no factory outlives the apartment that
// activated it. Do not replace these with the generated statics and do not cache the factories.

fn wgc_session_is_supported() -> windows::core::Result<bool> {
    let statics = factory::<GraphicsCaptureSession, IGraphicsCaptureSessionStatics>()?;
    // SAFETY: Mirrors the generated binding: statics is a live IGraphicsCaptureSessionStatics
    // whose IsSupported slot writes a bool into the initialized result__ storage.
    unsafe {
        let mut result__ = std::mem::zeroed();
        (Interface::vtable(&statics).IsSupported)(Interface::as_raw(&statics), &mut result__)
            .from_abi(result__)
    }
}

fn wgc_create_free_threaded_frame_pool(
    device: &IDirect3DDevice,
    pixel_format: DirectXPixelFormat,
    number_of_buffers: i32,
    size: SizeInt32,
) -> windows::core::Result<Direct3D11CaptureFramePool> {
    let statics = factory::<Direct3D11CaptureFramePool, IDirect3D11CaptureFramePoolStatics2>()?;
    // SAFETY: Mirrors the generated binding: statics is a live IDirect3D11CaptureFramePoolStatics2,
    // device is a borrowed live IDirect3DDevice passed as its raw ABI pointer, and result__ is
    // initialized storage that receives the new frame pool's owned reference.
    unsafe {
        let mut result__ = std::mem::zeroed();
        (Interface::vtable(&statics).CreateFreeThreaded)(
            Interface::as_raw(&statics),
            Interface::as_raw(device),
            pixel_format,
            number_of_buffers,
            size,
            &mut result__,
        )
        .from_abi(result__)
    }
}

struct WgcCaptureResources {
    pool: Direct3D11CaptureFramePool,
    session: Option<GraphicsCaptureSession>,
}

impl Drop for WgcCaptureResources {
    fn drop(&mut self) {
        if let Some(session) = self.session.as_ref() {
            let _ = session.Close();
        }
        let _ = self.pool.Close();
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice), CaptureError> {
    let mut device = None;
    let mut context = None;
    // SAFETY: Output pointers reference initialized Option slots. A null adapter with HARDWARE
    // selects the default hardware adapter and the software-module handle must be null.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE(0),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|error| windows_error("create_d3d11_device", error, true))?;
    let device = device.ok_or_else(|| {
        capture_error(
            CaptureErrorKind::NativeFailure,
            "create_d3d11_device",
            "D3D11CreateDevice returned no device",
            true,
            None,
        )
    })?;
    let context = context.ok_or_else(|| {
        capture_error(
            CaptureErrorKind::NativeFailure,
            "create_d3d11_device",
            "D3D11CreateDevice returned no immediate context",
            true,
            None,
        )
    })?;
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|error| windows_error("cast_dxgi_device", error, false))?;
    // SAFETY: dxgi_device is a live D3D11 device interface and the returned inspectable owns its
    // reference independently.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) }
        .map_err(|error| windows_error("create_winrt_d3d_device", error, false))?;
    let winrt_device: IDirect3DDevice = inspectable
        .cast()
        .map_err(|error| windows_error("cast_winrt_d3d_device", error, false))?;
    Ok((device, context, winrt_device))
}

fn readback_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    deadline: RenderDeadline,
) -> Result<WgcWindowFrame, CaptureError> {
    let content = frame
        .ContentSize()
        .map_err(|error| windows_error("get_content_size", error, true))?;
    let width = u32::try_from(content.Width).map_err(|_| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "get_content_size",
            "captured window width is invalid",
            false,
            None,
        )
    })?;
    let height = u32::try_from(content.Height).map_err(|_| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "get_content_size",
            "captured window height is invalid",
            false,
            None,
        )
    })?;
    if width == 0 || height == 0 {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "get_content_size",
            "captured window frame is empty",
            true,
            None,
        ));
    }

    let surface = frame
        .Surface()
        .map_err(|error| windows_error("get_frame_surface", error, true))?;
    let access: IDirect3DDxgiInterfaceAccess = surface
        .cast()
        .map_err(|error| windows_error("cast_surface_access", error, false))?;
    // SAFETY: access is the documented WinRT-to-DXGI bridge for this live frame surface.
    let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
        .map_err(|error| windows_error("get_dxgi_texture", error, true))?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: desc is valid writable storage and texture remains live through the query.
    unsafe { texture.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        return Err(capture_error(
            CaptureErrorKind::Unsupported,
            "validate_frame_texture",
            format!(
                "unexpected WGC texture format={} size={}x{} content={}x{}",
                desc.Format.0, desc.Width, desc.Height, width, height
            ),
            false,
            None,
        ));
    }
    if desc.Width < width || desc.Height < height {
        return Err(capture_error(
            CaptureErrorKind::TopologyChanged,
            "validate_frame_texture",
            format!(
                "WGC window resized while capturing: texture={}x{} content={}x{}",
                desc.Width, desc.Height, width, height
            ),
            true,
            None,
        ));
    }

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: desc.Width,
        Height: desc.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: desc.Format,
        SampleDesc: desc.SampleDesc,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    // SAFETY: staging_desc requests owned CPU-readable storage populated by CopyResource below.
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
        .map_err(|error| windows_error("create_staging_texture", error, true))?;
    let staging = staging.ok_or_else(|| {
        capture_error(
            CaptureErrorKind::NativeFailure,
            "create_staging_texture",
            "D3D11 returned no staging texture",
            true,
            None,
        )
    })?;
    // SAFETY: The textures share a device and have matching descriptors.
    unsafe {
        context.CopyResource(&staging, &texture);
        context.Flush();
    }
    let mapped = MappedTexture::map(context, &staging, deadline)?;
    let row_bytes = width.checked_mul(4).ok_or_else(|| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "readback",
            "captured window row size overflowed",
            false,
            None,
        )
    })? as usize;
    if (mapped.data.RowPitch as usize) < row_bytes {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "readback",
            "mapped WGC row pitch is smaller than the captured content",
            false,
            None,
        ));
    }
    let len = row_bytes.checked_mul(height as usize).ok_or_else(|| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "readback",
            "captured window buffer size overflowed",
            false,
            None,
        )
    })?;
    let mut pixels = vec![0_u8; len];
    for row in 0..height as usize {
        // SAFETY: Map returned a non-null pointer covering RowPitch bytes for every texture row.
        let source = unsafe {
            std::slice::from_raw_parts(
                (mapped.data.pData as *const u8).add(row * mapped.data.RowPitch as usize),
                row_bytes,
            )
        };
        let destination = &mut pixels[row * row_bytes..(row + 1) * row_bytes];
        destination.copy_from_slice(source);
    }
    Ok(WgcWindowFrame {
        pixels,
        width,
        height,
    })
}

struct MappedTexture<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
    data: D3D11_MAPPED_SUBRESOURCE,
}

impl<'a> MappedTexture<'a> {
    fn map(
        context: &'a ID3D11DeviceContext,
        texture: &'a ID3D11Texture2D,
        deadline: RenderDeadline,
    ) -> Result<Self, CaptureError> {
        // A zero budget still gets one non-blocking attempt: the copy may already have landed.
        let map_timeout = deadline.bounded_wait(GPU_MAP_TIMEOUT, PUBLISH_RESERVE);
        let started = Instant::now();
        let data = loop {
            let mut data = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: texture is a CPU-readable staging resource and a successful Map is balanced
            // by this guard's Drop implementation.
            match unsafe {
                context.Map(
                    texture,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut data),
                )
            } {
                Ok(()) => break data,
                Err(error)
                    if error.code() == DXGI_ERROR_WAS_STILL_DRAWING
                        && started.elapsed() < map_timeout =>
                {
                    std::thread::sleep(GPU_MAP_RETRY_DELAY);
                }
                Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                    return Err(capture_error(
                        CaptureErrorKind::Timeout,
                        "map_staging_texture",
                        format!(
                            "WGC readback did not complete within {} ms",
                            map_timeout.as_millis()
                        ),
                        true,
                        Some(i64::from(error.code().0)),
                    ));
                }
                Err(error) => {
                    return Err(windows_error("map_staging_texture", error, true));
                }
            }
        };
        if data.pData.is_null() {
            // SAFETY: Map succeeded and must be balanced despite the invalid pointer.
            unsafe { context.Unmap(texture, 0) };
            return Err(capture_error(
                CaptureErrorKind::InvalidFrame,
                "map_staging_texture",
                "D3D11 returned a null mapped pointer",
                false,
                None,
            ));
        }
        Ok(Self {
            context,
            texture,
            data,
        })
    }
}

impl Drop for MappedTexture<'_> {
    fn drop(&mut self) {
        // SAFETY: Balances this guard's one successful Map call.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

struct WinRtApartment;

impl WinRtApartment {
    fn initialize() -> Result<Self, CaptureError> {
        // SAFETY: Initializes WinRT only for this fresh, short-lived capture worker thread.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .map_err(|error| windows_error("initialize_winrt", error, false))?;
        Ok(Self)
    }
}

impl Drop for WinRtApartment {
    fn drop(&mut self) {
        // SAFETY: Balances this worker thread's successful RoInitialize call.
        unsafe { RoUninitialize() };
    }
}

/// Reports that the caller's render deadline left no room for this wait.
///
/// Distinct from an ordinary WGC timeout: nothing here was slow, the attempt simply started too
/// late to finish inside a window the caller would still be listening at.
fn deadline_exhausted(operation: &'static str, deadline: RenderDeadline) -> CaptureError {
    capture_error(
        CaptureErrorKind::Timeout,
        operation,
        format!(
            "only {} ms of the window-render deadline remained, too little to capture and publish a frame",
            deadline.remaining().as_millis()
        ),
        true,
        None,
    )
}

fn windows_error(operation: &'static str, error: WindowsError, retryable: bool) -> CaptureError {
    session_explained_error(operation, error, retryable, crate::session::desktop_state)
}

/// Maps a Windows Graphics Capture failure, asking the session about it only when it was denied.
///
/// Every operation this file names is a desktop operation: creating a capture item for a window,
/// starting a capture session, reading back the frame the compositor produced. A secure desktop
/// refuses all of them, and until now every refusal arrived as `NativeFailure in
/// windows-graphics-capture/<operation>: Access is denied.` — the bare denial issue #51 is about,
/// on the backend that had no session check anywhere in it.
///
/// The gate lives in [`crate::session::denied_by_session`] rather than being repeated here: the
/// session is asked only on `E_ACCESSDENIED`, so the readback loop, the frame-pool creation and
/// every other failure keep the mapping they have always had without paying four syscalls for a
/// probe, and a denial the session cannot account for keeps its original message, native code and
/// the `retryable` flag its call site chose. That last part is what keeps the integrity-boundary
/// case honest: `PrintWindow` failing across normal-to-elevated is a real refusal, and it must not
/// be reported as a lock.
///
/// The mapping is here rather than in the `dxgi` module's `map_windows_error` for the reason #75
/// gave: this is one capture attempt end to end, not the shared mapper for a per-frame duplication
/// path. The probe is taken as a closure so the cost can be shown by a counter in a test.
fn session_explained_error(
    operation: &'static str,
    error: WindowsError,
    retryable: bool,
    probe_session: impl FnOnce() -> crate::session::DesktopState,
) -> CaptureError {
    crate::session::denied_by_session(WGC_BACKEND, operation, error.code().0, probe_session)
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

/// The backend name every Windows Graphics Capture failure reports itself under, in the log and in
/// these tests. A session-explained denial keeps it, so the operation a reader greps for does not
/// move when the explanation is added.
const WGC_BACKEND: &str = "windows-graphics-capture";

fn capture_error(
    kind: CaptureErrorKind,
    operation: &'static str,
    message: impl Into<String>,
    retryable: bool,
    native_code: Option<i64>,
) -> CaptureError {
    CaptureError {
        kind,
        backend: WGC_BACKEND,
        operation,
        message: message.into(),
        retryable,
        native_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{DesktopState, HRESULT_ACCESS_DENIED};
    use windows::core::HRESULT;

    /// The refusal a secure desktop gives a window capture, as Windows reports it.
    fn access_denied() -> WindowsError {
        WindowsError::from(HRESULT(HRESULT_ACCESS_DENIED))
    }

    /// A secure desktop refusing a window capture has to say so, on this backend too.
    ///
    /// `CreateForWindow` is where a capture of a window on a desktop this process cannot read is
    /// refused, and before this it reported `NativeFailure in
    /// windows-graphics-capture/create_item_for_window: Access is denied.` — a permissions problem
    /// the user did not have. The operation and backend are what a log reader greps for and neither
    /// moves; only the explanation is new.
    #[test]
    fn a_secure_desktop_explains_a_refused_window_capture_item() {
        let denied =
            session_explained_error("create_item_for_window", access_denied(), true, || {
                DesktopState::NotOurs {
                    desktop: Some("Winlogon".to_owned()),
                }
            });
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
        assert!(
            denied.message.contains("secure desktop"),
            "the whole point is that the message says so: {denied}"
        );
        assert!(denied.message.contains("Winlogon"), "{denied}");
        assert_eq!(denied.backend, WGC_BACKEND);
        assert_eq!(denied.operation, "create_item_for_window");
        assert!(denied.retryable);
    }

    /// Every other temporary session state explains it too, because every one of them refuses.
    ///
    /// A lock, a disconnected RDP session and a Remote Desktop session composed onto a virtual
    /// adapter deny a window capture for reasons no more a permissions problem than a secure
    /// desktop is, and all of them end when somebody comes back.
    #[test]
    fn any_session_that_owns_no_desktop_explains_a_refused_window_capture() {
        for state in [
            DesktopState::Locked { desktop: None },
            DesktopState::NotOurs { desktop: None },
            DesktopState::Detached {
                connect_state: "disconnected",
            },
            DesktopState::Remote {
                protocol: "Remote Desktop",
            },
        ] {
            let denied =
                session_explained_error("start_capture", access_denied(), true, || state.clone());
            assert_eq!(
                denied.kind,
                CaptureErrorKind::DesktopUnavailable,
                "{state:?} should explain the refusal"
            );
            assert_eq!(denied.message, state.to_string());
            assert_eq!(denied.operation, "start_capture");
        }
    }

    /// A refusal the session cannot account for stays exactly what it was.
    ///
    /// This backend already expects genuine refusals — `PrintWindow` across the normal-to-elevated
    /// integrity boundary is why Windows Graphics Capture is reached at all — and a comfortable
    /// message about a lock would hide the one case a user can actually act on. The caller's own
    /// `retryable` choice survives with the rest of the error, because whether a refusal is worth
    /// retrying is a property of the operation, not of the session.
    #[test]
    fn an_interactive_session_leaves_a_refused_window_capture_alone() {
        for state in [
            DesktopState::Interactive,
            DesktopState::Unknown {
                detail: "the input desktop could not be named".to_owned(),
            },
        ] {
            let denied =
                session_explained_error("create_item_for_window", access_denied(), false, || {
                    state.clone()
                });
            assert_eq!(
                denied.kind,
                CaptureErrorKind::NativeFailure,
                "{state:?} does not explain a refusal and must not hide one"
            );
            assert_eq!(denied.message, access_denied().to_string());
            assert_eq!(denied.native_code, Some(i64::from(HRESULT_ACCESS_DENIED)));
            assert_eq!(denied.backend, WGC_BACKEND);
            assert_eq!(denied.operation, "create_item_for_window");
            assert!(!denied.retryable, "the call site's own choice, preserved");
        }
    }

    /// The session probe costs four syscalls, and this mapper serves the whole readback path. It
    /// may only be paid on the one failure it can explain.
    ///
    /// A window that resized mid-readback, a device that was lost, a frame pool that could not be
    /// created: none of them are anything the session has an opinion about, and the counter here is
    /// what proves they never ask it.
    #[test]
    fn only_a_refused_window_capture_pays_for_the_session_probe() {
        let probes = std::cell::Cell::new(0_u32);
        let probe = || {
            probes.set(probes.get() + 1);
            DesktopState::Locked { desktop: None }
        };
        let still_drawing = WindowsError::from(DXGI_ERROR_WAS_STILL_DRAWING);
        let failed =
            session_explained_error("map_staging_texture", still_drawing.clone(), true, probe);
        assert_eq!(probes.get(), 0, "a non-denial must not ask the session");
        assert_eq!(failed.kind, CaptureErrorKind::NativeFailure);
        assert_eq!(failed.message, still_drawing.to_string());
        assert_eq!(
            failed.native_code,
            Some(i64::from(DXGI_ERROR_WAS_STILL_DRAWING.0))
        );

        let denied = session_explained_error("start_capture", access_denied(), true, || {
            probes.set(probes.get() + 1);
            DesktopState::Locked { desktop: None }
        });
        assert_eq!(probes.get(), 1, "a denial asks the session exactly once");
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
    }

    /// Regression test for the access violation caused by cached WinRT activation factories
    /// outliving RoUninitialize. Each cycle mimics one window-render worker: a fresh thread
    /// initializes an MTA, exercises both uncached statics helpers (session IsSupported and
    /// free-threaded frame-pool creation over a real D3D/WinRT device), drops every factory
    /// and WGC resource, and tears the apartment down before the next cycle begins. With the
    /// process-wide FactoryCache this pattern could dereference a vtable in an unloaded
    /// module; with owned factories every cycle must reactivate and succeed.
    ///
    /// Activating the Windows.Graphics.Capture factories is native and environment-dependent:
    /// hosts without a compatible graphical session or the Windows Graphics Capture service
    /// fail activation with 0x80070424 even though nothing on screen is ever read. Activation
    /// failure is a test failure, never a silent pass, so the test is ignored by default.
    /// Run it manually on a compatible interactive Windows machine with:
    ///
    /// cargo test --locked -p captastic-windows -- --ignored reacquires_wgc_statics_across_apartment_teardown
    #[test]
    #[ignore = "requires a compatible Windows graphical session and Windows Graphics Capture service"]
    fn reacquires_wgc_statics_across_apartment_teardown() {
        for cycle in 0..8 {
            let worker = std::thread::Builder::new()
                .name(format!("wgc-factory-stress-{cycle}"))
                .spawn(|| -> Result<(), String> {
                    let _apartment = WinRtApartment::initialize()
                        .map_err(|error| format!("initialize WinRT apartment: {error}"))?;
                    let supported = wgc_session_is_supported()
                        .map_err(|error| format!("acquire/call session statics: {error}"))?;
                    if !supported {
                        return Err(
                            "Windows Graphics Capture reports unsupported on this host".to_owned()
                        );
                    }
                    let (_device, _context, winrt_device) = create_device()
                        .map_err(|error| format!("create D3D/WinRT device: {error}"))?;
                    let pool = wgc_create_free_threaded_frame_pool(
                        &winrt_device,
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        2,
                        SizeInt32 {
                            Width: 16,
                            Height: 16,
                        },
                    )
                    .map_err(|error| format!("acquire/call frame-pool statics: {error}"))?;
                    let _ = pool.Close();
                    // The pool, devices, and owned factories all drop here, before the
                    // apartment guard calls RoUninitialize.
                    Ok(())
                })
                .expect("spawn factory stress worker");
            worker
                .join()
                .expect("factory stress worker must exit without crashing")
                .unwrap_or_else(|error| {
                    panic!("WGC factory lifecycle failed on cycle {cycle}: {error}")
                });
        }
    }

    #[test]
    #[ignore = "requires CAPTASTIC_TEST_WGC_WINDOW_HANDLE naming a live interactive window"]
    fn captures_live_window_without_desktop_composition() {
        let raw = std::env::var("CAPTASTIC_TEST_WGC_WINDOW_HANDLE")
            .expect("set CAPTASTIC_TEST_WGC_WINDOW_HANDLE")
            .parse::<isize>()
            .expect("numeric window handle");
        let frame = capture_window(
            HWND(raw),
            RenderDeadline::starting_now(Duration::from_secs(30)),
        )
        .expect("Windows Graphics Capture frame");
        assert!(frame.width > 16);
        assert!(frame.height > 16);
        assert_eq!(
            frame.pixels.len(),
            frame.width as usize * frame.height as usize * 4
        );
        assert!(frame
            .pixels
            .chunks_exact(4)
            .any(|pixel| pixel[..3] != [0, 0, 0]));
    }
}
