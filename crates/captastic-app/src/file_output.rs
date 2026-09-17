//! The file destination: encode a capture and put it on disk.
//!
//! Everything expensive here — compression, allocation, the write itself — happens on this
//! worker's own thread, after CPU-frame readiness, which is the boundary ADR 0002 exists to
//! defend. The capture path hands over an owned frame and does not wait.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use captastic_core::{
    encode_capture, process_detach_ledger, validate_event_order, CaptureId, DetachKind,
    EncodeOptions, OutputFormat, PerfEventKind,
};
use serde_json::json;

use crate::error::AppError;
use crate::output::OutputJob;

const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const WORKER_STOP_POLL: Duration = Duration::from_millis(5);
const WORKER_RECEIVE_POLL: Duration = Duration::from_millis(50);
/// How many names are tried before a capture is abandoned. Reached only if that many captures
/// share a second *and* the same directory, which means something other than naming is wrong.
const MAX_COLLISION_ATTEMPTS: u32 = 100;

/// How the file destination is named in the destination set, in logs, and in structured output.
///
/// One constant rather than a literal at each site: the destination set is addressed by name when
/// file output is switched off, and a name that matched in three places and not the fourth would
/// leave a sink behind that nothing can reach.
pub const DESTINATION_NAME: &str = "file";

/// Where the worker thread stands relative to the deadline for stopping it.
///
/// A flag would not do. Deciding to detach races the thread's own exit: it can finish between the
/// last `is_finished` poll and the decision, and a detach recorded for a thread that had already
/// gone would hold the only file-output slot there is for the rest of the run. So the decision and
/// the exit claim the same cell, and exactly one of them wins.
struct WorkerLifetime(AtomicU8);

const WORKER_RUNNING: u8 = 0;
const WORKER_DETACHED: u8 = 1;
const WORKER_FINISHED: u8 = 2;

impl WorkerLifetime {
    const fn new() -> Self {
        Self(AtomicU8::new(WORKER_RUNNING))
    }

    /// Claims the detach. `false` means the thread finished first and nothing was abandoned.
    fn detach(&self) -> bool {
        self.0
            .compare_exchange(
                WORKER_RUNNING,
                WORKER_DETACHED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// The worker thread's last act. `true` means it had already been written off, which is the
    /// caller's cue to give back the ledger slot that detach took.
    fn finished(&self) -> bool {
        self.0.swap(WORKER_FINISHED, Ordering::AcqRel) == WORKER_DETACHED
    }
}

pub struct FileOutputWorker {
    sender: Option<mpsc::SyncSender<OutputJob>>,
    failure_receiver: mpsc::Receiver<FileOutputFailure>,
    /// Announces successful writes so the daemon can enable the history menu entries. Bounded and
    /// lossy on purpose: it carries "there is at least one capture now", not a record of each.
    written_receiver: mpsc::Receiver<()>,
    /// Carries the run's totals back from the worker thread when it exits.
    summary_receiver: mpsc::Receiver<crate::output_metrics::OutputMetrics>,
    stop_requested: Arc<AtomicBool>,
    lifetime: Arc<WorkerLifetime>,
    join: Option<JoinHandle<()>>,
    directory: PathBuf,
}

/// What the file destination has to say once it has stopped.
pub struct FileOutputTeardown {
    pub failures: Vec<FileOutputFailure>,
    pub summary: Option<crate::output_metrics::OutputSummary>,
}

/// A capture that did not reach disk.
pub struct FileOutputFailure {
    pub capture_id: CaptureId,
    pub message: String,
}

/// What one successful write did, for metrics and logs.
struct WriteReport {
    path: PathBuf,
    bytes: usize,
    encode_ns: u64,
    write_ns: u64,
    /// How many names were already taken before this one was free.
    collisions: u32,
}

impl FileOutputWorker {
    pub fn start(
        directory: PathBuf,
        filename_template: String,
        format: OutputFormat,
        encode_options: EncodeOptions,
        history: HistoryRecorder,
        json_output: bool,
        queue_capacity: usize,
    ) -> Result<Self, AppError> {
        crate::filename_template::validate_template(&filename_template)
            .map_err(AppError::InvalidArgument)?;
        // Created up front rather than on first capture: a directory that cannot be created is a
        // configuration problem, and the user should hear about it at startup rather than
        // discovering it the first time they press the hotkey.
        std::fs::create_dir_all(&directory).map_err(|error| {
            AppError::InvalidArgument(format!(
                "failed to create output directory {}: {error}",
                directory.display()
            ))
        })?;
        let (sender, receiver) = mpsc::sync_channel::<OutputJob>(queue_capacity);
        let (failure_sender, failure_receiver) = mpsc::sync_channel(queue_capacity);
        let (written_sender, written_receiver) = mpsc::sync_channel(1);
        let (summary_sender, summary_receiver) = mpsc::sync_channel(1);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_stop_requested = stop_requested.clone();
        let lifetime = Arc::new(WorkerLifetime::new());
        let worker_lifetime = lifetime.clone();
        let worker_directory = directory.clone();
        let worker_template = filename_template;
        let worker_history = history;
        let join = thread::Builder::new()
            .name("captastic-file-output".to_owned())
            .spawn(move || {
                // Owned by the worker thread so recording costs no synchronization, and handed
                // back once at teardown rather than being polled.
                let mut metrics = crate::output_metrics::OutputMetrics::new("file");
                loop {
                    if worker_stop_requested.load(Ordering::Acquire) {
                        break;
                    }
                    let mut job = match receiver.recv_timeout(WORKER_RECEIVE_POLL) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    match write_capture(
                        &worker_directory,
                        &worker_template,
                        format,
                        &encode_options,
                        &mut job,
                    ) {
                        Ok(report) => {
                            metrics.record_write(
                                report.bytes,
                                report.encode_ns,
                                report.write_ns,
                                report.collisions,
                            );
                            report_written(&job, &report, format, json_output);
                            worker_history.record(&job, &report);
                            // A full channel means the daemon has not drained the previous signal
                            // yet, which already says what this one would.
                            let _ = written_sender.try_send(());
                        }
                        Err(error) => {
                            metrics.record_failure();
                            crate::logging::error(format_args!(
                                "file output {} failed without invalidating capture: {error}",
                                job.capture_id.0
                            ));
                            let _ = failure_sender.try_send(FileOutputFailure {
                                capture_id: job.capture_id,
                                message: error.clone(),
                            });
                            if json_output {
                                println!(
                                    "{}",
                                    json!({
                                        "schema_version": 1,
                                        "event": "file_output_failed",
                                        "capture_id": job.capture_id,
                                        "source": job.source,
                                        "action": job.action,
                                        "error": error,
                                    })
                                );
                            }
                        }
                    }
                    finish_attempt(&mut job);
                }
                // Reported once, on the way out: a per-capture line already says what each
                // capture cost, and this is the shape of the whole run.
                let _ = summary_sender.try_send(metrics);
                if worker_lifetime.finished() {
                    // Written off at a deadline and back anyway. The slot it was holding is free,
                    // so file output can be switched on again. `live` falls; the history of the
                    // detach stays, which is the number that says whether this is habitual.
                    process_detach_ledger().rejoined(DetachKind::FileOutputWorker);
                    log::info!(
                        "the detached file output worker has exited; saving captures to disk can be switched on again"
                    );
                }
            })
            .map_err(|error| AppError::BackendUnavailable(error.to_string()))?;
        Ok(Self {
            sender: Some(sender),
            failure_receiver,
            written_receiver,
            summary_receiver,
            stop_requested,
            lifetime,
            join: Some(join),
            directory,
        })
    }

    /// The file destination, addressable without knowing it writes files.
    pub fn sink(&self) -> crate::output::ChannelSink {
        crate::output::ChannelSink::new(
            DESTINATION_NAME,
            self.sender
                .as_ref()
                .expect("file output worker is running")
                .clone(),
        )
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn try_recv_failure(&self) -> Option<FileOutputFailure> {
        self.failure_receiver.try_recv().ok()
    }

    /// Whether a capture has been written since this was last asked.
    pub fn took_write(&self) -> bool {
        self.written_receiver.try_recv().is_ok()
    }

    pub fn stop_before(mut self, deadline: Instant) -> FileOutputTeardown {
        self.request_stop();
        self.stop_inner(deadline);
        FileOutputTeardown {
            failures: self.failure_receiver.try_iter().collect(),
            // Absent when the worker was detached at its deadline rather than exiting, in which
            // case there are no totals to report because nothing finished counting them.
            summary: self
                .summary_receiver
                .try_recv()
                .ok()
                .filter(|metrics| !metrics.is_empty())
                .map(|metrics| metrics.summary()),
        }
    }

    pub fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        self.sender.take();
    }

    fn stop_inner(&mut self, deadline: Instant) {
        self.request_stop();
        let Some(join) = self.join.take() else {
            return;
        };
        while !join.is_finished() && Instant::now() < deadline {
            thread::sleep(WORKER_STOP_POLL);
        }
        // `detach` is what decides, not `is_finished`: the thread can exit between the poll above
        // and this line, and a detach recorded for a thread that had already gone would hold the
        // only file-output slot there is for the rest of the run.
        if join.is_finished() || !self.lifetime.detach() {
            let _ = join.join();
            return;
        }
        // Left running on purpose, exactly as the capture worker is: a write into a disk or a
        // network share that has stopped answering cannot be cancelled, and blocking here would
        // hand a menu click — or the process's exit — to whatever is wedged.
        //
        // Recorded rather than merely logged because, unlike the capture worker's, this detach is
        // reachable more than once per run: the notification area can switch file output off and
        // on again. Until this thread exits it may still write into the output directory, so the
        // ledger's ceiling is what stops a second worker joining it there.
        let detached = process_detach_ledger().detached(DetachKind::FileOutputWorker);
        crate::logging::error(format_args!(
            "file output worker did not stop before its deadline; detaching it with {} still running of {} allowed ({} detached in total). Saving captures to disk cannot be switched on again until it exits",
            detached.live,
            DetachKind::FileOutputWorker.ceiling(),
            detached.total
        ));
    }
}

impl Drop for FileOutputWorker {
    fn drop(&mut self) {
        self.stop_inner(Instant::now() + WORKER_STOP_TIMEOUT);
    }
}

/// Remembers where captures went, so they can be found again.
///
/// A failure to remember is never a failure to capture: the file is already on disk by the time
/// this runs, and losing the note is a smaller harm than losing the picture. Every path here logs
/// and continues.
#[derive(Clone)]
pub struct HistoryRecorder {
    store: captastic_config::HistoryStore,
    policy: captastic_config::RetentionPolicy,
}

impl HistoryRecorder {
    pub fn new(
        store: captastic_config::HistoryStore,
        policy: captastic_config::RetentionPolicy,
    ) -> Self {
        Self { store, policy }
    }

    fn record(&self, job: &OutputJob, report: &WriteReport) {
        if self.policy.max_items == 0 {
            return;
        }
        let entry = captastic_config::HistoryEntry {
            path: report.path.clone(),
            captured_at_micros: captastic_config::HistoryEntry::micros_since_epoch(
                SystemTime::now(),
            ),
            bytes: report.bytes as u64,
            width: job.frame.width(),
            height: job.frame.height(),
            display: job.frame.metadata.display_id.0.clone(),
            mode: job.action.as_str().to_owned(),
        };
        match self.store.record(entry, self.policy, SystemTime::now()) {
            Ok(dropped) if !dropped.is_empty() => {
                // Only the count: a path is the user's business and belongs in their history file,
                // not in a log that may be shared.
                log::debug!("capture history forgot {} older entr(ies)", dropped.len());
            }
            Ok(_) => {}
            Err(error) => {
                crate::logging::warn(format_args!(
                    "capture {} was written but could not be recorded in history: {error}",
                    job.capture_id.0
                ));
            }
        }
    }
}

/// Encodes and writes one capture synchronously, for callers with no worker to hand it to.
///
/// The one-shot `capture` command is exiting the moment this returns, so a worker thread would
/// only be something to wait for. The daemon uses the worker; this is the same work without the
/// hand-off, and the two share every step that matters — `Compact` encoding, and a finalization
/// that refuses to overwrite.
pub fn write_capture_now(
    directory: &Path,
    template: &str,
    action: captastic_config::HotkeyAction,
    frame: &captastic_core::CpuFrame,
    format: OutputFormat,
    encode_options: &EncodeOptions,
) -> Result<(PathBuf, usize, u64, u64), AppError> {
    crate::filename_template::validate_template(template).map_err(AppError::InvalidArgument)?;
    std::fs::create_dir_all(directory).map_err(|error| {
        AppError::InvalidArgument(format!(
            "failed to create output directory {}: {error}",
            directory.display()
        ))
    })?;
    let encode_started = Instant::now();
    let encoded = encode_capture(frame, format, encode_options).map_err(|error| {
        AppError::BackendUnavailable(format!("failed to encode capture: {error}"))
    })?;
    let encode_ns = duration_ns(encode_started.elapsed());
    report_alpha_flattening(&encoded, format);
    let write_started = Instant::now();
    let stem = crate::filename_template::expand(
        template,
        &crate::filename_template::TemplateContext {
            timestamp_micros: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_micros()),
            display: &frame.metadata.display_id.0,
            mode: action.as_str(),
            width: frame.width(),
            height: frame.height(),
            application: None,
            title: None,
        },
    );
    let (path, _) = write_without_clobbering(directory, &stem, encoded.extension, &encoded.bytes)
        .map_err(AppError::BackendUnavailable)?;
    Ok((
        path,
        encoded.bytes.len(),
        encode_ns,
        duration_ns(write_started.elapsed()),
    ))
}

/// Encodes and writes one capture, recording what it cost on the job's own trace.
fn write_capture(
    directory: &Path,
    template: &str,
    format: OutputFormat,
    encode_options: &EncodeOptions,
    job: &mut OutputJob,
) -> Result<WriteReport, String> {
    job.recorder
        .record(job.capture_id, PerfEventKind::EncodeStarted, elapsed(job));
    let encode_started = Instant::now();
    // PNG's `Compact` rather than the clipboard's `Fast`: this runs on a worker thread where bytes
    // on disk outlive the milliseconds spent producing them. The caller sets that, the format, and
    // the JPEG quality from `[output]`.
    let encoded = encode_capture(&job.frame, format, encode_options)
        .map_err(|error| format!("failed to encode capture: {error}"))?;
    let encode_ns = duration_ns(encode_started.elapsed());
    report_alpha_flattening(&encoded, format);
    job.recorder
        .record(job.capture_id, PerfEventKind::EncodeFinished, encode_ns);

    job.recorder.record(
        job.capture_id,
        PerfEventKind::FileWriteStarted,
        elapsed(job),
    );
    let write_started = Instant::now();
    let (path, collisions) = write_without_clobbering(
        directory,
        &capture_stem(template, job),
        encoded.extension,
        &encoded.bytes,
    )?;
    let write_ns = duration_ns(write_started.elapsed());
    job.recorder
        .record(job.capture_id, PerfEventKind::FileWriteFinished, write_ns);

    Ok(WriteReport {
        path,
        bytes: encoded.bytes.len(),
        encode_ns,
        write_ns,
        collisions,
    })
}

/// Writes `contents` under a name nothing else is using, and reports how many were taken.
///
/// A capture is never allowed to overwrite a file it did not create: the output directory is
/// somewhere the user also keeps things, and a screenshot silently replacing one of them is worse
/// than a screenshot that fails to save. `finalize_new` refuses rather than replaces, so the
/// refusal *is* the existence check and there is no window between the two.
fn write_without_clobbering(
    directory: &Path,
    stem: &str,
    extension: &str,
    contents: &[u8],
) -> Result<(PathBuf, u32), String> {
    for attempt in 0..MAX_COLLISION_ATTEMPTS {
        let candidate = if attempt == 0 {
            stem.to_owned()
        } else {
            format!("{stem}-{}", attempt + 1)
        };
        // The containment check runs on every write, not only in the sanitizer's tests. It should
        // be impossible for a sanitized stem to fail it; that is exactly why it is cheap to keep.
        let Some(path) = crate::filename_template::resolve(directory, &candidate, extension) else {
            return Err(format!(
                "refusing to write {candidate}.{extension}: it would land outside {}",
                directory.display()
            ));
        };
        match captastic_config::atomic_write(&path, contents, captastic_config::finalize_new) {
            Ok(()) => return Ok((path, attempt)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("failed to write {}: {error}", path.display()));
            }
        }
    }
    Err(format!(
        "failed to find an unused name in {} after {MAX_COLLISION_ATTEMPTS} attempts",
        directory.display()
    ))
}

/// Names a capture from the user's template and what the capture knows about itself.
fn capture_stem(template: &str, job: &OutputJob) -> String {
    crate::filename_template::expand(
        template,
        &crate::filename_template::TemplateContext {
            timestamp_micros: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_micros()),
            display: &job.frame.metadata.display_id.0,
            mode: job.action.as_str(),
            width: job.frame.width(),
            height: job.frame.height(),
            application: job.window_application.as_deref(),
            title: job.window_title.as_deref(),
        },
    )
}

/// Says when a capture's alpha channel was composited away to fit the chosen format.
///
/// JPEG cannot carry alpha, so a window capture written as one loses its rounded corners to opaque
/// white. That is inherent to the format the user chose rather than something Captastic decided,
/// but it is still a difference between what was on screen and what is on disk, and ADR 0008's
/// standard is that the user can find out. Debug rather than info: once the format is set this is
/// true of every window capture, and at info level it would crowd out the lines that vary.
fn report_alpha_flattening(encoded: &captastic_core::EncodedCapture, format: OutputFormat) {
    if encoded.alpha_flattened {
        log::debug!(
            "{} cannot carry alpha: this capture's transparent pixels were composited over opaque white",
            format.label()
        );
    }
}

fn report_written(job: &OutputJob, report: &WriteReport, format: OutputFormat, json_output: bool) {
    if json_output {
        println!(
            "{}",
            json!({
                "schema_version": 1,
                "event": "file_output_written",
                "capture_id": job.capture_id,
                "source": job.source,
                "action": job.action,
                "format": format.as_str(),
                "path": report.path.display().to_string(),
                "bytes": report.bytes,
                "encode_ns": report.encode_ns,
                "write_ns": report.write_ns,
                "collisions": report.collisions,
            })
        );
    } else {
        log::info!(
            "file output {}: wrote {} as {} ({} bytes, encode {:.3} ms, write {:.3} ms{})",
            job.capture_id.0,
            report.path.display(),
            format,
            report.bytes,
            ns_to_ms(report.encode_ns),
            ns_to_ms(report.write_ns),
            if report.collisions == 0 {
                String::new()
            } else {
                format!(", {} name(s) already taken", report.collisions)
            }
        );
    }
}

/// Closes this destination's trace, whether or not the write succeeded.
///
/// Every destination records its own `AttemptFinished` (ADR 0002): a capture delivered to two
/// destinations produces two complete traces, not one interleaved one.
fn finish_attempt(job: &mut OutputJob) {
    job.recorder
        .record(job.capture_id, PerfEventKind::AttemptFinished, elapsed(job));
    if let Err(error) = validate_event_order(job.recorder.events()) {
        crate::logging::error(format_args!(
            "file output capture {} metrics failed validation: {error}",
            job.capture_id.0
        ));
    }
}

fn elapsed(job: &OutputJob) -> u64 {
    duration_ns(job.triggered_at.elapsed())
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorder that remembers nothing, for tests about writing rather than remembering.
    fn no_history() -> HistoryRecorder {
        HistoryRecorder::new(
            captastic_config::HistoryStore::at(std::env::temp_dir().join("unused-history.toml")),
            captastic_config::RetentionPolicy {
                max_items: 0,
                max_age: None,
                max_total_bytes: None,
            },
        )
    }

    fn test_directory(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "captastic-file-output-{label}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test directory");
        path
    }

    /// A job carrying whatever the selection learned about the window it captured.
    fn window_job(application: Option<&str>, title: Option<&str>) -> OutputJob {
        let frame = captastic_core::CpuFrame::new(
            std::sync::Arc::from(vec![0_u8; 16]),
            2,
            2,
            8,
            captastic_core::PixelFormat::Bgra8Unorm,
            captastic_core::FrameOrigin::TopLeft,
            captastic_core::ColorSpace::Srgb,
            captastic_core::FrameMetadata {
                capture_id: captastic_core::CaptureId(1),
                backend: "test".to_owned(),
                display_id: captastic_core::DisplayId::primary(),
                source_rect: captastic_core::Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                rotation_degrees: 0,
                capture_mode: captastic_core::CaptureMode::Latest { max_age_ms: None },
                presentation_offset_ns: None,
                timing_provenance: captastic_core::TimingProvenance::Synthetic,
                native_ready_offset_ns: 0,
                cpu_ready_offset_ns: Some(0),
                frame_age_ns: Some(0),
                verified_current_offset_ns: None,
                frame_generation: Some(1),
                copy_count: 0,
                pool_slot: None,
                cursor: None,
            },
        )
        .expect("test frame");
        OutputJob {
            capture_id: captastic_core::CaptureId(1),
            triggered_at: Instant::now(),
            action: captastic_config::HotkeyAction::Window,
            chord: None,
            cpu_ready_offset_ns: 0,
            source: "test",
            frame,
            recorder: captastic_core::EventRecorder::with_capacity(4),
            window_title: title.map(str::to_owned),
            window_application: application.map(str::to_owned),
        }
    }

    #[test]
    fn the_default_template_names_a_capture_from_the_job_it_is_given() {
        // The seam between what a capture knows about itself and what the default asks for. The
        // template's own tests cover the expansion; this covers the wiring, which is the half that
        // would silently produce timestamp-only names if a field stopped being carried.
        let template = captastic_config::DEFAULT_FILENAME_TEMPLATE;

        let named = capture_stem(
            template,
            &window_job(Some("chrome"), Some("Pull Request 43")),
        );
        assert!(
            named.ends_with("-chrome-Pull-Request-43"),
            "a window capture should be named after its window: {named}"
        );

        // A display capture carries neither, and must not be named with the gaps where they went.
        let bare = capture_stem(template, &window_job(None, None));
        assert!(
            !bare.contains("--") && !bare.ends_with('-'),
            "a capture with no window left separators behind: {bare}"
        );
        assert_eq!(bare.len(), "20260816-230500-348".len(), "{bare}");
    }

    #[test]
    fn a_rejected_template_stops_the_worker_starting() {
        // The template comes from the user's configuration, so a typo should be a startup error
        // rather than something discovered when a capture lands under a strange name.
        let directory = test_directory("bad-template");

        let Err(error) = FileOutputWorker::start(
            directory.clone(),
            "captastic-{tilte}".to_owned(),
            OutputFormat::Png,
            EncodeOptions::default(),
            no_history(),
            false,
            1,
        ) else {
            panic!("an unknown token must be rejected");
        };
        assert!(error.to_string().contains("unknown token"), "{error}");

        std::fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn the_write_path_refuses_a_stem_that_would_escape() {
        // The guarantee has to hold at the point of writing, not only in the sanitizer's own
        // tests: a caller that reached here with an unsanitized stem must still be stopped.
        let directory = test_directory("escape");

        let error = write_without_clobbering(&directory, "../escape", "png", b"capture")
            .expect_err("a traversing stem must be refused");
        assert!(error.contains("outside"), "{error}");
        assert!(
            !directory
                .parent()
                .expect("a parent")
                .join("escape.png")
                .exists(),
            "a file was written outside the output directory"
        );

        std::fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn a_capture_is_named_for_the_format_it_was_encoded_in() {
        // The extension comes from the encoder rather than from a constant at the write site,
        // which is what stops a JPEG from landing as `.png` and opening as a broken image.
        let directory = test_directory("extensions");
        let frame = window_job(None, None).frame;
        for (format, expected) in [
            (OutputFormat::Png, "png"),
            (OutputFormat::Jpeg, "jpg"),
            (OutputFormat::Bmp, "bmp"),
        ] {
            let (path, ..) = write_capture_now(
                &directory,
                captastic_config::DEFAULT_FILENAME_TEMPLATE,
                captastic_config::HotkeyAction::FullDisplay,
                &frame,
                format,
                &EncodeOptions::default(),
            )
            .unwrap_or_else(|error| panic!("{format}: {error}"));
            assert_eq!(
                path.extension().and_then(|extension| extension.to_str()),
                Some(expected),
                "{format} landed at {}",
                path.display()
            );
            let written = std::fs::read(&path).expect("capture written");
            assert!(!written.is_empty(), "{format} wrote an empty file");
        }
        std::fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn a_capture_never_overwrites_a_file_it_did_not_create() {
        // The output directory is somewhere the user also keeps things. A screenshot silently
        // replacing one of them is worse than a screenshot that fails to save.
        let directory = test_directory("no-clobber");
        let stem = "captastic-20260815-221030-123";
        let occupied = directory.join(format!("{stem}.png"));
        std::fs::write(&occupied, b"something the user already had").expect("seed a file");

        let (path, collisions) =
            write_without_clobbering(&directory, stem, "png", b"capture").expect("write");

        assert_ne!(path, occupied);
        assert_eq!(collisions, 1);
        assert_eq!(
            std::fs::read(&occupied).expect("original survives"),
            b"something the user already had"
        );
        assert_eq!(std::fs::read(&path).expect("capture written"), b"capture");
        std::fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn successive_collisions_keep_finding_free_names() {
        let directory = test_directory("collisions");
        let mut written = Vec::new();
        for expected_collisions in 0..3_u32 {
            // One fixed stem, so every write after the first must find its own name.
            let (path, collisions) = write_without_clobbering(
                &directory,
                "captastic-20260815-221030-123",
                "png",
                b"capture",
            )
            .expect("write");
            assert_eq!(collisions, expected_collisions);
            assert!(!written.contains(&path), "reused {}", path.display());
            written.push(path);
        }
        std::fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn a_write_into_a_missing_directory_reports_rather_than_panicking() {
        let directory = test_directory("missing");
        let missing = directory.join("not-created");

        let error =
            write_without_clobbering(&missing, "captastic-20260815-221030-123", "png", b"capture")
                .expect_err("a missing directory cannot be written to");
        assert!(error.contains("failed to write"), "{error}");

        std::fs::remove_dir_all(directory).expect("clean up");
    }

    /// Counts the captures that have reached a directory.
    fn written_files(directory: &Path) -> usize {
        std::fs::read_dir(directory)
            .expect("read the output directory")
            .count()
    }

    /// Waits, briefly and boundedly, for the worker thread to finish a write.
    fn wait_for_files(directory: &Path, expected: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(5);
        while written_files(directory) < expected && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        written_files(directory)
    }

    #[test]
    fn a_destination_switched_on_and_off_starts_and_stops_reaching_disk() {
        // The mechanics of the notification area's "Save Captures to Disk", without a tray: a
        // worker started while the daemon runs, its sink joining the live destination set, and
        // both leaving again — with the clipboard delivering throughout, which is the Milestone 4
        // independence criterion applied to a destination that now comes and goes.
        let directory = test_directory("runtime-toggle");
        let (clipboard_sender, clipboard) = mpsc::sync_channel(4);
        let destinations = crate::output::OutputDestinations::new(vec![std::sync::Arc::new(
            crate::output::ChannelSink::new("clipboard", clipboard_sender),
        )]);

        let worker = FileOutputWorker::start(
            directory.clone(),
            captastic_config::DEFAULT_FILENAME_TEMPLATE.to_owned(),
            OutputFormat::Png,
            EncodeOptions::default(),
            no_history(),
            false,
            4,
        )
        .expect("start the file worker");
        destinations.insert(std::sync::Arc::new(worker.sink()));
        assert_eq!(destinations.names(), vec!["clipboard", DESTINATION_NAME]);

        for sink in destinations.current().iter() {
            sink.submit(window_job(Some("editor"), None))
                .expect("both destinations accept");
        }
        assert_eq!(wait_for_files(&directory, 1), 1, "the capture reached disk");
        assert!(clipboard.try_recv().is_ok());

        // Switching off: the sink leaves the set first, so nothing is queued to a worker that is
        // already stopping.
        assert!(destinations.remove(DESTINATION_NAME));
        let teardown = worker.stop_before(Instant::now() + Duration::from_secs(2));
        assert!(teardown.failures.is_empty());
        assert_eq!(destinations.names(), vec!["clipboard"]);

        for sink in destinations.current().iter() {
            sink.submit(window_job(Some("editor"), None))
                .expect("the clipboard still accepts");
        }
        assert!(
            clipboard.try_recv().is_ok(),
            "the clipboard is untouched by file output going away"
        );
        // Nothing new on disk, and no second worker to catch it later.
        thread::sleep(Duration::from_millis(50));
        assert_eq!(written_files(&directory), 1);
        std::fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn a_worker_that_outlives_its_deadline_is_detached_counted_and_released() {
        // The toggle path makes this reachable more than once per run, which is what makes the
        // accounting worth having: without it, two file-output threads could be writing into the
        // same directory with nothing saying so.
        let directory = test_directory("detached");
        let worker = FileOutputWorker::start(
            directory.clone(),
            captastic_config::DEFAULT_FILENAME_TEMPLATE.to_owned(),
            OutputFormat::Png,
            EncodeOptions::default(),
            no_history(),
            false,
            1,
        )
        .expect("start the file worker");
        // A live sink keeps the job channel connected, so the idle worker stays inside its
        // `WORKER_RECEIVE_POLL` receive instead of waking on a disconnect: it cannot observe the
        // stop until that poll expires, which is what makes an already-expired deadline find it
        // running. (In the daemon the same is true for a different reason — the destination set
        // still held the sink a moment ago, and a capture can be in flight.)
        let sink = worker.sink();
        thread::sleep(Duration::from_millis(1));

        let before = process_detach_ledger().count(DetachKind::FileOutputWorker);
        let teardown = worker.stop_before(Instant::now());
        let after = process_detach_ledger().count(DetachKind::FileOutputWorker);

        assert_eq!(after.live, before.live + 1, "the detach is counted live");
        assert_eq!(after.total, before.total + 1, "and kept in the history");
        assert!(
            after.at_ceiling(DetachKind::FileOutputWorker),
            "one detached worker is the whole ceiling, which is what refuses the next one"
        );
        assert!(
            teardown.summary.is_none(),
            "a worker that never finished counting has no totals to report"
        );

        // It notices the stop on its next poll and exits. The slot comes back, so the daemon can
        // start file output again rather than refusing for the rest of the run, and the history
        // still records that the detach happened.
        drop(sink);
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_detach_ledger()
            .count(DetachKind::FileOutputWorker)
            .live
            > before.live
            && Instant::now() < deadline
        {
            thread::sleep(WORKER_STOP_POLL);
        }
        let released = process_detach_ledger().count(DetachKind::FileOutputWorker);
        assert_eq!(released.live, before.live, "the slot is given back");
        assert_eq!(released.total, before.total + 1);
        std::fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn starting_the_worker_creates_its_directory() {
        // A directory that cannot be created is a configuration problem, and the user should hear
        // about it at startup rather than the first time they press the hotkey.
        let directory = test_directory("creates");
        let nested = directory.join("captures").join("nested");
        let worker = FileOutputWorker::start(
            nested.clone(),
            captastic_config::DEFAULT_FILENAME_TEMPLATE.to_owned(),
            OutputFormat::Png,
            EncodeOptions::default(),
            no_history(),
            false,
            1,
        )
        .expect("start worker");

        assert!(nested.is_dir());
        assert_eq!(worker.directory(), nested);
        drop(worker);
        std::fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn a_directory_that_cannot_be_created_fails_at_startup() {
        let directory = test_directory("blocked");
        // A file standing where the directory should be.
        let blocked = directory.join("in-the-way");
        std::fs::write(&blocked, b"not a directory").expect("seed a file");

        let Err(error) = FileOutputWorker::start(
            blocked.join("captures"),
            captastic_config::DEFAULT_FILENAME_TEMPLATE.to_owned(),
            OutputFormat::Png,
            EncodeOptions::default(),
            no_history(),
            false,
            1,
        ) else {
            panic!("a blocked directory must fail at startup");
        };
        assert!(error.to_string().contains("output directory"), "{error}");

        std::fs::remove_dir_all(directory).expect("clean up");
    }
}
