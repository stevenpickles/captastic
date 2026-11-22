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

/// Runs a parsed command, sending its request through `send`.
fn run(
    command: Subcommands,
    current_dir: &Path,
    send: impl FnOnce(&[u8]) -> Result<Vec<u8>, TransportError>,
) -> Outcome {
    let is_status = matches!(command, Subcommands::Status);
    let request = Request {
        version: PROTOCOL_VERSION,
        command: match command {
            Subcommands::Status => Command::Status,
            Subcommands::Displays => Command::Displays,
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
        Err(TransportError::NotRunning) => failure(
            Reason::NotRunning,
            "Captastic is not running in this session; start it and ask again",
        ),
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

#[cfg(windows)]
fn send_to_daemon(line: &[u8]) -> Result<Vec<u8>, TransportError> {
    use crate::command_channel::MAX_ANSWER_BYTES;
    use captastic_windows::{request_pipe_name, send_request, RequestError};
    let name = request_pipe_name().map_err(|error| TransportError::Failed(error.to_string()))?;
    let mut request = line.to_vec();
    request.push(b'\n');
    send_request(&name, &request, MAX_ANSWER_BYTES, REQUEST_TIMEOUT).map_err(|error| match error {
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

/// `captastic-cli`'s entry point.
pub fn main() -> ! {
    let outcome = match CommandLine::try_parse() {
        Ok(command_line) => {
            let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            run(command_line.command, &current_dir, send_to_daemon)
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
        let outcome = run(command, Path::new(r"C:\work"), |line| {
            sent = Some(serde_json::from_slice(line).expect("request is JSON"));
            answer.map(|text| text.as_bytes().to_vec())
        });
        (sent, outcome)
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
    fn a_capture_without_a_daemon_fails_as_not_running() {
        let outcome = ask(
            &["capture", "--display", "primary"],
            Err(TransportError::NotRunning),
        )
        .1;
        assert_eq!(outcome.code, EXIT_FAILED);
        let line: Value = serde_json::from_str(&outcome.line).expect("json");
        assert_eq!(line["status"], "failed");
        assert_eq!(line["reason"], "not_running");
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
