use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use captastic_config::{HotkeyAction, HotkeyChord};
#[cfg(test)]
use captastic_core::CaptureErrorKind;
use captastic_core::{
    validate_event_order, CaptureError, CaptureId, CpuFrame, EventRecorder, PerfEventKind,
};
use serde_json::json;

use crate::error::AppError;

// A terminal publish can consume three 250 ms backoffs plus native clipboard open waits.
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_STOP_POLL: Duration = Duration::from_millis(5);
const PUBLISH_RETRY_LIMIT: u32 = 3;
const PUBLISH_RETRY_DELAY: Duration = Duration::from_millis(250);

pub struct ClipboardWorker {
    sender: Option<mpsc::SyncSender<ClipboardJob>>,
    failure_receiver: mpsc::Receiver<ClipboardFailure>,
    join: Option<JoinHandle<()>>,
}

impl ClipboardWorker {
    pub fn start(json_output: bool, queue_capacity: usize) -> Result<Self, AppError> {
        let (sender, receiver) = mpsc::sync_channel::<ClipboardJob>(queue_capacity);
        let (failure_sender, failure_receiver) = mpsc::sync_channel(queue_capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("captastic-clipboard".to_owned())
            .spawn(move || {
                let mut publisher = match captastic_windows::ClipboardPublisher::new() {
                    Ok(publisher) => publisher,
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                if ready_sender.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(mut job) = receiver.recv() {
                    job.recorder.record(
                        job.capture_id,
                        PerfEventKind::ClipboardStarted,
                        offset_after_cpu(&job),
                    );
                    let (publish_result, publish_retries) = publish_with_retry(
                        || publisher.publish(&job.frame),
                        thread::sleep,
                    );
                    match publish_result {
                        Ok(report) => {
                            let total_offset_ns = duration_ns(job.triggered_at.elapsed());
                            let cpu_to_clipboard_ns =
                                total_offset_ns.saturating_sub(job.cpu_ready_offset_ns);
                            job.recorder.record(
                                job.capture_id,
                                PerfEventKind::ClipboardCommitted,
                                cpu_to_clipboard_ns,
                            );
                            job.recorder.record(
                                job.capture_id,
                                PerfEventKind::AttemptFinished,
                                total_offset_ns,
                            );
                            log::info!(
                                "clipboard {}: committed {:.3} ms after CPU ({:.3} ms total, open_retries={} publish_retries={}) payload_bytes={} png_bytes={} publish_ns={}",
                                job.capture_id.0,
                                ns_to_ms(cpu_to_clipboard_ns),
                                ns_to_ms(total_offset_ns),
                                report.open_retries,
                                publish_retries,
                                report.payload_bytes,
                                report.png_payload_bytes,
                                report.publish_ns
                            );
                            if let Err(error) = validate_event_order(job.recorder.events()) {
                                crate::logging::error(format_args!(
                                    "clipboard capture {} metrics failed validation: {error}",
                                    job.capture_id.0
                                ));
                                continue;
                            }
                            if json_output {
                                println!(
                                    "{}",
                                    json!({
                                        "schema_version": 1,
                                        "event": "clipboard_complete",
                                        "action": job.action,
                                        "chord": job.chord.map(|chord| chord.to_string()),
                                        "capture_id": job.capture_id,
                                        "source": job.source,
                                        "total_offset_ns": total_offset_ns,
                                        "cpu_to_clipboard_ns": cpu_to_clipboard_ns,
                                        "payload_bytes": report.payload_bytes,
                                        "png_payload_bytes": report.png_payload_bytes,
                                        "png_encode_ns": report.png_encode_ns,
                                        "allocation_copy_ns": report.allocation_copy_ns,
                                        "open_wait_ns": report.open_wait_ns,
                                        "open_retries": report.open_retries,
                                        "publish_retries": publish_retries,
                                        "publish_ns": report.publish_ns,
                                    })
                                );
                            }
                        }
                        Err(error) => {
                            job.recorder.record(
                                job.capture_id,
                                PerfEventKind::AttemptFinished,
                                duration_ns(job.triggered_at.elapsed()),
                            );
                            if let Err(metrics_error) = validate_event_order(job.recorder.events()) {
                                crate::logging::error(format_args!(
                                    "clipboard capture {} metrics failed validation: {metrics_error}",
                                    job.capture_id.0
                                ));
                            }
                            crate::logging::error(format_args!(
                                "clipboard {} failed without invalidating capture: {error}",
                                job.capture_id.0
                            ));
                            let _ = failure_sender.try_send(ClipboardFailure {
                                capture_id: job.capture_id,
                                message: error.to_string(),
                            });
                            if json_output {
                                println!(
                                    "{}",
                                    json!({
                                        "schema_version": 1,
                                        "event": "clipboard_failed",
                                        "capture_id": job.capture_id,
                                        "source": job.source,
                                        "action": job.action,
                                        "error": error.to_string(),
                                        "native_code": error.native_code,
                                        "retryable": error.retryable,
                                        "publish_retries": publish_retries,
                                    })
                                );
                            }
                        }
                    }
                }
            })
            .map_err(|error| AppError::BackendUnavailable(error.to_string()))?;
        ready_receiver.recv().map_err(|error| {
            AppError::BackendUnavailable(format!("clipboard worker ended during startup: {error}"))
        })??;
        Ok(Self {
            sender: Some(sender),
            failure_receiver,
            join: Some(join),
        })
    }

    pub fn submitter(&self) -> mpsc::SyncSender<ClipboardJob> {
        self.sender
            .as_ref()
            .expect("clipboard worker is running")
            .clone()
    }

    pub fn try_recv_failure(&self) -> Option<ClipboardFailure> {
        self.failure_receiver.try_recv().ok()
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let started = Instant::now();
            while !join.is_finished() && started.elapsed() < WORKER_STOP_TIMEOUT {
                thread::sleep(WORKER_STOP_POLL);
            }
            if join.is_finished() {
                let _ = join.join();
            } else {
                crate::logging::error(format_args!(
                    "clipboard worker did not stop within {} ms; detaching it so shutdown can continue",
                    WORKER_STOP_TIMEOUT.as_millis()
                ));
            }
        }
    }
}

pub struct ClipboardFailure {
    pub capture_id: CaptureId,
    pub message: String,
}

impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

pub struct ClipboardJob {
    pub capture_id: CaptureId,
    pub triggered_at: Instant,
    pub action: HotkeyAction,
    pub chord: Option<HotkeyChord>,
    pub cpu_ready_offset_ns: u64,
    pub source: &'static str,
    pub frame: CpuFrame,
    pub recorder: EventRecorder,
}

pub enum SubmitError {
    Full(Box<ClipboardJob>),
    Disconnected(Box<ClipboardJob>),
}

pub fn try_submit(
    sender: &mpsc::SyncSender<ClipboardJob>,
    job: ClipboardJob,
) -> Result<(), SubmitError> {
    sender.try_send(job).map_err(|error| match error {
        mpsc::TrySendError::Full(job) => SubmitError::Full(Box::new(job)),
        mpsc::TrySendError::Disconnected(job) => SubmitError::Disconnected(Box::new(job)),
    })
}

pub fn finish_rejected(mut job: ClipboardJob) -> Result<CaptureId, AppError> {
    job.recorder.record(
        job.capture_id,
        PerfEventKind::AttemptFinished,
        duration_ns(job.triggered_at.elapsed()),
    );
    validate_event_order(job.recorder.events())?;
    Ok(job.capture_id)
}

fn offset_after_cpu(job: &ClipboardJob) -> u64 {
    duration_ns(job.triggered_at.elapsed()).saturating_sub(job.cpu_ready_offset_ns)
}

fn duration_ns(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

fn publish_with_retry<T>(
    mut publish: impl FnMut() -> Result<T, CaptureError>,
    mut wait: impl FnMut(Duration),
) -> (Result<T, CaptureError>, u32) {
    let mut retries = 0_u32;
    loop {
        match publish() {
            Err(error) if error.retryable && retries < PUBLISH_RETRY_LIMIT => {
                retries = retries.saturating_add(1);
                wait(PUBLISH_RETRY_DELAY);
            }
            result => return (result, retries),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish_error(retryable: bool) -> CaptureError {
        CaptureError {
            kind: CaptureErrorKind::NativeFailure,
            backend: "test-clipboard",
            operation: "publish",
            message: "scripted failure".to_owned(),
            retryable,
            native_code: None,
        }
    }

    #[test]
    fn retryable_clipboard_failures_are_retried_to_the_limit() {
        let mut attempts = 0_u32;
        let mut waits = Vec::new();
        let (result, retries) = publish_with_retry(
            || {
                attempts = attempts.saturating_add(1);
                Err::<(), _>(publish_error(true))
            },
            |delay| waits.push(delay),
        );

        assert!(result.is_err());
        assert_eq!(attempts, PUBLISH_RETRY_LIMIT + 1);
        assert_eq!(retries, PUBLISH_RETRY_LIMIT);
        assert_eq!(
            waits,
            vec![PUBLISH_RETRY_DELAY; PUBLISH_RETRY_LIMIT as usize]
        );
    }

    #[test]
    fn non_retryable_clipboard_failure_is_not_retried() {
        let mut attempts = 0_u32;
        let (result, retries) = publish_with_retry(
            || {
                attempts = attempts.saturating_add(1);
                Err::<(), _>(publish_error(false))
            },
            |_| panic!("non-retryable failure must not wait"),
        );

        assert!(result.is_err());
        assert_eq!(attempts, 1);
        assert_eq!(retries, 0);
    }

    #[test]
    fn retry_loop_returns_success_after_transient_failures() {
        let mut attempts = 0_u32;
        let (result, retries) = publish_with_retry(
            || {
                attempts = attempts.saturating_add(1);
                if attempts < 3 {
                    Err(publish_error(true))
                } else {
                    Ok("published")
                }
            },
            |_| {},
        );

        assert_eq!(result.expect("eventual success"), "published");
        assert_eq!(attempts, 3);
        assert_eq!(retries, 2);
    }
}
