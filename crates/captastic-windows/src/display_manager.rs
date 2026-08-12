use std::sync::Arc;

use captastic_core::{
    BackendCapabilities, CaptureBackend, CaptureError, CaptureErrorKind, CaptureOutcome,
    CaptureRequest, CaptureSource, ColorSpace, CpuFrame, DisplayId, DisplayInfo, DisplayTopology,
    EventRecorder, FrameMetadata, FrameOrigin, PerfEventKind, PixelFormat, Rect, TimingProvenance,
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
            return Err(manager_error(
                CaptureErrorKind::SourceUnavailable,
                "initialize",
                "no attached desktop displays were found",
                false,
            ));
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
        let metadata = frame.metadata.clone();
        let frame = request.cpu_frame.then_some(frame);
        Ok(CaptureOutcome {
            metadata,
            frame,
            native_frame: None,
            backend_duration: request.triggered_at.elapsed(),
        })
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
    if frame.format != PixelFormat::Bgra8Unorm
        || frame.origin != FrameOrigin::TopLeft
        || frame.width != frame.metadata.source_rect.width
        || frame.height != frame.metadata.source_rect.height
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
    let row_bytes = frame.width as usize * 4;
    for row in 0..frame.height as usize {
        let source_start = row * frame.stride_bytes as usize;
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
            .copy_from_slice(&frame.pixels[source_start..source_start + row_bytes]);
    }
    Ok(())
}

/// Resolves the pointer once. This function does not poll or retain any input hooks.
pub fn display_containing_pointer(displays: &[DisplayInfo]) -> Result<DisplayId, CaptureError> {
    let mut point = POINT::default();
    // SAFETY: point is valid writable storage for the duration of this one-shot query.
    unsafe { GetPhysicalCursorPos(&mut point) }.map_err(|error| CaptureError {
        kind: if error.code().0 == 0x8007_0005_u32 as i32 {
            CaptureErrorKind::PermissionDenied
        } else {
            CaptureErrorKind::NativeFailure
        },
        backend: "windows",
        operation: "get_physical_cursor_position",
        message: error.to_string(),
        retryable: true,
        native_code: Some(i64::from(error.code().0)),
    })?;
    display_containing_point(displays, point.x, point.y)
        .map(|display| display.id.clone())
        .ok_or_else(|| {
            manager_error(
                CaptureErrorKind::TopologyChanged,
                "resolve_pointer_display",
                format!(
                    "pointer position ({}, {}) is outside the current display topology",
                    point.x, point.y
                ),
                true,
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
            frame_generation: Some(1),
            copy_count: 2,
            pool_slot: Some(0),
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

    fn pixel(frame: &CpuFrame, x: u32, y: u32) -> [u8; 4] {
        let start = y as usize * frame.stride_bytes as usize + x as usize * 4;
        frame.pixels[start..start + 4].try_into().unwrap()
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
        assert_eq!((frame.width, frame.height), (6, 3));
        assert_eq!(frame.metadata.source_rect, bounds);
        assert_eq!(pixel(&frame, 0, 1), [1, 1, 1, 0xff]);
        assert_eq!(pixel(&frame, 3, 0), [2, 2, 2, 0xff]);
        assert_eq!(pixel(&frame, 2, 1), OPAQUE_BLACK);
        assert_eq!(pixel(&frame, 5, 2), OPAQUE_BLACK);
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
        assert_eq!((composite.width, composite.height), (2, 3));
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
        assert_eq!((cropped.width, cropped.height), (2, 1));
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
        assert_eq!((new.width, new.height), (3, 2));
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
}
