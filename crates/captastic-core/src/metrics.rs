use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::{CaptureId, MetricsError};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfEventKind {
    HotkeyReceived,
    TriggerEnqueued,
    TriggerDequeued,
    CaptureRequested,
    NativeFrameReady,
    ReadbackStarted,
    CpuFrameReady,
    SelectionStarted,
    SelectionConfirmed,
    CropFinished,
    ClipboardStarted,
    ClipboardCommitted,
    EncodeStarted,
    EncodeFinished,
    FileWriteStarted,
    FileWriteFinished,
    AttemptFinished,
}

impl PerfEventKind {
    fn rank(self, order: CaptureOrder) -> u8 {
        match (order, self) {
            (CaptureOrder::SelectionFirst, Self::HotkeyReceived) => 0,
            (CaptureOrder::SelectionFirst, Self::TriggerEnqueued) => 1,
            (CaptureOrder::SelectionFirst, Self::TriggerDequeued) => 2,
            (CaptureOrder::SelectionFirst, Self::SelectionStarted) => 3,
            (CaptureOrder::SelectionFirst, Self::SelectionConfirmed) => 4,
            (CaptureOrder::SelectionFirst, Self::CaptureRequested) => 5,
            (CaptureOrder::SelectionFirst, Self::NativeFrameReady) => 6,
            (CaptureOrder::SelectionFirst, Self::ReadbackStarted) => 7,
            (CaptureOrder::SelectionFirst, Self::CpuFrameReady) => 8,
            (CaptureOrder::SelectionFirst, Self::CropFinished) => 9,
            (CaptureOrder::SelectionFirst, Self::ClipboardStarted) => 10,
            (CaptureOrder::SelectionFirst, Self::ClipboardCommitted) => 11,
            (CaptureOrder::SelectionFirst, Self::EncodeStarted) => 12,
            (CaptureOrder::SelectionFirst, Self::EncodeFinished) => 13,
            (CaptureOrder::SelectionFirst, Self::FileWriteStarted) => 14,
            (CaptureOrder::SelectionFirst, Self::FileWriteFinished) => 15,
            (CaptureOrder::SelectionFirst, Self::AttemptFinished) => 16,
            (CaptureOrder::CaptureFirst, Self::HotkeyReceived) => 0,
            (CaptureOrder::CaptureFirst, Self::TriggerEnqueued) => 1,
            (CaptureOrder::CaptureFirst, Self::TriggerDequeued) => 2,
            (CaptureOrder::CaptureFirst, Self::CaptureRequested) => 3,
            (CaptureOrder::CaptureFirst, Self::NativeFrameReady) => 4,
            (CaptureOrder::CaptureFirst, Self::ReadbackStarted) => 5,
            (CaptureOrder::CaptureFirst, Self::CpuFrameReady) => 6,
            (CaptureOrder::CaptureFirst, Self::SelectionStarted) => 7,
            (CaptureOrder::CaptureFirst, Self::SelectionConfirmed) => 8,
            (CaptureOrder::CaptureFirst, Self::CropFinished) => 9,
            (CaptureOrder::CaptureFirst, Self::ClipboardStarted) => 10,
            (CaptureOrder::CaptureFirst, Self::ClipboardCommitted) => 11,
            (CaptureOrder::CaptureFirst, Self::EncodeStarted) => 12,
            (CaptureOrder::CaptureFirst, Self::EncodeFinished) => 13,
            (CaptureOrder::CaptureFirst, Self::FileWriteStarted) => 14,
            (CaptureOrder::CaptureFirst, Self::FileWriteFinished) => 15,
            (CaptureOrder::CaptureFirst, Self::AttemptFinished) => 16,
        }
    }

    fn establishes_order(self) -> Option<CaptureOrder> {
        match self {
            Self::CaptureRequested => Some(CaptureOrder::CaptureFirst),
            Self::SelectionStarted => Some(CaptureOrder::SelectionFirst),
            _ => None,
        }
    }

    fn is_output(self) -> bool {
        matches!(
            self,
            Self::ClipboardStarted
                | Self::ClipboardCommitted
                | Self::EncodeStarted
                | Self::EncodeFinished
                | Self::FileWriteStarted
                | Self::FileWriteFinished
        )
    }

    fn label(self) -> &'static str {
        match self {
            Self::HotkeyReceived => "hotkey_received",
            Self::TriggerEnqueued => "trigger_enqueued",
            Self::TriggerDequeued => "trigger_dequeued",
            Self::CaptureRequested => "capture_requested",
            Self::NativeFrameReady => "native_frame_ready",
            Self::ReadbackStarted => "readback_started",
            Self::CpuFrameReady => "cpu_frame_ready",
            Self::SelectionStarted => "selection_started",
            Self::SelectionConfirmed => "selection_confirmed",
            Self::CropFinished => "crop_finished",
            Self::ClipboardStarted => "clipboard_started",
            Self::ClipboardCommitted => "clipboard_committed",
            Self::EncodeStarted => "encode_started",
            Self::EncodeFinished => "encode_finished",
            Self::FileWriteStarted => "file_write_started",
            Self::FileWriteFinished => "file_write_finished",
            Self::AttemptFinished => "attempt_finished",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureOrder {
    CaptureFirst,
    SelectionFirst,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PerfEvent {
    pub capture_id: CaptureId,
    pub kind: PerfEventKind,
    pub ticks_ns: u64,
    pub value: u64,
}

#[derive(Debug)]
pub struct EventRecorder {
    origin: Instant,
    events: Vec<PerfEvent>,
    capacity: usize,
    lost_events: u64,
}

impl EventRecorder {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            origin: Instant::now(),
            events: Vec::with_capacity(capacity),
            capacity,
            lost_events: 0,
        }
    }

    pub fn record(&mut self, capture_id: CaptureId, kind: PerfEventKind, value: u64) {
        let ticks_ns = nanos_u64(self.origin.elapsed().as_nanos());
        if self.events.len() == self.capacity {
            self.lost_events = self.lost_events.saturating_add(1);
            return;
        }
        self.events.push(PerfEvent {
            capture_id,
            kind,
            ticks_ns,
            value,
        });
    }

    pub fn events(&self) -> &[PerfEvent] {
        &self.events
    }

    pub fn lost_events(&self) -> u64 {
        self.lost_events
    }

    pub fn into_events(self) -> Vec<PerfEvent> {
        self.events
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LatencySummary {
    pub count: usize,
    pub min_ns: u64,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
    pub mean_ns: u64,
}

impl LatencySummary {
    pub fn from_samples(samples: &[u64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let sum = sorted
            .iter()
            .fold(0_u128, |acc, value| acc.saturating_add(u128::from(*value)));
        Self {
            count: sorted.len(),
            min_ns: sorted[0],
            p50_ns: percentile(&sorted, 50),
            p90_ns: percentile(&sorted, 90),
            p95_ns: percentile(&sorted, 95),
            p99_ns: percentile(&sorted, 99),
            max_ns: *sorted.last().unwrap_or(&0),
            mean_ns: (sum / sorted.len() as u128) as u64,
        }
    }
}

pub fn validate_event_order(events: &[PerfEvent]) -> Result<(), MetricsError> {
    let mut state: HashMap<CaptureId, (u8, bool, Option<CaptureOrder>)> = HashMap::new();
    for event in events {
        let entry = state.entry(event.capture_id).or_insert((0, false, None));
        if event.kind.is_output() && !entry.1 {
            return Err(MetricsError::OutputBeforeCpuFrame {
                capture_id: event.capture_id.0,
                current: event.kind.label(),
            });
        }
        if entry.2.is_none() {
            entry.2 = event.kind.establishes_order();
        }
        let rank = event
            .kind
            .rank(entry.2.unwrap_or(CaptureOrder::CaptureFirst));
        if rank < entry.0 {
            return Err(MetricsError::EventOrderRegression {
                capture_id: event.capture_id.0,
                previous_rank: entry.0,
                current_rank: rank,
            });
        }
        if event.kind == PerfEventKind::CpuFrameReady {
            entry.1 = true;
        }
        entry.0 = rank;
    }
    Ok(())
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    let numerator = percentile.saturating_mul(sorted.len()).saturating_add(99);
    let rank = numerator / 100;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

pub(crate) fn nanos_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: u64, kind: PerfEventKind, ticks_ns: u64) -> PerfEvent {
        PerfEvent {
            capture_id: CaptureId(id),
            kind,
            ticks_ns,
            value: 0,
        }
    }

    #[test]
    fn calculates_nearest_rank_percentiles() {
        let summary = LatencySummary::from_samples(&[10, 20, 30, 40, 50]);
        assert_eq!(summary.p50_ns, 30);
        assert_eq!(summary.p95_ns, 50);
        assert_eq!(summary.mean_ns, 30);
    }

    #[test]
    fn rejects_encoding_before_cpu_frame() {
        let events = [
            event(7, PerfEventKind::HotkeyReceived, 0),
            event(7, PerfEventKind::NativeFrameReady, 10),
            event(7, PerfEventKind::EncodeStarted, 11),
        ];
        assert_eq!(
            validate_event_order(&events),
            Err(MetricsError::OutputBeforeCpuFrame {
                capture_id: 7,
                current: "encode_started",
            })
        );
    }

    #[test]
    fn accepts_output_after_cpu_frame() {
        let events = [
            event(7, PerfEventKind::HotkeyReceived, 0),
            event(7, PerfEventKind::CaptureRequested, 1),
            event(7, PerfEventKind::NativeFrameReady, 2),
            event(7, PerfEventKind::CpuFrameReady, 3),
            event(7, PerfEventKind::EncodeStarted, 4),
            event(7, PerfEventKind::EncodeFinished, 5),
            event(7, PerfEventKind::AttemptFinished, 6),
        ];
        assert_eq!(validate_event_order(&events), Ok(()));
    }

    #[test]
    fn accepts_confirmation_anchored_capture_after_live_selection() {
        let events = [
            event(7, PerfEventKind::HotkeyReceived, 0),
            event(7, PerfEventKind::TriggerEnqueued, 1),
            event(7, PerfEventKind::TriggerDequeued, 2),
            event(7, PerfEventKind::SelectionStarted, 3),
            event(7, PerfEventKind::SelectionConfirmed, 4),
            event(7, PerfEventKind::CaptureRequested, 5),
            event(7, PerfEventKind::NativeFrameReady, 6),
            event(7, PerfEventKind::ReadbackStarted, 7),
            event(7, PerfEventKind::CpuFrameReady, 8),
            event(7, PerfEventKind::CropFinished, 9),
            event(7, PerfEventKind::ClipboardStarted, 10),
            event(7, PerfEventKind::ClipboardCommitted, 11),
            event(7, PerfEventKind::AttemptFinished, 12),
        ];

        assert_eq!(validate_event_order(&events), Ok(()));
    }

    #[test]
    fn rejects_returning_to_selection_after_live_capture_starts() {
        let events = [
            event(7, PerfEventKind::SelectionStarted, 0),
            event(7, PerfEventKind::SelectionConfirmed, 1),
            event(7, PerfEventKind::CaptureRequested, 2),
            event(7, PerfEventKind::SelectionConfirmed, 3),
        ];

        assert!(matches!(
            validate_event_order(&events),
            Err(MetricsError::EventOrderRegression { .. })
        ));
    }
}
