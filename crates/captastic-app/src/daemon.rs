#[cfg(windows)]
use std::collections::BTreeMap;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(windows)]
use std::sync::{mpsc, Arc, Mutex};
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
const CAPTURE_WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(windows)]
const CAPTURE_WORKER_STOP_POLL: Duration = Duration::from_millis(5);
#[cfg(windows)]
const DAEMON_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(windows)]
fn join_capture_worker_until(join: thread::JoinHandle<()>, deadline: Instant) {
    while !join.is_finished() && Instant::now() < deadline {
        thread::sleep(CAPTURE_WORKER_STOP_POLL);
    }
    if join.is_finished() {
        let _ = join.join();
    } else {
        crate::logging::error(format_args!(
            "capture worker did not stop before the daemon shutdown deadline; detaching it so shutdown can continue"
        ));
    }
}

#[cfg(windows)]
fn join_capture_worker(join: thread::JoinHandle<()>) {
    join_capture_worker_until(join, Instant::now() + CAPTURE_WORKER_STOP_TIMEOUT);
}

#[cfg(windows)]
use captastic_config::{
    AppConfig, ConfirmedRegion, HotkeyAction, HotkeyBinding, HotkeyChord, PreviewMode, UiConfig,
};
#[cfg(windows)]
use captastic_core::{
    validate_event_order, CaptureBackend, CaptureError, CaptureErrorKind, CaptureId, CaptureMode,
    CaptureRequest, CaptureSource, CpuFrame, CursorMode, DisplayId, DisplayInfo, EventRecorder,
    FrameMetadata, NativeFrame, PerfEventKind, Rect, TimingProvenance,
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
const CAPTURE_RECOVERY_RETRIES: u32 = 3;

#[cfg(windows)]
struct ResolvedDaemonArgs {
    backend: String,
    display_policy: DisplayPolicy,
    mode: CaptureMode,
    cpu_frame: bool,
    clipboard: bool,
    selection: bool,
    selection_preview: PreviewMode,
    trigger_queue_capacity: usize,
    hotkey_bindings: Vec<HotkeyBinding>,
    confirmed_regions: BTreeMap<String, ConfirmedRegion>,
    ui: UiConfig,
    ui_state_store: captastic_config::UiStateStore,
    clipboard_queue_capacity: usize,
    selection_queue_capacity: usize,
    max_captures: Option<usize>,
    self_trigger: bool,
    json: bool,
    startup_warnings: Vec<String>,
}

#[cfg(windows)]
fn resolve_daemon_args(args: DaemonArgs) -> Result<ResolvedDaemonArgs, AppError> {
    resolve_daemon_args_with_default(args, AppConfig::load_default_recovering)
}

#[cfg(windows)]
fn resolve_daemon_args_with_default(
    args: DaemonArgs,
    load_default: impl FnOnce() -> Result<
        (AppConfig, Option<captastic_config::ConfigRecovery>),
        captastic_config::ConfigError,
    >,
) -> Result<ResolvedDaemonArgs, AppError> {
    let mut startup_warnings = Vec::new();
    let ui_state_store = args
        .config
        .as_ref()
        .map_or_else(captastic_config::UiStateStore::for_default_config, |path| {
            captastic_config::UiStateStore::for_config(path.clone())
        });
    let config = match args.config.as_deref() {
        Some(path) => AppConfig::load(path)?,
        None => {
            let (config, recovery) = load_default()?;
            if let Some(recovery) = recovery {
                let message = format!(
                    "damaged configuration {} was quarantined as {}; continuing with defaults: {}",
                    recovery.original_path.display(),
                    recovery.quarantined_path.display(),
                    recovery.reason
                );
                crate::logging::error(format_args!("{message}"));
                startup_warnings.push(message);
            }
            config
        }
    };
    config.validate()?;
    let hotkey_bindings = config.hotkey.resolved_bindings()?;
    let confirmed_regions = config.confirmed_regions();
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
        selection_preview: config.selection.preview,
        trigger_queue_capacity: config.daemon.trigger_queue_capacity,
        clipboard_queue_capacity: config.clipboard.queue_capacity,
        hotkey_bindings,
        confirmed_regions,
        ui: config.ui.clone(),
        ui_state_store,
        selection_queue_capacity: config.selection.queue_capacity,
        max_captures: args.max_captures,
        self_trigger: args.self_trigger,
        json: args.json,
        startup_warnings,
    })
}

#[cfg(windows)]
pub fn run(args: DaemonArgs) -> Result<(), AppError> {
    let args = resolve_daemon_args(args)?;
    log::info!(
        "starting daemon version={} backend={} display={} mode={:?} cpu_frame={} selection={} clipboard={}",
        crate::build_info::BUILD_VERSION,
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
    if !args.selection {
        if let Some(binding) = args
            .hotkey_bindings
            .iter()
            .find(|binding| action_requires_selection(binding.action))
        {
            return Err(AppError::InvalidArgument(format!(
                "hotkey action {} requires selection.enabled = true",
                binding.action
            )));
        }
    }
    let mut clipboard_worker = args
        .clipboard
        .then(|| crate::clipboard::ClipboardWorker::start(args.json, args.clipboard_queue_capacity))
        .transpose()?;
    let (command_sender, command_receiver) = mpsc::sync_channel(args.trigger_queue_capacity);
    let confirmed_regions = Arc::new(Mutex::new(args.confirmed_regions.clone()));
    let mut selection_worker = args
        .selection
        .then(|| {
            crate::selection::SelectionWorker::start(
                clipboard_worker
                    .as_ref()
                    .expect("selection requires clipboard")
                    .submitter(),
                command_sender.clone(),
                args.json,
                args.selection_queue_capacity,
                confirmed_regions.clone(),
                args.ui_state_store.clone(),
            )
        })
        .transpose()?;
    let selection_sender = selection_worker
        .as_ref()
        .map(crate::selection::SelectionWorker::submitter);
    let clipboard_sender = clipboard_worker
        .as_ref()
        .map(crate::clipboard::ClipboardWorker::submitter);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let (done_sender, done_receiver) = mpsc::sync_channel::<Result<(), AppError>>(1);
    let capture_stop_requested = Arc::new(AtomicBool::new(false));
    let worker_stop_requested = capture_stop_requested.clone();
    let backend_name = args.backend.clone();
    let display_policy = args.display_policy.clone();
    let mode = args.mode.clone();
    let cpu_frame = args.cpu_frame;
    let selection_enabled = args.selection;
    let selection_preview = args.selection_preview;
    let max_captures = args.max_captures;
    let json_output = args.json;
    let capture_confirmed_regions = confirmed_regions.clone();
    let cached_ui = args.ui.clone();

    let capture_join = thread::Builder::new()
        .name("captastic-capture".to_owned())
        .spawn(move || {
            let backend = match super::create_backend(&backend_name, &display_policy) {
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
            let mut backend = Some(backend);

            let mut attempts = 0_usize;
            let mut next_capture_id = 1_u64;
            let mut recovery: Option<BackendRecovery> = None;
            loop {
                if worker_stop_requested.load(Ordering::Acquire) {
                    let _ = done_sender.send(Ok(()));
                    break;
                }
                if recovery
                    .as_ref()
                    .is_some_and(|state| Instant::now() >= state.next_attempt)
                {
                    backend.take();
                    match super::create_backend(&backend_name, &display_policy) {
                        Ok(replacement) => {
                            backend = Some(replacement);
                            recovery = None;
                            log::info!("capture engine reinitialized; validation is deferred until capture");
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
                        if backend.is_none() {
                            crate::logging::warn(format_args!(
                                "capture {} ignored while the capture engine is recovering",
                                capture_id.0
                            ));
                            continue;
                        }
                        if selection_enabled
                            && selection_preview != PreviewMode::Frozen
                            && matches!(action_route(trigger.action), ActionRoute::Overlay(_))
                        {
                            let active_backend = backend
                                .as_ref()
                                .expect("backend availability was checked above");
                            let source = match super::resolve_capture_source(
                                &display_policy,
                                active_backend.displays(),
                            ) {
                                Ok(source) => source,
                                Err(error) => {
                                    crate::logging::error(format_args!(
                                        "live selection {} could not resolve its display: {error}",
                                        capture_id.0
                                    ));
                                    continue;
                                }
                            };
                            let metadata = match preview_metadata(
                                capture_id,
                                &source,
                                active_backend.displays(),
                                mode.clone(),
                            ) {
                                Ok(metadata) => metadata,
                                Err(error) => {
                                    crate::logging::error(format_args!(
                                        "live selection {} could not describe its display: {error}",
                                        capture_id.0
                                    ));
                                    continue;
                                }
                            };
                            let Some(sender) = selection_sender.as_ref() else {
                                crate::logging::error(format_args!(
                                    "live selection {} has no selection worker",
                                    capture_id.0
                                ));
                                continue;
                            };
                            let recorder = trigger_recorder(capture_id, &trigger);
                            if let Err(error) = dispatch_live_selection(
                                sender,
                                capture_id,
                                &trigger,
                                action_route(trigger.action),
                                metadata,
                                &cached_ui,
                                recorder,
                            ) {
                                let _ = done_sender.send(Err(error));
                                break;
                            }
                            log::info!(
                                "capture {} action={} output=live_selection_queued",
                                capture_id.0,
                                trigger.action
                            );
                            continue;
                        }
                        let (
                            capture_result,
                            mut recorder,
                            recovery_attempts,
                            reinitialize_error,
                        ) = capture_with_backend_recovery(
                            &mut backend,
                            |active_backend| {
                                let mut recorder = trigger_recorder(capture_id, &trigger);
                                let capture_result = super::resolve_capture_source(
                                    &display_policy,
                                    active_backend.displays(),
                                )
                                .and_then(|source| {
                                    log::debug!(
                                        "capture {} action={} resolved source={source:?}",
                                        capture_id.0,
                                        trigger.action
                                    );
                                    let request = CaptureRequest {
                                        id: capture_id,
                                        triggered_at: trigger.received_at,
                                        source,
                                        mode: mode.clone(),
                                        cpu_frame,
                                        retain_native_frame: selection_enabled
                                            && trigger.action != HotkeyAction::FullDisplay,
                                        cursor: CursorMode::Exclude,
                                    };
                                    active_backend.capture(&request, &mut recorder)
                                });
                                (capture_result, recorder)
                            },
                            || super::create_backend(&backend_name, &display_policy),
                            thread::sleep,
                            |attempt, delay, error| {
                                crate::logging::warn(format_args!(
                                    "capture {} lost the capture engine; dropping it and retrying {}/{} in {:.0} ms: {error}",
                                    capture_id.0,
                                    attempt,
                                    CAPTURE_RECOVERY_RETRIES,
                                    delay.as_secs_f64() * 1_000.0
                                ));
                            },
                        );
                        if let Some(reinitialize_error) = reinitialize_error {
                            crate::logging::warn(format_args!(
                                "capture engine reinitialization failed during capture {}: {reinitialize_error}",
                                capture_id.0
                            ));
                            recovery = Some(BackendRecovery {
                                failed_attempts: recovery_attempts,
                                next_attempt: Instant::now()
                                    + recovery_delay(recovery_attempts),
                            });
                        }
                        if recovery_attempts > 0 && capture_result.is_ok() {
                            log::info!(
                                "capture engine recovered during capture {} after {} attempt(s)",
                                capture_id.0,
                                recovery_attempts
                            );
                        }
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
                                    trigger.action,
                                    trigger.chord,
                                    &outcome.metadata,
                                    &capture_confirmed_regions,
                                    &cached_ui,
                                    json_output,
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
                                    "capture {} action={}: native {:.3} ms, CPU {} output={} bytes={:?}",
                                    capture_id.0,
                                    trigger.action,
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
                                        "action": trigger.action,
                                        "chord": trigger.chord.map(|chord| chord.to_string()),
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
                                    "capture {} action={} failed: {error}",
                                    capture_id.0,
                                    trigger.action
                                ));
                                if requires_backend_recovery(&error) && recovery.is_none() {
                                    backend.take();
                                    recovery = Some(BackendRecovery::immediate());
                                }
                            }
                        }
                        if max_captures.is_some_and(|maximum| attempts >= maximum) {
                            let _ = done_sender.send(Ok(()));
                            break;
                        }
                    }
                    CaptureCommand::LiveSelection(mut request) => {
                        let capture_id = request.job.capture_id;
                        let capture_source = if request.job.metadata.display_id
                            == DisplayId::virtual_desktop()
                        {
                            CaptureSource::VirtualDesktop
                        } else {
                            CaptureSource::Display(request.job.metadata.display_id.clone())
                        };
                        if backend.is_none() {
                            request.job.terminal_error = Some(
                                "capture engine is recovering after live selection confirmation"
                                    .to_owned(),
                            );
                            request.job.confirmed_selection = Some(request.selection);
                            let _ = crate::selection::try_submit(
                                selection_sender
                                    .as_ref()
                                    .expect("live selection requires its worker"),
                                request.job,
                            );
                            continue;
                        }
                        let capture_request = CaptureRequest {
                            id: capture_id,
                            triggered_at: request.confirmed_at,
                            source: capture_source,
                            mode: mode.clone(),
                            cpu_frame: true,
                            retain_native_frame: request.selection.kind
                                == captastic_windows::SelectionKind::Region,
                            cursor: CursorMode::Exclude,
                        };
                        let (capture_result, (), recovery_attempts, reinitialize_error) =
                            capture_with_backend_recovery(
                                &mut backend,
                                |active_backend| {
                                    let result = active_backend
                                        .capture(&capture_request, &mut request.job.recorder);
                                    (result, ())
                                },
                                || super::create_backend(&backend_name, &display_policy),
                                thread::sleep,
                                |attempt, delay, error| {
                                    crate::logging::warn(format_args!(
                                        "confirmation capture {} lost the engine; retrying {}/{} in {:.0} ms: {error}",
                                        capture_id.0,
                                        attempt,
                                        CAPTURE_RECOVERY_RETRIES,
                                        delay.as_secs_f64() * 1_000.0
                                    ));
                                },
                            );
                        if let Some(reinitialize_error) = reinitialize_error {
                            recovery = Some(BackendRecovery {
                                failed_attempts: recovery_attempts,
                                next_attempt: Instant::now() + recovery_delay(recovery_attempts),
                            });
                            crate::logging::warn(format_args!(
                                "capture engine reinitialization failed during confirmation capture {}: {reinitialize_error}",
                                capture_id.0
                            ));
                        }
                        match capture_result {
                            Ok(outcome) => {
                                request.job.cpu_ready_offset_ns = Some(duration_ns(
                                    request.job.triggered_at,
                                    Instant::now(),
                                ));
                                request.job.metadata = outcome.metadata;
                                request.job.frame = outcome.frame;
                                request.job.native_frame = outcome.native_frame;
                                request.job.confirmed_selection = Some(request.selection);
                            }
                            Err(error) => {
                                if requires_backend_recovery(&error) && recovery.is_none() {
                                    backend.take();
                                    recovery = Some(BackendRecovery::immediate());
                                }
                                request.job.terminal_error = Some(error.to_string());
                                request.job.confirmed_selection = Some(request.selection);
                            }
                        }
                        let sender = selection_sender
                            .as_ref()
                            .expect("live selection requires its worker");
                        match crate::selection::try_submit(sender, request.job) {
                            Ok(()) => {}
                            Err(crate::selection::SubmitError::Full(job))
                            | Err(crate::selection::SubmitError::Disconnected(job)) => {
                                let _ = crate::selection::finish_rejected(*job);
                                crate::logging::warn(format_args!(
                                    "selection {} could not resume after confirmation capture",
                                    capture_id.0
                                ));
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
    let callback_stop_requested = capture_stop_requested.clone();
    let mut hotkey = match captastic_windows::HotkeyListener::start(
        &args.hotkey_bindings,
        move |action, chord, received_at| {
            if callback_paused.load(Ordering::Acquire)
                || callback_stop_requested.load(Ordering::Acquire)
            {
                return;
            }
            let trigger = TriggerEvent {
                received_at,
                enqueued_at: Instant::now(),
                source: "hotkey",
                action,
                chord: Some(chord),
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
            join_capture_worker(capture_join);
            return Err(error.into());
        }
    };
    let console_shutdown = match captastic_windows::ConsoleShutdown::install() {
        Ok(handler) => handler,
        Err(error) => {
            let _ = hotkey.stop();
            let _ = command_sender.send(CaptureCommand::Shutdown);
            join_capture_worker(capture_join);
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
    if let Some(tray) = tray.as_ref() {
        for warning in &args.startup_warnings {
            if let Err(error) =
                tray.show_error_with_title("Captastic configuration recovered", warning.clone())
            {
                crate::logging::warn(format_args!(
                    "failed to surface startup warning in the notification area: {error}"
                ));
            }
        }
    }

    let active_hotkeys = args
        .hotkey_bindings
        .iter()
        .map(|binding| json!({"action": binding.action, "chord": binding.chord.to_string()}))
        .collect::<Vec<_>>();
    let active_hotkey_labels = args
        .hotkey_bindings
        .iter()
        .map(|binding| format!("{}={}", binding.action, binding.chord))
        .collect::<Vec<_>>()
        .join(", ");

    let ready_value = json!({
        "schema_version": 1,
        "event": "ready",
        "hotkeys": active_hotkeys,
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
            "Captastic is ready: hotkeys [{}] using {}, selection {}, clipboard {} (press Ctrl+C to stop)",
            active_hotkey_labels,
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
                action: HotkeyAction::LastWorkflow,
                chord: None,
            }))
            .map_err(|error| AppError::InvalidArgument(error.to_string()))?;
    }

    let mut shutdown_sent = false;
    let mut shutdown_deadline = None;
    let mut tray_shutdown_requested = false;
    let mut last_persistence_notification: Option<String> = None;
    let mut daemon_result = Ok(());
    loop {
        let session_shutdown_requested = tray
            .as_ref()
            .is_some_and(captastic_windows::TrayIcon::session_shutdown_requested);
        if session_shutdown_requested {
            tray_shutdown_requested = true;
        }
        if tray_shutdown_requested && shutdown_deadline.is_none() {
            log::info!(
                "{}; draining daemon workers",
                if session_shutdown_requested {
                    "Windows session is ending"
                } else {
                    "notification-area shutdown requested"
                }
            );
            shutdown_deadline = Some(Instant::now() + DAEMON_SHUTDOWN_TIMEOUT);
            paused.store(true, Ordering::Release);
            capture_stop_requested.store(true, Ordering::Release);
            if let Some(worker) = selection_worker.as_mut() {
                worker.request_stop();
            }
            if let Some(worker) = clipboard_worker.as_mut() {
                worker.request_stop();
            }
            if let Err(error) = hotkey.request_stop() {
                crate::logging::warn(format_args!("failed to request hotkey shutdown: {error}"));
            }
        }
        if shutdown_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            crate::logging::error(format_args!(
                "daemon shutdown exceeded {} ms; continuing bounded teardown while the capture worker is detached",
                DAEMON_SHUTDOWN_TIMEOUT.as_millis()
            ));
            break;
        }
        match done_receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => {
                daemon_result = result;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(worker) = clipboard_worker.as_ref() {
                    while let Some(failure) = worker.try_recv_failure() {
                        if let Some(tray) = tray.as_ref() {
                            let message = format!(
                                "Capture {} was not copied to the clipboard. {}",
                                failure.capture_id.0, failure.message
                            );
                            if let Err(error) = tray.show_error(message) {
                                crate::logging::warn(format_args!(
                                    "failed to surface clipboard error in the notification area: {error}"
                                ));
                            }
                        }
                    }
                }
                if let Some(worker) = selection_worker.as_ref() {
                    while let Some(failure) = worker.try_recv_persistence_failure() {
                        if let Some(tray) = tray.as_ref() {
                            let message = format!(
                                "Captastic could not save selection preferences. {failure}"
                            );
                            if last_persistence_notification.as_deref() == Some(message.as_str()) {
                                continue;
                            }
                            last_persistence_notification = Some(message.clone());
                            if let Err(error) = tray.show_error_with_title(
                                "Captastic preferences were not saved",
                                message,
                            ) {
                                crate::logging::warn(format_args!(
                                    "failed to surface UI-state persistence error in the notification area: {error}"
                                ));
                            }
                        }
                    }
                }
                if let Some(tray) = tray.as_ref() {
                    while let Some(event) = tray.try_recv() {
                        match event {
                            captastic_windows::TrayEvent::Capture => {
                                let received_at = Instant::now();
                                let trigger = TriggerEvent {
                                    received_at,
                                    enqueued_at: Instant::now(),
                                    source: "tray",
                                    action: HotkeyAction::LastWorkflow,
                                    chord: None,
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
                            captastic_windows::TrayEvent::OpenConfig => {
                                if let Err(message) = open_config_from_tray(&args.ui_state_store) {
                                    crate::logging::warn(format_args!("{message}"));
                                    if let Err(error) = tray.show_error_with_title(
                                        "Captastic could not open configuration",
                                        message,
                                    ) {
                                        crate::logging::warn(format_args!(
                                            "failed to surface Open Config error in the notification area: {error}"
                                        ));
                                    }
                                }
                            }
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
                if console_shutdown.requested()
                    || daemon_control.requested()
                    || tray_shutdown_requested
                {
                    if shutdown_deadline.is_none() {
                        shutdown_deadline = Some(Instant::now() + DAEMON_SHUTDOWN_TIMEOUT);
                        paused.store(true, Ordering::Release);
                        capture_stop_requested.store(true, Ordering::Release);
                        if let Some(worker) = selection_worker.as_mut() {
                            worker.request_stop();
                        }
                        if let Some(worker) = clipboard_worker.as_mut() {
                            worker.request_stop();
                        }
                        if let Err(error) = hotkey.request_stop() {
                            crate::logging::warn(format_args!(
                                "failed to request hotkey shutdown: {error}"
                            ));
                        }
                    }
                    if !shutdown_sent {
                        match command_sender.try_send(CaptureCommand::Shutdown) {
                            Ok(()) => shutdown_sent = true,
                            Err(mpsc::TrySendError::Full(_)) => {}
                            Err(mpsc::TrySendError::Disconnected(_)) => {
                                shutdown_sent = true;
                            }
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                daemon_result = Err(AppError::BackendUnavailable(
                    "capture worker stopped unexpectedly".to_owned(),
                ));
                break;
            }
        }
    }
    let teardown_deadline =
        shutdown_deadline.unwrap_or_else(|| Instant::now() + DAEMON_SHUTDOWN_TIMEOUT);
    capture_stop_requested.store(true, Ordering::Release);
    if let Some(worker) = selection_worker.as_mut() {
        worker.request_stop();
    }
    if let Some(worker) = clipboard_worker.as_mut() {
        worker.request_stop();
    }
    let hotkey_stop_error = hotkey.stop_before(teardown_deadline).err();
    let _ = command_sender.try_send(CaptureCommand::Shutdown);
    join_capture_worker_until(capture_join, teardown_deadline);
    let persistence_failures = selection_worker
        .take()
        .map_or_else(Vec::new, |worker| worker.stop_before(teardown_deadline));
    let clipboard_failures = clipboard_worker
        .take()
        .map_or_else(Vec::new, |worker| worker.stop_before(teardown_deadline));
    for failure in clipboard_failures {
        crate::logging::warn(format_args!(
            "shutdown retained clipboard failure for capture {} in the persistent log: {}",
            failure.capture_id.0, failure.message
        ));
    }
    for failure in persistence_failures {
        crate::logging::warn(format_args!(
            "shutdown retained UI-state persistence failure in the persistent log: {failure}"
        ));
    }
    console_shutdown.signal_drained();
    if let Some(tray) = tray.as_ref() {
        tray.signal_session_drained();
    }
    if let Some(tray) = tray {
        tray.stop()?;
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
    if let Some(error) = hotkey_stop_error {
        return Err(error.into());
    }
    daemon_result
}

#[cfg(windows)]
fn open_config_from_tray(store: &captastic_config::UiStateStore) -> Result<(), String> {
    let path = store.prepare_for_open();
    match path {
        Ok(path) if path.exists() => captastic_windows::open_path(&path)
            .map_err(|error| format!("failed to open configuration: {error}")),
        Ok(path) => Err(format!("configuration does not exist: {}", path.display())),
        Err(error) => Err(format!("failed to prepare the configuration: {error}")),
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
fn capture_with_backend_recovery<T, R>(
    backend: &mut Option<Box<dyn CaptureBackend>>,
    mut attempt_capture: impl FnMut(&mut dyn CaptureBackend) -> (Result<T, CaptureError>, R),
    mut rebuild_backend: impl FnMut() -> Result<Box<dyn CaptureBackend>, AppError>,
    mut wait: impl FnMut(Duration),
    mut on_retry: impl FnMut(u32, Duration, &CaptureError),
) -> (Result<T, CaptureError>, R, u32, Option<AppError>) {
    let mut recovery_attempts = 0_u32;
    loop {
        let active_backend = backend
            .as_deref_mut()
            .expect("capture backend exists outside recovery");
        let (capture_result, recorder) = attempt_capture(active_backend);
        let Err(error) = &capture_result else {
            return (capture_result, recorder, recovery_attempts, None);
        };
        if !requires_backend_recovery(error) || recovery_attempts >= CAPTURE_RECOVERY_RETRIES {
            return (capture_result, recorder, recovery_attempts, None);
        }

        recovery_attempts = recovery_attempts.saturating_add(1);
        let delay = recovery_delay(recovery_attempts);
        on_retry(recovery_attempts, delay, error);
        backend.take();
        wait(delay);
        match rebuild_backend() {
            Ok(replacement) => *backend = Some(replacement),
            Err(reinitialize_error) => {
                return (
                    capture_result,
                    recorder,
                    recovery_attempts,
                    Some(reinitialize_error),
                );
            }
        }
    }
}

#[cfg(windows)]
fn action_requires_selection(action: HotkeyAction) -> bool {
    matches!(
        action,
        HotkeyAction::Region | HotkeyAction::Window | HotkeyAction::RepeatLastRegion
    )
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActionRoute {
    Overlay(captastic_windows::InitialSelectionTool),
    FullDisplay,
    RepeatLastRegion,
}

#[cfg(windows)]
fn action_route(action: HotkeyAction) -> ActionRoute {
    match action {
        HotkeyAction::LastWorkflow => {
            ActionRoute::Overlay(captastic_windows::InitialSelectionTool::Remembered)
        }
        HotkeyAction::Region => {
            ActionRoute::Overlay(captastic_windows::InitialSelectionTool::Region)
        }
        HotkeyAction::Window => {
            ActionRoute::Overlay(captastic_windows::InitialSelectionTool::Window)
        }
        HotkeyAction::FullDisplay => ActionRoute::FullDisplay,
        HotkeyAction::RepeatLastRegion => ActionRoute::RepeatLastRegion,
    }
}

#[cfg(windows)]
fn preview_metadata(
    capture_id: CaptureId,
    source: &CaptureSource,
    displays: &[DisplayInfo],
    mode: CaptureMode,
) -> Result<FrameMetadata, AppError> {
    let (display_id, source_rect, rotation_degrees) = match source {
        CaptureSource::Display(requested) => {
            let display = if requested.is_primary_alias() {
                displays
                    .iter()
                    .find(|display| display.is_primary)
                    .or_else(|| displays.first())
            } else {
                displays.iter().find(|display| display.id == *requested)
            }
            .ok_or_else(|| {
                AppError::BackendUnavailable(format!(
                    "display {} disappeared before live selection",
                    requested.0
                ))
            })?;
            (display.id.clone(), display.bounds, display.rotation_degrees)
        }
        CaptureSource::VirtualDesktop => {
            let first = displays.first().ok_or_else(|| {
                AppError::BackendUnavailable(
                    "no attached displays are available for live selection".to_owned(),
                )
            })?;
            let mut left = i64::from(first.bounds.x);
            let mut top = i64::from(first.bounds.y);
            let mut right = first.bounds.right();
            let mut bottom = first.bounds.bottom();
            for display in &displays[1..] {
                left = left.min(i64::from(display.bounds.x));
                top = top.min(i64::from(display.bounds.y));
                right = right.max(display.bounds.right());
                bottom = bottom.max(display.bounds.bottom());
            }
            let source_rect = Rect {
                x: i32::try_from(left).map_err(|_| {
                    AppError::BackendUnavailable("virtual desktop x origin overflowed".to_owned())
                })?,
                y: i32::try_from(top).map_err(|_| {
                    AppError::BackendUnavailable("virtual desktop y origin overflowed".to_owned())
                })?,
                width: u32::try_from(right.saturating_sub(left)).map_err(|_| {
                    AppError::BackendUnavailable("virtual desktop width overflowed".to_owned())
                })?,
                height: u32::try_from(bottom.saturating_sub(top)).map_err(|_| {
                    AppError::BackendUnavailable("virtual desktop height overflowed".to_owned())
                })?,
            };
            (DisplayId::virtual_desktop(), source_rect, 0)
        }
    };

    Ok(FrameMetadata {
        capture_id,
        backend: "selection-preview".to_owned(),
        display_id,
        source_rect,
        rotation_degrees,
        capture_mode: mode,
        presentation_offset_ns: None,
        timing_provenance: TimingProvenance::Unavailable,
        native_ready_offset_ns: 0,
        cpu_ready_offset_ns: None,
        frame_age_ns: None,
        frame_generation: None,
        copy_count: 0,
        pool_slot: None,
    })
}

#[cfg(windows)]
fn dispatch_live_selection(
    sender: &mpsc::SyncSender<crate::selection::SelectionJob>,
    capture_id: CaptureId,
    trigger: &TriggerEvent,
    route: ActionRoute,
    metadata: FrameMetadata,
    cached_ui: &UiConfig,
    recorder: EventRecorder,
) -> Result<&'static str, AppError> {
    let ActionRoute::Overlay(initial_tool) = route else {
        return Err(AppError::InvalidArgument(
            "live selection requires an overlay action".to_owned(),
        ));
    };
    let remembered_ui = Some(captastic_config::resolve_display_ui_state(
        cached_ui,
        &metadata.display_id.0,
    ));
    let job = crate::selection::SelectionJob {
        capture_id,
        triggered_at: trigger.received_at,
        action: trigger.action,
        chord: trigger.chord,
        initial_tool,
        cpu_ready_offset_ns: None,
        remembered_ui,
        source: trigger.source,
        metadata,
        frame: None,
        native_frame: None,
        recorder,
        confirmed_selection: None,
        terminal_error: None,
        selection_offset_ns: None,
        confirmation_anchored: true,
    };
    match crate::selection::try_submit(sender, job) {
        Ok(()) => Ok("live_selection_queued"),
        Err(crate::selection::SubmitError::Full(job)) => {
            let capture_id = crate::selection::finish_rejected(*job)?;
            crate::logging::warn(format_args!(
                "live selection {} skipped because the selection queue is full",
                capture_id.0
            ));
            Ok("selection_queue_full")
        }
        Err(crate::selection::SubmitError::Disconnected(job)) => {
            let capture_id = crate::selection::finish_rejected(*job)?;
            crate::logging::warn(format_args!(
                "live selection {} skipped because the selection worker stopped",
                capture_id.0
            ));
            Ok("selection_worker_disconnected")
        }
    }
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn dispatch_output(
    selection_sender: Option<&mpsc::SyncSender<crate::selection::SelectionJob>>,
    clipboard_sender: Option<&mpsc::SyncSender<crate::clipboard::ClipboardJob>>,
    capture_id: CaptureId,
    triggered_at: Instant,
    source: &'static str,
    action: HotkeyAction,
    chord: Option<HotkeyChord>,
    metadata: &FrameMetadata,
    confirmed_regions: &crate::selection::ConfirmedRegionCache,
    cached_ui: &UiConfig,
    json_output: bool,
    cpu_ready_offset_ns: Option<u64>,
    frame: Option<CpuFrame>,
    native_frame: Option<Arc<dyn NativeFrame>>,
    recorder: EventRecorder,
) -> Result<&'static str, AppError> {
    let route = action_route(action);
    if let ActionRoute::Overlay(initial_tool) = route {
        if let Some(sender) = selection_sender {
            return dispatch_selection(
                sender,
                capture_id,
                triggered_at,
                source,
                action,
                chord,
                initial_tool,
                Some(captastic_config::resolve_display_ui_state(
                    cached_ui,
                    &metadata.display_id.0,
                )),
                cpu_ready_offset_ns,
                frame,
                native_frame,
                recorder,
            );
        }
        if action != HotkeyAction::LastWorkflow {
            return Err(AppError::InvalidArgument(format!(
                "hotkey action {action} requires selection.enabled = true"
            )));
        }
    }

    if route == ActionRoute::RepeatLastRegion {
        let confirmed = confirmed_regions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&metadata.display_id.0)
            .copied();
        match repeat_region_rect(confirmed, metadata) {
            Ok(rect) => {
                return dispatch_repeat_region(
                    clipboard_sender,
                    capture_id,
                    triggered_at,
                    source,
                    action,
                    chord,
                    cpu_ready_offset_ns,
                    frame,
                    native_frame,
                    rect,
                    recorder,
                    json_output,
                );
            }
            Err(reason) => {
                crate::logging::warn(format_args!(
                    "capture {} action={} repeat_region_validation={} route=region_overlay display={}",
                    capture_id.0,
                    action,
                    reason,
                    metadata.display_id.0
                ));
                if json_output {
                    println!(
                        "{}",
                        json!({
                            "schema_version": 1,
                            "event": "repeat_region_fallback",
                            "capture_id": capture_id,
                            "action": action,
                            "display_id": metadata.display_id,
                            "reason": reason,
                            "route": "region_overlay",
                        })
                    );
                }
                let sender = selection_sender.ok_or_else(|| {
                    AppError::InvalidArgument(
                        "repeat_last_region fallback requires selection.enabled = true".to_owned(),
                    )
                })?;
                return dispatch_selection(
                    sender,
                    capture_id,
                    triggered_at,
                    source,
                    action,
                    chord,
                    captastic_windows::InitialSelectionTool::Region,
                    Some(captastic_config::resolve_display_ui_state(
                        cached_ui,
                        &metadata.display_id.0,
                    )),
                    cpu_ready_offset_ns,
                    frame,
                    native_frame,
                    recorder,
                );
            }
        }
    }

    dispatch_clipboard(
        clipboard_sender,
        capture_id,
        triggered_at,
        source,
        action,
        chord,
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
    action: HotkeyAction,
    chord: Option<HotkeyChord>,
    initial_tool: captastic_windows::InitialSelectionTool,
    remembered_ui: Option<captastic_config::DisplayUiState>,
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
        action,
        chord,
        initial_tool,
        remembered_ui,
        cpu_ready_offset_ns: Some(cpu_ready_offset_ns),
        source,
        metadata: frame.metadata.clone(),
        frame: Some(frame),
        native_frame,
        recorder,
        confirmed_selection: None,
        terminal_error: None,
        selection_offset_ns: None,
        confirmation_anchored: false,
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
#[allow(clippy::too_many_arguments)]
fn dispatch_repeat_region(
    clipboard_sender: Option<&mpsc::SyncSender<crate::clipboard::ClipboardJob>>,
    capture_id: CaptureId,
    triggered_at: Instant,
    source: &'static str,
    action: HotkeyAction,
    chord: Option<HotkeyChord>,
    cpu_ready_offset_ns: Option<u64>,
    frame: Option<CpuFrame>,
    native_frame: Option<Arc<dyn NativeFrame>>,
    rect: Rect,
    mut recorder: EventRecorder,
    json_output: bool,
) -> Result<&'static str, AppError> {
    let frame = frame.ok_or_else(|| {
        AppError::BackendUnavailable(
            "repeat_last_region requires a CPU frame for checked fallback".to_owned(),
        )
    })?;
    let materialize_started = Instant::now();
    let mut materialization = "cpu_region";
    let mut gpu_fallback_error = None;
    let gpu_result = native_frame
        .as_deref()
        .map(|native| captastic_windows::materialize_native_region(native, rect))
        .transpose();
    let selected_frame = match gpu_result {
        Ok(Some(Some(result))) => {
            materialization = "dxgi_gpu_region";
            result.frame
        }
        Ok(Some(None)) | Ok(None) => frame
            .crop(rect)
            .map_err(|error| AppError::BackendUnavailable(error.to_string()))?,
        Err(error) => {
            gpu_fallback_error = Some(error.to_string());
            crate::logging::warn(format_args!(
                "capture {} action={} GPU repeat-region materialization failed; using CPU crop: {error}",
                capture_id.0,
                action
            ));
            frame
                .crop(rect)
                .map_err(|error| AppError::BackendUnavailable(error.to_string()))?
        }
    };
    let materialization_ns = duration_ns(materialize_started, Instant::now());
    recorder.record(capture_id, PerfEventKind::CropFinished, materialization_ns);
    log::info!(
        "capture {} action={} route=direct materialization={} rect={}x{} at ({}, {})",
        capture_id.0,
        action,
        materialization,
        rect.width,
        rect.height,
        rect.x,
        rect.y
    );
    if json_output {
        println!(
            "{}",
            json!({
                "schema_version": 1,
                "event": "repeat_region_materialized",
                "capture_id": capture_id,
                "action": action,
                "route": "direct",
                "rect": rect,
                "materialization": materialization,
                "materialization_ns": materialization_ns,
                "gpu_fallback_error": gpu_fallback_error,
            })
        );
    }
    dispatch_clipboard(
        clipboard_sender,
        capture_id,
        triggered_at,
        source,
        action,
        chord,
        cpu_ready_offset_ns,
        Some(selected_frame),
        recorder,
    )
}

#[cfg(windows)]
fn repeat_region_rect(
    confirmed: Option<ConfirmedRegion>,
    metadata: &FrameMetadata,
) -> Result<Rect, &'static str> {
    let confirmed = confirmed.ok_or("missing_confirmed_region")?;
    let source = metadata.source_rect;
    if confirmed.source.width != source.width
        || confirmed.source.height != source.height
        || confirmed.source.rotation_degrees != metadata.rotation_degrees
    {
        return Err("source_geometry_changed");
    }
    let region = confirmed.region;
    if region.x < 0 || region.y < 0 || region.width == 0 || region.height == 0 {
        return Err("invalid_region_bounds");
    }
    let right = i64::from(region.x) + i64::from(region.width);
    let bottom = i64::from(region.y) + i64::from(region.height);
    if right > i64::from(source.width) || bottom > i64::from(source.height) {
        return Err("invalid_region_bounds");
    }
    let x = source
        .x
        .checked_add(region.x)
        .ok_or("invalid_region_bounds")?;
    let y = source
        .y
        .checked_add(region.y)
        .ok_or("invalid_region_bounds")?;
    Ok(Rect {
        x,
        y,
        width: region.width,
        height: region.height,
    })
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn dispatch_clipboard(
    sender: Option<&mpsc::SyncSender<crate::clipboard::ClipboardJob>>,
    capture_id: CaptureId,
    triggered_at: Instant,
    source: &'static str,
    action: HotkeyAction,
    chord: Option<HotkeyChord>,
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
        action,
        chord,
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
pub(crate) enum CaptureCommand {
    Trigger(TriggerEvent),
    LiveSelection(Box<crate::selection::LiveSelectionRequest>),
    Shutdown,
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct TriggerEvent {
    received_at: Instant,
    enqueued_at: Instant,
    source: &'static str,
    action: HotkeyAction,
    chord: Option<HotkeyChord>,
}

#[cfg(windows)]
fn trigger_recorder(capture_id: CaptureId, trigger: &TriggerEvent) -> EventRecorder {
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
    recorder
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
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use captastic_core::{
        CaptureMode, CaptureSource, ColorSpace, FakeBackend, FakeBackendConfig, FakeFailure,
        FrameMetadata, FrameOrigin, PixelFormat, Rect, TimingProvenance,
    };

    use super::*;

    struct TempConfig {
        directory: PathBuf,
        path: PathBuf,
    }

    static NEXT_TEMP_CONFIG: AtomicU64 = AtomicU64::new(0);

    fn preview_display(id: &str, bounds: Rect, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds,
            scale_factor: 1.0,
            rotation_degrees: 0,
            is_primary: primary,
        }
    }

    #[test]
    fn live_preview_metadata_preserves_display_identity_and_virtual_bounds() {
        let displays = [
            preview_display(
                "left",
                Rect {
                    x: -1280,
                    y: 0,
                    width: 1280,
                    height: 1024,
                },
                false,
            ),
            preview_display(
                "primary-id",
                Rect {
                    x: 0,
                    y: -200,
                    width: 1920,
                    height: 1080,
                },
                true,
            ),
        ];
        let mode = CaptureMode::Latest { max_age_ms: None };

        let primary = preview_metadata(
            CaptureId(1),
            &CaptureSource::Display(DisplayId::primary()),
            &displays,
            mode.clone(),
        )
        .expect("primary preview metadata");
        assert_eq!(primary.display_id.0, "primary-id");
        assert_eq!(primary.source_rect, displays[1].bounds);

        let desktop = preview_metadata(
            CaptureId(2),
            &CaptureSource::VirtualDesktop,
            &displays,
            mode,
        )
        .expect("virtual preview metadata");
        assert_eq!(desktop.display_id, DisplayId::virtual_desktop());
        assert_eq!(
            desktop.source_rect,
            Rect {
                x: -1280,
                y: -200,
                width: 3200,
                height: 1224,
            }
        );
    }

    impl TempConfig {
        fn with_contents(contents: &str) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "captastic-daemon-test-{}-{}",
                std::process::id(),
                NEXT_TEMP_CONFIG.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&directory).expect("create temporary config directory");
            let path = directory.join("captastic.toml");
            fs::write(&path, contents).expect("write temporary config");
            Self { directory, path }
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn explicit_config_remains_strict_and_is_never_quarantined() {
        let config = TempConfig::with_contents("capture_mod = 'latest'\n");
        let args = DaemonArgs {
            config: Some(config.path.clone()),
            ..DaemonArgs::default()
        };

        let error = match resolve_daemon_args_with_default(args, || {
            panic!("explicit configuration must not invoke the recovering default loader")
        }) {
            Ok(_) => panic!("unknown explicit field must fail strictly"),
            Err(error) => error,
        };

        assert!(matches!(error, AppError::Config(_)));
        assert!(
            config.path.exists(),
            "the explicit file must remain in place"
        );
        assert_eq!(
            fs::read_dir(&config.directory)
                .expect("list temporary config directory")
                .count(),
            1,
            "strict loading must not create a quarantine file"
        );
    }

    #[test]
    fn tray_open_config_reports_a_missing_explicit_path() {
        let config = TempConfig::with_contents("schema_version = 1\n");
        fs::remove_file(&config.path).expect("remove explicit config");
        let store = captastic_config::UiStateStore::for_config(&config.path);

        let error = open_config_from_tray(&store).expect_err("missing explicit config must fail");

        assert!(error.contains("does not exist"));
        assert!(!config.path.exists());
    }

    #[test]
    fn missing_config_argument_uses_the_recovering_default_loader() {
        let original_path = PathBuf::from("damaged-default.toml");
        let quarantined_path = PathBuf::from("damaged-default.toml.corrupt-test");
        let resolved = resolve_daemon_args_with_default(DaemonArgs::default(), || {
            Ok((
                AppConfig::default(),
                Some(captastic_config::ConfigRecovery {
                    original_path,
                    quarantined_path,
                    reason: "test syntax damage".to_owned(),
                }),
            ))
        })
        .expect("default recovery must permit daemon argument resolution");

        assert!(!resolved.backend.is_empty());
        assert_eq!(resolved.startup_warnings.len(), 1);
        assert!(resolved.startup_warnings[0].contains("quarantined"));
    }

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
    fn scripted_access_loss_rebuilds_backend_and_retries_capture() {
        let mut backend: Option<Box<dyn CaptureBackend>> =
            Some(Box::new(FakeBackend::new(FakeBackendConfig {
                failure_script: vec![FakeFailure::new(1, CaptureErrorKind::AccessLost, true)],
                ..FakeBackendConfig::default()
            })));
        let request = CaptureRequest {
            id: CaptureId(1),
            triggered_at: Instant::now(),
            source: CaptureSource::Display(DisplayId::primary()),
            mode: CaptureMode::Latest { max_age_ms: None },
            cpu_frame: true,
            retain_native_frame: false,
            cursor: CursorMode::Exclude,
        };
        let mut rebuilds = 0_u32;
        let mut waits = Vec::new();
        let mut retry_kinds = Vec::new();

        let (result, recorder, attempts, rebuild_error) = capture_with_backend_recovery(
            &mut backend,
            |active_backend| {
                let mut recorder = EventRecorder::with_capacity(8);
                let result = active_backend.capture(&request, &mut recorder);
                (result, recorder)
            },
            || {
                rebuilds = rebuilds.saturating_add(1);
                Ok(Box::new(FakeBackend::new(FakeBackendConfig::default())))
            },
            |delay| waits.push(delay),
            |_, _, error| retry_kinds.push(error.kind),
        );

        assert!(result.is_ok());
        validate_event_order(recorder.events()).expect("successful retry event order");
        assert_eq!(attempts, 1);
        assert_eq!(rebuilds, 1);
        assert_eq!(waits, vec![Duration::from_millis(50)]);
        assert_eq!(retry_kinds, vec![CaptureErrorKind::AccessLost]);
        assert!(rebuild_error.is_none());
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
            HotkeyAction::FullDisplay,
            None,
            Some(2),
            Some(frame),
            recorder,
        )
        .expect("capture remains successful");
        assert_eq!(status, "worker_disconnected");
    }
    #[test]
    fn every_hotkey_action_has_exactly_one_route() {
        assert_eq!(
            action_route(HotkeyAction::LastWorkflow),
            ActionRoute::Overlay(captastic_windows::InitialSelectionTool::Remembered)
        );
        assert_eq!(
            action_route(HotkeyAction::Region),
            ActionRoute::Overlay(captastic_windows::InitialSelectionTool::Region)
        );
        assert_eq!(
            action_route(HotkeyAction::Window),
            ActionRoute::Overlay(captastic_windows::InitialSelectionTool::Window)
        );
        assert_eq!(
            action_route(HotkeyAction::FullDisplay),
            ActionRoute::FullDisplay
        );
        assert_eq!(
            action_route(HotkeyAction::RepeatLastRegion),
            ActionRoute::RepeatLastRegion
        );
        assert!(!action_requires_selection(HotkeyAction::LastWorkflow));
        assert!(!action_requires_selection(HotkeyAction::FullDisplay));
        assert!(action_requires_selection(HotkeyAction::Region));
        assert!(action_requires_selection(HotkeyAction::Window));
        assert!(action_requires_selection(HotkeyAction::RepeatLastRegion));
    }

    fn repeat_metadata(source_rect: Rect) -> FrameMetadata {
        FrameMetadata {
            capture_id: CaptureId(42),
            backend: "test".to_owned(),
            display_id: DisplayId("display-a".to_owned()),
            source_rect,
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
        }
    }

    #[test]
    fn repeat_region_validates_source_and_preserves_negative_display_origins() {
        let metadata = repeat_metadata(Rect {
            x: -1920,
            y: -240,
            width: 3840,
            height: 2160,
        });
        let confirmed = ConfirmedRegion {
            region: captastic_config::CaptureRegion {
                x: 40,
                y: 20,
                width: 800,
                height: 600,
            },
            source: captastic_config::CaptureRegionSource {
                width: 3840,
                height: 2160,
                rotation_degrees: 0,
            },
        };
        assert_eq!(
            repeat_region_rect(Some(confirmed), &metadata),
            Ok(Rect {
                x: -1880,
                y: -220,
                width: 800,
                height: 600,
            })
        );
    }

    #[test]
    fn missing_stale_and_out_of_bounds_repeat_regions_fall_back() {
        let metadata = repeat_metadata(Rect {
            x: 0,
            y: 0,
            width: 3840,
            height: 2160,
        });
        assert_eq!(
            repeat_region_rect(None, &metadata),
            Err("missing_confirmed_region")
        );
        let stale = ConfirmedRegion {
            region: captastic_config::CaptureRegion {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
            source: captastic_config::CaptureRegionSource {
                width: 1920,
                height: 1080,
                rotation_degrees: 0,
            },
        };
        assert_eq!(
            repeat_region_rect(Some(stale), &metadata),
            Err("source_geometry_changed")
        );
        let outside = ConfirmedRegion {
            region: captastic_config::CaptureRegion {
                x: 3800,
                y: 2100,
                width: 100,
                height: 100,
            },
            source: captastic_config::CaptureRegionSource {
                width: 3840,
                height: 2160,
                rotation_degrees: 0,
            },
        };
        assert_eq!(
            repeat_region_rect(Some(outside), &metadata),
            Err("invalid_region_bounds")
        );
    }
}
