//! What a command-line program asks the running daemon for, and what it is told (ADR 0014).
//!
//! One request and one answer per connection, each a single line of JSON carrying a version. The
//! answer's `status` is `ok`, `refused` or `failed`, which is also the command line's exit code: 0,
//! 2 or 1. A refusal or a failure carries a stable `reason` code, so a program never has to read
//! the prose beside it.
//!
//! The rest is the part of a capture that needs no engine: which source a target names, and which
//! part of that source's frame to keep.

// Only Windows has a daemon to listen yet; elsewhere this is exercised by its tests alone.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::PathBuf;

use captastic_core::{CaptureSource, DisplayId, DisplayInfo, OutputFormat, Rect};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// The version of both the request and the answer. A request of another version is refused, so a
/// newer command line meeting an older daemon says so rather than being half understood.
pub const PROTOCOL_VERSION: u32 = 1;
/// A request is a target and a few options; anything longer is not one.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// An answer lists at most the displays and windows on the desktop.
#[allow(dead_code)] // The command line's limit on what it reads back.
pub const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Whether the daemon is running, its version and its process id.
    Status,
    /// The displays, with the ids a display target takes.
    Displays,
    Capture(CaptureCommand),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureCommand {
    pub target: Target,
    /// Where to write the capture, instead of the output directory under the filename template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<PathBuf>,
    /// The format, instead of the configured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    /// Whether the capture also goes to the clipboard. Off unless asked: a capture the user did
    /// not start must not replace what they copied.
    #[serde(default)]
    pub clipboard: bool,
}

/// What to capture: exactly one of these per request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// `primary`, `pointer`, `virtual` (the whole virtual desktop), or a display's id.
    Display { display: String },
    /// A rectangle in desktop pixels.
    Rect { rect: Rect },
    /// A window, by the handle `windows` lists.
    Window { handle: u64 },
    /// The frontmost window whose title contains the text.
    WindowTitle { title: String },
    /// The frontmost window of the process with this name.
    Process { name: String },
    /// The region Captastic last captured.
    LastRegion,
    /// The overlay, for the user to choose and confirm.
    Interactive,
}

/// Why a request got no capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// A recording is running, and still captures wait for it (ADR 0011).
    RecordingRunning,
    /// No display has the id the target names.
    UnknownDisplay,
    /// The rectangle is empty or reaches outside every display.
    OutsideDesktop,
    /// This daemon cannot capture that target yet.
    Unsupported,
    /// The request was not one: not JSON, too long, or missing what it needs.
    BadRequest,
    /// The request is of a protocol version this daemon does not speak.
    UnsupportedVersion,
    /// The capture engine is recovering, or has no display to capture.
    EngineUnavailable,
    /// The capture was attempted and could not be taken.
    CaptureFailed,
    /// The capture was taken and could not be written.
    WriteFailed,
    /// The daemon is stopping.
    ShuttingDown,
}

impl Reason {
    /// Whether the request was declined, rather than tried and failed: exit code 2 rather than 1.
    pub fn is_refusal(self) -> bool {
        matches!(
            self,
            Self::RecordingRunning
                | Self::UnknownDisplay
                | Self::OutsideDesktop
                | Self::Unsupported
                | Self::BadRequest
                | Self::UnsupportedVersion
                | Self::ShuttingDown
        )
    }
}

/// A request that got no result, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub reason: Reason,
    pub message: String,
}

impl Failure {
    pub fn new(reason: Reason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

/// An answer: the result's fields, or why there is none.
pub type Answer = Result<Map<String, Value>, Failure>;

/// The answer as the line sent back: `{"version":1,"status":"ok",...}`, or a `status` of `refused`
/// or `failed` with its `reason` and `message`.
pub fn answer_line(answer: &Answer) -> Vec<u8> {
    let mut line = Map::new();
    line.insert("version".to_owned(), json!(PROTOCOL_VERSION));
    match answer {
        Ok(fields) => {
            line.insert("status".to_owned(), json!("ok"));
            line.extend(fields.clone());
        }
        Err(failure) => {
            let status = if failure.reason.is_refusal() {
                "refused"
            } else {
                "failed"
            };
            line.insert("status".to_owned(), json!(status));
            line.insert("reason".to_owned(), json!(failure.reason));
            line.insert("message".to_owned(), json!(failure.message));
        }
    }
    serde_json::to_vec(&Value::Object(line)).unwrap_or_else(|_| b"{}".to_vec())
}

/// Reads a request line, refusing one that is not a request of this version.
pub fn parse_request(line: &[u8]) -> Result<Request, Failure> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(Failure::new(
            Reason::BadRequest,
            format!("a request is at most {MAX_REQUEST_BYTES} bytes"),
        ));
    }
    let value: Value = serde_json::from_slice(line)
        .map_err(|error| Failure::new(Reason::BadRequest, format!("not JSON: {error}")))?;
    match value.get("version").and_then(Value::as_u64) {
        Some(version) if version == u64::from(PROTOCOL_VERSION) => {}
        Some(version) => {
            return Err(Failure::new(
                Reason::UnsupportedVersion,
                format!("this Captastic speaks version {PROTOCOL_VERSION}, not {version}"),
            ))
        }
        None => {
            return Err(Failure::new(
                Reason::BadRequest,
                "the request has no version",
            ))
        }
    }
    serde_json::from_value(value)
        .map_err(|error| Failure::new(Reason::BadRequest, format!("not a request: {error}")))
}

/// What to capture for a display or rectangle target: the source to ask the engine for, and the
/// part of its frame to keep, in desktop pixels (`None` keeps it all).
#[derive(Clone, Debug, PartialEq)]
pub struct CapturePlan {
    pub source: CaptureSource,
    pub crop: Option<Rect>,
    /// What was captured, for the answer.
    pub described: Value,
}

/// Plans a display or rectangle target against the engine's displays. `pointer` is the display the
/// pointer is on, asked only when the target names it.
pub fn plan_capture(
    target: &Target,
    displays: &[DisplayInfo],
    pointer: impl FnOnce() -> Result<DisplayId, Failure>,
) -> Result<CapturePlan, Failure> {
    match target {
        Target::Display { display } => {
            let found = match display.trim() {
                "virtual" | "virtual_desktop" => {
                    return Ok(CapturePlan {
                        source: CaptureSource::VirtualDesktop,
                        crop: None,
                        described: json!({ "kind": "display", "display": "virtual" }),
                    })
                }
                "primary" => displays
                    .iter()
                    .find(|info| info.is_primary)
                    .or_else(|| displays.first()),
                "pointer" => {
                    let id = pointer()?;
                    displays.iter().find(|info| info.id == id)
                }
                id => displays.iter().find(|info| info.id.0 == id),
            };
            let info = found.ok_or_else(|| {
                Failure::new(
                    Reason::UnknownDisplay,
                    format!("no display is {display:?}; `displays` lists them"),
                )
            })?;
            Ok(CapturePlan {
                source: CaptureSource::Display(info.id.clone()),
                crop: None,
                described: json!({
                    "kind": "display",
                    "display": info.id.0,
                    "rect": info.bounds,
                }),
            })
        }
        Target::Rect { rect } => {
            if rect.width == 0 || rect.height == 0 {
                return Err(Failure::new(
                    Reason::OutsideDesktop,
                    "the rectangle is empty",
                ));
            }
            let described = json!({ "kind": "rect", "rect": rect });
            // One display holds it whole: that display, cropped.
            if let Some(info) = displays
                .iter()
                .find(|info| info.bounds.intersection(*rect) == Some(*rect))
            {
                return Ok(CapturePlan {
                    source: CaptureSource::Display(info.id.clone()),
                    crop: Some(*rect),
                    described,
                });
            }
            // Across displays: the virtual desktop, cropped, as long as every pixel is on one.
            let covered: u64 = displays
                .iter()
                .filter_map(|info| info.bounds.intersection(*rect))
                .map(|part| u64::from(part.width) * u64::from(part.height))
                .sum();
            if covered == u64::from(rect.width) * u64::from(rect.height) {
                return Ok(CapturePlan {
                    source: CaptureSource::VirtualDesktop,
                    crop: Some(*rect),
                    described,
                });
            }
            Err(Failure::new(
                Reason::OutsideDesktop,
                "the rectangle reaches outside every display",
            ))
        }
        Target::Window { .. }
        | Target::WindowTitle { .. }
        | Target::Process { .. }
        | Target::LastRegion
        | Target::Interactive => Err(Failure::new(
            Reason::Unsupported,
            "this Captastic captures displays and rectangles from the command line; windows, the \
             last region and the overlay come later",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: &str, x: i32, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id.to_owned()),
            name: id.to_owned(),
            bounds: Rect {
                x,
                y: 0,
                width: 1920,
                height: 1080,
            },
            scale_factor: 1.0,
            rotation_degrees: 0,
            is_primary: primary,
        }
    }

    fn two_displays() -> Vec<DisplayInfo> {
        vec![display("left", 0, true), display("right", 1920, false)]
    }

    fn no_pointer() -> Result<DisplayId, Failure> {
        panic!("the pointer was asked for")
    }

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_capture_request_reads_as_its_command_line_writes_it() {
        let request = Request {
            version: PROTOCOL_VERSION,
            command: Command::Capture(CaptureCommand {
                target: Target::Rect {
                    rect: rect(10, 20, 300, 200),
                },
                output: Some(PathBuf::from("C:/shots/a.png")),
                format: Some(OutputFormat::Png),
                clipboard: false,
            }),
        };
        let line = serde_json::to_vec(&request).expect("serialize");
        let text = String::from_utf8(line.clone()).expect("utf-8");
        assert!(text.contains(r#""command":"capture""#), "{text}");
        assert!(text.contains(r#""kind":"rect""#), "{text}");
        assert_eq!(parse_request(&line).expect("parse"), request);
        assert_eq!(
            parse_request(br#"{"version":1,"command":"status"}"#).expect("status"),
            Request {
                version: 1,
                command: Command::Status
            }
        );
    }

    #[test]
    fn what_is_not_a_request_of_this_version_is_refused_with_a_reason() {
        let reason = |line: &[u8]| parse_request(line).expect_err("refused").reason;
        assert_eq!(reason(b"capture the screen"), Reason::BadRequest);
        assert_eq!(reason(br#"{"command":"status"}"#), Reason::BadRequest);
        assert_eq!(
            reason(br#"{"version":2,"command":"status"}"#),
            Reason::UnsupportedVersion
        );
        assert_eq!(
            reason(br#"{"version":1,"command":"format_the_disk"}"#),
            Reason::BadRequest
        );
        assert_eq!(
            reason(br#"{"version":1,"command":"capture"}"#),
            Reason::BadRequest
        );
        assert_eq!(
            reason(&vec![b' '; MAX_REQUEST_BYTES + 1]),
            Reason::BadRequest
        );
    }

    #[test]
    fn an_answer_says_ok_refused_or_failed_with_a_stable_reason() {
        let line = |answer: Answer| -> Value {
            serde_json::from_slice(&answer_line(&answer)).expect("json")
        };
        let mut fields = Map::new();
        fields.insert("path".to_owned(), json!("C:/shots/a.png"));
        let ok = line(Ok(fields));
        assert_eq!(ok["status"], "ok");
        assert_eq!(ok["version"], PROTOCOL_VERSION);
        assert_eq!(ok["path"], "C:/shots/a.png");
        let refused = line(Err(Failure::new(
            Reason::RecordingRunning,
            "a recording runs",
        )));
        assert_eq!(refused["status"], "refused");
        assert_eq!(refused["reason"], "recording_running");
        let failed = line(Err(Failure::new(Reason::CaptureFailed, "no frame")));
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["reason"], "capture_failed");
    }

    #[test]
    fn a_display_target_names_a_display_by_alias_or_id() {
        let displays = two_displays();
        let plan = |name: &str| {
            plan_capture(
                &Target::Display {
                    display: name.to_owned(),
                },
                &displays,
                || Ok(DisplayId("right".to_owned())),
            )
        };
        assert_eq!(
            plan("primary").expect("primary").source,
            CaptureSource::Display(DisplayId("left".to_owned()))
        );
        assert_eq!(
            plan("pointer").expect("pointer").source,
            CaptureSource::Display(DisplayId("right".to_owned()))
        );
        assert_eq!(
            plan("virtual").expect("virtual").source,
            CaptureSource::VirtualDesktop
        );
        let by_id = plan("right").expect("by id");
        assert_eq!(by_id.crop, None);
        assert_eq!(by_id.described["rect"]["x"], 1920);
        assert_eq!(
            plan("middle").expect_err("unknown").reason,
            Reason::UnknownDisplay
        );
    }

    #[test]
    fn a_rectangle_is_cropped_from_its_display_or_from_the_desktop_it_spans() {
        let displays = two_displays();
        let plan = |area: Rect| plan_capture(&Target::Rect { rect: area }, &displays, no_pointer);
        let inside = plan(rect(2000, 100, 400, 300)).expect("inside one display");
        assert_eq!(
            inside.source,
            CaptureSource::Display(DisplayId("right".to_owned()))
        );
        assert_eq!(inside.crop, Some(rect(2000, 100, 400, 300)));
        let across = plan(rect(1800, 100, 400, 300)).expect("across two displays");
        assert_eq!(across.source, CaptureSource::VirtualDesktop);
        assert_eq!(across.crop, Some(rect(1800, 100, 400, 300)));
        assert_eq!(
            plan(rect(3700, 100, 400, 300))
                .expect_err("off the edge")
                .reason,
            Reason::OutsideDesktop
        );
        assert_eq!(
            plan(rect(10, 10, 0, 30)).expect_err("empty").reason,
            Reason::OutsideDesktop
        );
    }

    #[test]
    fn targets_not_built_yet_are_refused_as_unsupported() {
        let failure =
            plan_capture(&Target::LastRegion, &two_displays(), no_pointer).expect_err("not yet");
        assert_eq!(failure.reason, Reason::Unsupported);
        assert!(failure.reason.is_refusal());
    }
}
