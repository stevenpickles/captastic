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
//! Three questions, because no single call answers all of them. A locked session is still
//! `WTSActive`, so the connect state cannot see a lock; a disconnected session may still have an
//! openable desktop, so the desktop probe cannot see a disconnect; and a locked session does not
//! always hand input to `Winlogon`, so the desktop probe cannot reliably see a lock either.
//!
//! That last one is measured rather than assumed. Windows 11's lock screen is an ordinary
//! application on the *`Default`* desktop, and input only moves to `Winlogon` while the credential
//! prompt is actually up - so for most of a lock, `OpenInputDesktop` succeeds and answers
//! "`Default`", exactly as it does for an unlocked session. Duplication is denied anyway once the
//! displays power down, which is the state a machine left alone settles into: 125 seconds of it in
//! one probe run, every sample reporting `Access is denied` while the desktop probe said the
//! session was fine. `WTSSessionInfoEx` is the signal that answers in every phase.

use std::fmt;

use windows::Win32::Foundation::{BOOL, HANDLE};
use windows::Win32::System::RemoteDesktop::{
    WTSActive, WTSClientProtocolType, WTSConnectQuery, WTSConnectState, WTSConnected,
    WTSDisconnected, WTSDown, WTSFreeMemory, WTSIdle, WTSInit, WTSListen,
    WTSQuerySessionInformationW, WTSReset, WTSSessionInfoEx, WTSShadow, WTSINFOEXW,
    WTS_CONNECTSTATE_CLASS, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK,
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
/// the credential prompt and UAC, `Screen-saver` for a running screensaver — means the pixels on
/// screen are not this session's to read.
///
/// Note what this does *not* cover: a locked session usually reports `Default` here, because the
/// lock screen is an ordinary application. The lock is asked about separately.
const INTERACTIVE_DESKTOP: &str = "Default";

/// Why the desktop is not available for capture, when the session rather than the hardware is why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DesktopState {
    /// The session owns an ordinary interactive desktop, so displays should enumerate.
    Interactive,
    /// The workstation is locked.
    ///
    /// Reported from `WTSSessionInfoEx` rather than inferred from who owns input, because for most
    /// of a lock the answer to that is `Default` and looks exactly like an unlocked session. Once
    /// the displays power down — where a machine left alone ends up — duplication is refused with
    /// a bare `Access is denied`, and without this the failure has no explanation attached to it.
    ///
    /// Carries the desktop's name when a secure one owns input as well, which is the credential
    /// prompt rather than the lock screen behind it.
    Locked { desktop: Option<String> },
    /// Input belongs to a desktop this process cannot read: the credential prompt, a UAC elevation
    /// prompt, or a screensaver. Carries the desktop's name where Windows named it.
    NotOurs { desktop: Option<String> },
    /// The session is not attached to a console or a client — a disconnected RDP session, or one
    /// still being set up.
    Detached { connect_state: &'static str },
    /// The session is being driven remotely, so the desktop belongs to a virtual display adapter.
    ///
    /// Desktop Duplication is not available on one. Windows composes the remote session onto an
    /// indirect display driver, and DXGI refuses to duplicate it — reported as a bare "Access is
    /// denied", which reads like a permissions problem and is not one.
    Remote { protocol: &'static str },
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
        matches!(
            self,
            Self::Locked { .. }
                | Self::NotOurs { .. }
                | Self::Detached { .. }
                | Self::Remote { .. }
        )
    }
}

impl fmt::Display for DesktopState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interactive => formatter.write_str("the session owns an interactive desktop"),
            Self::Locked {
                desktop: Some(name),
            } => write!(
                formatter,
                "the workstation is locked and the {name} desktop owns input"
            ),
            Self::Locked { desktop: None } => formatter.write_str("the workstation is locked"),
            // No longer says "locked": a lock has its own answer now, and this one is what is left
            // when the session is *not* locked and a secure desktop is in front anyway - a UAC
            // prompt, or a screensaver. Saying both would put the reader back to guessing.
            Self::NotOurs {
                desktop: Some(name),
            } => write!(
                formatter,
                "a secure desktop owns input; the {name} desktop is in front"
            ),
            Self::NotOurs { desktop: None } => {
                formatter.write_str("a secure desktop owns input")
            }
            Self::Detached { connect_state } => {
                write!(formatter, "the session is {connect_state}")
            }
            Self::Remote { protocol } => write!(
                formatter,
                "this is a {protocol} session; Windows composes it onto a virtual display and DXGI will not duplicate one. Capture works from the physical console session."
            ),
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
    classify(
        connect_state(),
        input_desktop(),
        remote_protocol(),
        session_locked(),
    )
}

/// Turns what the two probes saw into one answer.
///
/// Separated from the syscalls so the truth table can be tested. It cannot be tested any other
/// way: producing a locked session on a build agent means locking the build agent.
fn classify(
    connect: ConnectState,
    desktop: InputDesktop,
    remote_protocol: Option<&'static str>,
    locked: Option<bool>,
) -> DesktopState {
    // A detached session is reported first. It is the more fundamental condition — a session with
    // no client attached has no desktop for anyone to own — and its desktop probe result is not
    // worth explaining on top of it.
    if let ConnectState::Other(state) = connect {
        return DesktopState::Detached {
            connect_state: state,
        };
    }
    // A remote session owns an ordinary `Default` desktop, so the desktop probe reports it as
    // interactive and is not wrong - the session simply has no duplicable display behind it.
    // Checked before that answer is taken at face value.
    if let Some(protocol) = remote_protocol {
        return DesktopState::Remote { protocol };
    }
    // Before the desktop probe, because it is the only one of the two that answers during the part
    // of a lock where the lock screen sits on the `Default` desktop - which is most of it, and is
    // where duplication starts being refused once the displays power down. `None` means the query
    // itself failed, and falls through rather than inventing a lock.
    if locked == Some(true) {
        return DesktopState::Locked {
            desktop: secure_desktop_name(&desktop),
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

/// Reports the protocol driving this session, when it is not the physical console.
///
/// `WTSClientProtocolType` answers in one call what the symptom never does: a Remote Desktop
/// session composes onto a virtual display adapter, and DXGI will not duplicate one. The failure
/// surfaces as `duplicate_output: Access is denied`, which sends a reader looking for a
/// permissions problem — observed on the development host, where enumeration succeeded, no other
/// process held a duplication, the session was unlocked, and duplication was refused anyway.
fn remote_protocol() -> Option<&'static str> {
    let mut buffer = windows::core::PWSTR::null();
    let mut bytes = 0_u32;
    // SAFETY: The out-parameters are valid for the call; on success Windows allocates a buffer we
    // own until the WTSFreeMemory below.
    let queried = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER,
            WTS_CURRENT_SESSION,
            WTSClientProtocolType,
            &mut buffer,
            &mut bytes,
        )
    };
    if queried.is_err() || buffer.is_null() {
        return None;
    }
    let protocol = (bytes as usize >= std::mem::size_of::<u16>()).then(|| {
        // SAFETY: WTSClientProtocolType returns a USHORT, and the size check confirms Windows
        // returned at least that many bytes.
        unsafe { buffer.as_ptr().cast::<u16>().read_unaligned() }
    });
    // SAFETY: buffer was allocated by the successful query above and is freed exactly once.
    unsafe { WTSFreeMemory(buffer.as_ptr().cast()) };
    match protocol {
        // 0 is the physical console, which is the only one that can be duplicated.
        Some(0) | None => None,
        Some(1) => Some("Citrix ICA"),
        Some(2) => Some("Remote Desktop"),
        Some(_) => Some("remote"),
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

/// The name of the desktop owning input, when it is not this session's ordinary one.
///
/// A locked session usually reports `Default`, which says nothing beyond "not the credential
/// prompt"; only a different name is worth repeating back to the reader.
fn secure_desktop_name(desktop: &InputDesktop) -> Option<String> {
    match desktop {
        InputDesktop::Named(name) if !name.eq_ignore_ascii_case(INTERACTIVE_DESKTOP) => {
            Some(name.clone())
        }
        _ => None,
    }
}

/// Whether Windows considers this session locked.
///
/// The one signal that answers in every phase of a lock. The desktop probe does not: Windows 11's
/// lock screen is an ordinary application on the `Default` desktop, so `OpenInputDesktop` succeeds
/// and answers exactly what it answers for an unlocked session, and input only moves to `Winlogon`
/// while the credential prompt is up. Duplication is denied anyway once the displays power down,
/// which is where a machine left alone ends up.
///
/// `None` rather than a guess when the query fails or answers something outside the two documented
/// values — including the reversed pair Windows Server 2008 R2 is documented to return, which
/// cannot be told apart from the modern one by inspection and is not worth mis-reporting a lock
/// over. Measured here as `WTS_SESSIONSTATE_UNLOCK` while unlocked and `WTS_SESSIONSTATE_LOCK`
/// throughout a lock, flipping exactly at both transitions.
fn session_locked() -> Option<bool> {
    let mut buffer = windows::core::PWSTR::null();
    let mut bytes = 0_u32;
    // SAFETY: The out-parameters are valid for the call; on success Windows allocates a buffer we
    // own until the WTSFreeMemory below.
    let queried = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        )
    };
    if queried.is_err() || buffer.is_null() {
        return None;
    }
    let flags = (bytes as usize >= std::mem::size_of::<WTSINFOEXW>()).then(|| {
        // SAFETY: The size check confirms Windows returned a whole WTSINFOEXW, and level 1 is the
        // only level it defines. Read unaligned because the buffer is Windows-allocated.
        unsafe {
            let info = buffer.as_ptr().cast::<WTSINFOEXW>().read_unaligned();
            info.Data.WTSInfoExLevel1.SessionFlags
        }
    });
    // SAFETY: buffer was allocated by the successful query above and is freed exactly once.
    unsafe { WTSFreeMemory(buffer.as_ptr().cast()) };
    match flags {
        Some(flags) if flags == WTS_SESSIONSTATE_LOCK as i32 => Some(true),
        Some(flags) if flags == WTS_SESSIONSTATE_UNLOCK as i32 => Some(false),
        _ => None,
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

    /// Every duplication failure a locked workstation causes must arrive with the lock attached to
    /// it, for the whole of the lock rather than the part of it where input moves to `Winlogon`.
    ///
    /// This is issue #51's criterion, applied to the state that used to escape it. The test locks
    /// the workstation, powers the displays down - which is where a machine left alone ends up, and
    /// what turns duplication from working to refused about three seconds later - and then samples
    /// the two answers side by side: what `DuplicateOutput` did, and what the session probe said
    /// about it. A failure the probe calls temporary is explained; one it calls interactive is the
    /// bare `Access is denied` that sends a reader looking for a permissions problem.
    ///
    /// Before the lock query existed this failed on 499 of 549 samples across a 137-second lock.
    /// With it, 139 of 140 samples were refused while locked and every one arrived explained; the
    /// odd one out succeeded, because it landed in the seconds before the displays slept.
    ///
    /// **Leaves the workstation locked**, and takes about eight minutes at the default sample
    /// count - a refused `DuplicateOutput` costs a few seconds, so the run is much slower than its
    /// sample count suggests. Nothing can unlock the workstation from here, so the result is
    /// waiting when the operator signs back in. Any input during the run wakes the displays and
    /// ends the condition being measured.
    ///
    /// cargo test --locked -p captastic-windows --release
    ///     -- --ignored --nocapture a_locked_session_explains_every_duplication_failure
    #[test]
    #[ignore = "locks the workstation and powers the displays down"]
    fn a_locked_session_explains_every_duplication_failure() {
        use std::time::{Duration, Instant};

        // Counted rather than timed. A locked session suspends applications, and the broadcast
        // below can wait on each of them in turn - one run spent four minutes in it, which a
        // wall-clock bound turned into a loop that never sampled anything and a test that reported
        // it had measured nothing rather than what it had seen.
        let samples = std::env::var("CAPTASTIC_LOCK_TEST_SAMPLES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(140_u32);

        assert_eq!(
            session_locked(),
            Some(false),
            "the workstation is already locked, or the lock query does not answer on this build; \
             either way what follows would measure nothing"
        );

        let started = Instant::now();
        // SAFETY: takes no arguments and locks the interactive session.
        unsafe { windows::Win32::System::Shutdown::LockWorkStation() }
            .expect("LockWorkStation; this test cannot measure a lock it could not cause");
        std::thread::sleep(Duration::from_secs(3));
        // Duplication keeps working while the lock screen is lit; it is the power-down that ends
        // it, and without this the test would mostly sample the state that was never broken. On
        // its own thread because it can block for minutes, as above.
        std::thread::spawn(power_monitors_off);

        let mut locked_samples = 0_u32;
        let mut refused_while_locked = 0_u32;
        let mut succeeded = 0_u32;
        let mut across_a_transition = 0_u32;
        let mut unexplained = Vec::new();
        for _ in 0..samples {
            // Read either side of the duplication attempt, and judge only the samples where both
            // reads agree. A lock has three phases and the session moves between them while this
            // runs; a sample taken across a move describes no single moment, and judging one
            // reports the unlock transition as though it were the condition under test - which is
            // exactly what an earlier run of this test did, on four samples out of a hundred.
            let locked_before = session_locked();
            let state_before = desktop_state();
            let duplication = crate::dxgi::DxgiBackend::new_primary();
            let state_after = desktop_state();
            let locked_after = session_locked();

            let locked = locked_before == Some(true) && locked_after == Some(true);
            if locked {
                locked_samples += 1;
            }
            match duplication {
                Ok(_) => succeeded += 1,
                Err(_) if !locked => across_a_transition += 1,
                Err(error) => {
                    refused_while_locked += 1;
                    if !state_before.is_temporary() && !state_after.is_temporary() {
                        unexplained.push(format!(
                            "at {:.1}s: {:?} / {} — probe said: {state_before}",
                            started.elapsed().as_secs_f64(),
                            error.kind,
                            error.message
                        ));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        println!(
            "{samples} samples over {:.0}s: {locked_samples} wholly locked, {succeeded}              duplications succeeded, {refused_while_locked} refused while locked,              {across_a_transition} refused across a transition and not judged",
            started.elapsed().as_secs_f64()
        );
        assert!(
            locked_samples > 0,
            "the workstation never reported itself locked, so nothing here was measured. This is              the first thing to check if the lock query is what regressed"
        );
        assert!(
            refused_while_locked > 0,
            "duplication was never refused while locked, so this run cannot say whether such a              failure would have been explained. It is the display power-down rather than the lock              that causes the refusal, so if the monitors stayed awake - a dock or a remote input              device will do that - try again without touching the mouse"
        );
        assert!(
            unexplained.is_empty(),
            "{} of {refused_while_locked} duplication failures came back with the session              reporting an ordinary interactive desktop, which is the bare `Access is denied` that              issue #51 exists to prevent:
  {}",
            unexplained.len(),
            unexplained
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join("
  ")
        );
    }

    /// Asks every top-level window to power the displays down, the way the idle timer would.
    ///
    /// Broadcast with a timeout rather than a plain `SendMessage`, so one unresponsive window
    /// cannot wedge the test - the same shape `scripts/probe-display-power.ps1` already uses.
    fn power_monitors_off() {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_SYSCOMMAND,
        };

        const HWND_BROADCAST: HWND = HWND(0xffff);
        const SC_MONITORPOWER: usize = 0xf170;
        const POWER_OFF: isize = 2;
        // SAFETY: a broadcast of a documented system command, bounded by its own timeout.
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SYSCOMMAND,
                WPARAM(SC_MONITORPOWER),
                LPARAM(POWER_OFF),
                SMTO_ABORTIFHUNG,
                1_000,
                None,
            )
        };
    }

    /// The state that had no explanation: locked, displays asleep, and the desktop probe reporting
    /// an ordinary session.
    ///
    /// This is not a corner. It is where a machine left alone settles, and it lasted 125 seconds of
    /// a 190-second probe run - every sample refusing duplication with a bare `Access is denied`
    /// while `OpenInputDesktop` answered `Default`, because Windows 11's lock screen is an ordinary
    /// application and only the credential prompt moves input to `Winlogon`. Issue #51's whole
    /// point was that this failure should explain itself, and for this state it did not.
    #[test]
    fn a_locked_session_is_reported_even_when_its_desktop_looks_ordinary() {
        let state = classify(
            ConnectState::Active,
            InputDesktop::Named("Default".to_owned()),
            None,
            Some(true),
        );
        assert_eq!(state, DesktopState::Locked { desktop: None });
        assert!(!state.is_interactive());
        // Temporary: it clears the moment the user signs back in, so a daemon should wait it out
        // rather than exit over it.
        assert!(state.is_temporary());
        assert!(
            state.to_string().contains("locked"),
            "the message has to say so, because the symptom says `Access is denied`: {state}"
        );
    }

    /// The other half of a lock, where the credential prompt does own input.
    ///
    /// Both signals agree here, and the more specific one is worth keeping: naming `Winlogon` says
    /// which of the two phases the session is in.
    #[test]
    fn a_locked_session_at_the_credential_prompt_names_the_desktop_too() {
        let state = classify(
            ConnectState::Active,
            InputDesktop::Named("Winlogon".to_owned()),
            None,
            Some(true),
        );
        assert_eq!(
            state,
            DesktopState::Locked {
                desktop: Some("Winlogon".to_owned())
            }
        );
        assert!(state.is_temporary());
        assert!(state.to_string().contains("Winlogon"), "{state}");

        // A refusal carries no name, and inventing one would be worse than saying nothing.
        assert_eq!(
            classify(
                ConnectState::Active,
                InputDesktop::Refused,
                None,
                Some(true),
            ),
            DesktopState::Locked { desktop: None }
        );
    }

    /// A secure desktop in an *unlocked* session is a different thing and keeps its own answer.
    ///
    /// A UAC prompt or a screensaver owns input without the workstation being locked, and telling
    /// a reader their machine is locked when it is not would trade one wrong explanation for
    /// another.
    #[test]
    fn a_secure_prompt_without_a_lock_is_not_reported_as_one() {
        let state = classify(
            ConnectState::Active,
            InputDesktop::Named("Winlogon".to_owned()),
            None,
            Some(false),
        );
        assert_eq!(
            state,
            DesktopState::NotOurs {
                desktop: Some("Winlogon".to_owned())
            }
        );
        assert!(!state.to_string().contains("locked"), "{state}");
    }

    /// A lock query that failed answers nothing, and nothing is what it must contribute.
    ///
    /// `None` is not `false`: reporting an ordinary session because a syscall failed is the guess
    /// this module exists to avoid, and reporting a lock on the same evidence would be worse. The
    /// desktop probe decides on its own, exactly as it did before the lock query existed.
    #[test]
    fn an_unanswered_lock_query_changes_nothing() {
        for desktop in [
            InputDesktop::Named("Default".to_owned()),
            InputDesktop::Named("Winlogon".to_owned()),
            InputDesktop::Refused,
        ] {
            assert_eq!(
                classify(ConnectState::Active, desktop.clone(), None, None),
                classify(ConnectState::Active, desktop.clone(), None, Some(false)),
                "an unanswered lock query should decide nothing for {desktop:?}"
            );
        }
    }

    /// A lock does not outrank the conditions that are more fundamental than it.
    ///
    /// A session with no client attached has no desktop for anyone to lock, and a remote session's
    /// duplication is refused whether it is locked or not - and for a different reason, which is
    /// the one worth printing.
    #[test]
    fn a_detached_or_remote_session_outranks_the_lock() {
        assert_eq!(
            classify(
                ConnectState::Other("disconnected"),
                InputDesktop::Named("Default".to_owned()),
                None,
                Some(true),
            ),
            DesktopState::Detached {
                connect_state: "disconnected"
            }
        );
        assert_eq!(
            classify(
                ConnectState::Active,
                InputDesktop::Named("Default".to_owned()),
                Some("Remote Desktop"),
                Some(true),
            ),
            DesktopState::Remote {
                protocol: "Remote Desktop"
            }
        );
    }

    #[test]
    fn a_refused_input_desktop_reads_as_a_secure_desktop_rather_than_a_failure() {
        // A secure desktop denies OpenInputDesktop outright. Treating that as an error would put
        // the daemon back where issue #51 found it, reporting something that sounds like broken
        // hardware. It is not by itself a lock - the session here is not locked - which is why
        // this asks about the desktop rather than about the workstation.
        let state = classify(
            ConnectState::Active,
            InputDesktop::Refused,
            None,
            Some(false),
        );
        assert_eq!(state, DesktopState::NotOurs { desktop: None });
        assert!(state.is_temporary());
        assert!(!state.is_interactive());
    }

    #[test]
    fn the_secure_desktop_is_named_when_windows_names_it() {
        let state = classify(
            ConnectState::Active,
            InputDesktop::Named("Winlogon".to_owned()),
            None,
            Some(false),
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
            let state = classify(
                ConnectState::Other("disconnected"),
                desktop,
                None,
                Some(false),
            );
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
    fn a_remote_session_is_named_even_though_its_desktop_looks_ordinary() {
        // The case that costs the most time to diagnose without this. A Remote Desktop session
        // owns a perfectly ordinary `Default` desktop and reports itself active, so every other
        // signal says "fine" - and duplication is refused anyway, because Windows composes the
        // session onto a virtual display adapter that DXGI will not duplicate. The bare "Access
        // is denied" that results reads like a permissions problem and is not one.
        let state = classify(
            ConnectState::Active,
            InputDesktop::Named("Default".to_owned()),
            Some("Remote Desktop"),
            Some(false),
        );
        assert_eq!(
            state,
            DesktopState::Remote {
                protocol: "Remote Desktop"
            }
        );
        assert!(!state.is_interactive());
        // Temporary: it clears when the user sits back down at the console.
        assert!(state.is_temporary());
        assert!(state.to_string().contains("console"), "{state}");

        // A console session is not remote, and must not be reported as one.
        assert!(classify(
            ConnectState::Active,
            InputDesktop::Named("Default".to_owned()),
            None,
            Some(false)
        )
        .is_interactive());
    }

    #[test]
    fn a_disconnected_remote_session_reports_the_disconnection_first() {
        // Both are true at once when an RDP client drops. The disconnection is the more
        // fundamental fact - there is no session to duplicate for, remote display or not.
        assert_eq!(
            classify(
                ConnectState::Other("disconnected"),
                InputDesktop::Refused,
                Some("Remote Desktop"),
                Some(false)
            ),
            DesktopState::Detached {
                connect_state: "disconnected"
            }
        );
    }

    #[test]
    fn an_unanswered_probe_is_not_treated_as_temporary() {
        // Waiting forever on a question nobody answered would turn one failed syscall into a
        // daemon that never captures and never says why.
        let state = classify(
            ConnectState::Active,
            InputDesktop::Failed("no name".to_owned()),
            None,
            Some(false),
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
                InputDesktop::Named("Default".to_owned()),
                None,
                Some(false),
            ),
            DesktopState::Interactive
        );
        assert!(classify(
            ConnectState::Unavailable,
            InputDesktop::Refused,
            None,
            Some(false)
        )
        .is_temporary());
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
