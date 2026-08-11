#[cfg(windows)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::{mpsc, Arc};
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use captastic_config::AppConfig;
#[cfg(all(windows, test))]
use captastic_core::DisplayId;
#[cfg(windows)]
use captastic_core::{
    validate_event_order, CaptureError, CaptureErrorKind, CaptureId, CaptureMode, CaptureRequest,
    CaptureSource, CpuFrame, CursorMode, EventRecorder, NativeFrame, PerfEventKind,
};
#[cfg(windows)]
use serde_json::json;

use crate::cli::DaemonArgs;
#[cfg(windows)]
use crate::cli::ModeArg;
use crate::error::AppError;
#[cfg(windows)]
use crate::DisplayPolicy;

#[cfg(windows)]
struct ResolvedDaemonArgs {
    backend: String,
    display_policy: DisplayPolicy,
    mode: CaptureMode,
    cpu_frame: bool,
    clipboard: bool,
    selection: bool,
    trigger_queue_capacity: usize,
    clipboard_queue_capacity: usize,
    selection_queue_capacity: usize,
    max_captures: Option<usize>,
    self_trigger: bool,
    json: bool,
}

#[cfg(windows)]
fn resolve_daemon_args(args: DaemonArgs) -> Result<ResolvedDaemonArgs, AppError> {
    let config = match args.config.as_deref() {
        Some(path) => AppConfig::load(path)?,
        None => AppConfig::load_default()?,
    };
    config.validate()?;
    let mode = args.mode.unwrap_or(match config.capture.mode.as_str() {
        "fresh" => ModeArg::Fresh,
        _ => ModeArg::Latest,
    });
    let fresh_timeout_ms = args
        .fresh_timeout_ms
        .unwrap_or(config.capture.fresh_timeout_ms);
    let maximum_age = args
        .max_frame_age_ms
        .unwrap_or(config.capture.max_frame_age_ms);
    let mode = match mode {
        ModeArg::Fresh => CaptureMode::Fresh {
            timeout_ms: fresh_timeout_ms,
        },
        ModeArg::Latest => CaptureMode::Latest {
            max_age_ms: (maximum_age != 0).then_some(maximum_age),
        },
    };
    Ok(ResolvedDaemonArgs {
        backend: args.backend.unwrap_or(config.daemon.backend),
        display_policy: super::resolve_display_policy(
            args.display.as_deref().unwrap_or(&config.daemon.display),
        )?,
        mode,
        cpu_frame: args.cpu_frame.unwrap_or(config.capture.cpu_frame),
        clipboard: args.clipboard.unwrap_or(config.clipboard.enabled),
        selection: args.selection.unwrap_or(config.selection.enabled),
        trigger_queue_capacity: config.daemon.trigger_queue_capacity,
        clipboard_queue_capacity: config.clipboard.queue_capacity,
        selection_queue_capacity: config.selection.queue_capacity,
        max_captures: args.max_captures,
        self_trigger: args.self_trigger,
        json: args.json,
    })
}

#[cfg(windows)]
pub fn run(args: DaemonArgs) -> Result<(), AppError> {
    let args = resolve_daemon_args(args)?;
    log::info!(
        "starting daemon backend={} display={} mode={:?} cpu_frame={} selection={} clipboard={}",
        args.backend,
        args.display_policy.as_config_value(),
        args.mode,
        args.cpu_frame,
        args.selection,
        args.clipboard
    );
    let daemon_control = captastic_windows::DaemonControl::create()?;
    if args.max_captures == Some(0) {
        return Err(AppError::InvalidArgument(
            "max-captures must be greater than zero".to_owned(),
        ));
    }
    if args.clipboard && !args.cpu_frame {
        return Err(AppError::InvalidArgument(
            "clipboard output requires --cpu-frame true".to_owned(),
        ));
    }
    if args.selection && !args.clipboard {
        return Err(AppError::InvalidArgument(
            "native selection currently requires --clipboard true".to_owned(),
        ));
    }
    let clipboard_worker = args
        .clipboard
        .then(|| crate::clipboard::ClipboardWorker::start(args.json, args.clipboard_queue_capacity))
        .transpose()?;
    let selection_worker = args
        .selection
        .then(|| {
            crate::selection::SelectionWorker::start(
                clipboard_worker
                    .as_ref()
                    .expect("selection requires clipboard")
                    .submitter(),
                args.json,
                args.selection_queue_capacity,
            )
        })
        .transpose()?;
    let selection_sender = selection_worker
        .as_ref()
        .map(crate::selection::SelectionWorker::submitter);
    let clipboard_sender = if args.selection {
        None
    } else {
        clipboard_worker
            .as_ref()
            .map(crate::clipboard::ClipboardWorker::submitter)
    };
    let (command_sender, command_receiver) = mpsc::sync_channel(args.trigger_queue_capacity);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let (done_sender, done_receiver) = mpsc::sync_channel::<Result<(), AppError>>(1);
    let backend_name = args.backend.clone();
    let display_policy = args.display_policy.clone();
    let mode = args.mode.clone();
    let cpu_frame = args.cpu_frame;
    let retain_native_frame = args.selection;
    let max_captures = args.max_captures;
    let json_output = args.json;

    let capture_join = thread::Builder::new()
        .name("captastic-capture".to_owned())
        .spawn(move || {
            let mut backend = match super::create_backend(&backend_name, &display_policy) {
                Ok(backend) => backend,
                Err(error) => {
                    let _ = ready_sender.send(Err(error));
                    return;
                }
            };
            let ready = json!({
                "backend": backend.name(),
                "configured_display": display_policy.as_config_value(),
                "displays": backend.displays(),
            });
            if ready_sender.send(Ok(ready)).is_err() {
                return;
            }

            let mut attempts = 0_usize;
            let mut next_capture_id = 1_u64;
            let mut recovery: Option<BackendRecovery> = None;
            loop {
                if recovery
                    .as_ref()
                    .is_some_and(|state| Instant::now() >= state.next_attempt)
                {
                    match super::create_backend(&backend_name, &display_policy) {
                        Ok(replacement) => {
                            backend = replacement;
                            recovery = None;
                            log::info!("capture engine recovered and is ready for capture");
                        }
                        Err(error) => {
                            let state = recovery
                                .as_mut()
                                .expect("recovery state exists while retrying");
                            state.failed_attempts = state.failed_attempts.saturating_add(1);
                            state.next_attempt =
                                Instant::now() + recovery_delay(state.failed_attempts);
                            crate::logging::warn(format_args!(
                                "capture engine reinitialization failed; retrying in {:.0} ms: {error}",
                                recovery_delay(state.failed_attempts).as_secs_f64() * 1_000.0
                            ));
                        }
                    }
                }

                let command = if let Some(state) = recovery.as_ref() {
                    let wait = state.next_attempt.saturating_duration_since(Instant::now());
                    match command_receiver.recv_timeout(wait) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    match command_receiver.recv() {
                        Ok(command) => command,
                        Err(mpsc::RecvError) => break,
                    }
                };
                match command {
                    CaptureCommand::Trigger(trigger) => {
                        attempts = attempts.saturating_add(1);
                        let capture_id = CaptureId(next_capture_id);
                        next_capture_id = next_capture_id.saturating_add(1);
                        let mut recorder = EventRecorder::with_capacity(16);
                        recorder.record(capture_id, PerfEventKind::HotkeyReceived, 0);
                        recorder.record(
                            capture_id,
                            PerfEventKind::TriggerEnqueued,
                            duration_ns(trigger.received_at, trigger.enqueued_at),
                        );
                        recorder.record(
                            capture_id,
                            PerfEventKind::TriggerDequeued,
                            duration_ns(trigger.received_at, Instant::now()),
                        );
                        let capture_result = super::resolve_capture_display(
                            &display_policy,
                            backend.displays(),
                        )
                        .and_then(|display_id| {
                            log::debug!(
                                "capture {} resolved display={}",
                                capture_id.0,
                                display_id.0
                            );
                            let request = CaptureRequest {
                                id: capture_id,
                                triggered_at: trigger.received_at,
                                source: CaptureSource::Display(display_id),
                                mode: mode.clone(),
                                cpu_frame,
                                retain_native_frame,
                                cursor: CursorMode::Exclude,
                            };
                            backend.capture(&request, &mut recorder)
                        });
                        match capture_result {
                            Ok(outcome) => {
                                let frame_bytes = outcome
                                    .frame
                                    .as_ref()
                                    .map(captastic_core::CpuFrame::required_bytes);
                                let native_frame_retained = outcome.native_frame.is_some();
                                let output_status = match dispatch_output(
                                    selection_sender.as_ref(),
                                    clipboard_sender.as_ref(),
                                    capture_id,
                                    trigger.received_at,
                                    trigger.source,
                                    outcome.metadata.cpu_ready_offset_ns,
                                    outcome.frame,
                                    outcome.native_frame,
                                    recorder,
                                ) {
                                    Ok(status) => status,
                                    Err(error) => {
                                        let _ = done_sender.send(Err(error));
                                        break;
                                    }
                                };
                                log::info!(
                                    "capture {}: native {:.3} ms, CPU {} output={} bytes={:?}",
                                    capture_id.0,
                                    ns_to_ms(outcome.metadata.native_ready_offset_ns),
                                    outcome
                                        .metadata
                                        .cpu_ready_offset_ns
                                        .map(|value| format!("{:.3} ms", ns_to_ms(value)))
                                        .unwrap_or_else(|| "disabled".to_owned()),
                                    output_status,
                                    frame_bytes
                                );
                                if json_output {
                                    let value = json!({
                                        "schema_version": 1,
                                        "event": "capture_complete",
                                        "source": trigger.source,
                                        "metadata": outcome.metadata,
                                        "cpu_frame_bytes": frame_bytes,
                                        "native_frame_retained": native_frame_retained,
                                        "output": output_status,
                                    });
                                    println!("{value}");
                                }
                            }
                            Err(error) => {
                                recorder.record(capture_id, PerfEventKind::AttemptFinished, 0);
                                if let Err(metrics_error) = validate_event_order(recorder.events())
                                {
                                    let _ = done_sender.send(Err(metrics_error.into()));
                                    break;
                                }
                                crate::logging::error(format_args!(
                                    "capture {} failed: {error}",
                                    capture_id.0
                                ));
                                if requires_backend_recovery(&error) {
                                    recovery = Some(BackendRecovery::immediate());
                                }
                            }
                        }
                        if max_captures.is_some_and(|maximum| attempts >= maximum) {
                            let _ = done_sender.send(Ok(()));
                            break;
                        }
                    }
                    CaptureCommand::Shutdown => {
                        let _ = done_sender.send(Ok(()));
                        break;
                    }
                }
            }
        })
        .map_err(|error| AppError::InvalidArgument(error.to_string()))?;

    let ready = ready_receiver.recv().map_err(|error| {
        AppError::BackendUnavailable(format!("capture worker ended during startup: {error}"))
    })??;
    let dropped = Arc::new(AtomicU64::new(0));
    let paused = Arc::new(AtomicBool::new(false));
    let callback_sender = command_sender.clone();
    let callback_dropped = dropped.clone();
    let callback_paused = paused.clone();
    let hotkey = match captastic_windows::HotkeyListener::start(
        captastic_windows::HotkeySpec::ctrl_shift_f9(),
        move |received_at| {
            if callback_paused.load(Ordering::Acquire) {
                log::debug!("capture hotkey ignored while Captastic is paused");
                return;
            }
            let trigger = TriggerEvent {
                received_at,
                enqueued_at: Instant::now(),
                source: "hotkey",
            };
            if callback_sender
                .try_send(CaptureCommand::Trigger(trigger))
                .is_err()
            {
                callback_dropped.fetch_add(1, Ordering::Relaxed);
            }
        },
    ) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = command_sender.send(CaptureCommand::Shutdown);
            let _ = capture_join.join();
            return Err(error.into());
        }
    };
    let console_shutdown = match captastic_windows::ConsoleShutdown::install() {
        Ok(handler) => handler,
        Err(error) => {
            let _ = hotkey.stop();
            let _ = command_sender.send(CaptureCommand::Shutdown);
            let _ = capture_join.join();
            return Err(error.into());
        }
    };
    let startup_enabled = match captastic_windows::startup_command() {
        Ok(command) => command.is_some(),
        Err(error) => {
            crate::logging::warn(format_args!(
                "failed to read launch-at-login state: {error}"
            ));
            false
        }
    };
    let tray = match captastic_windows::TrayIcon::start(startup_enabled) {
        Ok(tray) => Some(tray),
        Err(error) => {
            crate::logging::warn(format_args!(
                "native tray is unavailable; daemon will continue without it: {error}"
            ));
            None
        }
    };

    let ready_value = json!({
        "schema_version": 1,
        "event": "ready",
        "hotkey": captastic_windows::HotkeySpec::ctrl_shift_f9().label(),
        "capture": ready,
        "queue_capacity": args.trigger_queue_capacity,
        "clipboard": args.clipboard,
        "clipboard_queue_capacity": args.clipboard.then_some(args.clipboard_queue_capacity),
        "selection": args.selection,
        "selection_queue_capacity": args.selection.then_some(args.selection_queue_capacity),
        "tray": tray.is_some(),
        "log_file": crate::logging::path().map(|path| path.display().to_string()),
    });
    if args.json {
        println!("{ready_value}");
    } else {
        log::info!(
            "Captastic is ready: {} using {}, selection {}, clipboard {} (press Ctrl+C to stop)",
            captastic_windows::HotkeySpec::ctrl_shift_f9().label(),
            args.backend,
            if args.selection {
                "enabled"
            } else {
                "disabled"
            },
            if args.clipboard {
                "enabled"
            } else {
                "disabled"
            },
        );
        if let Some(path) = crate::logging::path() {
            log::info!("Persistent log: {}", path.display());
        }
    }

    if args.self_trigger {
        command_sender
            .try_send(CaptureCommand::Trigger(TriggerEvent {
                received_at: Instant::now(),
                enqueued_at: Instant::now(),
                source: "self_test",
            }))
            .map_err(|error| AppError::InvalidArgument(error.to_string()))?;
    }

    let mut shutdown_sent = false;
    let mut tray_shutdown_requested = false;
    loop {
        match done_receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => {
                result?;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(tray) = tray.as_ref() {
                    while let Some(event) = tray.try_recv() {
                        match event {
                            captastic_windows::TrayEvent::Capture => {
                                let received_at = Instant::now();
                                let trigger = TriggerEvent {
                                    received_at,
                                    enqueued_at: Instant::now(),
                                    source: "tray",
                                };
                                if command_sender
                                    .try_send(CaptureCommand::Trigger(trigger))
                                    .is_err()
                                {
                                    dropped.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            captastic_windows::TrayEvent::PausedChanged(value) => {
                                paused.store(value, Ordering::Release);
                                log::info!(
                                    "Captastic {} from the notification area",
                                    if value { "paused" } else { "resumed" }
                                );
                            }
                            captastic_windows::TrayEvent::OpenConfig => open_config_from_tray(),
                            captastic_windows::TrayEvent::OpenLogs => open_logs_from_tray(),
                            captastic_windows::TrayEvent::ToggleStartup => {
                                toggle_startup_from_tray(tray)
                            }
                            captastic_windows::TrayEvent::Exit => {
                                tray_shutdown_requested = true;
                            }
                        }
                    }
                }
                if (console_shutdown.requested()
                    || daemon_control.requested()
                    || tray_shutdown_requested)
                    && !shutdown_sent
                {
                    match command_sender.try_send(CaptureCommand::Shutdown) {
                        Ok(()) => shutdown_sent = true,
                        Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => {
                            return Err(AppError::BackendUnavailable(
                                "capture worker stopped during console shutdown".to_owned(),
                            ));
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(AppError::BackendUnavailable(
                    "capture worker stopped unexpectedly".to_owned(),
                ));
            }
        }
    }
    if let Some(tray) = tray {
        tray.stop()?;
    }
    hotkey.stop()?;
    let _ = command_sender.send(CaptureCommand::Shutdown);
    let _ = capture_join.join();
    if let Some(worker) = selection_worker {
        worker.stop();
    }
    if let Some(worker) = clipboard_worker {
        worker.stop();
    }
    let dropped = dropped.load(Ordering::Relaxed);
    if dropped != 0 {
        crate::logging::warn(format_args!(
            "Captastic dropped {dropped} hotkey trigger(s) because the queue was full"
        ));
    }
    if shutdown_sent {
        log::info!("Captastic stopped cleanly");
    } else {
        log::info!("daemon stopped cleanly");
    }
    Ok(())
}

#[cfg(windows)]
fn open_config_from_tray() {
    match captastic_config::ensure_default_config() {
        Ok(path) => {
            if let Err(error) = captastic_windows::open_path(&path) {
                crate::logging::warn(format_args!("failed to open configuration: {error}"));
            }
        }
        Err(error) => crate::logging::warn(format_args!(
            "failed to prepare the default configuration: {error}"
        )),
    }
}

#[cfg(windows)]
fn open_logs_from_tray() {
    let Some(path) = crate::logging::path() else {
        crate::logging::warn(format_args!("persistent log path is unavailable"));
        return;
    };
    if let Err(error) = captastic_windows::open_path(path) {
        crate::logging::warn(format_args!("failed to open persistent log: {error}"));
    }
}

#[cfg(windows)]
fn toggle_startup_from_tray(tray: &captastic_windows::TrayIcon) {
    let result = match captastic_windows::startup_command() {
        Ok(Some(_)) => captastic_windows::disable_startup().map(|_| false),
        Ok(None) => desktop_launcher_path()
            .map_err(|error| captastic_core::CaptureError {
                kind: CaptureErrorKind::SourceUnavailable,
                backend: "windows-startup",
                operation: "locate_desktop_launcher",
                message: error.to_string(),
                retryable: false,
                native_code: None,
            })
            .and_then(|path| captastic_windows::enable_startup(&path).map(|()| true)),
        Err(error) => Err(error),
    };
    match result {
        Ok(enabled) => {
            if let Err(error) = tray.set_startup_enabled(enabled) {
                crate::logging::warn(format_args!(
                    "launch-at-login changed but the tray menu did not update: {error}"
                ));
            }
            log::info!(
                "launch at login {} from the notification area",
                if enabled { "enabled" } else { "disabled" }
            );
        }
        Err(error) => crate::logging::warn(format_args!(
            "failed to change launch-at-login state: {error}"
        )),
    }
}

#[cfg(windows)]
fn desktop_launcher_path() -> Result<std::path::PathBuf, std::io::Error> {
    let mut path = std::env::current_exe()?;
    path.set_file_name("captastic-desktop.exe");
    Ok(path)
}

#[cfg(windows)]
struct BackendRecovery {
    failed_attempts: u32,
    next_attempt: Instant,
}

#[cfg(windows)]
impl BackendRecovery {
    fn immediate() -> Self {
        Self {
            failed_attempts: 0,
            next_attempt: Instant::now(),
        }
    }
}

#[cfg(windows)]
fn recovery_delay(failed_attempts: u32) -> Duration {
    let exponent = failed_attempts.saturating_sub(1).min(5);
    Duration::from_millis(50_u64.saturating_mul(1_u64 << exponent)).min(Duration::from_secs(2))
}

#[cfg(windows)]
fn requires_backend_recovery(error: &CaptureError) -> bool {
    matches!(
        error.kind,
        CaptureErrorKind::AccessLost
            | CaptureErrorKind::DeviceRemoved
            | CaptureErrorKind::TopologyChanged
    )
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn dispatch_output(
    selection_sender: Option<&mpsc::SyncSender<crate::selection::SelectionJob>>,
    clipboard_sender: Option<&mpsc::SyncSender<crate::clipboard::ClipboardJob>>,
    capture_id: CaptureId,
    triggered_at: Instant,
    source: &'static str,
    cpu_ready_offset_ns: Option<u64>,
    frame: Option<CpuFrame>,
    native_frame: Option<Arc<dyn NativeFrame>>,
    recorder: EventRecorder,
) -> Result<&'static str, AppError> {
    if let Some(sender) = selection_sender {
        return dispatch_selection(
            sender,
            capture_id,
            triggered_at,
            source,
            cpu_ready_offset_ns,
            frame,
            native_frame,
            recorder,
        );
    }
    dispatch_clipboard(
        clipboard_sender,
        capture_id,
        triggered_at,
        source,
        cpu_ready_offset_ns,
        frame,
        recorder,
    )
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn dispatch_selection(
    sender: &mpsc::SyncSender<crate::selection::SelectionJob>,
    capture_id: CaptureId,
    triggered_at: Instant,
    source: &'static str,
    cpu_ready_offset_ns: Option<u64>,
    frame: Option<CpuFrame>,
    native_frame: Option<Arc<dyn NativeFrame>>,
    recorder: EventRecorder,
) -> Result<&'static str, AppError> {
    let frame = frame.ok_or_else(|| {
        AppError::BackendUnavailable(
            "selection was enabled but the capture backend returned no CPU frame".to_owned(),
        )
    })?;
    let cpu_ready_offset_ns = cpu_ready_offset_ns.ok_or_else(|| {
        AppError::BackendUnavailable(
            "selection was enabled but CPU readiness was not measured".to_owned(),
        )
    })?;
    let job = crate::selection::SelectionJob {
        capture_id,
        triggered_at,
        cpu_ready_offset_ns,
        source,
        frame,
        native_frame,
        recorder,
    };
    match crate::selection::try_submit(sender, job) {
        Ok(()) => Ok("selection_queued"),
        Err(crate::selection::SubmitError::Full(job)) => {
            let capture_id = crate::selection::finish_rejected(*job)?;
            crate::logging::warn(format_args!(
                "selection {} skipped because the selection queue is full; capture remains valid",
                capture_id.0
            ));
            Ok("selection_queue_full")
        }
        Err(crate::selection::SubmitError::Disconnected(job)) => {
            let capture_id = crate::selection::finish_rejected(*job)?;
            crate::logging::warn(format_args!(
                "selection {} skipped because the selection worker stopped; capture remains valid",
                capture_id.0
            ));
            Ok("selection_worker_disconnected")
        }
    }
}

#[cfg(windows)]
fn dispatch_clipboard(
    sender: Option<&mpsc::SyncSender<crate::clipboard::ClipboardJob>>,
    capture_id: CaptureId,
    triggered_at: Instant,
    source: &'static str,
    cpu_ready_offset_ns: Option<u64>,
    frame: Option<CpuFrame>,
    mut recorder: EventRecorder,
) -> Result<&'static str, AppError> {
    let Some(sender) = sender else {
        recorder.record(
            capture_id,
            PerfEventKind::AttemptFinished,
            duration_ns(triggered_at, Instant::now()),
        );
        validate_event_order(recorder.events())?;
        return Ok("disabled");
    };
    let frame = frame.ok_or_else(|| {
        AppError::BackendUnavailable(
            "clipboard was enabled but the capture backend returned no CPU frame".to_owned(),
        )
    })?;
    let cpu_ready_offset_ns = cpu_ready_offset_ns.ok_or_else(|| {
        AppError::BackendUnavailable(
            "clipboard was enabled but CPU readiness was not measured".to_owned(),
        )
    })?;
    let job = crate::clipboard::ClipboardJob {
        capture_id,
        triggered_at,
        cpu_ready_offset_ns,
        source,
        frame,
        recorder,
    };
    match crate::clipboard::try_submit(sender, job) {
        Ok(()) => Ok("queued"),
        Err(crate::clipboard::SubmitError::Full(job)) => {
            let capture_id = crate::clipboard::finish_rejected(*job)?;
            crate::logging::warn(format_args!(
                "clipboard {} skipped because the clipboard queue is full; capture remains valid",
                capture_id.0
            ));
            Ok("queue_full")
        }
        Err(crate::clipboard::SubmitError::Disconnected(job)) => {
            let capture_id = crate::clipboard::finish_rejected(*job)?;
            crate::logging::warn(format_args!(
                "clipboard {} skipped because the clipboard worker stopped; capture remains valid",
                capture_id.0
            ));
            Ok("worker_disconnected")
        }
    }
}

#[cfg(not(windows))]
pub fn run(_args: DaemonArgs) -> Result<(), AppError> {
    Err(AppError::BackendUnavailable(
        "the native hotkey daemon is currently available only on Windows".to_owned(),
    ))
}

#[cfg(windows)]
#[derive(Debug)]
enum CaptureCommand {
    Trigger(TriggerEvent),
    Shutdown,
}

#[cfg(windows)]
#[derive(Debug)]
struct TriggerEvent {
    received_at: Instant,
    enqueued_at: Instant,
    source: &'static str,
}

#[cfg(windows)]
fn duration_ns(start: Instant, end: Instant) -> u64 {
    u64::try_from(end.saturating_duration_since(start).as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(windows)]
fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::Arc;

    use captastic_core::{
        CaptureMode, ColorSpace, FrameMetadata, FrameOrigin, PixelFormat, Rect, TimingProvenance,
    };

    use super::*;

    #[test]
    fn backend_recovery_is_limited_to_session_invalidating_errors() {
        let error = |kind| CaptureError {
            kind,
            backend: "test",
            operation: "test",
            message: "test".to_owned(),
            retryable: true,
            native_code: None,
        };
        assert!(requires_backend_recovery(&error(
            CaptureErrorKind::AccessLost
        )));
        assert!(requires_backend_recovery(&error(
            CaptureErrorKind::DeviceRemoved
        )));
        assert!(!requires_backend_recovery(&error(
            CaptureErrorKind::Timeout
        )));
        assert_eq!(recovery_delay(1), Duration::from_millis(50));
        assert_eq!(recovery_delay(99), Duration::from_millis(1_600));
    }

    #[test]
    fn disconnected_clipboard_worker_does_not_invalidate_capture() {
        let capture_id = CaptureId(1);
        let triggered_at = Instant::now();
        let mut recorder = EventRecorder::with_capacity(16);
        for kind in [
            PerfEventKind::HotkeyReceived,
            PerfEventKind::TriggerEnqueued,
            PerfEventKind::TriggerDequeued,
            PerfEventKind::CaptureRequested,
            PerfEventKind::NativeFrameReady,
            PerfEventKind::ReadbackStarted,
            PerfEventKind::CpuFrameReady,
        ] {
            recorder.record(capture_id, kind, 0);
        }
        let metadata = FrameMetadata {
            capture_id,
            backend: "test".to_owned(),
            display_id: DisplayId::primary(),
            source_rect: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            rotation_degrees: 0,
            capture_mode: CaptureMode::Latest { max_age_ms: None },
            presentation_offset_ns: Some(0),
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: 1,
            cpu_ready_offset_ns: Some(2),
            frame_age_ns: Some(0),
            frame_generation: Some(1),
            copy_count: 1,
            pool_slot: Some(0),
        };
        let frame = CpuFrame::new(
            Arc::from([1_u8, 2, 3, 4]),
            1,
            1,
            4,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("valid frame");
        let (sender, receiver) = mpsc::sync_channel::<crate::clipboard::ClipboardJob>(1);
        drop(receiver);
        let status = dispatch_clipboard(
            Some(&sender),
            capture_id,
            triggered_at,
            "test",
            Some(2),
            Some(frame),
            recorder,
        )
        .expect("capture remains successful");
        assert_eq!(status, "worker_disconnected");
    }
}
