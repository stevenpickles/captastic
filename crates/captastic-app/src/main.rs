#![deny(unsafe_code)]

mod benchmark;
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
use cli::{BenchmarkArgs, Cli, Command, ConfigCommand, ModeArg};
use error::AppError;
use serde_json::json;

fn main() {
    let cli = Cli::parse();
    let logging_config = resolve_logging_config(&cli);
    let logging_available = match logging::init(&logging_config) {
        Ok(path) => {
            log::info!(
                "Captastic started; persistent log file is {}",
                path.display()
            );
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
    log::info!("Captastic stopped successfully");
    log::logger().flush();
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
        _ => LoggingConfig::default(),
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
            let backend = create_backend(&backend)?;
            print_value(json, backend.displays())
        }
        Command::Capture(args) => capture(args),
        Command::Benchmark(args) => benchmark(args),
        Command::Doctor { json } => doctor(json),
        Command::Config { command } => config(command),
    }
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
        &json!({"schema_version": 1, "status": "not_running"}),
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
    if (args.selection || args.clipboard) && !args.cpu_frame {
        return Err(AppError::InvalidArgument(
            "selection and clipboard output require --cpu-frame true".to_owned(),
        ));
    }
    let mut backend = create_backend(&args.backend)?;
    let mut recorder = EventRecorder::with_capacity(16);
    let request = CaptureRequest {
        id: CaptureId(1),
        triggered_at: Instant::now(),
        source: CaptureSource::Display(DisplayId::primary()),
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
    if args.selection {
        #[cfg(windows)]
        {
            let full_frame = frame.take().ok_or_else(|| {
                AppError::BackendUnavailable(
                    "selection was requested but no CPU frame was returned".to_owned(),
                )
            })?;
            recorder.record(request.id, PerfEventKind::SelectionStarted, 0);
            let Some(selection) = captastic_windows::select_from_frozen_frame(&full_frame)? else {
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
                "overlay_preparation_ns": selection.preparation_ns,
                "window_overview_ns": selection.window_overview_ns,
                "window_preview_count": selection.window_preview_count,
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
    Ok(())
}

fn benchmark(args: BenchmarkArgs) -> Result<(), AppError> {
    let options = benchmark::BenchmarkOptions {
        iterations: args.iterations,
        warmup: args.warmup,
        mode: capture_mode(args.mode),
        cpu_frame: args.cpu_frame,
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
        let mut backend = create_backend(&args.backend)?;
        benchmark::run_with_backend(backend.as_mut(), &options)?
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

fn create_backend(name: &str) -> Result<Box<dyn CaptureBackend>, AppError> {
    match name {
        "fake" => Ok(Box::new(captastic_core::FakeBackend::new(
            Default::default(),
        ))),
        "auto" | "dxgi" => create_dxgi_backend(),
        other => Err(AppError::BackendUnavailable(format!(
            "unknown backend {other}; available backends: fake, dxgi"
        ))),
    }
}

#[cfg(windows)]
fn create_dxgi_backend() -> Result<Box<dyn CaptureBackend>, AppError> {
    Ok(Box::new(captastic_windows::DxgiBackend::new_primary()?))
}

#[cfg(not(windows))]
fn create_dxgi_backend() -> Result<Box<dyn CaptureBackend>, AppError> {
    Err(AppError::BackendUnavailable(
        "DXGI is available only on Windows".to_owned(),
    ))
}

fn doctor(json_output: bool) -> Result<(), AppError> {
    let (native_status, native_error, displays) = match create_dxgi_backend() {
        Ok(backend) => ("available", None, Some(backend.displays().to_vec())),
        Err(error) => ("unavailable", Some(error.to_string()), None),
    };
    print_value(
        json_output,
        &json!({
            "schema_version": 1,
            "phase": 1,
            "fake_backend": "available",
            "dxgi_backend": native_status,
            "dxgi_error": native_error,
            "dxgi_displays": displays,
            "cpu_readback": "available_for_unrotated_bgra8_outputs",
            "windows_clipboard": "available_as_uncompressed_dibv5",
            "windows_selection_overlay": "available_for_regions_and_native_window_rendering",
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
