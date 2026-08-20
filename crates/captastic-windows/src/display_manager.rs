use std::sync::Arc;

use captastic_core::{
    BackendCapabilities, CaptureBackend, CaptureError, CaptureErrorKind, CaptureOutcome,
    CaptureRequest, CaptureSource, ColorSpace, CpuFrame, DisplayId, DisplayInfo, DisplayTopology,
    EventRecorder, FrameMetadata, FrameOrigin, PerfEventKind, PixelEncoding, PixelFormat, Rect,
    TimingProvenance,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetPhysicalCursorPos;

use crate::dxgi::{enumerate_display_adapters, DxgiBackend};

const COMPOSITE_BUFFER_SLOTS: usize = 3;
const MAX_COMPOSITE_FRAME_BYTES: usize = 512 * 1024 * 1024;
const OPAQUE_BLACK: [u8; 4] = [0, 0, 0, 0xff];

struct DisplaySession {
    display_id: DisplayId,
    backend: DxgiBackend,
}

/// Retains one initialized Desktop Duplication session for every usable output.
pub struct DxgiDisplayManager {
    displays: Vec<DisplayInfo>,
    sessions: Vec<DisplaySession>,
    unavailable: Vec<(DisplayId, String)>,
    capabilities: BackendCapabilities,
    virtual_bounds: Rect,
    virtual_topology_error: Option<CaptureError>,
    composite_pool: CompositeBufferPool,
}

impl DxgiDisplayManager {
    pub fn new() -> Result<Self, CaptureError> {
        let enumerated = enumerate_display_adapters()?;
        let displays: Vec<_> = enumerated
            .iter()
            .map(|(display, _)| display.clone())
            .collect();
        if displays.is_empty() {
            return Err(crate::dxgi::no_desktop_to_capture("initialize"));
        }
        let virtual_bounds = DisplayTopology::new(0, displays.clone())
            .map_err(|error| {
                manager_error(
                    CaptureErrorKind::Unsupported,
                    "initialize",
                    error.to_string(),
                    false,
                )
            })?
            .virtual_bounds()
            .ok_or_else(|| {
                manager_error(
                    CaptureErrorKind::Unsupported,
                    "initialize",
                    "virtual-desktop bounds overflow the physical-pixel coordinate range",
                    false,
                )
            })?;

        let mut sessions = Vec::with_capacity(displays.len());
        let mut unavailable = Vec::new();
        for display in &displays {
            match DxgiBackend::new(&display.id) {
                Ok(backend) => {
                    log::info!(
                        "retained DXGI session initialized display={} name={:?} bounds={}x{}{:+}{:+}",
                        display.id.0,
                        display.name,
                        display.bounds.width,
                        display.bounds.height,
                        display.bounds.x,
                        display.bounds.y
                    );
                    sessions.push(DisplaySession {
                        display_id: display.id.clone(),
                        backend,
                    });
                }
                Err(error) => {
                    log::warn!(
                        "DXGI session unavailable display={} name={:?}: {error}",
                        display.id.0,
                        display.name
                    );
                    unavailable.push((display.id.clone(), error.to_string()));
                }
            }
        }
        let mut capabilities = sessions
            .first()
            .map(|session| session.backend.capabilities().clone())
            .ok_or_else(|| {
                // Displays enumerated but not one of them would duplicate. A session that locked
                // between the two steps produces exactly that, and per-display errors listing
                // "Access is denied" for each is a long way of not saying so.
                if let Some(obstacle) = crate::dxgi::desktop_obstacle("initialize") {
                    return obstacle;
                }
                manager_error(
                    CaptureErrorKind::SourceUnavailable,
                    "initialize",
                    format!(
                        "no attached display could initialize Desktop Duplication: {}",
                        unavailable
                            .iter()
                            .map(|(id, error)| format!("{} ({error})", id.0))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    true,
                )
            })?;
        let virtual_topology_error = validate_virtual_topology(
            &enumerated
                .iter()
                .map(|(_, adapter)| *adapter)
                .collect::<Vec<_>>(),
            &unavailable
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>(),
            virtual_bounds,
        )
        .err();
        capabilities.virtual_desktop_capture = virtual_topology_error.is_none();
        Ok(Self {
            displays,
            sessions,
            unavailable,
            capabilities,
            virtual_bounds,
            virtual_topology_error,
            composite_pool: CompositeBufferPool::new(COMPOSITE_BUFFER_SLOTS),
        })
    }

    fn session_index(&self, requested: &DisplayId) -> Option<usize> {
        let resolved = resolve_display_id(&self.displays, requested)?;
        self.sessions
            .iter()
            .position(|session| session.display_id == *resolved)
    }

    fn capture_virtual_desktop(
        &mut self,
        request: &CaptureRequest,
        recorder: &mut EventRecorder,
    ) -> Result<CaptureOutcome, CaptureError> {
        if let Some(error) = &self.virtual_topology_error {
            return Err(error.clone());
        }
        validate_virtual_desktop_request(request)?;
        if request.retain_native_frame {
            log::debug!(
                "capture {} composes the virtual desktop from CPU readbacks; no native frame is retained",
                request.id.0
            );
        }
        recorder.record(request.id, PerfEventKind::CaptureRequested, 0);
        let mut frames = Vec::with_capacity(self.sessions.len());
        for session in &mut self.sessions {
            let mut output_request = request.clone();
            output_request.source = CaptureSource::Display(session.display_id.clone());
            output_request.cpu_frame = true;
            output_request.retain_native_frame = false;
            let mut output_recorder = EventRecorder::with_capacity(8);
            let outcome = session
                .backend
                .capture(&output_request, &mut output_recorder)?;
            frames.push(outcome.frame.ok_or_else(|| {
                manager_error(
                    CaptureErrorKind::InvalidFrame,
                    "compose_virtual_desktop",
                    format!("display {} returned no CPU frame", session.display_id.0),
                    false,
                )
            })?);
        }
        let native_ready_ns = duration_ns(request.triggered_at.elapsed().as_nanos());
        recorder.record(request.id, PerfEventKind::NativeFrameReady, native_ready_ns);
        recorder.record(request.id, PerfEventKind::ReadbackStarted, 0);
        let mut frame = compose_virtual_desktop(
            &frames,
            self.virtual_bounds,
            request,
            &mut self.composite_pool,
        )?;
        let cpu_ready_ns = duration_ns(request.triggered_at.elapsed().as_nanos());
        recorder.record(request.id, PerfEventKind::CpuFrameReady, cpu_ready_ns);
        frame.metadata.native_ready_offset_ns = native_ready_ns;
        frame.metadata.cpu_ready_offset_ns = Some(cpu_ready_ns);
        Ok(virtual_desktop_outcome(frame, request.cpu_frame))
    }
}

impl CaptureBackend for DxgiDisplayManager {
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
        let CaptureSource::Display(requested) = &request.source else {
            return self.capture_virtual_desktop(request, recorder);
        };
        let index = self.session_index(requested).ok_or_else(|| {
            let unavailable = self
                .unavailable
                .iter()
                .find(|(id, _)| id == requested)
                .map(|(_, error)| format!("; initialization failed: {error}"))
                .unwrap_or_default();
            manager_error(
                CaptureErrorKind::SourceUnavailable,
                "route_capture",
                format!(
                    "display {} has no retained capture session{unavailable}",
                    requested.0
                ),
                true,
            )
        })?;
        self.sessions[index].backend.capture(request, recorder)
    }
}

struct CompositeBufferPool {
    slots: Vec<Option<Arc<[u8]>>>,
    cursor: usize,
}

impl CompositeBufferPool {
    fn new(slot_count: usize) -> Self {
        Self {
            slots: vec![None; slot_count],
            cursor: 0,
        }
    }

    fn available_index(&mut self, required_len: usize) -> Option<usize> {
        for offset in 0..self.slots.len() {
            let index = (self.cursor + offset) % self.slots.len();
            let available = self.slots[index]
                .as_ref()
                .is_none_or(|slot| Arc::strong_count(slot) == 1);
            if !available {
                continue;
            }
            if self.slots[index]
                .as_ref()
                .is_none_or(|slot| slot.len() != required_len)
            {
                self.slots[index] = Some(vec![0; required_len].into());
            }
            self.cursor = (index + 1) % self.slots.len();
            return Some(index);
        }
        None
    }
}

fn compose_virtual_desktop(
    frames: &[CpuFrame],
    bounds: Rect,
    request: &CaptureRequest,
    pool: &mut CompositeBufferPool,
) -> Result<CpuFrame, CaptureError> {
    let stride = bounds.width.checked_mul(4).ok_or_else(|| {
        manager_error(
            CaptureErrorKind::InvalidFrame,
            "compose_virtual_desktop",
            "virtual-desktop stride overflowed",
            false,
        )
    })?;
    let len = usize::try_from(stride)
        .ok()
        .and_then(|stride| {
            usize::try_from(bounds.height)
                .ok()
                .and_then(|height| stride.checked_mul(height))
        })
        .ok_or_else(|| {
            manager_error(
                CaptureErrorKind::InvalidFrame,
                "compose_virtual_desktop",
                "virtual-desktop buffer size overflowed",
                false,
            )
        })?;
    let slot = pool.available_index(len).ok_or_else(|| {
        manager_error(
            CaptureErrorKind::BufferExhausted,
            "compose_virtual_desktop",
            "all bounded virtual-desktop frame slots are still in use",
            true,
        )
    })?;
    let destination = Arc::get_mut(pool.slots[slot].as_mut().expect("slot initialized"))
        .expect("available composite slot has exactly one owner");
    for pixel in destination.chunks_exact_mut(4) {
        pixel.copy_from_slice(&OPAQUE_BLACK);
    }

    let mut ordered: Vec<_> = frames.iter().collect();
    // Smaller stable display IDs win overlaps, independent of DXGI enumeration order.
    ordered.sort_by(|left, right| right.metadata.display_id.0.cmp(&left.metadata.display_id.0));
    for frame in ordered {
        copy_frame_into(frame, bounds, destination, stride as usize)?;
    }

    let pixels = pool.slots[slot].as_ref().expect("slot initialized").clone();
    let presentation_offset_ns = frames
        .iter()
        .filter_map(|frame| frame.metadata.presentation_offset_ns)
        .min();
    let frame_age_ns = frames
        .iter()
        .filter_map(|frame| frame.metadata.frame_age_ns)
        .max();
    // A composite is only as verified as its least-recently-verified output, and not verified at
    // all unless every output is: claiming otherwise would let one freshly-checked display vouch
    // for a stale neighbour it knows nothing about.
    let verified_current_offset_ns = frames
        .iter()
        .map(|frame| frame.metadata.verified_current_offset_ns)
        .try_fold(None::<u64>, |oldest, verified| {
            verified.map(|offset| Some(oldest.map_or(offset, |previous: u64| previous.min(offset))))
        })
        .flatten();
    let copy_count = frames.iter().fold(1_u32, |count, frame| {
        count.saturating_add(frame.metadata.copy_count)
    });
    let metadata = FrameMetadata {
        capture_id: request.id,
        backend: "dxgi".to_owned(),
        display_id: DisplayId::virtual_desktop(),
        source_rect: bounds,
        rotation_degrees: 0,
        capture_mode: request.mode.clone(),
        presentation_offset_ns,
        verified_current_offset_ns,
        timing_provenance: if frames
            .iter()
            .all(|frame| frame.metadata.timing_provenance == TimingProvenance::OsPresentationTime)
        {
            TimingProvenance::OsPresentationTime
        } else {
            TimingProvenance::Unavailable
        },
        native_ready_offset_ns: 0,
        cpu_ready_offset_ns: None,
        frame_age_ns,
        frame_generation: None,
        copy_count,
        pool_slot: Some(slot as u16),
        cursor: None,
    };
    CpuFrame::new(
        pixels,
        bounds.width,
        bounds.height,
        stride,
        PixelFormat::Bgra8Unorm,
        FrameOrigin::TopLeft,
        ColorSpace::Srgb,
        metadata,
    )
    .map_err(|error| {
        manager_error(
            CaptureErrorKind::InvalidFrame,
            "compose_virtual_desktop",
            error.to_string(),
            false,
        )
    })
}

fn copy_frame_into(
    frame: &CpuFrame,
    bounds: Rect,
    destination: &mut [u8],
    destination_stride: usize,
) -> Result<(), CaptureError> {
    let PixelEncoding::EightBitRgba { blue_first: true } = frame.format().encoding() else {
        return Err(manager_error(
            CaptureErrorKind::InvalidFrame,
            "compose_virtual_desktop",
            "virtual-desktop composition requires 8-bit BGRA pixels",
            false,
        ));
    };
    if frame.origin != FrameOrigin::TopLeft
        || frame.width() != frame.metadata.source_rect.width
        || frame.height() != frame.metadata.source_rect.height
    {
        return Err(manager_error(
            CaptureErrorKind::InvalidFrame,
            "compose_virtual_desktop",
            format!(
                "display {} did not return normalized top-left BGRA pixels",
                frame.metadata.display_id.0
            ),
            false,
        ));
    }
    let local_x = usize::try_from(
        i64::from(frame.metadata.source_rect.x).saturating_sub(i64::from(bounds.x)),
    )
    .map_err(|_| {
        manager_error(
            CaptureErrorKind::TopologyChanged,
            "compose_virtual_desktop",
            "display origin lies outside virtual-desktop bounds",
            true,
        )
    })?;
    let local_y = usize::try_from(
        i64::from(frame.metadata.source_rect.y).saturating_sub(i64::from(bounds.y)),
    )
    .map_err(|_| {
        manager_error(
            CaptureErrorKind::TopologyChanged,
            "compose_virtual_desktop",
            "display origin lies outside virtual-desktop bounds",
            true,
        )
    })?;
    let frame_right = local_x
        .checked_add(frame.width() as usize)
        .filter(|right| *right <= bounds.width as usize);
    let frame_bottom = local_y
        .checked_add(frame.height() as usize)
        .filter(|bottom| *bottom <= bounds.height as usize);
    if frame_right.is_none() || frame_bottom.is_none() {
        return Err(manager_error(
            CaptureErrorKind::TopologyChanged,
            "compose_virtual_desktop",
            "display bounds extend outside the virtual-desktop rows",
            true,
        ));
    }
    let row_bytes = frame.width() as usize * 4;
    for row in 0..frame.height() as usize {
        let source_start = row * frame.stride_bytes() as usize;
        let destination_start = (local_y + row) * destination_stride + local_x * 4;
        let destination_end = destination_start.checked_add(row_bytes).ok_or_else(|| {
            manager_error(
                CaptureErrorKind::TopologyChanged,
                "compose_virtual_desktop",
                "display bounds overflow the virtual-desktop frame",
                true,
            )
        })?;
        if destination_end > destination.len() {
            return Err(manager_error(
                CaptureErrorKind::TopologyChanged,
                "compose_virtual_desktop",
                "display bounds extend outside the virtual-desktop frame",
                true,
            ));
        }
        destination[destination_start..destination_end]
            .copy_from_slice(&frame.pixels()[source_start..source_start + row_bytes]);
    }
    Ok(())
}

/// The HRESULT a cursor query refused by a secure desktop comes back with.
const HRESULT_ACCESS_DENIED: i32 = 0x8007_0005_u32 as i32;

/// The operation name a denied cursor query is reported under, in the log and in these tests.
const CURSOR_QUERY: &str = "get_physical_cursor_position";

/// Explains a failed cursor query, asking the session about it only when it was denied.
///
/// A lock refuses this call while the credential prompt owns input, and until 2026-08-20 the
/// refusal arrived as a bare `PermissionDenied in windows/get_physical_cursor_position: Access is
/// denied.` — measured twice, 1.2 s after a real lock, under the `pointer` display policy. That is
/// the permissions-shaped lie issue #51 exists to prevent, on a path #51 never covered:
/// `duplicate_output` has asked the session about its denials since #51 and this had not.
///
/// The session is asked **only** on a denial, and only after the call has already failed. Every
/// capture under the `pointer` policy runs this query, so the four syscalls behind
/// [`crate::session::desktop_state`] must not be on the path a working cursor takes; the probe is
/// taken as a closure so a successful query, and a failure that is not a denial, never call it.
///
/// A denial the session cannot explain keeps its original kind, message and native code. Swallowing
/// a real access failure behind a comfortable message about a lock would be the same defect pointing
/// the other way.
fn cursor_query_error(
    code: i32,
    message: String,
    probe_session: impl FnOnce() -> crate::session::DesktopState,
) -> CaptureError {
    if code == HRESULT_ACCESS_DENIED {
        if let Some(obstacle) =
            crate::dxgi::session_obstacle("windows", CURSOR_QUERY, &probe_session())
        {
            return obstacle;
        }
    }
    CaptureError {
        kind: if code == HRESULT_ACCESS_DENIED {
            CaptureErrorKind::PermissionDenied
        } else {
            CaptureErrorKind::NativeFailure
        },
        backend: "windows",
        operation: CURSOR_QUERY,
        message,
        retryable: true,
        native_code: Some(i64::from(code)),
    }
}

/// Resolves the pointer once. This function does not poll or retain any input hooks.
pub fn display_containing_pointer(displays: &[DisplayInfo]) -> Result<DisplayId, CaptureError> {
    let mut point = POINT::default();
    // SAFETY: point is valid writable storage for the duration of this one-shot query.
    unsafe { GetPhysicalCursorPos(&mut point) }.map_err(|error| {
        cursor_query_error(
            error.code().0,
            error.to_string(),
            crate::session::desktop_state,
        )
    })?;
    display_containing_point(displays, point.x, point.y)
        .map(|display| display.id.clone())
        .ok_or_else(|| {
            manager_error(
                CaptureErrorKind::PointerOutsideDisplays,
                "resolve_pointer_display",
                format!(
                    "pointer position ({}, {}) is on no attached display",
                    point.x, point.y
                ),
                // Not retryable: retrying re-reads the same cursor position against the same
                // displays. Reported as TopologyChanged until 2026-08, which made every occurrence
                // tear down and rebuild the capture engine three times before failing anyway.
                false,
            )
        })
}

fn resolve_display_id<'a>(
    displays: &'a [DisplayInfo],
    requested: &DisplayId,
) -> Option<&'a DisplayId> {
    if requested.is_primary_alias() {
        displays
            .iter()
            .find(|display| display.is_primary)
            .or_else(|| displays.first())
            .map(|display| &display.id)
    } else {
        displays
            .iter()
            .find(|display| display.id == *requested)
            .map(|display| &display.id)
    }
}

fn display_containing_point(displays: &[DisplayInfo], x: i32, y: i32) -> Option<&DisplayInfo> {
    displays
        .iter()
        .filter(|display| display.bounds.contains(x, y))
        .min_by(|left, right| left.id.0.cmp(&right.id.0))
}

fn duration_ns(nanos: u128) -> u64 {
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

/// Rejects virtual-desktop requests the composite path cannot answer with anything usable.
///
/// The composite is assembled from per-display CPU readbacks, so there is no native frame to
/// retain. A caller that also opted out of the CPU frame would otherwise receive a successful
/// outcome carrying neither, which reads as an empty capture rather than an unsupported one.
/// Requests that keep `cpu_frame` still succeed: they get the composite and materialize regions
/// from it on the CPU.
fn validate_virtual_desktop_request(request: &CaptureRequest) -> Result<(), CaptureError> {
    if request.retain_native_frame && !request.cpu_frame {
        return Err(manager_error(
            CaptureErrorKind::Unsupported,
            "capture_virtual_desktop",
            "virtual-desktop capture composes CPU readbacks and cannot retain a native frame; request cpu_frame to receive the composite",
            false,
        ));
    }
    Ok(())
}

/// Wraps the composite so the metadata only describes what the caller actually receives.
fn virtual_desktop_outcome(composite: CpuFrame, cpu_frame: bool) -> CaptureOutcome {
    let mut metadata = composite.metadata.clone();
    let frame = cpu_frame.then_some(composite);
    if frame.is_none() {
        // The composite returns to the bounded pool as it drops, so naming its slot would point
        // telemetry at storage the very next capture is free to overwrite.
        metadata.pool_slot = None;
    }
    CaptureOutcome {
        metadata,
        frame,
        native_frame: None,
    }
}

fn validate_virtual_topology(
    adapters: &[i64],
    unavailable: &[DisplayId],
    bounds: Rect,
) -> Result<(), CaptureError> {
    let first_adapter = adapters.first().copied();
    if adapters
        .iter()
        .any(|adapter| Some(*adapter) != first_adapter)
    {
        return Err(manager_error(
            CaptureErrorKind::Unsupported,
            "validate_virtual_topology",
            "virtual-desktop capture currently requires every display to use the same DXGI adapter"
                .to_owned(),
            false,
        ));
    }
    if !unavailable.is_empty() {
        return Err(manager_error(
            CaptureErrorKind::Unsupported,
            "validate_virtual_topology",
            format!(
                "virtual-desktop capture requires a retained session for every display; unavailable: {}",
                unavailable
                    .iter()
                    .map(|id| id.0.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            false,
        ));
    }
    let frame_bytes = usize::try_from(bounds.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .and_then(|stride| {
            usize::try_from(bounds.height)
                .ok()
                .and_then(|height| stride.checked_mul(height))
        });
    if frame_bytes.is_none_or(|bytes| bytes > MAX_COMPOSITE_FRAME_BYTES) {
        return Err(manager_error(
            CaptureErrorKind::Unsupported,
            "validate_virtual_topology",
            format!(
                "virtual-desktop bounds {}x{} exceed the bounded {} MiB composite-frame limit",
                bounds.width,
                bounds.height,
                MAX_COMPOSITE_FRAME_BYTES / (1024 * 1024)
            ),
            false,
        ));
    }
    Ok(())
}

fn manager_error(
    kind: CaptureErrorKind,
    operation: &'static str,
    message: impl Into<String>,
    retryable: bool,
) -> CaptureError {
    CaptureError {
        kind,
        backend: "dxgi-manager",
        operation,
        message: message.into(),
        retryable,
        native_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use captastic_core::CaptureMode;
    use captastic_core::{CaptureId, CursorMode, FrameAlpha};
    use std::time::Instant;

    fn display(id: &str, bounds: Rect, rotation_degrees: u16, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds,
            scale_factor: 1.0,
            rotation_degrees,
            is_primary: primary,
        }
    }

    fn request() -> CaptureRequest {
        CaptureRequest {
            id: CaptureId(7),
            triggered_at: Instant::now(),
            source: CaptureSource::VirtualDesktop,
            mode: CaptureMode::Latest { max_age_ms: None },
            cpu_frame: true,
            retain_native_frame: false,
            cursor: CursorMode::Exclude,
        }
    }

    fn solid_frame(info: &DisplayInfo, value: u8) -> CpuFrame {
        let stride = info.bounds.width * 4;
        let mut pixels = vec![0; stride as usize * info.bounds.height as usize];
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[value, value, value, 0xff]);
        }
        let metadata = FrameMetadata {
            capture_id: CaptureId(7),
            backend: "test".to_owned(),
            display_id: info.id.clone(),
            source_rect: info.bounds,
            rotation_degrees: info.rotation_degrees,
            capture_mode: CaptureMode::Latest { max_age_ms: None },
            presentation_offset_ns: Some(-1),
            timing_provenance: TimingProvenance::OsPresentationTime,
            native_ready_offset_ns: 1,
            cpu_ready_offset_ns: Some(2),
            frame_age_ns: Some(1),
            verified_current_offset_ns: None,
            frame_generation: Some(1),
            copy_count: 2,
            pool_slot: Some(0),
            cursor: None,
        };
        CpuFrame::new(
            pixels.into(),
            info.bounds.width,
            info.bounds.height,
            stride,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("frame")
        .with_alpha(FrameAlpha::Opaque)
    }

    /// The same fixture in a format the compositor cannot copy byte-for-byte.
    fn half_float_frame(info: &DisplayInfo) -> CpuFrame {
        let template = solid_frame(info, 0x20);
        let stride = info.bounds.width * 8;
        CpuFrame::new(
            vec![0_u8; stride as usize * info.bounds.height as usize].into(),
            info.bounds.width,
            info.bounds.height,
            stride,
            PixelFormat::Rgba16Float,
            FrameOrigin::TopLeft,
            ColorSpace::ScRgb,
            template.metadata.clone(),
        )
        .expect("a valid half-float frame")
    }

    #[test]
    fn virtual_desktop_composition_refuses_pixels_it_cannot_copy() {
        // Composition memcpys rows on the assumption that a pixel is four bytes of BGRA. A wider
        // pixel has to be refused rather than copied at the wrong width, which would produce a
        // composite of the right size holding a quarter of the image and three quarters of
        // whatever followed it.
        let info = display(
            "solo",
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            },
            0,
            true,
        );
        let mut destination = vec![0_u8; 4 * 2 * 4];

        let error = copy_frame_into(&half_float_frame(&info), info.bounds, &mut destination, 16)
            .expect_err("a half-float frame cannot be composed");

        assert_eq!(error.kind, CaptureErrorKind::InvalidFrame);
        assert!(destination.iter().all(|byte| *byte == 0));
    }

    fn pixel(frame: &CpuFrame, x: u32, y: u32) -> [u8; 4] {
        let start = y as usize * frame.stride_bytes() as usize + x as usize * 4;
        frame.pixels()[start..start + 4].try_into().unwrap()
    }

    #[test]
    fn point_resolution_handles_mixed_resolutions_and_origins() {
        let displays = vec![
            display(
                "laptop",
                Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1200,
                },
                0,
                true,
            ),
            display(
                "external",
                Rect {
                    x: 1920,
                    y: -240,
                    width: 3840,
                    height: 2160,
                },
                0,
                false,
            ),
        ];
        assert_eq!(
            display_containing_point(&displays, 100, 100).map(|display| display.id.0.as_str()),
            Some("laptop")
        );
        assert_eq!(
            display_containing_point(&displays, 2000, -100).map(|display| display.id.0.as_str()),
            Some("external")
        );
        assert!(display_containing_point(&displays, 100, -100).is_none());
    }

    #[test]
    fn primary_alias_routes_to_the_enumerated_primary_identity() {
        let displays = vec![
            display(
                "laptop",
                Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                0,
                true,
            ),
            display(
                "external",
                Rect {
                    x: 2,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                0,
                false,
            ),
        ];
        assert_eq!(
            resolve_display_id(&displays, &DisplayId::primary()).map(|id| id.0.as_str()),
            Some("laptop")
        );
    }

    #[test]
    fn composes_negative_origins_gaps_and_differing_resolutions() {
        let left = display(
            "left",
            Rect {
                x: -2,
                y: 1,
                width: 2,
                height: 2,
            },
            0,
            false,
        );
        let primary = display(
            "primary",
            Rect {
                x: 1,
                y: 0,
                width: 3,
                height: 1,
            },
            0,
            true,
        );
        let bounds = DisplayTopology::new(1, vec![left.clone(), primary.clone()])
            .unwrap()
            .virtual_bounds()
            .unwrap();
        assert_eq!(
            bounds,
            Rect {
                x: -2,
                y: 0,
                width: 6,
                height: 3
            }
        );
        let mut pool = CompositeBufferPool::new(3);
        let frame = compose_virtual_desktop(
            &[solid_frame(&primary, 2), solid_frame(&left, 1)],
            bounds,
            &request(),
            &mut pool,
        )
        .unwrap();
        assert_eq!((frame.width(), frame.height()), (6, 3));
        assert_eq!(frame.metadata.source_rect, bounds);
        assert_eq!(pixel(&frame, 0, 1), [1, 1, 1, 0xff]);
        assert_eq!(pixel(&frame, 3, 0), [2, 2, 2, 0xff]);
        assert_eq!(pixel(&frame, 2, 1), OPAQUE_BLACK);
        assert_eq!(pixel(&frame, 5, 2), OPAQUE_BLACK);
    }

    #[test]
    fn stale_frame_cannot_wrap_past_the_virtual_desktop_row() {
        let stale = display(
            "stale",
            Rect {
                x: 2,
                y: 0,
                width: 2,
                height: 1,
            },
            0,
            true,
        );
        let current_bounds = Rect {
            x: 0,
            y: 0,
            width: 3,
            height: 2,
        };
        let mut pool = CompositeBufferPool::new(1);

        let error = compose_virtual_desktop(
            &[solid_frame(&stale, 7)],
            current_bounds,
            &request(),
            &mut pool,
        )
        .expect_err("stale frame crosses the row boundary");

        assert_eq!(error.kind, CaptureErrorKind::TopologyChanged);
        assert!(error.message.contains("outside the virtual-desktop rows"));
    }

    #[test]
    fn normalized_rotated_frames_keep_virtual_physical_coordinates() {
        let portrait = display(
            "portrait",
            Rect {
                x: -2,
                y: -1,
                width: 2,
                height: 3,
            },
            90,
            true,
        );
        let frame = solid_frame(&portrait, 9);
        let mut pool = CompositeBufferPool::new(1);
        let composite =
            compose_virtual_desktop(&[frame], portrait.bounds, &request(), &mut pool).unwrap();
        assert_eq!((composite.width(), composite.height()), (2, 3));
        assert_eq!(composite.metadata.rotation_degrees, 0);
        assert_eq!(composite.metadata.source_rect, portrait.bounds);
    }

    #[test]
    fn selections_use_absolute_virtual_desktop_coordinates() {
        let left = display(
            "left",
            Rect {
                x: -3,
                y: -2,
                width: 3,
                height: 2,
            },
            0,
            true,
        );
        let mut pool = CompositeBufferPool::new(1);
        let composite =
            compose_virtual_desktop(&[solid_frame(&left, 4)], left.bounds, &request(), &mut pool)
                .unwrap();
        let selection = Rect {
            x: -2,
            y: -1,
            width: 2,
            height: 1,
        };
        let cropped = composite.crop(selection).unwrap();
        assert_eq!((cropped.width(), cropped.height()), (2, 1));
        assert_eq!(cropped.metadata.source_rect, selection);
        assert_eq!(pixel(&cropped, 0, 0), [4, 4, 4, 0xff]);
    }

    #[test]
    fn display_order_does_not_change_overlap_precedence() {
        let a = display(
            "a",
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            0,
            true,
        );
        let b = display(
            "b",
            Rect {
                x: 1,
                y: 0,
                width: 2,
                height: 1,
            },
            0,
            false,
        );
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 3,
            height: 1,
        };
        let a_frame = solid_frame(&a, 1);
        let b_frame = solid_frame(&b, 2);
        for frames in [
            vec![a_frame.clone(), b_frame.clone()],
            vec![b_frame.clone(), a_frame.clone()],
        ] {
            let mut pool = CompositeBufferPool::new(1);
            let composite =
                compose_virtual_desktop(&frames, bounds, &request(), &mut pool).unwrap();
            assert_eq!(pixel(&composite, 1, 0), [1, 1, 1, 0xff]);
        }
    }

    #[test]
    fn topology_changes_recompute_bounds_without_reusing_wrong_sized_buffers() {
        let first = display(
            "a",
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            0,
            true,
        );
        let changed = display(
            "a",
            Rect {
                x: -1,
                y: -1,
                width: 3,
                height: 2,
            },
            0,
            true,
        );
        let mut pool = CompositeBufferPool::new(1);
        let old = compose_virtual_desktop(
            &[solid_frame(&first, 1)],
            first.bounds,
            &request(),
            &mut pool,
        )
        .unwrap();
        drop(old);
        let new = compose_virtual_desktop(
            &[solid_frame(&changed, 2)],
            changed.bounds,
            &request(),
            &mut pool,
        )
        .unwrap();
        assert_eq!((new.width(), new.height()), (3, 2));
        assert_eq!(new.metadata.source_rect, changed.bounds);
    }

    #[test]
    fn composite_pool_is_bounded_while_frames_are_leased() {
        let display = display(
            "a",
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            0,
            true,
        );
        let source = solid_frame(&display, 1);
        let mut pool = CompositeBufferPool::new(1);
        let leased = compose_virtual_desktop(
            std::slice::from_ref(&source),
            display.bounds,
            &request(),
            &mut pool,
        )
        .unwrap();
        let error =
            compose_virtual_desktop(&[source], display.bounds, &request(), &mut pool).unwrap_err();
        assert_eq!(error.kind, CaptureErrorKind::BufferExhausted);
        drop(leased);
    }

    #[test]
    fn retaining_a_native_frame_without_a_cpu_frame_is_rejected() {
        let mut retain_only = request();
        retain_only.retain_native_frame = true;
        retain_only.cpu_frame = false;
        let error = validate_virtual_desktop_request(&retain_only)
            .expect_err("the composite path can return neither frame");
        assert_eq!(error.kind, CaptureErrorKind::Unsupported);
        assert_eq!(error.operation, "capture_virtual_desktop");
        assert!(!error.retryable);
        assert!(error.message.contains("cannot retain a native frame"));

        // Requests that still take the composite keep working; they crop it on the CPU.
        let mut retain_with_cpu = request();
        retain_with_cpu.retain_native_frame = true;
        assert!(validate_virtual_desktop_request(&retain_with_cpu).is_ok());
        assert!(validate_virtual_desktop_request(&request()).is_ok());
        let mut metadata_only = request();
        metadata_only.cpu_frame = false;
        assert!(validate_virtual_desktop_request(&metadata_only).is_ok());
    }

    #[test]
    fn a_discarded_composite_does_not_name_a_pool_slot() {
        let only = display(
            "a",
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            0,
            true,
        );
        let mut pool = CompositeBufferPool::new(1);
        let composite =
            compose_virtual_desktop(&[solid_frame(&only, 1)], only.bounds, &request(), &mut pool)
                .unwrap();
        assert_eq!(composite.metadata.pool_slot, Some(0));

        let kept = virtual_desktop_outcome(composite.clone(), true);
        assert!(kept.frame.is_some());
        assert_eq!(kept.metadata.pool_slot, Some(0));
        drop(kept);

        let dropped = virtual_desktop_outcome(composite, false);
        assert!(dropped.frame.is_none());
        assert!(dropped.native_frame.is_none());
        assert_eq!(dropped.metadata.pool_slot, None);
        // The slot is genuinely free again, so metadata naming it would have been a lie.
        assert!(pool.available_index(4).is_some());
    }

    #[test]
    fn unsupported_adapters_and_incomplete_topologies_are_explicit() {
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let multi_adapter = validate_virtual_topology(&[10, 20], &[], bounds).unwrap_err();
        assert_eq!(multi_adapter.kind, CaptureErrorKind::Unsupported);
        assert_eq!(multi_adapter.operation, "validate_virtual_topology");
        assert!(multi_adapter.message.contains("same DXGI adapter"));
        let unavailable =
            validate_virtual_topology(&[10, 10], &[DisplayId("portrait".to_owned())], bounds)
                .unwrap_err();
        assert_eq!(unavailable.kind, CaptureErrorKind::Unsupported);
        assert!(unavailable.message.contains("portrait"));
        assert!(validate_virtual_topology(&[10, 10], &[], bounds).is_ok());
        let oversized = validate_virtual_topology(
            &[10],
            &[],
            Rect {
                x: 0,
                y: 0,
                width: 32_768,
                height: 32_768,
            },
        )
        .unwrap_err();
        assert_eq!(oversized.kind, CaptureErrorKind::Unsupported);
        assert!(oversized.message.contains("bounded 512 MiB"));
    }

    /// The message a real lock produced, and what the fix has to turn it into.
    ///
    /// Measured 2026-08-20 17:17:02.8Z, 1.2 s after `LockWorkStation`, twice: `PermissionDenied in
    /// windows/get_physical_cursor_position: Access is denied. (0x80070005)`. The credential prompt
    /// owned input and refused the `pointer` policy's cursor query, and the reported error named a
    /// permissions problem the user did not have — the same bare denial issue #51 was filed about,
    /// on the one path #51 never covered.
    #[test]
    fn a_locked_session_explains_a_denied_cursor_query() {
        let denied = cursor_query_error(
            HRESULT_ACCESS_DENIED,
            "Access is denied. (0x80070005)".to_owned(),
            || crate::session::DesktopState::Locked {
                desktop: Some("Winlogon".to_owned()),
            },
        );
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
        assert!(
            denied.message.contains("locked"),
            "the whole point is that the message says so: {denied}"
        );
        assert!(denied.message.contains("Winlogon"), "{denied}");
        // The operation a log reader greps for is unchanged; only the explanation is new.
        assert_eq!(denied.backend, "windows");
        assert_eq!(denied.operation, CURSOR_QUERY);
        assert!(denied.retryable);
    }

    /// Every other temporary session state explains it too, because every one of them refuses.
    ///
    /// A disconnected RDP session, a UAC prompt and a screensaver deny this call for reasons that
    /// are no more a permissions problem than a lock is, and they clear the same way.
    #[test]
    fn any_session_that_owns_no_desktop_explains_a_denied_cursor_query() {
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
            let denied = cursor_query_error(
                HRESULT_ACCESS_DENIED,
                "Access is denied. (0x80070005)".to_owned(),
                || state.clone(),
            );
            assert_eq!(
                denied.kind,
                CaptureErrorKind::DesktopUnavailable,
                "{state:?} should explain the denial"
            );
            assert_eq!(denied.message, state.to_string());
        }
    }

    /// A denial the session cannot account for stays exactly what it was.
    ///
    /// This is the half that makes the fix safe to make. If an unlocked, attached, console session
    /// is denied the cursor, something really is wrong with this process's rights, and reporting a
    /// lock instead would trade one misleading message for another. `Unknown` counts as "cannot
    /// account for it": a probe that failed answered nothing, and nothing is not "locked".
    #[test]
    fn an_unlocked_session_leaves_a_denied_cursor_query_alone() {
        for state in [
            crate::session::DesktopState::Interactive,
            crate::session::DesktopState::Unknown {
                detail: "the input desktop could not be named".to_owned(),
            },
        ] {
            let denied = cursor_query_error(
                HRESULT_ACCESS_DENIED,
                "Access is denied. (0x80070005)".to_owned(),
                || state.clone(),
            );
            assert_eq!(
                denied.kind,
                CaptureErrorKind::PermissionDenied,
                "{state:?} does not explain a denial and must not hide one"
            );
            assert_eq!(denied.message, "Access is denied. (0x80070005)");
            assert_eq!(denied.native_code, Some(i64::from(HRESULT_ACCESS_DENIED)));
            assert_eq!(denied.backend, "windows");
            assert_eq!(denied.operation, CURSOR_QUERY);
        }
    }

    /// The session probe costs four syscalls, and the `pointer` policy runs this query every
    /// capture. It may only be paid on the failure it can explain.
    ///
    /// `duplicate_output` set that discipline — it asks the session inside `map_err` and nowhere
    /// else. Here the probe is a closure precisely so the cheap paths can be shown never to call
    /// it: a successful query does not reach this function at all, and a failure that is not
    /// `E_ACCESSDENIED` has nothing for the session to say about it.
    #[test]
    fn only_a_denied_cursor_query_pays_for_the_session_probe() {
        let probes = std::cell::Cell::new(0_u32);
        let probe = || {
            probes.set(probes.get() + 1);
            crate::session::DesktopState::Locked { desktop: None }
        };
        let failed = cursor_query_error(
            0x8007_001f_u32 as i32,
            "A device attached to the system is not functioning.".to_owned(),
            probe,
        );
        assert_eq!(probes.get(), 0, "a non-denial must not ask the session");
        assert_eq!(failed.kind, CaptureErrorKind::NativeFailure);
        assert_eq!(
            failed.message,
            "A device attached to the system is not functioning."
        );
        assert_eq!(failed.native_code, Some(i64::from(0x8007_001f_u32 as i32)));

        let denied = cursor_query_error(
            HRESULT_ACCESS_DENIED,
            "Access is denied.".to_owned(),
            || {
                probes.set(probes.get() + 1);
                crate::session::DesktopState::Locked { desktop: None }
            },
        );
        assert_eq!(probes.get(), 1, "a denial asks the session exactly once");
        assert_eq!(denied.kind, CaptureErrorKind::DesktopUnavailable);
    }
}
