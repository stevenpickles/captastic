use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::metrics::nanos_u64;
use crate::{
    BackendCapabilities, CaptureBackend, CaptureError, CaptureErrorKind, CaptureMode,
    CaptureOutcome, CaptureRequest, CaptureSource, ColorSpace, CpuFrame, DisplayId, DisplayInfo,
    EventRecorder, FrameMetadata, FrameOrigin, PerfEventKind, PixelFormat, Rect, TimingProvenance,
};

#[derive(Clone, Debug)]
pub struct FakeBackendConfig {
    pub width: u32,
    pub height: u32,
    pub native_delay: Duration,
    pub readback_delay: Duration,
    pub frame_age: Duration,
    pub fail_every: Option<u64>,
    pub failure_script: Vec<FakeFailure>,
    pub capabilities: BackendCapabilities,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FakeFailure {
    pub attempt: u64,
    pub kind: CaptureErrorKind,
    pub operation: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl FakeFailure {
    pub fn new(attempt: u64, kind: CaptureErrorKind, retryable: bool) -> Self {
        Self {
            attempt,
            kind,
            operation: "capture",
            message: format!("scripted {kind:?} failure on attempt {attempt}"),
            retryable,
        }
    }
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
            failure_script: Vec::new(),
            capabilities: default_capabilities(),
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
        Self::with_displays(config, vec![display])
    }

    /// Creates a deterministic backend with an arbitrary display topology.
    ///
    /// This is intended for platform-neutral tests of selection, rotation metadata, and topology
    /// changes. Callers are responsible for supplying valid display records.
    pub fn with_displays(config: FakeBackendConfig, displays: Vec<DisplayInfo>) -> Self {
        Self {
            capabilities: config.capabilities.clone(),
            config,
            displays,
            attempts: 0,
        }
    }

    fn requested_display(&self, source: &CaptureSource) -> Result<DisplayInfo, CaptureError> {
        let CaptureSource::Display(id) = source else {
            return Err(CaptureError {
                kind: CaptureErrorKind::Unsupported,
                backend: "fake",
                operation: "resolve_display",
                message: "virtual-desktop capture is not supported by the fake backend".to_owned(),
                retryable: false,
                native_code: None,
            });
        };
        let display = if id.is_primary_alias() {
            self.displays
                .iter()
                .find(|display| display.is_primary)
                .or_else(|| self.displays.first())
        } else {
            self.displays.iter().find(|display| display.id == *id)
        };
        display.cloned().ok_or_else(|| CaptureError {
            kind: CaptureErrorKind::SourceUnavailable,
            backend: "fake",
            operation: "resolve_display",
            message: format!("display {} is not attached", id.0),
            retryable: false,
            native_code: None,
        })
    }

    /// Applies the capture mode's timing contract, mirroring what the DXGI backend enforces.
    ///
    /// `Fresh` promises a frame presented after the trigger, within `timeout_ms`. The fake's
    /// `native_delay` is how long acquisition takes, so a delay past the deadline is the same
    /// situation the real backend reports when no frame arrives in time.
    ///
    /// `Latest` promises a retained frame no older than `max_age_ms`. The fake's `frame_age` is
    /// that age directly.
    ///
    /// Both failures are retryable and carry the real backend's operation names, because callers
    /// branch on those: the daemon's recovery path distinguishes a timeout from an outage.
    fn enforce_capture_mode(&self, request: &CaptureRequest) -> Result<(), CaptureError> {
        match request.mode {
            CaptureMode::Fresh { timeout_ms } => {
                let deadline = Duration::from_millis(timeout_ms);
                // Zero means "no waiting is acceptable", which only a zero-cost acquisition meets.
                if self.config.native_delay > deadline {
                    return Err(CaptureError {
                        kind: CaptureErrorKind::Timeout,
                        backend: "fake",
                        operation: "acquire_next_frame",
                        message: format!(
                            "no post-trigger frame arrived within {timeout_ms} ms; the configured native delay is {} ms",
                            self.config.native_delay.as_millis()
                        ),
                        retryable: true,
                        native_code: None,
                    });
                }
            }
            CaptureMode::Latest { max_age_ms } => {
                let Some(max_age_ms) = max_age_ms else {
                    // Age is not a constraint: the documented default, where any retained frame
                    // is acceptable.
                    return Ok(());
                };
                let age_ms = self.config.frame_age.as_millis();
                if age_ms > u128::from(max_age_ms) {
                    return Err(CaptureError {
                        kind: CaptureErrorKind::Timeout,
                        backend: "fake",
                        operation: "capture_latest",
                        message: format!(
                            "retained frame age {age_ms} ms exceeds the configured maximum of {max_age_ms} ms"
                        ),
                        retryable: true,
                        native_code: None,
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_request_capabilities(&self, request: &CaptureRequest) -> Result<(), CaptureError> {
        let supported = match request.mode {
            CaptureMode::Fresh { .. } => self.capabilities.fresh_mode,
            CaptureMode::Latest { .. } => self.capabilities.latest_mode,
        };
        if !supported {
            return Err(self.unsupported(
                "capture_mode",
                format!(
                    "{} mode is disabled by the fake backend capabilities",
                    request.mode.name()
                ),
            ));
        }
        if request.cursor == crate::CursorMode::Include && !self.capabilities.cursor_control {
            return Err(self.unsupported(
                "cursor",
                "cursor inclusion is disabled by the fake backend capabilities",
            ));
        }
        Ok(())
    }

    fn unsupported(&self, operation: &'static str, message: impl Into<String>) -> CaptureError {
        CaptureError {
            kind: CaptureErrorKind::Unsupported,
            backend: "fake",
            operation,
            message: message.into(),
            retryable: false,
            native_code: None,
        }
    }
}

fn default_capabilities() -> BackendCapabilities {
    BackendCapabilities {
        display_capture: true,
        window_capture: false,
        virtual_desktop_capture: false,
        fresh_mode: true,
        latest_mode: true,
        cursor_control: true,
        hdr: false,
        presentation_time: true,
        warm_stream: true,
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
        if !self.capabilities.display_capture {
            return Err(self.unsupported(
                "capture_display",
                "display capture is disabled by the fake backend capabilities",
            ));
        }
        self.validate_request_capabilities(request)?;
        self.attempts = self.attempts.saturating_add(1);
        recorder.record(request.id, PerfEventKind::CaptureRequested, 0);

        // Scripted failures are raised before the display is resolved, because that is the order
        // the real backend works in: `DxgiBackend::capture` compares the display-configuration
        // generation as its very first act and reports `TopologyChanged` without consulting its
        // own display list, precisely because that list is the thing that may have gone stale.
        //
        // The fake used to resolve first, which made it impossible to express the situation a
        // hot-plug creates: a request for a display this backend has never heard of, on a backend
        // whose view of the world has moved. It answered `SourceUnavailable` — "that display does
        // not exist" — where the real backend answers "my topology is stale, rebuild me", and the
        // daemon rebuilds for one and not the other. A fake that cannot express the difference
        // cannot test the recovery that depends on it.
        if let Some(failure) = self
            .config
            .failure_script
            .iter()
            .find(|failure| failure.attempt == self.attempts)
        {
            return Err(CaptureError {
                kind: failure.kind,
                backend: "fake",
                operation: failure.operation,
                message: failure.message.clone(),
                retryable: failure.retryable,
                native_code: None,
            });
        }
        let display = self.requested_display(&request.source)?;

        if self
            .config
            .fail_every
            .is_some_and(|n| n != 0 && self.attempts.is_multiple_of(n))
        {
            return Err(CaptureError::synthetic("configured deterministic failure"));
        }

        // The capture mode's knobs are honored before any frame is produced, because that is what
        // the real backend does: `Fresh` fails when no new frame arrives inside its timeout, and
        // `Latest` fails when the retained frame is older than the caller will accept. A fake that
        // ignored them let the daemon's timeout and staleness handling be "tested" against a
        // backend incapable of exercising either.
        self.enforce_capture_mode(request)?;

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
            display_id: display.id.clone(),
            source_rect: display.bounds,
            rotation_degrees: display.rotation_degrees,
            capture_mode: request.mode.clone(),
            presentation_offset_ns,
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: native_ready_ns,
            cpu_ready_offset_ns: None,
            frame_age_ns,
            frame_generation: Some(self.attempts),
            copy_count: 0,
            pool_slot: Some((self.attempts % 3) as u16),
            cursor: None,
            // The fake presents on demand, so it has nothing it could verify as unchanged.
            verified_current_offset_ns: None,
        };

        let frame = if request.cpu_frame {
            recorder.record(request.id, PerfEventKind::ReadbackStarted, 0);
            sleep_if_nonzero(self.config.readback_delay);
            let cpu_ready_ns = nanos_u64(request.triggered_at.elapsed().as_nanos());
            metadata.cpu_ready_offset_ns = Some(cpu_ready_ns);
            metadata.copy_count = 1;
            let stride = display
                .bounds
                .width
                .checked_mul(PixelFormat::Bgra8Unorm.bytes_per_pixel())
                .ok_or_else(|| CaptureError::synthetic("fake frame stride overflow"))?;
            let len = usize::try_from(stride)
                .ok()
                .and_then(|row| {
                    usize::try_from(display.bounds.height)
                        .ok()
                        .and_then(|height| row.checked_mul(height))
                })
                .ok_or_else(|| CaptureError::synthetic("fake frame size overflow"))?;
            let marker = (request.id.0 & 0xff) as u8;
            let pixels: Arc<[u8]> = vec![marker; len].into();
            let cpu_frame = CpuFrame::new(
                pixels,
                display.bounds.width,
                display.bounds.height,
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
    use std::time::Instant;

    use super::*;
    use crate::{CaptureId, CaptureSource, CursorMode};

    fn request_for(display: DisplayId, mode: CaptureMode) -> CaptureRequest {
        CaptureRequest {
            id: CaptureId(1),
            triggered_at: Instant::now(),
            source: CaptureSource::Display(display),
            mode,
            cpu_frame: false,
            retain_native_frame: false,
            cursor: CursorMode::Exclude,
        }
    }

    fn request(mode: CaptureMode) -> CaptureRequest {
        request_for(DisplayId::primary(), mode)
    }

    #[test]
    fn latest_rejects_a_frame_older_than_the_caller_accepts() {
        // The DXGI contract: a retained frame past `max_age_ms` is a timeout, not a stale frame
        // handed over anyway. The daemon's staleness handling was previously "tested" against a
        // fake that could never produce this.
        let mut backend = FakeBackend::new(FakeBackendConfig {
            native_delay: Duration::ZERO,
            readback_delay: Duration::ZERO,
            frame_age: Duration::from_millis(50),
            ..FakeBackendConfig::default()
        });
        let mut recorder = EventRecorder::with_capacity(8);

        let error = backend
            .capture(
                &request(CaptureMode::Latest {
                    max_age_ms: Some(10),
                }),
                &mut recorder,
            )
            .expect_err("a 50 ms frame does not satisfy a 10 ms maximum");

        assert_eq!(error.kind, CaptureErrorKind::Timeout);
        assert_eq!(error.operation, "capture_latest");
        assert!(error.retryable, "a fresher frame may arrive");
        assert!(error.message.contains("50"), "{}", error.message);
    }

    #[test]
    fn latest_accepts_a_frame_inside_the_maximum_and_any_frame_without_one() {
        let mut backend = FakeBackend::new(FakeBackendConfig {
            native_delay: Duration::ZERO,
            readback_delay: Duration::ZERO,
            frame_age: Duration::from_millis(50),
            ..FakeBackendConfig::default()
        });
        let mut recorder = EventRecorder::with_capacity(16);

        // Exactly at the limit is inside it.
        backend
            .capture(
                &request(CaptureMode::Latest {
                    max_age_ms: Some(50),
                }),
                &mut recorder,
            )
            .expect("a frame at the limit is acceptable");
        // `None` is the documented default: age is not a constraint at all.
        backend
            .capture(
                &request(CaptureMode::Latest { max_age_ms: None }),
                &mut recorder,
            )
            .expect("no maximum accepts any retained frame");
    }

    #[test]
    fn fresh_rejects_an_acquisition_that_misses_its_deadline() {
        // `Fresh` promises a frame presented after the trigger, within the timeout. A backend that
        // cannot produce one in time reports a retryable timeout rather than an older frame.
        let mut backend = FakeBackend::new(FakeBackendConfig {
            native_delay: Duration::from_millis(40),
            readback_delay: Duration::ZERO,
            ..FakeBackendConfig::default()
        });
        let mut recorder = EventRecorder::with_capacity(8);

        let error = backend
            .capture(
                &request(CaptureMode::Fresh { timeout_ms: 5 }),
                &mut recorder,
            )
            .expect_err("40 ms of acquisition does not fit in 5 ms");

        assert_eq!(error.kind, CaptureErrorKind::Timeout);
        assert_eq!(error.operation, "acquire_next_frame");
        assert!(error.retryable);

        // The same backend succeeds when the caller waits long enough, so the failure is about
        // the deadline rather than the backend being broken.
        backend
            .capture(
                &request(CaptureMode::Fresh { timeout_ms: 100 }),
                &mut recorder,
            )
            .expect("a deadline the acquisition fits inside");
    }

    #[test]
    fn a_mode_timeout_costs_nothing_and_leaves_no_frame_events() {
        // The real backend fails before producing a frame, so a timeout must not record
        // NativeFrameReady or pay the configured delay — otherwise a test measuring recovery
        // latency would measure the fake's own sleep.
        let mut backend = FakeBackend::new(FakeBackendConfig {
            native_delay: Duration::from_millis(200),
            readback_delay: Duration::from_millis(200),
            ..FakeBackendConfig::default()
        });
        let mut recorder = EventRecorder::with_capacity(8);

        let started = Instant::now();
        let _ = backend
            .capture(
                &request(CaptureMode::Fresh { timeout_ms: 1 }),
                &mut recorder,
            )
            .expect_err("the deadline is missed");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "a timeout waited for the delay it was supposed to reject"
        );
        assert!(
            !recorder
                .events()
                .iter()
                .any(|event| event.kind == PerfEventKind::NativeFrameReady),
            "a frame that never arrived must not be recorded as ready"
        );
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

    #[test]
    fn captures_the_requested_display_with_its_native_dimensions_and_rotation() {
        let portrait = DisplayInfo {
            id: DisplayId("portrait".to_owned()),
            name: "Portrait".to_owned(),
            bounds: Rect {
                x: 1920,
                y: -200,
                width: 1080,
                height: 1920,
            },
            scale_factor: 1.5,
            rotation_degrees: 90,
            is_primary: false,
        };
        let primary = DisplayInfo {
            id: DisplayId("main".to_owned()),
            name: "Main".to_owned(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale_factor: 1.0,
            rotation_degrees: 0,
            is_primary: true,
        };
        let mut backend = FakeBackend::with_displays(
            FakeBackendConfig {
                native_delay: Duration::ZERO,
                readback_delay: Duration::ZERO,
                ..FakeBackendConfig::default()
            },
            vec![primary, portrait],
        );
        let mut recorder = EventRecorder::with_capacity(8);
        let mut capture = request_for(
            DisplayId("portrait".to_owned()),
            CaptureMode::Latest { max_age_ms: None },
        );
        capture.cpu_frame = true;
        let outcome = backend.capture(&capture, &mut recorder).expect("capture");
        assert_eq!(outcome.metadata.display_id.0, "portrait");
        assert_eq!(outcome.metadata.source_rect, portrait_bounds());
        assert_eq!(outcome.metadata.rotation_degrees, 90);
        let frame = outcome.frame.expect("CPU frame");
        assert_eq!(frame.width(), 1080);
        assert_eq!(frame.height(), 1920);
    }

    #[test]
    fn a_stale_topology_is_reported_before_the_display_is_looked_up() {
        // Fidelity with `DxgiBackend::capture`, which compares the display-configuration
        // generation as its first act and reports TopologyChanged without consulting its own
        // display list — because that list is exactly what may have gone stale.
        //
        // The ordering is the contract, not an implementation detail: the daemon rebuilds for
        // TopologyChanged and does not for SourceUnavailable, so a fake that answered "no such
        // display" here would test the wrong recovery. Asking for a display this backend has never
        // heard of is the sharpest way to state it.
        let mut backend = FakeBackend::with_displays(
            FakeBackendConfig {
                failure_script: vec![FakeFailure::new(1, CaptureErrorKind::TopologyChanged, true)],
                ..FakeBackendConfig::default()
            },
            vec![display("built-in", true)],
        );
        let mut recorder = EventRecorder::with_capacity(8);

        let error = backend
            .capture(
                &request_for(
                    DisplayId("never-heard-of".to_owned()),
                    CaptureMode::Latest { max_age_ms: None },
                ),
                &mut recorder,
            )
            .expect_err("the scripted failure is raised");
        assert_eq!(error.kind, CaptureErrorKind::TopologyChanged);

        // With the script spent, the same request gets the answer about the display itself.
        let error = backend
            .capture(
                &request_for(
                    DisplayId("never-heard-of".to_owned()),
                    CaptureMode::Latest { max_age_ms: None },
                ),
                &mut recorder,
            )
            .expect_err("an absent display is still absent");
        assert_eq!(error.kind, CaptureErrorKind::SourceUnavailable);
    }

    #[test]
    fn primary_alias_resolves_to_the_enumerated_primary_display() {
        let displays = vec![
            display("secondary", false),
            display("persistent-primary", true),
        ];
        let mut backend = FakeBackend::with_displays(FakeBackendConfig::default(), displays);
        let mut recorder = EventRecorder::with_capacity(8);
        let outcome = backend
            .capture(
                &request(CaptureMode::Latest { max_age_ms: None }),
                &mut recorder,
            )
            .expect("capture");
        assert_eq!(outcome.metadata.display_id.0, "persistent-primary");
    }

    #[test]
    fn unavailable_display_fails_before_frame_acquisition() {
        let mut backend = FakeBackend::new(FakeBackendConfig::default());
        let mut recorder = EventRecorder::with_capacity(8);
        let error = backend
            .capture(
                &request_for(
                    DisplayId("missing".to_owned()),
                    CaptureMode::Latest { max_age_ms: None },
                ),
                &mut recorder,
            )
            .expect_err("missing display must fail");
        assert_eq!(error.kind, CaptureErrorKind::SourceUnavailable);
        assert_eq!(error.operation, "resolve_display");
    }

    #[test]
    fn scripted_failures_preserve_kind_retryability_and_attempt_order() {
        let mut backend = FakeBackend::new(FakeBackendConfig {
            native_delay: Duration::ZERO,
            readback_delay: Duration::ZERO,
            failure_script: vec![
                FakeFailure::new(2, CaptureErrorKind::Timeout, true),
                FakeFailure {
                    attempt: 3,
                    kind: CaptureErrorKind::PermissionDenied,
                    operation: "acquire_frame",
                    message: "scripted permission loss".to_owned(),
                    retryable: false,
                },
            ],
            ..FakeBackendConfig::default()
        });
        let mut recorder = EventRecorder::with_capacity(16);

        backend
            .capture(
                &request(CaptureMode::Latest { max_age_ms: None }),
                &mut recorder,
            )
            .expect("first attempt succeeds");
        let timeout = backend
            .capture(
                &request(CaptureMode::Latest { max_age_ms: None }),
                &mut recorder,
            )
            .expect_err("second attempt is scripted to time out");
        assert_eq!(timeout.kind, CaptureErrorKind::Timeout);
        assert!(timeout.retryable);

        let denied = backend
            .capture(
                &request(CaptureMode::Latest { max_age_ms: None }),
                &mut recorder,
            )
            .expect_err("third attempt is scripted to lose permission");
        assert_eq!(denied.kind, CaptureErrorKind::PermissionDenied);
        assert_eq!(denied.operation, "acquire_frame");
        assert!(!denied.retryable);
    }

    #[test]
    fn configured_capability_limits_are_reported_and_enforced() {
        let mut config = FakeBackendConfig::default();
        config.capabilities.latest_mode = false;
        let mut backend = FakeBackend::new(config);
        let mut recorder = EventRecorder::with_capacity(8);

        assert!(!backend.capabilities().latest_mode);
        let error = backend
            .capture(
                &request(CaptureMode::Latest { max_age_ms: None }),
                &mut recorder,
            )
            .expect_err("disabled latest mode must fail");
        assert_eq!(error.kind, CaptureErrorKind::Unsupported);
        assert_eq!(error.operation, "capture_mode");
        assert!(!error.retryable);
        assert_eq!(backend.attempts, 0);
    }

    fn display(id: &str, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
            scale_factor: 1.0,
            rotation_degrees: 0,
            is_primary: primary,
        }
    }

    fn portrait_bounds() -> Rect {
        Rect {
            x: 1920,
            y: -200,
            width: 1080,
            height: 1920,
        }
    }
}
