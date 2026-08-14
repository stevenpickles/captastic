#![deny(unsafe_code)]

mod benchmark;
mod build_info;
mod cli;
#[cfg(windows)]
mod clipboard;
mod daemon;
mod error;
mod logging;
#[cfg(windows)]
mod selection;

use std::process;
use std::time::Instant;

use captastic_config::{AppConfig, LoggingConfig};
use captastic_core::{
    CaptureBackend, CaptureId, CaptureMode, CaptureRequest, CaptureSource, CursorMode, DisplayId,
    EventRecorder, PerfEventKind,
};
use clap::Parser;
#[cfg(windows)]
use cli::PreviewArg;
use cli::{BenchmarkArgs, Cli, Command, ConfigCommand, ModeArg, StartupCommand};
use error::AppError;
use serde_json::json;

#[derive(Clone, Debug, Eq, PartialEq)]
enum DisplayPolicy {
    Pointer,
    Primary,
    Fixed(DisplayId),
    VirtualDesktop,
}

impl DisplayPolicy {
    #[cfg(windows)]
    fn as_config_value(&self) -> String {
        match self {
            Self::Pointer => "pointer".to_owned(),
            Self::Primary => "primary".to_owned(),
            Self::Fixed(id) => format!("display:{}", id.0),
            Self::VirtualDesktop => "virtual_desktop".to_owned(),
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let logging_config = resolve_logging_config(&cli);
    let persistent_logging = uses_persistent_logging(&cli);
    let logging_result = if persistent_logging {
        logging::init(&logging_config).map(Some)
    } else {
        logging::init_console(&logging_config).map(|()| None)
    };
    let logging_available = match logging_result {
        Ok(path) => {
            if let Some(path) = path {
                log::info!(
                    "Captastic {} started; persistent log file is {}",
                    build_info::BUILD_VERSION,
                    path.display()
                );
            }
            true
        }
        Err(error) => {
            eprintln!("captastic: persistent logging unavailable: {error}");
            false
        }
    };
    if let Err(error) = run(cli) {
        if logging_available {
            log::error!("Captastic stopped with an error: {error}");
            log::logger().flush();
        } else {
            eprintln!("captastic: {error}");
        }
        process::exit(error.exit_code());
    }
    if logging_available {
        log::info!("Captastic stopped successfully");
        log::logger().flush();
    }
}

fn uses_persistent_logging(cli: &Cli) -> bool {
    cli.log_file.is_some()
        || matches!(
            cli.command.as_ref(),
            None | Some(Command::Daemon(_) | Command::Capture(_) | Command::Benchmark(_))
        )
}

fn resolve_logging_config(cli: &Cli) -> LoggingConfig {
    let mut logging = match &cli.command {
        Some(Command::Daemon(args)) => {
            let config = match args.config.as_deref() {
                Some(path) => AppConfig::load(path),
                None => AppConfig::load_default(),
            };
            config.map_or_else(|_| LoggingConfig::default(), |config| config.logging)
        }
        None => AppConfig::load_default()
            .map_or_else(|_| LoggingConfig::default(), |config| config.logging),
        _ => AppConfig::load_default()
            .map_or_else(|_| LoggingConfig::default(), |config| config.logging),
    };
    if let Some(path) = &cli.log_file {
        logging.file = Some(path.clone());
    }
    if let Some(level) = &cli.log_level {
        logging.level.clone_from(level);
    }
    if let Some(format) = &cli.log_format {
        logging.format.clone_from(format);
    }
    logging
}

fn run(cli: Cli) -> Result<(), AppError> {
    match cli.command.unwrap_or_default() {
        Command::Daemon(args) => daemon::run(args),
        Command::Status { json } => status(json),
        Command::Stop => stop(),
        Command::Displays { backend, json } => {
            let displays = enumerate_displays(&backend)?;
            print_value(json, &displays)
        }
        Command::Capture(args) => capture(args),
        Command::Benchmark(args) => benchmark(args),
        Command::Version { json } => version(json),
        Command::Doctor { json } => doctor(json),
        Command::Startup { command } => startup(command),
        Command::Config { command } => config(command),
    }
}

fn version(json_output: bool) -> Result<(), AppError> {
    print_value(json_output, &build_info::BUILD_INFO)
}

#[cfg(windows)]
fn startup(command: StartupCommand) -> Result<(), AppError> {
    match command {
        StartupCommand::Enable => {
            let launcher = desktop_launcher_path()?;
            captastic_windows::enable_startup(&launcher)?;
            println!("Captastic will start with Windows");
            Ok(())
        }
        StartupCommand::Disable => {
            if captastic_windows::disable_startup()? {
                println!("Captastic will no longer start with Windows");
            } else {
                println!("Captastic startup was already disabled");
            }
            Ok(())
        }
        StartupCommand::Status { json } => {
            let command = captastic_windows::startup_command()?;
            print_value(
                json,
                &json!({
                    "schema_version": 1,
                    "enabled": command.is_some(),
                    "command": command,
                }),
            )
        }
    }
}

#[cfg(not(windows))]
fn startup(_command: StartupCommand) -> Result<(), AppError> {
    Err(AppError::BackendUnavailable(
        "launch at login is currently available only on Windows".to_owned(),
    ))
}

#[cfg(windows)]
fn desktop_launcher_path() -> Result<std::path::PathBuf, AppError> {
    let mut path = std::env::current_exe().map_err(|error| {
        AppError::BackendUnavailable(format!("failed to locate Captastic executable: {error}"))
    })?;
    path.set_file_name("captastic-desktop.exe");
    Ok(path)
}

#[cfg(windows)]
fn status(json_output: bool) -> Result<(), AppError> {
    let running = captastic_windows::DaemonControl::is_running();
    print_value(
        json_output,
        &json!({
            "schema_version": 1,
            "status": if running { "running" } else { "not_running" },
        }),
    )
}

#[cfg(not(windows))]
fn status(json_output: bool) -> Result<(), AppError> {
    print_value(
        json_output,
        &json!({
            "schema_version": 1,
            "status": "unsupported",
            "reason": "the native hotkey daemon is currently available only on Windows",
        }),
    )
}

#[cfg(windows)]
fn stop() -> Result<(), AppError> {
    if captastic_windows::DaemonControl::request_stop()? {
        println!("Captastic daemon stop requested");
    } else {
        println!("Captastic daemon is not running");
    }
    Ok(())
}

#[cfg(not(windows))]
fn stop() -> Result<(), AppError> {
    Err(AppError::BackendUnavailable(
        "daemon control is currently available only on Windows".to_owned(),
    ))
}

fn capture(args: cli::CaptureArgs) -> Result<(), AppError> {
    capture_with_preview_fallback(args, None)
}

fn capture_with_preview_fallback(
    args: cli::CaptureArgs,
    preview_fallback_reason: Option<String>,
) -> Result<(), AppError> {
    #[cfg(not(windows))]
    let _ = preview_fallback_reason;
    if (args.selection || args.clipboard) && !args.cpu_frame {
        return Err(AppError::InvalidArgument(
            "selection and clipboard output require --cpu-frame true".to_owned(),
        ));
    }
    #[cfg(windows)]
    if args.selection && args.selection_preview != PreviewArg::Frozen {
        return capture_with_live_selection(args);
    }
    let display_policy = resolve_display_policy(&args.display)?;
    let mut backend = create_backend(&args.backend, &display_policy)?;
    let source = resolve_capture_source(&display_policy, backend.displays())?;
    let mut recorder = EventRecorder::with_capacity(16);
    let request = CaptureRequest {
        id: CaptureId(1),
        triggered_at: Instant::now(),
        source,
        mode: capture_mode(args.mode),
        cpu_frame: args.cpu_frame,
        retain_native_frame: args.selection,
        cursor: CursorMode::Exclude,
    };
    recorder.record(request.id, PerfEventKind::HotkeyReceived, 0);
    recorder.record(request.id, PerfEventKind::TriggerEnqueued, 0);
    recorder.record(request.id, PerfEventKind::TriggerDequeued, 0);
    let outcome = backend.capture(&request, &mut recorder)?;
    let frame = outcome.frame;
    #[cfg(windows)]
    let mut frame = frame;
    let native_frame = outcome.native_frame;
    #[cfg(windows)]
    let mut selection_value = None;
    #[cfg(not(windows))]
    let selection_value: Option<serde_json::Value> = None;
    // The UI-state worker outlives the overlay so that persisting selection preferences never
    // stands between a confirmed selection and the capture it produces.
    #[cfg(windows)]
    let mut ui_worker: Option<selection::OneShotUiStateWorker> = None;
    if args.selection {
        #[cfg(windows)]
        {
            let full_frame = frame.take().ok_or_else(|| {
                AppError::BackendUnavailable(
                    "selection was requested but no CPU frame was returned".to_owned(),
                )
            })?;
            recorder.record(request.id, PerfEventKind::SelectionStarted, 0);
            let ui_store = captastic_config::UiStateStore::for_default_config();
            let remembered_ui =
                load_optional_one_shot_ui_state(&ui_store, &full_frame.metadata.display_id.0);
            let worker = selection::OneShotUiStateWorker::start(ui_store)?;
            // One-shot: the resources live for this single run and drop with it.
            let mut overlay_resources = captastic_windows::OverlayResources::new();
            let selection = captastic_windows::select_from_frozen_frame_with_initial_tool_and_ui(
                &full_frame,
                worker.controller(),
                captastic_windows::InitialSelectionTool::Remembered,
                Some(remembered_ui),
                &mut overlay_resources,
            )?;
            let Some(selection) = selection else {
                finish_one_shot_ui_state(Some(worker));
                recorder.record(request.id, PerfEventKind::AttemptFinished, 0);
                captastic_core::validate_event_order(recorder.events())?;
                let value = json!({
                    "schema_version": 1,
                    "event": "selection_cancelled",
                    "capture_id": request.id,
                });
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    log::info!("selection {} cancelled", request.id.0);
                }
                return Ok(());
            };
            ui_worker = Some(worker);
            recorder.record(
                request.id,
                PerfEventKind::SelectionConfirmed,
                selection.selection_ns,
            );
            let materialize_started = Instant::now();
            let mut materialization = match selection.kind {
                captastic_windows::SelectionKind::Display => "frozen_display",
                captastic_windows::SelectionKind::Region => "frozen_desktop_crop",
                captastic_windows::SelectionKind::Window => "native_window_render",
            };
            let mut gpu_materialization = None;
            let mut gpu_fallback_error = None;
            let gpu_result = if selection.kind == captastic_windows::SelectionKind::Region {
                native_frame.as_deref().map(|native_frame| {
                    captastic_windows::materialize_native_region(native_frame, selection.rect)
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
                    captastic_windows::materialize_selection(&full_frame, &selection)?
                }
                Err(error) => {
                    crate::logging::warn(format_args!(
                        "capture {} GPU region materialization failed; using CPU crop: {error}",
                        request.id.0
                    ));
                    gpu_fallback_error = Some(error.to_string());
                    captastic_windows::materialize_selection(&full_frame, &selection)?
                }
            };
            let materialization_ns = duration_ns(materialize_started.elapsed());
            recorder.record(request.id, PerfEventKind::CropFinished, materialization_ns);
            selection_value = Some(json!({
                "kind": match selection.kind {
                    captastic_windows::SelectionKind::Display => "display",
                    captastic_windows::SelectionKind::Region => "region",
                    captastic_windows::SelectionKind::Window => "window",
                },
                "rect": selection.rect,
                "selection_ns": selection.selection_ns,
                "requested_preview_mode": if preview_fallback_reason.is_some() {
                    "auto"
                } else {
                    match args.selection_preview {
                        PreviewArg::Auto => "auto",
                        PreviewArg::Live => "live",
                        PreviewArg::Frozen => "frozen",
                    }
                },
                "preview_mode": "frozen",
                "preview_fallback_reason": preview_fallback_reason.as_deref(),
                "capture_anchor": if preview_fallback_reason.is_some() { "fallback" } else { "trigger" },
                "overlay_preparation_ns": selection.preparation_ns,
                "window_overview_ns": selection.window_overview_ns,
                "window_preview_count": selection.window_preview_count,
                "window_live_preview_count": selection.window_live_preview_count,
                "window_frozen_preview_count": selection.window_frozen_preview_count,
                "window_preview_bytes": selection.window_preview_bytes,
                "materialization": materialization,
                "materialization_ns": materialization_ns,
                "gpu_materialization": gpu_materialization,
                "gpu_fallback_error": gpu_fallback_error,
            }));
            frame = Some(selected_frame);
        }
        #[cfg(not(windows))]
        {
            return Err(AppError::BackendUnavailable(
                "native selection is currently available only on Windows".to_owned(),
            ));
        }
    }
    #[cfg(windows)]
    let mut clipboard_value = None;
    #[cfg(not(windows))]
    let clipboard_value: Option<serde_json::Value> = None;
    if args.clipboard {
        #[cfg(windows)]
        {
            let clipboard_frame = frame.as_ref().ok_or_else(|| {
                AppError::BackendUnavailable(
                    "clipboard output was requested but no CPU frame was returned".to_owned(),
                )
            })?;
            recorder.record(request.id, PerfEventKind::ClipboardStarted, 0);
            let report = captastic_windows::ClipboardPublisher::new()?.publish(clipboard_frame)?;
            recorder.record(
                request.id,
                PerfEventKind::ClipboardCommitted,
                report.publish_ns,
            );
            clipboard_value = Some(json!({
                "payload_bytes": report.payload_bytes,
                "png_payload_bytes": report.png_payload_bytes,
                "png_encode_ns": report.png_encode_ns,
                "allocation_copy_ns": report.allocation_copy_ns,
                "open_wait_ns": report.open_wait_ns,
                "open_retries": report.open_retries,
                "publish_ns": report.publish_ns,
            }));
        }
        #[cfg(not(windows))]
        {
            return Err(AppError::BackendUnavailable(
                "native clipboard output is currently available only on Windows".to_owned(),
            ));
        }
    }
    recorder.record(request.id, PerfEventKind::AttemptFinished, 0);
    captastic_core::validate_event_order(recorder.events())?;
    let metadata = frame
        .as_ref()
        .map(|frame| frame.metadata.clone())
        .unwrap_or(outcome.metadata);
    let value = json!({
        "schema_version": 1,
        "synthetic": backend.name() == "fake",
        "metadata": metadata,
        "cpu_frame_bytes": frame.as_ref().map(|frame| frame.required_bytes()),
        "native_frame_retained": native_frame.is_some(),
        "selection": selection_value,
        "clipboard": clipboard_value,
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        log::info!("capture {} complete: {}", request.id.0, value);
    }
    #[cfg(windows)]
    finish_one_shot_ui_state(ui_worker);
    Ok(())
}

#[cfg(windows)]
fn capture_with_live_selection(mut args: cli::CaptureArgs) -> Result<(), AppError> {
    let display_policy = resolve_display_policy(&args.display)?;
    let mut backend = create_backend(&args.backend, &display_policy)?;
    let source = resolve_capture_source(&display_policy, backend.displays())?;
    let capture_id = CaptureId(1);
    let triggered_at = Instant::now();
    let mode = capture_mode(args.mode);
    let metadata = daemon::preview_metadata(capture_id, &source, backend.displays(), mode.clone())?;
    let mut recorder = EventRecorder::with_capacity(16);
    recorder.record(capture_id, PerfEventKind::HotkeyReceived, 0);
    recorder.record(capture_id, PerfEventKind::TriggerEnqueued, 0);
    recorder.record(capture_id, PerfEventKind::TriggerDequeued, 0);

    let ui_store = captastic_config::UiStateStore::for_default_config();
    let remembered_ui = load_optional_one_shot_ui_state(&ui_store, &metadata.display_id.0);
    let ui_worker = selection::OneShotUiStateWorker::start(ui_store)?;
    // One-shot: fresh resources per attempt. A live-presenter failure falls back into
    // capture_with_preview_fallback, whose frozen attempt allocates its own set - the old
    // thread_local handed the failed attempt's surfaces across that boundary, an optimization
    // this ownership model deliberately gives up on the rare error path.
    let mut overlay_resources = captastic_windows::OverlayResources::new();
    let selection_result = captastic_windows::select_from_preview_source_with_initial_tool_and_ui(
        captastic_windows::SelectionPreviewSource::live(&metadata),
        ui_worker.controller(),
        captastic_windows::InitialSelectionTool::Remembered,
        Some(remembered_ui),
        &mut overlay_resources,
    );
    let selection = match selection_result {
        Ok(selection) => selection,
        Err(error) if args.selection_preview == PreviewArg::Auto => {
            // The fallback capture starts its own worker, so this one must be retired first.
            finish_one_shot_ui_state(Some(ui_worker));
            crate::logging::warn(format_args!(
                "one-shot live presenter failed; retrying with a frozen preview: {error}"
            ));
            let reason = error.to_string();
            args.selection_preview = PreviewArg::Frozen;
            return capture_with_preview_fallback(args, Some(reason));
        }
        Err(error) => {
            finish_one_shot_ui_state(Some(ui_worker));
            return Err(error.into());
        }
    };
    recorder.record(capture_id, PerfEventKind::SelectionStarted, 0);
    let Some(selection) = selection else {
        finish_one_shot_ui_state(Some(ui_worker));
        recorder.record(capture_id, PerfEventKind::AttemptFinished, 0);
        captastic_core::validate_event_order(recorder.events())?;
        let value = json!({
            "schema_version": 1,
            "event": "selection_cancelled",
            "capture_id": capture_id,
        });
        if args.json {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            log::info!("selection {} cancelled", capture_id.0);
        }
        return Ok(());
    };
    recorder.record(
        capture_id,
        PerfEventKind::SelectionConfirmed,
        selection.selection_ns,
    );

    let (full_frame, native_frame) = if selection.kind == captastic_windows::SelectionKind::Window {
        let frame = captastic_windows::captured_window_frame(&selection).ok_or_else(|| {
            AppError::BackendUnavailable(
                "confirmed window selection did not retain its native frame".to_owned(),
            )
        })?;
        let ready_offset_ns = duration_ns(triggered_at.elapsed());
        recorder.record(capture_id, PerfEventKind::CaptureRequested, ready_offset_ns);
        recorder.record(capture_id, PerfEventKind::NativeFrameReady, ready_offset_ns);
        recorder.record(capture_id, PerfEventKind::CpuFrameReady, ready_offset_ns);
        (frame, None)
    } else {
        if let Err(error) = captastic_windows::flush_desktop_composition() {
            log::warn!(
                "one-shot selection could not synchronize overlay removal before capture: {error}"
            );
        }
        let request = CaptureRequest {
            id: capture_id,
            triggered_at: Instant::now(),
            source,
            mode,
            cpu_frame: true,
            retain_native_frame: selection.kind == captastic_windows::SelectionKind::Region,
            cursor: CursorMode::Exclude,
        };
        let outcome = backend.capture(&request, &mut recorder)?;
        let frame = outcome.frame.ok_or_else(|| {
            AppError::BackendUnavailable("confirmation capture returned no CPU frame".to_owned())
        })?;
        (frame, outcome.native_frame)
    };

    let materialize_started = Instant::now();
    let mut materialization = match selection.kind {
        captastic_windows::SelectionKind::Display => "confirmation_display",
        captastic_windows::SelectionKind::Region => "confirmation_desktop_crop",
        captastic_windows::SelectionKind::Window => "native_window_render",
    };
    let mut gpu_materialization = None;
    let mut gpu_fallback_error = None;
    let gpu_result = if selection.kind == captastic_windows::SelectionKind::Region {
        native_frame.as_deref().map(|native_frame| {
            captastic_windows::materialize_native_region(native_frame, selection.rect)
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
            captastic_windows::materialize_selection(&full_frame, &selection)?
        }
        Err(error) => {
            crate::logging::warn(format_args!(
                "capture {} GPU region materialization failed; using CPU crop: {error}",
                capture_id.0
            ));
            gpu_fallback_error = Some(error.to_string());
            captastic_windows::materialize_selection(&full_frame, &selection)?
        }
    };
    let materialization_ns = duration_ns(materialize_started.elapsed());
    recorder.record(capture_id, PerfEventKind::CropFinished, materialization_ns);
    let selection_value = json!({
        "kind": match selection.kind {
            captastic_windows::SelectionKind::Display => "display",
            captastic_windows::SelectionKind::Region => "region",
            captastic_windows::SelectionKind::Window => "window",
        },
        "rect": selection.rect,
        "selection_ns": selection.selection_ns,
        "requested_preview_mode": match args.selection_preview {
            PreviewArg::Auto => "auto",
            PreviewArg::Live => "live",
            PreviewArg::Frozen => "frozen",
        },
        "preview_mode": "live",
        "capture_anchor": "confirmation",
        "overlay_preparation_ns": selection.preparation_ns,
        "window_overview_ns": selection.window_overview_ns,
        "window_preview_count": selection.window_preview_count,
        "window_live_preview_count": selection.window_live_preview_count,
        "window_frozen_preview_count": selection.window_frozen_preview_count,
        "window_preview_bytes": selection.window_preview_bytes,
        "materialization": materialization,
        "materialization_ns": materialization_ns,
        "gpu_materialization": gpu_materialization,
        "gpu_fallback_error": gpu_fallback_error,
    });

    let mut clipboard_value = None;
    if args.clipboard {
        recorder.record(capture_id, PerfEventKind::ClipboardStarted, 0);
        let report = captastic_windows::ClipboardPublisher::new()?.publish(&selected_frame)?;
        recorder.record(
            capture_id,
            PerfEventKind::ClipboardCommitted,
            report.publish_ns,
        );
        clipboard_value = Some(json!({
            "payload_bytes": report.payload_bytes,
            "png_payload_bytes": report.png_payload_bytes,
            "png_encode_ns": report.png_encode_ns,
            "allocation_copy_ns": report.allocation_copy_ns,
            "open_wait_ns": report.open_wait_ns,
            "open_retries": report.open_retries,
            "publish_ns": report.publish_ns,
        }));
    }
    recorder.record(capture_id, PerfEventKind::AttemptFinished, 0);
    captastic_core::validate_event_order(recorder.events())?;
    let value = json!({
        "schema_version": 1,
        "synthetic": backend.name() == "fake",
        "metadata": selected_frame.metadata,
        "cpu_frame_bytes": selected_frame.required_bytes(),
        "native_frame_retained": native_frame.is_some(),
        "selection": selection_value,
        "clipboard": clipboard_value,
    });
    if args.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        log::info!("capture {} complete: {}", capture_id.0, value);
    }
    finish_one_shot_ui_state(Some(ui_worker));
    Ok(())
}

/// Retires the one-shot UI-state worker without letting a preference write failure destroy the
/// capture it produced. The daemon treats the same failure as a non-fatal notification, so the
/// one-shot command reports it and still exits successfully.
#[cfg(windows)]
fn finish_one_shot_ui_state(worker: Option<selection::OneShotUiStateWorker>) {
    let Some(worker) = worker else {
        return;
    };
    if let Err(error) = worker.finish() {
        crate::logging::warn(format_args!("{}", one_shot_ui_state_warning(&error)));
    }
}

#[cfg(windows)]
fn one_shot_ui_state_warning(error: &AppError) -> String {
    format!("Captastic could not save selection preferences. {error}")
}

#[cfg(windows)]
fn load_optional_one_shot_ui_state(
    store: &captastic_config::UiStateStore,
    display_id: &str,
) -> captastic_config::DisplayUiState {
    match store.load_display_ui_state(display_id) {
        Ok(state) => state,
        Err(error) => {
            crate::logging::warn(format_args!(
                "could not load remembered selection preferences; continuing with defaults: {error}"
            ));
            captastic_config::DisplayUiState::default()
        }
    }
}

fn benchmark(args: BenchmarkArgs) -> Result<(), AppError> {
    let display_policy = resolve_display_policy(&args.display)?;
    let mut native_backend = if args.backend == "fake" {
        None
    } else {
        Some(create_backend(&args.backend, &display_policy)?)
    };
    let source = match native_backend.as_deref() {
        Some(backend) => resolve_capture_source(&display_policy, backend.displays())?,
        None => match &display_policy {
            DisplayPolicy::Primary => CaptureSource::Display(DisplayId::primary()),
            DisplayPolicy::Fixed(id) => CaptureSource::Display(id.clone()),
            DisplayPolicy::Pointer => {
                return Err(AppError::InvalidArgument(
                    "the fake benchmark backend does not support pointer display selection"
                        .to_owned(),
                ));
            }
            DisplayPolicy::VirtualDesktop => {
                return Err(AppError::InvalidArgument(
                    "the fake benchmark backend does not support virtual-desktop capture"
                        .to_owned(),
                ));
            }
        },
    };
    let options = benchmark::BenchmarkOptions {
        iterations: args.iterations,
        warmup: args.warmup,
        mode: capture_mode(args.mode),
        cpu_frame: args.cpu_frame,
        source,
        trigger_queue_capacity: 4,
        metrics_capacity: args.iterations.saturating_mul(10).saturating_add(32),
        fake: benchmark::fake_config(
            args.native_delay_us,
            args.readback_delay_us,
            args.frame_age_us,
        ),
    };
    let run = if args.backend == "fake" {
        benchmark::run(&options)?
    } else {
        benchmark::run_with_backend(
            native_backend
                .as_deref_mut()
                .expect("non-fake backend initialized above"),
            &options,
        )?
    };
    if let Some(path) = args.output_results.as_deref() {
        benchmark::write_json(path, &run.report)?;
    }
    if let Some(path) = args.raw_events.as_deref() {
        benchmark::write_json_lines(path, &run.events)?;
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&run.report)?);
    } else {
        log::info!(
            "Captastic {} benchmark: {} successful / {} total ({})",
            run.report.backend,
            run.report.successes,
            run.report.timed_iterations,
            run.report.mode
        );
        print_latency("trigger-to-dequeue", &run.report.trigger_to_dequeue_latency);
        print_latency("native frame", &run.report.native_frame_latency);
        if let Some(summary) = &run.report.cpu_frame_latency {
            print_latency("CPU frame", summary);
        }
        if let Some(summary) = &run.report.readback_latency {
            print_latency("native-to-CPU readback", summary);
        }
        print_latency("frame age", &run.report.frame_age);
        log::info!(
            "critical-path event order: {}",
            if run.report.critical_path_order_verified {
                "verified"
            } else {
                "invalid"
            }
        );
    }
    Ok(())
}

fn config(command: ConfigCommand) -> Result<(), AppError> {
    match command {
        ConfigCommand::Show { path, json } => {
            let config = match path {
                Some(path) => AppConfig::load(&path)?,
                None => AppConfig::load_default()?,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&config)?);
            } else {
                print!("{}", config.to_toml_pretty()?);
            }
            Ok(())
        }
        ConfigCommand::Validate { path } => {
            AppConfig::load(&path)?;
            println!("valid: {}", path.display());
            Ok(())
        }
    }
}

fn capture_mode(mode: ModeArg) -> CaptureMode {
    match mode {
        ModeArg::Fresh => CaptureMode::Fresh { timeout_ms: 100 },
        ModeArg::Latest => CaptureMode::Latest { max_age_ms: None },
    }
}

fn resolve_display_policy(value: &str) -> Result<DisplayPolicy, AppError> {
    let value = value.trim();
    if value == "pointer" {
        return Ok(DisplayPolicy::Pointer);
    }
    if value == "primary" {
        return Ok(DisplayPolicy::Primary);
    }
    if value == "virtual_desktop" {
        return Ok(DisplayPolicy::VirtualDesktop);
    }
    if let Some(id) = value.strip_prefix("display:") {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(DisplayPolicy::Fixed(DisplayId(id.to_owned())));
        }
    }
    Err(AppError::InvalidArgument(
        "display must be pointer, primary, virtual_desktop, or display:<persistent-id>".to_owned(),
    ))
}

#[cfg(windows)]
fn resolve_capture_source(
    policy: &DisplayPolicy,
    displays: &[captastic_core::DisplayInfo],
) -> Result<CaptureSource, captastic_core::CaptureError> {
    match policy {
        DisplayPolicy::Pointer => {
            captastic_windows::display_containing_pointer(displays).map(CaptureSource::Display)
        }
        DisplayPolicy::Primary => Ok(CaptureSource::Display(DisplayId::primary())),
        DisplayPolicy::Fixed(id) => Ok(CaptureSource::Display(id.clone())),
        DisplayPolicy::VirtualDesktop => Ok(CaptureSource::VirtualDesktop),
    }
}

#[cfg(not(windows))]
fn resolve_capture_source(
    policy: &DisplayPolicy,
    _displays: &[captastic_core::DisplayInfo],
) -> Result<CaptureSource, captastic_core::CaptureError> {
    match policy {
        DisplayPolicy::Primary => Ok(CaptureSource::Display(DisplayId::primary())),
        DisplayPolicy::Fixed(id) => Ok(CaptureSource::Display(id.clone())),
        DisplayPolicy::VirtualDesktop => Ok(CaptureSource::VirtualDesktop),
        DisplayPolicy::Pointer => Err(captastic_core::CaptureError {
            kind: captastic_core::CaptureErrorKind::Unsupported,
            backend: "platform",
            operation: "resolve_pointer_display",
            message: "pointer display selection is currently available only on Windows".to_owned(),
            retryable: false,
            native_code: None,
        }),
    }
}

fn enumerate_displays(backend: &str) -> Result<Vec<captastic_core::DisplayInfo>, AppError> {
    match backend {
        "fake" => {
            let backend = captastic_core::FakeBackend::new(Default::default());
            Ok(backend.displays().to_vec())
        }
        "auto" | "dxgi" => enumerate_dxgi_displays(),
        other => Err(AppError::BackendUnavailable(format!(
            "unknown backend {other}; available backends: fake, dxgi"
        ))),
    }
}

#[cfg(windows)]
fn enumerate_dxgi_displays() -> Result<Vec<captastic_core::DisplayInfo>, AppError> {
    captastic_windows::enumerate_displays().map_err(AppError::from)
}

#[cfg(not(windows))]
fn enumerate_dxgi_displays() -> Result<Vec<captastic_core::DisplayInfo>, AppError> {
    Err(AppError::BackendUnavailable(
        "DXGI is available only on Windows".to_owned(),
    ))
}

fn create_backend(
    name: &str,
    display_policy: &DisplayPolicy,
) -> Result<Box<dyn CaptureBackend>, AppError> {
    match name {
        "fake" => Ok(Box::new(captastic_core::FakeBackend::new(
            Default::default(),
        ))),
        "auto" | "dxgi" => create_dxgi_backend(display_policy),
        other => Err(AppError::BackendUnavailable(format!(
            "unknown backend {other}; available backends: fake, dxgi"
        ))),
    }
}

#[cfg(windows)]
fn create_dxgi_backend(
    display_policy: &DisplayPolicy,
) -> Result<Box<dyn CaptureBackend>, AppError> {
    match display_policy {
        DisplayPolicy::Pointer => Ok(Box::new(captastic_windows::DxgiDisplayManager::new()?)),
        DisplayPolicy::Primary => Ok(Box::new(captastic_windows::DxgiBackend::new(
            &DisplayId::primary(),
        )?)),
        DisplayPolicy::Fixed(id) => Ok(Box::new(captastic_windows::DxgiBackend::new(id)?)),
        DisplayPolicy::VirtualDesktop => {
            Ok(Box::new(captastic_windows::DxgiDisplayManager::new()?))
        }
    }
}

#[cfg(not(windows))]
fn create_dxgi_backend(
    _display_policy: &DisplayPolicy,
) -> Result<Box<dyn CaptureBackend>, AppError> {
    Err(AppError::BackendUnavailable(
        "DXGI is available only on Windows".to_owned(),
    ))
}

fn doctor(json_output: bool) -> Result<(), AppError> {
    let (native_status, native_error, displays) = match create_dxgi_backend(&DisplayPolicy::Primary)
    {
        Ok(backend) => ("available", None, Some(backend.displays().to_vec())),
        Err(error) => ("unavailable", Some(error.to_string()), None),
    };
    let windows_clipboard = if cfg!(windows) {
        "available_as_uncompressed_dibv5"
    } else {
        "unavailable_on_this_platform"
    };
    let windows_selection_overlay = if cfg!(windows) {
        "available_for_regions_and_native_window_rendering"
    } else {
        "unavailable_on_this_platform"
    };
    print_value(
        json_output,
        &json!({
            "schema_version": 1,
            "build": build_info::BUILD_INFO,
            "phase": 1,
            "platform": std::env::consts::OS,
            "fake_backend": "available",
            "dxgi_backend": native_status,
            "dxgi_error": native_error,
            "dxgi_displays": displays,
            "cpu_readback": "available_for_unrotated_bgra8_outputs",
            "windows_clipboard": windows_clipboard,
            "windows_selection_overlay": windows_selection_overlay,
            "latest_warm_frame": "available",
            "critical_path_policy": "configured",
        }),
    )
}

fn print_value<T: serde::Serialize + std::fmt::Debug + ?Sized>(
    json_output: bool,
    value: &T,
) -> Result<(), AppError> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{value:#?}");
    }
    Ok(())
}

fn print_latency(label: &str, summary: &captastic_core::LatencySummary) {
    log::info!(
        "{label}: p50 {:.3} ms, p95 {:.3} ms, p99 {:.3} ms (n={})",
        ns_to_ms(summary.p50_ns),
        ns_to_ms(summary.p95_ns),
        ns_to_ms(summary.p99_ns),
        summary.count
    );
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

#[cfg(windows)]
fn duration_ns(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use std::fs;

    use super::*;

    fn cli(command: Option<Command>) -> Cli {
        Cli {
            log_file: None,
            log_level: None,
            log_format: None,
            command,
        }
    }

    #[test]
    fn only_operational_commands_persist_logs_by_default() {
        assert!(uses_persistent_logging(&cli(None)));
        assert!(uses_persistent_logging(&cli(Some(Command::Daemon(
            cli::DaemonArgs::default()
        )))));
        assert!(!uses_persistent_logging(&cli(Some(Command::Doctor {
            json: true
        }))));
        let capture = Cli::try_parse_from(["captastic", "capture"]).expect("capture CLI");
        assert!(uses_persistent_logging(&capture));
        let benchmark = Cli::try_parse_from(["captastic", "benchmark"]).expect("benchmark CLI");
        assert!(uses_persistent_logging(&benchmark));

        let mut explicit = cli(Some(Command::Doctor { json: true }));
        explicit.log_file = Some("doctor.log".into());
        assert!(uses_persistent_logging(&explicit));
    }

    #[cfg(windows)]
    #[test]
    fn one_shot_selection_ignores_unreadable_remembered_ui_state() {
        let directory =
            std::env::temp_dir().join(format!("captastic-one-shot-ui-load-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("create directory at config path");
        let store = captastic_config::UiStateStore::for_config(&directory);

        assert_eq!(
            load_optional_one_shot_ui_state(&store, "display-1"),
            captastic_config::DisplayUiState::default()
        );

        fs::remove_dir_all(directory).expect("remove directory at config path");
    }

    #[cfg(windows)]
    #[test]
    fn one_shot_ui_state_persistence_failure_does_not_abort_the_capture() {
        let directory = std::env::temp_dir().join(format!(
            "captastic-one-shot-ui-finish-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("create directory at config path");
        let store = captastic_config::UiStateStore::for_config(&directory);
        let worker = selection::OneShotUiStateWorker::start(store).expect("start one-shot worker");
        worker
            .controller()
            .submit_ui_update(captastic_windows::OverlayUiUpdate::ToolbarCenter {
                display_id: "display-1".to_owned(),
                center_x: 0.25,
                center_y: 0.75,
            });

        // The capture is the product; retiring the worker reports the failed preference write
        // instead of propagating it, so the surrounding capture path cannot exit with an error.
        finish_one_shot_ui_state(Some(worker));

        fs::remove_dir_all(directory).expect("remove directory at config path");
    }

    #[cfg(windows)]
    #[test]
    fn one_shot_ui_state_warning_matches_the_daemon_phrasing() {
        let warning = one_shot_ui_state_warning(&AppError::BackendUnavailable(
            "failed to persist UI state: access is denied".to_owned(),
        ));
        assert!(warning.starts_with("Captastic could not save selection preferences."));
        assert!(warning.contains("failed to persist UI state"));
    }

    #[test]
    fn display_policy_resolves_pointer_primary_virtual_desktop_and_fixed_ids() {
        assert_eq!(
            resolve_display_policy("pointer").expect("pointer"),
            DisplayPolicy::Pointer
        );
        assert_eq!(
            resolve_display_policy("primary").expect("primary"),
            DisplayPolicy::Primary
        );
        assert_eq!(
            resolve_display_policy("virtual_desktop").expect("virtual desktop"),
            DisplayPolicy::VirtualDesktop
        );
        assert_eq!(
            resolve_display_policy("display:windows-monitor-0123456789abcdef")
                .expect("fixed display"),
            DisplayPolicy::Fixed(DisplayId("windows-monitor-0123456789abcdef".to_owned()))
        );
    }

    #[test]
    fn unresolved_display_policies_fail_before_backend_initialization() {
        for value in ["display:", "unexpected"] {
            assert!(matches!(
                resolve_display_policy(value),
                Err(AppError::InvalidArgument(_))
            ));
        }
    }
}
