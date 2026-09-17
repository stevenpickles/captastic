//! What machine a report describes.
//!
//! ROADMAP Milestone 5 will not publish a performance number without "three compatible repeat
//! runs", and nothing can decide whether two runs are compatible without first knowing what they
//! ran on. Until now a report said only the OS, the architecture, the build version and the display
//! geometry — enough to notice a debug build or a resolution change, and not enough to notice a GPU
//! driver update, a different power plan, a laptop on battery, or a run started over Remote
//! Desktop, all of which move the numbers by more than the regressions the benchmark exists to
//! catch.
//!
//! So the fingerprint is split in two, deliberately:
//!
//! - **Identity**: everything that decides whether two runs asked the same question. It is compared
//!   between runs and matched by budget files, so it is stable by construction — lower-case tokens
//!   rather than prose, and absent rather than guessed when a probe cannot answer.
//! - **Volatile**: `recorded_at_utc`, which says when the run happened and is never compared. It is
//!   here because a baseline file six months old should say so to whoever reads it, and nowhere
//!   near the comparison because two runs a minute apart are not incomparable for it.
//!
//! Everything optional is `Option`/empty off Windows and wherever a probe is refused. A run that
//! succeeded must still be describable when one fact about the host was not available: CI runs on
//! `windows-latest`, where there is a software adapter and often no interactive session at all, and
//! a fingerprint that failed there would take the whole benchmark with it.

use std::time::{SystemTime, UNIX_EPOCH};

use captastic_core::DisplayInfo;
use serde::{Deserialize, Serialize};

use crate::build_info::{BuildInfo, BUILD_INFO};

/// The host a report describes, and when the report was taken.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EnvironmentFingerprint {
    pub os: String,
    pub architecture: String,
    pub build: BuildIdentity,
    pub debug_assertions: bool,
    pub displays: Vec<DisplayFingerprint>,
    /// The OS build, e.g. `26100.4061 (24H2)`. A driver-level change arrives at this granularity.
    pub os_build: Option<String>,
    pub cpu: Option<String>,
    pub logical_cpus: Option<usize>,
    pub adapters: Vec<AdapterFingerprint>,
    /// The session's state as a stable token: `interactive`, `remote`, `locked`, …
    ///
    /// A run over Remote Desktop composes onto a virtual adapter DXGI will not duplicate, so it
    /// measures something else entirely — and a locked session measures a desktop nobody is
    /// looking at. Neither is comparable with a run at the desk.
    pub session: Option<String>,
    /// `ac`, `battery`, or `unknown`.
    pub power_source: Option<String>,
    pub power_plan: Option<String>,
    /// When this was recorded. Volatile: reported, never compared.
    pub recorded_at_utc: String,
}

/// An owned mirror of [`BuildInfo`], so a report can be read back as well as written.
///
/// [`BuildInfo`] is a compile-time constant of `&'static str`s, which cannot be deserialized into:
/// nothing can turn a JSON string into a `&'static str` without leaking it. The field names and
/// their order match exactly, so the JSON a report emits is unchanged by this.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuildIdentity {
    pub release_version: String,
    pub version: String,
    pub channel: String,
    pub git_commit: Option<String>,
    pub git_short_commit: Option<String>,
    pub revision_count: Option<u64>,
    pub source_tag: Option<String>,
    pub dirty: bool,
    pub ci_run_id: Option<String>,
    pub ci_run_number: Option<u64>,
    pub ci_run_attempt: Option<u64>,
    pub ci_run_url: Option<String>,
    pub target: String,
    pub profile: String,
}

impl From<BuildInfo> for BuildIdentity {
    fn from(info: BuildInfo) -> Self {
        Self {
            release_version: info.release_version.to_owned(),
            version: info.version.to_owned(),
            channel: info.channel.to_owned(),
            git_commit: info.git_commit.map(str::to_owned),
            git_short_commit: info.git_short_commit.map(str::to_owned),
            revision_count: info.revision_count,
            source_tag: info.source_tag.map(str::to_owned),
            dirty: info.dirty,
            ci_run_id: info.ci_run_id.map(str::to_owned),
            ci_run_number: info.ci_run_number,
            ci_run_attempt: info.ci_run_attempt,
            ci_run_url: info.ci_run_url.map(str::to_owned),
            target: info.target.to_owned(),
            profile: info.profile.to_owned(),
        }
    }
}

/// One graphics adapter the host has.
///
/// Every adapter, not just the one that drove the capture: a laptop that switched from its
/// integrated GPU to its discrete one between two runs is exactly the difference a published
/// latency figure must not average over, and the one that is hardest to notice afterwards.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdapterFingerprint {
    pub description: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub luid: i64,
    /// Whether this is a software rasterizer. The fact that decides if a GPU budget means anything.
    pub software: bool,
    pub dedicated_video_memory_mb: u64,
    pub driver_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DisplayFingerprint {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub rotation_degrees: u16,
    pub primary: bool,
    pub scale_factor: f32,
    /// The active refresh rate in Hz, where Windows reports one.
    pub refresh_hz: Option<f64>,
    /// Which adapter drives this display, matching [`AdapterFingerprint::luid`].
    pub adapter_luid: Option<i64>,
}

impl EnvironmentFingerprint {
    /// Describes the machine this process is running on, alongside the displays a run used.
    ///
    /// The displays are passed in rather than probed because they are the backend's answer: a fake
    /// run has synthetic displays and must say so, and a real run must describe the outputs it
    /// actually enumerated rather than a second enumeration that may disagree.
    pub fn collect(displays: &[DisplayInfo]) -> Self {
        let host = HostFacts::probe();
        Self {
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            build: BUILD_INFO.into(),
            debug_assertions: cfg!(debug_assertions),
            displays: displays
                .iter()
                .map(|display| {
                    let hardware = host
                        .displays
                        .iter()
                        .find(|hardware| hardware.display_id == display.id.0);
                    DisplayFingerprint {
                        id: display.id.0.clone(),
                        name: display.name.clone(),
                        width: display.bounds.width,
                        height: display.bounds.height,
                        rotation_degrees: display.rotation_degrees,
                        primary: display.is_primary,
                        scale_factor: display.scale_factor,
                        refresh_hz: hardware.and_then(|hardware| hardware.refresh_hz),
                        adapter_luid: hardware.and_then(|hardware| hardware.adapter_luid),
                    }
                })
                .collect(),
            os_build: host.os_build,
            cpu: host.cpu,
            // Asked on every platform, because it is the one host fact the standard library can
            // answer anywhere, and a run's thread count is part of what it measured.
            logical_cpus: std::thread::available_parallelism()
                .ok()
                .map(std::num::NonZeroUsize::get),
            adapters: host.adapters,
            session: host.session,
            power_source: host.power_source,
            power_plan: host.power_plan,
            recorded_at_utc: recorded_now(),
        }
    }

    /// The adapter this run actually went through, as far as the fingerprint can tell.
    ///
    /// Not "is any adapter a software one": every real desktop enumerates the Microsoft Basic
    /// Render Driver alongside its GPU, so that question answers `true` on Steven's RTX 3070 box
    /// and would disqualify the one host the budgets describe. The one that matters is the adapter
    /// driving the run's displays; where no display named one — a synthetic run, or a host with no
    /// attached display at all — the first enumerated adapter is the answer, which on a hosted CI
    /// runner is the software rasterizer and on a desktop is the GPU.
    pub fn primary_adapter(&self) -> Option<&AdapterFingerprint> {
        let driving = self
            .displays
            .iter()
            .find_map(|display| display.adapter_luid)
            .and_then(|luid| self.adapters.iter().find(|adapter| adapter.luid == luid));
        driving.or_else(|| self.adapters.first())
    }
}

fn recorded_now() -> String {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_micros())
        .unwrap_or_default();
    crate::logging::format_utc_timestamp(micros)
}

/// What the platform could be asked about the host.
#[derive(Debug, Default)]
struct HostFacts {
    os_build: Option<String>,
    cpu: Option<String>,
    adapters: Vec<AdapterFingerprint>,
    displays: Vec<DisplayHardwareFacts>,
    session: Option<String>,
    power_source: Option<String>,
    power_plan: Option<String>,
}

#[derive(Debug)]
struct DisplayHardwareFacts {
    display_id: String,
    adapter_luid: Option<i64>,
    refresh_hz: Option<f64>,
}

impl HostFacts {
    #[cfg(windows)]
    fn probe() -> Self {
        let power = captastic_windows::power_status();
        Self {
            os_build: captastic_windows::os_build(),
            cpu: captastic_windows::processor_name(),
            adapters: captastic_windows::adapters()
                .into_iter()
                .map(|adapter| AdapterFingerprint {
                    description: adapter.description,
                    vendor_id: adapter.vendor_id,
                    device_id: adapter.device_id,
                    luid: adapter.luid,
                    software: adapter.software,
                    dedicated_video_memory_mb: adapter.dedicated_video_memory_mb,
                    driver_version: adapter.driver_version,
                })
                .collect(),
            displays: captastic_windows::display_hardware()
                .into_iter()
                .map(|display| DisplayHardwareFacts {
                    display_id: display.display_id,
                    adapter_luid: display.adapter_luid,
                    refresh_hz: display.refresh_hz,
                })
                .collect(),
            session: Some(captastic_windows::desktop_state().token().to_owned()),
            power_source: power.source,
            power_plan: power.plan,
        }
    }

    /// Off Windows every one of these is unanswerable, which the fingerprint states as absence.
    ///
    /// Absent rather than `"unknown"`: a budget that matches on `session` should not match a host
    /// that has no notion of one, and a comparison should not treat two unanswerable probes as
    /// agreement.
    #[cfg(not(windows))]
    fn probe() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_identity_mirrors_the_build_constant_field_for_field() {
        // The report's JSON must not change shape when the build stops being a compile-time
        // constant: an owned mirror that renamed or dropped a field would quietly break every
        // consumer reading `build.version`.
        let identity: BuildIdentity = BUILD_INFO.into();
        assert_eq!(identity.version, BUILD_INFO.version);
        assert_eq!(identity.release_version, BUILD_INFO.release_version);
        assert_eq!(identity.channel, BUILD_INFO.channel);
        assert_eq!(identity.target, BUILD_INFO.target);
        assert_eq!(identity.profile, BUILD_INFO.profile);
        assert_eq!(identity.dirty, BUILD_INFO.dirty);
        assert_eq!(
            identity.git_short_commit.as_deref(),
            BUILD_INFO.git_short_commit
        );

        let json = serde_json::to_value(&identity).expect("the identity serializes");
        let constant = serde_json::to_value(BUILD_INFO).expect("the constant serializes");
        // Not "every field I remembered to assert above": every field the constant has, so adding
        // one to `BuildInfo` and forgetting it here fails rather than silently shrinking the report.
        assert_eq!(json, constant);
    }

    #[test]
    fn a_fingerprint_survives_a_host_that_answers_nothing() {
        // Not a hypothetical: this is what CI looks like, and a fingerprint that failed there
        // would take the whole benchmark down with it.
        let fingerprint = EnvironmentFingerprint::collect(&[]);
        assert_eq!(fingerprint.os, std::env::consts::OS);
        assert_eq!(fingerprint.architecture, std::env::consts::ARCH);
        assert!(fingerprint.displays.is_empty());
        assert!(!fingerprint.recorded_at_utc.is_empty());
        assert!(fingerprint.recorded_at_utc.ends_with('Z'));
    }

    #[cfg(not(windows))]
    #[test]
    fn a_host_that_cannot_be_asked_reports_absence_rather_than_a_guess() {
        let fingerprint = EnvironmentFingerprint::collect(&[]);
        assert_eq!(fingerprint.os_build, None);
        assert_eq!(fingerprint.cpu, None);
        assert_eq!(fingerprint.session, None);
        assert_eq!(fingerprint.power_source, None);
        assert_eq!(fingerprint.power_plan, None);
        assert!(fingerprint.adapters.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn the_live_host_answers_without_panicking() {
        // Asserts only what is true of any machine able to run it — including a hosted runner with
        // a software adapter and no interactive session, which is where this runs in CI.
        let fingerprint = EnvironmentFingerprint::collect(&[]);
        if let Some(build) = fingerprint.os_build.as_deref() {
            assert!(!build.is_empty());
        }
        if let Some(cpu) = fingerprint.cpu.as_deref() {
            assert!(!cpu.is_empty());
        }
        let session = fingerprint
            .session
            .expect("Windows always answers a session");
        assert!(!session.is_empty());
        for adapter in &fingerprint.adapters {
            assert!(!adapter.description.is_empty());
        }
        assert!(fingerprint.logical_cpus.is_some_and(|count| count >= 1));
    }
}
