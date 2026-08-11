use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use captastic_core::{
    BackendCapabilities, CaptureBackend, CaptureError, CaptureErrorKind, CaptureMode,
    CaptureOutcome, CaptureRequest, CaptureSource, ColorSpace, CpuFrame, CursorMode, DisplayId,
    DisplayInfo, EventRecorder, FrameMetadata, FrameOrigin, NativeFrame, PerfEventKind,
    PixelFormat, Rect, TimingProvenance,
};
use windows::core::{ComInterface, Error as WindowsError};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
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
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE180,
    DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ADAPTER_DESC1, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTPUT_DESC,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

const INITIAL_LATEST_FRAME_TIMEOUT: Duration = Duration::from_millis(100);
const GPU_MAP_TIMEOUT: Duration = Duration::from_millis(250);
const GPU_MAP_RETRY_DELAY: Duration = Duration::from_millis(1);

pub fn enumerate_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    enumerate_outputs().map(|outputs| outputs.into_iter().map(|output| output.info).collect())
}

pub struct DxgiBackend {
    _com: ComApartment,
    device: ID3D11Device,
    context: Arc<Mutex<ID3D11DeviceContext>>,
    duplication: IDXGIOutputDuplication,
    retained: RetainedTexture,
    latest: Option<RetainedFrame>,
    staging: Option<StagingTexture>,
    cpu_pool: CpuBufferPool,
    displays: Vec<DisplayInfo>,
    selected: DisplayInfo,
    capabilities: BackendCapabilities,
    qpc_frequency: i64,
    _thread_affine: PhantomData<Rc<()>>,
}

impl DxgiBackend {
    pub fn new_primary() -> Result<Self, CaptureError> {
        Self::new(&DisplayId::primary())
    }

    pub fn new(display_id: &DisplayId) -> Result<Self, CaptureError> {
        let com = ComApartment::initialize()?;
        let outputs = enumerate_outputs()?;
        let displays: Vec<_> = outputs.iter().map(|output| output.info.clone()).collect();
        let selected_output = select_display_index(&displays, display_id).ok_or_else(|| {
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
        .map_err(|error| map_windows_error("create_d3d11_device", error))?;
        let device = device.ok_or_else(|| {
            capture_error(
                CaptureErrorKind::NativeFailure,
                "create_d3d11_device",
                "D3D11CreateDevice returned no device",
                false,
                None,
            )
        })?;
        let context = context.ok_or_else(|| {
            capture_error(
                CaptureErrorKind::NativeFailure,
                "create_d3d11_device",
                "D3D11CreateDevice returned no immediate context",
                false,
                None,
            )
        })?;
        // SAFETY: The output and device belong to the same enumerated adapter and remain alive.
        let duplication = unsafe { selected_record.output.DuplicateOutput(&device) }
            .map_err(|error| map_windows_error("duplicate_output", error))?;
        let qpc_frequency = query_performance_frequency()?;
        let staging = None;
        let retained = RetainedTexture::create(
            &device,
            retained_desc(
                selected_record.info.bounds.width,
                selected_record.info.bounds.height,
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
                cursor_control: false,
                hdr: false,
                presentation_time: true,
                warm_stream: false,
            },
            qpc_frequency,
            _thread_affine: PhantomData,
        };
        Ok(backend)
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

    fn capture(
        &mut self,
        request: &CaptureRequest,
        recorder: &mut EventRecorder,
    ) -> Result<CaptureOutcome, CaptureError> {
        if request.cursor == CursorMode::Include {
            return Err(capture_error(
                CaptureErrorKind::Unsupported,
                "capture",
                "cursor composition is not implemented in the native-frame milestone",
                false,
                None,
            ));
        }
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
                return Err(capture_error(
                    CaptureErrorKind::Timeout,
                    "acquire_next_frame",
                    "no post-trigger desktop frame arrived before the timeout",
                    true,
                    Some(i64::from(DXGI_ERROR_WAIT_TIMEOUT.0)),
                ));
            }
            let timeout_ms = duration_to_timeout_ms(remaining);
            let acquired = AcquiredFrame::acquire(&self.duplication, timeout_ms)?;
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
            let frame_generation =
                self.retain_frame(&texture, texture_desc, acquired.info.LastPresentTime)?;
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
                frame_generation,
                copy_count: 1,
                pool_slot: None,
            };
            let native_texture = if request.retain_native_frame {
                metadata.copy_count = metadata.copy_count.saturating_add(1);
                Some(self.snapshot_texture(&texture, texture_desc)?)
            } else {
                None
            };
            let cpu_frame = if request.cpu_frame {
                if self.selected.rotation_degrees != 0 {
                    return Err(capture_error(
                        CaptureErrorKind::Unsupported,
                        "readback",
                        "rotated-output normalization is not implemented yet",
                        false,
                        None,
                    ));
                }
                Some(self.readback(
                    &texture,
                    texture_desc,
                    request.triggered_at,
                    &mut metadata,
                    recorder,
                )?)
            } else {
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
            acquired.release()?;
            return Ok(CaptureOutcome {
                metadata,
                frame: cpu_frame,
                native_frame,
                backend_duration: request.triggered_at.elapsed(),
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
        self.refresh_latest_on_demand()?;
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
        if max_age_ms.is_some_and(|maximum| frame_age_ns > maximum.saturating_mul(1_000_000)) {
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
            frame_generation: Some(latest.generation),
            copy_count: 1,
            pool_slot: None,
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
            if self.selected.rotation_degrees != 0 {
                return Err(capture_error(
                    CaptureErrorKind::Unsupported,
                    "readback",
                    "rotated-output normalization is not implemented yet",
                    false,
                    None,
                ));
            }
            Some(self.readback(
                &retained_texture,
                retained_desc,
                request.triggered_at,
                &mut metadata,
                recorder,
            )?)
        } else {
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
            backend_duration: request.triggered_at.elapsed(),
        })
    }

    fn refresh_latest_on_demand(&mut self) -> Result<(), CaptureError> {
        match self.refresh_latest(0) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if error.kind == CaptureErrorKind::Timeout => {}
            Err(error) => return Err(error),
        }
        if self.latest.is_some() {
            return Ok(());
        }

        let deadline = Instant::now() + INITIAL_LATEST_FRAME_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(capture_error(
                    CaptureErrorKind::Timeout,
                    "capture_latest",
                    "no desktop frame was available before the initial capture timeout",
                    true,
                    Some(i64::from(DXGI_ERROR_WAIT_TIMEOUT.0)),
                ));
            }
            match self.refresh_latest(duration_to_timeout_ms(remaining)) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn refresh_latest(&mut self, timeout_ms: u32) -> Result<bool, CaptureError> {
        let acquired = AcquiredFrame::acquire(&self.duplication, timeout_ms)?;
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
        self.retain_frame(&texture, desc, acquired.info.LastPresentTime)?;
        acquired.release()?;
        Ok(true)
    }

    fn retain_frame(
        &mut self,
        source: &ID3D11Texture2D,
        source_desc: D3D11_TEXTURE2D_DESC,
        presentation_qpc: i64,
    ) -> Result<Option<u64>, CaptureError> {
        if presentation_qpc == 0 {
            return Ok(None);
        }
        if self.retained.desc.Width != source_desc.Width
            || self.retained.desc.Height != source_desc.Height
            || self.retained.desc.Format != source_desc.Format
        {
            self.retained = RetainedTexture::create(
                &self.device,
                retained_desc(
                    source_desc.Width,
                    source_desc.Height,
                    source_desc.Format,
                    source_desc.SampleDesc,
                ),
            )?;
            self.selected.bounds.width = source_desc.Width;
            self.selected.bounds.height = source_desc.Height;
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
        });
        Ok(Some(generation))
    }

    fn readback(
        &mut self,
        source: &ID3D11Texture2D,
        source_desc: D3D11_TEXTURE2D_DESC,
        triggered_at: Instant,
        metadata: &mut FrameMetadata,
        recorder: &mut EventRecorder,
    ) -> Result<CpuFrame, CaptureError> {
        if source_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(capture_error(
                CaptureErrorKind::Unsupported,
                "readback",
                format!("unsupported DXGI desktop format {}", source_desc.Format.0),
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
        let tight_stride = source_desc.Width.checked_mul(4).ok_or_else(|| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "readback",
                "CPU frame stride overflowed",
                false,
                None,
            )
        })?;
        if mapped.data.RowPitch < tight_stride {
            return Err(capture_error(
                CaptureErrorKind::InvalidFrame,
                "readback",
                format!(
                    "mapped row pitch {} is smaller than required {}",
                    mapped.data.RowPitch, tight_stride
                ),
                false,
                None,
            ));
        }
        let len = frame_byte_len(source_desc.Width, source_desc.Height)?;
        let slot_index = self.cpu_pool.available_index(len).ok_or_else(|| {
            capture_error(
                CaptureErrorKind::BufferExhausted,
                "readback",
                "all preallocated CPU frame slots are still in use",
                true,
                None,
            )
        })?;
        let tight_stride_usize = tight_stride as usize;
        let source_stride = mapped.data.RowPitch as usize;
        {
            let pixels = Arc::get_mut(
                self.cpu_pool.slots[slot_index]
                    .as_mut()
                    .expect("available slot is initialized"),
            )
            .expect("available slot has exactly one owner");
            if source_stride == tight_stride_usize {
                // SAFETY: Equal source/destination strides make the complete mapped texture one
                // contiguous initialized byte range valid until the matching Unmap.
                let source_pixels =
                    unsafe { std::slice::from_raw_parts(mapped.data.pData.cast::<u8>(), len) };
                pixels.copy_from_slice(source_pixels);
            } else {
                for row in 0..source_desc.Height as usize {
                    let destination_start = row * tight_stride_usize;
                    let destination_end = destination_start + tight_stride_usize;
                    // SAFETY: D3D11 Map returned a non-null pointer valid until Unmap. RowPitch
                    // covers at least tight_stride bytes and row is within the mapped height.
                    let source_row = unsafe {
                        std::slice::from_raw_parts(
                            (mapped.data.pData as *const u8).add(row * source_stride),
                            tight_stride_usize,
                        )
                    };
                    pixels[destination_start..destination_end].copy_from_slice(source_row);
                }
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
            source_desc.Width,
            source_desc.Height,
            tight_stride,
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
                format!("unsupported DXGI desktop format {}", self.desc.Format.0),
                false,
                None,
            ));
        }
        let destination_desc = staging_desc(
            selection.width,
            selection.height,
            self.desc.Format,
            self.desc.SampleDesc,
        );
        let staging = StagingTexture::create(&self.device, destination_desc)?.texture;
        let local_x = u32::try_from(local.x).map_err(|_| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "selected region has a negative local x coordinate",
                false,
                None,
            )
        })?;
        let local_y = u32::try_from(local.y).map_err(|_| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "selected region has a negative local y coordinate",
                false,
                None,
            )
        })?;
        let source_box = D3D11_BOX {
            left: local_x,
            top: local_y,
            front: 0,
            right: local_x.saturating_add(local.width),
            bottom: local_y.saturating_add(local.height),
            back: 1,
        };
        let context = lock_context(&self.context)?;
        let copy_started = Instant::now();
        // SAFETY: The source box was checked against the immutable snapshot, and the destination
        // staging texture exactly matches its dimensions and format. Context access is serialized.
        unsafe {
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &self.texture, 0, Some(&source_box))
        };
        let gpu_copy_submit_ns = duration_ns_u64(copy_started.elapsed());
        let map_started = Instant::now();
        let mapped = MappedTexture::map(&context, &staging)?;
        let map_wait_ns = duration_ns_u64(map_started.elapsed());
        let tight_stride = selection.width.checked_mul(4).ok_or_else(|| {
            capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "selected frame stride overflowed",
                false,
                None,
            )
        })?;
        if mapped.data.RowPitch < tight_stride {
            return Err(capture_error(
                CaptureErrorKind::InvalidFrame,
                "gpu_region_readback",
                "mapped row pitch is smaller than the selected frame stride",
                false,
                None,
            ));
        }
        let bytes_read = frame_byte_len(selection.width, selection.height)?;
        let full_frame_bytes = frame_byte_len(self.desc.Width, self.desc.Height)?;
        let mut pixels = vec![0_u8; bytes_read];
        let cpu_copy_started = Instant::now();
        let source_stride = mapped.data.RowPitch as usize;
        let tight_stride_usize = tight_stride as usize;
        let contiguous_rows = source_stride == tight_stride_usize;
        if contiguous_rows {
            // SAFETY: Equal pitches make the mapped region a contiguous byte range valid until
            // the matching Unmap performed by `MappedTexture`.
            let source_pixels =
                unsafe { std::slice::from_raw_parts(mapped.data.pData.cast::<u8>(), bytes_read) };
            pixels.copy_from_slice(source_pixels);
        } else {
            for row in 0..selection.height as usize {
                let destination_start = row * tight_stride_usize;
                // SAFETY: RowPitch covers a complete row and `row` is inside the mapped height.
                let source_row = unsafe {
                    std::slice::from_raw_parts(
                        (mapped.data.pData as *const u8).add(row * source_stride),
                        tight_stride_usize,
                    )
                };
                pixels[destination_start..destination_start + tight_stride_usize]
                    .copy_from_slice(source_row);
            }
        }
        let cpu_copy_ns = duration_ns_u64(cpu_copy_started.elapsed());
        drop(mapped);
        drop(context);
        let mut metadata = self.metadata.clone();
        metadata.source_rect = selection;
        metadata.copy_count = metadata.copy_count.saturating_add(2);
        metadata.pool_slot = None;
        let frame = CpuFrame::new(
            Arc::from(pixels),
            selection.width,
            selection.height,
            tight_stride,
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

struct OutputRecord {
    adapter: IDXGIAdapter1,
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

fn enumerate_outputs() -> Result<Vec<OutputRecord>, CaptureError> {
    // SAFETY: The generic result is a supported DXGI factory interface and Windows initializes it.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| map_windows_error("create_dxgi_factory", error))?;
    let identities = match display_config_identities() {
        Ok(identities) => identities,
        Err(error) => {
            log::warn!(
                "persistent display identity query failed; using session-local output names: {error}"
            );
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
                    output: output1,
                    info: DisplayInfo {
                        id,
                        name,
                        bounds,
                        scale_factor: 1.0,
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

fn display_config_identities() -> Result<Vec<DisplayConfigIdentity>, CaptureError> {
    const MAX_ATTEMPTS: usize = 3;
    for attempt in 0..MAX_ATTEMPTS {
        let mut path_count = 0_u32;
        let mut mode_count = 0_u32;
        // SAFETY: Both counts are valid writable values and the query flag requests active paths.
        unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        }
        .map_err(|error| map_windows_error("display_config_buffer_sizes", error))?;
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
                    let monitor_path = wide_array_to_string(&target_name.monitorDevicePath);
                    let friendly_name =
                        wide_array_to_string(&target_name.monitorFriendlyDeviceName);
                    let identity_material = if monitor_path.is_empty() {
                        source_name.clone()
                    } else {
                        monitor_path
                    };
                    identities.push(DisplayConfigIdentity {
                        gdi_name: source_name,
                        persistent_id: persistent_display_id(&identity_material),
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
            Err(error) => return Err(map_windows_error("query_display_config", error)),
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
    use captastic_core::{CaptureId, CaptureSource, CursorMode};

    #[test]
    fn converts_qpc_ticks_without_losing_sign() {
        assert_eq!(qpc_to_ns(5, 10), 500_000_000);
        assert_eq!(qpc_to_ns(-5, 10), -500_000_000);
    }

    #[test]
    fn converts_wide_device_names() {
        assert_eq!(wide_array_to_string(&[65, 66, 0, 67]), "AB");
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
        assert_eq!(actual.frame.width, expected.width);
        assert_eq!(actual.frame.height, expected.height);
        assert_eq!(&*actual.frame.pixels, &*expected.pixels);
    }
}
