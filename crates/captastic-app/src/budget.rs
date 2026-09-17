//! Performance budgets, and the rule that they only apply where they mean something.
//!
//! A latency budget is a claim about hardware. "Native frame acquisition stays under 2 ms" is true
//! of one machine with one GPU driving one display at one resolution, and says nothing about a
//! hosted CI runner with a software adapter — where it would fail every time and teach everyone to
//! ignore it, which is worse than having no budget at all.
//!
//! So a budget file names the host it describes, and enforcement happens only when the run matches
//! it. A run somewhere else is **skipped, loudly**: the numbers are still reported, and the reason
//! for skipping is stated, because a budget that silently passes on the wrong hardware is the one
//! failure mode worse than a budget that noisily fails on it.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::benchmark::BenchmarkReport;
use crate::error::AppError;

/// A budget file: which host it describes, and what it asks of that host.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetFile {
    pub host: HostMatch,
    #[serde(default)]
    pub absolute: AbsoluteBudgets,
    #[serde(default)]
    pub relative: RelativeBudgets,
}

/// The host a budget describes.
///
/// Every field is optional and an absent field matches anything, so a budget can be as specific as
/// the claim it protects. A budget about GPU acquisition wants the display geometry pinned; one
/// about queue handoff may not care.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostMatch {
    /// Free text naming the machine, for the message rather than the comparison.
    #[serde(default)]
    pub description: String,
    pub backend: Option<String>,
    pub mode: Option<String>,
    pub cursor: Option<String>,
    /// `false` requires a real backend: a synthetic run's timings are configured, not measured.
    pub synthetic: Option<bool>,
    /// Budgets are for optimized builds. A debug run is slower by a factor no budget should absorb.
    pub debug_assertions: Option<bool>,
    /// Display geometry as `WIDTHxHEIGHT`, matched against any attached display.
    pub display_resolution: Option<String>,
    /// Part of a graphics adapter's name, e.g. `RTX 3070`, matched case-insensitively against any
    /// adapter the host enumerates.
    ///
    /// A fragment rather than the full string because the full string is a marketing name that
    /// changes with a driver package — `NVIDIA GeForce RTX 3070` today — and a budget that stopped
    /// applying after a driver update would be a budget nobody noticed had stopped.
    pub gpu: Option<String>,
    /// An exact user-mode driver version, e.g. `32.0.15.9186`.
    ///
    /// Exact, unlike `gpu`: this is for pinning a calibration to the driver it was measured on
    /// when a regression is being chased, and "roughly this driver" would defeat the purpose.
    pub driver_version: Option<String>,
    /// Part of the CPU's name, matched case-insensitively.
    pub cpu: Option<String>,
    /// A prefix of the OS build, e.g. `26200` — the build without pinning the monthly revision.
    pub os_build: Option<String>,
    /// The session state the budget was measured in, e.g. `interactive`.
    ///
    /// A run over Remote Desktop composes onto a virtual adapter DXGI will not duplicate, and a
    /// locked session measures a desktop nobody is looking at. Neither is the machine the budget
    /// describes, however identical the hardware underneath is.
    pub session: Option<String>,
    /// `ac` or `battery`. The same laptop meets a different budget on each.
    pub power_source: Option<String>,
    /// Whether the adapter the run went through is a software rasterizer.
    ///
    /// `false` is the one that matters: it is what keeps a GPU acquisition budget from being
    /// judged on a hosted CI runner, where the figures describe a CPU emulating a GPU. Matched
    /// against the adapter driving the run's displays rather than against every adapter
    /// enumerated, because every real desktop also enumerates the Microsoft Basic Render Driver.
    pub software_adapter: Option<bool>,
}

/// Ceilings in nanoseconds. Absent means unbudgeted.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AbsoluteBudgets {
    pub native_frame_p50_ns: Option<u64>,
    pub native_frame_p99_ns: Option<u64>,
    pub cpu_frame_p50_ns: Option<u64>,
    pub cpu_frame_p99_ns: Option<u64>,
    pub trigger_to_dequeue_p99_ns: Option<u64>,
}

/// Ceilings expressed against the run itself rather than against a constant.
///
/// These survive a hardware upgrade, which absolute numbers do not: "the tail is within 4x the
/// median" stays meaningful on a machine twice as fast, while "p99 under 3 ms" quietly stops
/// testing anything.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RelativeBudgets {
    /// How far p99 may exceed p50, as a multiple. Catches a long tail that a median hides.
    pub native_p99_over_p50: Option<f64>,
    pub cpu_p99_over_p50: Option<f64>,
    /// The share of attempts allowed to fail, as a percentage.
    pub failure_percent: Option<f64>,
}

/// One budget that was checked, and what it found.
#[derive(Clone, Debug, Serialize)]
pub struct BudgetCheck {
    pub name: String,
    pub limit: String,
    pub measured: String,
    pub met: bool,
}

/// The outcome of applying a budget file to a run.
#[derive(Debug, Serialize)]
pub struct BudgetOutcome {
    pub host: String,
    /// Empty when the budget applied. Populated when it did not, saying why.
    pub skipped_because: Vec<String>,
    pub checks: Vec<BudgetCheck>,
}

impl BudgetOutcome {
    /// Whether the run breached a budget that actually applied to it.
    ///
    /// A skipped budget is not a pass and not a failure — it is a measurement taken somewhere the
    /// claim was never made — so it can only ever be `false` here, and the skip is reported
    /// separately rather than folded into a boolean nobody would question afterwards.
    pub fn breached(&self) -> bool {
        self.skipped_because.is_empty() && self.checks.iter().any(|check| !check.met)
    }

    pub fn applied(&self) -> bool {
        self.skipped_because.is_empty()
    }
}

impl fmt::Display for BudgetOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.skipped_because.is_empty() {
            writeln!(
                formatter,
                "budgets for {} were not applied to this run:",
                self.host
            )?;
            for reason in &self.skipped_because {
                writeln!(formatter, "  {reason}")?;
            }
            return formatter.write_str("  the measurements above stand; nothing was judged");
        }
        writeln!(formatter, "budgets for {}:", self.host)?;
        for check in &self.checks {
            writeln!(
                formatter,
                "  [{}] {} limit {} measured {}",
                if check.met { "ok" } else { "BREACH" },
                check.name,
                check.limit,
                check.measured
            )?;
        }
        Ok(())
    }
}

pub fn load(path: &Path) -> Result<BudgetFile, AppError> {
    let text = std::fs::read_to_string(path).map_err(|source| AppError::Write {
        path: path.display().to_string(),
        source,
    })?;
    toml::from_str(&text)
        .map_err(|error| AppError::InvalidArgument(format!("invalid budget file: {error}")))
}

/// Applies a budget file to a report.
pub fn evaluate(budget: &BudgetFile, report: &BenchmarkReport) -> BudgetOutcome {
    let host = if budget.host.description.is_empty() {
        "the documented benchmark host".to_owned()
    } else {
        budget.host.description.clone()
    };
    let skipped_because = host_mismatches(&budget.host, report);
    if !skipped_because.is_empty() {
        return BudgetOutcome {
            host,
            skipped_because,
            checks: Vec::new(),
        };
    }
    BudgetOutcome {
        host,
        skipped_because: Vec::new(),
        checks: checks(budget, report),
    }
}

/// Applies a budget file to every run of a repeat set.
///
/// A performance claim is supported by repeats that *all* meet the budget, not by their average. A
/// mean hides one run in three breaching, and "usually under 2 ms" is not the claim anybody wants
/// to read on a latency figure — so each run is judged on its own and the caller is told which
/// ones failed.
pub fn evaluate_each(budget: &BudgetFile, reports: &[BenchmarkReport]) -> Vec<BudgetOutcome> {
    reports
        .iter()
        .map(|report| evaluate(budget, report))
        .collect()
}

/// Whether any run that the budget applied to breached it.
pub fn any_breached(outcomes: &[BudgetOutcome]) -> bool {
    outcomes.iter().any(BudgetOutcome::breached)
}

fn host_mismatches(host: &HostMatch, report: &BenchmarkReport) -> Vec<String> {
    let mut mismatches = Vec::new();
    mismatches.extend(compare("backend", host.backend.as_deref(), &report.backend));
    mismatches.extend(compare("mode", host.mode.as_deref(), &report.mode));
    mismatches.extend(compare("cursor", host.cursor.as_deref(), &report.cursor));
    if let Some(expected) = host.synthetic {
        if expected != report.synthetic {
            mismatches.push(format!(
                "synthetic is {}, budget describes {expected}",
                report.synthetic
            ));
        }
    }
    if let Some(expected) = host.debug_assertions {
        if expected != report.environment.debug_assertions {
            mismatches.push(format!(
                "debug_assertions is {}, budget describes {expected}",
                report.environment.debug_assertions
            ));
        }
    }
    if let Some(expected) = host.display_resolution.as_deref() {
        let attached: Vec<String> = report
            .environment
            .displays
            .iter()
            .map(|display| format!("{}x{}", display.width, display.height))
            .collect();
        if !attached.iter().any(|actual| actual == expected) {
            mismatches.push(format!(
                "no attached display is {expected}; this run had [{}]",
                attached.join(", ")
            ));
        }
    }
    let adapters = &report.environment.adapters;
    if let Some(expected) = host.gpu.as_deref() {
        let wanted = expected.to_lowercase();
        if !adapters
            .iter()
            .any(|adapter| adapter.description.to_lowercase().contains(&wanted))
        {
            mismatches.push(format!(
                "no adapter matches {expected}; this run had [{}]",
                describe(adapters.iter().map(|adapter| adapter.description.clone()))
            ));
        }
    }
    if let Some(expected) = host.driver_version.as_deref() {
        if !adapters
            .iter()
            .any(|adapter| adapter.driver_version.as_deref() == Some(expected))
        {
            mismatches.push(format!(
                "no adapter is on driver {expected}; this run had [{}]",
                describe(
                    adapters
                        .iter()
                        .map(|adapter| adapter.driver_version.clone().unwrap_or_else(unknown))
                )
            ));
        }
    }
    if let Some(expected) = host.cpu.as_deref() {
        let actual = report.environment.cpu.clone().unwrap_or_else(unknown);
        if !actual.to_lowercase().contains(&expected.to_lowercase()) {
            mismatches.push(format!("cpu is {actual}, budget describes {expected}"));
        }
    }
    if let Some(expected) = host.os_build.as_deref() {
        let actual = report.environment.os_build.clone().unwrap_or_else(unknown);
        if !actual.starts_with(expected) {
            mismatches.push(format!("os_build is {actual}, budget describes {expected}"));
        }
    }
    mismatches.extend(compare(
        "session",
        host.session.as_deref(),
        &report.environment.session.clone().unwrap_or_else(unknown),
    ));
    mismatches.extend(compare(
        "power_source",
        host.power_source.as_deref(),
        &report
            .environment
            .power_source
            .clone()
            .unwrap_or_else(unknown),
    ));
    if let Some(expected) = host.software_adapter {
        match report.environment.primary_adapter() {
            Some(adapter) if adapter.software != expected => {
                mismatches.push(format!(
                    "the adapter this run used ({}) {} a software rasterizer, budget describes                      software_adapter = {expected}",
                    adapter.description,
                    if adapter.software { "is" } else { "is not" }
                ));
            }
            Some(_) => {}
            None => mismatches.push(format!(
                "this run named no graphics adapter, so software_adapter = {expected} cannot be                  checked"
            )),
        }
    }
    mismatches
}

/// One field's mismatch, in the wording every skip reason shares, or `None` when it matched.
fn compare(field: &str, expected: Option<&str>, actual: &str) -> Option<String> {
    let expected = expected?;
    (expected != actual).then(|| format!("{field} is {actual}, budget describes {expected}"))
}

fn unknown() -> String {
    "unknown".to_owned()
}

fn describe(values: impl Iterator<Item = String>) -> String {
    values.collect::<Vec<_>>().join(", ")
}

fn checks(budget: &BudgetFile, report: &BenchmarkReport) -> Vec<BudgetCheck> {
    let mut checks = Vec::new();
    let mut absolute = |name: &str, limit: Option<u64>, measured: Option<u64>| {
        if let (Some(limit), Some(measured)) = (limit, measured) {
            checks.push(BudgetCheck {
                name: name.to_owned(),
                limit: format!("{limit} ns"),
                measured: format!("{measured} ns"),
                met: measured <= limit,
            });
        }
    };
    let native = &report.native_frame_latency;
    absolute(
        "native frame p50",
        budget.absolute.native_frame_p50_ns,
        Some(native.p50_ns),
    );
    absolute(
        "native frame p99",
        budget.absolute.native_frame_p99_ns,
        Some(native.p99_ns),
    );
    absolute(
        "cpu frame p50",
        budget.absolute.cpu_frame_p50_ns,
        report.cpu_frame_latency.as_ref().map(|s| s.p50_ns),
    );
    absolute(
        "cpu frame p99",
        budget.absolute.cpu_frame_p99_ns,
        report.cpu_frame_latency.as_ref().map(|s| s.p99_ns),
    );
    absolute(
        "trigger-to-dequeue p99",
        budget.absolute.trigger_to_dequeue_p99_ns,
        Some(report.trigger_to_dequeue_latency.p99_ns),
    );

    let mut ratio = |name: &str, limit: Option<f64>, p50: u64, p99: u64| {
        if let Some(limit) = limit {
            // A zero median makes the ratio undefined rather than infinite. Reported as met with
            // the measurement shown, because failing a run for being too fast to measure would be
            // absurd and silently passing it would hide that nothing was checked.
            let measured = if p50 == 0 {
                0.0
            } else {
                p99 as f64 / p50 as f64
            };
            checks.push(BudgetCheck {
                name: name.to_owned(),
                limit: format!("{limit:.2}x"),
                measured: format!("{measured:.2}x"),
                met: measured <= limit,
            });
        }
    };
    ratio(
        "native p99 over p50",
        budget.relative.native_p99_over_p50,
        native.p50_ns,
        native.p99_ns,
    );
    if let Some(cpu) = report.cpu_frame_latency.as_ref() {
        ratio(
            "cpu p99 over p50",
            budget.relative.cpu_p99_over_p50,
            cpu.p50_ns,
            cpu.p99_ns,
        );
    }
    if let Some(limit) = budget.relative.failure_percent {
        let attempted = report.timed_iterations.max(1);
        let measured = (report.failures as f64 / attempted as f64) * 100.0;
        checks.push(BudgetCheck {
            name: "failure rate".to_owned(),
            limit: format!("{limit:.2}%"),
            measured: format!("{measured:.2}%"),
            met: measured <= limit,
        });
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use captastic_core::LatencySummary;

    fn summary(p50: u64, p99: u64) -> LatencySummary {
        LatencySummary {
            count: 100,
            min_ns: p50 / 2,
            p50_ns: p50,
            p90_ns: p99,
            p95_ns: p99,
            p99_ns: p99,
            max_ns: p99,
            mean_ns: p50,
        }
    }

    fn report() -> BenchmarkReport {
        let run = crate::benchmark::run(&crate::benchmark::BenchmarkOptions {
            iterations: 1,
            warmup: 0,
            mode: captastic_core::CaptureMode::Latest { max_age_ms: None },
            cpu_frame: true,
            cursor: captastic_core::CursorMode::Exclude,
            source: captastic_core::CaptureSource::Display(captastic_core::DisplayId::primary()),
            trigger_queue_capacity: 4,
            metrics_capacity: 64,
            fake: captastic_core::FakeBackendConfig {
                native_delay: std::time::Duration::ZERO,
                readback_delay: std::time::Duration::ZERO,
                ..captastic_core::FakeBackendConfig::default()
            },
        })
        .expect("a one-iteration run");
        run.report
    }

    #[test]
    fn a_budget_for_another_machine_is_skipped_rather_than_failed() {
        // The failure mode this exists to prevent: a GPU timing budget evaluated on a hosted CI
        // runner fails every time, and a suite that always fails is a suite nobody reads.
        let mut report = report();
        report.environment.debug_assertions = true;
        let budget = BudgetFile {
            host: HostMatch {
                description: "the bench box".to_owned(),
                backend: Some("dxgi".to_owned()),
                debug_assertions: Some(false),
                ..HostMatch::default()
            },
            absolute: AbsoluteBudgets {
                native_frame_p50_ns: Some(1),
                ..AbsoluteBudgets::default()
            },
            relative: RelativeBudgets::default(),
        };

        let outcome = evaluate(&budget, &report);
        assert!(!outcome.applied());
        // Emphatically not a breach: nothing was judged, so nothing failed.
        assert!(!outcome.breached());
        assert!(outcome.checks.is_empty());
        assert_eq!(outcome.skipped_because.len(), 2, "{outcome:?}");
        let text = outcome.to_string();
        assert!(text.contains("were not applied"), "{text}");
        assert!(text.contains("nothing was judged"), "{text}");
    }

    #[test]
    fn a_budget_for_this_machine_is_enforced() {
        let mut report = report();
        report.native_frame_latency = summary(1_000, 3_000);
        let budget = BudgetFile {
            host: HostMatch {
                description: "here".to_owned(),
                backend: Some("fake".to_owned()),
                ..HostMatch::default()
            },
            absolute: AbsoluteBudgets {
                native_frame_p50_ns: Some(2_000),
                ..AbsoluteBudgets::default()
            },
            relative: RelativeBudgets::default(),
        };

        let outcome = evaluate(&budget, &report);
        assert!(outcome.applied());
        assert!(!outcome.breached());
        assert_eq!(outcome.checks.len(), 1);

        // And it can fail, which is the other half of being enforced.
        report.native_frame_latency = summary(9_000, 12_000);
        let outcome = evaluate(&budget, &report);
        assert!(outcome.breached());
        assert!(outcome.to_string().contains("BREACH"), "{outcome}");
    }

    #[test]
    fn a_relative_budget_survives_a_faster_machine() {
        // The point of expressing a tail against the median: an absolute p99 stops testing
        // anything on hardware twice as fast, while "within 4x the median" still does.
        let mut report = report();
        let budget = BudgetFile {
            host: HostMatch::default(),
            absolute: AbsoluteBudgets::default(),
            relative: RelativeBudgets {
                native_p99_over_p50: Some(4.0),
                ..RelativeBudgets::default()
            },
        };

        report.native_frame_latency = summary(1_000, 3_000);
        assert!(!evaluate(&budget, &report).breached());
        // Same shape, machine ten times faster: still within budget.
        report.native_frame_latency = summary(100, 300);
        assert!(!evaluate(&budget, &report).breached());
        // A genuine tail regression is caught at either speed.
        report.native_frame_latency = summary(100, 900);
        assert!(evaluate(&budget, &report).breached());
    }

    #[test]
    fn a_median_of_zero_does_not_produce_an_infinite_ratio() {
        let mut report = report();
        report.native_frame_latency = summary(0, 5_000);
        let budget = BudgetFile {
            host: HostMatch::default(),
            absolute: AbsoluteBudgets::default(),
            relative: RelativeBudgets {
                native_p99_over_p50: Some(4.0),
                ..RelativeBudgets::default()
            },
        };
        let outcome = evaluate(&budget, &report);
        assert!(!outcome.breached());
        assert_eq!(outcome.checks[0].measured, "0.00x");
    }

    #[test]
    fn every_repeat_must_meet_a_budget_for_it_to_be_met() {
        // A mean would hide one run in three breaching. "Usually under 2 ms" is not a latency
        // figure anybody wants to read, so each run is judged on its own.
        let budget = BudgetFile {
            host: HostMatch::default(),
            absolute: AbsoluteBudgets {
                native_frame_p50_ns: Some(2_000),
                ..AbsoluteBudgets::default()
            },
            relative: RelativeBudgets::default(),
        };
        let mut good = report();
        good.native_frame_latency = summary(1_000, 1_200);
        let mut bad = report();
        bad.native_frame_latency = summary(9_000, 9_500);

        let all_good = evaluate_each(&budget, &[good.clone(), good.clone(), good.clone()]);
        assert_eq!(all_good.len(), 3);
        assert!(!any_breached(&all_good));

        // One bad run in three is a breach, and the outcomes say which.
        let mixed = evaluate_each(&budget, &[good.clone(), bad, good]);
        assert!(any_breached(&mixed));
        assert_eq!(mixed.iter().filter(|outcome| outcome.breached()).count(), 1);
    }

    /// A report describing a real desktop: a GPU, and the Basic Render Driver every desktop has.
    fn report_on_a_gpu() -> BenchmarkReport {
        use crate::fingerprint::AdapterFingerprint;
        let mut report = report();
        report.environment.adapters = vec![
            AdapterFingerprint {
                description: "NVIDIA GeForce RTX 3070".to_owned(),
                vendor_id: 0x10de,
                device_id: 0x2484,
                luid: 62_908,
                software: false,
                dedicated_video_memory_mb: 8_018,
                driver_version: Some("32.0.15.9186".to_owned()),
            },
            AdapterFingerprint {
                description: "Microsoft Basic Render Driver".to_owned(),
                vendor_id: 0x1414,
                device_id: 0x8c,
                luid: 69_935,
                software: true,
                dedicated_video_memory_mb: 0,
                driver_version: Some("10.0.26100.9278".to_owned()),
            },
        ];
        report.environment.cpu = Some("AMD Ryzen 9 5900X 12-Core Processor".to_owned());
        report.environment.os_build = Some("26200.9457 (25H2)".to_owned());
        report.environment.session = Some("interactive".to_owned());
        report.environment.power_source = Some("ac".to_owned());
        report
    }

    fn documented_host() -> HostMatch {
        HostMatch {
            description: "the bench box".to_owned(),
            gpu: Some("RTX 3070".to_owned()),
            cpu: Some("ryzen 9 5900x".to_owned()),
            os_build: Some("26200".to_owned()),
            session: Some("interactive".to_owned()),
            power_source: Some("ac".to_owned()),
            software_adapter: Some(false),
            ..HostMatch::default()
        }
    }

    #[test]
    fn the_documented_host_matches_the_machine_it_documents() {
        // Including the detail that made "is any adapter a software one" the wrong question: this
        // desktop enumerates the Basic Render Driver beside its GPU, as every desktop does, and a
        // budget that read that as "software rasterizer" would never apply anywhere real.
        let budget = BudgetFile {
            host: documented_host(),
            absolute: AbsoluteBudgets::default(),
            relative: RelativeBudgets {
                failure_percent: Some(0.0),
                ..RelativeBudgets::default()
            },
        };
        let outcome = evaluate(&budget, &report_on_a_gpu());
        assert!(outcome.applied(), "{outcome:?}");
        // A fragment of the marketing name matches, so a driver package rewording it does not
        // unpin the budget.
        let mut renamed = report_on_a_gpu();
        renamed.environment.adapters[0].description = "NVIDIA GeForce RTX 3070 Ti".to_owned();
        assert!(evaluate(&budget, &renamed).applied());
    }

    #[test]
    fn a_run_on_a_software_adapter_is_skipped_loudly_and_told_why() {
        // The hosted CI case, which is why `software_adapter` exists at all: every GPU acquisition
        // figure there describes a CPU emulating a GPU, so nothing may be judged and the reason
        // has to be visible rather than absorbed into a pass.
        let mut report = report_on_a_gpu();
        report.environment.adapters.remove(0);
        let budget = BudgetFile {
            host: documented_host(),
            absolute: AbsoluteBudgets {
                native_frame_p50_ns: Some(1),
                ..AbsoluteBudgets::default()
            },
            relative: RelativeBudgets::default(),
        };

        let outcome = evaluate(&budget, &report);
        assert!(!outcome.applied());
        // Emphatically not a breach: nothing was judged, so nothing failed.
        assert!(!outcome.breached());
        assert!(outcome.checks.is_empty());
        let text = outcome.to_string();
        assert!(text.contains("Microsoft Basic Render Driver"), "{text}");
        assert!(text.contains("software_adapter = false"), "{text}");
        // And the GPU the budget names is missing too, which is said rather than implied.
        assert!(text.contains("RTX 3070"), "{text}");
        assert!(text.contains("nothing was judged"), "{text}");
    }

    #[test]
    fn every_new_host_field_can_refuse_a_run_and_says_which_one_did() {
        let reasons = |host: HostMatch, report: &BenchmarkReport| {
            let budget = BudgetFile {
                host,
                absolute: AbsoluteBudgets::default(),
                relative: RelativeBudgets::default(),
            };
            evaluate(&budget, report).skipped_because
        };
        let report = report_on_a_gpu();

        for (host, expected) in [
            (
                HostMatch {
                    gpu: Some("RX 7900".to_owned()),
                    ..HostMatch::default()
                },
                "RX 7900",
            ),
            (
                HostMatch {
                    driver_version: Some("31.0.15.7270".to_owned()),
                    ..HostMatch::default()
                },
                "31.0.15.7270",
            ),
            (
                HostMatch {
                    cpu: Some("Core i9".to_owned()),
                    ..HostMatch::default()
                },
                "cpu is",
            ),
            (
                HostMatch {
                    os_build: Some("22631".to_owned()),
                    ..HostMatch::default()
                },
                "os_build is",
            ),
            (
                HostMatch {
                    power_source: Some("battery".to_owned()),
                    ..HostMatch::default()
                },
                "power_source is",
            ),
        ] {
            let skipped = reasons(host, &report);
            assert_eq!(skipped.len(), 1, "{skipped:?}");
            assert!(skipped[0].contains(expected), "{skipped:?}");
        }

        // The session is checked against a run that was not at the desk, which is the mismatch
        // that actually happens: the same machine, over Remote Desktop.
        let mut remote = report;
        remote.environment.session = Some("remote".to_owned());
        let skipped = reasons(
            HostMatch {
                session: Some("interactive".to_owned()),
                ..HostMatch::default()
            },
            &remote,
        );
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].contains("session is remote"), "{skipped:?}");
    }

    #[test]
    fn a_host_fact_the_machine_would_not_answer_does_not_pass_by_default() {
        // Absence is not agreement. A budget that names a session must not apply to a run whose
        // session could not be determined, or the skip it exists to cause never happens.
        let mut report = report_on_a_gpu();
        report.environment.session = None;
        report.environment.power_source = None;
        let skipped = evaluate(
            &BudgetFile {
                host: HostMatch {
                    session: Some("interactive".to_owned()),
                    power_source: Some("ac".to_owned()),
                    ..HostMatch::default()
                },
                absolute: AbsoluteBudgets::default(),
                relative: RelativeBudgets::default(),
            },
            &report,
        )
        .skipped_because;
        assert_eq!(skipped.len(), 2, "{skipped:?}");
    }

    #[test]
    fn the_committed_budget_file_still_parses() {
        // `deny_unknown_fields` is the point of this test. The file is hand-edited and nothing
        // else in the suite reads it, so a renamed or mistyped host field would first be noticed
        // by whoever was mid-way through a measuring session with the machine set up for it.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/budgets.toml");
        let budget = load(&path).expect("the committed budget file parses");
        assert_eq!(budget.host.gpu.as_deref(), Some("RTX 3070"));
        assert_eq!(budget.host.session.as_deref(), Some("interactive"));
        assert_eq!(budget.host.power_source.as_deref(), Some("ac"));
        assert_eq!(budget.host.software_adapter, Some(false));
        // The description is prose for the skip message; nothing is derived from it.
        assert!(budget.host.description.contains("RTX 3070"));
    }

    #[test]
    fn an_empty_host_matches_anywhere() {
        // A budget that names no host is a claim about the software rather than the machine -
        // queue handoff, failure rate - and applies wherever it runs.
        let budget = BudgetFile {
            host: HostMatch::default(),
            absolute: AbsoluteBudgets::default(),
            relative: RelativeBudgets {
                failure_percent: Some(0.0),
                ..RelativeBudgets::default()
            },
        };
        let outcome = evaluate(&budget, &report());
        assert!(outcome.applied());
        assert_eq!(outcome.checks.len(), 1);
    }
}
