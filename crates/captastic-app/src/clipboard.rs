use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(test)]
use captastic_core::CaptureErrorKind;
use captastic_core::{validate_event_order, CaptureError, CaptureId, PerfEventKind};
use serde_json::json;

use crate::error::AppError;

// A terminal publish can consume three 250 ms backoffs plus native clipboard open waits.
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_STOP_POLL: Duration = Duration::from_millis(5);
const WORKER_RECEIVE_POLL: Duration = Duration::from_millis(50);
const PUBLISH_RETRY_LIMIT: u32 = 3;
const PUBLISH_RETRY_DELAY: Duration = Duration::from_millis(250);

pub struct ClipboardWorker {
    sender: Option<mpsc::SyncSender<ClipboardJob>>,
    failure_receiver: mpsc::Receiver<ClipboardFailure>,
    stop_requested: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ClipboardWorker {
    pub fn start(json_output: bool, queue_capacity: usize) -> Result<Self, AppError> {
        let (sender, receiver) = mpsc::sync_channel::<ClipboardJob>(queue_capacity);
        let (failure_sender, failure_receiver) = mpsc::sync_channel(queue_capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_stop_requested = stop_requested.clone();
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
                loop {
                    if worker_stop_requested.load(Ordering::Acquire) {
                        break;
                    }
                    let mut job = match receiver.recv_timeout(WORKER_RECEIVE_POLL) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if worker_stop_requested.load(Ordering::Acquire) {
                                break;
                            }
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    job.recorder.record(
                        job.capture_id,
                        PerfEventKind::ClipboardStarted,
                        offset_after_cpu(&job),
                    );
                    // Encode once, outside the retry loop: the bytes are identical on every
                    // attempt, and re-encoding a 4K frame per retry was the expensive half of a
                    // failing publish.
                    let (publish_result, publish_retries) = match captastic_windows::ClipboardPayload::prepare(&job.frame) {
                        Ok(payload) => publish_with_retry(
                            || publisher.publish(&payload),
                            thread::sleep,
                            || worker_stop_requested.load(Ordering::Acquire),
                        ),
                        Err(error) => (
                            Err(captastic_windows::ClipboardPublishError {
                                error,
                                cleared_previous_contents: false,
                            }),
                            0,
                        ),
                    };
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
                                message: error.error.to_string(),
                                cleared_previous_contents: error.cleared_previous_contents,
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
                                        "error": error.error.to_string(),
                                        "native_code": error.error.native_code,
                                        "retryable": error.error.retryable,
                                        "publish_retries": publish_retries,
                                    })
                                );
                            }
                        }
                    }
                    if worker_stop_requested.load(Ordering::Acquire) {
                        break;
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
            stop_requested,
            join: Some(join),
        })
    }

    /// The clipboard as a destination, addressable without knowing it is the clipboard.
    pub fn sink(&self) -> crate::output::ChannelSink {
        crate::output::ChannelSink::new(
            "clipboard",
            self.sender
                .as_ref()
                .expect("clipboard worker is running")
                .clone(),
        )
    }

    pub fn try_recv_failure(&self) -> Option<ClipboardFailure> {
        self.failure_receiver.try_recv().ok()
    }

    #[cfg(test)]
    pub fn stop(mut self) -> Vec<ClipboardFailure> {
        self.stop_inner(Instant::now() + WORKER_STOP_TIMEOUT);
        self.failure_receiver.try_iter().collect()
    }

    pub fn stop_before(mut self, deadline: Instant) -> Vec<ClipboardFailure> {
        self.request_stop();
        self.stop_inner(deadline);
        self.failure_receiver.try_iter().collect()
    }

    pub fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        self.sender.take();
    }

    fn stop_inner(&mut self, deadline: Instant) {
        self.request_stop();
        if let Some(join) = self.join.take() {
            while !join.is_finished() && Instant::now() < deadline {
                thread::sleep(WORKER_STOP_POLL);
            }
            if join.is_finished() {
                let _ = join.join();
            } else {
                crate::logging::error(format_args!(
                    "clipboard worker did not stop before its shutdown deadline; detaching it so shutdown can continue"
                ));
            }
        }
    }
}

pub struct ClipboardFailure {
    pub capture_id: CaptureId,
    pub message: String,
    /// True when the failed publish had already emptied the clipboard, so the user lost whatever
    /// they had copied as well as the capture they asked for.
    pub cleared_previous_contents: bool,
}

impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        self.stop_inner(Instant::now() + WORKER_STOP_TIMEOUT);
    }
}

/// The clipboard is one destination among what will shortly be several, so it takes the shared
/// job rather than a shape of its own.
pub type ClipboardJob = crate::output::OutputJob;

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

/// Anything a publish can fail with that knows whether it is worth trying again.
trait Retryable {
    fn retryable(&self) -> bool;
}

impl Retryable for CaptureError {
    fn retryable(&self) -> bool {
        self.retryable
    }
}

impl Retryable for captastic_windows::ClipboardPublishError {
    fn retryable(&self) -> bool {
        self.error.retryable
    }
}

fn publish_with_retry<T, E: Retryable>(
    mut publish: impl FnMut() -> Result<T, E>,
    mut wait: impl FnMut(Duration),
    mut stop_requested: impl FnMut() -> bool,
) -> (Result<T, E>, u32) {
    let mut retries = 0_u32;
    loop {
        match publish() {
            Err(error)
                if error.retryable() && retries < PUBLISH_RETRY_LIMIT && !stop_requested() =>
            {
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
            || false,
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
            || false,
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
            || false,
        );

        assert_eq!(result.expect("eventual success"), "published");
        assert_eq!(attempts, 3);
        assert_eq!(retries, 2);
    }

    #[test]
    fn retry_loop_stops_after_shutdown_is_requested() {
        let mut attempts = 0_u32;
        let (result, retries) = publish_with_retry(
            || {
                attempts = attempts.saturating_add(1);
                Err::<(), _>(publish_error(true))
            },
            |_| panic!("shutdown must prevent another retry delay"),
            || true,
        );

        assert!(result.is_err());
        assert_eq!(attempts, 1);
        assert_eq!(retries, 0);
    }

    #[test]
    fn a_publish_that_cleared_the_clipboard_is_retried_and_reported_as_such() {
        // Win32 requires emptying the clipboard to take ownership of it, so a publish that fails
        // afterwards leaves the user with neither their capture nor what they had copied. The
        // distinction has to survive the retry loop, because it is what the tray apologises for.
        let mut attempts = 0_u32;
        let (result, retries) = publish_with_retry(
            || {
                attempts = attempts.saturating_add(1);
                Err::<(), _>(captastic_windows::ClipboardPublishError {
                    error: publish_error(true),
                    cleared_previous_contents: true,
                })
            },
            |_| {},
            || false,
        );

        assert_eq!(retries, PUBLISH_RETRY_LIMIT);
        assert_eq!(attempts, PUBLISH_RETRY_LIMIT + 1);
        let failure = result.expect_err("every attempt failed");
        assert!(failure.cleared_previous_contents);
        assert!(failure
            .to_string()
            .contains("previous clipboard contents were cleared"));
    }

    #[test]
    fn a_publish_that_failed_before_emptying_reports_no_loss() {
        let (result, _) = publish_with_retry(
            || {
                Err::<(), _>(captastic_windows::ClipboardPublishError {
                    error: publish_error(false),
                    cleared_previous_contents: false,
                })
            },
            |_| {},
            || false,
        );

        let failure = result.expect_err("the attempt failed");
        assert!(!failure.cleared_previous_contents);
        assert!(
            !failure.to_string().contains("cleared"),
            "a publish that never emptied the clipboard must not claim it did: {failure}"
        );
    }

    #[test]
    fn stop_returns_failures_queued_during_worker_teardown() {
        let (failure_sender, failure_receiver) = mpsc::sync_channel(1);
        failure_sender
            .send(ClipboardFailure {
                capture_id: CaptureId(9),
                message: "scripted shutdown failure".to_owned(),
                cleared_previous_contents: false,
            })
            .expect("queue teardown failure");
        let worker = ClipboardWorker {
            sender: None,
            failure_receiver,
            stop_requested: Arc::new(AtomicBool::new(false)),
            join: None,
        };

        let failures = worker.stop();

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].capture_id, CaptureId(9));
        assert_eq!(failures[0].message, "scripted shutdown failure");
    }
}
