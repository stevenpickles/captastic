//! The daemon's end of the request channel (ADR 0014): a thread that takes one request at a time
//! from the pipe, answers it, and hands any capture to the capture worker.
//!
//! It runs beside the capture path, never on it. A client that is slow to write or to read costs
//! this thread a timeout and nobody else anything, and a capture it asks for waits in the capture
//! worker's queue behind the user's own, as a hotkey press would.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use captastic_config::ConfirmedRegion;
use std::thread;
use std::time::Duration;

use captastic_core::{
    encode_capture, CaptureSource, CpuFrame, DisplayId, DisplayInfo, EncodeOptions, OutputFormat,
};
use serde_json::{json, Map, Value};

use crate::command_channel::{
    answer_line, choose_window, parse_request, plan_capture, plan_last_region, Answer,
    CaptureCommand, Command, Failure, Reason, Target, WindowInfo, MAX_REQUEST_BYTES,
};

/// How long a client has to send its request, and to read its answer.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a request waits on the capture worker, which may be busy with the user's own capture.
const ENGINE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `--interactive` waits for the user to choose on the overlay (the maintainer's choice of
/// 2026-11-22) before closing it.
pub(crate) const INTERACTIVE_CHOICE_TIMEOUT: Duration = Duration::from_secs(120);
/// How much longer it waits once the user has confirmed: they chose Edit and are in the editor.
pub(crate) const INTERACTIVE_EDIT_TIMEOUT: Duration = Duration::from_secs(600);

/// Where a command-line request's frames and displays come from: the daemon's capture worker, the
/// command line's own one-shot backend when no daemon is running, or a test's fake backend.
pub(crate) trait CommandEngine {
    fn displays(&self) -> Result<Vec<DisplayInfo>, Failure>;
    fn frame(&self, source: CaptureSource) -> Result<CpuFrame, Failure>;
    /// The windows that may be captured, frontmost first.
    fn windows(&self, displays: &[DisplayInfo]) -> Result<Vec<WindowInfo>, Failure>;
    /// One window, captured as the overlay captures a window it is given.
    fn window_frame(&self, window: &WindowInfo) -> Result<CpuFrame, Failure>;
    /// The regions last confirmed on the overlay, by display id.
    fn last_regions(&self) -> BTreeMap<String, ConfirmedRegion>;
    /// Puts a capture on the clipboard, under the user's `[clipboard]` settings.
    fn publish_clipboard(&self, frame: &CpuFrame) -> Result<(), String>;
    /// Opens the overlay for the user to choose, and waits for what they confirm: the pixels and
    /// a description of the choice.
    fn interactive(&self) -> Result<(CpuFrame, Value), Failure>;
}

/// Where a capture is written when the request names no path.
pub(crate) struct CommandOutput {
    pub(crate) directory: PathBuf,
    pub(crate) filename_template: String,
    pub(crate) format: OutputFormat,
    pub(crate) encode_options: EncodeOptions,
}

/// Answers requests.
pub(crate) struct Commands<E> {
    pub(crate) engine: E,
    pub(crate) output: CommandOutput,
    /// Where the announcement of every capture goes (ADR 0014). `None` only in tests.
    pub(crate) notices: Option<mpsc::SyncSender<crate::daemon::DaemonNotice>>,
    /// The display the pointer is on, for a `pointer` target.
    pub(crate) pointer: fn(&[DisplayInfo]) -> Result<DisplayId, Failure>,
    /// Who took a capture, as the answer reports it: `daemon` or `one_shot`.
    pub(crate) taken_by: &'static str,
    /// Where every capture is remembered, marked as the command line's. `None` only in tests
    /// that do not look at history.
    pub(crate) history: Option<crate::file_output::HistoryRecorder>,
}

impl<E: CommandEngine> Commands<E> {
    pub(crate) fn answer(&self, command: Command) -> Answer {
        match command {
            Command::Status => {
                let mut fields = Map::new();
                fields.insert("running".to_owned(), json!(true));
                fields.insert(
                    "captastic_version".to_owned(),
                    json!(crate::build_info::BUILD_VERSION),
                );
                fields.insert("pid".to_owned(), json!(std::process::id()));
                Ok(fields)
            }
            Command::Displays => {
                let displays = self.engine.displays()?;
                let mut fields = Map::new();
                fields.insert("displays".to_owned(), json!(displays));
                Ok(fields)
            }
            Command::Windows => {
                let displays = self.engine.displays()?;
                let windows = self.engine.windows(&displays)?;
                let mut fields = Map::new();
                fields.insert("windows".to_owned(), json!(windows));
                Ok(fields)
            }
            Command::Capture(capture) => self.capture(capture),
        }
    }

    fn capture(&self, capture: CaptureCommand) -> Answer {
        let displays = self.engine.displays()?;
        let (frame, described) = match &capture.target {
            Target::Interactive => {
                let (frame, chosen) = self.engine.interactive()?;
                (frame, json!({ "kind": "interactive", "chosen": chosen }))
            }
            Target::Window { .. } | Target::WindowTitle { .. } | Target::Process { .. } => {
                let window = choose_window(&capture.target, &self.engine.windows(&displays)?)?;
                let frame = self.engine.window_frame(&window)?;
                let mut described = json!(window);
                described["kind"] = json!("window");
                (frame, described)
            }
            target => {
                let plan = match target {
                    Target::LastRegion => {
                        plan_last_region(&self.engine.last_regions(), &displays, || {
                            (self.pointer)(&displays)
                        })?
                    }
                    _ => plan_capture(target, &displays, || (self.pointer)(&displays))?,
                };
                let frame = self.engine.frame(plan.source.clone())?;
                let frame = match plan.crop {
                    Some(area) => frame.crop(area).map_err(|error| {
                        Failure::new(
                            Reason::CaptureFailed,
                            format!("the capture could not be cropped to the rectangle: {error}"),
                        )
                    })?,
                    None => frame,
                };
                (frame, plan.described)
            }
        };
        let format = capture.format.unwrap_or(self.output.format);
        let path = self.write(&frame, format, capture.output.as_deref())?;
        // Only when asked: a capture the user did not start must not replace what they copied.
        if capture.clipboard {
            self.engine.publish_clipboard(&frame).map_err(|error| {
                Failure::new(
                    Reason::ClipboardFailed,
                    format!("the capture was saved and could not be put on the clipboard: {error}"),
                )
                .with_detail("path", json!(path.display().to_string()))
            })?;
        }
        if let Some(history) = &self.history {
            history.record_command_line(
                &path,
                &frame,
                described["kind"].as_str().unwrap_or("command_line"),
            );
        }
        log::info!(
            "a command-line request captured {}x{} to {}",
            frame.width(),
            frame.height(),
            path.display()
        );
        if let Some(notices) = &self.notices {
            let _ = notices.try_send(crate::daemon::DaemonNotice::CommandLineCapture {
                file_name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path: path.clone(),
            });
        }
        let mut fields = Map::new();
        fields.insert("path".to_owned(), json!(path.display().to_string()));
        fields.insert("format".to_owned(), json!(format));
        fields.insert("width".to_owned(), json!(frame.width()));
        fields.insert("height".to_owned(), json!(frame.height()));
        fields.insert("captured".to_owned(), described);
        fields.insert("taken_by".to_owned(), json!(self.taken_by));
        fields.insert("clipboard".to_owned(), json!(capture.clipboard));
        fields.insert("capture_id".to_owned(), json!(frame.metadata.capture_id.0));
        Ok(fields)
    }

    fn write(
        &self,
        frame: &CpuFrame,
        format: OutputFormat,
        output: Option<&Path>,
    ) -> Result<PathBuf, Failure> {
        let write_failed = |message: String| Failure::new(Reason::WriteFailed, message);
        let Some(path) = output else {
            // The output directory, under the filename template, never over an existing file.
            let (path, ..) = crate::file_output::write_capture_now(
                &self.output.directory,
                &self.output.filename_template,
                captastic_config::HotkeyAction::FullDisplay,
                frame,
                format,
                &self.output.encode_options,
            )
            .map_err(|error| write_failed(error.to_string()))?;
            return Ok(path);
        };
        if !path.is_absolute() {
            // The daemon's working directory is not the caller's; the command line resolves it.
            return Err(Failure::new(
                Reason::BadRequest,
                "an output path must be absolute",
            ));
        }
        let encoded = encode_capture(frame, format, &self.output.encode_options)
            .map_err(|error| write_failed(format!("the capture could not be encoded: {error}")))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                write_failed(format!(
                    "{} could not be created: {error}",
                    parent.display()
                ))
            })?;
        }
        // As for any capture: never over a file that is already there.
        captastic_config::atomic_write(path, &encoded.bytes, captastic_config::finalize_new)
            .map_err(|error| {
                write_failed(format!("{} was not written: {error}", path.display()))
            })?;
        Ok(path.to_path_buf())
    }
}

/// The listening thread, stopped and joined when dropped.
pub(crate) struct CommandListener {
    stopper: captastic_windows::RequestPipeStopper,
    join: Option<thread::JoinHandle<()>>,
}

impl CommandListener {
    pub(crate) fn start<E: CommandEngine + Send + 'static>(
        server: captastic_windows::RequestPipeServer,
        commands: Commands<E>,
    ) -> std::io::Result<Self> {
        let stopper = server.stopper();
        let join = thread::Builder::new()
            .name("captastic-requests".to_owned())
            .spawn(move || serve(server, &commands))?;
        Ok(Self {
            stopper,
            join: Some(join),
        })
    }
}

impl Drop for CommandListener {
    fn drop(&mut self) {
        self.stopper.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn serve<E: CommandEngine>(
    mut server: captastic_windows::RequestPipeServer,
    commands: &Commands<E>,
) {
    let mut failures = 0_u32;
    loop {
        let mut connection = match server.accept() {
            Ok(Some(connection)) => connection,
            Ok(None) => return,
            Err(error) => {
                failures += 1;
                crate::logging::warn(format_args!(
                    "the command-line request pipe could not take a client: {error}"
                ));
                if failures >= 5 {
                    crate::logging::warn(format_args!(
                        "the command-line request pipe is closed after repeated failures"
                    ));
                    return;
                }
                continue;
            }
        };
        failures = 0;
        let request = match connection.read_line(MAX_REQUEST_BYTES, CLIENT_TIMEOUT) {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(error) => {
                log::info!("a command-line request was not read: {error}");
                continue;
            }
        };
        let answer = match parse_request(&request) {
            Ok(request) => commands.answer(request.command),
            Err(failure) => Err(failure),
        };
        if let Err(failure) = &answer {
            log::info!(
                "a command-line request got no result ({:?}): {}",
                failure.reason,
                failure.message
            );
        }
        if let Err(error) = connection.write_line(&answer_line(&answer), CLIENT_TIMEOUT) {
            log::info!("a command-line answer was not delivered: {error}");
        }
    }
}

/// The daemon's capture worker, asked through its command queue.
pub(crate) struct DaemonEngine {
    pub(crate) commands: mpsc::SyncSender<crate::daemon::CaptureCommand>,
    /// The overlay, to close one nobody answered in time. `None` when selection is turned off.
    pub(crate) overlay: Option<captastic_windows::OverlayController>,
    /// The regions the overlay last confirmed, shared with the repeat-last-region hotkey.
    pub(crate) confirmed_regions: crate::selection::ConfirmedRegionCache,
    pub(crate) clipboard_retention: captastic_windows::ClipboardRetention,
}

/// What the capture worker is asked for on a command-line request's behalf.
pub(crate) enum EngineRequest {
    Displays {
        reply: mpsc::SyncSender<Result<Vec<DisplayInfo>, Failure>>,
    },
    Frame {
        source: CaptureSource,
        reply: mpsc::SyncSender<Result<CpuFrame, Failure>>,
    },
}

impl DaemonEngine {
    fn ask<T>(
        &self,
        request: impl FnOnce(mpsc::SyncSender<Result<T, Failure>>) -> EngineRequest,
    ) -> Result<T, Failure> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .try_send(crate::daemon::CaptureCommand::CommandLine(Box::new(
                request(reply),
            )))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => Failure::new(
                    Reason::EngineUnavailable,
                    "the capture engine is busy; try again",
                ),
                mpsc::TrySendError::Disconnected(_) => {
                    Failure::new(Reason::ShuttingDown, "Captastic is stopping")
                }
            })?;
        answer.recv_timeout(ENGINE_TIMEOUT).map_err(|_| {
            Failure::new(
                Reason::EngineUnavailable,
                "the capture engine did not answer in time",
            )
        })?
    }
}

impl CommandEngine for DaemonEngine {
    fn displays(&self) -> Result<Vec<DisplayInfo>, Failure> {
        self.ask(|reply| EngineRequest::Displays { reply })
    }

    fn frame(&self, source: CaptureSource) -> Result<CpuFrame, Failure> {
        self.ask(|reply| EngineRequest::Frame { source, reply })
    }

    fn windows(&self, displays: &[DisplayInfo]) -> Result<Vec<WindowInfo>, Failure> {
        native_windows(displays)
    }

    fn last_regions(&self) -> BTreeMap<String, ConfirmedRegion> {
        self.confirmed_regions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn publish_clipboard(&self, frame: &CpuFrame) -> Result<(), String> {
        publish_to_clipboard(frame, self.clipboard_retention)
    }

    fn interactive(&self) -> Result<(CpuFrame, Value), Failure> {
        use crate::selection::{CommandLineSelection, CommandLineSelectionState};
        use std::sync::atomic::Ordering;
        let Some(overlay) = &self.overlay else {
            return Err(Failure::new(
                Reason::Unsupported,
                "the overlay is turned off in this Captastic's configuration",
            ));
        };
        let (reply, answer) = mpsc::sync_channel(1);
        let state = std::sync::Arc::new(CommandLineSelectionState::default());
        self.commands
            .try_send(crate::daemon::command_line_trigger(CommandLineSelection {
                reply,
                state: state.clone(),
            }))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => Failure::new(
                    Reason::EngineUnavailable,
                    "the capture engine is busy; try again",
                ),
                mpsc::TrySendError::Disconnected(_) => {
                    Failure::new(Reason::ShuttingDown, "Captastic is stopping")
                }
            })?;
        let mut deadline = std::time::Instant::now() + INTERACTIVE_CHOICE_TIMEOUT;
        let mut extended = false;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match answer.recv_timeout(left.max(Duration::from_millis(1))) {
                Ok(outcome) => return interactive_answer(outcome),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Failure::new(
                        Reason::CaptureFailed,
                        "the overlay closed without an answer",
                    ))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if state.confirmed.load(Ordering::SeqCst) && !extended {
                        // Confirmed and in the editor: the choice is made, so give the edit time.
                        deadline += INTERACTIVE_EDIT_TIMEOUT;
                        extended = true;
                        continue;
                    }
                    // Given up: an overlay still queued is never opened, and one on screen closes.
                    state.abandoned.store(true, Ordering::SeqCst);
                    if state.showing.load(Ordering::SeqCst) {
                        overlay.cancel();
                    }
                    return Err(Failure::new(
                        Reason::TimedOut,
                        if extended {
                            "nobody finished editing in time; nothing was captured"
                        } else {
                            "nobody chose on the overlay within two minutes; it was closed"
                        },
                    ));
                }
            }
        }
    }

    fn window_frame(&self, window: &WindowInfo) -> Result<CpuFrame, Failure> {
        // The window's display through the capture worker first: it waits behind the user's own
        // captures, is refused while a recording runs, and gives the window capture the reference
        // frame the overlay would give it.
        let reference = self.frame(window_display_source(window))?;
        capture_native_window(window, &reference)
    }
}

/// What the overlay's answer means for the request.
fn interactive_answer(
    outcome: crate::selection::CommandLineOutcome,
) -> Result<(CpuFrame, Value), Failure> {
    use crate::selection::CommandLineOutcome;
    match outcome {
        CommandLineOutcome::Selected { frame, described } => Ok((*frame, described)),
        CommandLineOutcome::Cancelled => Err(Failure::new(
            Reason::Cancelled,
            "the overlay was closed without choosing",
        )),
        CommandLineOutcome::RecordingRefused => Err(Failure::new(
            Reason::RecordingUnsupported,
            "Record was chosen; the command line takes still captures only",
        )),
        CommandLineOutcome::RecordingRunning => Err(Failure::new(
            Reason::RecordingRunning,
            "a recording is running; still captures wait until it ends",
        )),
        CommandLineOutcome::Failed(message) => Err(Failure::new(Reason::CaptureFailed, message)),
    }
}

/// Puts a capture on the clipboard as the daemon's clipboard destination does, keeping it out of
/// clipboard history and cloud sync unless the user's `[clipboard]` settings allow them.
pub(crate) fn publish_to_clipboard(
    frame: &CpuFrame,
    retention: captastic_windows::ClipboardRetention,
) -> Result<(), String> {
    let payload = captastic_windows::ClipboardPayload::prepare(frame).map_err(|e| e.to_string())?;
    let mut publisher =
        captastic_windows::ClipboardPublisher::new(retention).map_err(|e| e.to_string())?;
    publisher
        .publish(&payload)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The windows Win32 lets be captured, as the overlay's chooser finds them, frontmost first.
pub(crate) fn native_windows(displays: &[DisplayInfo]) -> Result<Vec<WindowInfo>, Failure> {
    let windows = captastic_windows::capturable_windows(displays).map_err(|error| {
        Failure::new(
            Reason::CaptureFailed,
            format!("the windows could not be listed: {error}"),
        )
    })?;
    Ok(windows
        .into_iter()
        .map(|window| WindowInfo {
            handle: window.handle.raw() as u64,
            title: window.title,
            process: window.process,
            rect: window.frame,
            display: window.display_id.map(|id| id.0),
        })
        .collect())
}

/// The display a window's reference frame comes from.
pub(crate) fn window_display_source(window: &WindowInfo) -> CaptureSource {
    window
        .display
        .clone()
        .map_or(CaptureSource::VirtualDesktop, |id| {
            CaptureSource::Display(DisplayId(id))
        })
}

/// Captures one window as the overlay does, against a frame of its display.
pub(crate) fn capture_native_window(
    window: &WindowInfo,
    reference: &CpuFrame,
) -> Result<CpuFrame, Failure> {
    let handle = captastic_windows::NativeWindowHandle::from_raw(window.handle as isize);
    captastic_windows::capture_window_frame(handle, &reference.metadata).map_err(|error| {
        Failure::new(
            Reason::CaptureFailed,
            format!("the window could not be captured: {error}"),
        )
    })
}

/// The pointer's display, as the daemon finds it.
pub(crate) fn pointer_display(displays: &[DisplayInfo]) -> Result<DisplayId, Failure> {
    captastic_windows::display_containing_pointer(displays).map_err(|error| {
        Failure::new(
            Reason::CaptureFailed,
            format!("the pointer's display could not be found: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use captastic_core::{CaptureBackend, CaptureMode, CaptureRequest, EventRecorder, Rect};
    use serde_json::Value;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    /// The fake backend, answering as the capture worker would.
    struct FakeEngine {
        backend: Mutex<captastic_core::FakeBackend>,
        recording: bool,
        last_regions: BTreeMap<String, ConfirmedRegion>,
        /// What the overlay answers: the area the user chose, or why there is none.
        interactive: Option<Result<Rect, Reason>>,
        /// The size of every capture put on the clipboard.
        clipboard: Mutex<Vec<(u32, u32)>>,
    }

    impl FakeEngine {
        fn new(recording: bool) -> Self {
            Self {
                backend: Mutex::new(captastic_core::FakeBackend::new(
                    captastic_core::FakeBackendConfig {
                        width: 320,
                        height: 200,
                        ..Default::default()
                    },
                )),
                recording,
                last_regions: BTreeMap::new(),
                interactive: None,
                clipboard: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandEngine for FakeEngine {
        fn displays(&self) -> Result<Vec<DisplayInfo>, Failure> {
            Ok(self.backend.lock().expect("backend").displays().to_vec())
        }

        fn windows(&self, _displays: &[DisplayInfo]) -> Result<Vec<WindowInfo>, Failure> {
            Ok(fake_windows())
        }

        fn last_regions(&self) -> BTreeMap<String, ConfirmedRegion> {
            self.last_regions.clone()
        }

        fn interactive(&self) -> Result<(CpuFrame, Value), Failure> {
            match &self.interactive {
                Some(Ok(area)) => {
                    let display = self.displays()?[0].id.clone();
                    let frame = self
                        .frame(CaptureSource::Display(display))?
                        .crop(*area)
                        .map_err(|error| Failure::new(Reason::CaptureFailed, error.to_string()))?;
                    Ok((
                        frame,
                        json!({ "kind": "region", "rect": area, "intent": "capture" }),
                    ))
                }
                Some(Err(reason)) => Err(Failure::new(*reason, "as the test asked")),
                None => Err(Failure::new(Reason::Unsupported, "no overlay here")),
            }
        }

        fn publish_clipboard(&self, frame: &CpuFrame) -> Result<(), String> {
            self.clipboard
                .lock()
                .expect("clipboard")
                .push((frame.width(), frame.height()));
            Ok(())
        }

        fn window_frame(&self, window: &WindowInfo) -> Result<CpuFrame, Failure> {
            // The fake display, cropped to the window: what a window capture of it would hold.
            let display = self.displays()?[0].id.clone();
            let frame = self.frame(CaptureSource::Display(display))?;
            frame
                .crop(window.rect)
                .map_err(|error| Failure::new(Reason::CaptureFailed, error.to_string()))
        }

        fn frame(&self, source: CaptureSource) -> Result<CpuFrame, Failure> {
            if self.recording {
                return Err(Failure::new(Reason::RecordingRunning, "a recording runs"));
            }
            let request = CaptureRequest {
                id: captastic_core::CaptureId(9),
                triggered_at: Instant::now(),
                source,
                mode: CaptureMode::Latest { max_age_ms: None },
                cpu_frame: true,
                retain_native_frame: false,
                cursor: captastic_core::CursorMode::Exclude,
            };
            let mut recorder = EventRecorder::with_capacity(16);
            self.backend
                .lock()
                .expect("backend")
                .capture(&request, &mut recorder)
                .map_err(|error| Failure::new(Reason::CaptureFailed, error.to_string()))?
                .frame
                .ok_or_else(|| Failure::new(Reason::CaptureFailed, "no frame"))
        }
    }

    /// Three windows on the fake 320x200 display, frontmost first: two of them Notepad's.
    fn fake_windows() -> Vec<WindowInfo> {
        let window = |handle, title: &str, process: &str, x| WindowInfo {
            handle,
            title: title.to_owned(),
            process: Some(process.to_owned()),
            rect: Rect {
                x,
                y: 10,
                width: 60,
                height: 40,
            },
            display: None,
        };
        vec![
            window(0x10, "notes.txt - Notepad", "Notepad.exe", 0),
            window(0x20, "Build log - Terminal", "WindowsTerminal.exe", 100),
            window(0x30, "todo.txt - Notepad", "notepad.exe", 200),
        ]
    }

    /// A directory of its own for one test, removed afterwards.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let directory = std::env::temp_dir().join(format!(
                "captastic-command-listener-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&directory);
            std::fs::create_dir_all(&directory).expect("scratch directory");
            Self(directory)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn commands(directory: &Path, recording: bool) -> Commands<FakeEngine> {
        Commands {
            engine: FakeEngine::new(recording),
            output: CommandOutput {
                directory: directory.to_path_buf(),
                filename_template: "shot-{timestamp}".to_owned(),
                format: OutputFormat::Png,
                encode_options: EncodeOptions::default(),
            },
            notices: None,
            pointer: |_| panic!("the pointer was asked for"),
            taken_by: "daemon",
            history: None,
        }
    }

    fn test_pipe_name() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            r"\\.\pipe\captastic-listener-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn ask(name: &str, request: &str) -> Value {
        let answer = captastic_windows::send_request(
            name,
            request.as_bytes(),
            crate::command_channel::MAX_ANSWER_BYTES,
            Duration::from_secs(10),
        )
        .expect("an answer");
        serde_json::from_slice(&answer).expect("JSON")
    }

    fn primary_bounds(commands: &Commands<FakeEngine>) -> Rect {
        commands.engine.displays().expect("displays")[0].bounds
    }

    #[test]
    fn a_request_down_the_pipe_is_answered_and_its_capture_written() {
        let directory = Scratch::new();
        let name = test_pipe_name();
        let server = captastic_windows::RequestPipeServer::create_named(&name).expect("server");
        let commands = commands(directory.path(), false);
        let bounds = primary_bounds(&commands);
        let _listener = CommandListener::start(server, commands).expect("listener");

        let status = ask(&name, r#"{"version":1,"command":"status"}"#);
        assert_eq!(status["status"], "ok");
        assert_eq!(status["pid"], std::process::id());
        assert_eq!(status["version"], 1);
        assert_eq!(
            status["captastic_version"],
            crate::build_info::BUILD_VERSION
        );

        let displays = ask(&name, r#"{"version":1,"command":"displays"}"#);
        assert_eq!(displays["displays"][0]["bounds"]["width"], bounds.width);

        let request = json!({
            "version": 1,
            "command": "capture",
            "target": { "kind": "rect", "rect": { "x": bounds.x + 10, "y": bounds.y + 20, "width": 64, "height": 48 } },
        });
        let captured = ask(&name, &request.to_string());
        assert_eq!(captured["status"], "ok", "{captured}");
        assert_eq!(
            (captured["width"].clone(), captured["height"].clone()),
            (json!(64), json!(48))
        );
        assert_eq!(captured["taken_by"], "daemon");
        let path = PathBuf::from(captured["path"].as_str().expect("path"));
        assert!(path.starts_with(directory.path()), "{}", path.display());
        let png = std::fs::read(&path).expect("the capture was written");
        assert_eq!(&png[1..4], b"PNG");

        // A display, to a path of the caller's choosing.
        let chosen = directory.path().join("chosen").join("whole.bmp");
        let request = json!({
            "version": 1,
            "command": "capture",
            "target": { "kind": "display", "display": "primary" },
            "output": chosen,
            "format": "bmp",
        });
        let whole = ask(&name, &request.to_string());
        assert_eq!(whole["status"], "ok", "{whole}");
        assert_eq!(whole["width"], bounds.width);
        assert_eq!(&std::fs::read(&chosen).expect("written")[..2], b"BM");
        // And not over it a second time.
        let again = ask(&name, &request.to_string());
        assert_eq!(again["status"], "failed");
        assert_eq!(again["reason"], "write_failed");
    }

    #[test]
    fn what_cannot_be_captured_is_refused_and_says_why() {
        let directory = Scratch::new();
        let idle = commands(directory.path(), false);
        let reason = |commands: &Commands<FakeEngine>, target: Value| {
            let request: crate::command_channel::Request = serde_json::from_value(json!({
                "version": 1, "command": "capture", "target": target,
            }))
            .expect("request");
            commands
                .answer(request.command)
                .expect_err("no capture")
                .reason
        };
        assert_eq!(
            reason(&idle, json!({ "kind": "display", "display": "nowhere" })),
            Reason::UnknownDisplay
        );
        assert_eq!(
            reason(
                &idle,
                json!({ "kind": "rect", "rect": { "x": -50000, "y": 0, "width": 10, "height": 10 } })
            ),
            Reason::OutsideDesktop
        );
        assert_eq!(
            reason(&idle, json!({ "kind": "window", "handle": 42 })),
            Reason::NoWindow
        );
        assert_eq!(
            reason(&idle, json!({ "kind": "window_title", "title": "Notepad" })),
            Reason::AmbiguousWindow
        );
        assert_eq!(
            reason(&idle, json!({ "kind": "last_region" })),
            Reason::NoLastRegion
        );
        assert_eq!(
            reason(&idle, json!({ "kind": "interactive" })),
            Reason::Unsupported
        );
        let recording = commands(directory.path(), true);
        assert_eq!(
            reason(
                &recording,
                json!({ "kind": "display", "display": "primary" })
            ),
            Reason::RecordingRunning
        );
        assert_eq!(
            reason(&recording, json!({ "kind": "window", "handle": 0x20 })),
            Reason::RecordingRunning
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).expect("list").count(),
            0,
            "nothing was written"
        );
    }

    #[test]
    fn windows_are_listed_and_one_is_captured_by_handle_title_or_process() {
        let directory = Scratch::new();
        let name = test_pipe_name();
        let server = captastic_windows::RequestPipeServer::create_named(&name).expect("server");
        let _listener =
            CommandListener::start(server, commands(directory.path(), false)).expect("listener");

        let listed = ask(&name, r#"{"version":1,"command":"windows"}"#);
        assert_eq!(listed["status"], "ok", "{listed}");
        assert_eq!(listed["windows"].as_array().expect("windows").len(), 3);
        assert_eq!(listed["windows"][1]["title"], "Build log - Terminal");
        assert_eq!(listed["windows"][1]["handle"], 0x20);

        for target in [
            json!({ "kind": "window", "handle": 0x20 }),
            json!({ "kind": "window_title", "title": "build LOG" }),
            json!({ "kind": "process", "name": "windowsterminal" }),
        ] {
            let request = json!({ "version": 1, "command": "capture", "target": target });
            let captured = ask(&name, &request.to_string());
            assert_eq!(captured["status"], "ok", "{captured}");
            assert_eq!(
                (captured["width"].clone(), captured["height"].clone()),
                (json!(60), json!(40))
            );
            assert_eq!(captured["captured"]["kind"], "window");
            assert_eq!(captured["captured"]["handle"], 0x20);
            assert_eq!(captured["captured"]["process"], "WindowsTerminal.exe");
            let path = PathBuf::from(captured["path"].as_str().expect("path"));
            assert_eq!(&std::fs::read(&path).expect("written")[1..4], b"PNG");
        }

        // Two Notepads: refused with both listed, and nothing written for it.
        let written = std::fs::read_dir(directory.path()).expect("list").count();
        let request = json!({
            "version": 1, "command": "capture",
            "target": { "kind": "process", "name": "notepad.exe" },
        });
        let ambiguous = ask(&name, &request.to_string());
        assert_eq!(ambiguous["status"], "refused", "{ambiguous}");
        assert_eq!(ambiguous["reason"], "ambiguous_window");
        let matches = ambiguous["matches"].as_array().expect("matches");
        assert_eq!(
            matches
                .iter()
                .map(|m| m["handle"].clone())
                .collect::<Vec<_>>(),
            vec![json!(0x10), json!(0x30)]
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).expect("list").count(),
            written
        );
    }

    #[test]
    fn the_last_region_is_captured_and_the_clipboard_only_when_asked() {
        let directory = Scratch::new();
        let mut commands = commands(directory.path(), false);
        let display = commands.engine.displays().expect("displays")[0].clone();
        commands.engine.last_regions.insert(
            display.id.0.clone(),
            ConfirmedRegion {
                region: captastic_config::CaptureRegion {
                    x: 20,
                    y: 10,
                    width: 50,
                    height: 30,
                },
                source: captastic_config::CaptureRegionSource {
                    width: display.bounds.width,
                    height: display.bounds.height,
                    rotation_degrees: display.rotation_degrees,
                },
            },
        );
        let answer = |request: Value| {
            let request: crate::command_channel::Request =
                serde_json::from_value(request).expect("request");
            commands.answer(request.command)
        };
        let repeated = answer(json!({
            "version": 1, "command": "capture", "target": { "kind": "last_region" },
        }))
        .expect("the last region");
        assert_eq!(
            (repeated["width"].clone(), repeated["height"].clone()),
            (json!(50), json!(30))
        );
        assert_eq!(repeated["captured"]["kind"], "last_region");
        assert_eq!(repeated["clipboard"], false);
        assert!(commands
            .engine
            .clipboard
            .lock()
            .expect("clipboard")
            .is_empty());

        let copied = answer(json!({
            "version": 1, "command": "capture", "target": { "kind": "last_region" },
            "clipboard": true,
        }))
        .expect("to the clipboard too");
        assert_eq!(copied["clipboard"], true);
        assert!(PathBuf::from(copied["path"].as_str().expect("path")).exists());
        assert_eq!(
            *commands.engine.clipboard.lock().expect("clipboard"),
            vec![(50, 30)]
        );
    }

    #[test]
    fn an_interactive_choice_is_written_and_a_cancel_or_record_is_refused() {
        let directory = Scratch::new();
        let mut commands = commands(directory.path(), false);
        let interactive = json!({
            "version": 1, "command": "capture", "target": { "kind": "interactive" },
        });
        let ask = |commands: &Commands<FakeEngine>| {
            let request: crate::command_channel::Request =
                serde_json::from_value(interactive.clone()).expect("request");
            commands.answer(request.command)
        };
        commands.engine.interactive = Some(Ok(Rect {
            x: 5,
            y: 6,
            width: 70,
            height: 40,
        }));
        let chosen = ask(&commands).expect("chosen");
        assert_eq!(
            (chosen["width"].clone(), chosen["height"].clone()),
            (json!(70), json!(40))
        );
        assert_eq!(chosen["captured"]["kind"], "interactive");
        assert_eq!(chosen["captured"]["chosen"]["kind"], "region");
        assert!(PathBuf::from(chosen["path"].as_str().expect("path")).exists());

        let written = std::fs::read_dir(directory.path()).expect("list").count();
        for reason in [
            Reason::Cancelled,
            Reason::TimedOut,
            Reason::RecordingUnsupported,
            Reason::RecordingRunning,
        ] {
            commands.engine.interactive = Some(Err(reason));
            let refused = ask(&commands).expect_err("no capture");
            assert_eq!(refused.reason, reason);
            assert!(refused.reason.is_refusal(), "{reason:?} is exit code 2");
        }
        assert_eq!(
            std::fs::read_dir(directory.path()).expect("list").count(),
            written
        );
    }

    #[test]
    fn the_overlay_answer_is_read_as_the_request_needs_it() {
        use crate::selection::CommandLineOutcome;
        let reason = |outcome| interactive_answer(outcome).expect_err("refused").reason;
        assert_eq!(reason(CommandLineOutcome::Cancelled), Reason::Cancelled);
        assert_eq!(
            reason(CommandLineOutcome::RecordingRefused),
            Reason::RecordingUnsupported
        );
        assert_eq!(
            reason(CommandLineOutcome::RecordingRunning),
            Reason::RecordingRunning
        );
        assert_eq!(
            reason(CommandLineOutcome::Failed("gone".to_owned())),
            Reason::CaptureFailed
        );
    }

    #[test]
    fn a_command_line_capture_is_remembered_as_the_command_lines() {
        let directory = Scratch::new();
        let mut commands = commands(directory.path(), false);
        let store =
            captastic_config::HistoryStore::for_config(directory.path().join("captastic.toml"));
        commands.history = Some(crate::file_output::HistoryRecorder::new(
            store.clone(),
            captastic_config::RetentionPolicy {
                max_items: 10,
                max_age: None,
                max_total_bytes: None,
            },
        ));
        let bounds = primary_bounds(&commands);
        let request: crate::command_channel::Request = serde_json::from_value(json!({
            "version": 1, "command": "capture",
            "target": { "kind": "rect", "rect": { "x": bounds.x, "y": bounds.y, "width": 40, "height": 30 } },
        }))
        .expect("request");
        let captured = commands.answer(request.command).expect("captured");
        let history = store.load().expect("history");
        let entry = &history.entries[0];
        assert_eq!(entry.origin, captastic_config::COMMAND_LINE_ORIGIN);
        assert_eq!(entry.mode, "rect");
        assert_eq!((entry.width, entry.height), (40, 30));
        assert_eq!(
            entry.path,
            PathBuf::from(captured["path"].as_str().expect("path"))
        );
        assert!(entry.bytes > 0);
    }

    #[test]
    fn a_relative_output_path_is_refused() {
        let directory = Scratch::new();
        let commands = commands(directory.path(), false);
        let request: crate::command_channel::Request = serde_json::from_value(json!({
            "version": 1, "command": "capture",
            "target": { "kind": "display", "display": "primary" },
            "output": "shot.png",
        }))
        .expect("request");
        assert_eq!(
            commands
                .answer(request.command)
                .expect_err("refused")
                .reason,
            Reason::BadRequest
        );
    }

    #[test]
    fn the_listener_stops_when_dropped() {
        let name = test_pipe_name();
        let server = captastic_windows::RequestPipeServer::create_named(&name).expect("server");
        let directory = Scratch::new();
        let listener =
            CommandListener::start(server, commands(directory.path(), false)).expect("listener");
        let started = Instant::now();
        drop(listener);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            captastic_windows::send_request(&name, b"{}", 1024, Duration::from_secs(1)),
            Err(captastic_windows::RequestError::NotRunning)
        ));
    }
}
