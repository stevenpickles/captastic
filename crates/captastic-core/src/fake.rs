use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::metrics::nanos_u64;
use crate::{
    BackendCapabilities, CaptureBackend, CaptureError, CaptureMode, CaptureOutcome, CaptureRequest,
    ColorSpace, CpuFrame, DisplayId, DisplayInfo, EventRecorder, FrameMetadata, FrameOrigin,
    PerfEventKind, PixelFormat, Rect, TimingProvenance,
};

#[derive(Clone, Debug)]
pub struct FakeBackendConfig {
    pub width: u32,
    pub height: u32,
    pub native_delay: Duration,
    pub readback_delay: Duration,
    pub frame_age: Duration,
    pub fail_every: Option<u64>,
}

impl Default for FakeBackendConfig {
    fn default() -> Self {
        Self {
            width: 64,
            height: 64,
            native_delay: Duration::from_micros(250),
            readback_delay: Duration::from_micros(250),
            frame_age: Duration::from_millis(1),
            fail_every: None,
        }
    }
}

pub struct FakeBackend {
    config: FakeBackendConfig,
    capabilities: BackendCapabilities,
    displays: Vec<DisplayInfo>,
    attempts: u64,
}

impl FakeBackend {
    pub fn new(config: FakeBackendConfig) -> Self {
        let display = DisplayInfo {
            id: DisplayId::primary(),
            name: "Synthetic Display".to_owned(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: config.width,
                height: config.height,
            },
            scale_factor: 1.0,
            rotation_degrees: 0,
            is_primary: true,
        };
        Self {
            config,
            capabilities: BackendCapabilities {
                display_capture: true,
                window_capture: false,
                virtual_desktop_capture: false,
                fresh_mode: true,
                latest_mode: true,
                cursor_control: true,
                hdr: false,
                presentation_time: true,
                warm_stream: true,
            },
            displays: vec![display],
            attempts: 0,
        }
    }
}

impl CaptureBackend for FakeBackend {
    fn name(&self) -> &'static str {
        "fake"
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
        self.attempts = self.attempts.saturating_add(1);
        let started = Instant::now();
        recorder.record(request.id, PerfEventKind::CaptureRequested, 0);

        if self
            .config
            .fail_every
            .is_some_and(|n| n != 0 && self.attempts % n == 0)
        {
            return Err(CaptureError::synthetic("configured deterministic failure"));
        }

        sleep_if_nonzero(self.config.native_delay);
        let native_ready_ns = nanos_u64(request.triggered_at.elapsed().as_nanos());
        recorder.record(request.id, PerfEventKind::NativeFrameReady, native_ready_ns);

        let frame_age_ns = match request.mode {
            CaptureMode::Latest { .. } => Some(nanos_u64(self.config.frame_age.as_nanos())),
            CaptureMode::Fresh { .. } => Some(0),
        };
        let presentation_offset_ns = match request.mode {
            CaptureMode::Latest { .. } => {
                let age = i64::try_from(self.config.frame_age.as_nanos()).unwrap_or(i64::MAX);
                Some(-age)
            }
            CaptureMode::Fresh { .. } => Some(i64::try_from(native_ready_ns).unwrap_or(i64::MAX)),
        };

        let mut metadata = FrameMetadata {
            capture_id: request.id,
            backend: self.name().to_owned(),
            display_id: DisplayId::primary(),
            source_rect: self.displays[0].bounds,
            rotation_degrees: 0,
            capture_mode: request.mode.clone(),
            presentation_offset_ns,
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: native_ready_ns,
            cpu_ready_offset_ns: None,
            frame_age_ns,
            frame_generation: Some(self.attempts),
            copy_count: 0,
            pool_slot: Some((self.attempts % 3) as u16),
        };

        let frame = if request.cpu_frame {
            recorder.record(request.id, PerfEventKind::ReadbackStarted, 0);
            sleep_if_nonzero(self.config.readback_delay);
            let cpu_ready_ns = nanos_u64(request.triggered_at.elapsed().as_nanos());
            metadata.cpu_ready_offset_ns = Some(cpu_ready_ns);
            metadata.copy_count = 1;
            let stride = self
                .config
                .width
                .checked_mul(PixelFormat::Bgra8Unorm.bytes_per_pixel())
                .ok_or_else(|| CaptureError::synthetic("fake frame stride overflow"))?;
            let len = usize::try_from(stride)
                .ok()
                .and_then(|row| {
                    usize::try_from(self.config.height)
                        .ok()
                        .and_then(|height| row.checked_mul(height))
                })
                .ok_or_else(|| CaptureError::synthetic("fake frame size overflow"))?;
            let marker = (request.id.0 & 0xff) as u8;
            let pixels: Arc<[u8]> = vec![marker; len].into();
            let cpu_frame = CpuFrame::new(
                pixels,
                self.config.width,
                self.config.height,
                stride,
                PixelFormat::Bgra8Unorm,
                FrameOrigin::TopLeft,
                ColorSpace::Srgb,
                metadata.clone(),
            )
            .map_err(|error| CaptureError::synthetic(error.to_string()))?;
            recorder.record(request.id, PerfEventKind::CpuFrameReady, cpu_ready_ns);
            Some(cpu_frame)
        } else {
            None
        };

        Ok(CaptureOutcome {
            metadata,
            frame,
            native_frame: None,
            backend_duration: started.elapsed(),
        })
    }
}

fn sleep_if_nonzero(duration: Duration) {
    if !duration.is_zero() {
        thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureId, CaptureSource, CursorMode};

    fn request(mode: CaptureMode) -> CaptureRequest {
        CaptureRequest {
            id: CaptureId(1),
            triggered_at: Instant::now(),
            source: CaptureSource::Display(DisplayId::primary()),
            mode,
            cpu_frame: false,
            retain_native_frame: false,
            cursor: CursorMode::Exclude,
        }
    }

    #[test]
    fn latest_reports_configured_frame_age() {
        let mut backend = FakeBackend::new(FakeBackendConfig {
            native_delay: Duration::ZERO,
            readback_delay: Duration::ZERO,
            frame_age: Duration::from_millis(7),
            ..FakeBackendConfig::default()
        });
        let mut recorder = EventRecorder::with_capacity(8);
        let outcome = backend
            .capture(
                &request(CaptureMode::Latest {
                    max_age_ms: Some(25),
                }),
                &mut recorder,
            )
            .expect("capture");
        assert_eq!(outcome.metadata.frame_age_ns, Some(7_000_000));
        assert_eq!(outcome.metadata.presentation_offset_ns, Some(-7_000_000));
    }

    #[test]
    fn fresh_reports_post_trigger_frame() {
        let mut backend = FakeBackend::new(FakeBackendConfig {
            native_delay: Duration::ZERO,
            readback_delay: Duration::ZERO,
            ..FakeBackendConfig::default()
        });
        let mut recorder = EventRecorder::with_capacity(8);
        let outcome = backend
            .capture(
                &request(CaptureMode::Fresh { timeout_ms: 100 }),
                &mut recorder,
            )
            .expect("capture");
        assert_eq!(outcome.metadata.frame_age_ns, Some(0));
        assert!(outcome.metadata.presentation_offset_ns.unwrap_or_default() >= 0);
    }
}
