//! Whether this session still owns a desktop worth capturing.
//!
//! Nearly every unexplained DXGI failure on this project turned out to be the same thing: the
//! workstation was locked, or an RDP client had disconnected, so the desktop belonged to Winlogon
//! rather than to us. Enumeration then returns no attached outputs and duplication is denied —
//! which is indistinguishable, from inside DXGI, from every monitor being unplugged.
//!
//! The difference is worth a syscall. One of those conditions is permanent until someone plugs a
//! cable in; the other fixes itself the moment the user signs back in, and a tool meant to be
//! running when they return should wait for that rather than exiting with a message about missing
//! monitors.
//!
//! Two questions, because no single call answers both. A locked session is still `WTSActive`, so
//! the connect state cannot see a lock; and a disconnected session may still have an openable
//! desktop, so the desktop probe cannot see a disconnect.

use std::fmt;

use windows::Win32::Foundation::{BOOL, HANDLE};
use windows::Win32::System::RemoteDesktop::{
    WTSActive, WTSConnectQuery, WTSConnectState, WTSConnected, WTSDisconnected, WTSDown,
    WTSFreeMemory, WTSIdle, WTSInit, WTSListen, WTSQuerySessionInformationW, WTSReset, WTSShadow,
    WTS_CONNECTSTATE_CLASS,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_CONTROL_FLAGS,
    DESKTOP_READOBJECTS, UOI_NAME,
};

/// The session's own session id, rather than one looked up by number.
const WTS_CURRENT_SESSION: u32 = u32::MAX;
/// The local server, rather than a terminal server reached over the network.
const WTS_CURRENT_SERVER: HANDLE = HANDLE(0);
/// The desktop an ordinary interactive session owns. Anything else owning input — `Winlogon` for
/// the lock screen and UAC, `Screen-saver` for a running screensaver — means the pixels on screen
/// are not this session's to read.
const INTERACTIVE_DESKTOP: &str = "Default";

/// Why the desktop is not available for capture, when the session rather than the hardware is why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DesktopState {
    /// The session owns an ordinary interactive desktop, so displays should enumerate.
    Interactive,
    /// Input belongs to a desktop this process cannot read: the lock screen, the credential
    /// prompt, or a UAC elevation prompt. Carries the desktop's name where Windows named it.
    NotOurs { desktop: Option<String> },
    /// The session is not attached to a console or a client — a disconnected RDP session, or one
    /// still being set up.
    Detached { connect_state: &'static str },
    /// The probe could not answer, which is not the same as answering "fine".
    Unknown { detail: String },
}

impl DesktopState {
    /// Whether captures should be expected to work.
    pub fn is_interactive(&self) -> bool {
        matches!(self, Self::Interactive)
    }

    /// Whether this is a condition that clears itself when the user comes back.
    ///
    /// `Unknown` is deliberately not temporary. Waiting forever on a question that was never
    /// answered would turn a probe failure into a daemon that silently never captures.
    pub fn is_temporary(&self) -> bool {
        matches!(self, Self::NotOurs { .. } | Self::Detached { .. })
    }
}

impl fmt::Display for DesktopState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interactive => formatter.write_str("the session owns an interactive desktop"),
            Self::NotOurs {
                desktop: Some(name),
            } => write!(
                formatter,
                "the session is locked or a secure prompt is up; the {name} desktop owns input"
            ),
            Self::NotOurs { desktop: None } => {
                formatter.write_str("the session is locked or a secure prompt owns the desktop")
            }
            Self::Detached { connect_state } => {
                write!(formatter, "the session is {connect_state}")
            }
            Self::Unknown { detail } => {
                write!(
                    formatter,
                    "the session state could not be determined: {detail}"
                )
            }
        }
    }
}

/// What the connect-state query saw.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectState {
    Active,
    Other(&'static str),
    Unavailable,
}

/// What the input-desktop probe saw.
#[derive(Clone, Debug, Eq, PartialEq)]
enum InputDesktop {
    Named(String),
    /// Opening the input desktop was refused, which is what a locked session looks like.
    Refused,
    /// Something other than a refusal went wrong.
    Failed(String),
}

/// Asks Windows whether this session currently owns a desktop.
///
/// Cheap enough to call on a failure path — two syscalls, no allocation beyond a desktop name —
/// and deliberately not cached: the whole point is that the answer changes while the process runs.
pub fn desktop_state() -> DesktopState {
    classify(connect_state(), input_desktop())
}

/// Turns what the two probes saw into one answer.
///
/// Separated from the syscalls so the truth table can be tested. It cannot be tested any other
/// way: producing a locked session on a build agent means locking the build agent.
fn classify(connect: ConnectState, desktop: InputDesktop) -> DesktopState {
    // A detached session is reported first. It is the more fundamental condition — a session with
    // no client attached has no desktop for anyone to own — and its desktop probe result is not
    // worth explaining on top of it.
    if let ConnectState::Other(state) = connect {
        return DesktopState::Detached {
            connect_state: state,
        };
    }
    match desktop {
        InputDesktop::Named(name) if name.eq_ignore_ascii_case(INTERACTIVE_DESKTOP) => {
            DesktopState::Interactive
        }
        InputDesktop::Named(name) => DesktopState::NotOurs {
            desktop: Some(name),
        },
        InputDesktop::Refused => DesktopState::NotOurs { desktop: None },
        InputDesktop::Failed(detail) => DesktopState::Unknown { detail },
    }
}

/// Reads this session's connect state, which is how a disconnected RDP session is visible.
///
/// A locked session stays `WTSActive`, so this answers only half the question by design.
fn connect_state() -> ConnectState {
    let mut buffer = windows::core::PWSTR::null();
    let mut bytes = 0_u32;
    // SAFETY: The out-parameters are valid for the call. On success Windows allocates a buffer of
    // `bytes` bytes at `buffer`, owned by us until the WTSFreeMemory below.
    let queried = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER,
            WTS_CURRENT_SESSION,
            WTSConnectState,
            &mut buffer,
            &mut bytes,
        )
    };
    if queried.is_err() || buffer.is_null() {
        return ConnectState::Unavailable;
    }
    let state = if bytes as usize >= std::mem::size_of::<i32>() {
        // SAFETY: WTSConnectState returns a WTS_CONNECTSTATE_CLASS, which is an i32, and the size
        // check above confirms Windows returned at least that many bytes.
        Some(WTS_CONNECTSTATE_CLASS(unsafe {
            buffer.as_ptr().cast::<i32>().read_unaligned()
        }))
    } else {
        None
    };
    // SAFETY: buffer was allocated by the successful query above and is freed exactly once.
    unsafe { WTSFreeMemory(buffer.as_ptr().cast()) };
    match state {
        Some(state) if state == WTSActive => ConnectState::Active,
        Some(state) => ConnectState::Other(connect_state_name(state)),
        None => ConnectState::Unavailable,
    }
}

/// Names a connect state, so a message can say which one rather than a number.
fn connect_state_name(state: WTS_CONNECTSTATE_CLASS) -> &'static str {
    match state {
        state if state == WTSConnected => "connected without an active desktop",
        state if state == WTSConnectQuery => "in connect query",
        state if state == WTSShadow => "shadowing another session",
        state if state == WTSDisconnected => "disconnected",
        state if state == WTSIdle => "idle",
        state if state == WTSListen => "a listener rather than an interactive session",
        state if state == WTSReset => "resetting",
        state if state == WTSDown => "down",
        state if state == WTSInit => "initializing",
        _ => "in an unrecognized connect state",
    }
}

/// Opens the desktop that currently owns input, to see whose it is.
fn input_desktop() -> InputDesktop {
    // SAFETY: No flags, no inheritance, and read access only — this asks a question and takes no
    // hold on the desktop beyond the handle closed below.
    let desktop =
        match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), BOOL(0), DESKTOP_READOBJECTS) } {
            Ok(desktop) => desktop,
            // A refusal is the answer, not an error: this is exactly what a locked workstation does to
            // a process that is not running as the local system.
            Err(_) => return InputDesktop::Refused,
        };
    let mut name = [0_u16; 128];
    let mut needed = 0_u32;
    // SAFETY: HDESK is a user-object handle, valid until CloseDesktop below. The buffer length is
    // given in bytes, as the API expects, and `needed` is a valid out-parameter.
    let queried = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            u32::try_from(std::mem::size_of_val(&name)).unwrap_or(u32::MAX),
            Some(&mut needed),
        )
    };
    // SAFETY: desktop came from the successful OpenInputDesktop above and is closed exactly once.
    let _ = unsafe { CloseDesktop(desktop) };
    match queried {
        Ok(()) => InputDesktop::Named(wide_to_string(&name)),
        // The desktop opened, so it is ours; only its name is missing. Reported as a failure
        // rather than assumed interactive, because guessing is what this module exists to stop.
        Err(error) => {
            InputDesktop::Failed(format!("the input desktop could not be named: {error}"))
        }
    }
}

fn wide_to_string(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_session_is_interactive() {
        assert_eq!(
            classify(
                ConnectState::Active,
                InputDesktop::Named("Default".to_owned())
            ),
            DesktopState::Interactive
        );
        // Windows is not consistent about the case of user-object names.
        assert!(classify(
            ConnectState::Active,
            InputDesktop::Named("default".to_owned())
        )
        .is_interactive());
    }

    #[test]
    fn a_refused_input_desktop_reads_as_a_lock_rather_than_a_failure() {
        // The signature of a locked workstation: OpenInputDesktop is denied outright. Treating it
        // as an error would put the daemon back where issue #51 found it, reporting something that
        // sounds like broken hardware.
        let state = classify(ConnectState::Active, InputDesktop::Refused);
        assert_eq!(state, DesktopState::NotOurs { desktop: None });
        assert!(state.is_temporary());
        assert!(!state.is_interactive());
    }

    #[test]
    fn the_secure_desktop_is_named_when_windows_names_it() {
        let state = classify(
            ConnectState::Active,
            InputDesktop::Named("Winlogon".to_owned()),
        );
        assert!(state.is_temporary());
        assert!(
            state.to_string().contains("Winlogon"),
            "the message should say whose desktop it is: {state}"
        );
    }

    #[test]
    fn a_detached_session_outranks_whatever_its_desktop_probe_said() {
        // A session with no client attached has no desktop for anyone to own, so reporting the
        // desktop result on top of it would explain the symptom rather than the cause.
        for desktop in [
            InputDesktop::Refused,
            InputDesktop::Named("Default".to_owned()),
            InputDesktop::Failed("anything".to_owned()),
        ] {
            let state = classify(ConnectState::Other("disconnected"), desktop);
            assert_eq!(
                state,
                DesktopState::Detached {
                    connect_state: "disconnected"
                }
            );
            assert!(state.is_temporary());
        }
    }

    #[test]
    fn an_unanswered_probe_is_not_treated_as_temporary() {
        // Waiting forever on a question nobody answered would turn one failed syscall into a
        // daemon that never captures and never says why.
        let state = classify(
            ConnectState::Active,
            InputDesktop::Failed("no name".to_owned()),
        );
        assert!(!state.is_temporary());
        assert!(!state.is_interactive());
        assert!(
            state.to_string().contains("could not be determined"),
            "{state}"
        );
    }

    #[test]
    fn an_unavailable_connect_state_still_lets_the_desktop_answer() {
        // WTS is not always answerable; that is not a reason to give up on the question, because
        // the desktop probe is the half that detects the common case.
        assert_eq!(
            classify(
                ConnectState::Unavailable,
                InputDesktop::Named("Default".to_owned())
            ),
            DesktopState::Interactive
        );
        assert!(classify(ConnectState::Unavailable, InputDesktop::Refused).is_temporary());
    }

    /// Runs the real probe. Asserts only what is true of any machine able to run it.
    #[test]
    fn the_live_probe_answers_without_panicking() {
        let state = desktop_state();
        // A CI agent may have no interactive desktop at all, so the state is not asserted — but
        // whatever it is, it must be describable, and it must not be two things at once.
        assert!(!state.to_string().is_empty());
        assert!(!(state.is_interactive() && state.is_temporary()));
    }
}
