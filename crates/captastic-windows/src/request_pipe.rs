//! The daemon's request channel (ADR 0014): a named pipe a command-line program sends one request
//! down and reads one answer back from, each a single line of bytes.
//!
//! Pipe names are machine-wide, unlike the `Local\` control event of ADR 0007, so the boundary is
//! kept by the pipe itself. Its name carries the session id, its DACL admits only the user running
//! the daemon, and it is owned by that user. The daemon creates it with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, so it cannot join a pipe someone else created first. A client
//! checks the other way round: it talks only to a pipe owned by its own user, served from its own
//! session, and lets the server identify it but never act as it.
//!
//! Every wait is overlapped, so a client that connects and never writes, or never reads its answer,
//! costs the listener a timeout rather than the listener itself, and a stop is seen at once.

use std::time::{Duration, Instant};

use captastic_core::{CaptureError, CaptureErrorKind};
use windows::core::{Error as WindowsError, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, PSID,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerSessionId,
    WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentProcessId, OpenProcessToken, SetEvent,
    WaitForMultipleObjects,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const BACKEND: &str = "windows-request-pipe";
const BUFFER_BYTES: u32 = 64 * 1024;
const NEWLINE: u8 = b'\n';

/// `\\.\pipe\captastic-<session>-v1`: the request pipe of the daemon in this process's session.
pub fn request_pipe_name() -> Result<String, CaptureError> {
    Ok(format!(r"\\.\pipe\captastic-{}-v1", current_session()?))
}

/// The listening end, held by the daemon. One client is served at a time; the next one waits in
/// the pipe's queue of instances.
pub struct RequestPipeServer {
    name: HSTRING,
    /// Set by [`RequestPipeStopper::stop`], which ends a wait for a client or for its bytes.
    stop: OwnedHandle,
    security: SecurityDescriptor,
    /// The instance the next client connects to.
    pipe: OwnedHandle,
}

/// Stops a [`RequestPipeServer`] from another thread.
#[derive(Clone)]
pub struct RequestPipeStopper {
    stop: isize,
}

impl RequestPipeStopper {
    pub fn stop(&self) {
        // SAFETY: Setting an event is safe on any handle value; one already closed fails cleanly.
        let _ = unsafe { SetEvent(HANDLE(self.stop)) };
    }
}

// SAFETY: The handles are owned by the server and used by one thread at a time; the event a
// stopper sets is only ever signalled, never closed, through it.
unsafe impl Send for RequestPipeServer {}

/// One connected client.
pub struct RequestPipeConnection<'a> {
    server: &'a RequestPipeServer,
    pipe: OwnedHandle,
}

impl RequestPipeServer {
    /// Creates this session's request pipe. Fails if one already exists, whoever made it: another
    /// daemon, or someone squatting on the name.
    pub fn create() -> Result<Self, CaptureError> {
        Self::create_named(&request_pipe_name()?)
    }

    /// As [`Self::create`], under a name of the caller's choosing, for tests that must not meet a
    /// daemon running on the same desktop.
    pub fn create_named(name: &str) -> Result<Self, CaptureError> {
        let security = SecurityDescriptor::for_current_user()?;
        let name = HSTRING::from(name);
        let pipe = create_instance(&name, &security, true)?;
        // SAFETY: An anonymous manual-reset event, owned by the server.
        let stop = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|error| pipe_error("create_stop_event", error))?;
        Ok(Self {
            name,
            stop: OwnedHandle(stop),
            security,
            pipe,
        })
    }

    pub fn stopper(&self) -> RequestPipeStopper {
        RequestPipeStopper {
            stop: self.stop.0 .0,
        }
    }

    /// Waits for the next client. `Ok(None)` once stopped.
    pub fn accept(&mut self) -> Result<Option<RequestPipeConnection<'_>>, CaptureError> {
        let overlapped = Overlapped::new()?;
        // SAFETY: The pipe and the OVERLAPPED both outlive the operation, which is waited for or
        // cancelled below before either is dropped.
        let started = unsafe { ConnectNamedPipe(self.pipe.0, Some(overlapped.pointer())) };
        let connected = match started {
            Ok(()) => true,
            // A client connected between creating the instance and this call.
            Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => true,
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                match overlapped.wait(self.pipe.0, Some(&self.stop), None)? {
                    Waited::Done(_) => true,
                    Waited::Stopped => false,
                    Waited::TimedOut => unreachable!("no timeout was given"),
                }
            }
            Err(error) => return Err(pipe_error("connect_named_pipe", error)),
        };
        if !connected {
            return Ok(None);
        }
        // The next client gets a fresh instance; this one is the connection's until it drops.
        let next = create_instance(&self.name, &self.security, false)?;
        let pipe = std::mem::replace(&mut self.pipe, next);
        Ok(Some(RequestPipeConnection { server: self, pipe }))
    }
}

impl RequestPipeConnection<'_> {
    /// Reads the client's request: bytes up to the first newline, at most `limit` of them, within
    /// `timeout`. `Ok(None)` if the server was stopped meanwhile.
    pub fn read_line(
        &mut self,
        limit: usize,
        timeout: Duration,
    ) -> Result<Option<Vec<u8>>, CaptureError> {
        read_line(self.pipe.0, Some(&self.server.stop), limit, timeout)
    }

    /// Writes the answer and a newline, within `timeout`.
    pub fn write_line(&mut self, line: &[u8], timeout: Duration) -> Result<(), CaptureError> {
        write_all(self.pipe.0, Some(&self.server.stop), line, timeout)?;
        // SAFETY: Waits until the client has read what was written, so disconnecting below does
        // not discard it.
        let _ = unsafe { FlushFileBuffers(self.pipe.0) };
        Ok(())
    }
}

impl Drop for RequestPipeConnection<'_> {
    fn drop(&mut self) {
        // SAFETY: The instance is this connection's own; disconnecting it ends the client's end.
        let _ = unsafe { DisconnectNamedPipe(self.pipe.0) };
    }
}

/// Sends one request line to the daemon's pipe named `name` and returns its answer line, at most
/// `limit` bytes. [`RequestError::NotRunning`] when there is no pipe to talk to.
pub fn send_request(
    name: &str,
    request: &[u8],
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>, RequestError> {
    let deadline = Instant::now() + timeout;
    let name = HSTRING::from(name);
    let pipe = loop {
        // SAFETY: Opens the named pipe for overlapped reads and writes. SECURITY_IDENTIFICATION
        // lets the server learn who is asking but not act as them.
        let opened = unsafe {
            CreateFileW(
                &name,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        };
        match opened {
            Ok(pipe) => break OwnedHandle(pipe),
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {
                return Err(RequestError::NotRunning)
            }
            Err(error) if error.code() == ERROR_PIPE_BUSY.to_hresult() => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(RequestError::Busy);
                }
                // SAFETY: Waits for an instance of the named pipe to be free.
                let _ = unsafe { WaitNamedPipeW(&name, millis(left).max(1)) };
            }
            Err(error) => return Err(RequestError::Failed(pipe_error("open_request_pipe", error))),
        }
    };
    verify_server(pipe.0).map_err(RequestError::Untrusted)?;
    let left = deadline.saturating_duration_since(Instant::now());
    write_all(pipe.0, None, request, left).map_err(RequestError::Failed)?;
    let left = deadline.saturating_duration_since(Instant::now());
    read_line(pipe.0, None, limit, left)
        .map_err(RequestError::Failed)?
        .ok_or_else(|| RequestError::Failed(synthetic("read_answer", "the daemon stopped")))
}

/// Why a request got no answer.
#[derive(Debug)]
pub enum RequestError {
    /// No daemon is listening in this session.
    NotRunning,
    /// Every instance stayed taken until the deadline.
    Busy,
    /// Something is serving the pipe, but not a daemon of this user in this session.
    Untrusted(CaptureError),
    Failed(CaptureError),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning => formatter.write_str("Captastic is not running in this session"),
            Self::Busy => formatter.write_str("Captastic is busy with other requests"),
            Self::Untrusted(error) => write!(
                formatter,
                "the request pipe is not served by this user's Captastic: {error}"
            ),
            Self::Failed(error) => write!(formatter, "the request to Captastic failed: {error}"),
        }
    }
}

impl std::error::Error for RequestError {}

/// Refuses a pipe that is not owned by this user or not served from this session: the name is
/// machine-wide, so anyone could have created it first.
fn verify_server(pipe: HANDLE) -> Result<(), CaptureError> {
    let mut session = 0;
    // SAFETY: Reads the server's session id into a local.
    unsafe { GetNamedPipeServerSessionId(pipe, &mut session) }
        .map_err(|error| pipe_error("get_pipe_server_session", error))?;
    if session != current_session()? {
        return Err(synthetic(
            "verify_request_pipe",
            format!("the pipe is served from session {session}, not this one"),
        ));
    }
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: Asks for the pipe's owner; the descriptor holding it is freed below.
    unsafe {
        GetSecurityInfo(
            pipe,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION.0,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut descriptor),
        )
    }
    .map_err(|error| pipe_error("get_pipe_owner", error))?;
    let user = TokenUserSid::current()?;
    // SAFETY: Both SIDs are valid: one inside `descriptor`, one inside `user`.
    let same = unsafe { EqualSid(owner, user.sid()) }.is_ok();
    // SAFETY: Frees the descriptor GetSecurityInfo allocated; `owner` pointed into it.
    let _ = unsafe { LocalFree(HLOCAL(descriptor.0)) };
    if same {
        Ok(())
    } else {
        Err(synthetic(
            "verify_request_pipe",
            "the pipe is owned by another user",
        ))
    }
}

fn create_instance(
    name: &HSTRING,
    security: &SecurityDescriptor,
    first: bool,
) -> Result<OwnedHandle, CaptureError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.0 .0,
        bInheritHandle: false.into(),
    };
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: The name and the security attributes outlive the call.
    let pipe = unsafe {
        CreateNamedPipeW(
            name,
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_BYTES,
            BUFFER_BYTES,
            0,
            Some(&attributes),
        )
    };
    if pipe.is_invalid() {
        let error = WindowsError::from_win32();
        return Err(CaptureError {
            kind: CaptureErrorKind::SourceUnavailable,
            backend: BACKEND,
            operation: "create_request_pipe",
            message: if first {
                format!("the request pipe already exists or could not be created: {error}")
            } else {
                error.to_string()
            },
            retryable: false,
            native_code: Some(i64::from(error.code().0)),
        });
    }
    Ok(OwnedHandle(pipe))
}

fn read_line(
    pipe: HANDLE,
    stop: Option<&OwnedHandle>,
    limit: usize,
    timeout: Duration,
) -> Result<Option<Vec<u8>>, CaptureError> {
    let deadline = Instant::now() + timeout;
    let mut line = Vec::new();
    let mut buffer = vec![0_u8; BUFFER_BYTES as usize];
    loop {
        let overlapped = Overlapped::new()?;
        // SAFETY: The buffer and the OVERLAPPED outlive the operation, which is waited for or
        // cancelled before either is dropped. Its event is set whether it completes now or later.
        let started =
            unsafe { ReadFile(pipe, Some(&mut buffer), None, Some(overlapped.pointer())) };
        match started {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {}
            Err(error) if error.code() == ERROR_BROKEN_PIPE.to_hresult() => {
                return Err(closed_early());
            }
            Err(error) => return Err(pipe_error("read_request_pipe", error)),
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let read = match overlapped.wait(pipe, stop, Some(left))? {
            Waited::Done(read) => read,
            Waited::Stopped => return Ok(None),
            Waited::TimedOut => return Err(timed_out("read_request_pipe")),
        };
        if read == 0 {
            return Err(closed_early());
        }
        let chunk = &buffer[..read as usize];
        if let Some(end) = chunk.iter().position(|byte| *byte == NEWLINE) {
            line.extend_from_slice(&chunk[..end]);
            return if line.len() > limit {
                Err(too_long(limit))
            } else {
                Ok(Some(line))
            };
        }
        line.extend_from_slice(chunk);
        if line.len() > limit {
            return Err(too_long(limit));
        }
    }
}

fn write_all(
    pipe: HANDLE,
    stop: Option<&OwnedHandle>,
    line: &[u8],
    timeout: Duration,
) -> Result<(), CaptureError> {
    let deadline = Instant::now() + timeout;
    let mut bytes = line.to_vec();
    bytes.push(NEWLINE);
    let mut sent = 0;
    while sent < bytes.len() {
        let overlapped = Overlapped::new()?;
        // SAFETY: As for ReadFile above.
        let started =
            unsafe { WriteFile(pipe, Some(&bytes[sent..]), None, Some(overlapped.pointer())) };
        match started {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {}
            Err(error) => return Err(pipe_error("write_request_pipe", error)),
        }
        let left = deadline.saturating_duration_since(Instant::now());
        let written = match overlapped.wait(pipe, stop, Some(left))? {
            Waited::Done(written) => written,
            Waited::Stopped => return Err(synthetic("write_request_pipe", "the server stopped")),
            Waited::TimedOut => return Err(timed_out("write_request_pipe")),
        };
        sent += written as usize;
    }
    Ok(())
}

/// An OVERLAPPED with an event of its own.
struct Overlapped {
    event: OwnedHandle,
    overlapped: Box<OVERLAPPED>,
}

enum Waited {
    Done(u32),
    Stopped,
    TimedOut,
}

impl Overlapped {
    fn new() -> Result<Self, CaptureError> {
        // SAFETY: An anonymous manual-reset event for one operation.
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|error| pipe_error("create_io_event", error))?;
        let overlapped = Box::new(OVERLAPPED {
            hEvent: event,
            ..Default::default()
        });
        Ok(Self {
            event: OwnedHandle(event),
            overlapped,
        })
    }

    fn pointer(&self) -> *mut OVERLAPPED {
        (&*self.overlapped as *const OVERLAPPED).cast_mut()
    }

    /// Waits for the operation, for `stop`, or for `timeout`. An operation not done is cancelled
    /// and waited for, so nothing writes into memory this frees.
    fn wait(
        &self,
        file: HANDLE,
        stop: Option<&OwnedHandle>,
        timeout: Option<Duration>,
    ) -> Result<Waited, CaptureError> {
        let milliseconds = timeout.map_or(u32::MAX, millis);
        let both;
        let events: &[HANDLE] = match stop {
            Some(stop) => {
                both = [self.event.0, stop.0];
                &both
            }
            None => std::slice::from_ref(&self.event.0),
        };
        // SAFETY: Every handle is a live event.
        let waited = unsafe { WaitForMultipleObjects(events, false, milliseconds) };
        let mut transferred = 0_u32;
        if waited == WAIT_OBJECT_0 {
            // SAFETY: The operation has completed; this reads its result.
            return match unsafe {
                GetOverlappedResult(file, self.pointer(), &mut transferred, false)
            } {
                Ok(()) => Ok(Waited::Done(transferred)),
                Err(error) if error.code() == ERROR_BROKEN_PIPE.to_hresult() => Err(closed_early()),
                Err(error) => Err(pipe_error("complete_request_pipe_io", error)),
            };
        }
        // SAFETY: Cancels this operation only, then waits for the cancellation to land so the
        // buffer and the OVERLAPPED may be dropped.
        unsafe {
            let _ = CancelIoEx(file, Some(self.pointer()));
            let _ = GetOverlappedResult(file, self.pointer(), &mut transferred, true);
        }
        Ok(if waited == WAIT_TIMEOUT {
            Waited::TimedOut
        } else {
            Waited::Stopped
        })
    }
}

/// A security descriptor admitting only this process's user, and owned by them.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn for_current_user() -> Result<Self, CaptureError> {
        let user = TokenUserSid::current()?;
        let sid = user.to_string()?;
        // Owner the user; a protected DACL with one entry, all access for the user.
        let sddl = HSTRING::from(format!("O:{sid}D:P(A;;GA;;;{sid})"));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: The SDDL string outlives the call; the descriptor is freed on drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|error| pipe_error("build_request_pipe_security", error))?;
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: Frees the descriptor ConvertStringSecurityDescriptorToSecurityDescriptorW made.
        let _ = unsafe { LocalFree(HLOCAL(self.0 .0)) };
    }
}

/// This process's user, read from its token.
struct TokenUserSid {
    buffer: Vec<u64>,
}

impl TokenUserSid {
    fn current() -> Result<Self, CaptureError> {
        let mut token = HANDLE::default();
        // SAFETY: Opens this process's own token for reading; closed below.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(|error| pipe_error("open_process_token", error))?;
        let token = OwnedHandle(token);
        let mut length = 0_u32;
        // SAFETY: A size query; it fails with the length needed.
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut length) };
        // u64s, so the TOKEN_USER at the front is aligned.
        let mut buffer = vec![0_u64; (length as usize).div_ceil(8).max(1)];
        // SAFETY: The buffer holds at least `length` bytes.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                (buffer.len() * 8) as u32,
                &mut length,
            )
        }
        .map_err(|error| pipe_error("read_token_user", error))?;
        Ok(Self { buffer })
    }

    fn sid(&self) -> PSID {
        // SAFETY: The buffer holds the TOKEN_USER GetTokenInformation wrote.
        unsafe { (*self.buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn to_string(&self) -> Result<String, CaptureError> {
        let mut text = PWSTR::null();
        // SAFETY: The SID is valid; the string is freed below.
        unsafe { ConvertSidToStringSidW(self.sid(), &mut text) }
            .map_err(|error| pipe_error("format_user_sid", error))?;
        // SAFETY: `text` is a NUL-terminated string from ConvertSidToStringSidW.
        let converted = unsafe { text.to_string() };
        // SAFETY: Frees the string ConvertSidToStringSidW allocated.
        let _ = unsafe { LocalFree(HLOCAL(text.0.cast())) };
        converted.map_err(|error| synthetic("format_user_sid", error.to_string()))
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: The handle is owned here and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn current_session() -> Result<u32, CaptureError> {
    let mut session = 0;
    // SAFETY: Reads this process's session id into a local.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
        .map_err(|error| pipe_error("read_session_id", error))?;
    Ok(session)
}

fn millis(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis()).unwrap_or(u32::MAX - 1)
}

fn pipe_error(operation: &'static str, error: WindowsError) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::SourceUnavailable,
        backend: BACKEND,
        operation,
        message: error.to_string(),
        retryable: false,
        native_code: Some(i64::from(error.code().0)),
    }
}

fn synthetic(operation: &'static str, message: impl Into<String>) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::SourceUnavailable,
        backend: BACKEND,
        operation,
        message: message.into(),
        retryable: false,
        native_code: None,
    }
}

fn timed_out(operation: &'static str) -> CaptureError {
    synthetic(operation, "the other end did not keep up in time")
}

fn closed_early() -> CaptureError {
    synthetic(
        "read_request_pipe",
        "the other end closed before a whole line arrived",
    )
}

fn too_long(limit: usize) -> CaptureError {
    synthetic(
        "read_request_pipe",
        format!("the line is longer than {limit} bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    /// A pipe name no daemon on this desktop, and no other test, is using.
    fn test_pipe_name() -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            r"\\.\pipe\captastic-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Serves `answers` clients, answering each request with `answer(request)`.
    fn serve(
        name: &str,
        clients: usize,
        answer: fn(Vec<u8>) -> Vec<u8>,
    ) -> (RequestPipeStopper, thread::JoinHandle<()>) {
        let mut server = RequestPipeServer::create_named(name).expect("server");
        let stopper = server.stopper();
        let join = thread::spawn(move || {
            for _ in 0..clients {
                let Some(mut connection) = server.accept().expect("accept") else {
                    return;
                };
                if let Ok(Some(request)) = connection.read_line(1024, Duration::from_secs(5)) {
                    let _ = connection.write_line(&answer(request), Duration::from_secs(5));
                }
            }
        });
        (stopper, join)
    }

    #[test]
    fn a_request_gets_its_answer_and_several_clients_are_served_in_turn() {
        let name = test_pipe_name();
        let (_stopper, join) = serve(&name, 3, |mut request| {
            request.reverse();
            request
        });
        for request in ["status", "displays", "capture"] {
            let answer = send_request(&name, request.as_bytes(), 1024, Duration::from_secs(5))
                .expect("answer");
            let mut expected = request.as_bytes().to_vec();
            expected.reverse();
            assert_eq!(answer, expected);
        }
        join.join().expect("server thread");
    }

    #[test]
    fn a_long_answer_arrives_whole() {
        let name = test_pipe_name();
        let (_stopper, join) = serve(&name, 1, |_| vec![b'x'; 300_000]);
        let answer =
            send_request(&name, b"windows", 1 << 20, Duration::from_secs(5)).expect("answer");
        assert_eq!(answer.len(), 300_000);
        join.join().expect("server thread");
    }

    #[test]
    fn nobody_listening_is_not_running() {
        assert!(matches!(
            send_request(&test_pipe_name(), b"status", 1024, Duration::from_secs(1)),
            Err(RequestError::NotRunning)
        ));
    }

    #[test]
    fn a_second_server_cannot_take_the_name() {
        let name = test_pipe_name();
        let _first = RequestPipeServer::create_named(&name).expect("first");
        assert!(RequestPipeServer::create_named(&name).is_err());
    }

    #[test]
    fn a_request_over_the_limit_is_refused_and_the_server_carries_on() {
        let name = test_pipe_name();
        let mut server = RequestPipeServer::create_named(&name).expect("server");
        let client_name = name.clone();
        let client = thread::spawn(move || {
            let _ = send_request(&client_name, &[b'y'; 5000], 16, Duration::from_secs(2));
            send_request(&client_name, b"ok", 16, Duration::from_secs(5))
        });
        let mut connection = server.accept().expect("accept").expect("a client");
        assert!(connection.read_line(1024, Duration::from_secs(5)).is_err());
        drop(connection);
        let mut connection = server.accept().expect("accept").expect("a second client");
        let request = connection
            .read_line(1024, Duration::from_secs(5))
            .expect("read")
            .expect("a line");
        connection
            .write_line(&request, Duration::from_secs(5))
            .expect("write");
        drop(connection);
        assert_eq!(client.join().expect("client").expect("answer"), b"ok");
    }

    #[test]
    fn a_silent_client_times_out_and_a_stop_ends_the_wait_for_the_next() {
        let name = test_pipe_name();
        let mut server = RequestPipeServer::create_named(&name).expect("server");
        let stopper = server.stopper();
        let client_name = name.clone();
        // Connects and says nothing.
        let silent = thread::spawn(move || {
            // SAFETY: Opens the test pipe and holds it open without writing.
            let pipe = unsafe {
                CreateFileW(
                    &HSTRING::from(client_name),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    Default::default(),
                    None,
                )
            }
            .expect("open");
            thread::sleep(Duration::from_millis(600));
            drop(OwnedHandle(pipe));
        });
        let mut connection = server.accept().expect("accept").expect("a client");
        let started = Instant::now();
        assert!(connection
            .read_line(1024, Duration::from_millis(200))
            .is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(connection);
        silent.join().expect("silent client");

        let stopping = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            stopper.stop();
        });
        assert!(server.accept().expect("accept").is_none());
        stopping.join().expect("stopper");
    }

    #[test]
    fn the_pipe_is_named_for_the_session() {
        let name = request_pipe_name().expect("name");
        assert!(name.starts_with(r"\\.\pipe\captastic-"), "{name}");
        assert!(name.ends_with("-v1"), "{name}");
    }
}
