use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use captastic_core::{
    BackendCapabilities, CaptureBackend, CaptureError, CaptureErrorKind, CaptureMode,
    CaptureOutcome, CaptureRequest, CaptureSource, ColorSpace, CpuFrame, CursorAbsence,
    CursorCapture, CursorMode, DisplayId, DisplayInfo, EventRecorder, FrameMetadata, FrameOrigin,
    NativeFrame, PerfEventKind, PixelFormat, Rect, TimingProvenance,
};
use windows::core::{ComInterface, Error as WindowsError};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE180,
    DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource, DXGI_ADAPTER_DESC1,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_ERROR_WAS_STILL_DRAWING,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

const INITIAL_LATEST_FRAME_TIMEOUT: Duration = Duration::from_millis(100);
const GPU_MAP_TIMEOUT: Duration = Duration::from_millis(250);
const GPU_MAP_RETRY_DELAY: Duration = Duration::from_millis(1);
const BASE_DPI: u32 = 96;

static DISPLAY_CONFIGURATION_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(crate) fn mark_display_configuration_changed(reason: &'static str) {
    let generation = DISPLAY_CONFIGURATION_GENERATION
        .fetch_add(1, Ordering::AcqRel)
        .saturating_add(1);
    log::info!("display configuration invalidated generation={generation} reason={reason}");
}

pub(crate) fn display_configuration_generation() -> u64 {
    DISPLAY_CONFIGURATION_GENERATION.load(Ordering::Acquire)
}

/// The refusal every generation-carrying backend gives once the counter has moved past the value
/// it enumerated under. Retryable, because the fix is a rebuild and the daemon performs one.
pub(crate) fn stale_display_configuration_error(
    operation: &'static str,
    enumerated_generation: u64,
    current_generation: u64,
) -> CaptureError {
    capture_error(
        CaptureErrorKind::TopologyChanged,
        operation,
        format!(
            "display configuration changed from generation {enumerated_generation} to {current_generation}; recreate the capture backend"
        ),
        true,
        None,
    )
}

pub fn enumerate_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    enumerate_outputs().map(|outputs| outputs.into_iter().map(|output| output.info).collect())
}

/// Reports that there is no desktop to capture, naming the cause where the session can name it.
///
/// Every display being absent is one condition with several causes: the workstation is locked or a
/// secure prompt owns the desktop, the session is disconnected, the monitors are asleep, or they
/// are genuinely unplugged. DXGI presents all of them identically — no attached outputs — and the
/// session probe can distinguish only some of them.
///
/// They share a kind anyway, because they share the only answer that matters to a caller: there is
/// nothing to capture *right now*. Measured, not assumed: a locked session on the development host
/// enumerates and duplicates perfectly well, so keying this on the lock alone would have missed the
/// very failure issue #51 was filed about. The session state is reported when it explains
/// something, and its absence is not treated as evidence either way.
pub(crate) fn no_desktop_to_capture(operation: &'static str) -> CaptureError {
    let state = crate::session::desktop_state();
    let message = if state.is_interactive() {
        "no attached desktop displays were found".to_owned()
    } else {
        format!("no attached desktop displays were found: {state}")
    };
    capture_error(
        CaptureErrorKind::DesktopUnavailable,
        operation,
        message,
        true,
        None,
    )
}

/// Explains a *denied* desktop operation, when the session is what denied it.
///
/// Narrower than [`no_desktop_to_capture`] on purpose. An enumeration that comes back empty is
/// self-evidently "nothing to capture"; a denial is only a session problem if the session says so,
/// and swallowing an unexplained denial would hide a real fault behind a comfortable message.
pub(crate) fn desktop_obstacle(operation: &'static str) -> Option<CaptureError> {
    crate::session::session_obstacle(DXGI_BACKEND, operation, &crate::session::desktop_state())
}

/// The backend name every DXGI failure reports itself under, in the log and in these tests.
const DXGI_BACKEND: &str = "dxgi";

/// The operation a failed D3D11 device creation is reported under, in the log and in these tests.
const CREATE_DEVICE: &str = "create_d3d11_device";

/// Explains a refused D3D11 device creation, asking the session about it only when it was refused.
///
/// This is the first call a duplication session makes after enumerating, and it is deniable for the
/// same reasons the calls after it are: `docs/windows-backend.md` records that Desktop Duplication
/// access can be denied from an isolated sandbox even where the same executable succeeds in the
/// user's interactive session, and a session that has handed its desktop to the lock screen is the
/// same shape of refusal. Routed through the generic `map_windows_error` that refusal arrived as
/// `NativeFailure in dxgi/create_d3d11_device: Access is denied.`, with nothing on it to say the
/// desktop was the reason.
///
/// The kind is what the daemon reads, and it changes what the daemon *does*. A `NativeFailure` here
/// is neither a desktop wait nor a recoverable engine, so a locked session at daemon start reached
/// `ready_sender.send(Err(..))` and the daemon exited — the resident tool that is missing when the
/// user comes back, which is the failure issue #51 was filed about — and a locked session during a
/// rebuild was paced by `recovery_delay`, building a fresh D3D device against a lock screen on an
/// exponential back-off and warning once per attempt. `DesktopUnavailable` puts both on the path
/// `duplicate_output` denials already take: the daemon keeps its hotkeys and waits, polling the
/// two-syscall session probe every 500 ms instead of building devices, and builds the engine once
/// there is a desktop for it to duplicate.
///
/// The same guard exists one layer up, in `display_manager`'s `initialize`, but only for the
/// `pointer` and `virtual-desktop` policies and only once *every* display has failed; a
/// `display = primary` daemon never reaches it, and the per-display warn line it does reach quotes
/// the bare denial. As everywhere else, a denial the session cannot account for keeps its original
/// kind, message and native code, and the operation string is unchanged.
fn device_creation_error(
    error: WindowsError,
    probe_session: impl FnOnce() -> crate::session::DesktopState,
) -> CaptureError {
    crate::session::denied_by_session(DXGI_BACKEND, CREATE_DEVICE, error.code().0, probe_session)
        .unwrap_or_else(|| map_windows_error(CREATE_DEVICE, error))
}

pub(crate) fn enumerate_display_adapters() -> Result<Vec<(DisplayInfo, i64)>, CaptureError> {
    enumerate_outputs().map(|outputs| {
        outputs
            .into_iter()
            .map(|output| (output.info, output.adapter_luid))
            .collect()
    })
}

pub struct DxgiBackend {
    _com: ComApartment,
    device: ID3D11Device,
    context: Arc<Mutex<ID3D11DeviceContext>>,
    duplication: IDXGIOutputDuplication,
    /// The last pointer shape this duplication sent. Owned by the backend rather than by a frame
    /// because the compositor sends a shape only when it changes; a rebuild after AccessLost
    /// creates a new backend and so discards it, which is correct - the new duplication will send
    /// a fresh shape before it reports a position that needs one.
    pointer: crate::cursor::PointerCache,
    retained: RetainedTexture,
    latest: Option<RetainedFrame>,
    staging: Option<StagingTexture>,
    cpu_pool: CpuBufferPool,
    displays: Vec<DisplayInfo>,
    selected: DisplayInfo,
    capabilities: BackendCapabilities,
    qpc_frequency: i64,
    display_configuration_generation: u64,
    _thread_affine: PhantomData<Rc<()>>,
}

impl DxgiBackend {
    pub fn new_primary() -> Result<Self, CaptureError> {
        Self::new(&DisplayId::primary())
    }

    pub fn new(display_id: &DisplayId) -> Result<Self, CaptureError> {
        let com = ComApartment::initialize()?;
        // Sample the generation before enumerating so a display change that lands while we are
        // building the backend cannot be swallowed: the stored value stays behind the counter and
        // the first capture reports TopologyChanged instead of trusting stale DisplayInfo.
        let generation_before_enumeration = display_configuration_generation();
        let outputs = enumerate_outputs()?;
        let displays: Vec<_> = outputs.iter().map(|output| output.info.clone()).collect();
        let selected_output = select_display_index(&displays, display_id).ok_or_else(|| {
            // No displays at all is a different condition from the configured display being
            // missing, and only the second is about the configuration. The first says the desktop
            // is not there to capture — asleep, unplugged, or owned by a lock screen — and it is
            // the one worth waiting out rather than exiting over (issue #51).
            if displays.is_empty() {
                return no_desktop_to_capture("enumerate_outputs");
            }
            let available = displays
                .iter()
                .map(|display| display.id.0.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            capture_error(
                CaptureErrorKind::SourceUnavailable,
                "enumerate_outputs",
                format!(
                    "configured display {} is not attached; available displays: [{}]",
                    display_id.0, available
                ),
                false,
                None,
            )
        })?;

        let selected_record = &outputs[selected_output];
        let adapter: IDXGIAdapter = selected_record
            .adapter
            .cast()
            .map_err(|error| map_windows_error("cast_adapter", error))?;
        let mut device = None;
        let mut context = None;
        // SAFETY: Output pointers reference initialized Option slots for the duration of the call.
        // The selected adapter remains alive, the software module is null for a hardware adapter,
        // and all flags/SDK constants are supplied by the Windows bindings.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE(0),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|error| device_creation_error(error, crate::session::desktop_state))?;
        let device = device.ok_or_else(|| {
            capture_error(
                CaptureErrorKind::NativeFailure,
                CREATE_DEVICE,
                "D3D11CreateDevice returned no device",
                false,
                None,
            )
        })?;
        let context = context.ok_or_else(|| {
            capture_error(
                CaptureErrorKind::NativeFailure,
                CREATE_DEVICE,
                "D3D11CreateDevice returned no immediate context",
                false,
                None,
            )
        })?;
        let duplication = duplicate_output_as_bgra8(&selected_record.output, &device)?;
        let qpc_frequency = query_performance_frequency()?;
        let staging = None;
        let (retained_width, retained_height) = dimensions_after_rotation(
            selected_record.info.bounds.width,
            selected_record.info.bounds.height,
            selected_record.info.rotation_degrees,
        );
        let retained = RetainedTexture::create(
            &device,
            retained_desc(
                retained_width,
                retained_height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
            ),
        )?;
        let cpu_pool = CpuBufferPool::new(3);

        let backend = Self {
            _com: com,
            device,
            context: Arc::new(Mutex::new(context)),
            duplication,
            pointer: crate::cursor::PointerCache::default(),
            retained,
            latest: None,
            staging,
            cpu_pool,
            displays,
            selected: selected_record.info.clone(),
            capabilities: BackendCapabilities {
                display_capture: true,
                window_capture: false,
                virtual_desktop_capture: false,
                fresh_mode: true,
                latest_mode: true,
                cursor_control: true,
                hdr: false,
                presentation_time: true,
                warm_stream: false,
            },
            qpc_frequency,
            display_configuration_generation: generation_before_enumeration,
            _thread_affine: PhantomData,
        };
        Ok(backend)
    }

    /// Refuses with `TopologyChanged` once a display change has been marked since this backend
    /// enumerated. Its very first act on both the capture and the validation path, and made
    /// without consulting the display list, because that list is the thing that may be stale.
    fn check_display_configuration(&self, operation: &'static str) -> Result<(), CaptureError> {
        let current_generation = display_configuration_generation();
        if self.display_configuration_generation == current_generation {
            return Ok(());
        }
        Err(stale_display_configuration_error(
            operation,
            self.display_configuration_generation,
            current_generation,
        ))
    }
}

impl CaptureBackend for DxgiBackend {
    fn name(&self) -> &'static str {
        "dxgi"
    }

    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn displays(&self) -> &[DisplayInfo] {
        &self.displays
    }

    fn validate_display_configuration(&self) -> Result<(), CaptureError> {
        self.check_display_configuration("validate_display_configuration")
    }

    fn capture(
        &mut self,
        request: &CaptureRequest,
        recorder: &mut EventRecorder,
    ) -> Result<CaptureOutcome, CaptureError> {
        self.check_display_configuration("capture")?;
        match &request.source {
            CaptureSource::Display(id)
                if *id == self.selected.id || (id.0 == "primary" && self.selected.is_primary) => {}
            CaptureSource::Display(id) => {
                return Err(capture_error(
                    CaptureErrorKind::SourceUnavailable,
                    "capture",
                    format!(
                        "backend was initialized for {}, not {}",
                        self.selected.id.0, id.0
                    ),
                    false,
                    None,
                ));
            }
            CaptureSource::VirtualDesktop => {
                return Err(capture_error(
                    CaptureErrorKind::Unsupported,
                    "capture",
                    "a single-output DXGI backend cannot capture the virtual desktop; use the DXGI display manager",
                    false,
                    None,
                ));
            }
        }
        recorder.record(request.id, PerfEventKind::CaptureRequested, 0);
        let timeout = match request.mode {
            CaptureMode::Fresh { timeout_ms } => Duration::from_millis(timeout_ms),
            CaptureMode::Latest { max_age_ms } => {
                return self.capture_latest(request, max_age_ms, recorder)
            }
        };

        let qpc_anchor = query_performance_counter()?;
        let anchor_offset_ns = duration_ns_i64(request.triggered_at.elapsed());
        let deadline = request.triggered_at.checked_add(timeout).ok_or_else(|| {
            capture_error(
                CaptureErrorKind::Timeout,
                "capture",
                "fresh capture deadline overflowed",
                false,
                None,
            )
        })?;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // Nothing was presented in the window - which, on a static desktop, is because
                // there was nothing to present. A retained frame that a probe proves identical to
                // the screen satisfies what `fresh` asks for in every way the caller can observe,
                // so it is used rather than refused (ADR 0003, amended 2026-08-17). Without this,
                // `fresh` fails on any idle desktop, and `fresh` + `virtual_desktop` fails whenever
                // *any* display is idle - which is most of the time on a real multi-monitor desk.
                if self.refresh_latest_on_demand().unwrap_or(false) {
                    return self.capture_latest(request, None, recorder);
                }
                return Err(capture_error(
                    CaptureErrorKind::Timeout,
                    "acquire_next_frame",
                    "no post-trigger desktop frame arrived before the timeout, and no retained                      frame could be proven current",
                    true,
                    Some(i64::from(DXGI_ERROR_WAIT_TIMEOUT.0)),
                ));
            }
            let timeout_ms = duration_to_timeout_ms(remaining);
            let acquired = AcquiredFrame::acquire(&self.duplication, timeout_ms)?;
            self.record_pointer_report(&acquired)?;
            let presentation_offset_ns = if acquired.info.LastPresentTime == 0 {
                None
            } else {
                let qpc_delta = acquired.info.LastPresentTime.saturating_sub(qpc_anchor);
                Some(anchor_offset_ns.saturating_add(qpc_to_ns(qpc_delta, self.qpc_frequency)))
            };

            if presentation_offset_ns.is_none_or(|offset| offset < 0) {
                acquired.release()?;
                continue;
            }

            let texture: ID3D11Texture2D = acquired
                .resource
                .cast()
                .map_err(|error| map_windows_error("cast_desktop_texture", error))?;
            let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: texture_desc is valid writable storage and texture is a live acquired resource.
            unsafe { texture.GetDesc(&mut texture_desc) };
            if texture_desc.Width == 0 || texture_desc.Height == 0 {
                return Err(capture_error(
                    CaptureErrorKind::InvalidFrame,
                    "texture_desc",
                    "DXGI returned an empty desktop texture",
                    false,
                    None,
                ));
            }

            let native_ready_ns = duration_ns_u64(request.triggered_at.elapsed());
            recorder.record(request.id, PerfEventKind::NativeFrameReady, native_ready_ns);
            // Same incremental contract as the live path: this frame describes the pointer only
            // if it changed, so the report is recorded when it comes and remembered when it does
            // not. This is the `latest` path, which is what the resident daemon runs, so reading
            // the raw fields here meant a retained frame almost always recorded "no pointer".
            if acquired.info.LastMouseUpdateTime != 0 {
                self.pointer.store_position(
                    acquired.info.PointerPosition.Position.x,
                    acquired.info.PointerPosition.Position.y,
                    acquired.info.PointerPosition.Visible.as_bool(),
                );
            }
            let retained_pointer_at = self
                .pointer
                .position()
                .filter(|position| position.visible)
                .map(|position| (position.x, position.y));
            let frame_generation = self.retain_frame(
                &texture,
                texture_desc,
                acquired.info.LastPresentTime,
                retained_pointer_at,
            )?;
            let mut metadata = FrameMetadata {
                capture_id: request.id,
                backend: self.name().to_owned(),
                display_id: self.selected.id.clone(),
                source_rect: self.selected.bounds,
                rotation_degrees: self.selected.rotation_degrees,
                capture_mode: request.mode.clone(),
                presentation_offset_ns,
                timing_provenance: TimingProvenance::OsPresentationTime,
                native_ready_offset_ns: native_ready_ns,
                cpu_ready_offset_ns: None,
                frame_age_ns: Some(0),
                verified_current_offset_ns: None,
                frame_generation,
                copy_count: 1,
                pool_slot: None,
                cursor: None,
            };
            let native_texture = if request.retain_native_frame {
                metadata.copy_count = metadata.copy_count.saturating_add(1);
                Some(self.snapshot_texture(&texture, texture_desc)?)
            } else {
                None
            };
            let cpu_frame = if request.cpu_frame {
                let pointer_at = self.pointer_for(&acquired, &request.cursor, &mut metadata)?;
                Some(self.readback(
                    &texture,
                    texture_desc,
                    request.triggered_at,
                    &mut metadata,
                    pointer_at,
                    recorder,
                )?)
            } else {
                self.ensure_device_present("capture_fresh")?;
                None
            };
            let native_frame = native_texture.map(|texture| {
                Arc::new(DxgiGpuFrame {
                    device: self.device.clone(),
                    context: self.context.clone(),
                    texture,
                    desc: texture_desc,
                    metadata: metadata.clone(),
                }) as Arc<dyn NativeFrame>
            });
            // The capture is already complete here: the CPU pixels are copied and the snapshot is
            // an independent texture, so a failing ReleaseFrame reports that the duplication
            // session is going away, not that these pixels are bad. Discarding a good frame over
            // it helps nobody -- the next acquire raises ACCESS_LOST on its own and drives
            // recovery from there. Device loss is not hidden by this: the readback reports it
            // through Map, and the native-only branch above asks the device directly.
            if let Err(error) = acquired.release() {
                log::warn!(
                    "capture {} could not release its duplication frame; returning the completed capture anyway: {error}",
                    request.id.0
                );
            }
            return Ok(CaptureOutcome {
                metadata,
                frame: cpu_frame,
                native_frame,
            });
        }
    }
}

impl DxgiBackend {
    fn capture_latest(
        &mut self,
        request: &CaptureRequest,
        max_age_ms: Option<u64>,
        recorder: &mut EventRecorder,
    ) -> Result<CaptureOutcome, CaptureError> {
        let verified_current = self.refresh_latest_on_demand()?;
        let latest = self.latest.ok_or_else(|| {
            capture_error(
                CaptureErrorKind::SourceUnavailable,
                "capture_latest",
                "no retained desktop frame is available yet",
                true,
                None,
            )
        })?;
        let qpc_anchor = query_performance_counter()?;
        let anchor_offset_ns = duration_ns_i64(request.triggered_at.elapsed());
        let presentation_offset_ns = anchor_offset_ns.saturating_add(qpc_to_ns(
            latest.presentation_qpc.saturating_sub(qpc_anchor),
            self.qpc_frequency,
        ));
        let frame_age_ns = if presentation_offset_ns < 0 {
            presentation_offset_ns.unsigned_abs()
        } else {
            0
        };
        // The verification happened during the probe above, a few microseconds ago; recording the
        // elapsed time now overstates its age slightly, which is the safe direction.
        let verified_current_offset_ns =
            verified_current.then(|| duration_ns_u64(request.triggered_at.elapsed()));
        if frame_is_too_stale(max_age_ms, frame_age_ns, verified_current) {
            return Err(capture_error(
                CaptureErrorKind::Timeout,
                "capture_latest",
                format!(
                    "retained frame age {:.3} ms exceeds the configured maximum",
                    frame_age_ns as f64 / 1_000_000.0
                ),
                true,
                None,
            ));
        }

        let native_ready_ns = duration_ns_u64(request.triggered_at.elapsed());
        recorder.record(request.id, PerfEventKind::NativeFrameReady, native_ready_ns);
        let mut metadata = FrameMetadata {
            capture_id: request.id,
            backend: self.name().to_owned(),
            display_id: self.selected.id.clone(),
            source_rect: self.selected.bounds,
            rotation_degrees: self.selected.rotation_degrees,
            capture_mode: request.mode.clone(),
            presentation_offset_ns: Some(presentation_offset_ns),
            timing_provenance: TimingProvenance::OsPresentationTime,
            native_ready_offset_ns: native_ready_ns,
            cpu_ready_offset_ns: None,
            frame_age_ns: Some(frame_age_ns),
            verified_current_offset_ns,
            frame_generation: Some(latest.generation),
            copy_count: 1,
            pool_slot: None,
            cursor: None,
        };
        let retained_texture = self.retained.texture.clone();
        let retained_desc = self.retained.desc;
        let native_texture = if request.retain_native_frame {
            metadata.copy_count = metadata.copy_count.saturating_add(1);
            Some(self.snapshot_texture(&retained_texture, retained_desc)?)
        } else {
            None
        };
        let cpu_frame = if request.cpu_frame {
            let pointer_at =
                self.retained_pointer(latest.pointer_at, &request.cursor, &mut metadata);
            Some(self.readback(
                &retained_texture,
                retained_desc,
                request.triggered_at,
                &mut metadata,
                pointer_at,
                recorder,
            )?)
        } else {
            self.ensure_device_present("capture_latest")?;
            None
        };
        let native_frame = native_texture.map(|texture| {
            Arc::new(DxgiGpuFrame {
                device: self.device.clone(),
                context: self.context.clone(),
                texture,
                desc: retained_desc,
                metadata: metadata.clone(),
            }) as Arc<dyn NativeFrame>
        });
        Ok(CaptureOutcome {
            metadata,
            frame: cpu_frame,
            native_frame,
        })
    }

    /// Brings the retained frame up to date, reporting whether it was *proven* up to date.
    ///
    /// `Ok(true)` means a zero-timeout acquisition found nothing pending, which is not a failure to
    /// find a frame - it is positive evidence that nothing has been presented since the retained
    /// one, because duplication yields only on change. That evidence used to be discarded here.
    fn refresh_latest_on_demand(&mut self) -> Result<bool, CaptureError> {
        match self.refresh_latest(0) {
            // A new frame: current by construction, with nothing to verify.
            Ok(true) => return Ok(false),
            Ok(false) => {}
            Err(error) if error.kind == CaptureErrorKind::Timeout => {
                if self.latest.is_some() {
                    return Ok(true);
                }
            }
            Err(error) => return Err(error),
        }
        if self.latest.is_some() {
            return Ok(false);
        }

        let deadline = Instant::now() + INITIAL_LATEST_FRAME_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(capture_error(
                    CaptureErrorKind::Timeout,
                    "capture_latest",
                    "the desktop has not changed since Captastic started, so no frame has ever                      been available to retain",
                    // Not retryable. Duplication produces a frame only when the desktop image
                    // changes, so retrying cannot succeed until something repaints - and a caller
                    // that retries in a tight loop learns nothing while burning the deadline.
                    false,
                    Some(i64::from(DXGI_ERROR_WAIT_TIMEOUT.0)),
                ));
            }
            match self.refresh_latest(duration_to_timeout_ms(remaining)) {
                // Freshly presented, so there is nothing to have verified.
                Ok(true) => return Ok(false),
                Ok(false) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn refresh_latest(&mut self, timeout_ms: u32) -> Result<bool, CaptureError> {
        let acquired = AcquiredFrame::acquire(&self.duplication, timeout_ms)?;
        self.record_pointer_report(&acquired)?;
        if acquired.info.LastPresentTime == 0 {
            acquired.release()?;
            return Ok(false);
        }
        let texture: ID3D11Texture2D = acquired
            .resource
            .cast()
            .map_err(|error| map_windows_error("cast_desktop_texture", error))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: desc is valid writable storage and texture is a live acquired resource.
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Width == 0 || desc.Height == 0 {
            return Err(capture_error(
                CaptureErrorKind::InvalidFrame,
                "refresh_latest",
                "DXGI returned an empty desktop texture",
                false,
                None,
            ));
        }
        // Cached before the frame is retained, so a shape delivered alongside this frame is
        // available when the frame is materialized later.
        self.refresh_pointer_shape(&acquired)?;
        // From the cache, not from this frame's raw fields, for the same reason the live path
        // reads the cache: `PointerPosition` is filled in only on a frame carrying a mouse update,
        // and a frame retained on a plain repaint would otherwise record "no pointer" while the
        // cache knew exactly where it was. Measured on an idle desktop with the pointer parked and
        // never touched, 9 of 40 `latest` captures reported `NotVisible`. `record_pointer_report`
        // ran at the top of this function, so the cache already accounts for this acquisition.
        let pointer_at = self
            .pointer
            .position()
            .filter(|position| position.visible)
            .map(|position| (position.x, position.y));
        self.retain_frame(&texture, desc, acquired.info.LastPresentTime, pointer_at)?;
        acquired.release()?;
        Ok(true)
    }

    fn retain_frame(
        &mut self,
        source: &ID3D11Texture2D,
        source_desc: D3D11_TEXTURE2D_DESC,
        presentation_qpc: i64,
        pointer_at: Option<(i32, i32)>,
    ) -> Result<Option<u64>, CaptureError> {
        if presentation_qpc == 0 {
            return Ok(None);
        }
        if self.retained.desc.Width != source_desc.Width
            || self.retained.desc.Height != source_desc.Height
            || self.retained.desc.Format != source_desc.Format
        {
            let (normalized_width, normalized_height) = dimensions_after_rotation(
                source_desc.Width,
                source_desc.Height,
                self.selected.rotation_degrees,
            );
            self.retained = RetainedTexture::create(
                &self.device,
                retained_desc(
                    source_desc.Width,
                    source_desc.Height,
                    source_desc.Format,
                    source_desc.SampleDesc,
                ),
            )?;
            self.selected.bounds.width = normalized_width;
            self.selected.bounds.height = normalized_height;
            for display in &mut self.displays {
                if display.id == self.selected.id {
                    display.bounds = self.selected.bounds;
                }
            }
        }
        let context = lock_context(&self.context)?;
        // SAFETY: retained/source are live resources on the same device with matching layouts.
        // The immediate context is serialized by the shared context lock.
        unsafe { context.CopyResource(&self.retained.texture, source) };
        let generation = self
            .latest
            .map_or(1, |frame| frame.generation.saturating_add(1));
        self.latest = Some(RetainedFrame {
            presentation_qpc,
            generation,
            pointer_at,
        });
        Ok(Some(generation))
    }

    /// Takes whatever pointer information this frame carried, and says what to do with it.
    ///
    /// Returns `Ok(None)` when there is nothing to draw, with the reason already recorded on the
    /// metadata - a request that could not be honoured is reported rather than left looking like a
    /// request never made.
    /// Takes whatever a freshly acquired frame says about the pointer, before anything decides
    /// whether to keep the frame.
    ///
    /// DXGI's pointer stream is incremental *and* per-acquisition: each `AcquireNextFrame` reports
    /// the changes since the previous one, and a report not read is gone. Recording at the point
    /// of use loses every report that arrives on a frame the caller then discards - a stale frame
    /// rejected in `fresh` mode, a frame drained in `latest` mode, or any frame captured while the
    /// cursor was excluded. On this machine the very first acquisition carried the position *and*
    /// a 9,216-byte shape, both were thrown away, and no further report arrived for seconds at a
    /// time - so composition had nothing to draw and reported the pointer missing.
    ///
    /// Every acquisition feeds the cache, whatever becomes of its pixels.
    fn record_pointer_report(&mut self, acquired: &AcquiredFrame) -> Result<(), CaptureError> {
        if acquired.info.LastMouseUpdateTime != 0 {
            self.pointer.store_position(
                acquired.info.PointerPosition.Position.x,
                acquired.info.PointerPosition.Position.y,
                acquired.info.PointerPosition.Visible.as_bool(),
            );
        }
        self.refresh_pointer_shape(acquired)
    }

    fn pointer_for(
        &mut self,
        acquired: &AcquiredFrame,
        mode: &CursorMode,
        metadata: &mut FrameMetadata,
    ) -> Result<Option<(i32, i32)>, CaptureError> {
        if matches!(mode, CursorMode::Exclude) {
            metadata.cursor = Some(CursorCapture::Excluded);
            return Ok(None);
        }
        // Cached before the visibility test, not after: a shape can arrive on the same frame that
        // moves the pointer onto another output, and throwing it away would mean drawing nothing
        // when it comes back.
        self.refresh_pointer_shape(acquired)?;

        // A frame describes the pointer only when the pointer changed, which DXGI signals with a
        // non-zero LastMouseUpdateTime; otherwise `PointerPosition` holds defaults that read as an
        // invisible pointer at the origin. Taking those at face value is why composition almost
        // never happened: a still pointer over a repainting desktop reported "not visible" on
        // every frame. The report is recorded when it arrives and remembered when it does not,
        // exactly as the shape already was.
        if acquired.info.LastMouseUpdateTime != 0 {
            self.pointer.store_position(
                acquired.info.PointerPosition.Position.x,
                acquired.info.PointerPosition.Position.y,
                acquired.info.PointerPosition.Visible.as_bool(),
            );
        }
        let Some(position) = self.pointer.position() else {
            metadata.cursor = Some(CursorCapture::Absent {
                reason: CursorAbsence::PositionNotYetKnown,
            });
            return Ok(None);
        };
        if !position.visible {
            metadata.cursor = Some(CursorCapture::Absent {
                reason: CursorAbsence::NotVisible,
            });
            return Ok(None);
        }
        // Nothing here turns with the display, and that is a measurement rather than an assumption:
        // `PointerPosition` is reported in the same upright desktop space `normalize_bgra_into`
        // produces, and `GetFramePointerShape` hands back the logical cursor bitmap, upright, at
        // every orientation. See `cursor_composition_on_a_rotated_display_is_upright_and_in_place`.
        if self.pointer.current().is_none() {
            metadata.cursor = Some(CursorCapture::Absent {
                reason: CursorAbsence::ShapeNotYetKnown,
            });
            return Ok(None);
        }
        Ok(Some((position.x, position.y)))
    }

    /// The pointer decision for a frame that was captured earlier and is being materialized now.
    ///
    /// Shares every rule with the live path except where the position comes from: this one was
    /// recorded when the frame was captured, because that is the only position consistent with its
    /// pixels.
    fn retained_pointer(
        &self,
        pointer_at: Option<(i32, i32)>,
        mode: &CursorMode,
        metadata: &mut FrameMetadata,
    ) -> Option<(i32, i32)> {
        if matches!(mode, CursorMode::Exclude) {
            metadata.cursor = Some(CursorCapture::Excluded);
            return None;
        }
        let reason = if pointer_at.is_none() {
            CursorAbsence::NotVisible
        } else if self.pointer.current().is_none() {
            CursorAbsence::ShapeNotYetKnown
        } else {
            metadata.cursor = None;
            return pointer_at;
        };
        metadata.cursor = Some(CursorCapture::Absent { reason });
        None
    }

    /// Stores this frame's pointer shape, if it carried one.
    fn refresh_pointer_shape(&mut self, acquired: &AcquiredFrame) -> Result<(), CaptureError> {
        let size = acquired.info.PointerShapeBufferSize;
        if size == 0 {
            return Ok(());
        }
        let mut buffer = vec![0_u8; size as usize];
        let mut required = 0_u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: The buffer is `size` bytes, which is the size DXGI just reported for this
        // frame's shape, and both out-parameters are valid writable storage.
        unsafe {
            self.duplication.GetFramePointerShape(
                size,
                buffer.as_mut_ptr().cast(),
                &mut required,
                &mut info,
            )
        }
        .map_err(|error| map_windows_error("get_frame_pointer_shape", error))?;

        let shape_type = info.Type as i32;
        let kind = if shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
            crate::cursor::PointerShapeKind::Color
        } else if shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
            crate::cursor::PointerShapeKind::Monochrome
        } else if shape_type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
            crate::cursor::PointerShapeKind::MaskedColor
        } else {
            // Left uncached deliberately: an unknown encoding would otherwise be drawn as
            // whichever of the three it was mistaken for.
            log::debug!("ignoring pointer shape of unknown type {shape_type}");
            return Ok(());
        };
        // A monochrome shape's buffer is two masks stacked, so its drawn height is half what DXGI
        // reports here.
        let height = if matches!(kind, crate::cursor::PointerShapeKind::Monochrome) {
            info.Height / 2
        } else {
            info.Height
        };
        self.pointer.store(crate::cursor::PointerShape {
            kind,
            width: info.Width,
            height,
            pitch: info.Pitch,
            pixels: buffer,
        });
        Ok(())
    }

    fn readback(
        &mut self,
        source: &ID3D11Texture2D,
        source_desc: D3D11_TEXTURE2D_DESC,
        triggered_at: Instant,
        metadata: &mut FrameMetadata,
        pointer_at: Option<(i32, i32)>,
        recorder: &mut EventRecorder,
    ) -> Result<CpuFrame, CaptureError> {
        if source_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(capture_error(
                CaptureErrorKind::Unsupported,
                "readback",
                describe_unsupported_format(source_desc.Format),
                false,
                None,
            ));
        }
        let staging = self.staging_texture(source_desc)?;
        recorder.record(metadata.capture_id, PerfEventKind::ReadbackStarted, 0);
        let context = lock_context(&self.context)?;
        // SAFETY: Both textures are live resources created on the same D3D11 device and have
        // matching dimensions/format. Immediate-context access is serialized by the lock.
        unsafe { context.CopyResource(&staging, source) };
        let mapped = MappedTexture::map(&context, &staging)?;
        let raw_tight_stride = source_desc.Width.checked_mul(4).ok_or_else(|| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "readback",
                "raw CPU frame stride overflowed",
                false,
                None,
            )
        })?;
        if mapped.data.RowPitch < raw_tight_stride {
            return Err(capture_error(
                CaptureErrorKind::InvalidFrame,
                "readback",
                format!(
                    "mapped row pitch {} is smaller than required {}",
                    mapped.data.RowPitch, raw_tight_stride
                ),
                false,
                None,
            ));
        }
        let layout = normalized_layout(
            source_desc.Width,
            source_desc.Height,
            metadata.rotation_degrees,
        )?;
        if layout.width != metadata.source_rect.width
            || layout.height != metadata.source_rect.height
        {
            return Err(capture_error(
                CaptureErrorKind::TopologyChanged,
                "readback",
                format!(
                    "normalized DXGI dimensions {}x{} do not match display bounds {}x{}",
                    layout.width,
                    layout.height,
                    metadata.source_rect.width,
                    metadata.source_rect.height
                ),
                true,
                None,
            ));
        }
        let len = frame_byte_len(layout.width, layout.height)?;
        let slot_index = self.cpu_pool.available_index(len).ok_or_else(|| {
            capture_error(
                CaptureErrorKind::BufferExhausted,
                "readback",
                "all preallocated CPU frame slots are still in use",
                true,
                None,
            )
        })?;
        let source_stride = mapped.data.RowPitch as usize;
        let source_len = source_stride
            .checked_mul(source_desc.Height as usize)
            .ok_or_else(|| {
                capture_error(
                    CaptureErrorKind::InvalidFrame,
                    "readback",
                    "mapped DXGI source size overflowed",
                    false,
                    None,
                )
            })?;
        {
            let pixels = Arc::get_mut(
                self.cpu_pool.slots[slot_index]
                    .as_mut()
                    .expect("available slot is initialized"),
            )
            .expect("available slot has exactly one owner");
            // SAFETY: Map returned a non-null pointer covering RowPitch bytes for every raw row;
            // the slice does not outlive the mapped guard.
            let source_pixels =
                unsafe { std::slice::from_raw_parts(mapped.data.pData.cast::<u8>(), source_len) };
            normalize_bgra_into(
                source_pixels,
                source_desc.Width,
                source_desc.Height,
                source_stride,
                metadata.rotation_degrees,
                pixels,
            )?;
            // Drawn here, into the full frame, before any crop. A pointer straddling a selection
            // edge is then clipped by that crop exactly as it was clipped by the edge of the
            // screen, without a second bounds test that could disagree with the first.
            if let (Some((x, y)), Some(shape)) = (pointer_at, self.pointer.current()) {
                let composited = std::time::Instant::now();
                metadata.cursor = Some(crate::cursor::composite_pointer(
                    pixels,
                    layout.width,
                    layout.height,
                    layout.stride as usize,
                    &crate::cursor::PointerSample { x, y, shape },
                ));
                log::debug!(
                    "capture {} composited its cursor in {:.3} ms",
                    metadata.capture_id.0,
                    composited.elapsed().as_secs_f64() * 1_000.0
                );
            }
        }
        drop(mapped);
        let pixels = self.cpu_pool.slots[slot_index]
            .as_ref()
            .expect("available slot is initialized")
            .clone();
        let cpu_ready_ns = duration_ns_u64(triggered_at.elapsed());
        metadata.cpu_ready_offset_ns = Some(cpu_ready_ns);
        metadata.copy_count = metadata.copy_count.saturating_add(2);
        metadata.pool_slot = Some(slot_index as u16);
        recorder.record(
            metadata.capture_id,
            PerfEventKind::CpuFrameReady,
            cpu_ready_ns,
        );
        CpuFrame::new(
            pixels,
            layout.width,
            layout.height,
            layout.stride,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata.clone(),
        )
        .map_err(|error| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "readback",
                error.to_string(),
                false,
                None,
            )
        })
    }
    fn staging_texture(
        &mut self,
        source_desc: D3D11_TEXTURE2D_DESC,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        let needs_rebuild = self.staging.as_ref().is_none_or(|staging| {
            staging.width != source_desc.Width
                || staging.height != source_desc.Height
                || staging.format != source_desc.Format
        });
        if needs_rebuild {
            self.staging = Some(StagingTexture::create(
                &self.device,
                staging_desc(
                    source_desc.Width,
                    source_desc.Height,
                    source_desc.Format,
                    source_desc.SampleDesc,
                ),
            )?);
            self.cpu_pool = CpuBufferPool::new(3);
        }
        Ok(self
            .staging
            .as_ref()
            .expect("staging initialized")
            .texture
            .clone())
    }

    /// Reports a device loss that the silent copies on the native-only path would otherwise hide.
    ///
    /// `CopyResource` returns `()`, so when the caller skips the CPU readback nothing between the
    /// acquire and the returned snapshot carries an HRESULT: a TDR in that window would surface as
    /// a successful capture holding undefined pixels, and recovery would never run. The readback
    /// path gets this for free because `Map` fails with `DXGI_ERROR_DEVICE_REMOVED`.
    fn ensure_device_present(&self, operation: &'static str) -> Result<(), CaptureError> {
        // SAFETY: the device is a live COM interface owned by this backend for its whole lifetime.
        match unsafe { self.device.GetDeviceRemovedReason() } {
            Ok(()) => Ok(()),
            Err(reason) => Err(device_removed_error(operation, reason)),
        }
    }

    fn snapshot_texture(
        &self,
        source: &ID3D11Texture2D,
        source_desc: D3D11_TEXTURE2D_DESC,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        let snapshot = RetainedTexture::create(
            &self.device,
            retained_desc(
                source_desc.Width,
                source_desc.Height,
                source_desc.Format,
                source_desc.SampleDesc,
            ),
        )?;
        let context = lock_context(&self.context)?;
        // SAFETY: Both resources belong to the same device and have identical layouts. The
        // immutable snapshot is not exposed until this serialized context operation returns.
        unsafe { context.CopyResource(&snapshot.texture, source) };
        Ok(snapshot.texture)
    }
}

#[derive(Debug)]
struct DxgiGpuFrame {
    device: ID3D11Device,
    context: Arc<Mutex<ID3D11DeviceContext>>,
    texture: ID3D11Texture2D,
    desc: D3D11_TEXTURE2D_DESC,
    metadata: FrameMetadata,
}

impl NativeFrame for DxgiGpuFrame {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Debug)]
pub struct GpuMaterialization {
    pub frame: CpuFrame,
    pub gpu_copy_submit_ns: u64,
    pub map_wait_ns: u64,
    pub cpu_copy_ns: u64,
    pub total_ns: u64,
    pub bytes_read: usize,
    pub full_frame_bytes: usize,
    pub bytes_avoided: usize,
    pub contiguous_rows: bool,
}

/// Materializes an absolute display region from an immutable DXGI snapshot.
///
/// `Ok(None)` means the supplied native frame belongs to another backend. This lets callers keep
/// a portable CPU-crop fallback without teaching the common crate about D3D11 resources.
pub fn materialize_native_region(
    native_frame: &dyn NativeFrame,
    selection: Rect,
) -> Result<Option<GpuMaterialization>, CaptureError> {
    let Some(frame) = native_frame.as_any().downcast_ref::<DxgiGpuFrame>() else {
        return Ok(None);
    };
    frame.materialize_region(selection).map(Some)
}

impl DxgiGpuFrame {
    fn materialize_region(&self, selection: Rect) -> Result<GpuMaterialization, CaptureError> {
        let started = Instant::now();
        let source = self.metadata.source_rect;
        let local = local_selection(source, selection)?;
        if self.desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(capture_error(
                CaptureErrorKind::Unsupported,
                "gpu_region_readback",
                describe_unsupported_format(self.desc.Format),
                false,
                None,
            ));
        }
        let raw_local = raw_selection_for_rotation(
            local,
            self.desc.Width,
            self.desc.Height,
            self.metadata.rotation_degrees,
        )?;
        let destination_desc = staging_desc(
            raw_local.width,
            raw_local.height,
            self.desc.Format,
            self.desc.SampleDesc,
        );
        let staging = StagingTexture::create(&self.device, destination_desc)?.texture;
        let local_x = u32::try_from(raw_local.x).map_err(|_| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "raw selected region has a negative x coordinate",
                false,
                None,
            )
        })?;
        let local_y = u32::try_from(raw_local.y).map_err(|_| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "raw selected region has a negative y coordinate",
                false,
                None,
            )
        })?;
        let source_box = D3D11_BOX {
            left: local_x,
            top: local_y,
            front: 0,
            right: local_x.saturating_add(raw_local.width),
            bottom: local_y.saturating_add(raw_local.height),
            back: 1,
        };
        let context = lock_context(&self.context)?;
        let copy_started = Instant::now();
        // SAFETY: The raw source box was transformed and checked against the immutable snapshot;
        // the staging texture exactly matches its dimensions and format. Context access is serialized.
        unsafe {
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &self.texture, 0, Some(&source_box))
        };
        let gpu_copy_submit_ns = duration_ns_u64(copy_started.elapsed());
        let map_started = Instant::now();
        let mapped = MappedTexture::map(&context, &staging)?;
        let map_wait_ns = duration_ns_u64(map_started.elapsed());
        let raw_tight_stride = raw_local.width.checked_mul(4).ok_or_else(|| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "raw selected frame stride overflowed",
                false,
                None,
            )
        })?;
        if mapped.data.RowPitch < raw_tight_stride {
            return Err(capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "mapped row pitch is smaller than the raw selected frame stride",
                false,
                None,
            ));
        }
        let layout = normalized_layout(
            raw_local.width,
            raw_local.height,
            self.metadata.rotation_degrees,
        )?;
        if layout.width != selection.width || layout.height != selection.height {
            return Err(capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "rotated GPU region dimensions do not match the normalized selection",
                false,
                None,
            ));
        }
        let bytes_read = frame_byte_len(layout.width, layout.height)?;
        let full_frame_bytes = frame_byte_len(self.desc.Width, self.desc.Height)?;
        let mut pixels = vec![0_u8; bytes_read];
        let cpu_copy_started = Instant::now();
        let source_stride = mapped.data.RowPitch as usize;
        let source_len = source_stride
            .checked_mul(raw_local.height as usize)
            .ok_or_else(|| {
                capture_error(
                    CaptureErrorKind::InvalidFrame,
                    "gpu_region_readback",
                    "mapped raw region size overflowed",
                    false,
                    None,
                )
            })?;
        let contiguous_rows =
            self.metadata.rotation_degrees == 0 && source_stride == raw_tight_stride as usize;
        // SAFETY: Map returned a non-null pointer covering RowPitch bytes for every copied raw row;
        // the slice does not outlive the mapped guard.
        let source_pixels =
            unsafe { std::slice::from_raw_parts(mapped.data.pData.cast::<u8>(), source_len) };
        normalize_bgra_into(
            source_pixels,
            raw_local.width,
            raw_local.height,
            source_stride,
            self.metadata.rotation_degrees,
            &mut pixels,
        )?;
        let cpu_copy_ns = duration_ns_u64(cpu_copy_started.elapsed());
        drop(mapped);
        drop(context);
        let mut metadata = self.metadata.clone();
        metadata.source_rect = selection;
        metadata.copy_count = metadata.copy_count.saturating_add(2);
        metadata.pool_slot = None;
        let frame = CpuFrame::new(
            Arc::from(pixels),
            layout.width,
            layout.height,
            layout.stride,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .map_err(|error| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                error.to_string(),
                false,
                None,
            )
        })?;
        Ok(GpuMaterialization {
            frame,
            gpu_copy_submit_ns,
            map_wait_ns,
            cpu_copy_ns,
            total_ns: duration_ns_u64(started.elapsed()),
            bytes_read,
            full_frame_bytes,
            bytes_avoided: full_frame_bytes.saturating_sub(bytes_read),
            contiguous_rows,
        })
    }
}

fn local_selection(source: Rect, selection: Rect) -> Result<Rect, CaptureError> {
    if selection.width == 0 || selection.height == 0 {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "gpu_region_readback",
            "selected region must be non-empty",
            false,
            None,
        ));
    }
    let source_right = i64::from(source.x) + i64::from(source.width);
    let source_bottom = i64::from(source.y) + i64::from(source.height);
    let selection_right = i64::from(selection.x) + i64::from(selection.width);
    let selection_bottom = i64::from(selection.y) + i64::from(selection.height);
    if selection.x < source.x
        || selection.y < source.y
        || selection_right > source_right
        || selection_bottom > source_bottom
    {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "gpu_region_readback",
            "selected region lies outside the native frame",
            false,
            None,
        ));
    }
    Ok(Rect {
        x: selection.x - source.x,
        y: selection.y - source.y,
        width: selection.width,
        height: selection.height,
    })
}

fn raw_selection_for_rotation(
    normalized: Rect,
    raw_width: u32,
    raw_height: u32,
    rotation_degrees: u16,
) -> Result<Rect, CaptureError> {
    let layout = normalized_layout(raw_width, raw_height, rotation_degrees)?;
    let x = u32::try_from(normalized.x).map_err(|_| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "gpu_region_readback",
            "normalized region has a negative x coordinate",
            false,
            None,
        )
    })?;
    let y = u32::try_from(normalized.y).map_err(|_| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "gpu_region_readback",
            "normalized region has a negative y coordinate",
            false,
            None,
        )
    })?;
    let right = x.checked_add(normalized.width).ok_or_else(|| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "gpu_region_readback",
            "normalized region right edge overflowed",
            false,
            None,
        )
    })?;
    let bottom = y.checked_add(normalized.height).ok_or_else(|| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "gpu_region_readback",
            "normalized region bottom edge overflowed",
            false,
            None,
        )
    })?;
    if normalized.width == 0
        || normalized.height == 0
        || right > layout.width
        || bottom > layout.height
    {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "gpu_region_readback",
            "normalized region lies outside the rotated DXGI texture",
            false,
            None,
        ));
    }
    let (raw_x, raw_y, width, height) = match rotation_degrees {
        0 => (x, y, normalized.width, normalized.height),
        90 => (y, raw_height - right, normalized.height, normalized.width),
        180 => (
            raw_width - right,
            raw_height - bottom,
            normalized.width,
            normalized.height,
        ),
        270 => (raw_width - bottom, x, normalized.height, normalized.width),
        _ => unreachable!("rotation was validated above"),
    };
    Ok(Rect {
        x: i32::try_from(raw_x).map_err(|_| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "raw region x coordinate exceeds Win32 limits",
                false,
                None,
            )
        })?,
        y: i32::try_from(raw_y).map_err(|_| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "raw region y coordinate exceeds Win32 limits",
                false,
                None,
            )
        })?,
        width,
        height,
    })
}
fn lock_context(
    context: &Arc<Mutex<ID3D11DeviceContext>>,
) -> Result<std::sync::MutexGuard<'_, ID3D11DeviceContext>, CaptureError> {
    context.lock().map_err(|_| {
        capture_error(
            CaptureErrorKind::NativeFailure,
            "lock_d3d11_context",
            "D3D11 context lock was poisoned",
            false,
            None,
        )
    })
}

#[derive(Clone, Copy)]
struct RetainedFrame {
    presentation_qpc: i64,
    generation: u64,
    /// Where the pointer was when this frame was captured.
    ///
    /// Retained with the frame rather than read fresh at materialization time, because `latest`
    /// mode may hand back a frame acquired seconds ago and the mouse has moved since. Drawing the
    /// cursor where it is now would place it over pixels that never had it there.
    pointer_at: Option<(i32, i32)>,
}

struct RetainedTexture {
    texture: ID3D11Texture2D,
    desc: D3D11_TEXTURE2D_DESC,
}

impl RetainedTexture {
    fn create(device: &ID3D11Device, desc: D3D11_TEXTURE2D_DESC) -> Result<Self, CaptureError> {
        let mut texture = None;
        // SAFETY: desc is fully initialized and the output slot is valid. The texture is
        // populated by CopyResource before it is exposed as the latest frame.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|error| map_windows_error("create_retained_texture", error))?;
        let texture = texture.ok_or_else(|| {
            capture_error(
                CaptureErrorKind::NativeFailure,
                "create_retained_texture",
                "D3D11 returned no retained texture",
                false,
                None,
            )
        })?;
        Ok(Self { texture, desc })
    }
}

struct StagingTexture {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
}

impl StagingTexture {
    fn create(device: &ID3D11Device, desc: D3D11_TEXTURE2D_DESC) -> Result<Self, CaptureError> {
        let mut texture = None;
        // SAFETY: desc is fully initialized and the output slot is valid. No initial data is
        // required for a staging texture populated through CopyResource.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|error| map_windows_error("create_staging_texture", error))?;
        let texture = texture.ok_or_else(|| {
            capture_error(
                CaptureErrorKind::NativeFailure,
                "create_staging_texture",
                "D3D11 returned no staging texture",
                false,
                None,
            )
        })?;
        Ok(Self {
            texture,
            width: desc.Width,
            height: desc.Height,
            format: desc.Format,
        })
    }
}

struct CpuBufferPool {
    slots: Vec<Option<Arc<[u8]>>>,
    cursor: usize,
}

impl CpuBufferPool {
    fn new(slot_count: usize) -> Self {
        let slots = vec![None; slot_count];
        Self { slots, cursor: 0 }
    }

    fn available_index(&mut self, required_len: usize) -> Option<usize> {
        if let Some(index) = self.index_from_cursor(|slot| {
            slot.as_ref()
                .is_some_and(|slot| Arc::strong_count(slot) == 1 && slot.len() == required_len)
        }) {
            self.advance_cursor(index);
            return Some(index);
        }

        if let Some(index) = self.index_from_cursor(Option::is_none) {
            self.slots[index] = Some(Arc::from(vec![0_u8; required_len]));
            self.advance_cursor(index);
            return Some(index);
        }

        let index = self.index_from_cursor(|slot| {
            slot.as_ref()
                .is_some_and(|slot| Arc::strong_count(slot) == 1)
        })?;
        self.slots[index] = Some(Arc::from(vec![0_u8; required_len]));
        self.advance_cursor(index);
        Some(index)
    }

    fn index_from_cursor(
        &self,
        mut predicate: impl FnMut(&Option<Arc<[u8]>>) -> bool,
    ) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        (0..self.slots.len())
            .map(|offset| (self.cursor + offset) % self.slots.len())
            .find(|&index| predicate(&self.slots[index]))
    }

    fn advance_cursor(&mut self, index: usize) {
        self.cursor = (index + 1) % self.slots.len();
    }
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
    ) -> Result<Self, CaptureError> {
        let started = Instant::now();
        // Flush only submits the preceding copy; it does not wait for GPU completion. Map uses
        // DO_NOT_WAIT below so a stalled device or driver can never pin the capture worker.
        // SAFETY: context is the serialized immediate context that issued the staging copy.
        unsafe { context.Flush() };
        let data = loop {
            let mut data = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: texture is a live staging resource with CPU read access, data is a valid
            // output, and a successful mapped lifetime is bounded by this guard's matching Unmap.
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
                        && started.elapsed() < GPU_MAP_TIMEOUT =>
                {
                    std::thread::sleep(GPU_MAP_RETRY_DELAY);
                }
                Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                    return Err(capture_error(
                        CaptureErrorKind::Timeout,
                        "map_staging_texture",
                        format!(
                            "GPU readback did not complete within {} ms",
                            GPU_MAP_TIMEOUT.as_millis()
                        ),
                        true,
                        Some(i64::from(error.code().0)),
                    ));
                }
                Err(error) => {
                    return Err(map_windows_error("map_staging_texture", error));
                }
            }
        };
        if data.pData.is_null() {
            // SAFETY: Map succeeded and must be balanced even though it returned an invalid pointer.
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
        // SAFETY: This guard represents one successful Map call for subresource zero.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

fn staging_desc(
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    sample_desc: DXGI_SAMPLE_DESC,
) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: sample_desc,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    }
}

fn retained_desc(
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    sample_desc: DXGI_SAMPLE_DESC,
) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: sample_desc,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: 0,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

fn frame_byte_len(width: u32, height: u32) -> Result<usize, CaptureError> {
    width
        .checked_mul(4)
        .and_then(|stride| stride.checked_mul(height))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "buffer_pool",
                "CPU frame size overflowed",
                false,
                None,
            )
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PixelLayout {
    width: u32,
    height: u32,
    stride: u32,
}

fn dimensions_after_rotation(width: u32, height: u32, rotation_degrees: u16) -> (u32, u32) {
    if matches!(rotation_degrees, 90 | 270) {
        (height, width)
    } else {
        (width, height)
    }
}

fn normalized_layout(
    raw_width: u32,
    raw_height: u32,
    rotation_degrees: u16,
) -> Result<PixelLayout, CaptureError> {
    if raw_width == 0 || raw_height == 0 {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "normalize_rotation",
            "raw DXGI frame dimensions must be non-zero",
            false,
            None,
        ));
    }
    if !matches!(rotation_degrees, 0 | 90 | 180 | 270) {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "normalize_rotation",
            format!("unsupported display rotation {rotation_degrees}"),
            false,
            None,
        ));
    }
    let (width, height) = dimensions_after_rotation(raw_width, raw_height, rotation_degrees);
    let stride = width.checked_mul(4).ok_or_else(|| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "normalize_rotation",
            "normalized CPU frame stride overflowed",
            false,
            None,
        )
    })?;
    Ok(PixelLayout {
        width,
        height,
        stride,
    })
}

fn normalize_bgra_into(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    source_stride: usize,
    rotation_degrees: u16,
    destination: &mut [u8],
) -> Result<PixelLayout, CaptureError> {
    let layout = normalized_layout(source_width, source_height, rotation_degrees)?;
    let source_row_bytes = usize::try_from(source_width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "normalize_rotation",
                "raw DXGI row size overflowed",
                false,
                None,
            )
        })?;
    if source_stride < source_row_bytes {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "normalize_rotation",
            "raw DXGI stride is smaller than one pixel row",
            false,
            None,
        ));
    }
    let required_source = source_stride
        .checked_mul(source_height as usize)
        .ok_or_else(|| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "normalize_rotation",
                "mapped DXGI source size overflowed",
                false,
                None,
            )
        })?;
    if source.len() < required_source {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "normalize_rotation",
            "mapped DXGI source is shorter than its declared layout",
            false,
            None,
        ));
    }
    let required_destination = frame_byte_len(layout.width, layout.height)?;
    if destination.len() != required_destination {
        return Err(capture_error(
            CaptureErrorKind::InvalidFrame,
            "normalize_rotation",
            "normalized destination does not match its declared layout",
            false,
            None,
        ));
    }

    let output_stride = layout.stride as usize;
    if rotation_degrees == 0 {
        for row in 0..source_height as usize {
            let source_start = row * source_stride;
            let destination_start = row * output_stride;
            destination[destination_start..destination_start + output_stride]
                .copy_from_slice(&source[source_start..source_start + source_row_bytes]);
        }
        return Ok(layout);
    }

    let raw_width = source_width as usize;
    let raw_height = source_height as usize;
    for destination_y in 0..layout.height as usize {
        for destination_x in 0..layout.width as usize {
            let (source_x, source_y) = match rotation_degrees {
                90 => (destination_y, raw_height - 1 - destination_x),
                180 => (
                    raw_width - 1 - destination_x,
                    raw_height - 1 - destination_y,
                ),
                270 => (raw_width - 1 - destination_y, destination_x),
                _ => unreachable!("rotation was validated above"),
            };
            let source_start = source_y * source_stride + source_x * 4;
            let destination_start = destination_y * output_stride + destination_x * 4;
            destination[destination_start..destination_start + 4]
                .copy_from_slice(&source[source_start..source_start + 4]);
        }
    }
    Ok(layout)
}
struct OutputRecord {
    adapter: IDXGIAdapter1,
    adapter_luid: i64,
    output: IDXGIOutput1,
    info: DisplayInfo,
}

fn select_display_index(displays: &[DisplayInfo], display_id: &DisplayId) -> Option<usize> {
    if display_id.is_primary_alias() {
        displays
            .iter()
            .position(|display| display.is_primary)
            .or_else(|| (!displays.is_empty()).then_some(0))
    } else {
        displays
            .iter()
            .position(|display| display.id == *display_id)
    }
}

#[derive(Clone, Debug)]
struct DisplayConfigIdentity {
    gdi_name: String,
    persistent_id: DisplayId,
    friendly_name: String,
}

/// Test-only: reports no attached outputs for the first N milliseconds of the process.
///
/// `CAPTASTIC_TEST_NO_DISPLAYS_MS` exists because the condition the daemon's wait-and-recover path
/// is built around — a machine with nothing attached — cannot be produced on a working desk
/// without unplugging the monitor someone is reading. Expressed as a duration rather than a flag
/// so one run proves both halves: the daemon waits while it is set, and picks the displays up by
/// itself when it lapses.
///
/// Absent from release builds entirely, so a shipped binary has no such switch to find.
#[cfg(debug_assertions)]
fn displays_are_hidden_for_testing() -> bool {
    use std::sync::OnceLock;
    static BLACKOUT: OnceLock<Option<(Instant, Duration)>> = OnceLock::new();
    let configured = BLACKOUT.get_or_init(|| {
        let milliseconds = std::env::var("CAPTASTIC_TEST_NO_DISPLAYS_MS")
            .ok()?
            .parse::<u64>()
            .ok()?;
        log::warn!(
            "CAPTASTIC_TEST_NO_DISPLAYS_MS is set: reporting no attached displays for {milliseconds} ms"
        );
        Some((Instant::now(), Duration::from_millis(milliseconds)))
    });
    configured.is_some_and(|(started, window)| started.elapsed() < window)
}

#[cfg(not(debug_assertions))]
const fn displays_are_hidden_for_testing() -> bool {
    false
}

fn enumerate_outputs() -> Result<Vec<OutputRecord>, CaptureError> {
    if displays_are_hidden_for_testing() {
        return Ok(Vec::new());
    }
    // SAFETY: The generic result is a supported DXGI factory interface and Windows initializes it.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| map_windows_error("create_dxgi_factory", error))?;
    let identities = match display_config_identities() {
        Ok(identities) => identities,
        Err(error) => {
            log::warn!("{}", display_identity_fallback_log(&error));
            Vec::new()
        }
    };
    let mut records = Vec::new();
    let mut adapter_index = 0_u32;
    loop {
        // SAFETY: adapter_index is an enumeration index; NOT_FOUND terminates enumeration.
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(map_windows_error("enumerate_adapters", error)),
        };
        let mut adapter_desc = DXGI_ADAPTER_DESC1::default();
        // SAFETY: adapter_desc is valid writable storage for the live adapter.
        unsafe { adapter.GetDesc1(&mut adapter_desc) }
            .map_err(|error| map_windows_error("adapter_desc", error))?;
        let mut output_index = 0_u32;
        loop {
            // SAFETY: output_index is an enumeration index; NOT_FOUND terminates this adapter.
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(map_windows_error("enumerate_outputs", error)),
            };
            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|error| map_windows_error("cast_output", error))?;
            let mut desc = DXGI_OUTPUT_DESC::default();
            // SAFETY: desc is valid writable storage for the live output.
            unsafe { output.GetDesc(&mut desc) }
                .map_err(|error| map_windows_error("output_desc", error))?;
            if desc.AttachedToDesktop.as_bool() {
                let bounds = rect_from_windows(desc.DesktopCoordinates)?;
                let gdi_name = wide_array_to_string(&desc.DeviceName);
                let scale_factor = effective_monitor_scale(desc.Monitor, &gdi_name);
                let identity = identities
                    .iter()
                    .find(|identity| identity.gdi_name.eq_ignore_ascii_case(&gdi_name));
                let id = identity
                    .map(|identity| identity.persistent_id.clone())
                    .unwrap_or_else(|| persistent_display_id(&gdi_name));
                let name = identity
                    .filter(|identity| !identity.friendly_name.is_empty())
                    .map(|identity| identity.friendly_name.clone())
                    .unwrap_or(gdi_name);
                records.push(OutputRecord {
                    adapter: adapter.clone(),
                    adapter_luid: luid_to_i64(adapter_desc.AdapterLuid),
                    output: output1,
                    info: DisplayInfo {
                        id,
                        name,
                        bounds,
                        scale_factor,
                        rotation_degrees: rotation_degrees(desc.Rotation),
                        is_primary: desc.DesktopCoordinates.left == 0
                            && desc.DesktopCoordinates.top == 0,
                    },
                });
            }
            output_index = output_index.saturating_add(1);
        }
        adapter_index = adapter_index.saturating_add(1);
    }
    Ok(records)
}

fn luid_to_i64(luid: windows::Win32::Foundation::LUID) -> i64 {
    (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart)
}

fn effective_monitor_scale(monitor: HMONITOR, display_name: &str) -> f32 {
    let mut dpi_x = BASE_DPI;
    let mut dpi_y = BASE_DPI;
    // SAFETY: dpi_x and dpi_y are writable values and monitor comes from a live DXGI output.
    let dpi_result =
        unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if let Err(error) = dpi_result {
        log::warn!(
            "effective DPI query failed display={display_name:?}: {error}; reporting 100% scaling"
        );
        return 1.0;
    }
    if dpi_x != dpi_y {
        log::debug!(
            "display {display_name:?} reports asymmetric DPI x={dpi_x} y={dpi_y}; using x-axis DPI"
        );
    }
    dpi_x as f32 / BASE_DPI as f32
}

/// What the log says when the display-identity query fails and enumeration carries on without it.
///
/// This failure does not fail a capture. `enumerate_outputs` degrades to the names DXGI hands out
/// by itself, so this one line is the whole of what anybody ever sees of it — which is why the
/// wording is the fix. Until 2026-08-20 a locked or disconnected session produced exactly
/// `NativeFailure in dxgi/query_display_config: Access is denied.` here, the line quoted in issue
/// #51, describing a permissions problem the user did not have.
///
/// The prefix is left alone so a search that already finds this line keeps finding it. When the
/// session accounts for the failure the line also says what the fallback costs and when it ends,
/// because the fallback identity is derived from the GDI device name rather than from the panel:
/// a display named by that branch does not match what `state.toml` remembered under its persistent
/// id, and a `display =` naming a persistent id does not select it either.
fn display_identity_fallback_log(error: &CaptureError) -> String {
    let mut line = format!(
        "persistent display identity query failed; using session-local output names: {error}"
    );
    if error.kind == CaptureErrorKind::DesktopUnavailable {
        line.push_str(
            "; this clears when the session comes back, and until it does displays are identified \
             by their session-local device names rather than by their panels",
        );
    }
    line
}

/// The operation a failed display-identity query is reported under, in the log and in these tests.
const DISPLAY_CONFIG_QUERY: &str = "query_display_config";

/// The operation the sizing call that precedes it is reported under.
///
/// The same query, one call earlier, and denied by the same sessions: Windows documents
/// `ERROR_ACCESS_DENIED` from `GetDisplayConfigBufferSizes` for a caller that does not have access
/// to the current desktop. Whichever of the two the session refuses first produces the log line, so
/// explaining only the second one would leave the bare denial reachable under another name.
const DISPLAY_CONFIG_SIZES: &str = "display_config_buffer_sizes";

/// Explains a failed display-identity query, asking the session about it only when it was denied.
///
/// A locked or disconnected session refuses this call, and the refusal used to arrive through the
/// generic `map_windows_error` as `NativeFailure in dxgi/query_display_config: Access is denied.` —
/// the literal line issue #51 was filed about, naming a permissions problem in a session whose only
/// problem was that nobody was signed in to it.
///
/// The mapping is done here rather than in `map_windows_error`, which serves every DXGI operation
/// and has no business asking the session about a failed texture map.
/// [`crate::session::denied_by_session`] holds both halves of the discipline: the session is asked
/// only on `E_ACCESSDENIED`, so enumeration — which runs this query every time it walks the
/// adapters — never pays the four syscalls behind the probe on a working machine, and a denial the
/// session cannot account for keeps its original kind, message and native code. An unlocked console
/// session that is genuinely refused this query has something wrong with its rights, and reporting a
/// lock instead would be the same defect pointing the other way. The probe is taken as a closure so
/// the cost can be shown by a counter rather than asserted.
fn display_config_query_error(
    operation: &'static str,
    error: WindowsError,
    probe_session: impl FnOnce() -> crate::session::DesktopState,
) -> CaptureError {
    crate::session::denied_by_session(DXGI_BACKEND, operation, error.code().0, probe_session)
        .unwrap_or_else(|| map_windows_error(operation, error))
}

fn display_config_identities() -> Result<Vec<DisplayConfigIdentity>, CaptureError> {
    const MAX_ATTEMPTS: usize = 3;
    for attempt in 0..MAX_ATTEMPTS {
        let mut path_count = 0_u32;
        let mut mode_count = 0_u32;
        // SAFETY: Both counts are valid writable values and the query flag requests active paths.
        unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        }
        .map_err(|error| {
            display_config_query_error(DISPLAY_CONFIG_SIZES, error, crate::session::desktop_state)
        })?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: The arrays have the capacities reported immediately above. The mutable counts
        // describe their current lengths and are updated by Windows to the number of valid items.
        let result = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        match result {
            Ok(()) => {
                paths.truncate(path_count as usize);
                let mut identities = Vec::with_capacity(paths.len());
                for path in paths {
                    let source_name = display_config_source_name(&path)?;
                    let target_name = display_config_target_name(&path)?;
                    let friendly_name =
                        wide_array_to_string(&target_name.monitorFriendlyDeviceName);
                    identities.push(DisplayConfigIdentity {
                        persistent_id: display_identity(&target_name, &source_name),
                        gdi_name: source_name,
                        friendly_name,
                    });
                }
                return Ok(identities);
            }
            Err(error) if attempt + 1 < MAX_ATTEMPTS => {
                log::debug!(
                    "display identity query raced or failed transiently; retrying: {error}"
                );
            }
            Err(error) => {
                return Err(display_config_query_error(
                    DISPLAY_CONFIG_QUERY,
                    error,
                    crate::session::desktop_state,
                ))
            }
        }
    }
    unreachable!("display identity query loop always returns")
}

fn display_config_source_name(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String, CaptureError> {
    let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: The packet header identifies the concrete structure and its exact initialized size.
    let result = unsafe { DisplayConfigGetDeviceInfo(&mut name.header) };
    if result != 0 {
        return Err(display_config_error("display_config_source_name", result));
    }
    Ok(wide_array_to_string(&name.viewGdiDeviceName))
}

fn display_config_target_name(
    path: &DISPLAYCONFIG_PATH_INFO,
) -> Result<DISPLAYCONFIG_TARGET_DEVICE_NAME, CaptureError> {
    let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: The packet header identifies the concrete structure and its exact initialized size.
    let result = unsafe { DisplayConfigGetDeviceInfo(&mut name.header) };
    if result != 0 {
        return Err(display_config_error("display_config_target_name", result));
    }
    Ok(name)
}

fn display_config_error(operation: &'static str, result: i32) -> CaptureError {
    capture_error(
        CaptureErrorKind::NativeFailure,
        operation,
        format!("Windows display configuration query failed with status {result}"),
        true,
        Some(i64::from(result)),
    )
}

/// Names a display by what the panel says it is, rather than by how it happens to be plugged in.
///
/// The identity is a persistence key: `state.toml` records the last tool and the last region per
/// display under it, so a key that moves loses the user everything they had built up on that
/// screen and leaves an orphan table behind. It moved. A single-monitor machine accumulated two
/// entries, and the same panel reported two different ids four hours apart (issue #60).
///
/// The cause was the material. A monitor device path names the connection, not the panel: it
/// carries a parent bus instance and a UID, both assigned per *connection*. EDID carries what the
/// panel is: a manufacturer, a product code, and — with the connector it is attached to — enough
/// to separate two identical monitors on different ports.
///
/// Three sources, tried in order, and the id says which one answered so a support question does
/// not need this function to be read:
///
/// | id | material | stability |
/// | --- | --- | --- |
/// | `windows-monitor-DEL41B4-dp1` | EDID plus connector | survives power cycles and re-enumeration |
/// | `windows-monitor-path-<hash>` | monitor device path | survives a reboot, not a reconnection |
/// | `windows-monitor-session-DISPLAY1` | GDI device name | session-local; changes freely |
///
/// Changing the scheme resets every remembered display once, which is the cost of the key no
/// longer moving afterwards. Old entries do not match anything and are inert, which is exactly
/// what an entry for a monitor that is not attached has always been.
fn display_identity(target: &DISPLAYCONFIG_TARGET_DEVICE_NAME, source_name: &str) -> DisplayId {
    if let Some(edid) = edid_identity(target) {
        return DisplayId(format!("windows-monitor-{edid}"));
    }
    // No EDID is not a rare edge. Windows binds the active path to a generic `Default_Monitor`
    // device when it re-enumerates a display without reading EDID — a DisplayPort link event is
    // enough — and reports a blank manufacturer, a blank friendly name and an output technology of
    // -1 while it lasts. The real monitor's device node, EDID and serial can still be sitting in
    // the device tree beside it; the *active path* simply is not attached to them, so no amount of
    // care here can recover an identity Windows itself has lost. Logged at debug because it says
    // something true about the machine rather than about this capture.
    let monitor_path = wide_array_to_string(&target.monitorDevicePath);
    if !monitor_path.is_empty() {
        log::debug!(
            "display {source_name} reports no EDID; identifying it by device path, which changes when it reconnects"
        );
        let DisplayId(hashed) = persistent_display_id(&monitor_path);
        return DisplayId(hashed.replace("windows-monitor-", "windows-monitor-path-"));
    }
    log::debug!(
        "display {source_name} reports neither EDID nor a device path; identifying it by session-local name"
    );
    DisplayId(format!(
        "windows-monitor-session-{}",
        sanitize_identity_fragment(source_name)
    ))
}

/// Builds the EDID half of an identity, or nothing when the panel did not supply one.
///
/// A zero manufacturer id means no EDID was read — a virtual display, a KVM that does not pass
/// EDID through, or a driver that declined. Guessing from a product code alone would collide
/// across vendors, so the ladder falls through instead.
fn edid_identity(target: &DISPLAYCONFIG_TARGET_DEVICE_NAME) -> Option<String> {
    let manufacturer = edid_manufacturer(target.edidManufactureId)?;
    // The connector distinguishes two identical monitors, which EDID alone cannot: many panels
    // ship with a blank or duplicated serial, so the port is the more dependable tiebreaker.
    Some(format!(
        "{manufacturer}{:04X}-{}{}",
        target.edidProductCodeId,
        output_technology_tag(target.outputTechnology),
        target.connectorInstance
    ))
}

/// Decodes EDID's three packed five-bit letters into a manufacturer code such as `DEL`.
///
/// EDID stores the field big-endian while `DISPLAYCONFIG_TARGET_DEVICE_NAME` hands it over as a
/// native `u16`, so the bytes are swapped back before unpacking. Dell's `DEL` is `0x10AC`, which
/// is the value to reach for when checking this by hand.
fn edid_manufacturer(raw: u16) -> Option<String> {
    let packed = raw.swap_bytes();
    if packed == 0 {
        return None;
    }
    let letters = [(packed >> 10) & 0x1f, (packed >> 5) & 0x1f, packed & 0x1f];
    let mut code = String::with_capacity(3);
    for letter in letters {
        // 1 is 'A'; 0 and anything past 'Z' means this is not a manufacturer code.
        if !(1..=26).contains(&letter) {
            return None;
        }
        code.push(char::from(b'A' + (letter as u8) - 1));
    }
    Some(code)
}

/// A short, stable tag for the connector type, so an id reads as `dp1` rather than a number.
fn output_technology_tag(technology: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY) -> &'static str {
    match technology {
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL => "dp",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED => "edp",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI => "hdmi",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DVI => "dvi",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HD15 => "vga",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL => "internal",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EXTERNAL => "udi",
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED => "eudi",
        _ => "out",
    }
}

/// Reduces a device name to something safe to use as a TOML table key.
fn sanitize_identity_fragment(value: &str) -> String {
    let fragment: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect();
    if fragment.is_empty() {
        "unnamed".to_owned()
    } else {
        fragment
    }
}

fn persistent_display_id(identity: &str) -> DisplayId {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in identity.bytes().map(|byte| byte.to_ascii_lowercase()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    DisplayId(format!("windows-monitor-{hash:016x}"))
}

/// Explains a desktop format this backend cannot read, rather than naming its number.
///
/// The number alone was the whole message until 2026-08, and format 10 is the one a user actually
/// meets: it is what the compositor produces once HDR is switched on for any display, so a setting
/// change in Windows made every capture fail with an integer and no way to connect the two.
fn describe_unsupported_format(format: DXGI_FORMAT) -> String {
    let explanation = if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
        ". This is the format the compositor uses while HDR is enabled, and Captastic asked it for          8-bit BGRA and was refused. Turning HDR off for this display, or updating the display          driver, restores capture"
    } else {
        ""
    };
    format!(
        "the desktop is composed in DXGI format {} ({}), which Captastic cannot read{explanation}",
        format.0,
        format_name(format)
    )
}

/// A name for the handful of desktop formats worth naming; the number otherwise.
fn format_name(format: DXGI_FORMAT) -> &'static str {
    if format == DXGI_FORMAT_B8G8R8A8_UNORM {
        "B8G8R8A8_UNORM"
    } else if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
        "R16G16B16A16_FLOAT"
    } else if format == DXGI_FORMAT_R10G10B10A2_UNORM {
        "R10G10B10A2_UNORM"
    } else {
        "unrecognized"
    }
}

/// Duplicates an output, asking the compositor for 8-bit BGRA even when the desktop is not.
///
/// On an HDR desktop the composition surface is `R16G16B16A16_FLOAT` in scRGB, and plain
/// `DuplicateOutput` hands that back — a format nothing downstream can carry, since a PNG has no
/// way to say "these samples are linear and 1.0 is not the maximum" and the clipboard's DIBV5 has
/// no encoding for half-floats. Before this, every capture on an HDR desktop failed with an
/// unexplained format number.
///
/// `DuplicateOutput1` takes the formats the caller can accept, in order of preference, and the
/// compositor converts. Only BGRA8 is listed, deliberately: listing the float format as a fallback
/// would let the OS hand back pixels this backend has decided not to interpret, and the decision to
/// let Windows own the tone mapping is only coherent if there is no second path (ADR 0006).
///
/// The conversion is the compositor's own, the same one it performs for anything that reads the
/// desktop as SDR, which is what makes a Captastic screenshot of an HDR desktop look like every
/// other tool's screenshot of it rather than like Captastic's opinion of it.
/// Whether a retained frame fails the caller's staleness limit.
///
/// Currency, not age, is what a maximum age is asking about. A frame presented thirty seconds ago
/// that has just been proven pixel-identical to the screen is more current than one presented a
/// moment ago and never checked since; rejecting the first would refuse a frame on the strength of a
/// number describing something else (ADR 0003, amended 2026-08-17).
fn frame_is_too_stale(max_age_ms: Option<u64>, frame_age_ns: u64, verified_current: bool) -> bool {
    if verified_current {
        return false;
    }
    max_age_ms.is_some_and(|maximum| frame_age_ns > maximum.saturating_mul(1_000_000))
}

fn duplicate_output_as_bgra8(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
) -> Result<IDXGIOutputDuplication, CaptureError> {
    if let Ok(output5) = output.cast::<IDXGIOutput5>() {
        // SAFETY: The output and device belong to the same enumerated adapter and remain alive for
        // the call; the format slice is valid for its stated length and the flags must be zero.
        match unsafe { output5.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) } {
            Ok(duplication) => return Ok(duplication),
            Err(error) => {
                // Falls through rather than failing. An output that cannot deliver BGRA8 is still
                // capturable when the desktop is SDR, and the readback below reports precisely
                // what arrived if it is not.
                log::debug!(
                    "DuplicateOutput1 with BGRA8 was refused ({error}); falling back to the                      compositor's own format"
                );
            }
        }
    }
    // SAFETY: The output and device belong to the same enumerated adapter and remain alive.
    unsafe { output.DuplicateOutput(device) }.map_err(|error| {
        // "Access is denied" here is what a locked workstation looks like from DXGI, and it is
        // the message that sent this project looking for a permissions problem more than once.
        // The session is asked before the error is handed on, so it explains itself (issue #51).
        desktop_obstacle("duplicate_output")
            .unwrap_or_else(|| map_windows_error("duplicate_output", error))
    })
}

struct AcquiredFrame {
    duplication: IDXGIOutputDuplication,
    resource: IDXGIResource,
    info: DXGI_OUTDUPL_FRAME_INFO,
    released: bool,
}

impl AcquiredFrame {
    fn acquire(
        duplication: &IDXGIOutputDuplication,
        timeout_ms: u32,
    ) -> Result<Self, CaptureError> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        // SAFETY: info/resource are valid out-parameters and no prior frame is held by this backend.
        unsafe { duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) }
            .map_err(|error| map_windows_error("acquire_next_frame", error))?;
        let resource = match resource {
            Some(resource) => resource,
            None => {
                // SAFETY: AcquireNextFrame succeeded, so this balances the acquired frame.
                let _ = unsafe { duplication.ReleaseFrame() };
                return Err(capture_error(
                    CaptureErrorKind::InvalidFrame,
                    "acquire_next_frame",
                    "DXGI reported success without a desktop resource",
                    false,
                    None,
                ));
            }
        };
        Ok(Self {
            duplication: duplication.clone(),
            resource,
            info,
            released: false,
        })
    }

    fn release(mut self) -> Result<(), CaptureError> {
        self.released = true;
        // SAFETY: This object owns exactly one successful AcquireNextFrame operation.
        unsafe { self.duplication.ReleaseFrame() }
            .map_err(|error| map_windows_error("release_frame", error))
    }
}

impl Drop for AcquiredFrame {
    fn drop(&mut self) {
        if !self.released {
            // SAFETY: The guard owns an unreleased successful AcquireNextFrame operation.
            let _ = unsafe { self.duplication.ReleaseFrame() };
            self.released = true;
        }
    }
}

struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    fn initialize() -> Result<Self, CaptureError> {
        // SAFETY: Called once for this backend on its owning thread with a null reserved pointer.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .map_err(|error| map_windows_error("initialize_com", error))?;
        Ok(Self { initialized: true })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: Balances the successful CoInitializeEx call on the same owning thread.
            unsafe { CoUninitialize() };
        }
    }
}

fn rect_from_windows(rect: windows::Win32::Foundation::RECT) -> Result<Rect, CaptureError> {
    let width = rect.right.checked_sub(rect.left).ok_or_else(|| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "output_desc",
            "desktop rectangle width overflowed",
            false,
            None,
        )
    })?;
    let height = rect.bottom.checked_sub(rect.top).ok_or_else(|| {
        capture_error(
            CaptureErrorKind::InvalidFrame,
            "output_desc",
            "desktop rectangle height overflowed",
            false,
            None,
        )
    })?;
    Ok(Rect {
        x: rect.left,
        y: rect.top,
        width: u32::try_from(width).map_err(|_| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "output_desc",
                "desktop rectangle has negative width",
                false,
                None,
            )
        })?,
        height: u32::try_from(height).map_err(|_| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "output_desc",
                "desktop rectangle has negative height",
                false,
                None,
            )
        })?,
    })
}

fn rotation_degrees(rotation: DXGI_MODE_ROTATION) -> u16 {
    if rotation == DXGI_MODE_ROTATION_ROTATE90 {
        90
    } else if rotation == DXGI_MODE_ROTATION_ROTATE180 {
        180
    } else if rotation == DXGI_MODE_ROTATION_ROTATE270 {
        270
    } else {
        0
    }
}

fn wide_array_to_string(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn query_performance_counter() -> Result<i64, CaptureError> {
    let mut value = 0_i64;
    // SAFETY: value is valid writable storage.
    unsafe { QueryPerformanceCounter(&mut value) }
        .map_err(|error| map_windows_error("query_performance_counter", error))?;
    Ok(value)
}

fn query_performance_frequency() -> Result<i64, CaptureError> {
    let mut value = 0_i64;
    // SAFETY: value is valid writable storage.
    unsafe { QueryPerformanceFrequency(&mut value) }
        .map_err(|error| map_windows_error("query_performance_frequency", error))?;
    if value <= 0 {
        return Err(capture_error(
            CaptureErrorKind::NativeFailure,
            "query_performance_frequency",
            "Windows returned a non-positive performance-counter frequency",
            false,
            None,
        ));
    }
    Ok(value)
}

fn qpc_to_ns(ticks: i64, frequency: i64) -> i64 {
    let value = i128::from(ticks)
        .saturating_mul(1_000_000_000)
        .checked_div(i128::from(frequency))
        .unwrap_or(0);
    i64::try_from(value).unwrap_or_else(|_| {
        if value.is_negative() {
            i64::MIN
        } else {
            i64::MAX
        }
    })
}

fn duration_ns_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

fn duration_ns_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn duration_to_timeout_ms(duration: Duration) -> u32 {
    let milliseconds = duration.as_millis().max(1);
    u32::try_from(milliseconds).unwrap_or(u32::MAX)
}

fn map_windows_error(operation: &'static str, error: WindowsError) -> CaptureError {
    let code = error.code();
    let (kind, retryable) = if code == DXGI_ERROR_WAIT_TIMEOUT {
        (CaptureErrorKind::Timeout, true)
    } else if code == DXGI_ERROR_ACCESS_LOST {
        (CaptureErrorKind::AccessLost, true)
    } else if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
        (CaptureErrorKind::DeviceRemoved, true)
    } else {
        (CaptureErrorKind::NativeFailure, false)
    };
    capture_error(
        kind,
        operation,
        error.to_string(),
        retryable,
        Some(i64::from(code.0)),
    )
}

/// Classifies a `GetDeviceRemovedReason` failure as the loss it always is.
///
/// The reason code is not the code the rest of the backend sees: calls made after a TDR fail with
/// `DXGI_ERROR_DEVICE_REMOVED`, while the reason behind it is usually `DEVICE_HUNG`, `DEVICE_RESET`
/// or `DRIVER_INTERNAL_ERROR`. Running those through `map_windows_error` would demote two of them
/// to a non-retryable `NativeFailure` and skip backend recovery, so any non-success reason is
/// reported as `DeviceRemoved` with the reason preserved as the native code.
fn device_removed_error(operation: &'static str, reason: WindowsError) -> CaptureError {
    capture_error(
        CaptureErrorKind::DeviceRemoved,
        operation,
        format!("the D3D11 device was lost during capture: {reason}"),
        true,
        Some(i64::from(reason.code().0)),
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
        backend: "dxgi",
        operation,
        message: message.into(),
        retryable,
        native_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::HRESULT_ACCESS_DENIED;
    use captastic_core::{CaptureId, CaptureSource, CursorMode};
    use windows::core::HRESULT;

    /// The denial a locked or disconnected session gives this query, as Windows reports it.
    fn access_denied() -> WindowsError {
        WindowsError::from(HRESULT(HRESULT_ACCESS_DENIED))
    }

    /// A lock refuses the device the duplication is built on, and the report has to say so.
    ///
    /// This is the first call `DxgiBackend::new` makes after enumerating, and until now a refusal
    /// arrived as `NativeFailure in dxgi/create_d3d11_device: Access is denied.` — a kind the daemon
    /// reads as neither a desktop wait nor a recoverable engine, which under `display = primary`
    /// meant a daemon that exited at a lock screen rather than waiting at one.
    #[test]
    fn a_locked_session_explains_a_refused_d3d11_device() {
        let denied =
            device_creation_error(access_denied(), || crate::session::DesktopState::Locked {
                desktop: Some("Winlogon".to_owned()),
            });
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
        assert!(
            denied.message.contains("locked"),
            "the whole point is that the message says so: {denied}"
        );
        assert!(denied.message.contains("Winlogon"), "{denied}");
        assert_eq!(denied.backend, DXGI_BACKEND);
        assert_eq!(denied.operation, CREATE_DEVICE);
        assert!(
            denied.retryable,
            "a lock ends when a person comes back, so the daemon must be allowed to wait it out"
        );
    }

    /// Every other temporary session state explains it too, because every one of them refuses.
    ///
    /// An isolated sandbox showing up as a detached session, a secure desktop, and a Remote Desktop
    /// session composed onto a virtual adapter all refuse this device for reasons that are no more
    /// a permissions problem than a lock is, and all of them end the same way.
    #[test]
    fn any_session_that_owns_no_desktop_explains_a_refused_d3d11_device() {
        for state in [
            crate::session::DesktopState::Locked { desktop: None },
            crate::session::DesktopState::NotOurs {
                desktop: Some("Winlogon".to_owned()),
            },
            crate::session::DesktopState::Detached {
                connect_state: "disconnected",
            },
            crate::session::DesktopState::Remote {
                protocol: "Remote Desktop",
            },
        ] {
            let denied = device_creation_error(access_denied(), || state.clone());
            assert_eq!(
                denied.kind,
                CaptureErrorKind::DesktopUnavailable,
                "{state:?} should explain the refusal"
            );
            assert_eq!(denied.message, state.to_string());
            assert_eq!(denied.operation, CREATE_DEVICE);
        }
    }

    /// A refusal the session cannot account for stays exactly what it was.
    ///
    /// This is the half that makes the re-classification safe to make on a path that decides
    /// whether the daemon starts. An unlocked, attached, console session refused a D3D11 device has
    /// something genuinely wrong — the isolated sandbox in `docs/windows-backend.md` is precisely
    /// that case — and turning it into a desktop wait would replace a daemon that says why it
    /// failed with one that waits forever for a lock that was never on.
    #[test]
    fn an_unlocked_session_leaves_a_refused_d3d11_device_alone() {
        for state in [
            crate::session::DesktopState::Interactive,
            crate::session::DesktopState::Unknown {
                detail: "the input desktop could not be named".to_owned(),
            },
        ] {
            let denied = device_creation_error(access_denied(), || state.clone());
            assert_eq!(
                denied.kind,
                CaptureErrorKind::NativeFailure,
                "{state:?} does not explain a refusal and must not hide one"
            );
            assert_eq!(denied.message, access_denied().to_string());
            assert_eq!(denied.native_code, Some(i64::from(HRESULT_ACCESS_DENIED)));
            assert_eq!(denied.backend, DXGI_BACKEND);
            assert_eq!(denied.operation, CREATE_DEVICE);
        }
    }

    /// A device that was lost rather than refused keeps the classification recovery depends on.
    ///
    /// `DXGI_ERROR_DEVICE_REMOVED` from this call is a GPU reset, and the daemon rebuilds for it.
    /// Sending it to the session probe would cost four syscalls for an answer that cannot apply, and
    /// re-classifying it as a desktop wait would stop the rebuild that fixes it — so the gate is on
    /// the denial code, and the counter here is what proves the probe is never asked.
    #[test]
    fn only_a_refused_d3d11_device_pays_for_the_session_probe() {
        let probes = std::cell::Cell::new(0_u32);
        let probe = || {
            probes.set(probes.get() + 1);
            crate::session::DesktopState::Locked { desktop: None }
        };
        let removed = WindowsError::from(DXGI_ERROR_DEVICE_REMOVED);
        let lost = device_creation_error(removed.clone(), probe);
        assert_eq!(probes.get(), 0, "a non-denial must not ask the session");
        assert_eq!(lost.kind, CaptureErrorKind::DeviceRemoved);
        assert_eq!(lost.message, removed.to_string());
        assert_eq!(lost.operation, CREATE_DEVICE);
        assert!(lost.retryable);

        let denied = device_creation_error(access_denied(), || {
            probes.set(probes.get() + 1);
            crate::session::DesktopState::Locked { desktop: None }
        });
        assert_eq!(probes.get(), 1, "a denial asks the session exactly once");
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
    }

    /// The line issue #51 was filed about, and what the fix has to turn it into.
    ///
    /// `NativeFailure in dxgi/query_display_config: Access is denied.` is quoted verbatim in #51:
    /// a session with nobody signed in to it refuses the display-identity query, and the report
    /// named a permissions problem instead of the sign-in state that caused it. The operation stays
    /// `dxgi/query_display_config` because that is what a log reader greps for; only the
    /// explanation is new.
    #[test]
    fn a_locked_session_explains_a_denied_display_identity_query() {
        let denied = display_config_query_error(DISPLAY_CONFIG_QUERY, access_denied(), || {
            crate::session::DesktopState::Locked {
                desktop: Some("Winlogon".to_owned()),
            }
        });
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
        assert!(
            denied.message.contains("locked"),
            "the whole point is that the message says so: {denied}"
        );
        assert!(denied.message.contains("Winlogon"), "{denied}");
        assert_eq!(denied.backend, "dxgi");
        assert_eq!(denied.operation, DISPLAY_CONFIG_QUERY);
        assert!(denied.retryable);
    }

    /// Every other temporary session state explains it too, because every one of them refuses.
    ///
    /// A disconnected RDP session and a secure desktop deny this call for reasons no more a
    /// permissions problem than a lock is, and they clear the same way — by somebody coming back.
    #[test]
    fn any_session_that_owns_no_desktop_explains_a_denied_display_identity_query() {
        for state in [
            crate::session::DesktopState::Locked { desktop: None },
            crate::session::DesktopState::NotOurs {
                desktop: Some("Winlogon".to_owned()),
            },
            crate::session::DesktopState::Detached {
                connect_state: "disconnected",
            },
            crate::session::DesktopState::Remote {
                protocol: "Remote Desktop",
            },
        ] {
            let denied =
                display_config_query_error(DISPLAY_CONFIG_QUERY, access_denied(), || state.clone());
            assert_eq!(
                denied.kind,
                CaptureErrorKind::DesktopUnavailable,
                "{state:?} should explain the denial"
            );
            assert_eq!(denied.message, state.to_string());
            assert_eq!(denied.operation, DISPLAY_CONFIG_QUERY);
        }
    }

    /// The sizing call one line earlier is the same query and is explained the same way.
    ///
    /// `GetDisplayConfigBufferSizes` is documented as returning `ERROR_ACCESS_DENIED` to a caller
    /// without access to the current desktop, exactly as `QueryDisplayConfig` is, and it runs first.
    /// If only the second call were explained, a session that refused the pair would still produce a
    /// bare `Access is denied` — the same defect under a different operation name. Each keeps its
    /// own name, because they are two calls and a log reader should be able to tell which failed.
    #[test]
    fn the_sizing_call_before_the_query_is_explained_under_its_own_name() {
        let denied = display_config_query_error(DISPLAY_CONFIG_SIZES, access_denied(), || {
            crate::session::DesktopState::Detached {
                connect_state: "disconnected",
            }
        });
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
        assert_eq!(denied.operation, DISPLAY_CONFIG_SIZES);
        assert!(denied.message.contains("disconnected"), "{denied}");

        let unexplained = display_config_query_error(DISPLAY_CONFIG_SIZES, access_denied(), || {
            crate::session::DesktopState::Interactive
        });
        assert_eq!(unexplained.kind, CaptureErrorKind::NativeFailure);
        assert_eq!(unexplained.operation, DISPLAY_CONFIG_SIZES);
    }

    /// A denial the session cannot account for stays exactly what it was.
    ///
    /// This is the half that makes the re-classification safe. An unlocked, attached, console
    /// session that is refused this query has something genuinely wrong with its rights, and a
    /// comfortable message about a lock would trade one misleading line for another. `Unknown`
    /// counts as "cannot account for it": a probe that failed answered nothing, and nothing is not
    /// "locked".
    #[test]
    fn an_unlocked_session_leaves_a_denied_display_identity_query_alone() {
        for state in [
            crate::session::DesktopState::Interactive,
            crate::session::DesktopState::Unknown {
                detail: "the input desktop could not be named".to_owned(),
            },
        ] {
            let denied =
                display_config_query_error(DISPLAY_CONFIG_QUERY, access_denied(), || state.clone());
            assert_eq!(
                denied.kind,
                CaptureErrorKind::NativeFailure,
                "{state:?} does not explain a denial and must not hide one"
            );
            assert_eq!(denied.message, access_denied().to_string());
            assert_eq!(denied.native_code, Some(i64::from(HRESULT_ACCESS_DENIED)));
            assert_eq!(denied.backend, "dxgi");
            assert_eq!(denied.operation, DISPLAY_CONFIG_QUERY);
        }
    }

    /// The session probe costs four syscalls, and every enumeration runs this query. It may only be
    /// paid on the one failure it can explain.
    ///
    /// `duplicate_output` set that discipline — it asks the session inside `map_err` and nowhere
    /// else. Here the probe is a closure precisely so the cheap paths can be shown never to call
    /// it: a successful query does not reach this function at all, and a failure that is not
    /// `E_ACCESSDENIED` has nothing for the session to say about it, so it keeps the mapping
    /// `map_windows_error` has always given it.
    #[test]
    fn only_a_denied_display_identity_query_pays_for_the_session_probe() {
        let probes = std::cell::Cell::new(0_u32);
        let probe = || {
            probes.set(probes.get() + 1);
            crate::session::DesktopState::Locked { desktop: None }
        };
        let device_error = WindowsError::from(HRESULT(0x8007_001f_u32 as i32));
        let failed = display_config_query_error(DISPLAY_CONFIG_QUERY, device_error.clone(), probe);
        assert_eq!(probes.get(), 0, "a non-denial must not ask the session");
        assert_eq!(failed.kind, CaptureErrorKind::NativeFailure);
        assert_eq!(failed.message, device_error.to_string());
        assert_eq!(failed.native_code, Some(i64::from(0x8007_001f_u32 as i32)));
        assert_eq!(failed.operation, DISPLAY_CONFIG_QUERY);

        let denied = display_config_query_error(DISPLAY_CONFIG_QUERY, access_denied(), || {
            probes.set(probes.get() + 1);
            crate::session::DesktopState::Locked { desktop: None }
        });
        assert_eq!(probes.get(), 1, "a denial asks the session exactly once");
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
    }

    /// The log line is the entire user-visible artifact of this failure, so it is tested as one.
    ///
    /// `enumerate_outputs` does not fail when the identity query does — it warns and carries on
    /// with the names DXGI supplies itself. Nothing else about the capture changes, which is why
    /// the re-classification is only worth anything if it reaches this line.
    #[test]
    fn the_display_identity_fallback_log_names_the_session_that_denied_the_query() {
        let explained = display_identity_fallback_log(&display_config_query_error(
            DISPLAY_CONFIG_QUERY,
            access_denied(),
            || crate::session::DesktopState::Locked { desktop: None },
        ));
        assert!(
            explained.starts_with("persistent display identity query failed"),
            "the searchable prefix must not move: {explained}"
        );
        assert!(
            explained.contains("the workstation is locked"),
            "the log line is the fix; it has to say what happened: {explained}"
        );
        assert!(
            explained.contains("clears when the session comes back"),
            "a fallback that ends by itself should say so: {explained}"
        );

        // A failure the session cannot account for keeps the plain line, because there is nothing
        // extra that is true to say about it: nobody knows when, or whether, it clears.
        let unexplained = display_identity_fallback_log(&display_config_query_error(
            DISPLAY_CONFIG_QUERY,
            access_denied(),
            || crate::session::DesktopState::Interactive,
        ));
        assert!(unexplained.contains("NativeFailure in dxgi/query_display_config"));
        assert!(!unexplained.contains("clears when the session comes back"));
    }

    #[test]
    fn converts_qpc_ticks_without_losing_sign() {
        assert_eq!(qpc_to_ns(5, 10), 500_000_000);
        assert_eq!(qpc_to_ns(-5, 10), -500_000_000);
    }

    #[test]
    fn converts_wide_device_names() {
        assert_eq!(wide_array_to_string(&[65, 66, 0, 67]), "AB");
    }

    /// Builds a target-name packet with the EDID fields a real monitor supplies.
    fn edid_target(
        manufacturer: u16,
        product: u16,
        instance: u32,
    ) -> DISPLAYCONFIG_TARGET_DEVICE_NAME {
        DISPLAYCONFIG_TARGET_DEVICE_NAME {
            edidManufactureId: manufacturer,
            edidProductCodeId: product,
            connectorInstance: instance,
            outputTechnology: DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EXTERNAL,
            ..Default::default()
        }
    }

    #[test]
    fn an_edid_identity_names_the_panel_rather_than_the_cable() {
        // 0x10AC is Dell's EDID manufacturer code, stored byte-swapped in the display-config
        // packet. Checked against a real U2720Q, whose device node reads DISPLAY\DEL41B4.
        let target = edid_target(0x10AC_u16.swap_bytes(), 0x41B4, 0);
        assert_eq!(
            display_identity(&target, r"\.\DISPLAY1").0,
            "windows-monitor-DEL41B4-dp0"
        );

        // The point of the exercise: nothing in the id comes from the connection, so a monitor
        // that reconnects with a new bus instance and UID still resolves to the same key.
        let reconnected = edid_target(0x10AC_u16.swap_bytes(), 0x41B4, 0);
        assert_eq!(
            display_identity(&reconnected, r"\.\DISPLAY129").0,
            display_identity(&target, r"\.\DISPLAY1").0
        );
    }

    #[test]
    fn two_identical_panels_are_separated_by_the_connector() {
        // EDID alone cannot tell them apart - many panels ship with a blank or duplicated serial -
        // so the port is the tiebreaker. Inheriting the other screen's remembered region would be
        // worse than starting blank.
        let first = edid_target(0x10AC_u16.swap_bytes(), 0x41B4, 0);
        let second = edid_target(0x10AC_u16.swap_bytes(), 0x41B4, 1);
        assert_ne!(
            display_identity(&first, "a").0,
            display_identity(&second, "b").0
        );
    }

    #[test]
    fn a_display_with_no_edid_falls_through_and_says_so() {
        // Windows binds the active path to a generic Default_Monitor device when it re-enumerates
        // a display without reading EDID: blank manufacturer, blank friendly name, output
        // technology -1. Observed on the development host (issue #60). The identity cannot be
        // recovered, so the id names the material it did use instead of pretending.
        let mut generic = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            outputTechnology: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY(-1),
            ..Default::default()
        };
        let path = r"\?\DISPLAY#Default_Monitor#1&5771b5a&0&UID256#{e6f07b5f}";
        for (index, unit) in path.encode_utf16().enumerate() {
            generic.monitorDevicePath[index] = unit;
        }
        let id = display_identity(&generic, r"\.\DISPLAY129").0;
        assert!(id.starts_with("windows-monitor-path-"), "{id}");

        // And with nothing at all to go on, the id is visibly session-local rather than a hash
        // that would pass for permanent in a configuration file.
        let empty = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        assert_eq!(
            display_identity(&empty, r"\.\DISPLAY129").0,
            "windows-monitor-session-DISPLAY129"
        );
    }

    #[test]
    fn a_manufacturer_code_is_three_letters_or_nothing() {
        assert_eq!(
            edid_manufacturer(0x10AC_u16.swap_bytes()).as_deref(),
            Some("DEL")
        );
        // No EDID at all.
        assert_eq!(edid_manufacturer(0), None);
        // A packed value whose letters fall outside A-Z is not a manufacturer code, and rendering
        // it would put control characters into a file name and a TOML key.
        assert_eq!(edid_manufacturer(0xFFFF), None);
    }

    #[test]
    fn persistent_display_ids_are_case_insensitive_and_device_specific() {
        assert_eq!(
            persistent_display_id(r"\\?\DISPLAY#DEL40A9#first"),
            persistent_display_id(r"\\?\display#del40a9#FIRST")
        );
        assert_ne!(
            persistent_display_id(r"\\?\DISPLAY#DEL40A9#first"),
            persistent_display_id(r"\\?\DISPLAY#DEL40A9#second")
        );
        assert!(persistent_display_id("monitor")
            .0
            .starts_with("windows-monitor-"));
    }

    #[test]
    fn configured_display_selection_prefers_identity_over_enumeration_order() {
        let displays = [
            test_display("secondary", false),
            test_display("primary-id", true),
        ];
        assert_eq!(
            select_display_index(&displays, &DisplayId::primary()),
            Some(1)
        );
        assert_eq!(
            select_display_index(&displays, &DisplayId("secondary".to_owned())),
            Some(0)
        );
        assert_eq!(
            select_display_index(&displays, &DisplayId("missing".to_owned())),
            None
        );
    }

    fn test_display(id: &str, is_primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale_factor: 1.0,
            rotation_degrees: 0,
            is_primary,
        }
    }

    #[test]
    fn timeout_rounds_up_to_one_millisecond() {
        assert_eq!(duration_to_timeout_ms(Duration::from_micros(1)), 1);
    }

    fn initialized_cpu_slot_count(pool: &CpuBufferPool) -> usize {
        pool.slots.iter().flatten().count()
    }

    fn lease_cpu_slot(pool: &CpuBufferPool, index: usize) -> Arc<[u8]> {
        pool.slots[index]
            .as_ref()
            .expect("CPU slot is initialized")
            .clone()
    }

    #[test]
    fn cpu_pool_reuses_one_allocation_for_many_sequential_acquisitions() {
        let mut pool = CpuBufferPool::new(3);

        for _ in 0..100 {
            let index = pool.available_index(64).expect("available slot");
            let lease = lease_cpu_slot(&pool, index);
            drop(lease);
        }

        assert_eq!(initialized_cpu_slot_count(&pool), 1);
    }

    #[test]
    fn cpu_pool_does_not_reuse_a_leased_slot() {
        let mut pool = CpuBufferPool::new(3);
        let first = pool.available_index(16).expect("first slot");
        let lease = lease_cpu_slot(&pool, first);
        let second = pool.available_index(16).expect("second slot");
        assert_ne!(first, second);
        Arc::get_mut(
            pool.slots[second]
                .as_mut()
                .expect("second CPU slot is initialized"),
        )
        .expect("second CPU slot is not leased")
        .fill(0xa5);
        assert!(lease.iter().all(|&byte| byte == 0));
        assert_eq!(initialized_cpu_slot_count(&pool), 2);
        drop(lease);
    }

    #[test]
    fn cpu_pool_allocates_a_second_slot_for_an_overlapping_lease() {
        let mut pool = CpuBufferPool::new(3);
        let first = pool.available_index(16).expect("first slot");
        let first_lease = lease_cpu_slot(&pool, first);
        let second = pool.available_index(16).expect("second slot");
        let second_lease = lease_cpu_slot(&pool, second);
        assert_ne!(first, second);
        assert_eq!(initialized_cpu_slot_count(&pool), 2);
        drop((first_lease, second_lease));
    }

    #[test]
    fn cpu_pool_uses_all_three_slots_and_reports_exhaustion() {
        let mut pool = CpuBufferPool::new(3);
        let mut leases = Vec::new();
        for _ in 0..3 {
            let index = pool.available_index(16).expect("available slot");
            leases.push(lease_cpu_slot(&pool, index));
        }
        assert_eq!(initialized_cpu_slot_count(&pool), 3);
        assert_eq!(pool.available_index(16), None);
        drop(leases);
    }

    #[test]
    fn cpu_pool_prefers_a_released_allocation_over_an_unused_slot() {
        let mut pool = CpuBufferPool::new(3);
        let first = pool.available_index(16).expect("first slot");
        let first_lease = lease_cpu_slot(&pool, first);
        let second = pool.available_index(16).expect("second slot");
        let second_lease = lease_cpu_slot(&pool, second);
        drop(first_lease);
        assert_eq!(pool.available_index(16), Some(first));
        assert_eq!(initialized_cpu_slot_count(&pool), 2);
        drop(second_lease);
    }

    #[test]
    fn cpu_pool_size_change_does_not_reuse_an_incompatible_allocation() {
        let mut pool = CpuBufferPool::new(2);
        let first = pool.available_index(16).expect("first slot");
        let second = pool.available_index(32).expect("second slot");
        assert_ne!(first, second);
        assert_eq!(pool.slots[first].as_ref().map(|slot| slot.len()), Some(16));
        assert_eq!(pool.slots[second].as_ref().map(|slot| slot.len()), Some(32));
    }

    #[test]
    fn cpu_pool_replaces_a_free_incompatible_allocation_when_full() {
        let mut pool = CpuBufferPool::new(1);
        let index = pool.available_index(16).expect("first allocation");
        assert_eq!(pool.available_index(32), Some(index));
        assert_eq!(pool.slots[index].as_ref().map(|slot| slot.len()), Some(32));
    }

    #[test]
    fn cpu_pool_allocates_slots_only_when_requested() {
        let mut pool = CpuBufferPool::new(3);
        assert!(pool.slots.iter().all(Option::is_none));
        let first = pool.available_index(64).expect("first slot");
        assert_eq!(pool.slots[first].as_ref().map(|slot| slot.len()), Some(64));
        assert_eq!(initialized_cpu_slot_count(&pool), 1);
    }

    #[test]
    fn frame_size_overflow_is_rejected() {
        assert!(frame_byte_len(u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn every_device_removal_reason_triggers_recovery() {
        use windows::Win32::Graphics::Dxgi::{
            DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DRIVER_INTERNAL_ERROR,
        };

        for reason in [
            DXGI_ERROR_DEVICE_HUNG,
            DXGI_ERROR_DEVICE_RESET,
            DXGI_ERROR_DEVICE_REMOVED,
            DXGI_ERROR_DRIVER_INTERNAL_ERROR,
        ] {
            let error = device_removed_error("capture_fresh", WindowsError::from(reason));
            assert_eq!(error.kind, CaptureErrorKind::DeviceRemoved);
            assert_eq!(error.operation, "capture_fresh");
            assert!(error.retryable);
            assert_eq!(error.native_code, Some(i64::from(reason.0)));
        }
        // The generic mapping is not a substitute: it reads a hang as an ordinary native failure,
        // which callers do not recover from.
        assert_eq!(
            map_windows_error("capture_fresh", WindowsError::from(DXGI_ERROR_DEVICE_HUNG)).kind,
            CaptureErrorKind::NativeFailure
        );
    }

    /// Presses Ctrl+Win+Shift+B, the chord Windows binds to "restart the graphics driver".
    ///
    /// Injected only when the operator asks for it, because whether it works is unknown. The chord
    /// is handled inside win32k rather than through `RegisterHotKey`, `SendInput` stamps every
    /// event it delivers as `LLKHF_INJECTED`, and nothing in the documentation says what win32k
    /// does with an injected copy of this particular chord. The honest answer is that it can only
    /// be learned by sending it on a machine that can afford to have its screen blank, so the
    /// default path below waits for a human hand and this is the opt-in.
    ///
    /// The modifiers are released in reverse order after `B`, so the Windows key is consumed by
    /// the chord rather than left looking like a bare press, which would open the Start menu.
    fn send_display_driver_reset_chord() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
            KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_SHIFT,
        };

        const VK_B: VIRTUAL_KEY = VIRTUAL_KEY(0x42);

        fn key(code: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: code,
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        }

        let press = KEYBD_EVENT_FLAGS(0);
        let inputs = [
            key(VK_CONTROL, press),
            key(VK_LWIN, press),
            key(VK_SHIFT, press),
            key(VK_B, press),
            key(VK_B, KEYEVENTF_KEYUP),
            key(VK_SHIFT, KEYEVENTF_KEYUP),
            key(VK_LWIN, KEYEVENTF_KEYUP),
            key(VK_CONTROL, KEYEVENTF_KEYUP),
        ];
        let stride = i32::try_from(std::mem::size_of::<INPUT>()).expect("INPUT fits in an i32");
        // SAFETY: every element is a fully initialized keyboard event, and the stride describes
        // this build's own INPUT layout.
        let sent = unsafe { SendInput(&inputs, stride) };
        assert_eq!(
            sent as usize,
            inputs.len(),
            "the injected chord was blocked before it reached the desktop, so nothing below \
             could have measured a device loss this test caused"
        );
    }

    /// Names the DXGI removal reasons, because a bare HRESULT in a measurement log is a number the
    /// reader has to go and look up.
    fn removal_reason_name(code: i32) -> &'static str {
        use windows::Win32::Graphics::Dxgi::{
            DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_ERROR_INVALID_CALL,
        };

        if code == DXGI_ERROR_DEVICE_REMOVED.0 {
            "DXGI_ERROR_DEVICE_REMOVED"
        } else if code == DXGI_ERROR_DEVICE_RESET.0 {
            "DXGI_ERROR_DEVICE_RESET"
        } else if code == DXGI_ERROR_DEVICE_HUNG.0 {
            "DXGI_ERROR_DEVICE_HUNG"
        } else if code == DXGI_ERROR_DRIVER_INTERNAL_ERROR.0 {
            "DXGI_ERROR_DRIVER_INTERNAL_ERROR"
        } else if code == DXGI_ERROR_ACCESS_LOST.0 {
            "DXGI_ERROR_ACCESS_LOST"
        } else if code == DXGI_ERROR_INVALID_CALL.0 {
            "DXGI_ERROR_INVALID_CALL"
        } else {
            "unrecognized"
        }
    }

    /// What a *real* GPU device loss does to duplication, and whether the rebuild seam gets it
    /// back with nobody helping.
    ///
    /// Every other test of this seam is synthetic. `every_device_removal_reason_triggers_recovery`
    /// hands `device_removed_error` HRESULTs this process invented, and the daemon's recovery tests
    /// drive a scripted fake; between them they prove the mapping table and the retry loop, and
    /// nothing at all about which code Windows actually raises, at which call, when a driver really
    /// goes away. Issue #53 is that gap: the recovery path has never seen a device it did not fake,
    /// so "DXGI device loss is recovered from" has so far been a claim about a lookup table.
    ///
    /// Continuous rather than a single before/after pair, for the reason the lock harness in
    /// `session.rs` learned expensively: a transition sampled once describes whichever side of it
    /// the sample happened to land on, and then generalises from there. Every attempt here is
    /// bracketed by `GetDeviceRemovedReason`, and the pair decides what the sample is allowed to
    /// say. Both reads healthy and the failure was not a device loss at all, whatever it looked
    /// like. Both reads removed and the loss predates the call, so the classification is fully
    /// judgeable. A disagreement is the sample the loss landed *inside*, which is the only kind
    /// that can name where a loss surfaces - `AcquireNextFrame`, `Map`, or the next
    /// `DuplicateOutput` - and `operation` on the error is what names it.
    ///
    /// Counted in samples rather than run to a wall-clock deadline. A DXGI call against a driver
    /// that is restarting can block for seconds, so a run bounded by time becomes a loop that
    /// samples almost nothing and then reports that nothing was wrong.
    ///
    /// The trigger belongs to the operator by default: the test builds the backend, starts
    /// sampling, and says to press Ctrl+Win+Shift+B whenever they are ready, with the sample budget
    /// as its patience. `CAPTASTIC_GPU_RESET_SENDINPUT=1` makes the test press it instead, at
    /// sample `CAPTASTIC_GPU_RESET_TRIGGER_SAMPLE`; see [`send_display_driver_reset_chord`] for why
    /// that is opt-in rather than the default.
    ///
    /// `CAPTASTIC_GPU_RESET_EXPECT` says which outcome the chosen trigger is supposed to produce,
    /// because they are not all supposed to produce the same one and a harness that assumes they
    /// are cannot record the difference. `loss` (the default) is for a trigger believed to take the
    /// device away: no loss observed means the run proved nothing and it fails. `survival` is for a
    /// trigger measured *not* to take it away: it asserts the opposite, that every sample captured,
    /// which turns a negative result into a standing guard rather than an anecdote. Either way a
    /// loss that does occur has its classification and its recovery judged identically.
    ///
    /// Measured on 2026-08-20, Intel Arc iGPU, Windows 11 26200: a Ctrl+Win+Shift+B driver restart,
    /// screen blanked and machine beeped, confirmed by the operator at the keyboard, produced
    /// **1800 captures out of 1800 samples and not one lost device**, over 194 s. The assumption it
    /// was hired to check, that Ctrl+Win+Shift+B raises `DXGI_ERROR_DEVICE_REMOVED` on a duplication
    /// device, is measured false: 3600 bracketing `GetDeviceRemovedReason` calls all returned
    /// success.
    ///
    /// It does not follow that the chord leaves duplication alone. Later the same day, the same
    /// chord on the same host took all three of the daemon's retained sessions away with
    /// `DXGI_ERROR_ACCESS_LOST` - a different failure from the one this test looks for, and one the
    /// daemon recovered from in a single rebuild. This test holds *one* `new_primary()` duplication
    /// and has no message loop, so it cannot see a display reconfiguration and cannot say whether
    /// its run had one. Nothing yet explains the difference; `docs/windows-backend.md` keeps both
    /// results and the candidate explanations. Pick `CAPTASTIC_GPU_RESET_EXPECT` for the outcome
    /// the run is meant to guard, and read a `survival` failure as that open question rather than
    /// as a regression until it is closed.
    ///
    /// Every sample is written to `%TEMP%\captastic-gpu-reset.log`, overridable with
    /// `CAPTASTIC_GPU_RESET_LOG`, and flushed line by line so a run that hangs still leaves its
    /// evidence. Successes are aggregated rather than logged one by one, but any sample slower than
    /// `SLOW_SAMPLE` gets its own line: the first run of this test could say only that 1800 samples
    /// cost 194 s in total, which bounds a stall without locating one, and "was the sample next to
    /// the blank slow?" is the question that wanted answering.
    ///
    /// **Restarts the GPU driver.** The screen blanks for a second or two and every Direct3D
    /// application on the machine may lose its device: games, GPU-composited browsers, and video
    /// calls can drop out or close. Nothing is written to the clipboard or to disk, and the desktop
    /// is not otherwise disturbed.
    ///
    /// For the driver-restart chord, which is now known not to remove the device:
    ///
    /// CAPTASTIC_GPU_RESET_EXPECT=survival cargo test --locked -p captastic-windows --release
    ///     -- --ignored --nocapture a_real_device_loss_routes_through_the_rebuild_path
    ///
    /// For a trigger expected to remove it, such as an adapter cycle:
    ///
    /// cargo test --locked -p captastic-windows --release
    ///     -- --ignored --nocapture a_real_device_loss_routes_through_the_rebuild_path
    #[test]
    #[ignore = "restarts the GPU driver: the screen blanks and Direct3D applications can lose their devices"]
    fn a_real_device_loss_routes_through_the_rebuild_path() {
        use std::io::Write as _;

        /// How long between capture samples while the backend is healthy. Fast enough that a
        /// restart is not missed between two attempts, slow enough that the run is not itself a
        /// duplication soak.
        const SAMPLE_CADENCE: Duration = Duration::from_millis(100);
        /// A capture slow enough to be worth a line of its own. Healthy samples on this host cost
        /// single-digit milliseconds, so this is far above the noise and well below the seconds a
        /// stalled DXGI call is expected to cost.
        const SLOW_SAMPLE: Duration = Duration::from_millis(250);
        /// The kinds the daemon's `requires_backend_recovery` rebuilds for. Restated here because
        /// it lives in the binary crate and this one cannot see it; if the two ever disagree, this
        /// harness is measuring a recovery the product does not perform.
        const REBUILDABLE: [CaptureErrorKind; 3] = [
            CaptureErrorKind::AccessLost,
            CaptureErrorKind::DeviceRemoved,
            CaptureErrorKind::TopologyChanged,
        ];

        fn env_u32(name: &str, fallback: u32) -> u32 {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(fallback)
        }

        /// The daemon's own back-off curve, 50 ms doubling to a 2 s ceiling, restated for the same
        /// reason as `REBUILDABLE`. A harness that waited on a different curve would measure a
        /// recovery nobody ships.
        fn recovery_delay(failed_attempts: u32) -> Duration {
            let exponent = failed_attempts.saturating_sub(1).min(5);
            Duration::from_millis(50_u64.saturating_mul(1_u64 << exponent))
                .min(Duration::from_secs(2))
        }

        /// What the device says about itself, as the raw HRESULT, so the log reads against the
        /// DXGI header rather than against this crate's opinion of it.
        fn removal_reason(device: &ID3D11Device) -> Option<i32> {
            // SAFETY: the device is a live COM interface owned by the backend that lent it.
            match unsafe { device.GetDeviceRemovedReason() } {
                Ok(()) => None,
                Err(reason) => Some(reason.code().0),
            }
        }

        fn describe_reason(reason: Option<i32>) -> String {
            reason.map_or_else(
                || "live".to_owned(),
                |code| format!("{code:#010X}/{}", removal_reason_name(code)),
            )
        }

        let samples = env_u32("CAPTASTIC_GPU_RESET_SAMPLES", 400);
        let inject = std::env::var("CAPTASTIC_GPU_RESET_SENDINPUT").is_ok_and(|value| value == "1");
        let trigger_sample = env_u32("CAPTASTIC_GPU_RESET_TRIGGER_SAMPLE", 25);
        let expect_survival = std::env::var("CAPTASTIC_GPU_RESET_EXPECT")
            .is_ok_and(|value| value.eq_ignore_ascii_case("survival"));
        let log_path = std::env::var_os("CAPTASTIC_GPU_RESET_LOG").map_or_else(
            || std::env::temp_dir().join("captastic-gpu-reset.log"),
            std::path::PathBuf::from,
        );
        let mut sink = std::fs::File::create(&log_path).unwrap_or_else(|error| {
            panic!(
                "the measurement log at {} must be writable, because it is the deliverable: {error}",
                log_path.display()
            )
        });
        let started = Instant::now();
        let mut record = |line: String| {
            println!("{line}");
            let _ = writeln!(sink, "{line}");
            let _ = sink.flush();
        };

        record(format!("measurement log: {}", log_path.display()));
        record(format!(
            "{samples} samples at {} ms; expecting the trigger to {}; trigger: {}",
            SAMPLE_CADENCE.as_millis(),
            if expect_survival {
                "leave the device alone"
            } else {
                "remove the device"
            },
            if inject {
                format!("injected at sample {trigger_sample}")
            } else {
                "PRESS Ctrl+Win+Shift+B WHEN READY".to_owned()
            }
        ));

        let mut backend = Some(
            DxgiBackend::new_primary()
                .expect("a duplication backend must exist before a device loss can be measured"),
        );
        let mut next_capture_id = 1_u64;
        let mut successes = 0_u32;
        let mut timeouts = 0_u32;
        let mut losses = 0_u32;
        let mut rebuild_attempts = 0_u32;
        let mut total_rebuild_attempts = 0_u32;
        let mut first_loss_at: Option<Duration> = None;
        let mut recovered_at: Option<Duration> = None;
        let mut surfaced_in: Vec<String> = Vec::new();
        let mut misclassified: Vec<String> = Vec::new();
        let mut unexplained: Vec<String> = Vec::new();
        // Aggregate capture cost, kept because the first run could report only that the whole
        // sweep took 194 s - enough to bound a stall, not enough to find one.
        let mut capture_cost = Duration::ZERO;
        let mut slowest = Duration::ZERO;
        let mut slow_samples = 0_u32;

        for sample in 0..samples {
            if inject && sample == trigger_sample {
                record(format!(
                    "t={:7.3}s sample={sample} injecting Ctrl+Win+Shift+B",
                    started.elapsed().as_secs_f64()
                ));
                send_display_driver_reset_chord();
            }
            let Some(active) = backend.as_mut() else {
                // The backend is gone, so this sample's work is the daemon's: wait out the
                // back-off, then try to build a replacement. Rebuild attempts are samples too,
                // which is what keeps the loop counted rather than timed.
                rebuild_attempts = rebuild_attempts.saturating_add(1);
                total_rebuild_attempts = total_rebuild_attempts.saturating_add(1);
                std::thread::sleep(recovery_delay(rebuild_attempts));
                match DxgiBackend::new_primary() {
                    Ok(replacement) => {
                        record(format!(
                            "t={:7.3}s sample={sample} REBUILT on attempt {rebuild_attempts}",
                            started.elapsed().as_secs_f64()
                        ));
                        backend = Some(replacement);
                    }
                    Err(error) => {
                        record(format!(
                            "t={:7.3}s sample={sample} rebuild {rebuild_attempts} refused kind={:?} op={} retryable={}: {}",
                            started.elapsed().as_secs_f64(),
                            error.kind,
                            error.operation,
                            error.retryable,
                            error.message
                        ));
                    }
                }
                continue;
            };

            let reason_before = removal_reason(&active.device);
            let request = CaptureRequest {
                id: CaptureId(next_capture_id),
                triggered_at: Instant::now(),
                source: CaptureSource::Display(active.selected.id.clone()),
                mode: CaptureMode::Latest { max_age_ms: None },
                // The daemon's own default, and it matters: the CPU readback is where a lost
                // device reports itself through `Map`, so skipping it would move the seam under
                // test somewhere else.
                cpu_frame: true,
                retain_native_frame: false,
                cursor: CursorMode::Exclude,
            };
            next_capture_id = next_capture_id.saturating_add(1);
            let mut recorder = EventRecorder::with_capacity(16);
            let attempt_started = Instant::now();
            let outcome = active.capture(&request, &mut recorder);
            let took = attempt_started.elapsed();
            let reason_after = removal_reason(&active.device);
            let at = started.elapsed();
            capture_cost = capture_cost.saturating_add(took);
            slowest = slowest.max(took);
            if took >= SLOW_SAMPLE {
                slow_samples = slow_samples.saturating_add(1);
                record(format!(
                    "t={:7.3}s sample={sample} SLOW {:.0} ms; device {} -> {}",
                    at.as_secs_f64(),
                    took.as_secs_f64() * 1_000.0,
                    describe_reason(reason_before),
                    describe_reason(reason_after)
                ));
            }

            match outcome {
                Ok(_) => {
                    successes = successes.saturating_add(1);
                    if reason_after.is_some() {
                        // Possible, and not a fault: the readback finished before the device died.
                        // Recorded rather than judged, because the sample straddles a transition.
                        record(format!(
                            "t={:7.3}s sample={sample} ok across a transition; device now {}",
                            at.as_secs_f64(),
                            describe_reason(reason_after)
                        ));
                    }
                    if first_loss_at.is_some() && recovered_at.is_none() {
                        recovered_at = Some(at);
                        record(format!(
                            "t={:7.3}s sample={sample} RECOVERED: first capture to succeed after the loss, {:.0} ms and {total_rebuild_attempts} rebuild attempt(s) later",
                            at.as_secs_f64(),
                            at.saturating_sub(first_loss_at.unwrap_or_default())
                                .as_secs_f64()
                                * 1_000.0
                        ));
                    }
                }
                Err(error) => {
                    let line = format!(
                        "t={:7.3}s sample={sample} FAIL kind={:?} op={} retryable={} hr={} device_before={} device_after={} took={:.1}ms: {}",
                        at.as_secs_f64(),
                        error.kind,
                        error.operation,
                        error.retryable,
                        error
                            .native_code
                            .map_or_else(|| "none".to_owned(), |code| format!("{code:#X}")),
                        describe_reason(reason_before),
                        describe_reason(reason_after),
                        took.as_secs_f64() * 1_000.0,
                        error.message
                    );
                    record(line.clone());
                    if error.kind == CaptureErrorKind::Timeout && reason_after.is_none() {
                        // An idle desktop presents nothing, so duplication has nothing to hand
                        // over. Ordinary, and evidence of nothing either way.
                        timeouts = timeouts.saturating_add(1);
                    } else if reason_after.is_some() {
                        losses = losses.saturating_add(1);
                        if first_loss_at.is_none() {
                            first_loss_at = Some(at);
                        }
                        if reason_before.is_none() {
                            surfaced_in.push(format!(
                                "{} (kind {:?}, reason {})",
                                error.operation,
                                error.kind,
                                describe_reason(reason_after)
                            ));
                        }
                        if !REBUILDABLE.contains(&error.kind) {
                            misclassified.push(line.clone());
                        }
                    } else {
                        // Healthy on both sides of its own failure: whatever this was, it was not
                        // the device going away, and calling it one would be the mistake the
                        // bracketing exists to prevent.
                        unexplained.push(line);
                    }
                    if REBUILDABLE.contains(&error.kind) {
                        backend = None;
                        rebuild_attempts = 0;
                    }
                }
            }
            std::thread::sleep(SAMPLE_CADENCE);
        }

        record(format!(
            "{samples} samples over {:.0}s: {successes} captured, {losses} failed with a lost device, {timeouts} idle timeouts, {total_rebuild_attempts} rebuild attempt(s), {} unexplained",
            started.elapsed().as_secs_f64(),
            unexplained.len()
        ));
        record(format!(
            "capture cost: {:.1}s total, {:.1} ms mean, {:.0} ms slowest, {slow_samples} over {} ms",
            capture_cost.as_secs_f64(),
            capture_cost.as_secs_f64() * 1_000.0 / f64::from(successes.max(1)),
            slowest.as_secs_f64() * 1_000.0,
            SLOW_SAMPLE.as_millis()
        ));
        for surfaced in &surfaced_in {
            record(format!("the loss surfaced in {surfaced}"));
        }
        for line in &unexplained {
            record(format!("unexplained: {line}"));
        }

        if expect_survival {
            // The trigger is one already measured not to take the device away, so the finding is
            // the survival itself and the run is a guard against that changing. A loss here would
            // not fail the run - it would mean the trigger became a `loss` trigger, which the
            // classification and recovery assertions below then judge on their own terms.
            assert_eq!(
                successes,
                samples,
                "the trigger was expected to leave duplication alone and {} of {samples} samples \
                 did not capture. Before calling that a regression, note that the daemon's three \
                 retained sessions did lose access to this same chord on 2026-08-20 while this \
                 harness's single duplication did not, and nothing explains the difference yet; \
                 see the GPU device loss section of docs/windows-backend.md. The log is at {}",
                samples.saturating_sub(successes),
                log_path.display()
            );
            record(format!(
                "expected survival and got it: {successes}/{samples} samples captured, {losses} lost devices"
            ));
        } else {
            assert!(
                losses > 0,
                "no duplication call ever saw a removed device, so this trigger measured nothing \
                 about the recovery path. Either it never fired, or it does not remove the device \
                 this backend holds - which is what Ctrl+Win+Shift+B was measured to do on \
                 2026-08-20, and why that chord wants CAPTASTIC_GPU_RESET_EXPECT=survival rather \
                 than this branch. The log is at {}",
                log_path.display()
            );
        }
        assert!(
            misclassified.is_empty(),
            "the device was gone and the error said something the daemon does not rebuild for, \
             which is exactly the demotion `device_removed_error` exists to prevent:\n{}",
            misclassified.join("\n")
        );
        // Conditional on there having been something to recover from. Unconditional, this would
        // report a trigger that never removed the device as a failure to recover from a loss that
        // never happened, which is the wrong finding written over the right one.
        assert!(
            losses == 0 || recovered_at.is_some(),
            "duplication never came back on its own after the loss, so the daemon would not have \
             either. The log is at {}",
            log_path.display()
        );
    }

    #[test]
    fn gpu_region_coordinates_are_normalized_against_the_display() {
        assert_eq!(
            local_selection(
                Rect {
                    x: -1920,
                    y: 120,
                    width: 1920,
                    height: 1080,
                },
                Rect {
                    x: -1820,
                    y: 320,
                    width: 640,
                    height: 480,
                },
            )
            .expect("valid selection"),
            Rect {
                x: 100,
                y: 200,
                width: 640,
                height: 480,
            }
        );
    }

    #[test]
    fn gpu_region_rejects_a_selection_outside_the_snapshot() {
        assert!(local_selection(
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            Rect {
                x: 1800,
                y: 900,
                width: 200,
                height: 200,
            },
        )
        .is_err());
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop and DXGI duplication access"]
    fn native_gpu_region_matches_the_frozen_cpu_frame() {
        let mut backend = DxgiBackend::new_primary().expect("DXGI backend");
        assert!(
            backend.latest.is_none(),
            "backend initialization must not acquire a desktop frame"
        );
        let source = backend.selected.bounds;
        let capture_started = Instant::now();
        let outcome = loop {
            let request = CaptureRequest {
                id: CaptureId(1),
                triggered_at: Instant::now(),
                source: CaptureSource::Display(backend.selected.id.clone()),
                mode: CaptureMode::Latest { max_age_ms: None },
                cpu_frame: true,
                retain_native_frame: true,
                cursor: CursorMode::Exclude,
            };
            let mut recorder = EventRecorder::with_capacity(16);
            match backend.capture(&request, &mut recorder) {
                Ok(outcome) => break outcome,
                Err(error)
                    if error.kind == CaptureErrorKind::Timeout
                        && capture_started.elapsed() < Duration::from_secs(2) =>
                {
                    std::thread::sleep(Duration::from_millis(16));
                }
                Err(error) => panic!("capture: {error:?}"),
            }
        };
        let selection = Rect {
            x: source.x,
            y: source.y,
            width: source.width.min(64),
            height: source.height.min(64),
        };
        let expected = outcome
            .frame
            .expect("CPU frame")
            .crop(selection)
            .expect("CPU crop");
        let native_frame = outcome.native_frame.expect("native frame");
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = materialize_native_region(native_frame.as_ref(), selection);
            let _ = sender.send(result);
        });
        let actual = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("cross-thread GPU materialization must not hang")
            .expect("GPU materialization")
            .expect("DXGI frame");
        assert_eq!(actual.frame.width(), expected.width());
        assert_eq!(actual.frame.height(), expected.height());
        assert_eq!(actual.frame.pixels(), expected.pixels());
    }
    fn labeled_bgra(width: u32, height: u32, stride: usize) -> Vec<u8> {
        let mut pixels = vec![0xee; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let label = (y * width as usize + x + 1) as u8;
                let start = y * stride + x * 4;
                pixels[start..start + 4].copy_from_slice(&[label, 0, 0, 0xff]);
            }
        }
        pixels
    }

    fn blue_channel(pixels: &[u8]) -> Vec<u8> {
        pixels.chunks_exact(4).map(|pixel| pixel[0]).collect()
    }

    /// Milestone 5's exit criterion, measured rather than asserted: a cursor-on capture and a
    /// cursor-off capture of the same desktop must be identical everywhere except where the
    /// pointer is, and must actually differ there.
    ///
    /// Needs a desktop that is actually changing, and moving the mouse is not enough: a pointer
    /// move is a hardware-cursor update and does not dirty the desktop image, which is the same
    /// reason DXGI reports pointer position separately from frame content. Drag a window, scroll
    /// something, or play a video while this runs.
    ///
    /// Serializes the tests that drive the mouse.
    ///
    /// There is one pointer on a desk, and these tests park it somewhere and then assert on where
    /// it ended up. Run in parallel they take it from each other, and the assertion that fails is
    /// the position one - so a scheduling collision reads exactly like the rotation bug these
    /// tests exist to catch.
    fn pointer_lock() -> std::sync::MutexGuard<'static, ()> {
        static POINTER: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // A test that panics while holding this poisons it, and the next one still needs the desk.
        POINTER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Makes this process speak physical pixels, as DXGI does.
    ///
    /// A test binary carries no manifest, so it starts DPI-unaware and `GetCursorPos` and
    /// `SetCursorPos` would report and accept virtualized coordinates while DXGI reports physical
    /// ones. On a 150% display the two differ by a third, which reads exactly like a rotation bug.
    fn make_process_dpi_aware() {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };

        // SAFETY: No DPI-dependent call has been made yet, and a second call in the same process
        // is a no-op failure that nothing here depends on.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }

    fn park_pointer(x: i32, y: i32) {
        // SAFETY: SetCursorPos takes two plain coordinates.
        unsafe { windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y) }.ok();
    }

    fn pointer_position() -> (i32, i32) {
        let mut point = windows::Win32::Foundation::POINT::default();
        // SAFETY: point is valid writable storage.
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point) }
            .expect("GetCursorPos");
        (point.x, point.y)
    }

    /// Moves the pointer one pixel and back, so the next duplicated frame carries a mouse update,
    /// and leaves it exactly on `(x, y)`.
    ///
    /// DXGI populates `PointerPosition` only on frames where the pointer changed, so a stationary
    /// pointer is indistinguishable from an absent one no matter how long a test waits.
    ///
    /// Absolute moves rather than a `SendInput` relative pair. A relative move goes through pointer
    /// acceleration, so a +1/-1 round trip does not reliably return to where it started; over a
    /// retry loop the drift reached tens of pixels, which is indistinguishable from the position
    /// being reported in the wrong coordinate space.
    fn nudge_pointer_at(x: i32, y: i32) {
        park_pointer(x + 1, y);
        std::thread::sleep(std::time::Duration::from_millis(25));
        park_pointer(x, y);
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    /// Nudges the pointer where the primary display's own tests want it.
    ///
    /// Parked mid-screen first. A pointer resting at a screen edge, or over an application that has
    /// hidden it, is reported visible with a fully transparent shape - which is a truthful answer
    /// to a question those tests are not asking.
    fn nudge_pointer() {
        nudge_pointer_at(960, 540);
    }

    /// cargo test --locked -p captastic-windows --release
    ///     -- --ignored --nocapture cursor_composition_changes_only_the_pointer_rectangle
    #[test]
    #[ignore = "requires an interactive desktop with a visible pointer"]
    fn cursor_composition_changes_only_the_pointer_rectangle() {
        use std::time::Instant;

        let _pointer = pointer_lock();

        let mut backend = DxgiBackend::new_primary().expect("dxgi backend");
        let mut recorder = EventRecorder::with_capacity(16);
        let request = |cursor| CaptureRequest {
            id: CaptureId(1),
            triggered_at: Instant::now(),
            source: CaptureSource::Display(DisplayId::primary()),
            // `latest` rather than `fresh`, so both captures read the *same* retained frame and
            // the only difference between them is the composition. Two fresh frames are two
            // moments, and on a desktop that repainted between them millions of pixels differ for
            // reasons that have nothing to do with the pointer - which makes "the change is
            // confined to the pointer rectangle" unprovable exactly when it matters.
            mode: CaptureMode::Latest { max_age_ms: None },
            cpu_frame: true,
            retain_native_frame: false,
            cursor,
        };

        // Nudged rather than waited for. DXGI fills in PointerPosition only on a frame that
        // carries a mouse update, so a still pointer reads as `NotVisible` however long the wait -
        // which is how this test first failed, having asked a human to move the mouse and then
        // sampled before they could. A one-pixel round trip is an unambiguous update and leaves
        // the pointer where it started.
        let mut with = None;
        for attempt in 0..20 {
            nudge_pointer();
            let frame = backend
                .capture(&request(CursorMode::Include), &mut recorder)
                .expect("capture with a cursor")
                .frame
                .expect("cpu frame");
            println!("attempt {}: {:?}", attempt + 1, frame.metadata.cursor);
            if matches!(
                frame.metadata.cursor,
                Some(CursorCapture::Composited { .. })
            ) {
                println!("composited pointer on attempt {}", attempt + 1);
                with = Some(frame);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let with = with.expect(
            "no capture in 20 attempts carried a composited pointer; is the pointer on this display,              and is something drawing its own cursor?",
        );
        // Taken after, and without touching the mouse in between, so it reads the same retained
        // frame this one did. The pointer position is remembered, so excluding the cursor does not
        // disturb it.
        let without = backend
            .capture(&request(CursorMode::Exclude), &mut recorder)
            .expect("capture without a cursor")
            .frame
            .expect("cpu frame");

        let Some(CursorCapture::Composited {
            x,
            y,
            width,
            height,
        }) = with.metadata.cursor
        else {
            unreachable!("the loop above only accepts a composited pointer");
        };
        assert_eq!(without.metadata.cursor, Some(CursorCapture::Excluded));
        println!("pointer reported at ({x},{y}) sized {width}x{height}");

        // The two captures are of different moments, so anything that repainted between them
        // differs too. What must hold is the containment: every difference outside the pointer
        // rectangle has to be explained by the desktop changing, and inside it there must be at
        // least one difference, or nothing was drawn.
        let stride = with.stride_bytes() as usize;
        let mut differing_inside = 0_u64;
        let mut differing_outside = 0_u64;
        for row in 0..with.height() as usize {
            for column in 0..with.width() as usize {
                let start = row * stride + column * 4;
                if with.pixels()[start..start + 4] == without.pixels()[start..start + 4] {
                    continue;
                }
                let inside = (column as i64) >= i64::from(x)
                    && (column as i64) < i64::from(x) + i64::from(width)
                    && (row as i64) >= i64::from(y)
                    && (row as i64) < i64::from(y) + i64::from(height);
                if inside {
                    differing_inside += 1;
                } else {
                    differing_outside += 1;
                }
            }
        }

        let pointer_area = u64::from(width) * u64::from(height);
        println!(
            "{differing_inside}/{pointer_area} pixels differ inside the pointer rectangle,              {differing_outside} outside it (desktop repaints)"
        );
        assert!(
            differing_inside > 0,
            "nothing was drawn inside the pointer rectangle. If the shape Windows supplied is              entirely transparent - an application that has hidden its cursor reports exactly              that, while still reporting the pointer visible - then this is a truthful capture of              an invisible pointer rather than a composition failure. Move the mouse over an              ordinary window and run it again."
        );
    }

    /// Milestone 5's rotation question, settled by measurement rather than left as a refusal: on a
    /// rotated display the pointer must be drawn where the user sees it, the right way up.
    ///
    /// Both halves of DXGI's pointer report turn out to be in the *upright* desktop space already -
    /// the space `normalize_bgra_into` produces - so composition needs no transform at all. That is
    /// a surprising answer, and the whole point of this test is that the two ways of getting it
    /// wrong both produce a screenshot a human would accept:
    ///
    /// * A position run through the same mapping the pixels get. On a quarter turn that is a
    ///   transpose, so the pointer lands somewhere else entirely on screen but still inside the
    ///   frame, looking like nothing worse than a mis-parked mouse.
    /// * A shape turned to match the panel. The arrow is then drawn lying on its side, in exactly
    ///   the right place, which reads as a rendering artefact rather than a coordinate bug.
    ///
    /// So the position is checked against `GetCursorPos` - which is upright by definition - with an
    /// explicit assertion that the transposed answer would have been *different*, and the shape is
    /// checked against GDI's rendering of the same cursor, which has no orientation of its own.
    ///
    /// Verified on a DELL U2723QE driven through 0, 90, 180 and 270 by `ChangeDisplaySettingsExW`;
    /// residuals against `GetCursorPos` were 0 px at every orientation across six spread-out sample
    /// points, while the transposed hypothesis was out by up to ~3,500 px.
    ///
    /// cargo test --locked -p captastic-windows --release
    ///     -- --ignored --nocapture cursor_composition_on_a_rotated_display_is_upright_and_in_place
    #[test]
    #[ignore = "requires an interactive desktop with a rotated display"]
    fn cursor_composition_on_a_rotated_display_is_upright_and_in_place() {
        use std::time::Instant;

        let _pointer = pointer_lock();

        make_process_dpi_aware();
        let displays = enumerate_display_adapters().expect("enumerate displays");
        let Some((display, _)) = displays
            .iter()
            .find(|(display, _)| display.rotation_degrees != 0)
        else {
            println!(
                "no rotated display is attached, so there is nothing here to verify. Turn one to             portrait in Settings > System > Display and run this again."
            );
            return;
        };
        let rotation = display.rotation_degrees;
        let bounds = display.bounds;
        println!(
            "display {} at {rotation} degrees, bounds {},{} {}x{}",
            display.id.0, bounds.x, bounds.y, bounds.width, bounds.height
        );

        // Deliberately off-centre and off-diagonal, so that the transposed reading of the same
        // point is a different point - and still inside the frame, because a transform that merely
        // pushed the pointer out of bounds would be caught by the clipping rather than by this.
        let chosen = (bounds.width as i32 * 3 / 10, bounds.height as i32 / 5);
        let parked = (bounds.x + chosen.0, bounds.y + chosen.1);
        let (raw_width, raw_height) =
            dimensions_after_rotation(bounds.width, bounds.height, rotation);

        let mut backend =
            DxgiBackend::new(&display.id).expect("dxgi backend for a rotated display");
        let mut recorder = EventRecorder::with_capacity(16);
        let request = |cursor, mode| CaptureRequest {
            id: CaptureId(1),
            triggered_at: Instant::now(),
            source: CaptureSource::Display(display.id.clone()),
            mode,
            cpu_frame: true,
            retain_native_frame: false,
            cursor,
        };
        let latest = || CaptureMode::Latest { max_age_ms: None };

        // What DXGI says about the pointer, on its own, before any frame is involved. This needs
        // no repaint: the cache is fed at every acquisition, whatever becomes of the pixels.
        //
        // `GetCursorPos` answers with the hotspot and DXGI answers with the shape's top-left, so
        // the hotspot of whatever cursor is under the pointer stands between the two. Read here
        // rather than assumed zero: over plain desktop it is an arrow and it is zero, but over a
        // link or a window edge it is not, and a test that only ever ran over wallpaper would pass
        // for the wrong reason. Subtracting it also pins the top-left semantics the positioning
        // relies on - if DXGI ever reported the hotspot instead, this is what would notice.
        //
        // The pointer is put back on the chosen point every attempt, but what the assertion uses
        // is where it actually *was*, sampled either side of the read. A hand on the mouse then
        // costs an attempt instead of failing the run, and the geometry is unaffected: any point
        // on this display discriminates as long as it is not on the symmetry, which is checked.
        let mut measurement = None;
        for _ in 0..30 {
            nudge_pointer_at(parked.0, parked.1);
            let before = pointer_position();
            let _ = backend.capture(&request(CursorMode::Exclude, latest()), &mut recorder);
            let Some(at) = backend.pointer.position().filter(|at| at.visible) else {
                continue;
            };
            let Some(shape) = backend.pointer.current() else {
                continue;
            };
            let Some(hotspot) = cursor_hotspot(shape.width) else {
                continue;
            };
            if pointer_position() == before {
                measurement = Some((at, hotspot, before));
                break;
            }
        }
        let (reported, hotspot, desktop) = measurement.expect(
            "never got a reading with the pointer holding still on the rotated display. Something             is moving the mouse - a hand on it, or another test - so nothing here can be compared             against anything",
        );
        let observed = (desktop.0 - bounds.x, desktop.1 - bounds.y);
        assert!(
            observed.0 >= 0
                && observed.1 >= 0
                && observed.0 < bounds.width as i32
                && observed.1 < bounds.height as i32,
            "the pointer ended up at {desktop:?}, which is not on the rotated display at all"
        );
        let expected_top_left = (observed.0 - hotspot.0, observed.1 - hotspot.1);
        let transposed = upright_from_raw(
            expected_top_left,
            raw_width as i32,
            raw_height as i32,
            rotation,
        );
        assert_ne!(
            transposed, expected_top_left,
            "the sample point reads the same in both coordinate spaces, so it cannot tell them             apart; move it off the symmetry"
        );
        println!(
            "GetCursorPos {observed:?} minus hotspot {hotspot:?} is {expected_top_left:?}; DXGI             reported ({},{})",
            reported.x, reported.y
        );
        assert_eq!(
            (reported.x, reported.y),
            expected_top_left,
            "DXGI reported the pointer at ({},{}) on a {rotation} degree display, but GetCursorPos             puts its shape's top-left at {expected_top_left:?} in the frame's own coordinates. Had             the position needed the rotation mapping the pixels get, it would have read             {transposed:?}",
            reported.x,
            reported.y
        );

        // Now end to end, through both pointer paths. `fresh` runs the live one and needs a frame,
        // and a display showing something static produces none - so the desktop is asked to
        // repaint. `latest` then reads the frame that capture retained, which is the path the
        // resident daemon actually runs.
        // Where the pointer was when *this* frame was taken, sampled either side of the capture
        // for the same reason as above: the frame and the desktop have to describe one moment.
        let mut with = None;
        for _ in 0..30 {
            nudge_pointer_at(parked.0, parked.1);
            force_desktop_repaint();
            let before = pointer_position();
            let Ok(outcome) = backend.capture(
                &request(CursorMode::Include, CaptureMode::Fresh { timeout_ms: 500 }),
                &mut recorder,
            ) else {
                continue;
            };
            let frame = outcome.frame.expect("cpu frame");
            if pointer_position() != before {
                continue;
            }
            if matches!(
                frame.metadata.cursor,
                Some(CursorCapture::Composited { .. })
            ) {
                with = Some((frame, before));
                break;
            }
            println!("live path did not composite: {:?}", frame.metadata.cursor);
        }
        let (with, desktop) = with.expect(
            "no fresh capture composited a pointer on the rotated display while the pointer held             still; if the shape Windows supplies there is entirely transparent, park the pointer             over an ordinary window and run it again",
        );
        let Some(CursorCapture::Composited {
            x,
            y,
            width,
            height,
        }) = with.metadata.cursor
        else {
            unreachable!("the loop above only accepts a composited pointer");
        };
        let drawn_at = (
            desktop.0 - bounds.x - hotspot.0,
            desktop.1 - bounds.y - hotspot.1,
        );
        let transposed = upright_from_raw(drawn_at, raw_width as i32, raw_height as i32, rotation);
        println!("composited at ({x},{y}) sized {width}x{height}, expected {drawn_at:?}");
        assert_ne!(
            transposed, drawn_at,
            "the pointer ended up on the symmetry, where both coordinate spaces read alike, so this             capture cannot tell them apart"
        );
        assert_eq!(
            (x, y),
            drawn_at,
            "the live path drew the pointer at ({x},{y}) instead of {drawn_at:?}; the transposed             reading of that point is {transposed:?}"
        );

        // The retained path, reading the frame the capture above retained, must agree with the
        // live one. They are separate decisions in separate functions, and a guard removed from
        // one and left in the other is exactly the kind of divergence this pins down.
        let retained = backend
            .capture(&request(CursorMode::Include, latest()), &mut recorder)
            .expect("retained capture with a cursor")
            .frame
            .expect("cpu frame");
        assert_eq!(
            retained.metadata.cursor,
            Some(CursorCapture::Composited {
                x,
                y,
                width,
                height
            }),
            "the retained path disagreed with the live one about the same pointer"
        );

        // Same retained frame, cursor excluded, so composition is the only difference between the
        // two images and every differing pixel outside the rectangle is a desktop repaint.
        let without = backend
            .capture(&request(CursorMode::Exclude, latest()), &mut recorder)
            .expect("retained capture without a cursor")
            .frame
            .expect("cpu frame");
        assert_eq!(without.metadata.cursor, Some(CursorCapture::Excluded));
        let stride = retained.stride_bytes() as usize;
        let mut differing_inside = 0_u64;
        let mut differing_outside = 0_u64;
        for row in 0..retained.height() as usize {
            for column in 0..retained.width() as usize {
                let start = row * stride + column * 4;
                if retained.pixels()[start..start + 4] == without.pixels()[start..start + 4] {
                    continue;
                }
                let inside = (column as i64) >= i64::from(x)
                    && (column as i64) < i64::from(x) + i64::from(width)
                    && (row as i64) >= i64::from(y)
                    && (row as i64) < i64::from(y) + i64::from(height);
                if inside {
                    differing_inside += 1;
                } else {
                    differing_outside += 1;
                }
            }
        }
        println!(
            "{differing_inside}/{} pixels differ inside the pointer rectangle, {differing_outside}             outside it",
            u64::from(width) * u64::from(height)
        );
        assert!(
            differing_inside > 0,
            "nothing was drawn inside the pointer rectangle"
        );
        assert_eq!(
            differing_outside, 0,
            "the two captures read the same retained frame, so a difference outside the pointer             rectangle means something other than the pointer was drawn"
        );

        // And the shape's orientation, against an oracle that knows nothing about DXGI. GDI draws
        // the logical cursor, which does not turn with the panel, so if what DXGI delivered has
        // been turned it will match one of the other three quarters better than it matches this.
        let shape = backend.pointer.current().expect("a cached pointer shape");
        assert_eq!(
            shape.width, shape.height,
            "this comparison turns the shape in quarters, which needs it square; DXGI delivered             {}x{}",
            shape.width, shape.height
        );
        let delivered = shape_ink(shape);
        let reference = gdi_cursor_ink(shape.width)
            .expect("GDI would not render the current cursor, so there is nothing to compare to");
        let size = shape.width as usize;

        let self_similarity = quarter_turn_overlaps(&reference, &reference, size);
        println!("reference against its own quarter turns: {self_similarity:?}");
        let most_symmetric = self_similarity[1]
            .max(self_similarity[2])
            .max(self_similarity[3]);
        assert!(
            most_symmetric < 0.85,
            "the cursor under the pointer is too close to rotationally symmetric to say which way             up it is ({most_symmetric:.2} overlap with one of its own quarter turns). Park the             pointer over plain desktop, where it is an arrow, and run it again"
        );

        let overlaps = quarter_turn_overlaps(&reference, &delivered, size);
        println!(
            "delivered shape against GDI's rendering, by quarter turn: 0 {:.2}, 90 {:.2}, 180             {:.2}, 270 {:.2}",
            overlaps[0], overlaps[1], overlaps[2], overlaps[3]
        );
        let best_turned = overlaps[1].max(overlaps[2]).max(overlaps[3]);
        assert!(
            overlaps[0] > best_turned + 0.15,
            "the shape DXGI delivered on a {rotation} degree display matches a quarter turn of the             cursor better than it matches the cursor itself ({:.2} against {best_turned:.2}), so it             arrived rotated and drawing it as-is would put the arrow on its side",
            overlaps[0]
        );
    }

    /// Reads a position in the duplicated surface's own coordinates as one in the upright frame,
    /// which is the mapping `normalize_bgra_into` applies to the pixels.
    ///
    /// Present only so the test above can say what the wrong answer would have looked like.
    fn upright_from_raw(
        position: (i32, i32),
        raw_width: i32,
        raw_height: i32,
        rotation_degrees: u16,
    ) -> (i32, i32) {
        let (x, y) = position;
        match rotation_degrees {
            90 => (raw_height - 1 - y, x),
            180 => (raw_width - 1 - x, raw_height - 1 - y),
            270 => (y, raw_width - 1 - x),
            _ => position,
        }
    }

    /// Asks every window to repaint, so a duplication of a display showing something static still
    /// yields a frame. A null window means the desktop, which covers every display.
    fn force_desktop_repaint() {
        use windows::Win32::Graphics::Gdi::{
            RedrawWindow, HRGN, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW,
        };

        // SAFETY: A null window and a null update region ask for the whole desktop.
        unsafe {
            RedrawWindow(
                None,
                None,
                HRGN(0),
                RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
            )
        };
        std::thread::sleep(std::time::Duration::from_millis(60));
    }

    /// Which pixels of a pointer shape leave a mark, whatever their format says about how.
    ///
    /// Colour and inversion are both marks; only "leave the frame alone" is not. That is the right
    /// abstraction for comparing silhouettes, because the two mask formats have no colour to
    /// compare in the first place.
    fn shape_ink(shape: &crate::cursor::PointerShape) -> Vec<bool> {
        let width = shape.width as usize;
        let height = shape.height as usize;
        let pitch = shape.pitch as usize;
        let mut ink = vec![false; width * height];
        for y in 0..height {
            for x in 0..width {
                ink[y * width + x] = match shape.kind {
                    crate::cursor::PointerShapeKind::Color => {
                        shape.pixels[y * pitch + x * 4 + 3] != 0
                    }
                    crate::cursor::PointerShapeKind::MaskedColor => {
                        let pixel = &shape.pixels[y * pitch + x * 4..y * pitch + x * 4 + 4];
                        // Alpha is a selector here: zero copies the colour, anything else XORs it,
                        // and XOR with black changes nothing.
                        pixel[3] == 0 || pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0
                    }
                    crate::cursor::PointerShapeKind::Monochrome => {
                        let bit =
                            |row: usize| shape.pixels[row * pitch + x / 8] & (0x80 >> (x % 8)) != 0;
                        // AND clear paints; AND set with XOR set inverts; the fourth case is the
                        // transparent one.
                        !bit(y) || bit(height + y)
                    }
                };
            }
        }
        ink
    }

    /// The hotspot of the cursor Windows currently has on screen, in the pixels of a shape `size`
    /// wide.
    ///
    /// Windows hands out DPI-scaled cursors, so the handle behind `GetCursorInfo` is not always
    /// the size DXGI delivers; the hotspot is scaled by the ratio between them rather than assumed
    /// to be in the same units.
    fn cursor_hotspot(size: u32) -> Option<(i32, i32)> {
        use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetCursorInfo, GetIconInfo, CURSORINFO, CURSORINFO_FLAGS, HICON, ICONINFO,
        };

        let mut cursor = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: cursor is writable storage carrying its own size.
        unsafe { GetCursorInfo(&mut cursor) }.ok()?;
        if cursor.flags != CURSORINFO_FLAGS(1) {
            return None;
        }
        let mut icon = ICONINFO::default();
        // SAFETY: the handle comes from GetCursorInfo and icon is writable storage. The two
        // bitmaps it fills in belong to the caller and are deleted below.
        unsafe { GetIconInfo(HICON(cursor.hCursor.0), &mut icon) }.ok()?;

        // A monochrome cursor has no colour bitmap, and its mask carries both halves stacked - so
        // width is the measurement to take, and it is the same either way.
        let source = if icon.hbmColor.is_invalid() {
            icon.hbmMask
        } else {
            icon.hbmColor
        };
        let mut bitmap = BITMAP::default();
        // SAFETY: source is a live bitmap handle and bitmap is storage of the size given.
        let written = unsafe {
            GetObjectW(
                source,
                std::mem::size_of::<BITMAP>() as i32,
                Some(std::ptr::addr_of_mut!(bitmap).cast()),
            )
        };
        // SAFETY: both handles came from GetIconInfo, which passes ownership to the caller, and
        // an invalid one is simply not deleted.
        unsafe {
            if !icon.hbmColor.is_invalid() {
                let _ = DeleteObject(icon.hbmColor);
            }
            if !icon.hbmMask.is_invalid() {
                let _ = DeleteObject(icon.hbmMask);
            }
        }
        if written == 0 || bitmap.bmWidth <= 0 {
            return None;
        }
        let scale = f64::from(size) / f64::from(bitmap.bmWidth);
        Some((
            (f64::from(icon.xHotspot) * scale).round() as i32,
            (f64::from(icon.yHotspot) * scale).round() as i32,
        ))
    }

    /// The silhouette of the cursor Windows currently has on screen, rendered by GDI at `size`.
    ///
    /// Independent of DXGI by construction: `DrawIconEx` draws the logical cursor, which has no
    /// orientation of its own, so this is the same picture whichever way the panel is turned.
    ///
    /// Drawn twice, once over white and once over black, because a cursor is not simply an image
    /// with alpha - the mask formats invert what is underneath, and there is no single background
    /// that reveals them. A pixel the cursor covers comes out the same over both backgrounds; one
    /// it leaves alone comes out white and then black.
    fn gdi_cursor_ink(size: u32) -> Option<Vec<bool>> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, PatBlt, SelectObject,
            BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLACKNESS, DIB_RGB_COLORS, HDC, WHITENESS,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            DrawIconEx, GetCursorInfo, CURSORINFO, CURSORINFO_FLAGS, DI_NORMAL, HICON,
        };

        let mut cursor = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: cursor is writable storage carrying its own size.
        unsafe { GetCursorInfo(&mut cursor) }.ok()?;
        const CURSOR_SHOWING: CURSORINFO_FLAGS = CURSORINFO_FLAGS(1);
        if cursor.flags != CURSOR_SHOWING {
            return None;
        }
        let icon = HICON(cursor.hCursor.0);

        // SAFETY: a null DC asks for one compatible with the screen.
        let dc = unsafe { CreateCompatibleDC(HDC(0)) };
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                // Negative for a top-down bitmap, so its rows are in the order every other buffer
                // here is in.
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: info describes the section being asked for and bits receives its address.
        let bitmap =
            unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, HANDLE(0), 0) }.ok()?;
        // SAFETY: both handles are live and were just created.
        let previous = unsafe { SelectObject(dc, bitmap) };

        let side = size as i32;
        let pixels = size as usize * size as usize * 4;
        let draw_over = |fill| {
            // SAFETY: the DC holds the section, and the rectangle is the whole of it.
            unsafe { PatBlt(dc, 0, 0, side, side, fill) };
            // SAFETY: as above; the icon handle belongs to the system cursor and is not owned here.
            let _ = unsafe { DrawIconEx(dc, 0, 0, icon, side, side, 0, None, DI_NORMAL) };
            // SAFETY: CreateDIBSection returned a buffer of exactly this size, and the drawing
            // above has completed into it.
            unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), pixels) }.to_vec()
        };
        let over_white = draw_over(WHITENESS);
        let over_black = draw_over(BLACKNESS);

        // SAFETY: restoring and releasing the objects this function created.
        unsafe {
            SelectObject(dc, previous);
            let _ = DeleteObject(bitmap);
            let _ = DeleteDC(dc);
        }

        Some(
            (0..size as usize * size as usize)
                .map(|index| {
                    let start = index * 4;
                    over_white[start..start + 3] == over_black[start..start + 3]
                })
                .collect(),
        )
    }

    /// How well `candidate` overlaps `reference` at each quarter turn, as intersection over union.
    ///
    /// Union rather than a plain pixel count, so a shape that simply covers more does not score
    /// better than one that covers the same pixels.
    fn quarter_turn_overlaps(reference: &[bool], candidate: &[bool], size: usize) -> [f64; 4] {
        let mut turned = candidate.to_vec();
        let mut overlaps = [0.0; 4];
        for overlap in &mut overlaps {
            let mut intersection = 0_u32;
            let mut union = 0_u32;
            for index in 0..size * size {
                if reference[index] && turned[index] {
                    intersection += 1;
                }
                if reference[index] || turned[index] {
                    union += 1;
                }
            }
            *overlap = if union == 0 {
                0.0
            } else {
                f64::from(intersection) / f64::from(union)
            };
            turned = quarter_turn(&turned, size);
        }
        overlaps
    }

    /// One quarter turn clockwise of a square grid.
    fn quarter_turn(grid: &[bool], size: usize) -> Vec<bool> {
        let mut turned = vec![false; size * size];
        for y in 0..size {
            for x in 0..size {
                turned[y * size + x] = grid[(size - 1 - x) * size + y];
            }
        }
        turned
    }

    /// The shape comparison the rotated-display test leans on has to be able to *fail*.
    ///
    /// Every quarter turn scoring zero is the right answer for an arrow and also exactly what a
    /// broken rotation that produced an empty grid would report, so the two are told apart here,
    /// with no desktop involved: a silhouette that arrived turned must score best at the turn that
    /// undoes it and worse than that where it started.
    #[test]
    fn a_silhouette_that_arrived_turned_scores_best_where_it_is_undone() {
        // An L, which shares no pixel with any of its own quarter turns except the corner.
        let size = 8;
        let mut upright = vec![false; size * size];
        for y in 1..6 {
            upright[y * size + 1] = true;
        }
        for x in 1..5 {
            upright[5 * size + x] = true;
        }

        let overlaps = quarter_turn_overlaps(&upright, &upright, size);
        assert_eq!(overlaps[0], 1.0, "a silhouette must match itself exactly");
        for (turn, overlap) in overlaps.iter().enumerate().skip(1) {
            assert!(
                *overlap < 0.2,
                "the L overlaps its own {}-degree turn by {overlap}, so it cannot serve as an              asymmetric fixture",
                turn * 90
            );
        }

        // What a shape delivered already rotated looks like: undone by three further turns, and
        // conspicuously wrong where it arrived.
        let turned = quarter_turn(&upright, size);
        let overlaps = quarter_turn_overlaps(&upright, &turned, size);
        assert_eq!(overlaps[3], 1.0, "three more turns must put the L back");
        assert!(
            overlaps[0] < overlaps[3],
            "a turned silhouette scored {} where it arrived and {} once undone, so the comparison              cannot tell the two apart",
            overlaps[0],
            overlaps[3]
        );
    }

    /// The rotated-display test claims a particular wrong answer - the position read as if it were
    /// in the duplicated surface's coordinates - and that claim is only worth making if it matches
    /// what actually happens to the pixels.
    ///
    /// So `upright_from_raw` is checked against `normalize_bgra_into` itself rather than against a
    /// second copy of the same arithmetic: every source pixel is labelled, the frame is normalized,
    /// and the label has to turn up exactly where the mapping says it will.
    #[test]
    fn the_transposed_reading_is_the_one_the_pixels_actually_get() {
        let (raw_width, raw_height, stride) = (3_u32, 2_u32, 16_usize);
        let raw = labeled_bgra(raw_width, raw_height, stride);
        for rotation in [0_u16, 90, 180, 270] {
            let (width, height) = dimensions_after_rotation(raw_width, raw_height, rotation);
            let mut normalized = vec![0_u8; (width * height * 4) as usize];
            let layout = normalize_bgra_into(
                &raw,
                raw_width,
                raw_height,
                stride,
                rotation,
                &mut normalized,
            )
            .expect("rotation normalization");
            for source_y in 0..raw_height as i32 {
                for source_x in 0..raw_width as i32 {
                    let label = raw[source_y as usize * stride + source_x as usize * 4];
                    let (x, y) = upright_from_raw(
                        (source_x, source_y),
                        raw_width as i32,
                        raw_height as i32,
                        rotation,
                    );
                    let found = normalized[y as usize * layout.stride as usize + x as usize * 4];
                    assert_eq!(
                        found, label,
                        "at {rotation} degrees the pixel at raw ({source_x},{source_y}) was              mapped to ({x},{y}), where the frame holds {found} rather than {label}"
                    );
                }
            }
        }
    }

    /// A retained frame must keep the pointer it was retained with, even though the acquisition
    /// that produced it carried no mouse update.
    ///
    /// `latest` is the mode the resident daemon runs, and it composites from a retained frame. The
    /// frame is retained by whichever acquisition happened to find a repaint pending, and a repaint
    /// is not a pointer move - so the raw `PointerPosition` on that acquisition is at its defaults,
    /// which read as an invisible pointer at the origin. Reading them there recorded "no pointer"
    /// on a frame whose pixels were captured with the pointer plainly on screen, and the capture
    /// came back `Absent { NotVisible }` while the cache held the position all along.
    ///
    /// The pointer is parked once and then never touched, which is the case that used to fail; the
    /// printing keeps the console repainting so frames keep being retained.
    ///
    /// cargo test --locked -p captastic-windows --release
    ///     -- --ignored --nocapture a_retained_frame_keeps_the_pointer_across_a_repaint
    #[test]
    #[ignore = "requires an interactive desktop with a visible pointer"]
    fn a_retained_frame_keeps_the_pointer_across_a_repaint() {
        use std::time::Instant;

        let _pointer = pointer_lock();

        make_process_dpi_aware();
        let mut backend = DxgiBackend::new_primary().expect("dxgi backend");
        let bounds = backend.selected.bounds;
        let parked = (
            bounds.x + bounds.width as i32 / 2,
            bounds.y + bounds.height as i32 / 2,
        );
        let mut recorder = EventRecorder::with_capacity(16);
        let request = || CaptureRequest {
            id: CaptureId(1),
            triggered_at: Instant::now(),
            source: CaptureSource::Display(DisplayId::primary()),
            mode: CaptureMode::Latest { max_age_ms: None },
            cpu_frame: true,
            retain_native_frame: false,
            cursor: CursorMode::Include,
        };

        // Warm up until composition is working at all, so what follows measures retention rather
        // than a duplication that has not yet been told what the pointer looks like.
        let mut ready = false;
        for _ in 0..20 {
            nudge_pointer_at(parked.0, parked.1);
            let outcome = backend.capture(&request(), &mut recorder).expect("capture");
            let cursor = outcome.frame.expect("cpu frame").metadata.cursor;
            if matches!(cursor, Some(CursorCapture::Composited { .. })) {
                ready = true;
                break;
            }
        }
        assert!(
            ready,
            "composition never worked even with the pointer moving, so this test cannot say             anything about retention; is the pointer on the primary display?"
        );

        // From here the pointer is not touched again.
        let mut composited = 0_u32;
        let mut absences = Vec::new();
        for round in 0..40 {
            println!("round {round}: keeping the console repainting, pointer untouched");
            std::thread::sleep(std::time::Duration::from_millis(25));
            let outcome = backend.capture(&request(), &mut recorder).expect("capture");
            let frame = outcome.frame.expect("cpu frame");
            match frame.metadata.cursor {
                Some(CursorCapture::Composited { .. }) => composited += 1,
                other => absences.push(format!("{other:?}")),
            }
        }
        assert_eq!(
            pointer_position(),
            parked,
            "something moved the pointer during the measurement, so its result means nothing"
        );
        println!("{composited} of 40 captures composited");
        assert!(
            absences.is_empty(),
            "the pointer never moved and its position was known throughout, yet {} of 40 retained             frames reported no pointer: {absences:?}",
            absences.len()
        );
    }

    /// The acceptance test for ADR 0003's amendment: `fresh` succeeds on a desktop that has not
    /// changed, by proving the retained frame is what the screen shows.
    ///
    /// A zero millisecond budget expires before the loop runs, so the fallback is reached on every
    /// attempt. Whether it can *verify* depends on nothing being presented in the microseconds
    /// between attempts, which is why this retries: a busy desktop simply needs a few tries to have
    /// a quiet gap, and an idle one succeeds first time.
    ///
    /// cargo test --locked -p captastic-windows --release
    ///     -- --ignored --nocapture fresh_falls_back_to_a_frame_proven_current
    #[test]
    #[ignore = "requires a real desktop and a working duplication"]
    fn fresh_falls_back_to_a_frame_proven_current() {
        use std::time::Instant;

        let mut backend = DxgiBackend::new_primary().expect("dxgi backend");
        let mut recorder = EventRecorder::with_capacity(16);
        let request = |timeout_ms| CaptureRequest {
            id: CaptureId(1),
            triggered_at: Instant::now(),
            source: CaptureSource::Display(DisplayId::primary()),
            mode: CaptureMode::Fresh { timeout_ms },
            cpu_frame: true,
            retain_native_frame: false,
            cursor: CursorMode::Exclude,
        };

        // Prime the retained frame, with a budget long enough to find one.
        backend
            .capture(&request(2_000), &mut recorder)
            .expect("an initial frame");

        for attempt in 1..=20 {
            let outcome = match backend.capture(&request(0), &mut recorder) {
                Ok(outcome) => outcome,
                // A frame was pending, so the desktop is not static and a zero budget genuinely
                // failed. Correct, and worth another attempt.
                Err(error) if error.kind == CaptureErrorKind::Timeout => continue,
                Err(error) => panic!("unexpected failure: {error}"),
            };
            let metadata = outcome.metadata;
            let Some(verified) = metadata.verified_current_offset_ns else {
                continue;
            };
            println!(
                "attempt {attempt}: fresh with a 0 ms budget returned a frame presented {:.1} ms before the trigger, verified current {:.3} ms after it",
                metadata.frame_age_ns.unwrap_or(0) as f64 / 1_000_000.0,
                verified as f64 / 1_000_000.0
            );
            assert!(
                outcome.frame.is_some(),
                "the fallback must return the retained frame, not merely describe it"
            );
            return;
        }
        panic!("no attempt could prove the retained frame current; is the desktop repainting continuously?");
    }

    #[test]
    fn a_verified_current_frame_is_never_too_stale() {
        let thirty_seconds = 30_000_000_000;

        // The case that motivated the change: an opt-in staleness limit refusing a frame that a
        // probe had just proven identical to the screen.
        assert!(!frame_is_too_stale(Some(100), thirty_seconds, true));
        // The same frame without that proof is exactly as stale as it looks.
        assert!(frame_is_too_stale(Some(100), thirty_seconds, false));
        // No limit asked for, nothing to fail.
        assert!(!frame_is_too_stale(None, thirty_seconds, false));
        // A limit that the age satisfies on its own needs no verification to pass.
        assert!(!frame_is_too_stale(Some(100), 50_000_000, false));
        // Exactly at the limit is inside it.
        assert!(!frame_is_too_stale(Some(100), 100_000_000, false));
        assert!(frame_is_too_stale(Some(100), 100_000_001, false));
    }

    #[test]
    fn an_hdr_desktop_format_explains_itself_rather_than_reporting_a_number() {
        let message = describe_unsupported_format(DXGI_FORMAT_R16G16B16A16_FLOAT);

        // The number stays, because a bug report needs it; what changes is that it is no longer
        // the entire message. A user who switched HDR on in Settings can connect the two now.
        assert!(message.contains("10"), "{message}");
        assert!(message.contains("R16G16B16A16_FLOAT"), "{message}");
        assert!(message.contains("HDR"), "{message}");
        assert!(
            message.contains("Turning HDR off"),
            "the message should say what would fix it: {message}"
        );
    }

    #[test]
    fn an_unrecognized_desktop_format_still_names_its_number() {
        // No HDR explanation attached to a format that has nothing to do with HDR, which would be
        // a confident answer to a question nobody asked.
        let message = describe_unsupported_format(DXGI_FORMAT_R10G10B10A2_UNORM);
        assert!(message.contains("R10G10B10A2_UNORM"), "{message}");
        assert!(!message.contains("HDR"), "{message}");
    }

    #[test]
    fn rotation_normalization_orients_pixels_and_removes_row_padding() {
        let raw = labeled_bgra(3, 2, 16);
        let cases = [
            (0, 3, 2, vec![1, 2, 3, 4, 5, 6]),
            (90, 2, 3, vec![4, 1, 5, 2, 6, 3]),
            (180, 3, 2, vec![6, 5, 4, 3, 2, 1]),
            (270, 2, 3, vec![3, 6, 2, 5, 1, 4]),
        ];

        for (rotation, expected_width, expected_height, expected_labels) in cases {
            let mut normalized = vec![0; expected_width as usize * expected_height as usize * 4];
            let layout = normalize_bgra_into(&raw, 3, 2, 16, rotation, &mut normalized)
                .expect("rotation normalization");
            assert_eq!(layout.width, expected_width);
            assert_eq!(layout.height, expected_height);
            assert_eq!(layout.stride, expected_width * 4);
            assert_eq!(blue_channel(&normalized), expected_labels);
        }
    }

    #[test]
    fn normalized_regions_map_back_to_raw_texture_coordinates() {
        let selection = Rect {
            x: 100,
            y: 200,
            width: 300,
            height: 400,
        };
        let cases = [
            (
                0,
                Rect {
                    x: 100,
                    y: 200,
                    width: 300,
                    height: 400,
                },
            ),
            (
                90,
                Rect {
                    x: 200,
                    y: 680,
                    width: 400,
                    height: 300,
                },
            ),
            (
                180,
                Rect {
                    x: 1520,
                    y: 480,
                    width: 300,
                    height: 400,
                },
            ),
            (
                270,
                Rect {
                    x: 1320,
                    y: 100,
                    width: 400,
                    height: 300,
                },
            ),
        ];

        for (rotation, expected) in cases {
            assert_eq!(
                raw_selection_for_rotation(selection, 1920, 1080, rotation)
                    .expect("valid rotated selection"),
                expected
            );
        }
    }

    #[test]
    fn normalizing_a_raw_region_matches_cropping_the_normalized_frame() {
        let raw_width = 4;
        let raw_height = 3;
        let raw_stride = raw_width as usize * 4;
        let raw = labeled_bgra(raw_width, raw_height, raw_stride);
        let raw_selection = Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };

        for rotation in [0, 90, 180, 270] {
            let full_layout = normalized_layout(raw_width, raw_height, rotation)
                .expect("normalized full-frame layout");
            let mut full = vec![0; frame_byte_len(full_layout.width, full_layout.height).unwrap()];
            normalize_bgra_into(&raw, raw_width, raw_height, raw_stride, rotation, &mut full)
                .expect("full-frame normalization");

            let normalized_selection = match rotation {
                0 => raw_selection,
                90 => Rect {
                    x: 0,
                    y: 1,
                    width: 2,
                    height: 2,
                },
                180 => Rect {
                    x: 1,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                270 => Rect {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                },
                _ => unreachable!(),
            };
            assert_eq!(
                raw_selection_for_rotation(normalized_selection, raw_width, raw_height, rotation,)
                    .expect("normalized selection mapping"),
                raw_selection
            );

            let raw_x = raw_selection.x as usize;
            let raw_y = raw_selection.y as usize;
            let selected_stride = raw_selection.width as usize * 4;
            let mut selected_raw = vec![0; selected_stride * raw_selection.height as usize];
            for row in 0..raw_selection.height as usize {
                let source_start = (raw_y + row) * raw_stride + raw_x * 4;
                let destination_start = row * selected_stride;
                selected_raw[destination_start..destination_start + selected_stride]
                    .copy_from_slice(&raw[source_start..source_start + selected_stride]);
            }
            let mut normalized_region =
                vec![
                    0;
                    frame_byte_len(normalized_selection.width, normalized_selection.height,)
                        .unwrap()
                ];
            normalize_bgra_into(
                &selected_raw,
                raw_selection.width,
                raw_selection.height,
                selected_stride,
                rotation,
                &mut normalized_region,
            )
            .expect("selected-region normalization");

            let full_stride = full_layout.stride as usize;
            let crop_stride = normalized_selection.width as usize * 4;
            let mut cropped = Vec::with_capacity(normalized_region.len());
            for row in 0..normalized_selection.height as usize {
                let start = (normalized_selection.y as usize + row) * full_stride
                    + normalized_selection.x as usize * 4;
                cropped.extend_from_slice(&full[start..start + crop_stride]);
            }
            assert_eq!(normalized_region, cropped, "rotation {rotation}");
        }
    }

    /// Properties of rotation normalization and of the region mapping that reads back through it.
    ///
    /// Both are hand-derived index arithmetic across four rotations, written in opposite
    /// directions: one moves pixels from the rotated texture into the upright frame, the other
    /// moves a rectangle from the upright frame back into the rotated texture. The example tests
    /// above check one 3x2 and one 4x3 frame with one selection each, which is exactly the size
    /// where an off-by-one in a `- 1 -` term can hide.
    mod properties {
        use super::*;
        use proptest::prelude::*;

        fn any_rotation() -> impl Strategy<Value = u16> {
            prop::sample::select(vec![0u16, 90, 180, 270])
        }

        /// The rotation that undoes `rotation`.
        fn inverse_of(rotation: u16) -> u16 {
            (360 - rotation) % 360
        }

        fn normalize(
            raw: &[u8],
            width: u32,
            height: u32,
            stride: usize,
            rotation: u16,
        ) -> (Vec<u8>, PixelLayout) {
            let layout =
                normalized_layout(width, height, rotation).expect("generated dimensions are valid");
            let mut normalized =
                vec![0; frame_byte_len(layout.width, layout.height).expect("frame fits in memory")];
            normalize_bgra_into(raw, width, height, stride, rotation, &mut normalized)
                .expect("generated frames satisfy the normalization contract");
            (normalized, layout)
        }

        /// Picks `start` and `length` inside `extent`, from two arbitrary seeds.
        fn span_within(extent: u32, start_seed: u32, length_seed: u32) -> (u32, u32) {
            let start = start_seed % extent;
            let length = 1 + length_seed % (extent - start);
            (start, length)
        }

        proptest! {
            /// Rotating a frame and rotating it back returns the frame. This is the one property
            /// here with no reference implementation to be wrong in the same way: it compares the
            /// transform against its own inverse, so a mirrored axis or a dropped `- 1` cannot
            /// cancel out.
            #[test]
            fn rotating_a_frame_back_restores_it(
                width in 1u32..=12,
                height in 1u32..=12,
                padding in 0usize..=8,
                rotation in any_rotation(),
            ) {
                // Row padding of a mapped DXGI texture, which normalization also has to remove.
                let stride = width as usize * 4 + padding;
                let raw = labeled_bgra(width, height, stride);

                let (rotated, layout) = normalize(&raw, width, height, stride, rotation);
                let (restored, restored_layout) = normalize(
                    &rotated,
                    layout.width,
                    layout.height,
                    layout.stride as usize,
                    inverse_of(rotation),
                );

                prop_assert_eq!(restored_layout.width, width);
                prop_assert_eq!(restored_layout.height, height);
                // Compared against a tightly packed original: the padding is gone after the first
                // pass, and its removal is part of what is being checked.
                let packed = labeled_bgra(width, height, width as usize * 4);
                prop_assert_eq!(blue_channel(&restored), blue_channel(&packed));
            }

            /// Reading one region back through the rotation must produce the same pixels as
            /// normalizing the whole frame and cropping it. This is the promise the GPU-region
            /// readback fast path makes: it exists to avoid normalizing pixels nobody asked for,
            /// and it is only sound while the shortcut is invisible in the result.
            #[test]
            fn a_region_read_through_the_rotation_matches_cropping_the_whole_frame(
                raw_width in 1u32..=10,
                raw_height in 1u32..=10,
                x_seed in 0u32..1_000,
                y_seed in 0u32..1_000,
                width_seed in 0u32..1_000,
                height_seed in 0u32..1_000,
                rotation in any_rotation(),
            ) {
                let raw_stride = raw_width as usize * 4;
                let raw = labeled_bgra(raw_width, raw_height, raw_stride);
                let (full, layout) = normalize(&raw, raw_width, raw_height, raw_stride, rotation);

                let (x, width) = span_within(layout.width, x_seed, width_seed);
                let (y, height) = span_within(layout.height, y_seed, height_seed);
                let selection = Rect {
                    x: x as i32,
                    y: y as i32,
                    width,
                    height,
                };

                let raw_selection =
                    raw_selection_for_rotation(selection, raw_width, raw_height, rotation)
                        .expect("a selection inside the normalized frame maps back");

                // Whatever the rotation, the region has to land inside the texture it will be
                // copied from, and cover the same number of pixels it did upright.
                prop_assert!(raw_selection.x >= 0 && raw_selection.y >= 0);
                prop_assert!(raw_selection.right() <= i64::from(raw_width));
                prop_assert!(raw_selection.bottom() <= i64::from(raw_height));
                prop_assert_eq!(raw_selection.area(), selection.area());

                // What the readback does: copy the sub-rectangle out of the rotated texture, then
                // normalize only that.
                let selected_stride = raw_selection.width as usize * 4;
                let mut selected_raw = vec![0; selected_stride * raw_selection.height as usize];
                for row in 0..raw_selection.height as usize {
                    let source = (raw_selection.y as usize + row) * raw_stride
                        + raw_selection.x as usize * 4;
                    let destination = row * selected_stride;
                    selected_raw[destination..destination + selected_stride]
                        .copy_from_slice(&raw[source..source + selected_stride]);
                }
                let (region, region_layout) = normalize(
                    &selected_raw,
                    raw_selection.width,
                    raw_selection.height,
                    selected_stride,
                    rotation,
                );
                prop_assert_eq!(region_layout.width, selection.width);
                prop_assert_eq!(region_layout.height, selection.height);

                // What it must equal: the same rectangle cut out of the fully normalized frame.
                let full_stride = layout.stride as usize;
                let crop_stride = selection.width as usize * 4;
                let mut cropped = Vec::with_capacity(region.len());
                for row in 0..selection.height as usize {
                    let start = (y as usize + row) * full_stride + x as usize * 4;
                    cropped.extend_from_slice(&full[start..start + crop_stride]);
                }
                prop_assert_eq!(blue_channel(&region), blue_channel(&cropped));
            }
        }
    }
}

#[cfg(test)]
mod identity_probe {
    use super::*;

    /// Dumps the raw identity material Windows reports for every active display path.
    ///
    /// Temporary diagnostic for issue #60: the derived id is only as stable as its inputs, and
    /// this is the only way to see which of them Windows is actually populating right now.
    ///
    /// cargo test --locked -p captastic-windows -- --ignored --nocapture raw_display_identity
    #[test]
    #[ignore = "diagnostic; prints live display-config material"]
    fn raw_display_identity_material() {
        let mut path_count = 0_u32;
        let mut mode_count = 0_u32;
        // SAFETY: Both counts are valid writable values and the flag requests active paths only.
        unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        }
        .expect("buffer sizes");
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: The arrays have the capacities reported immediately above, and the counts
        // describe their current lengths for Windows to update.
        unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        }
        .expect("query display config");
        paths.truncate(path_count as usize);
        for path in paths {
            let source = display_config_source_name(&path).expect("source name");
            let target = display_config_target_name(&path).expect("target name");
            println!("source gdi name       : {source}");
            println!(
                "friendly name         : {:?}",
                wide_array_to_string(&target.monitorFriendlyDeviceName)
            );
            println!(
                "monitor device path   : {:?}",
                wide_array_to_string(&target.monitorDevicePath)
            );
            println!(
                "edidManufactureId     : 0x{:04X} (swapped 0x{:04X}) -> {:?}",
                target.edidManufactureId,
                target.edidManufactureId.swap_bytes(),
                edid_manufacturer(target.edidManufactureId)
            );
            println!("edidProductCodeId     : 0x{:04X}", target.edidProductCodeId);
            println!("outputTechnology      : {:?}", target.outputTechnology.0);
            println!("connectorInstance     : {}", target.connectorInstance);
            println!(
                "derived id            : {}",
                display_identity(&target, &source).0
            );
        }
    }
}
