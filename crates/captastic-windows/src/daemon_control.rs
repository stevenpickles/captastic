use captastic_core::{CaptureError, CaptureErrorKind};
use windows::core::{w, Error as WindowsError};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ,
    FILE_MAP_WRITE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, OpenEventW, OpenProcess, QueryFullProcessImageNameW,
    SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

const CONTROL_EVENT_NAME: windows::core::PCWSTR = w!("Local\\CaptasticDaemonControl-v1");
/// Names the process holding the control event, so a caller can tell a daemon from a squatter.
///
/// A separate object because an event carries no payload. Same session and the same default security
/// as the event itself: this identifies the holder, it does not protect the name.
const CONTROL_OWNER_NAME: windows::core::PCWSTR = w!("Local\\\\CaptasticDaemonControl-v1-owner");
const HRESULT_ALREADY_EXISTS: i32 = 0x8007_00B7_u32 as i32;

pub struct DaemonControl {
    event: HANDLE,
    /// Publishes this process's ID for as long as the daemon runs, so the record cannot outlive the
    /// daemon it describes.
    owner: Option<HANDLE>,
}

impl DaemonControl {
    pub fn create() -> Result<Self, CaptureError> {
        // SAFETY: Default same-user security is requested, and the static name has process lifetime.
        let event = unsafe { CreateEventW(None, true, false, CONTROL_EVENT_NAME) }
            .map_err(|error| control_error("create_daemon_control", error, false))?;
        // SAFETY: This reads the calling thread's last-error value immediately after CreateEventW.
        let already_exists =
            unsafe { GetLastError() }.is_err_and(|error| error.code().0 == HRESULT_ALREADY_EXISTS);
        if already_exists {
            // SAFETY: event is the valid handle returned for the existing named event.
            let _ = unsafe { CloseHandle(event) };
            return Err(CaptureError {
                kind: CaptureErrorKind::SourceUnavailable,
                backend: "windows-daemon-control",
                operation: "create_daemon_control",
                message: describe_existing_holder(),
                retryable: false,
                native_code: Some(i64::from(ERROR_ALREADY_EXISTS.0)),
            });
        }
        // Best effort: a daemon that cannot publish its identity still works, it is merely harder
        // to tell apart from a squatter next time.
        let owner = publish_owner_pid(CONTROL_OWNER_NAME).unwrap_or_else(|error| {
            log::debug!("could not publish the daemon control owner: {error}");
            None
        });
        Ok(Self { event, owner })
    }

    pub fn requested(&self) -> bool {
        // SAFETY: event is a live event handle owned by this guard; a zero timeout never blocks.
        (unsafe { WaitForSingleObject(self.event, 0) }) == WAIT_OBJECT_0
    }

    pub fn is_running() -> bool {
        match open_control_event() {
            Ok(event) => {
                // SAFETY: Closes the temporary event handle returned by OpenEventW.
                let _ = unsafe { CloseHandle(event) };
                true
            }
            Err(_) => false,
        }
    }

    pub fn request_stop() -> Result<bool, CaptureError> {
        let event = match open_control_event() {
            Ok(event) => event,
            Err(_) => return Ok(false),
        };
        // SAFETY: event was opened with EVENT_MODIFY_STATE specifically for this operation.
        let result = unsafe { SetEvent(event) };
        // SAFETY: event is no longer needed after signaling and is closed exactly once.
        let _ = unsafe { CloseHandle(event) };
        result
            .map(|()| true)
            .map_err(|error| control_error("request_daemon_stop", error, true))
    }
}

impl Drop for DaemonControl {
    fn drop(&mut self) {
        // SAFETY: event is the unique live handle retained by this guard.
        let _ = unsafe { CloseHandle(self.event) };
        if let Some(owner) = self.owner.take() {
            // SAFETY: owner is the unique live mapping handle retained by this guard.
            let _ = unsafe { CloseHandle(owner) };
        }
    }
}

/// Explains who already holds the control name, rather than assuming it is a daemon.
///
/// The previous message said "another Captastic daemon is already running" in every case, including
/// a name taken by something that is not Captastic at all. That is a same-session denial of service
/// with a misleading diagnosis, and the diagnosis is the part worth fixing: the trust boundary is
/// unchanged and deliberate, because any process in this session can take this name and a
/// single-user tool has nothing to defend against there.
fn describe_existing_holder() -> String {
    let Some(pid) = owner_pid(CONTROL_OWNER_NAME) else {
        return "the daemon control channel is already held, but its holder did not identify \
                itself. Either a Captastic daemon older than this one is running, or another \
                process in this session has taken the name"
            .to_owned();
    };
    match process_image_name(pid) {
        Some(image) if image.eq_ignore_ascii_case("captastic.exe") => {
            format!("another Captastic daemon is already running in this session (process {pid})")
        }
        Some(image) => format!(
            "the daemon control channel is held by process {pid} ({image}), which is not a \
             Captastic daemon"
        ),
        None => format!(
            "the daemon control channel names process {pid} as its holder, but that process is \
             gone; the name should be released shortly"
        ),
    }
}

/// Publishes this process's ID alongside the control event.
fn publish_owner_pid(name: windows::core::PCWSTR) -> windows::core::Result<Option<HANDLE>> {
    const SIZE: usize = std::mem::size_of::<u32>();
    // SAFETY: A page-backed anonymous mapping under the static name; no file backing is requested.
    let mapping = unsafe {
        CreateFileMappingW(
            windows::Win32::Foundation::INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            SIZE as u32,
            name,
        )
    }?;
    // SAFETY: The mapping was just created with at least SIZE writable bytes.
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, SIZE) };
    if view.Value.is_null() {
        // SAFETY: mapping is live and owned here, and nothing else refers to it yet.
        let _ = unsafe { CloseHandle(mapping) };
        return Ok(None);
    }
    // SAFETY: view covers SIZE writable bytes that nothing else reads until this returns.
    unsafe {
        view.Value
            .cast::<u32>()
            .write_unaligned(GetCurrentProcessId())
    };
    // SAFETY: view came from the matching MapViewOfFile and is not used again.
    let _ = unsafe { UnmapViewOfFile(view) };
    Ok(Some(mapping))
}

/// Reads the published owner PID, if one was published.
fn owner_pid(name: windows::core::PCWSTR) -> Option<u32> {
    const SIZE: usize = std::mem::size_of::<u32>();
    // SAFETY: Opens the existing named mapping for reading; a missing name simply fails.
    let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, name) }.ok()?;
    // SAFETY: The mapping was opened for reading and is at least SIZE bytes.
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, SIZE) };
    let pid = if view.Value.is_null() {
        None
    } else {
        // SAFETY: view covers SIZE readable bytes written by whoever published them.
        let pid = unsafe { view.Value.cast::<u32>().read_unaligned() };
        // SAFETY: view came from the matching MapViewOfFile and is not used again.
        let _ = unsafe { UnmapViewOfFile(view) };
        Some(pid)
    };
    // SAFETY: mapping is the only handle held here and is closed exactly once.
    let _ = unsafe { CloseHandle(mapping) };
    pid.filter(|pid| *pid != 0)
}

/// The file name of a running process, or `None` if it is gone or will not say.
fn process_image_name(pid: u32) -> Option<String> {
    // SAFETY: Requests the least privilege that can name an image; a dead PID simply fails.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buffer = [0_u16; 260];
    let mut length = buffer.len() as u32;
    // SAFETY: buffer is `length` writable u16s, and the handle was opened for exactly this query.
    let queried = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    // SAFETY: process is the only handle held here and is closed exactly once.
    let _ = unsafe { CloseHandle(process) };
    queried.ok()?;
    let path = String::from_utf16_lossy(&buffer[..length as usize]);
    path.rsplit([SEPARATORS[0], SEPARATORS[1]])
        .next()
        .map(str::to_owned)
}

/// Path separators a Windows image path may use.
const SEPARATORS: [char; 2] = ['\\', '/'];

fn open_control_event() -> windows::core::Result<HANDLE> {
    // SAFETY: The static name has process lifetime and no handle inheritance is requested.
    unsafe { OpenEventW(EVENT_MODIFY_STATE, false, CONTROL_EVENT_NAME) }
}

fn control_error(operation: &'static str, error: WindowsError, retryable: bool) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-daemon-control",
        operation,
        message: error.to_string(),
        retryable,
        native_code: Some(i64::from(error.code().0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name of this test's own, so exercising the record cannot overwrite a running daemon's.
    const TEST_OWNER_NAME: windows::core::PCWSTR = w!("Local\\CaptasticDaemonControlTest-owner");

    #[test]
    fn a_published_owner_can_be_read_back_and_named() {
        let published = publish_owner_pid(TEST_OWNER_NAME)
            .expect("publishing an owner record")
            .expect("a mapping handle");

        // SAFETY: GetCurrentProcessId has no preconditions.
        let expected = unsafe { GetCurrentProcessId() };
        assert_eq!(owner_pid(TEST_OWNER_NAME), Some(expected));

        // The other half of the diagnosis: a PID is only useful if it can be turned into a name.
        let image = process_image_name(expected).expect("this process can name itself");
        assert!(
            image.ends_with(".exe"),
            "expected an image file name, got {image}"
        );

        // SAFETY: published is the unique handle returned above and is closed exactly once.
        let _ = unsafe { CloseHandle(published) };
    }

    #[test]
    fn an_unpublished_name_reports_no_owner() {
        // The state a daemon older than this record format leaves behind, which is why the
        // "did not identify itself" branch exists rather than being treated as impossible.
        assert_eq!(
            owner_pid(w!("Local\\CaptasticDaemonControlTest-absent")),
            None
        );
    }

    #[test]
    fn a_dead_process_cannot_be_named() {
        // PID 0 is the idle process and can never be opened for a query like this, which stands in
        // for the stale-record case the caller has to describe differently.
        assert_eq!(process_image_name(0), None);
    }
}
