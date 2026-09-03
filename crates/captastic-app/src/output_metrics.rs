//! What a destination cost, counted apart from what the capture cost.
//!
//! ADR 0002 forbids destination work from being counted as capture latency, and the fan-out gave
//! each destination its own trace so the two cannot be conflated per capture. This is the same
//! separation in aggregate: encode time, write time, bytes, collisions and failures belong to the
//! destination, and reporting them beside native-frame or CPU-frame percentiles would invite
//! exactly the comparison ADR 0002 exists to prevent.

use captastic_core::LatencySummary;
use serde_json::json;

/// How many recent samples back the latency percentiles.
///
/// Bounded because a daemon runs for weeks: the counts below are exact for the whole run, and the
/// percentiles describe the recent past, which is the part anyone is asking about.
const SAMPLE_CAPACITY: usize = 1_024;

/// Running totals for one destination.
pub struct OutputMetrics {
    destination: &'static str,
    written: u64,
    failed: u64,
    /// Names that were already taken, summed. A steady rise means captures are arriving faster
    /// than the name template can distinguish them.
    collisions: u64,
    bytes: u64,
    encode_ns: Vec<u64>,
    write_ns: Vec<u64>,
}

impl OutputMetrics {
    pub fn new(destination: &'static str) -> Self {
        Self {
            destination,
            written: 0,
            failed: 0,
            collisions: 0,
            bytes: 0,
            encode_ns: Vec::new(),
            write_ns: Vec::new(),
        }
    }

    pub fn record_write(&mut self, bytes: usize, encode_ns: u64, write_ns: u64, collisions: u32) {
        self.written = self.written.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
        self.collisions = self.collisions.saturating_add(u64::from(collisions));
        push_bounded(&mut self.encode_ns, encode_ns);
        push_bounded(&mut self.write_ns, write_ns);
    }

    pub fn record_failure(&mut self) {
        self.failed = self.failed.saturating_add(1);
    }

    /// True when this destination did anything at all, so an unused one stays silent.
    pub fn is_empty(&self) -> bool {
        self.written == 0 && self.failed == 0
    }

    pub fn summary(&self) -> OutputSummary {
        OutputSummary {
            destination: self.destination,
            written: self.written,
            failed: self.failed,
            collisions: self.collisions,
            bytes: self.bytes,
            encode: LatencySummary::from_samples(&self.encode_ns),
            write: LatencySummary::from_samples(&self.write_ns),
        }
    }
}

/// Keeps the most recent `SAMPLE_CAPACITY` samples, dropping the oldest.
///
/// Dropping the oldest rather than refusing the newest: the question a percentile answers is about
/// how the destination is behaving now, and a buffer frozen at startup answers it about a daemon
/// that has since been running for a fortnight.
fn push_bounded(samples: &mut Vec<u64>, value: u64) {
    if samples.len() == SAMPLE_CAPACITY {
        samples.remove(0);
    }
    samples.push(value);
}

pub struct OutputSummary {
    pub destination: &'static str,
    pub written: u64,
    pub failed: u64,
    pub collisions: u64,
    pub bytes: u64,
    pub encode: LatencySummary,
    pub write: LatencySummary,
}

impl OutputSummary {
    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "schema_version": 1,
            "event": "output_summary",
            "destination": self.destination,
            "written": self.written,
            "failed": self.failed,
            "collisions": self.collisions,
            "bytes": self.bytes,
            "encode_ns": summary_json(&self.encode),
            "write_ns": summary_json(&self.write),
        })
    }

    /// A one-line human form, deliberately not shaped like the capture-latency lines beside it.
    pub fn to_line(&self) -> String {
        format!(
            "{} output: {} written ({:.1} MiB), {} failed, {} name collision(s); encode p50 {:.1} ms p99 {:.1} ms, write p50 {:.1} ms p99 {:.1} ms",
            self.destination,
            self.written,
            self.bytes as f64 / (1024.0 * 1024.0),
            self.failed,
            self.collisions,
            ns_to_ms(self.encode.p50_ns),
            ns_to_ms(self.encode.p99_ns),
            ns_to_ms(self.write.p50_ns),
            ns_to_ms(self.write.p99_ns),
        )
    }
}

fn summary_json(summary: &LatencySummary) -> serde_json::Value {
    json!({
        "count": summary.count,
        "min": summary.min_ns,
        "p50": summary.p50_ns,
        "p90": summary.p90_ns,
        "p95": summary.p95_ns,
        "p99": summary.p99_ns,
        "max": summary.max_ns,
        "mean": summary.mean_ns,
    })
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_exact_and_percentiles_describe_the_samples() {
        let mut metrics = OutputMetrics::new("file");
        for index in 1..=10_u64 {
            metrics.record_write(1_000, index * 1_000_000, index * 2_000_000, 0);
        }
        metrics.record_failure();

        let summary = metrics.summary();
        assert_eq!(summary.written, 10);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.bytes, 10_000);
        assert_eq!(summary.encode.count, 10);
        assert_eq!(summary.encode.min_ns, 1_000_000);
        assert_eq!(summary.encode.max_ns, 10_000_000);
        assert_eq!(summary.write.max_ns, 20_000_000);
    }

    #[test]
    fn collisions_accumulate_across_captures() {
        // A steady rise here is the signal that captures are arriving faster than the name
        // template can tell them apart, which is worth being able to see.
        let mut metrics = OutputMetrics::new("file");
        metrics.record_write(10, 1, 1, 0);
        metrics.record_write(10, 1, 1, 3);
        metrics.record_write(10, 1, 1, 1);

        assert_eq!(metrics.summary().collisions, 4);
    }

    #[test]
    fn counts_stay_exact_after_the_sample_buffer_is_full() {
        // The buffer bounds memory over a long run; it must not bound the counts, which are what
        // someone reads to know how much a daemon has actually written.
        let mut metrics = OutputMetrics::new("file");
        for index in 0..(SAMPLE_CAPACITY as u64 + 500) {
            metrics.record_write(1, index, index, 0);
        }

        let summary = metrics.summary();
        assert_eq!(summary.written, SAMPLE_CAPACITY as u64 + 500);
        assert_eq!(summary.bytes, SAMPLE_CAPACITY as u64 + 500);
        assert_eq!(summary.encode.count, SAMPLE_CAPACITY);
        // The retained window is the recent past, so the oldest samples are the ones dropped.
        assert_eq!(summary.encode.min_ns, 500);
        assert_eq!(summary.encode.max_ns, SAMPLE_CAPACITY as u64 + 499);
    }

    #[test]
    fn an_unused_destination_reports_nothing() {
        // A daemon with file output disabled should not emit an empty summary that reads like a
        // destination which tried and did nothing.
        let metrics = OutputMetrics::new("file");
        assert!(metrics.is_empty());

        let mut used = OutputMetrics::new("file");
        used.record_failure();
        assert!(
            !used.is_empty(),
            "a failure is still activity worth reporting"
        );
    }

    #[test]
    fn the_summary_names_its_destination_in_both_forms() {
        let mut metrics = OutputMetrics::new("file");
        metrics.record_write(2_097_152, 5_000_000, 1_000_000, 2);
        let summary = metrics.summary();

        let line = summary.to_line();
        assert!(
            line.starts_with("file output: 1 written (2.0 MiB)"),
            "{line}"
        );
        assert!(line.contains("2 name collision(s)"), "{line}");

        let json = summary.to_json();
        assert_eq!(json["destination"], "file");
        assert_eq!(json["written"], 1);
        assert_eq!(json["collisions"], 2);
        assert_eq!(json["encode_ns"]["p50"], 5_000_000);
    }
}
