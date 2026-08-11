use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use captastic_core::{
    trigger_queue, validate_event_order, CaptureBackend, CaptureErrorKind, CaptureId, CaptureMode,
    CaptureRequest, CaptureSource, CursorMode, DisplayId, EventRecorder, FakeBackend,
    FakeBackendConfig, LatencySummary, PerfEvent, PerfEventKind,
};
use serde::Serialize;

use crate::error::AppError;

#[derive(Clone, Debug)]
pub struct BenchmarkOptions {
    pub iterations: usize,
    pub warmup: usize,
    pub mode: CaptureMode,
    pub cpu_frame: bool,
    pub display_id: DisplayId,
    pub trigger_queue_capacity: usize,
    pub metrics_capacity: usize,
    pub fake: FakeBackendConfig,
}

#[derive(Debug, Serialize)]
pub struct EnvironmentFingerprint {
    pub os: &'static str,
    pub architecture: &'static str,
    pub package_version: &'static str,
    pub debug_assertions: bool,
    pub displays: Vec<DisplayFingerprint>,
}

#[derive(Debug, Serialize)]
pub struct DisplayFingerprint {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub rotation_degrees: u16,
    pub primary: bool,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkReport {
    pub schema_version: u32,
    pub backend: &'static str,
    pub mode: String,
    pub synthetic: bool,
    pub warmup_iterations: usize,
    pub timed_iterations: usize,
    pub successes: usize,
    pub failures: usize,
    pub timeouts: usize,
    pub failures_by_kind: BTreeMap<&'static str, usize>,
    pub trigger_to_dequeue_latency: LatencySummary,
    pub native_frame_latency: LatencySummary,
    pub cpu_frame_latency: Option<LatencySummary>,
    pub readback_latency: Option<LatencySummary>,
    pub frame_age: LatencySummary,
    pub lost_metric_events: u64,
    pub critical_path_order_verified: bool,
    pub environment: EnvironmentFingerprint,
}

pub struct BenchmarkRun {
    pub report: BenchmarkReport,
    pub events: Vec<PerfEvent>,
}

pub fn run(options: &BenchmarkOptions) -> Result<BenchmarkRun, AppError> {
    let mut backend = FakeBackend::new(options.fake.clone());
    run_with_backend(&mut backend, options)
}

pub fn run_with_backend(
    backend: &mut dyn CaptureBackend,
    options: &BenchmarkOptions,
) -> Result<BenchmarkRun, AppError> {
    if options.iterations == 0 {
        return Err(AppError::InvalidArgument(
            "iterations must be greater than zero".to_owned(),
        ));
    }
    let mut warmup_recorder = EventRecorder::with_capacity(options.warmup.saturating_mul(10));
    for index in 0..options.warmup {
        let request = request(
            index as u64,
            &options.display_id,
            &options.mode,
            options.cpu_frame,
        );
        let _ = backend.capture(&request, &mut warmup_recorder);
    }

    let trigger_queue = trigger_queue(options.trigger_queue_capacity)?;
    let mut recorder = EventRecorder::with_capacity(options.metrics_capacity);
    let mut native_samples = Vec::with_capacity(options.iterations);
    let mut cpu_samples = Vec::with_capacity(options.iterations);
    let mut readback_samples = Vec::with_capacity(options.iterations);
    let mut trigger_to_dequeue_samples = Vec::with_capacity(options.iterations);
    let mut frame_age_samples = Vec::with_capacity(options.iterations);
    let mut failures = 0_usize;
    let mut timeouts = 0_usize;
    let mut failures_by_kind = BTreeMap::new();
    let mut successful_ids = Vec::with_capacity(options.iterations);

    for index in 0..options.iterations {
        let capture_id = CaptureId(index as u64 + 1);
        recorder.record(capture_id, PerfEventKind::HotkeyReceived, 0);
        let request = request(
            capture_id.0,
            &options.display_id,
            &options.mode,
            options.cpu_frame,
        );
        let triggered_at = request.triggered_at;
        trigger_queue.try_send(request)?;
        let enqueued_ns = duration_ns(triggered_at.elapsed());
        recorder.record(capture_id, PerfEventKind::TriggerEnqueued, enqueued_ns);
        let request = trigger_queue.try_recv()?;
        let dequeued_ns = duration_ns(request.triggered_at.elapsed());
        recorder.record(capture_id, PerfEventKind::TriggerDequeued, dequeued_ns);
        trigger_to_dequeue_samples.push(dequeued_ns);
        match backend.capture(&request, &mut recorder) {
            Ok(outcome) => {
                successful_ids.push(capture_id);
                native_samples.push(outcome.metadata.native_ready_offset_ns);
                if let Some(value) = outcome.metadata.cpu_ready_offset_ns {
                    cpu_samples.push(value);
                    readback_samples
                        .push(value.saturating_sub(outcome.metadata.native_ready_offset_ns));
                }
                if let Some(value) = outcome.metadata.frame_age_ns {
                    frame_age_samples.push(value);
                }
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                if error.kind == CaptureErrorKind::Timeout {
                    timeouts = timeouts.saturating_add(1);
                }
                let label = capture_error_kind_label(error.kind);
                *failures_by_kind.entry(label).or_insert(0) += 1;
            }
        }
        recorder.record(capture_id, PerfEventKind::AttemptFinished, 0);
    }

    validate_event_order(recorder.events())?;
    let lost_metric_events = recorder.lost_events();
    let critical_path_order_verified = lost_metric_events == 0
        && successful_ids
            .iter()
            .all(|id| has_complete_capture_path(recorder.events(), *id, options.cpu_frame));
    let events = recorder.into_events();
    let successes = options.iterations.saturating_sub(failures);
    let report = BenchmarkReport {
        schema_version: 1,
        backend: backend.name(),
        mode: options.mode.name().to_owned(),
        synthetic: backend.name() == "fake",
        warmup_iterations: options.warmup,
        timed_iterations: options.iterations,
        successes,
        failures,
        timeouts,
        failures_by_kind,
        trigger_to_dequeue_latency: LatencySummary::from_samples(&trigger_to_dequeue_samples),
        native_frame_latency: LatencySummary::from_samples(&native_samples),
        cpu_frame_latency: options
            .cpu_frame
            .then(|| LatencySummary::from_samples(&cpu_samples)),
        readback_latency: options
            .cpu_frame
            .then(|| LatencySummary::from_samples(&readback_samples)),
        frame_age: LatencySummary::from_samples(&frame_age_samples),
        lost_metric_events,
        critical_path_order_verified,
        environment: EnvironmentFingerprint {
            os: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            package_version: env!("CARGO_PKG_VERSION"),
            debug_assertions: cfg!(debug_assertions),
            displays: backend
                .displays()
                .iter()
                .map(|display| DisplayFingerprint {
                    id: display.id.0.clone(),
                    name: display.name.clone(),
                    width: display.bounds.width,
                    height: display.bounds.height,
                    rotation_degrees: display.rotation_degrees,
                    primary: display.is_primary,
                })
                .collect(),
        },
    };
    Ok(BenchmarkRun { report, events })
}

fn has_complete_capture_path(events: &[PerfEvent], id: CaptureId, cpu_frame: bool) -> bool {
    let kinds: HashSet<PerfEventKind> = events
        .iter()
        .filter(|event| event.capture_id == id)
        .map(|event| event.kind)
        .collect();
    let required = [
        PerfEventKind::HotkeyReceived,
        PerfEventKind::TriggerEnqueued,
        PerfEventKind::TriggerDequeued,
        PerfEventKind::CaptureRequested,
        PerfEventKind::NativeFrameReady,
        PerfEventKind::AttemptFinished,
    ];
    required.iter().all(|kind| kinds.contains(kind))
        && (!cpu_frame
            || (kinds.contains(&PerfEventKind::ReadbackStarted)
                && kinds.contains(&PerfEventKind::CpuFrameReady)))
}

fn capture_error_kind_label(kind: CaptureErrorKind) -> &'static str {
    match kind {
        CaptureErrorKind::Unsupported => "unsupported",
        CaptureErrorKind::PermissionDenied => "permission_denied",
        CaptureErrorKind::SourceUnavailable => "source_unavailable",
        CaptureErrorKind::Timeout => "timeout",
        CaptureErrorKind::AccessLost => "access_lost",
        CaptureErrorKind::DeviceRemoved => "device_removed",
        CaptureErrorKind::TopologyChanged => "topology_changed",
        CaptureErrorKind::BufferExhausted => "buffer_exhausted",
        CaptureErrorKind::InvalidFrame => "invalid_frame",
        CaptureErrorKind::NativeFailure => "native_failure",
        CaptureErrorKind::ShuttingDown => "shutting_down",
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(path, bytes).map_err(|source| AppError::Write {
        path: path.display().to_string(),
        source,
    })
}

pub fn write_json_lines(path: &Path, events: &[PerfEvent]) -> Result<(), AppError> {
    let mut output = String::new();
    for event in events {
        output.push_str(&serde_json::to_string(event)?);
        output.push('\n');
    }
    fs::write(path, output).map_err(|source| AppError::Write {
        path: path.display().to_string(),
        source,
    })
}

fn request(id: u64, display_id: &DisplayId, mode: &CaptureMode, cpu_frame: bool) -> CaptureRequest {
    CaptureRequest {
        id: CaptureId(id),
        triggered_at: Instant::now(),
        source: CaptureSource::Display(display_id.clone()),
        mode: mode.clone(),
        cpu_frame,
        retain_native_frame: false,
        cursor: CursorMode::Exclude,
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

pub fn fake_config(native_us: u64, readback_us: u64, frame_age_us: u64) -> FakeBackendConfig {
    FakeBackendConfig {
        native_delay: Duration::from_micros(native_us),
        readback_delay: Duration::from_micros(readback_us),
        frame_age: Duration::from_micros(frame_age_us),
        ..FakeBackendConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_benchmark_has_complete_samples() {
        let run = run(&BenchmarkOptions {
            iterations: 5,
            warmup: 1,
            mode: CaptureMode::Latest {
                max_age_ms: Some(25),
            },
            cpu_frame: true,
            display_id: DisplayId::primary(),
            trigger_queue_capacity: 1,
            metrics_capacity: 100,
            fake: fake_config(0, 0, 1_000),
        })
        .expect("benchmark succeeds");
        assert_eq!(run.report.successes, 5);
        assert_eq!(run.report.trigger_to_dequeue_latency.count, 5);
        assert_eq!(run.report.native_frame_latency.count, 5);
        assert_eq!(run.report.cpu_frame_latency.expect("CPU summary").count, 5);
        assert_eq!(
            run.report.readback_latency.expect("readback summary").count,
            5
        );
        assert!(run.report.critical_path_order_verified);
    }

    #[test]
    fn critical_path_requires_every_mandatory_event() {
        let id = CaptureId(7);
        let events = [PerfEvent {
            capture_id: id,
            kind: PerfEventKind::HotkeyReceived,
            ticks_ns: 0,
            value: 0,
        }];
        assert!(!has_complete_capture_path(&events, id, true));
    }
}
