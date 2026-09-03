//! The seam between a finished capture and wherever it is delivered.
//!
//! Captastic has one destination today and gains a second in Milestone 4, whose exit criteria
//! include "clipboard success remains independent of file-output failure and vice versa". That
//! independence is a property of how destinations are addressed, not something to be remembered
//! at each call site: as long as producers hold a concrete clipboard submitter, adding file output
//! means threading a second one through every path that publishes, and every one of those is a
//! chance to let one destination's rejection escape as the other's failure.
//!
//! So producers address an [`OutputSink`]. A sink accepts a job or rejects it, and a rejection is
//! a fact about that destination alone.

use std::sync::mpsc;

use captastic_config::{HotkeyAction, HotkeyChord};
use captastic_core::{CaptureId, CpuFrame, EventRecorder};
use std::time::Instant;

/// A capture, complete and owned, on its way to a destination.
///
/// Deliberately owned rather than borrowed: a sink runs on its own thread and must not extend the
/// life of anything on the capture path.
#[derive(Debug)]
pub struct OutputJob {
    pub capture_id: CaptureId,
    pub triggered_at: Instant,
    pub action: HotkeyAction,
    pub chord: Option<HotkeyChord>,
    pub cpu_ready_offset_ns: u64,
    pub source: &'static str,
    pub frame: CpuFrame,
    pub recorder: EventRecorder,
    /// The captured window's title, when the capture was of a window. Untrusted text: whichever
    /// application owns the window chose it, so every consumer sanitizes before use.
    pub window_title: Option<String>,
    /// The file stem of the executable owning the captured window, where it could be read.
    pub window_application: Option<String>,
}

/// Why a sink would not take a job, with the job handed back so the caller can account for it.
///
/// Both variants leave the capture itself valid. A destination that cannot keep up, or has already
/// stopped, is not a failed capture — which is the distinction the M4 exit criterion turns on.
#[derive(Debug)]
pub enum OutputRejection {
    /// The destination's queue is full. The capture was not delivered there.
    QueueFull(Box<OutputJob>),
    /// The destination's worker has stopped.
    Disconnected(Box<OutputJob>),
}

impl OutputRejection {
    /// The reason, as it appears in structured output and logs.
    pub fn status(&self) -> &'static str {
        match self {
            Self::QueueFull(_) => "queue_full",
            Self::Disconnected(_) => "worker_disconnected",
        }
    }

    pub fn into_job(self) -> Box<OutputJob> {
        match self {
            Self::QueueFull(job) | Self::Disconnected(job) => job,
        }
    }
}

/// Somewhere a finished capture can be delivered.
///
/// Implemented today by the clipboard worker's submitter; Milestone 4's file worker is the second
/// implementation, and the point of the trait is that adding it changes no producer.
pub trait OutputSink: Send + Sync {
    /// Names the destination for logs and structured output.
    fn name(&self) -> &'static str;

    /// Hands the job over without blocking, or gives it back.
    ///
    /// Non-blocking on purpose: this is called from the capture path and from the selection
    /// worker, and neither may be made to wait on a destination that has fallen behind.
    fn submit(&self, job: OutputJob) -> Result<(), OutputRejection>;
}

/// A sink backed by a bounded channel to a worker thread.
///
/// The whole of the current clipboard delivery mechanism, and what the file worker will reuse.
#[derive(Clone)]
pub struct ChannelSink {
    name: &'static str,
    sender: mpsc::SyncSender<OutputJob>,
}

impl ChannelSink {
    pub fn new(name: &'static str, sender: mpsc::SyncSender<OutputJob>) -> Self {
        Self { name, sender }
    }
}

impl OutputSink for ChannelSink {
    fn name(&self) -> &'static str {
        self.name
    }

    fn submit(&self, job: OutputJob) -> Result<(), OutputRejection> {
        self.sender.try_send(job).map_err(|error| match error {
            mpsc::TrySendError::Full(job) => OutputRejection::QueueFull(Box::new(job)),
            mpsc::TrySendError::Disconnected(job) => OutputRejection::Disconnected(Box::new(job)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use captastic_core::{
        CaptureMode, ColorSpace, DisplayId, FrameMetadata, FrameOrigin, PixelFormat, Rect,
        TimingProvenance,
    };
    use std::sync::Arc;

    fn job() -> OutputJob {
        let metadata = FrameMetadata {
            capture_id: CaptureId(1),
            backend: "test".to_owned(),
            display_id: DisplayId::primary(),
            source_rect: Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
            rotation_degrees: 0,
            capture_mode: CaptureMode::Latest { max_age_ms: None },
            presentation_offset_ns: None,
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: 0,
            cpu_ready_offset_ns: Some(0),
            frame_age_ns: Some(0),
            verified_current_offset_ns: None,
            frame_generation: Some(1),
            copy_count: 0,
            pool_slot: None,
            cursor: None,
        };
        OutputJob {
            capture_id: CaptureId(1),
            triggered_at: Instant::now(),
            action: HotkeyAction::LastWorkflow,
            chord: None,
            cpu_ready_offset_ns: 0,
            source: "test",
            frame: captastic_core::CpuFrame::new(
                Arc::from(vec![0_u8; 16]),
                2,
                2,
                8,
                PixelFormat::Bgra8Unorm,
                FrameOrigin::TopLeft,
                ColorSpace::Srgb,
                metadata,
            )
            .expect("test frame"),
            recorder: EventRecorder::with_capacity(4),
            window_title: None,
            window_application: None,
        }
    }

    #[test]
    fn a_full_sink_hands_the_job_back_rather_than_dropping_it() {
        // The capture stays valid when a destination cannot take it, so the job has to come back
        // for its metrics to be finished rather than vanishing with the rejection.
        let (sender, _receiver) = mpsc::sync_channel(1);
        let sink = ChannelSink::new("test", sender);

        sink.submit(job()).expect("the first job fits");
        let rejection = sink.submit(job()).expect_err("the second does not");

        assert_eq!(rejection.status(), "queue_full");
        assert_eq!(rejection.into_job().capture_id, CaptureId(1));
    }

    #[test]
    fn a_stopped_sink_reports_disconnection_distinctly() {
        // A destination that has stopped is a different operational fact from one that is merely
        // behind, and the logs say so differently.
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let sink = ChannelSink::new("test", sender);

        let rejection = sink.submit(job()).expect_err("nobody is listening");

        assert_eq!(rejection.status(), "worker_disconnected");
    }

    #[test]
    fn a_sink_that_accepts_reports_nothing() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let sink = ChannelSink::new("clipboard", sender);

        assert!(sink.submit(job()).is_ok());
        assert_eq!(sink.name(), "clipboard");
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn producers_can_hold_sinks_without_knowing_the_destination() {
        // The property the seam exists for: a producer holds `dyn OutputSink`, so Milestone 4's
        // file worker is a second element in this list rather than a second parameter everywhere.
        let (clipboard_sender, clipboard) = mpsc::sync_channel(1);
        let (file_sender, file) = mpsc::sync_channel(1);
        let sinks: Vec<Box<dyn OutputSink>> = vec![
            Box::new(ChannelSink::new("clipboard", clipboard_sender)),
            Box::new(ChannelSink::new("file", file_sender)),
        ];

        for sink in &sinks {
            assert!(sink.submit(job()).is_ok(), "{} rejected", sink.name());
        }
        assert!(clipboard.try_recv().is_ok());
        assert!(file.try_recv().is_ok());
    }

    #[test]
    fn a_job_carries_window_identity_for_naming() {
        // The filename template's `{title}` and `{application}` come from here. Both are optional
        // because a direct capture has no window and an elevated process will not name itself.
        let (sender, receiver) = mpsc::sync_channel(1);
        let sink = ChannelSink::new("file", sender);

        let mut job = job();
        job.window_title = Some("Some Document - Editor".to_owned());
        job.window_application = Some("editor".to_owned());
        sink.submit(job).expect("accepted");

        let delivered = receiver.try_recv().expect("delivered");
        assert_eq!(
            delivered.window_title.as_deref(),
            Some("Some Document - Editor")
        );
        assert_eq!(delivered.window_application.as_deref(), Some("editor"));
    }

    #[test]
    fn one_full_destination_does_not_affect_another() {
        // The Milestone 4 exit criterion, stated as a test: clipboard success is independent of
        // file-output failure and vice versa.
        let (clipboard_sender, clipboard) = mpsc::sync_channel(1);
        let (file_sender, _file) = mpsc::sync_channel(1);
        let clipboard_sink = ChannelSink::new("clipboard", clipboard_sender);
        let file_sink = ChannelSink::new("file", file_sender);

        // Fill the file sink so it can take no more.
        file_sink.submit(job()).expect("first file job fits");
        assert!(file_sink.submit(job()).is_err());

        // The clipboard is untouched by that.
        assert!(clipboard_sink.submit(job()).is_ok());
        assert!(clipboard.try_recv().is_ok());
    }
}
