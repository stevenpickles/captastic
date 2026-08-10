use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use captastic_core::{
    validate_event_order, CaptureId, CpuFrame, EventRecorder, NativeFrame, PerfEventKind,
};
use serde_json::json;

use crate::error::AppError;

const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(1);
const WORKER_STOP_POLL: Duration = Duration::from_millis(5);

pub struct SelectionWorker {
    sender: Option<mpsc::SyncSender<SelectionJob>>,
    controller: captastic_windows::OverlayController,
    join: Option<JoinHandle<()>>,
}

impl SelectionWorker {
    pub fn start(
        clipboard_sender: mpsc::SyncSender<crate::clipboard::ClipboardJob>,
        json_output: bool,
        queue_capacity: usize,
    ) -> Result<Self, AppError> {
        let (sender, receiver) = mpsc::sync_channel::<SelectionJob>(queue_capacity);
        let controller = captastic_windows::OverlayController::new();
        let worker_controller = controller.clone();
        let join = thread::Builder::new()
            .name("captastic-selection".to_owned())
            .spawn(move || loop {
                let mut job = match receiver.recv_timeout(Duration::from_secs(30)) {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        captastic_windows::clear_overlay_resource_cache();
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                job.recorder.record(
                    job.capture_id,
                    PerfEventKind::SelectionStarted,
                    offset_after_cpu(&job),
                );
                let selection = captastic_windows::select_from_frozen_frame_with_controller(
                    &job.frame,
                    &worker_controller,
                );
                match selection {
                    Ok(Some(selection)) => {
                        let selection_offset_ns = duration_ns(job.triggered_at.elapsed());
                        job.recorder.record(
                            job.capture_id,
                            PerfEventKind::SelectionConfirmed,
                            selection.selection_ns,
                        );
                        let materialize_started = Instant::now();
                        let mut materialization = selection_materialization(selection.kind);
                        let mut gpu_materialization = None;
                        let mut gpu_fallback_error = None;
                        let gpu_result = if selection.kind == captastic_windows::SelectionKind::Region {
                            job.native_frame.as_deref().map(|native_frame| {
                                captastic_windows::materialize_native_region(
                                    native_frame,
                                    selection.rect,
                                )
                            })
                        } else {
                            None
                        };
                        let selected_frame = match gpu_result.transpose() {
                            Ok(Some(Some(result))) => {
                                materialization = "dxgi_gpu_region";
                                gpu_materialization = Some(json!({
                                    "gpu_copy_submit_ns": result.gpu_copy_submit_ns,
                                    "map_wait_ns": result.map_wait_ns,
                                    "cpu_copy_ns": result.cpu_copy_ns,
                                    "total_ns": result.total_ns,
                                    "bytes_read": result.bytes_read,
                                    "full_frame_bytes": result.full_frame_bytes,
                                    "bytes_avoided": result.bytes_avoided,
                                    "contiguous_rows": result.contiguous_rows,
                                }));
                                result.frame
                            }
                            Ok(Some(None)) | Ok(None) => {
                                match captastic_windows::materialize_selection(&job.frame, &selection) {
                                    Ok(frame) => frame,
                                    Err(error) => {
                                        finish_without_clipboard(
                                            &mut job,
                                            json_output,
                                            "selection_failed",
                                            &error.to_string(),
                                        );
                                        continue;
                                    }
                                }
                            }
                            Err(error) => {
                                gpu_fallback_error = Some(error.to_string());
                                crate::logging::warn(format_args!(
                                    "selection {} GPU materialization failed; using CPU crop: {error}",
                                    job.capture_id.0
                                ));
                                match captastic_windows::materialize_selection(&job.frame, &selection) {
                                    Ok(frame) => frame,
                                    Err(error) => {
                                        finish_without_clipboard(
                                            &mut job,
                                            json_output,
                                            "selection_failed",
                                            &error.to_string(),
                                        );
                                        continue;
                                    }
                                }
                            }
                        };
                        let materialize_ns = duration_ns(materialize_started.elapsed());
                        job.recorder.record(
                            job.capture_id,
                            PerfEventKind::CropFinished,
                            materialize_ns,
                        );
                        log::info!(
                            "selection {}: {} {}x{} at ({}, {}), output {:.3} ms materialization={}",
                            job.capture_id.0,
                            selection_kind(selection.kind),
                            selection.rect.width,
                            selection.rect.height,
                            selection.rect.x,
                            selection.rect.y,
                            ns_to_ms(materialize_ns),
                            materialization
                        );
                        if json_output {
                            println!(
                                "{}",
                                json!({
                                    "schema_version": 1,
                                    "event": "selection_complete",
                                    "capture_id": job.capture_id,
                                    "source": job.source,
                                    "kind": selection_kind(selection.kind),
                                    "rect": selection.rect,
                                    "selection_offset_ns": selection_offset_ns,
                                    "selection_interaction_ns": selection.selection_ns,
                                    "overlay_preparation_ns": selection.preparation_ns,
                                        "window_overview_ns": selection.window_overview_ns,
                                        "window_preview_count": selection.window_preview_count,
                                        "window_preview_bytes": selection.window_preview_bytes,
                                    "materialization": materialization,
                                    "materialization_ns": materialize_ns,
                                    "gpu_materialization": gpu_materialization,
                                    "gpu_fallback_error": gpu_fallback_error,
                                    "selected_frame_bytes": selected_frame.required_bytes(),
                                })
                            );
                        }
                        let clipboard_job = crate::clipboard::ClipboardJob {
                            capture_id: job.capture_id,
                            triggered_at: job.triggered_at,
                            cpu_ready_offset_ns: job.cpu_ready_offset_ns,
                            source: job.source,
                            frame: selected_frame,
                            recorder: job.recorder,
                        };
                        match crate::clipboard::try_submit(&clipboard_sender, clipboard_job) {
                            Ok(()) => {}
                            Err(crate::clipboard::SubmitError::Full(job)) => {
                                report_clipboard_rejection(*job, "queue_full", json_output);
                            }
                            Err(crate::clipboard::SubmitError::Disconnected(job)) => {
                                report_clipboard_rejection(
                                    *job,
                                    "worker_disconnected",
                                    json_output,
                                );
                            }
                        }
                    }
                    Ok(None) => {
                        finish_without_clipboard(
                            &mut job,
                            json_output,
                            "selection_cancelled",
                            "selection was cancelled",
                        );
                    }
                    Err(error) => {
                        finish_without_clipboard(
                            &mut job,
                            json_output,
                            "selection_failed",
                            &error.to_string(),
                        );
                    }
                }
            })
            .map_err(|error| AppError::BackendUnavailable(error.to_string()))?;
        Ok(Self {
            sender: Some(sender),
            controller,
            join: Some(join),
        })
    }

    pub fn submitter(&self) -> mpsc::SyncSender<SelectionJob> {
        self.sender
            .as_ref()
            .expect("selection worker is running")
            .clone()
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.sender.take();
        self.controller.cancel();
        if let Some(join) = self.join.take() {
            let started = Instant::now();
            while !join.is_finished() && started.elapsed() < WORKER_STOP_TIMEOUT {
                thread::sleep(WORKER_STOP_POLL);
            }
            if join.is_finished() {
                let _ = join.join();
            } else {
                crate::logging::error(format_args!(
                    "selection worker did not stop within {} ms; detaching it so shutdown can continue",
                    WORKER_STOP_TIMEOUT.as_millis()
                ));
            }
        }
    }
}

impl Drop for SelectionWorker {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

pub struct SelectionJob {
    pub capture_id: CaptureId,
    pub triggered_at: Instant,
    pub cpu_ready_offset_ns: u64,
    pub source: &'static str,
    pub frame: CpuFrame,
    pub native_frame: Option<Arc<dyn NativeFrame>>,
    pub recorder: EventRecorder,
}

pub enum SubmitError {
    Full(Box<SelectionJob>),
    Disconnected(Box<SelectionJob>),
}

pub fn try_submit(
    sender: &mpsc::SyncSender<SelectionJob>,
    job: SelectionJob,
) -> Result<(), SubmitError> {
    sender.try_send(job).map_err(|error| match error {
        mpsc::TrySendError::Full(job) => SubmitError::Full(Box::new(job)),
        mpsc::TrySendError::Disconnected(job) => SubmitError::Disconnected(Box::new(job)),
    })
}

pub fn finish_rejected(mut job: SelectionJob) -> Result<CaptureId, AppError> {
    job.recorder.record(
        job.capture_id,
        PerfEventKind::AttemptFinished,
        duration_ns(job.triggered_at.elapsed()),
    );
    validate_event_order(job.recorder.events())?;
    Ok(job.capture_id)
}

fn finish_without_clipboard(
    job: &mut SelectionJob,
    json_output: bool,
    event: &'static str,
    message: &str,
) {
    job.recorder.record(
        job.capture_id,
        PerfEventKind::AttemptFinished,
        duration_ns(job.triggered_at.elapsed()),
    );
    if let Err(error) = validate_event_order(job.recorder.events()) {
        crate::logging::error(format_args!(
            "selection {} metrics failed validation: {error}",
            job.capture_id.0
        ));
    }
    if event == "selection_cancelled" {
        log::info!("selection {} cancelled", job.capture_id.0);
    } else {
        crate::logging::error(format_args!(
            "selection {} failed: {message}",
            job.capture_id.0
        ));
    }
    if json_output {
        println!(
            "{}",
            json!({
                "schema_version": 1,
                "event": event,
                "capture_id": job.capture_id,
                "source": job.source,
                "message": message,
            })
        );
    }
}

fn report_clipboard_rejection(
    job: crate::clipboard::ClipboardJob,
    reason: &'static str,
    json_output: bool,
) {
    let capture_id = job.capture_id;
    let _ = crate::clipboard::finish_rejected(job);
    crate::logging::warn(format_args!("clipboard {} skipped: {reason}", capture_id.0));
    if json_output {
        println!(
            "{}",
            json!({
                "schema_version": 1,
                "event": "clipboard_skipped",
                "capture_id": capture_id,
                "reason": reason,
            })
        );
    }
}

fn selection_kind(kind: captastic_windows::SelectionKind) -> &'static str {
    match kind {
        captastic_windows::SelectionKind::Display => "display",
        captastic_windows::SelectionKind::Region => "region",
        captastic_windows::SelectionKind::Window => "window",
    }
}

fn selection_materialization(kind: captastic_windows::SelectionKind) -> &'static str {
    match kind {
        captastic_windows::SelectionKind::Display => "frozen_display",
        captastic_windows::SelectionKind::Region => "frozen_desktop_crop",
        captastic_windows::SelectionKind::Window => "native_window_render",
    }
}

fn offset_after_cpu(job: &SelectionJob) -> u64 {
    duration_ns(job.triggered_at.elapsed()).saturating_sub(job.cpu_ready_offset_ns)
}

fn duration_ns(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}
