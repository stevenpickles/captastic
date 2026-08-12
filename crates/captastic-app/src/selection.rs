use std::collections::BTreeMap;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use captastic_config::{
    CaptureRegion, CaptureRegionSource, ConfirmedRegion, HotkeyAction, HotkeyChord,
};
use captastic_core::{
    validate_event_order, CaptureId, CpuFrame, EventRecorder, NativeFrame, PerfEventKind,
};
use serde_json::json;
use std::sync::Mutex;

use crate::error::AppError;

const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(1);
const WORKER_STOP_POLL: Duration = Duration::from_millis(5);

pub type ConfirmedRegionCache = Arc<Mutex<BTreeMap<String, ConfirmedRegion>>>;

pub struct OneShotUiStateWorker {
    controller: Option<captastic_windows::OverlayController>,
    join: Option<JoinHandle<()>>,
}

impl OneShotUiStateWorker {
    pub fn start(store: captastic_config::UiStateStore) -> Result<Self, AppError> {
        let (sender, receiver) = mpsc::channel();
        let controller = captastic_windows::OverlayController::with_ui_updates(sender);
        let join = thread::Builder::new()
            .name("captastic-ui-state-once".to_owned())
            .spawn(move || persist_ui_state(receiver, store))
            .map_err(|error| AppError::BackendUnavailable(error.to_string()))?;
        Ok(Self {
            controller: Some(controller),
            join: Some(join),
        })
    }

    pub fn controller(&self) -> &captastic_windows::OverlayController {
        self.controller.as_ref().expect("UI-state worker is active")
    }
}

impl Drop for OneShotUiStateWorker {
    fn drop(&mut self) {
        self.controller.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub struct SelectionWorker {
    sender: Option<mpsc::SyncSender<SelectionJob>>,
    controller: Option<captastic_windows::OverlayController>,
    ui_sender: Option<mpsc::Sender<captastic_windows::OverlayUiUpdate>>,
    join: Option<JoinHandle<()>>,
    ui_join: Option<JoinHandle<()>>,
}

impl SelectionWorker {
    pub fn start(
        clipboard_sender: mpsc::SyncSender<crate::clipboard::ClipboardJob>,
        json_output: bool,
        queue_capacity: usize,
        confirmed_regions: ConfirmedRegionCache,
        ui_state_store: captastic_config::UiStateStore,
    ) -> Result<Self, AppError> {
        let (sender, receiver) = mpsc::sync_channel::<SelectionJob>(queue_capacity);
        let (ui_sender, ui_receiver) = mpsc::channel();
        let ui_join = thread::Builder::new()
            .name("captastic-ui-state".to_owned())
            .spawn(move || persist_ui_state(ui_receiver, ui_state_store))
            .map_err(|error| AppError::BackendUnavailable(error.to_string()))?;
        let controller = captastic_windows::OverlayController::with_ui_updates(ui_sender.clone());
        let worker_controller = controller.clone();
        let join = match thread::Builder::new()
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
                let remembered_ui = worker_controller.remembered_ui(
                    &job.frame.metadata.display_id.0,
                    job.remembered_ui.unwrap_or_default(),
                );
                let selection = captastic_windows::select_from_frozen_frame_with_initial_tool_and_ui(
                    &job.frame,
                    &worker_controller,
                    job.initial_tool,
                    Some(remembered_ui),
                );
                match selection {
                    Ok(Some(selection)) => {
                        if selection.kind == captastic_windows::SelectionKind::Region {
                            remember_confirmed_region(
                                &confirmed_regions,
                                &worker_controller,
                                &job.frame.metadata,
                                selection.rect,
                            );
                        }
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
                                    "action": job.action,
                                    "chord": job.chord.map(|chord| chord.to_string()),
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
                            action: job.action,
                            chord: job.chord,
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
            }) {
            Ok(join) => join,
            Err(error) => {
                drop(controller);
                drop(ui_sender);
                let _ = ui_join.join();
                return Err(AppError::BackendUnavailable(error.to_string()));
            }
        };
        Ok(Self {
            sender: Some(sender),
            controller: Some(controller),
            ui_sender: Some(ui_sender),
            join: Some(join),
            ui_join: Some(ui_join),
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
        if let Some(controller) = self.controller.as_ref() {
            controller.cancel();
        }
        let mut selection_stopped = true;
        if let Some(join) = self.join.take() {
            let started = Instant::now();
            while !join.is_finished() && started.elapsed() < WORKER_STOP_TIMEOUT {
                thread::sleep(WORKER_STOP_POLL);
            }
            if join.is_finished() {
                let _ = join.join();
            } else {
                selection_stopped = false;
                crate::logging::error(format_args!(
                    "selection worker did not stop within {} ms; detaching it so shutdown can continue",
                    WORKER_STOP_TIMEOUT.as_millis()
                ));
            }
        }
        self.controller.take();
        self.ui_sender.take();
        if let Some(join) = self.ui_join.take() {
            if selection_stopped {
                let _ = join.join();
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
    pub action: HotkeyAction,
    pub chord: Option<HotkeyChord>,
    pub initial_tool: captastic_windows::InitialSelectionTool,
    pub cpu_ready_offset_ns: u64,
    pub remembered_ui: Option<captastic_config::DisplayUiState>,
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

fn remember_confirmed_region(
    cache: &ConfirmedRegionCache,
    controller: &captastic_windows::OverlayController,
    metadata: &captastic_core::FrameMetadata,
    rect: captastic_core::Rect,
) {
    let source = metadata.source_rect;
    let local_x = i64::from(rect.x) - i64::from(source.x);
    let local_y = i64::from(rect.y) - i64::from(source.y);
    let Some(local_x) = i32::try_from(local_x).ok() else {
        crate::logging::warn(format_args!(
            "confirmed region x coordinate is out of range"
        ));
        return;
    };
    let Some(local_y) = i32::try_from(local_y).ok() else {
        crate::logging::warn(format_args!(
            "confirmed region y coordinate is out of range"
        ));
        return;
    };
    let confirmed = ConfirmedRegion {
        region: CaptureRegion {
            x: local_x,
            y: local_y,
            width: rect.width,
            height: rect.height,
        },
        source: CaptureRegionSource {
            width: source.width,
            height: source.height,
            rotation_degrees: metadata.rotation_degrees,
        },
    };
    {
        let mut state = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.insert(metadata.display_id.0.clone(), confirmed);
    }
    controller.submit_ui_update(captastic_windows::OverlayUiUpdate::ConfirmedRegion {
        display_id: metadata.display_id.0.clone(),
        region: confirmed.region,
        source: confirmed.source,
    });
}

fn persist_ui_state(
    receiver: mpsc::Receiver<captastic_windows::OverlayUiUpdate>,
    store: captastic_config::UiStateStore,
) {
    while let Ok(first) = receiver.recv() {
        let mut latest = BTreeMap::<(String, u8), captastic_windows::OverlayUiUpdate>::new();
        for update in std::iter::once(first).chain(receiver.try_iter()) {
            let key = match &update {
                captastic_windows::OverlayUiUpdate::Interaction { display_id, .. } => {
                    (display_id.clone(), 0)
                }
                captastic_windows::OverlayUiUpdate::ToolbarCenter { display_id, .. } => {
                    (display_id.clone(), 1)
                }
                captastic_windows::OverlayUiUpdate::ConfirmedRegion { display_id, .. } => {
                    (display_id.clone(), 2)
                }
            };
            coalesce_ui_update(&mut latest, key, update);
        }
        for update in latest.into_values() {
            let result = match update {
                captastic_windows::OverlayUiUpdate::Interaction {
                    display_id,
                    tool,
                    region,
                    source,
                } => store.save_display_interaction_state(&display_id, tool, region, source),
                captastic_windows::OverlayUiUpdate::ToolbarCenter {
                    display_id,
                    center_x,
                    center_y,
                } => store.save_display_overlay_center(&display_id, center_x, center_y),
                captastic_windows::OverlayUiUpdate::ConfirmedRegion {
                    display_id,
                    region,
                    source,
                } => store.save_display_confirmed_region(&display_id, region, source),
            };
            if let Err(error) = result {
                crate::logging::warn(format_args!("failed to persist UI state: {error}"));
            }
        }
    }
}

fn coalesce_ui_update(
    latest: &mut BTreeMap<(String, u8), captastic_windows::OverlayUiUpdate>,
    key: (String, u8),
    mut update: captastic_windows::OverlayUiUpdate,
) {
    if let (
        Some(captastic_windows::OverlayUiUpdate::Interaction {
            region: previous_region,
            source: previous_source,
            ..
        }),
        captastic_windows::OverlayUiUpdate::Interaction { region, source, .. },
    ) = (latest.get(&key), &mut update)
    {
        if region.is_none() {
            *region = *previous_region;
            *source = *previous_source;
        }
    }
    latest.insert(key, update);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalescing_interactions_preserves_a_region_from_earlier_in_the_batch() {
        let key = ("display-1".to_owned(), 0);
        let region = CaptureRegion {
            x: 1,
            y: 2,
            width: 30,
            height: 40,
        };
        let mut latest = BTreeMap::new();
        coalesce_ui_update(
            &mut latest,
            key.clone(),
            captastic_windows::OverlayUiUpdate::Interaction {
                display_id: "display-1".to_owned(),
                tool: captastic_config::CaptureTool::Region,
                region: Some(region),
                source: None,
            },
        );
        coalesce_ui_update(
            &mut latest,
            key.clone(),
            captastic_windows::OverlayUiUpdate::Interaction {
                display_id: "display-1".to_owned(),
                tool: captastic_config::CaptureTool::Window,
                region: None,
                source: None,
            },
        );

        let captastic_windows::OverlayUiUpdate::Interaction {
            tool,
            region: saved_region,
            ..
        } = latest.get(&key).expect("coalesced interaction")
        else {
            panic!("expected interaction update");
        };
        assert_eq!(*tool, captastic_config::CaptureTool::Window);
        assert_eq!(*saved_region, Some(region));
    }
}
