//! Accounting for native workers that were left running on purpose.
//!
//! Two places give up waiting for a worker rather than blocking on it: a window render that
//! outlives its deadline, and the daemon's capture worker at shutdown. Both are deliberate. A
//! worker can be blocked inside a foreign window procedure or a display driver, and joining it
//! would hand Captastic's responsiveness — or its exit — to whatever is wedged.
//!
//! What was missing is the accounting. A detached worker keeps its thread and whatever GPU or GDI
//! resources it holds, so detaching can be neither free nor unbounded; and a detach that happens
//! once is a wedged window, while a detach that happens on every capture is a bug in Captastic.
//! Neither of those was visible. The only count that existed was a live gauge in a private static,
//! which meant a worker that detached and finished a second later left no trace behind it at all.
//!
//! So each kind records two numbers. `live` is the pressure right now, because a detached worker
//! holds a resource that a later capture will need. `total` is the history, because that is the one
//! that says whether detaching is an accident or the normal course of events.
//!
//! The policy this implements — including where each ceiling comes from — is ADR 0005.

use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

/// A kind of worker that Captastic is willing to abandon at a deadline.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetachKind {
    /// A `PrintWindow`/WGC render of one window, abandoned at the render deadline.
    WindowRender,
    /// The daemon's capture thread, abandoned when it outlives the shutdown budget.
    CaptureWorker,
}

impl DetachKind {
    pub const ALL: [Self; 2] = [Self::WindowRender, Self::CaptureWorker];

    /// How many of this kind may be detached at once before Captastic is out of room.
    ///
    /// Neither ceiling is enforced here — both are enforced by the thing that owns the resource,
    /// and stating them in one place is what lets those two be checked against each other. A
    /// window render holds one of eight worker slots until it exits, so eight detached renders
    /// means the next one is refused outright; the daemon has exactly one capture worker, and
    /// detaching it is the last thing that happens before the process exits.
    pub const fn ceiling(self) -> usize {
        match self {
            Self::WindowRender => 8,
            Self::CaptureWorker => 1,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::WindowRender => "window render",
            Self::CaptureWorker => "capture worker",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::WindowRender => 0,
            Self::CaptureWorker => 1,
        }
    }
}

/// What the ledger holds for one kind.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DetachCount {
    /// Detached workers that have not since returned.
    pub live: usize,
    /// Every detach of this kind since the process started, including those that later returned.
    pub total: usize,
}

impl DetachCount {
    /// True once no further worker of this kind can be detached without exceeding its ceiling.
    pub fn at_ceiling(&self, kind: DetachKind) -> bool {
        self.live >= kind.ceiling()
    }
}

/// A process-wide tally of detached workers.
#[derive(Debug, Default)]
pub struct DetachLedger {
    live: [AtomicUsize; DetachKind::ALL.len()],
    total: [AtomicUsize; DetachKind::ALL.len()],
}

impl DetachLedger {
    pub const fn new() -> Self {
        Self {
            live: [AtomicUsize::new(0), AtomicUsize::new(0)],
            total: [AtomicUsize::new(0), AtomicUsize::new(0)],
        }
    }

    /// Records a detach and returns the counts as they stand after it.
    ///
    /// The caller gets the numbers back rather than reading them afterwards, so the count it logs
    /// is the one its own detach produced instead of whatever a concurrent detach left behind.
    pub fn detached(&self, kind: DetachKind) -> DetachCount {
        let index = kind.index();
        DetachCount {
            live: self.live[index].fetch_add(1, Ordering::AcqRel) + 1,
            total: self.total[index].fetch_add(1, Ordering::AcqRel) + 1,
        }
    }

    /// Records that a previously detached worker finished after all, releasing what it held.
    ///
    /// Only `live` falls. The detach still happened, and the history of it is the point.
    pub fn rejoined(&self, kind: DetachKind) {
        let index = kind.index();
        self.live[index].fetch_sub(1, Ordering::AcqRel);
    }

    pub fn count(&self, kind: DetachKind) -> DetachCount {
        let index = kind.index();
        DetachCount {
            live: self.live[index].load(Ordering::Acquire),
            total: self.total[index].load(Ordering::Acquire),
        }
    }

    /// The kinds that have detached at least once, in declaration order.
    ///
    /// A kind that never detached is absent rather than reported as zero, so a summary that exists
    /// at all is one describing something that happened.
    pub fn summary(&self) -> DetachSummary {
        DetachSummary {
            entries: DetachKind::ALL
                .into_iter()
                .map(|kind| (kind, self.count(kind)))
                .filter(|(_, count)| count.total > 0)
                .collect(),
        }
    }
}

/// The detaches worth telling somebody about.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DetachSummary {
    entries: Vec<(DetachKind, DetachCount)>,
}

impl DetachSummary {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[(DetachKind, DetachCount)] {
        &self.entries
    }

    /// A one-line report, phrased so the two numbers cannot be mistaken for each other.
    pub fn to_line(&self) -> String {
        let detail = self
            .entries
            .iter()
            .map(|(kind, count)| {
                format!(
                    "{} {} detached, {} still running (ceiling {})",
                    count.total,
                    kind.label(),
                    count.live,
                    kind.ceiling()
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!("detached native workers: {detail}")
    }
}

static PROCESS_LEDGER: DetachLedger = DetachLedger::new();

/// The ledger every detach in this process is recorded against.
pub fn process_detach_ledger() -> &'static DetachLedger {
    &PROCESS_LEDGER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detached_worker_that_returns_leaves_its_history_behind() {
        let ledger = DetachLedger::new();

        let after_detach = ledger.detached(DetachKind::WindowRender);
        ledger.rejoined(DetachKind::WindowRender);

        // The gauge that existed before this ledger only ever reported `live`, so a render that
        // detached and finished a moment later was indistinguishable from one that never detached.
        assert_eq!(after_detach, DetachCount { live: 1, total: 1 });
        assert_eq!(
            ledger.count(DetachKind::WindowRender),
            DetachCount { live: 0, total: 1 }
        );
    }

    #[test]
    fn kinds_are_counted_apart_from_one_another() {
        let ledger = DetachLedger::new();

        ledger.detached(DetachKind::WindowRender);
        ledger.detached(DetachKind::WindowRender);
        ledger.detached(DetachKind::CaptureWorker);

        assert_eq!(
            ledger.count(DetachKind::WindowRender),
            DetachCount { live: 2, total: 2 }
        );
        assert_eq!(
            ledger.count(DetachKind::CaptureWorker),
            DetachCount { live: 1, total: 1 }
        );
    }

    #[test]
    fn the_ceiling_is_reached_by_live_workers_rather_than_by_history() {
        let ledger = DetachLedger::new();

        for _ in 0..DetachKind::WindowRender.ceiling() {
            ledger.detached(DetachKind::WindowRender);
        }
        assert!(ledger
            .count(DetachKind::WindowRender)
            .at_ceiling(DetachKind::WindowRender));

        // One of the wedged renders comes back. Room is available again even though the history
        // still records every detach that got us here - which is the distinction that matters,
        // because it is the running worker that holds the slot, not the memory of it.
        ledger.rejoined(DetachKind::WindowRender);

        let count = ledger.count(DetachKind::WindowRender);
        assert!(!count.at_ceiling(DetachKind::WindowRender));
        assert_eq!(count.total, DetachKind::WindowRender.ceiling());
    }

    #[test]
    fn a_kind_that_never_detached_is_absent_from_the_summary() {
        let ledger = DetachLedger::new();
        assert!(ledger.summary().is_empty());

        ledger.detached(DetachKind::CaptureWorker);

        let summary = ledger.summary();
        assert_eq!(
            summary.entries(),
            [(DetachKind::CaptureWorker, DetachCount { live: 1, total: 1 })]
        );
        assert_eq!(
            summary.to_line(),
            "detached native workers: 1 capture worker detached, 1 still running (ceiling 1)"
        );
    }
}
