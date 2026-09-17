//! Comparing a run against a baseline, and refusing to when they measured different things.
//!
//! A benchmark artifact is only evidence if a later one can be held against it. The refusal is the
//! load-bearing part: two runs a week apart on "the same machine" can differ by a driver update, a
//! Windows build, a session type or a hundred commits, and every one of those moves a latency
//! figure without moving anything the numbers say. Comparing them anyway produces a percentage
//! that looks exactly like a regression and is not one — so a mismatch stops the comparison and
//! names every field that differs, with both sides, rather than being absorbed into the result.
//!
//! What it deliberately does *not* do is fail on `slower`. A comparison is a measurement, and a
//! command that exits non-zero on a number the operator is still interpreting is a command that
//! gets run with `|| true`. Only "these cannot be compared" is an error.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::benchmark::{
    spread_percent, stage_summaries, BenchmarkReport, RepeatedBenchmark, RunCompatibility,
};
use crate::error::AppError;

/// One side of a comparison: a single run, or a repeat set, held the same way.
///
/// A single report is a one-run set rather than a separate case, so nothing downstream has to know
/// which shape it was loaded from. A one-run set simply has a spread of zero, which is the honest
/// answer: one run agreed with itself and measured nothing about repeatability.
#[derive(Debug)]
pub struct RunSet {
    pub label: String,
    pub runs: Vec<BenchmarkReport>,
}

impl RunSet {
    /// What every run in the set was asking, taken from the first.
    ///
    /// The set's own runs are checked against each other first, so this is not "whichever run
    /// happened to be first" — it is the set's identity or the set is refused.
    fn compatibility(&self) -> RunCompatibility {
        RunCompatibility::of(&self.runs[0])
    }

    /// The pointer outcomes of every run, added up.
    fn cursor_outcomes(&self) -> BTreeMap<String, usize> {
        let mut totals = BTreeMap::new();
        for run in &self.runs {
            for (outcome, count) in &run.cursor_outcomes {
                *totals.entry(outcome.clone()).or_insert(0) += count;
            }
        }
        totals
    }
}

/// Loads either a single `BenchmarkReport` or a `repeated.json` set, whichever the file holds.
///
/// Detected by shape rather than by filename or a flag: an operator holding two paths should not
/// have to tell the tool which kind each one is, and a wrong answer there would be a silent
/// deserialization failure rather than a comparison.
pub fn load(path: &Path) -> Result<RunSet, AppError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        AppError::InvalidArgument(format!("cannot read {}: {error}", path.display()))
    })?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        AppError::InvalidArgument(format!("{} is not benchmark JSON: {error}", path.display()))
    })?;
    let label = path.display().to_string();

    // `repeated.json` and the `--repeat` stdout envelope wrap the set under "repeated"; a bare
    // `RepeatedBenchmark` carries "runs" itself; anything else is a single report.
    let runs = if let Some(repeated) = value.get("repeated") {
        parse::<RepeatedBenchmark>(repeated.clone(), path)?.runs
    } else if value.get("runs").is_some() {
        parse::<RepeatedBenchmark>(value, path)?.runs
    } else {
        vec![parse::<BenchmarkReport>(value, path)?]
    };

    if runs.is_empty() {
        return Err(AppError::InvalidArgument(format!(
            "{} holds no runs to compare",
            path.display()
        )));
    }

    // A set that disagrees with itself cannot be one side of a comparison: its spread would be the
    // spread of two different questions, and that spread is what decides every verdict below.
    let set = RunSet { label, runs };
    let compatibility = RunCompatibility::of(&set.runs[0]);
    for (index, run) in set.runs.iter().enumerate().skip(1) {
        let differences = compatibility.differences(&RunCompatibility::of(run));
        if !differences.is_empty() {
            return Err(AppError::InvalidArgument(format!(
                "{} is not a comparable set: run {} differs in {}",
                set.label,
                index + 1,
                differences.join("; ")
            )));
        }
    }
    Ok(set)
}

fn parse<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    path: &Path,
) -> Result<T, AppError> {
    serde_json::from_value(value).map_err(|error| {
        AppError::InvalidArgument(format!(
            "{} is not a benchmark report or repeat set: {error}",
            path.display()
        ))
    })
}

/// What the candidate did relative to the baseline, stage by stage.
#[derive(Debug, Deserialize, Serialize)]
pub struct Comparison {
    /// The question both sides were asking. Present because a comparison quoted without it is a
    /// percentage with no host attached, which is the thing this module exists to prevent.
    pub compatibility: RunCompatibility,
    pub stages: Vec<StageComparison>,
    pub cursor_outcomes: CursorOutcomeComparison,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct StageComparison {
    pub stage: String,
    pub baseline_p50_ns: u64,
    pub candidate_p50_ns: u64,
    pub delta_percent: f64,
    pub baseline_p95_ns: u64,
    pub candidate_p95_ns: u64,
    pub p95_delta_percent: f64,
    /// How far the baseline's own runs disagreed at p50, which bounds what it can detect.
    pub baseline_spread_percent: f64,
    pub candidate_spread_percent: f64,
    pub verdict: Verdict,
}

/// The pointer outcomes of each side, side by side.
///
/// Here because a cursor-on run that composited nothing has happened twice on this project and
/// produced two perfectly plausible numbers. A comparison whose two sides disagree about how many
/// captures actually drew a pointer is comparing cursor-on against cursor-off under one name.
#[derive(Debug, Deserialize, Serialize)]
pub struct CursorOutcomeComparison {
    pub baseline: BTreeMap<String, usize>,
    pub candidate: BTreeMap<String, usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Inside the wider of the two sets' measured spreads, or inside the stated noise floor.
    WithinNoise,
    Slower,
    Faster,
    /// One side has no figure to be a percentage of, so nothing can be said about the change.
    ///
    /// A stage whose baseline p50 is 0 ns has no denominator: the delta was reported as 0.0 % and
    /// every candidate, however slow, came back `within_noise`. A stage that reads "unchanged" on
    /// arithmetic that could not run is worse than one that says it has no answer - the first is
    /// quoted, the second is looked into. `trigger_to_dequeue` lands here routinely on a fast host
    /// with few iterations, where the whole stage rounds to nothing.
    Unmeasurable,
}

impl fmt::Display for Verdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::WithinNoise => "within_noise",
            Self::Slower => "slower",
            Self::Faster => "faster",
            Self::Unmeasurable => "unmeasurable",
        })
    }
}

/// Compares two sets, or refuses and says exactly which host facts differ.
pub fn compare(
    baseline: &RunSet,
    candidate: &RunSet,
    noise_percent: f64,
) -> Result<Comparison, AppError> {
    if !noise_percent.is_finite() || noise_percent < 0.0 {
        return Err(AppError::InvalidArgument(format!(
            "--noise-percent must be a finite percentage of zero or more, not {noise_percent}"
        )));
    }
    let baseline_compatibility = baseline.compatibility();
    let differences = baseline_compatibility.differences(&candidate.compatibility());
    if !differences.is_empty() {
        // Every field, with both sides. A refusal that named only the first would send the reader
        // back to diffing two fingerprint blocks by eye, which is the work this replaces.
        return Err(AppError::InvalidArgument(format!(
            "{} and {} did not measure the same thing, so no figure is reported from them; they \
             differ in {}",
            baseline.label,
            candidate.label,
            differences.join("; ")
        )));
    }

    let mut stages = Vec::new();
    let names: Vec<&'static str> = stage_summaries(&baseline.runs[0])
        .into_iter()
        .map(|(stage, _)| stage)
        .collect();
    for (index, stage) in names.into_iter().enumerate() {
        let (Some(baseline_p50), Some(candidate_p50)) = (
            percentile(&baseline.runs, index, Percentile::P50),
            percentile(&candidate.runs, index, Percentile::P50),
        ) else {
            // One side did not measure this stage - `--cpu-frame false` against a run that had it.
            // Reporting a delta against nothing would be inventing the missing side.
            continue;
        };
        let baseline_p95 = percentile(&baseline.runs, index, Percentile::P95).unwrap_or_default();
        let candidate_p95 = percentile(&candidate.runs, index, Percentile::P95).unwrap_or_default();
        let baseline_spread = spread(&baseline.runs, index, Percentile::P50);
        let candidate_spread = spread(&candidate.runs, index, Percentile::P50);
        let delta = delta_percent(baseline_p50, candidate_p50);
        stages.push(StageComparison {
            stage: stage.to_owned(),
            baseline_p50_ns: baseline_p50,
            candidate_p50_ns: candidate_p50,
            delta_percent: delta,
            baseline_p95_ns: baseline_p95,
            candidate_p95_ns: candidate_p95,
            p95_delta_percent: delta_percent(baseline_p95, candidate_p95),
            baseline_spread_percent: baseline_spread,
            candidate_spread_percent: candidate_spread,
            verdict: verdict(
                baseline_p50,
                candidate_p50,
                delta,
                baseline_spread,
                candidate_spread,
                noise_percent,
            ),
        });
    }

    Ok(Comparison {
        compatibility: baseline_compatibility,
        stages,
        cursor_outcomes: CursorOutcomeComparison {
            baseline: baseline.cursor_outcomes(),
            candidate: candidate.cursor_outcomes(),
        },
    })
}

#[derive(Clone, Copy)]
enum Percentile {
    P50,
    P95,
}

/// The set's value for one stage at one percentile: the median across its runs.
///
/// The median rather than the mean, for the same reason the runs report medians: one repeat that
/// landed beside a Windows Update should not drag the figure the comparison is against.
fn percentile(runs: &[BenchmarkReport], stage: usize, percentile: Percentile) -> Option<u64> {
    let mut values = values(runs, stage, percentile);
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

fn values(runs: &[BenchmarkReport], stage: usize, percentile: Percentile) -> Vec<u64> {
    runs.iter()
        .filter_map(|run| {
            stage_summaries(run)[stage]
                .1
                .map(|summary| match percentile {
                    Percentile::P50 => summary.p50_ns,
                    Percentile::P95 => summary.p95_ns,
                })
        })
        .collect()
}

fn spread(runs: &[BenchmarkReport], stage: usize, percentile: Percentile) -> f64 {
    spread_percent(&values(runs, stage, percentile))
}

/// The candidate as a percentage of the baseline, signed so positive always means slower.
fn delta_percent(baseline: u64, candidate: u64) -> f64 {
    if baseline == 0 {
        // A zero floor makes a percentage meaningless rather than infinite; both raw figures are
        // reported beside it so the reader can see what happened.
        return 0.0;
    }
    ((candidate as f64 - baseline as f64) / baseline as f64) * 100.0
}

/// A move is only a change if it is bigger than what either side already disagreed with itself by.
///
/// The raw figures come in as well as the delta, because a delta of zero means two different
/// things: "the same" and "there was nothing to divide by". Only the first is `within_noise`.
fn verdict(
    baseline_p50_ns: u64,
    candidate_p50_ns: u64,
    delta_percent: f64,
    baseline_spread: f64,
    candidate_spread: f64,
    noise_percent: f64,
) -> Verdict {
    if baseline_p50_ns == 0 || candidate_p50_ns == 0 {
        // A zero baseline has no denominator; a candidate that fell to zero against a baseline
        // that was not is a change no percentage can size. Both raw figures travel alongside.
        return Verdict::Unmeasurable;
    }
    let floor = baseline_spread.max(candidate_spread).max(noise_percent);
    if delta_percent.abs() <= floor {
        Verdict::WithinNoise
    } else if delta_percent > 0.0 {
        Verdict::Slower
    } else {
        Verdict::Faster
    }
}

/// Prints the comparison as one aligned table, plus the host both sides agreed on.
pub fn report(comparison: &Comparison, baseline: &str, candidate: &str) {
    log::info!("comparing {candidate} against baseline {baseline}");
    log::info!(
        "  same question: {} {} cursor={} cpu_frame={} build={} displays={} adapters={}",
        comparison.compatibility.backend,
        comparison.compatibility.mode,
        comparison.compatibility.cursor,
        comparison.compatibility.cpu_frame,
        comparison.compatibility.build,
        comparison.compatibility.displays.join(", "),
        comparison.compatibility.adapters.join(", ")
    );
    log::info!(
        "  {:<18} {:>12} {:>12} {:>9} {:>12} {:>12} {:>9} {:>9} {:>9}  {}",
        "stage",
        "base p50",
        "cand p50",
        "delta",
        "base p95",
        "cand p95",
        "p95 delta",
        "base +/-",
        "cand +/-",
        "verdict"
    );
    for stage in &comparison.stages {
        log::info!(
            "  {:<18} {:>12} {:>12} {:>9} {:>12} {:>12} {:>9} {:>9} {:>9}  {}",
            stage.stage,
            milliseconds(stage.baseline_p50_ns),
            milliseconds(stage.candidate_p50_ns),
            signed(stage.delta_percent),
            milliseconds(stage.baseline_p95_ns),
            milliseconds(stage.candidate_p95_ns),
            signed(stage.p95_delta_percent),
            format!("{:.1}%", stage.baseline_spread_percent),
            format!("{:.1}%", stage.candidate_spread_percent),
            stage.verdict
        );
    }
    if comparison.cursor_outcomes.baseline != comparison.cursor_outcomes.candidate {
        // Not an error: iteration counts differ legitimately. But a cursor-on run that composited
        // nothing has produced plausible numbers here before, so the two sides are shown.
        log::warn!(
            "  pointer outcomes differ: baseline {:?}, candidate {:?}",
            comparison.cursor_outcomes.baseline,
            comparison.cursor_outcomes.candidate
        );
    } else {
        log::info!(
            "  pointer outcomes: {:?}",
            comparison.cursor_outcomes.baseline
        );
    }
}

fn milliseconds(ns: u64) -> String {
    format!("{:.3} ms", ns as f64 / 1_000_000.0)
}

fn signed(percent: f64) -> String {
    format!("{percent:+.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::benchmark::{run, BenchmarkOptions};
    use captastic_core::{CaptureMode, CaptureSource, CursorMode, DisplayId, FakeBackendConfig};
    use std::time::Duration;

    fn instant_options(cursor: CursorMode) -> BenchmarkOptions {
        BenchmarkOptions {
            iterations: 4,
            warmup: 1,
            mode: CaptureMode::Latest { max_age_ms: None },
            cpu_frame: true,
            cursor,
            source: CaptureSource::Display(DisplayId::primary()),
            trigger_queue_capacity: 8,
            metrics_capacity: 256,
            fake: FakeBackendConfig {
                native_delay: Duration::ZERO,
                readback_delay: Duration::ZERO,
                ..FakeBackendConfig::default()
            },
        }
    }

    fn one_run(label: &str) -> RunSet {
        RunSet {
            label: label.to_owned(),
            runs: vec![
                run(&instant_options(CursorMode::Exclude))
                    .expect("a run")
                    .report,
            ],
        }
    }

    /// A run with every stage pinned, so a delta is arithmetic rather than whatever the host did.
    fn pinned(native_p50: u64, native_p95: u64) -> BenchmarkReport {
        let mut report = run(&instant_options(CursorMode::Exclude))
            .expect("a run")
            .report;
        report.native_frame_latency.p50_ns = native_p50;
        report.native_frame_latency.p95_ns = native_p95;
        report
    }

    #[test]
    fn a_host_that_changed_is_named_field_by_field_rather_than_compared() {
        // The failure this exists to prevent: a driver update and a Windows build between two runs
        // produce a percentage that reads exactly like a regression. Every differing field is
        // named with both sides, because a refusal listing only the first sends the reader back to
        // diffing fingerprint blocks by eye.
        let baseline = one_run("baseline.json");
        let mut candidate = one_run("candidate.json");
        candidate.runs[0].environment.os_build = Some("26200.9999 (25H2)".to_owned());
        candidate.runs[0].environment.session = Some("remote".to_owned());
        candidate.runs[0].cursor = "include".to_owned();

        let error = compare(&baseline, &candidate, 7.0).expect_err("the hosts differ");
        let message = error.to_string();
        for field in ["os_build", "session", "cursor"] {
            assert!(message.contains(field), "{field} is not named in {message}");
        }
        assert!(message.contains("26200.9999"), "{message}");
        assert!(message.contains("remote"), "{message}");
        assert!(message.contains("baseline.json"), "{message}");
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn a_stage_that_moved_further_than_the_noise_floor_is_reported_as_a_change() {
        let baseline = RunSet {
            label: "baseline".to_owned(),
            runs: vec![pinned(1_000_000, 2_000_000)],
        };
        let candidate = RunSet {
            label: "candidate".to_owned(),
            runs: vec![pinned(1_500_000, 2_200_000)],
        };
        let comparison = compare(&baseline, &candidate, 7.0).expect("the hosts match");
        let native = comparison
            .stages
            .iter()
            .find(|stage| stage.stage == "native_frame")
            .expect("the native stage is compared");
        assert_eq!(native.baseline_p50_ns, 1_000_000);
        assert_eq!(native.candidate_p50_ns, 1_500_000);
        assert!((native.delta_percent - 50.0).abs() < 1e-9, "{native:?}");
        assert!((native.p95_delta_percent - 10.0).abs() < 1e-9, "{native:?}");
        assert_eq!(native.verdict, Verdict::Slower);

        // Faster is reported, and is still not a failure: the command never exits non-zero on a
        // number the operator is still interpreting.
        let comparison = compare(&candidate, &baseline, 7.0).expect("the hosts match");
        let native = comparison
            .stages
            .iter()
            .find(|stage| stage.stage == "native_frame")
            .expect("the native stage is compared");
        assert_eq!(native.verdict, Verdict::Faster);
    }

    #[test]
    fn a_move_smaller_than_the_noise_floor_is_not_called_a_regression() {
        let baseline = RunSet {
            label: "baseline".to_owned(),
            runs: vec![pinned(1_000_000, 2_000_000)],
        };
        let candidate = RunSet {
            label: "candidate".to_owned(),
            runs: vec![pinned(1_050_000, 2_000_000)],
        };
        let comparison = compare(&baseline, &candidate, 7.0).expect("the hosts match");
        let native = comparison
            .stages
            .iter()
            .find(|stage| stage.stage == "native_frame")
            .expect("the native stage is compared");
        assert_eq!(native.verdict, Verdict::WithinNoise);
        assert_eq!(native.baseline_spread_percent, 0.0);

        // A set that disagreed with itself by more than the move cannot detect it, whatever the
        // stated floor says: its own spread wins.
        let noisy = RunSet {
            label: "noisy".to_owned(),
            runs: vec![pinned(1_000_000, 2_000_000), pinned(1_300_000, 2_000_000)],
        };
        let comparison = compare(&noisy, &candidate, 1.0).expect("the hosts match");
        let native = comparison
            .stages
            .iter()
            .find(|stage| stage.stage == "native_frame")
            .expect("the native stage is compared");
        assert!(native.baseline_spread_percent > 25.0, "{native:?}");
        assert_eq!(native.verdict, Verdict::WithinNoise);
    }

    #[test]
    fn a_stage_with_nothing_to_divide_by_says_so_instead_of_reading_as_unchanged() {
        // A zero baseline made `delta_percent` return 0.0, so every candidate - at any latency at
        // all - came back `within_noise`. A stage reporting "unchanged" from arithmetic that never
        // ran gets quoted; one reporting "no answer" gets looked into. `trigger_to_dequeue` hits
        // this routinely on a fast host with few iterations.
        let mut zero_baseline = pinned(1_000_000, 2_000_000);
        zero_baseline.trigger_to_dequeue_latency.p50_ns = 0;
        let baseline = RunSet {
            label: "baseline".to_owned(),
            runs: vec![zero_baseline],
        };
        let candidate = RunSet {
            label: "candidate".to_owned(),
            runs: vec![pinned(1_000_000, 2_000_000)],
        };

        let comparison = compare(&baseline, &candidate, 7.0).expect("the hosts match");
        let stage = comparison
            .stages
            .iter()
            .find(|stage| stage.stage == "trigger_to_dequeue")
            .expect("the stage is still reported");
        assert_eq!(stage.verdict, Verdict::Unmeasurable);
        assert_eq!(stage.baseline_p50_ns, 0);
        // The raw figures still travel, because they are what the reader has to go on.
        assert!(stage.candidate_p50_ns > 0);
        assert_eq!(
            serde_json::to_value(stage.verdict).expect("the verdict serializes"),
            serde_json::Value::String("unmeasurable".to_owned())
        );

        // The stages that did have a denominator are judged as usual.
        let native = comparison
            .stages
            .iter()
            .find(|stage| stage.stage == "native_frame")
            .expect("the native stage is compared");
        assert_eq!(native.verdict, Verdict::WithinNoise);

        // And a candidate that fell to zero against a baseline that had not is a change no
        // percentage can size, rather than a spectacular improvement.
        let mut zero_candidate = pinned(1_000_000, 2_000_000);
        zero_candidate.native_frame_latency.p50_ns = 0;
        let comparison = compare(
            &RunSet {
                label: "baseline".to_owned(),
                runs: vec![pinned(1_000_000, 2_000_000)],
            },
            &RunSet {
                label: "candidate".to_owned(),
                runs: vec![zero_candidate],
            },
            7.0,
        )
        .expect("the hosts match");
        let native = comparison
            .stages
            .iter()
            .find(|stage| stage.stage == "native_frame")
            .expect("the native stage is compared");
        assert_eq!(native.verdict, Verdict::Unmeasurable);
    }

    #[test]
    fn a_negative_noise_floor_is_refused_rather_than_making_everything_a_change() {
        let baseline = one_run("baseline");
        let candidate = one_run("candidate");
        assert!(compare(&baseline, &candidate, -1.0).is_err());
        assert!(compare(&baseline, &candidate, f64::NAN).is_err());
    }

    #[test]
    fn both_file_shapes_load_as_the_same_kind_of_set() {
        // An operator holding two paths should not have to tell the tool which is which, and a
        // wrong guess would be a deserialization failure rather than a comparison.
        let directory = std::env::temp_dir().join(format!(
            "captastic-compare-shapes-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.subsec_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&directory);

        let options = instant_options(CursorMode::Exclude);
        crate::benchmark::prepare_output_dir(&directory, false).expect("an empty directory");
        let runs = crate::benchmark::run_repeated(
            &options,
            2,
            || {
                Ok(
                    Box::new(captastic_core::FakeBackend::new(options.fake.clone()))
                        as Box<dyn captastic_core::CaptureBackend>,
                )
            },
            |number, run| crate::benchmark::write_run_artifacts(&directory, number, run, false),
        )
        .expect("two runs");
        let file = crate::benchmark::RepeatedBenchmarkFile {
            schema_version: crate::benchmark::REPEATED_FILE_SCHEMA_VERSION,
            repeated: runs,
            budgets: None,
        };
        crate::benchmark::write_repeat_set(&directory, &file).expect("the set file is written");

        let single = load(&directory.join("run-1.json")).expect("a single report loads");
        assert_eq!(single.runs.len(), 1, "a single run is a one-run set");
        let set = load(&directory.join("repeated.json")).expect("a repeat set loads");
        assert_eq!(set.runs.len(), 2);

        // And the two shapes compare against each other without either side knowing.
        let comparison = compare(&single, &set, 7.0).expect("the same host both times");
        assert!(!comparison.stages.is_empty());
        assert_eq!(
            comparison.cursor_outcomes.candidate.values().sum::<usize>(),
            options.iterations * 2
        );

        std::fs::remove_dir_all(&directory).expect("remove the artifact directory");
    }

    #[test]
    fn a_file_that_is_not_a_benchmark_says_so_instead_of_comparing_nothing() {
        let path = std::env::temp_dir().join(format!(
            "captastic-compare-garbage-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, b"{\"hello\":1}").expect("write the file");
        let error = load(&path).expect_err("not a benchmark report");
        assert!(
            error.to_string().contains("not a benchmark report"),
            "{error}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
