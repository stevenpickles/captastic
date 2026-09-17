use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use captastic_core::{
    validate_event_order, CaptureBackend, CaptureErrorKind, CaptureId, CaptureMode, CaptureRequest,
    CaptureSource, CursorMode, EventRecorder, FakeBackend, FakeBackendConfig, LatencySummary,
    PerfEvent, PerfEventKind,
};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::fingerprint::EnvironmentFingerprint;

#[derive(Clone, Debug)]
pub struct BenchmarkOptions {
    pub iterations: usize,
    pub warmup: usize,
    pub mode: CaptureMode,
    pub cpu_frame: bool,
    pub source: CaptureSource,
    pub trigger_queue_capacity: usize,
    pub metrics_capacity: usize,
    /// Whether the pointer is composited into each capture.
    ///
    /// An option rather than a constant because composition is work: a shape lookup and a blend
    /// over the pointer rectangle, on the capture thread. Milestone 5 asks for cursor-on and
    /// cursor-off to be measured *separately*, and a benchmark that can only produce one of them
    /// cannot answer what the other costs.
    pub cursor: CursorMode,
    pub fake: FakeBackendConfig,
}

/// A benchmark run, as it is written out and as it is read back.
///
/// Owned strings rather than `&'static str` throughout, including the map keys: a report nothing
/// can deserialize is a report nothing can compare against, and comparing a run to a baseline is
/// the whole point of recording one. Nothing can turn a JSON string back into a `&'static str`
/// without leaking it, so the borrow had to go. The field names and their order are unchanged, so
/// the JSON a run emits is what it always was plus the new fingerprint fields.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BenchmarkReport {
    pub schema_version: u32,
    pub backend: String,
    pub mode: String,
    /// `include` or `exclude`. Recorded because two runs that differ only in this are the pair the
    /// cursor criterion asks for, and a result file that does not say which it is cannot be paired.
    pub cursor: String,
    pub synthetic: bool,
    pub warmup_iterations: usize,
    pub timed_iterations: usize,
    pub successes: usize,
    pub failures: usize,
    pub timeouts: usize,
    pub failures_by_kind: BTreeMap<String, usize>,
    /// What became of the pointer in each successful capture, counted by outcome.
    ///
    /// Without this a cursor-on run is indistinguishable from a cursor-off one: every capture
    /// succeeds either way, and a capture that declined to composite - because the compositor had
    /// not reported the pointer, or reported it hidden - looks exactly like one that drew it. Two
    /// separate measurements of the same thing is the failure this exists to make visible, and it
    /// is the failure that actually happened here twice before it was noticed.
    pub cursor_outcomes: BTreeMap<String, usize>,
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

/// What has to match before two runs may be compared.
///
/// "Three compatible repeat runs support every published performance claim" turns on the word
/// *compatible*. Averaging a run from a debug build with two from a release build, or a 4K run
/// with two at 1080p, produces a number that describes nothing — and does it silently, which is
/// the failure worth engineering against. So comparability is decided explicitly and a mismatch
/// is named rather than absorbed.
///
/// Deliberately not part of it: iteration counts, timings, and anything the run measured. Those
/// are the outputs. This is only about whether the runs were asking the same question.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunCompatibility {
    pub backend: String,
    pub mode: String,
    pub cursor: String,
    pub cpu_frame: bool,
    pub synthetic: bool,
    /// The whole build, not just its version.
    ///
    /// Two development builds of the same `0.2.0-dev` version can be a hundred commits and an
    /// optimization level apart, and they compared as compatible. Version, short commit, profile,
    /// target and the dirty flag together are what "the same software" actually means; a dirty
    /// tree is in it because uncommitted changes are exactly the ones nobody can reconstruct
    /// afterwards from a commit id.
    pub build: String,
    pub debug_assertions: bool,
    /// Each attached display as `id:WxH@rotation:scale:refresh`.
    ///
    /// Scale and refresh are here because both change the measurement without changing the
    /// geometry: composition works in physical pixels, and a 60 Hz panel and a 144 Hz one present
    /// on different cadences.
    pub displays: Vec<String>,
    /// Each adapter as `description (driver version)`.
    ///
    /// The driver is the part that moves. A driver update is the single likeliest cause of a
    /// capture latency change between two runs a week apart on the same machine, and without it
    /// the two compare as identical hosts.
    pub adapters: Vec<String>,
    pub os_build: Option<String>,
    /// The session's state. A run over RDP or against a locked desktop measures something else.
    pub session: Option<String>,
    /// `ac` or `battery`: the same laptop measures differently on each, silently.
    pub power_source: Option<String>,
}

impl RunCompatibility {
    fn of(report: &BenchmarkReport) -> Self {
        Self {
            backend: report.backend.clone(),
            mode: report.mode.clone(),
            cursor: report.cursor.clone(),
            cpu_frame: report.cpu_frame_latency.is_some(),
            synthetic: report.synthetic,
            build: build_identity(&report.environment.build),
            debug_assertions: report.environment.debug_assertions,
            displays: report
                .environment
                .displays
                .iter()
                .map(|display| {
                    format!(
                        "{}:{}x{}@{}:{:.2}x:{}",
                        display.id,
                        display.width,
                        display.height,
                        display.rotation_degrees,
                        display.scale_factor,
                        display
                            .refresh_hz
                            .map(|hz| format!("{hz:.3}Hz"))
                            .unwrap_or_else(|| UNKNOWN.to_owned())
                    )
                })
                .collect(),
            adapters: report
                .environment
                .adapters
                .iter()
                .map(|adapter| {
                    format!(
                        "{} ({})",
                        adapter.description,
                        adapter.driver_version.as_deref().unwrap_or(UNKNOWN)
                    )
                })
                .collect(),
            os_build: report.environment.os_build.clone(),
            session: report.environment.session.clone(),
            power_source: report.environment.power_source.clone(),
        }
    }

    /// Names every field that differs, so a refusal to compare says what to fix.
    fn differences(&self, other: &Self) -> Vec<String> {
        let mut differences = Vec::new();
        let mut note = |field: &str, first: String, second: String| {
            if first != second {
                differences.push(format!("{field} ({first} vs {second})"));
            }
        };
        note("backend", self.backend.clone(), other.backend.clone());
        note("mode", self.mode.clone(), other.mode.clone());
        note("cursor", self.cursor.clone(), other.cursor.clone());
        note(
            "cpu_frame",
            self.cpu_frame.to_string(),
            other.cpu_frame.to_string(),
        );
        note(
            "synthetic",
            self.synthetic.to_string(),
            other.synthetic.to_string(),
        );
        note("build", self.build.clone(), other.build.clone());
        note(
            "debug_assertions",
            self.debug_assertions.to_string(),
            other.debug_assertions.to_string(),
        );
        note(
            "displays",
            self.displays.join(","),
            other.displays.join(","),
        );
        note(
            "adapters",
            self.adapters.join(","),
            other.adapters.join(","),
        );
        note(
            "os_build",
            unknown_if_absent(&self.os_build),
            unknown_if_absent(&other.os_build),
        );
        note(
            "session",
            unknown_if_absent(&self.session),
            unknown_if_absent(&other.session),
        );
        note(
            "power_source",
            unknown_if_absent(&self.power_source),
            unknown_if_absent(&other.power_source),
        );
        differences
    }
}

/// What a fact the host would not answer is called, everywhere a comparison has to print one.
const UNKNOWN: &str = "unknown";

fn unknown_if_absent(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| UNKNOWN.to_owned())
}

/// The whole build as one comparable line: version, commit, profile, target, and dirtiness.
///
/// A version alone let two development builds a hundred commits apart compare as the same
/// software, which is the silent version of the failure this module exists to make loud.
fn build_identity(build: &crate::fingerprint::BuildIdentity) -> String {
    format!(
        "{} ({}, {}, {}, {})",
        build.version,
        build.git_short_commit.as_deref().unwrap_or(UNKNOWN),
        build.profile,
        build.target,
        if build.dirty { "dirty" } else { "clean" }
    )
}

/// Several timed runs of the same question, and what they agree on.
#[derive(Debug, Deserialize, Serialize)]
pub struct RepeatedBenchmark {
    pub schema_version: u32,
    pub runs: Vec<BenchmarkReport>,
    pub compatibility: RunCompatibility,
    /// Empty when every run matched. Populated, and the summary withheld, when one did not.
    pub incompatibilities: Vec<String>,
    /// Present only when the runs are compatible: a claim needs runs that measured the same thing.
    pub agreement: Option<RepeatAgreement>,
}

/// How closely the repeats agreed, which is the part a performance claim rests on.
///
/// The spread matters more than the average. Three runs whose medians differ by 40% do not
/// support a claim however good the mean looks, and reporting only a mean would hide exactly that.
#[derive(Debug, Deserialize, Serialize)]
pub struct RepeatAgreement {
    pub runs: usize,
    pub native_p50_ns: Vec<u64>,
    pub native_p50_spread_percent: f64,
    pub cpu_p50_ns: Vec<u64>,
    pub cpu_p50_spread_percent: f64,
    pub total_successes: usize,
    pub total_failures: usize,
    /// Every stage the pipeline reports, each with its own spread.
    ///
    /// The two fields above cover the two stages a claim has historically quoted, and they stay
    /// because tooling and the human summary read them by name. They are not enough to accept a
    /// run set: the acceptance rule is "every stage agreed", and a native frame that repeated to
    /// within 2 % beside a trigger-to-dequeue that swung by 60 % is a set whose quiet stage was
    /// the only one anybody looked at.
    pub stages: Vec<StageAgreement>,
}

/// One pipeline stage across the repeats, at each percentile a claim is quoted at.
///
/// p95 and p99 are here because the spread of the tail is not the spread of the median. A capture
/// tool's felt regression is an occasional stutter, so a set whose medians agree and whose tails do
/// not is exactly the set that must not be accepted on the strength of its medians.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StageAgreement {
    /// `trigger_to_dequeue`, `native_frame`, `cpu_frame`, `readback`, or `frame_age`.
    pub stage: String,
    pub p50_ns: Vec<u64>,
    pub p50_spread_percent: f64,
    pub p95_ns: Vec<u64>,
    pub p95_spread_percent: f64,
    pub p99_ns: Vec<u64>,
    pub p99_spread_percent: f64,
}

/// The five latency stages a report carries, in pipeline order, named once.
///
/// One list rather than five hand-written field accesses per consumer: a stage added to the report
/// and forgotten in the agreement summary is a stage nobody checks the spread of, and `compare`
/// would silently stop comparing it too.
pub(crate) fn stage_summaries(
    report: &BenchmarkReport,
) -> [(&'static str, Option<&LatencySummary>); 5] {
    [
        (
            "trigger_to_dequeue",
            Some(&report.trigger_to_dequeue_latency),
        ),
        ("native_frame", Some(&report.native_frame_latency)),
        ("cpu_frame", report.cpu_frame_latency.as_ref()),
        ("readback", report.readback_latency.as_ref()),
        ("frame_age", Some(&report.frame_age)),
    ]
}

/// The per-stage agreement of a set of runs, skipping stages no run measured.
fn stage_agreements(runs: &[BenchmarkReport]) -> Vec<StageAgreement> {
    let Some(first) = runs.first() else {
        return Vec::new();
    };
    let mut agreements = Vec::new();
    for (index, (stage, _)) in stage_summaries(first).into_iter().enumerate() {
        let summaries: Vec<&LatencySummary> = runs
            .iter()
            .filter_map(|run| stage_summaries(run)[index].1)
            .collect();
        if summaries.is_empty() {
            // `--cpu-frame false` leaves two of the five unmeasured. Reporting them as a spread of
            // zero would read as perfect agreement about nothing.
            continue;
        }
        let p50: Vec<u64> = summaries.iter().map(|summary| summary.p50_ns).collect();
        let p95: Vec<u64> = summaries.iter().map(|summary| summary.p95_ns).collect();
        let p99: Vec<u64> = summaries.iter().map(|summary| summary.p99_ns).collect();
        agreements.push(StageAgreement {
            stage: stage.to_owned(),
            p50_spread_percent: spread_percent(&p50),
            p50_ns: p50,
            p95_spread_percent: spread_percent(&p95),
            p95_ns: p95,
            p99_spread_percent: spread_percent(&p99),
            p99_ns: p99,
        });
    }
    agreements
}

/// Spread as a percentage of the smallest sample, or zero when there is nothing to compare.
pub(crate) fn spread_percent(samples: &[u64]) -> f64 {
    let Some(smallest) = samples.iter().copied().min() else {
        return 0.0;
    };
    let largest = samples.iter().copied().max().unwrap_or(smallest);
    if smallest == 0 {
        // A zero floor makes a percentage meaningless rather than infinite; the samples are
        // reported alongside so the reader can see what happened.
        return 0.0;
    }
    ((largest - smallest) as f64 / smallest as f64) * 100.0
}

/// A repeat set and the raw events each of its runs produced.
///
/// The events travel beside the reports rather than inside them because a report is a file on
/// disk with a fixed shape, and an event stream is hundreds of thousands of lines. `events[i]`
/// belongs to `repeated.runs[i]`.
pub struct RepeatedBenchmarkRuns {
    pub repeated: RepeatedBenchmark,
    pub events: Vec<Vec<PerfEvent>>,
}

/// A repeat set as it is written to `repeated.json` and read back by `benchmark compare`.
///
/// A typed envelope rather than the hand-built JSON object this used to print: the file an
/// operator commits as a baseline and the JSON the command prints are now the same struct, so
/// they cannot drift apart, and the file has a schema version like everything else that is kept.
#[derive(Debug, Deserialize, Serialize)]
pub struct RepeatedBenchmarkFile {
    pub schema_version: u32,
    pub repeated: RepeatedBenchmark,
    /// The budget verdict per run, when `--budgets` was given. `null` when it was not.
    pub budgets: Option<Vec<crate::budget::BudgetOutcome>>,
}

/// The schema version of `repeated.json` and of the `--repeat` stdout envelope.
pub const REPEATED_FILE_SCHEMA_VERSION: u32 = 1;

/// Runs the benchmark `repeat` times and reports whether the results may be compared at all.
pub fn run_repeated(
    options: &BenchmarkOptions,
    repeat: usize,
    mut make_backend: impl FnMut() -> Result<Box<dyn CaptureBackend>, AppError>,
) -> Result<RepeatedBenchmarkRuns, AppError> {
    if repeat == 0 {
        return Err(AppError::InvalidArgument(
            "repeat must be greater than zero".to_owned(),
        ));
    }
    let mut runs = Vec::with_capacity(repeat);
    let mut events = Vec::with_capacity(repeat);
    for _ in 0..repeat {
        // A fresh backend per run, because a warm one is a different measurement: the first
        // capture allocates a staging texture and a CPU slot, and reusing one across repeats
        // would hide that cost in every run but the first.
        let mut backend = make_backend()?;
        // The events are kept, not dropped. Discarding them here is what made `--raw-events`
        // silently do nothing under `--repeat`, which is the combination an operator collecting
        // evidence actually runs: the flag was accepted, no file appeared, and nothing said why.
        let run = run_with_backend(backend.as_mut(), options)?;
        runs.push(run.report);
        events.push(run.events);
    }

    let compatibility = RunCompatibility::of(&runs[0]);
    let mut incompatibilities = Vec::new();
    for (index, report) in runs.iter().enumerate().skip(1) {
        for difference in compatibility.differences(&RunCompatibility::of(report)) {
            incompatibilities.push(format!("run {} differs in {difference}", index + 1));
        }
    }

    let agreement = incompatibilities.is_empty().then(|| {
        let native: Vec<u64> = runs
            .iter()
            .map(|run| run.native_frame_latency.p50_ns)
            .collect();
        let cpu: Vec<u64> = runs
            .iter()
            .filter_map(|run| run.cpu_frame_latency.as_ref().map(|summary| summary.p50_ns))
            .collect();
        RepeatAgreement {
            runs: runs.len(),
            native_p50_spread_percent: spread_percent(&native),
            native_p50_ns: native,
            cpu_p50_spread_percent: spread_percent(&cpu),
            cpu_p50_ns: cpu,
            total_successes: runs.iter().map(|run| run.successes).sum(),
            total_failures: runs.iter().map(|run| run.failures).sum(),
            stages: stage_agreements(&runs),
        }
    });

    Ok(RepeatedBenchmarkRuns {
        repeated: RepeatedBenchmark {
            schema_version: 1,
            runs,
            compatibility,
            incompatibilities,
            agreement,
        },
        events,
    })
}

/// Writes every repeat's raw artifacts into one directory, plus the typed set file.
///
/// One directory per set rather than one file per flag: the evidence for a published figure is the
/// runs *and* their event streams *and* the budget verdict, and a baseline committed to the
/// repository as three unrelated paths is one somebody reassembles by hand a year later.
///
/// `run-N.json` is a whole `BenchmarkReport`, so any one run can be compared on its own.
pub fn write_repeat_artifacts(
    directory: &Path,
    file: &RepeatedBenchmarkFile,
    events: &[Vec<PerfEvent>],
    write_events: bool,
) -> Result<(), AppError> {
    fs::create_dir_all(directory).map_err(|source| AppError::Write {
        path: directory.display().to_string(),
        source,
    })?;
    for (index, report) in file.repeated.runs.iter().enumerate() {
        write_json(&directory.join(format!("run-{}.json", index + 1)), report)?;
        if write_events {
            if let Some(events) = events.get(index) {
                write_json_lines(
                    &directory.join(format!("run-{}.events.jsonl", index + 1)),
                    events,
                )?;
            }
        }
    }
    write_json(&directory.join("repeated.json"), file)
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
            &options.source,
            &options.mode,
            options.cpu_frame,
            options.cursor,
        );
        let _ = backend.capture(&request, &mut warmup_recorder);
    }

    if options.trigger_queue_capacity == 0 {
        return Err(AppError::InvalidArgument(
            "benchmark trigger queue capacity must be greater than zero".to_owned(),
        ));
    }
    let (trigger_sender, trigger_receiver) = mpsc::sync_channel(options.trigger_queue_capacity);
    let mut recorder = EventRecorder::with_capacity(options.metrics_capacity);
    let mut native_samples = Vec::with_capacity(options.iterations);
    let mut cpu_samples = Vec::with_capacity(options.iterations);
    let mut readback_samples = Vec::with_capacity(options.iterations);
    let mut trigger_to_dequeue_samples = Vec::with_capacity(options.iterations);
    let mut frame_age_samples = Vec::with_capacity(options.iterations);
    let mut failures = 0_usize;
    let mut timeouts = 0_usize;
    let mut failures_by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut cursor_outcomes: BTreeMap<String, usize> = BTreeMap::new();
    let mut successful_ids = Vec::with_capacity(options.iterations);

    for index in 0..options.iterations {
        let capture_id = CaptureId(index as u64 + 1);
        recorder.record(capture_id, PerfEventKind::HotkeyReceived, 0);
        let request = request(
            capture_id.0,
            &options.source,
            &options.mode,
            options.cpu_frame,
            options.cursor,
        );
        let triggered_at = request.triggered_at;
        trigger_sender.try_send(request).map_err(|error| {
            AppError::InvalidArgument(format!("benchmark trigger queue rejected input: {error}"))
        })?;
        let enqueued_ns = duration_ns(triggered_at.elapsed());
        recorder.record(capture_id, PerfEventKind::TriggerEnqueued, enqueued_ns);
        let request = trigger_receiver.try_recv().map_err(|error| {
            AppError::InvalidArgument(format!("benchmark trigger queue lost input: {error}"))
        })?;
        let dequeued_ns = duration_ns(request.triggered_at.elapsed());
        recorder.record(capture_id, PerfEventKind::TriggerDequeued, dequeued_ns);
        trigger_to_dequeue_samples.push(dequeued_ns);
        match backend.capture(&request, &mut recorder) {
            Ok(outcome) => {
                successful_ids.push(capture_id);
                *cursor_outcomes
                    .entry(cursor_outcome_label(outcome.metadata.cursor.as_ref()).to_owned())
                    .or_insert(0) += 1;
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
                *failures_by_kind.entry(label.to_owned()).or_insert(0) += 1;
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
        // 3: the environment fingerprint grew from "OS, architecture, build version, display
        // geometry" to the host identity a comparison can actually be keyed on, and the report
        // became deserializable. Nothing that was in a v2 report has moved or changed meaning.
        schema_version: 3,
        cursor: match options.cursor {
            CursorMode::Include => "include",
            CursorMode::Exclude => "exclude",
        }
        .to_owned(),
        backend: backend.name().to_owned(),
        mode: options.mode.name().to_owned(),
        synthetic: backend.name() == "fake",
        warmup_iterations: options.warmup,
        timed_iterations: options.iterations,
        successes,
        failures,
        timeouts,
        failures_by_kind,
        cursor_outcomes,
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
        environment: EnvironmentFingerprint::collect(backend.displays()),
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

/// Names what happened to the pointer, so a run can be checked for having measured it.
fn cursor_outcome_label(cursor: Option<&captastic_core::CursorCapture>) -> &'static str {
    use captastic_core::{CursorAbsence, CursorCapture};
    match cursor {
        None => "not_recorded",
        Some(CursorCapture::Excluded) => "excluded",
        Some(CursorCapture::Composited { .. }) => "composited",
        Some(CursorCapture::Absent { reason }) => match reason {
            CursorAbsence::NotVisible => "absent_not_visible",
            CursorAbsence::SourceCannotCompose => "absent_source_cannot_compose",
            CursorAbsence::SuppressedForSelection => "absent_suppressed_for_selection",
            CursorAbsence::ShapeNotYetKnown => "absent_shape_not_yet_known",
            CursorAbsence::PositionNotYetKnown => "absent_position_not_yet_known",
        },
    }
}

fn capture_error_kind_label(kind: CaptureErrorKind) -> &'static str {
    match kind {
        CaptureErrorKind::Unsupported => "unsupported",
        CaptureErrorKind::PermissionDenied => "permission_denied",
        CaptureErrorKind::SourceUnavailable => "source_unavailable",
        CaptureErrorKind::DesktopUnavailable => "desktop_unavailable",
        CaptureErrorKind::Timeout => "timeout",
        CaptureErrorKind::AccessLost => "access_lost",
        CaptureErrorKind::DeviceRemoved => "device_removed",
        CaptureErrorKind::TopologyChanged => "topology_changed",
        CaptureErrorKind::BufferExhausted => "buffer_exhausted",
        CaptureErrorKind::WorkersExhausted => "workers_exhausted",
        CaptureErrorKind::PointerOutsideDisplays => "pointer_outside_displays",
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

fn request(
    id: u64,
    source: &CaptureSource,
    mode: &CaptureMode,
    cpu_frame: bool,
    cursor: CursorMode,
) -> CaptureRequest {
    CaptureRequest {
        id: CaptureId(id),
        triggered_at: Instant::now(),
        source: source.clone(),
        mode: mode.clone(),
        cpu_frame,
        retain_native_frame: false,
        cursor,
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
    /// Options that make a run finish immediately, so a repeat test measures logic not delays.
    fn instant_options(cursor: CursorMode) -> BenchmarkOptions {
        BenchmarkOptions {
            iterations: 3,
            warmup: 1,
            mode: CaptureMode::Latest { max_age_ms: None },
            cpu_frame: true,
            cursor,
            source: CaptureSource::Display(captastic_core::DisplayId::primary()),
            trigger_queue_capacity: 4,
            metrics_capacity: 128,
            fake: FakeBackendConfig {
                native_delay: Duration::ZERO,
                readback_delay: Duration::ZERO,
                ..FakeBackendConfig::default()
            },
        }
    }

    #[test]
    fn compatible_repeats_are_summarised_by_their_spread() {
        // Three runs of the same question. The spread is what a performance claim rests on: a
        // mean alone would look identical whether the runs agreed or disagreed wildly.
        let options = instant_options(CursorMode::Exclude);
        let runs = run_repeated(&options, 3, || {
            Ok(Box::new(FakeBackend::new(options.fake.clone())) as Box<dyn CaptureBackend>)
        })
        .expect("three runs");
        let repeated = runs.repeated;

        assert_eq!(repeated.runs.len(), 3);
        assert!(repeated.incompatibilities.is_empty());
        let agreement = repeated.agreement.expect("compatible runs are summarised");
        assert_eq!(agreement.runs, 3);
        assert_eq!(agreement.native_p50_ns.len(), 3);
        assert_eq!(agreement.total_successes, 9);
        assert_eq!(agreement.total_failures, 0);

        // Every stage the run measured has its own spread, at every percentile. The two fields
        // above cover two of the five, and a set is accepted on all of them.
        let stages: Vec<&str> = agreement
            .stages
            .iter()
            .map(|stage| stage.stage.as_str())
            .collect();
        assert_eq!(
            stages,
            [
                "trigger_to_dequeue",
                "native_frame",
                "cpu_frame",
                "readback",
                "frame_age"
            ]
        );
        for stage in &agreement.stages {
            assert_eq!(stage.p50_ns.len(), 3, "{}", stage.stage);
            assert_eq!(stage.p95_ns.len(), 3, "{}", stage.stage);
            assert_eq!(stage.p99_ns.len(), 3, "{}", stage.stage);
        }
        let native = agreement
            .stages
            .iter()
            .find(|stage| stage.stage == "native_frame")
            .expect("the native stage is summarised");
        assert_eq!(native.p50_ns, agreement.native_p50_ns);
        assert_eq!(
            native.p50_spread_percent,
            agreement.native_p50_spread_percent
        );
    }

    #[test]
    fn a_run_without_a_cpu_frame_reports_no_spread_for_the_stages_it_never_measured() {
        // Zero would read as perfect agreement about a stage nothing timed.
        let options = BenchmarkOptions {
            cpu_frame: false,
            ..instant_options(CursorMode::Exclude)
        };
        let runs = run_repeated(&options, 2, || {
            Ok(Box::new(FakeBackend::new(options.fake.clone())) as Box<dyn CaptureBackend>)
        })
        .expect("two runs");
        let agreement = runs
            .repeated
            .agreement
            .expect("compatible runs are summarised");
        let stages: Vec<&str> = agreement
            .stages
            .iter()
            .map(|stage| stage.stage.as_str())
            .collect();
        assert_eq!(stages, ["trigger_to_dequeue", "native_frame", "frame_age"]);
    }

    #[test]
    fn every_repeat_leaves_its_own_artifacts_behind() {
        // `--raw-events` under `--repeat` wrote nothing at all: the events were dropped inside
        // `run_repeated` before anything could ask for them. An operator collecting the evidence
        // for a published figure got a silent no-op in the one combination they actually run.
        let directory = std::env::temp_dir().join(format!(
            "captastic-benchmark-artifacts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.subsec_nanos())
                .unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&directory);

        let options = instant_options(CursorMode::Exclude);
        let runs = run_repeated(&options, 3, || {
            Ok(Box::new(FakeBackend::new(options.fake.clone())) as Box<dyn CaptureBackend>)
        })
        .expect("three runs");
        assert_eq!(runs.events.len(), 3);
        assert!(runs.events.iter().all(|events| !events.is_empty()));

        let file = RepeatedBenchmarkFile {
            schema_version: REPEATED_FILE_SCHEMA_VERSION,
            repeated: runs.repeated,
            budgets: None,
        };
        write_repeat_artifacts(&directory, &file, &runs.events, true).expect("artifacts written");

        for index in 1..=3 {
            // Each run file is a whole report, so any single repeat can be compared on its own.
            let report = fs::read_to_string(directory.join(format!("run-{index}.json")))
                .expect("a run file per repeat");
            let report: BenchmarkReport =
                serde_json::from_str(&report).expect("the run file is a report");
            assert_eq!(report.schema_version, 3);
            assert_eq!(report.timed_iterations, options.iterations);

            let events = fs::read_to_string(directory.join(format!("run-{index}.events.jsonl")))
                .expect("an event stream per repeat");
            assert!(events.lines().count() > 0);
            for line in events.lines() {
                serde_json::from_str::<PerfEvent>(line).expect("every line is one event");
            }
        }

        let set = fs::read_to_string(directory.join("repeated.json")).expect("the set file");
        let parsed: RepeatedBenchmarkFile =
            serde_json::from_str(&set).expect("the set file reads back");
        assert_eq!(parsed.schema_version, REPEATED_FILE_SCHEMA_VERSION);
        assert_eq!(parsed.repeated.runs.len(), 3);
        assert!(parsed.budgets.is_none());
        assert_eq!(
            serde_json::to_string(&parsed).expect("the round trip re-serializes"),
            serde_json::to_string(&file).expect("the set serializes")
        );

        fs::remove_dir_all(&directory).expect("remove the artifact directory");
    }

    #[test]
    fn a_run_that_measured_something_else_is_refused_rather_than_averaged() {
        // The failure this exists to prevent is silent: averaging a cursor-on run with two
        // cursor-off runs produces a number that describes neither, and nothing about the output
        // would say so. The mismatch is named instead, field by field.
        let mut with_cursor = RunCompatibility {
            backend: "fake".to_owned(),
            mode: "latest".to_owned(),
            cursor: "include".to_owned(),
            cpu_frame: true,
            synthetic: true,
            build: "0.1.0 (abc1234, release, x86_64-pc-windows-msvc, clean)".to_owned(),
            debug_assertions: false,
            displays: vec!["primary:1920x1080@0:1.00x:60.000Hz".to_owned()],
            adapters: vec!["NVIDIA GeForce RTX 3070 (32.0.15.9186)".to_owned()],
            os_build: Some("26200.9457 (25H2)".to_owned()),
            session: Some("interactive".to_owned()),
            power_source: Some("ac".to_owned()),
        };
        let without_cursor = RunCompatibility {
            cursor: "exclude".to_owned(),
            ..with_cursor.clone()
        };
        let differences = with_cursor.differences(&without_cursor);
        assert_eq!(differences.len(), 1);
        assert!(differences[0].contains("cursor"), "{differences:?}");

        // Every field that changes the question is covered, not just the one the test author
        // happened to think of.
        with_cursor.displays = vec!["primary:3840x2160@0:1.50x:59.997Hz".to_owned()];
        with_cursor.debug_assertions = true;
        with_cursor.build = "0.2.0 (def5678, debug, x86_64-pc-windows-msvc, dirty)".to_owned();
        let differences = with_cursor.differences(&without_cursor);
        assert_eq!(differences.len(), 4, "{differences:?}");
    }

    #[test]
    fn the_host_facts_that_move_a_measurement_are_each_named_on_their_own() {
        // Each of these used to compare as "the same host": two builds a hundred commits apart at
        // the same version, a driver update, and a run started over Remote Desktop. A refusal that
        // did not name which one changed would send the reader back to comparing files by eye.
        let baseline = RunCompatibility {
            backend: "dxgi".to_owned(),
            mode: "latest".to_owned(),
            cursor: "exclude".to_owned(),
            cpu_frame: true,
            synthetic: false,
            build: "0.2.0-dev.408 (0ca78881, release, x86_64-pc-windows-msvc, clean)".to_owned(),
            debug_assertions: false,
            displays: vec!["dell:3840x2160@0:1.50x:59.997Hz".to_owned()],
            adapters: vec!["NVIDIA GeForce RTX 3070 (32.0.15.9186)".to_owned()],
            os_build: Some("26200.9457 (25H2)".to_owned()),
            session: Some("interactive".to_owned()),
            power_source: Some("ac".to_owned()),
        };

        let named = |candidate: &RunCompatibility, field: &str| {
            let differences = baseline.differences(candidate);
            assert_eq!(differences.len(), 1, "{differences:?}");
            assert!(differences[0].starts_with(field), "{differences:?}");
        };

        named(
            &RunCompatibility {
                adapters: vec!["NVIDIA GeForce RTX 3070 (32.0.15.7270)".to_owned()],
                ..baseline.clone()
            },
            "adapters",
        );
        named(
            &RunCompatibility {
                // Same version, same profile, different commit: previously identical.
                build: "0.2.0-dev.408 (deadbeef, release, x86_64-pc-windows-msvc, clean)"
                    .to_owned(),
                ..baseline.clone()
            },
            "build",
        );
        named(
            &RunCompatibility {
                session: Some("remote".to_owned()),
                ..baseline.clone()
            },
            "session",
        );
        named(
            &RunCompatibility {
                power_source: Some("battery".to_owned()),
                ..baseline.clone()
            },
            "power_source",
        );
        named(
            &RunCompatibility {
                os_build: Some("26200.9999 (25H2)".to_owned()),
                ..baseline.clone()
            },
            "os_build",
        );
        // A host that would not answer at all differs from one that did, rather than matching it.
        named(
            &RunCompatibility {
                session: None,
                ..baseline.clone()
            },
            "session",
        );
        // And the refresh rate, which changes the cadence without changing the geometry.
        named(
            &RunCompatibility {
                displays: vec!["dell:3840x2160@0:1.50x:143.998Hz".to_owned()],
                ..baseline.clone()
            },
            "displays",
        );
    }

    #[test]
    fn a_spread_percentage_reads_back_as_the_number_it_was() {
        // Found by CI, which produced a repeat set whose CPU spread was 15.384615384615385 % and
        // read it back as ...383 %. serde_json's default parser is allowed to land one ULP off
        // what it wrote, so without the `float_roundtrip` feature a run compared against its own
        // artifact differs from itself — in a tool whose entire job is comparing artifacts.
        let value: f64 = (2.0_f64 / 13.0) * 100.0;
        let text = serde_json::to_string(&value).expect("the spread serializes");
        let parsed: f64 = serde_json::from_str(&text).expect("the spread reads back");
        assert_eq!(value, parsed, "{text}");
    }

    #[test]
    fn a_spread_needs_a_floor_to_be_a_percentage_of() {
        assert_eq!(spread_percent(&[]), 0.0);
        assert_eq!(spread_percent(&[100, 100, 100]), 0.0);
        assert!((spread_percent(&[100, 150]) - 50.0).abs() < f64::EPSILON);
        // A zero floor would make the percentage infinite, which reports worse than nothing; the
        // raw samples travel alongside so the reader can see what happened.
        assert_eq!(spread_percent(&[0, 500]), 0.0);
    }

    #[test]
    fn a_report_survives_the_round_trip_it_will_have_to_make() {
        // A baseline is a file on disk that a later run is compared against, so a report that
        // cannot be read back is a report that can never be a baseline. Equality rather than
        // "it parses": a field silently dropped by a serde attribute parses perfectly and loses
        // the evidence.
        let options = instant_options(CursorMode::Include);
        let run = run(&options).expect("a run with no configured delays");
        let json = serde_json::to_string(&run.report).expect("the report serializes");
        let parsed: BenchmarkReport = serde_json::from_str(&json).expect("the report reads back");
        assert_eq!(
            serde_json::to_string(&parsed).expect("the round trip re-serializes"),
            json
        );
        assert_eq!(parsed.schema_version, 3);
        assert_eq!(parsed.backend, run.report.backend);
        assert_eq!(parsed.cursor_outcomes, run.report.cursor_outcomes);
        assert_eq!(parsed.environment, run.report.environment);
        // The fingerprint is not an empty shell that happens to round-trip.
        assert_eq!(parsed.environment.os, std::env::consts::OS);
        assert!(!parsed.environment.recorded_at_utc.is_empty());
        assert_eq!(
            parsed.environment.build.version,
            crate::build_info::BUILD_VERSION
        );
    }

    #[test]
    fn a_repeat_set_survives_the_round_trip_too() {
        // `--repeat` is what an operator actually runs, and B2 compares whole repeat sets: the
        // envelope has to read back as well as the reports inside it.
        let options = instant_options(CursorMode::Exclude);
        let repeated = run_repeated(&options, 2, || {
            Ok(Box::new(FakeBackend::new(options.fake.clone())) as Box<dyn CaptureBackend>)
        })
        .expect("two runs")
        .repeated;
        let json = serde_json::to_string(&repeated).expect("the repeat set serializes");
        let parsed: RepeatedBenchmark =
            serde_json::from_str(&json).expect("the repeat set reads back");
        assert_eq!(parsed.runs.len(), 2);
        assert_eq!(parsed.compatibility, repeated.compatibility);
        assert_eq!(
            serde_json::to_string(&parsed).expect("the round trip re-serializes"),
            json
        );
    }

    #[test]
    fn repeating_zero_times_is_refused() {
        let options = instant_options(CursorMode::Exclude);
        assert!(run_repeated(&options, 0, || {
            Ok(Box::new(FakeBackend::new(options.fake.clone())) as Box<dyn CaptureBackend>)
        })
        .is_err());
    }

    use super::*;
    use captastic_core::DisplayId;

    #[test]
    fn synthetic_benchmark_has_complete_samples() {
        let run = run(&BenchmarkOptions {
            iterations: 5,
            warmup: 1,
            mode: CaptureMode::Latest {
                max_age_ms: Some(25),
            },
            cpu_frame: true,
            cursor: CursorMode::Exclude,
            source: CaptureSource::Display(DisplayId::primary()),
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
