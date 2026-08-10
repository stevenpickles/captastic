use std::sync::atomic::{AtomicBool, Ordering};

use captastic_core::{CaptureError, CaptureErrorKind};
use windows::Win32::Foundation::BOOL;
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT, PHANDLER_ROUTINE,
};

static INSTALLED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

pub struct ConsoleShutdown;

impl ConsoleShutdown {
    pub fn install() -> Result<Self, CaptureError> {
        if INSTALLED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(console_error(
                "install_console_handler",
                "a console shutdown handler is already installed",
                None,
            ));
        }
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        // SAFETY: console_control_handler has the required system ABI, contains only atomic
        // operations, and remains valid for the process lifetime. Drop unregisters this instance.
        let registration =
            unsafe { SetConsoleCtrlHandler(PHANDLER_ROUTINE::Some(console_control_handler), true) };
        if let Err(error) = registration {
            INSTALLED.store(false, Ordering::Release);
            return Err(console_error(
                "install_console_handler",
                error.to_string(),
                Some(i64::from(error.code().0)),
            ));
        }
        Ok(Self)
    }

    pub fn requested(&self) -> bool {
        SHUTDOWN_REQUESTED.load(Ordering::Acquire)
    }
}

impl Drop for ConsoleShutdown {
    fn drop(&mut self) {
        // SAFETY: This removes the exact static callback successfully installed by this guard.
        let _ = unsafe {
            SetConsoleCtrlHandler(PHANDLER_ROUTINE::Some(console_control_handler), false)
        };
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        INSTALLED.store(false, Ordering::Release);
    }
}

unsafe extern "system" fn console_control_handler(control_type: u32) -> BOOL {
    match control_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            SHUTDOWN_REQUESTED.store(true, Ordering::Release);
            BOOL(1)
        }
        _ => BOOL(0),
    }
}

fn console_error(
    operation: &'static str,
    message: impl Into<String>,
    native_code: Option<i64>,
) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-console",
        operation,
        message: message.into(),
        retryable: false,
        native_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_console_events_are_not_claimed() {
        // SAFETY: The callback accepts any u32 console-event value and only touches an atomic.
        assert_eq!(unsafe { console_control_handler(u32::MAX) }, BOOL(0));
    }

    #[test]
    fn ctrl_c_requests_shutdown() {
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        // SAFETY: CTRL_C_EVENT is a documented console-event value and the callback only touches
        // an atomic flag.
        assert_eq!(unsafe { console_control_handler(CTRL_C_EVENT) }, BOOL(1));
        assert!(SHUTDOWN_REQUESTED.load(Ordering::Acquire));
    }
}
