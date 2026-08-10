use captastic_core::{CaptureError, CaptureErrorKind};
use windows::core::{w, Error as WindowsError};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
};

const CONTROL_EVENT_NAME: windows::core::PCWSTR = w!("Local\\CaptasticDaemonControl-v1");

pub struct DaemonControl {
    event: HANDLE,
}

impl DaemonControl {
    pub fn create() -> Result<Self, CaptureError> {
        // SAFETY: Default same-user security is requested, and the static name has process lifetime.
        let event = unsafe { CreateEventW(None, true, false, CONTROL_EVENT_NAME) }
            .map_err(|error| control_error("create_daemon_control", error, false))?;
        // SAFETY: This reads the calling thread's last-error value immediately after CreateEventW.
        if unsafe { GetLastError() }.is_err() {
            // SAFETY: event is the valid handle returned for the existing named event.
            let _ = unsafe { CloseHandle(event) };
            return Err(CaptureError {
                kind: CaptureErrorKind::SourceUnavailable,
                backend: "windows-daemon-control",
                operation: "create_daemon_control",
                message: "another Captastic daemon is already running in this session".to_owned(),
                retryable: false,
                native_code: Some(i64::from(ERROR_ALREADY_EXISTS.0)),
            });
        }
        Ok(Self { event })
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
    }
}

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
