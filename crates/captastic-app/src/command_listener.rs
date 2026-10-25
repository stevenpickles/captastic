//! The daemon's end of the request channel (ADR 0014): a thread that takes one request at a time
//! from the pipe, answers it, and hands any capture to the capture worker.
//!
//! It runs beside the capture path, never on it. A client that is slow to write or to read costs
//! this thread a timeout and nobody else anything, and a capture it asks for waits in the capture
//! worker's queue behind the user's own, as a hotkey press would.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use captastic_core::{
    encode_capture, CaptureSource, CpuFrame, DisplayId, DisplayInfo, EncodeOptions, OutputFormat,
};
use serde_json::{json, Map};

use crate::command_channel::{
    answer_line, parse_request, plan_capture, Answer, CaptureCommand, Command, Failure, Reason,
    MAX_REQUEST_BYTES,
};

/// How long a client has to send its request, and to read its answer.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a request waits on the capture worker, which may be busy with the user's own capture.
const ENGINE_TIMEOUT: Duration = Duration::from_secs(15);

/// Where a command-line request's frames and displays come from: the daemon's capture worker, or a
/// test's fake backend.
pub(crate) trait CommandEngine: Send + 'static {
    fn displays(&self) -> Result<Vec<DisplayInfo>, Failure>;
    fn frame(&self, source: CaptureSource) -> Result<CpuFrame, Failure>;
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
}

impl<E: CommandEngine> Commands<E> {
    pub(crate) fn answer(&self, command: Command) -> Answer {
        match command {
            Command::Status => {
                let mut fields = Map::new();
                fields.insert("running".to_owned(), json!(true));
                fields.insert(
                    "version".to_owned(),
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
            Command::Capture(capture) => self.capture(capture),
        }
    }

    fn capture(&self, capture: CaptureCommand) -> Answer {
        if capture.clipboard {
            return Err(Failure::new(
                Reason::Unsupported,
                "this Captastic does not put command-line captures on the clipboard yet",
            ));
        }
        let displays = self.engine.displays()?;
        let plan = plan_capture(&capture.target, &displays, || (self.pointer)(&displays))?;
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
        let format = capture.format.unwrap_or(self.output.format);
        let path = self.write(&frame, format, capture.output.as_deref())?;
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
        fields.insert("captured".to_owned(), plan.described);
        fields.insert("taken_by".to_owned(), json!("daemon"));
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
    pub(crate) fn start<E: CommandEngine>(
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
            }
        }
    }

    impl CommandEngine for FakeEngine {
        fn displays(&self) -> Result<Vec<DisplayInfo>, Failure> {
            Ok(self.backend.lock().expect("backend").displays().to_vec())
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
            std::fs::read_dir(directory.path()).expect("list").count(),
            0,
            "nothing was written"
        );
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
