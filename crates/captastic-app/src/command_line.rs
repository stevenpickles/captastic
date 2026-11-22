//! `captastic-cli`, the command line that ships (ADR 0014): it asks the running Captastic for a
//! capture over the request pipe and prints the answer as one line of JSON.
//!
//! Everything a program reads comes on standard output as one JSON object, success or not, so an
//! agent never parses prose. The exit code says which it was: 0 captured or answered, 1 failed, 2
//! refused, 64 the command line itself was wrong.
//!
//! What it asks for is built here and sent as is; the daemon decides what it can capture. So a
//! target this daemon cannot capture yet comes back refused as `unsupported`, from the daemon,
//! rather than being second-guessed by a command line that may be newer or older than it.

use std::path::{Path, PathBuf};

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use serde_json::{json, Map, Value};

use crate::build_info;
use crate::command_channel::{CaptureCommand, Command, Reason, Request, Target, PROTOCOL_VERSION};
use captastic_core::{OutputFormat, Rect};

/// Exit codes, as ADR 0014 fixes them.
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_REFUSED: i32 = 2;
pub const EXIT_USAGE: i32 = 64;

#[derive(Debug, Parser)]
#[command(
    name = "captastic-cli",
    version = build_info::BUILD_VERSION,
    about = "Ask Captastic for a capture and get back, as JSON, where it went",
    after_help = "Every answer is one line of JSON on standard output.\n\
                  Exit codes: 0 ok, 1 failed, 2 refused, 64 the command line was wrong."
)]
struct CommandLine {
    #[command(subcommand)]
    command: Subcommands,
}

#[derive(Debug, Subcommand)]
enum Subcommands {
    /// Say whether Captastic is running, its version and its process id.
    Status,
    /// List the displays, with the ids `capture --display` takes.
    Displays,
    /// List the windows that can be captured, frontmost first, with the handles `--window` takes.
    Windows,
    /// Take a still capture and print where it went.
    Capture(CaptureArgs),
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .args(["display", "rect", "window", "window_title", "process", "last_region", "interactive"])
))]
struct CaptureArgs {
    /// A whole display: `primary`, `pointer`, `virtual` (every display) or an id from `displays`.
    #[arg(long, value_name = "DISPLAY")]
    display: Option<String>,
    /// A rectangle in desktop pixels. X and Y are negative on a display left of or above the
    /// primary one.
    #[arg(
        long,
        value_name = "X,Y,WIDTH,HEIGHT",
        value_parser = parse_rect,
        allow_hyphen_values = true
    )]
    rect: Option<Rect>,
    /// A window, by its handle.
    #[arg(long, value_name = "HANDLE", value_parser = parse_handle)]
    window: Option<u64>,
    /// The frontmost window whose title contains the text.
    #[arg(long, value_name = "TEXT")]
    window_title: Option<String>,
    /// The frontmost window of the process with this name, such as `notepad.exe`.
    #[arg(long, value_name = "NAME")]
    process: Option<String>,
    /// The region Captastic last captured.
    #[arg(long)]
    last_region: bool,
    /// Open the overlay and wait for the user to choose and confirm.
    #[arg(long)]
    interactive: bool,
    /// Where to write the capture, instead of the output folder under the filename template.
    /// A relative path is taken from the current directory.
    #[arg(long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// The format, instead of the configured one.
    #[arg(long, value_enum)]
    format: Option<FormatArg>,
    /// Also put the capture on the clipboard. Off unless asked: what the user copied stays.
    #[arg(long)]
    clipboard: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FormatArg {
    Png,
    Jpeg,
    Bmp,
}

impl From<FormatArg> for OutputFormat {
    fn from(value: FormatArg) -> Self {
        match value {
            FormatArg::Png => Self::Png,
            FormatArg::Jpeg => Self::Jpeg,
            FormatArg::Bmp => Self::Bmp,
        }
    }
}

fn parse_rect(text: &str) -> Result<Rect, String> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    let [x, y, width, height] = parts.as_slice() else {
        return Err("expected X,Y,WIDTH,HEIGHT, such as 100,100,800,600".to_owned());
    };
    let x = x
        .parse()
        .map_err(|_| format!("x is not a whole number: {x}"))?;
    let y = y
        .parse()
        .map_err(|_| format!("y is not a whole number: {y}"))?;
    let width: u32 = width
        .parse()
        .map_err(|_| format!("width is not a positive whole number: {width}"))?;
    let height: u32 = height
        .parse()
        .map_err(|_| format!("height is not a positive whole number: {height}"))?;
    if width == 0 || height == 0 {
        return Err("the width and height must be more than zero".to_owned());
    }
    Ok(Rect {
        x,
        y,
        width,
        height,
    })
}

/// A window handle, in decimal or as `0x` hexadecimal, the way handles are usually shown.
fn parse_handle(text: &str) -> Result<u64, String> {
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|_| format!("not a window handle: {text}"))
}

/// Why the request never got an answer from a daemon. Only the Windows pipe transport reports
/// most of these; elsewhere there is no daemon to ask.
#[derive(Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum TransportError {
    /// No Captastic is listening in this session.
    NotRunning,
    /// Every instance of the pipe stayed taken.
    Busy,
    /// Something serves the pipe, but not this user's Captastic in this session.
    Untrusted(String),
    Failed(String),
}

/// What the command printed and the code it exits with.
#[derive(Debug, PartialEq)]
pub(crate) struct Outcome {
    pub(crate) line: String,
    pub(crate) code: i32,
}

/// Runs a parsed command, sending its request through `send`, or answering it through `one_shot`
/// when no Captastic is running to ask.
fn run(
    command: Subcommands,
    current_dir: &Path,
    send: impl FnOnce(&[u8]) -> Result<Vec<u8>, TransportError>,
    one_shot: impl FnOnce(Command) -> Outcome,
) -> Outcome {
    let is_status = matches!(command, Subcommands::Status);
    let request = Request {
        version: PROTOCOL_VERSION,
        command: match command {
            Subcommands::Status => Command::Status,
            Subcommands::Displays => Command::Displays,
            Subcommands::Windows => Command::Windows,
            Subcommands::Capture(args) => Command::Capture(capture_command(args, current_dir)),
        },
    };
    let line = match serde_json::to_vec(&request) {
        Ok(line) => line,
        Err(error) => return failure(Reason::BadRequest, format!("not a request: {error}")),
    };
    match send(&line) {
        Ok(answer) => from_answer(&answer),
        Err(TransportError::NotRunning) if is_status => Outcome {
            line: json!({ "version": PROTOCOL_VERSION, "status": "ok", "running": false })
                .to_string(),
            code: EXIT_OK,
        },
        // The maintainer's choice of 2026-10-21: with nothing running, capture anyway, with the
        // same configuration, output folder and announcement as the daemon.
        Err(TransportError::NotRunning) => one_shot(request.command),
        Err(TransportError::Busy) => failure(
            Reason::Busy,
            "Captastic is busy with other requests; ask again",
        ),
        Err(TransportError::Untrusted(message)) => failure(Reason::UntrustedPipe, message),
        Err(TransportError::Failed(message)) => failure(Reason::RequestFailed, message),
    }
}

fn capture_command(args: CaptureArgs, current_dir: &Path) -> CaptureCommand {
    let target = if let Some(display) = args.display {
        Target::Display { display }
    } else if let Some(rect) = args.rect {
        Target::Rect { rect }
    } else if let Some(handle) = args.window {
        Target::Window { handle }
    } else if let Some(title) = args.window_title {
        Target::WindowTitle { title }
    } else if let Some(name) = args.process {
        Target::Process { name }
    } else if args.last_region {
        Target::LastRegion
    } else {
        // The argument group requires exactly one target, so this is `--interactive`.
        Target::Interactive
    };
    CaptureCommand {
        target,
        // The daemon refuses a relative path, since its current directory is not the caller's.
        output: args.output.map(|path| {
            if path.is_absolute() {
                path
            } else {
                current_dir.join(path)
            }
        }),
        format: args.format.map(OutputFormat::from),
        clipboard: args.clipboard,
    }
}

/// The daemon's answer, printed as it came, with the exit code its status means.
fn from_answer(answer: &[u8]) -> Outcome {
    let text = String::from_utf8_lossy(answer).trim().to_owned();
    let status = serde_json::from_str::<Value>(&text).ok().and_then(|value| {
        value
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    match status.as_deref() {
        Some("ok") => Outcome {
            line: text,
            code: EXIT_OK,
        },
        Some("refused") => Outcome {
            line: text,
            code: EXIT_REFUSED,
        },
        Some("failed") => Outcome {
            line: text,
            code: EXIT_FAILED,
        },
        _ => failure(
            Reason::RequestFailed,
            format!("Captastic answered something that is not an answer: {text}"),
        ),
    }
}

fn failure(reason: Reason, message: impl Into<String>) -> Outcome {
    let status = if reason.is_refusal() {
        "refused"
    } else {
        "failed"
    };
    let mut line = Map::new();
    line.insert("version".to_owned(), json!(PROTOCOL_VERSION));
    line.insert("status".to_owned(), json!(status));
    line.insert("reason".to_owned(), json!(reason));
    line.insert("message".to_owned(), json!(message.into()));
    Outcome {
        line: Value::Object(line).to_string(),
        code: if reason.is_refusal() {
            EXIT_REFUSED
        } else {
            EXIT_FAILED
        },
    }
}

/// A command line that could not be parsed: help and version are printed as clap prints them,
/// anything else is a usage answer on standard output and exit code 64.
fn usage(error: &clap::Error) -> Option<Outcome> {
    use clap::error::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        return None;
    }
    let message = error.render().to_string();
    let message = message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("the command line was not understood")
        .trim_start_matches("error: ")
        .to_owned();
    Some(Outcome {
        line: json!({
            "version": PROTOCOL_VERSION,
            "status": "usage",
            "reason": "bad_command_line",
            "message": message,
        })
        .to_string(),
        code: EXIT_USAGE,
    })
}

/// How long a request may take end to end: the daemon waits up to 15 seconds on its capture
/// worker, behind any capture the user started.
#[cfg(windows)]
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// For `--interactive`, as long as the daemon waits on the overlay and the editor, and a margin.
#[cfg(windows)]
const INTERACTIVE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(
    crate::command_listener::INTERACTIVE_CHOICE_TIMEOUT.as_secs()
        + crate::command_listener::INTERACTIVE_EDIT_TIMEOUT.as_secs()
        + 30,
);

#[cfg(windows)]
fn send_to_daemon(line: &[u8]) -> Result<Vec<u8>, TransportError> {
    use crate::command_channel::MAX_ANSWER_BYTES;
    use captastic_windows::{request_pipe_name, send_request, RequestError};
    let name = request_pipe_name().map_err(|error| TransportError::Failed(error.to_string()))?;
    let mut request = line.to_vec();
    request.push(b'\n');
    let interactive = serde_json::from_slice::<Request>(line).is_ok_and(|request| {
        matches!(
            request.command,
            Command::Capture(CaptureCommand {
                target: Target::Interactive,
                ..
            })
        )
    });
    let timeout = if interactive {
        INTERACTIVE_REQUEST_TIMEOUT
    } else {
        REQUEST_TIMEOUT
    };
    send_request(&name, &request, MAX_ANSWER_BYTES, timeout).map_err(|error| match error {
        RequestError::NotRunning => TransportError::NotRunning,
        RequestError::Busy => TransportError::Busy,
        RequestError::Untrusted(error) => {
            TransportError::Untrusted(RequestError::Untrusted(error).to_string())
        }
        RequestError::Failed(error) => {
            TransportError::Failed(RequestError::Failed(error).to_string())
        }
    })
}

#[cfg(not(windows))]
fn send_to_daemon(_line: &[u8]) -> Result<Vec<u8>, TransportError> {
    Err(TransportError::Failed(
        "captastic-cli talks to Captastic on Windows only".to_owned(),
    ))
}

#[cfg(not(windows))]
fn without_daemon(_command: Command) -> Outcome {
    failure(Reason::NotRunning, "captastic-cli captures on Windows only")
}

/// Answers a request in this process, as the daemon would, when no daemon is running: the real
/// backend, the user's configuration, their output folder, and the same announcement.
#[cfg(windows)]
fn without_daemon(command: Command) -> Outcome {
    use crate::command_channel::answer_line;
    use crate::command_listener::{pointer_display, CommandOutput, Commands};

    let config = match crate::one_shot_config(None) {
        Ok(config) => config,
        Err(error) => {
            return failure(
                Reason::EngineUnavailable,
                format!("the configuration could not be read: {error}"),
            )
        }
    };
    let directory = match crate::configured_output_directory(&config) {
        Ok(directory) => directory,
        Err(error) => return failure(Reason::WriteFailed, error.to_string()),
    };
    let backend = match crate::create_dxgi_backend(
        &crate::DisplayPolicy::VirtualDesktop,
        crate::BASE_CPU_BUFFER_SLOTS,
    ) {
        Ok(backend) => backend,
        Err(error) => {
            return failure(
                Reason::EngineUnavailable,
                format!("the screen could not be captured here: {error}"),
            )
        }
    };
    let (notice_sender, notices) = std::sync::mpsc::sync_channel(4);
    let commands = Commands {
        engine: OneShotEngine {
            backend: std::cell::RefCell::new(backend),
            cursor: crate::configured_cursor_mode(&config),
            next_id: std::cell::Cell::new(1),
            clipboard_retention: crate::one_shot_clipboard_retention(&config),
        },
        output: CommandOutput {
            directory,
            filename_template: config.output.filename_template.clone(),
            format: config.output.format,
            encode_options: captastic_core::EncodeOptions {
                jpeg_quality: config.output.jpeg_quality,
                ..captastic_core::EncodeOptions::default()
            },
        },
        notices: Some(notice_sender),
        pointer: pointer_display,
        taken_by: "one_shot",
        // The same history the daemon keeps, locked against it, so a capture taken without
        // Captastic running is still found and still marked as the command line's.
        history: Some(crate::file_output::HistoryRecorder::new(
            captastic_config::HistoryStore::for_default_storage(),
            config.history.retention(),
        )),
    };
    let outcome = from_answer(&answer_line(&commands.answer(command)));
    drop(commands);
    for notice in notices.try_iter() {
        if let crate::daemon::DaemonNotice::CommandLineCapture { path, .. } = notice {
            announce(&path);
        }
    }
    outcome
}

/// The real backend, driven one request at a time on this thread.
#[cfg(windows)]
struct OneShotEngine {
    backend: std::cell::RefCell<Box<dyn captastic_core::CaptureBackend>>,
    cursor: captastic_core::CursorMode,
    next_id: std::cell::Cell<u64>,
    clipboard_retention: captastic_windows::ClipboardRetention,
}

#[cfg(windows)]
impl crate::command_listener::CommandEngine for OneShotEngine {
    fn displays(
        &self,
    ) -> Result<Vec<captastic_core::DisplayInfo>, crate::command_channel::Failure> {
        Ok(self.backend.borrow().displays().to_vec())
    }

    fn frame(
        &self,
        source: captastic_core::CaptureSource,
    ) -> Result<captastic_core::CpuFrame, crate::command_channel::Failure> {
        use crate::command_channel::Failure;
        let id = self.next_id.get();
        self.next_id.set(id.saturating_add(1));
        let request = captastic_core::CaptureRequest {
            id: captastic_core::CaptureId(id),
            triggered_at: std::time::Instant::now(),
            source,
            mode: captastic_core::CaptureMode::Latest { max_age_ms: None },
            cpu_frame: true,
            retain_native_frame: false,
            cursor: self.cursor,
        };
        let mut recorder = captastic_core::EventRecorder::with_capacity(16);
        self.backend
            .borrow_mut()
            .capture(&request, &mut recorder)
            .map_err(|error| Failure::new(Reason::CaptureFailed, error.to_string()))?
            .frame
            .ok_or_else(|| Failure::new(Reason::CaptureFailed, "the backend returned no frame"))
    }

    fn windows(
        &self,
        displays: &[captastic_core::DisplayInfo],
    ) -> Result<Vec<crate::command_channel::WindowInfo>, crate::command_channel::Failure> {
        crate::command_listener::native_windows(displays)
    }

    fn last_regions(
        &self,
    ) -> std::collections::BTreeMap<String, captastic_config::ConfirmedRegion> {
        // What the overlay last saved; without a daemon there is nothing newer in memory.
        captastic_config::UiStateStore::for_default_storage()
            .load()
            .map(|state| state.confirmed_regions())
            .unwrap_or_default()
    }

    fn publish_clipboard(&self, frame: &captastic_core::CpuFrame) -> Result<(), String> {
        crate::command_listener::publish_to_clipboard(frame, self.clipboard_retention)
    }

    fn interactive(
        &self,
    ) -> Result<(captastic_core::CpuFrame, serde_json::Value), crate::command_channel::Failure>
    {
        Err(crate::command_channel::Failure::new(
            Reason::Unsupported,
            "--interactive needs Captastic running to open its overlay; start it and ask again",
        ))
    }

    fn window_frame(
        &self,
        window: &crate::command_channel::WindowInfo,
    ) -> Result<captastic_core::CpuFrame, crate::command_channel::Failure> {
        use crate::command_listener::{capture_native_window, window_display_source};
        let reference = self.frame(window_display_source(window))?;
        capture_native_window(window, &reference)
    }
}

/// Hands the announcement to a short-lived `captastic.exe`, so this command returns while the
/// notification is still up. If that cannot start, it is shown from here instead, waiting: a
/// capture the user did not start is never left unannounced.
#[cfg(windows)]
fn announce(path: &Path) {
    let started = std::env::current_exe()
        .map_err(|error| error.to_string())
        .and_then(|exe| {
            captastic_windows::start_announcer(&exe.with_file_name("captastic.exe"), path)
                .map_err(|error| error.to_string())
        });
    if started.is_err() {
        crate::desktop::announce(path);
    }
}

/// `captastic-cli`'s entry point.
pub fn main() -> ! {
    let outcome = match CommandLine::try_parse() {
        Ok(command_line) => {
            let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            run(
                command_line.command,
                &current_dir,
                send_to_daemon,
                without_daemon,
            )
        }
        Err(error) => match usage(&error) {
            Some(outcome) => {
                // The human-readable explanation, for a person at the terminal.
                let _ = error.print();
                outcome
            }
            None => error.exit(),
        },
    };
    println!("{}", outcome.line);
    std::process::exit(outcome.code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<Subcommands, clap::Error> {
        let mut all = vec!["captastic-cli"];
        all.extend_from_slice(arguments);
        CommandLine::try_parse_from(all).map(|command_line| command_line.command)
    }

    /// Runs `arguments` against a fake daemon that answers `answer`, and returns what was sent
    /// with what was printed.
    fn ask(arguments: &[&str], answer: Result<&str, TransportError>) -> (Option<Value>, Outcome) {
        let command = parse(arguments).expect("parses");
        let mut sent = None;
        let outcome = run(
            command,
            Path::new(r"C:\work"),
            |line| {
                sent = Some(serde_json::from_slice(line).expect("request is JSON"));
                answer.map(|text| text.as_bytes().to_vec())
            },
            |_| panic!("no one-shot capture was expected"),
        );
        (sent, outcome)
    }

    /// Runs `arguments` with no daemon to ask, and returns the command the one-shot path got.
    fn ask_without_daemon(arguments: &[&str]) -> (Option<Command>, Outcome) {
        let command = parse(arguments).expect("parses");
        let mut taken = None;
        let outcome = run(
            command,
            Path::new(r"C:\work"),
            |_| Err(TransportError::NotRunning),
            |command| {
                taken = Some(command);
                from_answer(br#"{"version":1,"status":"ok","taken_by":"one_shot"}"#)
            },
        );
        (taken, outcome)
    }

    #[test]
    fn a_display_capture_is_sent_as_a_display_target() {
        let (sent, outcome) = ask(
            &["capture", "--display", "primary"],
            Ok(r#"{"version":1,"status":"ok","path":"C:/shots/a.png"}"#),
        );
        assert_eq!(
            sent.expect("sent"),
            json!({
                "version": 1, "command": "capture",
                "target": { "kind": "display", "display": "primary" },
                "clipboard": false,
            })
        );
        assert_eq!(outcome.code, EXIT_OK);
        assert_eq!(
            outcome.line,
            r#"{"version":1,"status":"ok","path":"C:/shots/a.png"}"#
        );
    }

    #[test]
    fn a_rectangle_with_a_path_and_format_is_sent_whole() {
        let (sent, _) = ask(
            &[
                "capture",
                "--rect",
                "-10, 20,300,200",
                "--output",
                "shots\\a.jpg",
                "--format",
                "jpeg",
                "--clipboard",
            ],
            Ok(r#"{"version":1,"status":"ok"}"#),
        );
        let sent = sent.expect("sent");
        assert_eq!(
            sent["target"],
            json!({ "kind": "rect", "rect": { "x": -10, "y": 20, "width": 300, "height": 200 } })
        );
        assert_eq!(
            PathBuf::from(sent["output"].as_str().expect("output")),
            Path::new(r"C:\work").join(r"shots\a.jpg")
        );
        assert_eq!(sent["format"], "jpeg");
        assert_eq!(sent["clipboard"], true);
    }

    #[test]
    fn window_targets_are_sent_for_the_daemon_to_judge() {
        let (sent, _) = ask(&["capture", "--window", "0x1A2B"], Ok(r#"{"status":"ok"}"#));
        assert_eq!(
            sent.expect("sent")["target"],
            json!({ "kind": "window", "handle": 0x1A2B })
        );
        let (sent, _) = ask(
            &["capture", "--process", "notepad.exe"],
            Ok(r#"{"status":"ok"}"#),
        );
        assert_eq!(
            sent.expect("sent")["target"],
            json!({ "kind": "process", "name": "notepad.exe" })
        );
        let (sent, _) = ask(&["capture", "--last-region"], Ok(r#"{"status":"ok"}"#));
        assert_eq!(
            sent.expect("sent")["target"],
            json!({ "kind": "last_region" })
        );
    }

    #[test]
    fn the_answer_status_is_the_exit_code() {
        let refused = ask(
            &["capture", "--display", "primary"],
            Ok(r#"{"version":1,"status":"refused","reason":"recording_running","message":"m"}"#),
        )
        .1;
        assert_eq!(refused.code, EXIT_REFUSED);
        let failed = ask(
            &["capture", "--display", "primary"],
            Ok(r#"{"version":1,"status":"failed","reason":"write_failed","message":"m"}"#),
        )
        .1;
        assert_eq!(failed.code, EXIT_FAILED);
        let garbled = ask(&["displays"], Ok("not json")).1;
        assert_eq!(garbled.code, EXIT_FAILED);
        assert!(
            garbled.line.contains(r#""reason":"request_failed""#),
            "{}",
            garbled.line
        );
    }

    #[test]
    fn status_without_a_daemon_says_it_is_not_running() {
        let outcome = ask(&["status"], Err(TransportError::NotRunning)).1;
        assert_eq!(outcome.code, EXIT_OK);
        assert_eq!(
            serde_json::from_str::<Value>(&outcome.line).expect("json"),
            json!({ "version": 1, "status": "ok", "running": false })
        );
    }

    #[test]
    fn without_a_daemon_a_capture_is_taken_here() {
        let (taken, outcome) = ask_without_daemon(&["capture", "--rect", "0,0,10,10"]);
        assert_eq!(outcome.code, EXIT_OK);
        assert!(
            outcome.line.contains(r#""taken_by":"one_shot""#),
            "{}",
            outcome.line
        );
        let Some(Command::Capture(capture)) = taken else {
            panic!("the capture went to the one-shot path: {taken:?}");
        };
        assert_eq!(
            capture.target,
            Target::Rect {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 10,
                    height: 10
                }
            }
        );
        let (taken, _) = ask_without_daemon(&["displays"]);
        assert_eq!(taken, Some(Command::Displays));
        // Status is the one command the one-shot path never answers: nothing is running.
        let (taken, outcome) = ask_without_daemon(&["status"]);
        assert_eq!(taken, None);
        assert!(
            outcome.line.contains(r#""running":false"#),
            "{}",
            outcome.line
        );
    }

    #[test]
    fn a_busy_or_untrusted_pipe_is_a_failure_not_a_one_shot() {
        let busy = ask(&["displays"], Err(TransportError::Busy)).1;
        assert_eq!(busy.code, EXIT_FAILED);
        assert!(busy.line.contains(r#""reason":"busy""#), "{}", busy.line);
        let untrusted = ask(
            &["status"],
            Err(TransportError::Untrusted("someone else's pipe".to_owned())),
        )
        .1;
        assert_eq!(untrusted.code, EXIT_FAILED);
        assert!(
            untrusted.line.contains(r#""reason":"untrusted_pipe""#),
            "{}",
            untrusted.line
        );
    }

    #[test]
    fn a_wrong_command_line_is_a_usage_answer() {
        for arguments in [
            &["capture"][..],
            &["capture", "--display", "primary", "--rect", "0,0,10,10"],
            &["capture", "--rect", "0,0,0,10"],
            &["capture", "--rect", "1,2,3"],
            &["capture", "--window", "notahandle"],
            &["capture", "--display", "primary", "--format", "gif"],
            &["snap"],
        ] {
            let error = parse(arguments).expect_err("refused");
            let outcome = usage(&error).expect("a usage error");
            assert_eq!(outcome.code, EXIT_USAGE, "{arguments:?}");
            let line: Value = serde_json::from_str(&outcome.line).expect("json");
            assert_eq!(line["status"], "usage");
            assert_eq!(line["reason"], "bad_command_line");
            assert!(!line["message"].as_str().expect("message").is_empty());
        }
    }

    #[test]
    fn help_and_version_are_not_usage_errors() {
        for arguments in [&["--help"][..], &["--version"], &["capture", "--help"]] {
            let error = parse(arguments).expect_err("help exits");
            assert!(usage(&error).is_none(), "{arguments:?}");
        }
    }
}
