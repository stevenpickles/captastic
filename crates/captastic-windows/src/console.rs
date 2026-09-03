use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use captastic_core::{CaptureError, CaptureErrorKind};
use windows::Win32::Foundation::BOOL;
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT, PHANDLER_ROUTINE,
};

static INSTALLED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static DRAIN_COMPLETED: AtomicBool = AtomicBool::new(false);
const CONSOLE_DRAIN_TIMEOUT: Duration = Duration::from_secs(4);
const CONSOLE_DRAIN_POLL: Duration = Duration::from_millis(10);

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
        DRAIN_COMPLETED.store(false, Ordering::Release);
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

    pub fn signal_drained(&self) {
        DRAIN_COMPLETED.store(true, Ordering::Release);
    }
}

impl Drop for ConsoleShutdown {
    fn drop(&mut self) {
        // SAFETY: This removes the exact static callback successfully installed by this guard.
        let _ = unsafe {
            SetConsoleCtrlHandler(PHANDLER_ROUTINE::Some(console_control_handler), false)
        };
        // SHUTDOWN_REQUESTED and DRAIN_COMPLETED are intentionally left latched here rather than
        // cleared. SetConsoleCtrlHandler(..., false) only removes the callback from the process's
        // handler list; it does not wait for an already-running invocation to return. A handler
        // invoked just before this drop can still be polling DRAIN_COMPLETED on its own OS thread,
        // and it must observe a true value to exit promptly instead of spinning for the full
        // CONSOLE_DRAIN_TIMEOUT. Clearing SHUTDOWN_REQUESTED here would also erase a CTRL event
        // that landed between the drain finishing and this drop running. install() resets both
        // flags for the next guard, so re-installation still starts from a clean slate.
        INSTALLED.store(false, Ordering::Release);
    }
}

unsafe extern "system" fn console_control_handler(control_type: u32) -> BOOL {
    match control_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
        | CTRL_SHUTDOWN_EVENT => {
            SHUTDOWN_REQUESTED.store(true, Ordering::Release);
            if matches!(
                control_type,
                CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
            ) {
                let started = Instant::now();
                while !DRAIN_COMPLETED.load(Ordering::Acquire)
                    && started.elapsed() < CONSOLE_DRAIN_TIMEOUT
                {
                    thread::sleep(CONSOLE_DRAIN_POLL);
                }
            }
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
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    // These tests read and write the process-global INSTALLED/SHUTDOWN_REQUESTED/DRAIN_COMPLETED
    // statics that the real console control handler also uses, so the default parallel test
    // runner can interleave them: a racing write can produce a spurious assertion failure, or
    // worse, make `terminal_console_events_are_claimed_and_wait_for_drain_signal` spin for the
    // full multi-second CONSOLE_DRAIN_TIMEOUT. Serialize every test in this module behind one
    // mutex instead of adding a test-only dependency.
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn lock_console_state() -> MutexGuard<'static, ()> {
        TEST_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn unrelated_console_events_are_not_claimed() {
        let _serialize = lock_console_state();
        // SAFETY: The callback accepts any u32 console-event value and only touches an atomic.
        assert_eq!(unsafe { console_control_handler(u32::MAX) }, BOOL(0));
    }

    #[test]
    fn ctrl_c_requests_shutdown() {
        let _serialize = lock_console_state();
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        // SAFETY: CTRL_C_EVENT is a documented console-event value and the callback only touches
        // an atomic flag.
        assert_eq!(unsafe { console_control_handler(CTRL_C_EVENT) }, BOOL(1));
        assert!(SHUTDOWN_REQUESTED.load(Ordering::Acquire));
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
    }

    #[test]
    fn terminal_console_events_are_claimed_and_wait_for_drain_signal() {
        let _serialize = lock_console_state();
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        DRAIN_COMPLETED.store(true, Ordering::Release);
        assert_eq!(
            // SAFETY: CTRL_LOGOFF_EVENT is documented and the pre-set signal avoids blocking.
            unsafe { console_control_handler(CTRL_LOGOFF_EVENT) },
            BOOL(1)
        );
        assert!(SHUTDOWN_REQUESTED.load(Ordering::Acquire));
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        DRAIN_COMPLETED.store(false, Ordering::Release);
    }

    #[test]
    fn drop_leaves_shutdown_flags_latched_for_an_in_flight_handler() {
        let _serialize = lock_console_state();
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        DRAIN_COMPLETED.store(false, Ordering::Release);

        let shutdown = ConsoleShutdown::install().expect("install console handler");
        // Simulate a CTRL event landing and the daemon completing its drain just before the
        // guard is dropped, mirroring a handler thread that is still mid-poll on DRAIN_COMPLETED.
        SHUTDOWN_REQUESTED.store(true, Ordering::Release);
        shutdown.signal_drained();
        drop(shutdown);

        // A concurrently polling handler must still observe both flags as true after this drop;
        // neither may be wiped out from under it.
        assert!(SHUTDOWN_REQUESTED.load(Ordering::Acquire));
        assert!(DRAIN_COMPLETED.load(Ordering::Acquire));
        assert!(!INSTALLED.load(Ordering::Acquire));

        // install → drop → install must still behave: re-installing resets both flags for the
        // next guard's lifetime.
        let shutdown = ConsoleShutdown::install().expect("reinstall console handler");
        assert!(!SHUTDOWN_REQUESTED.load(Ordering::Acquire));
        assert!(!DRAIN_COMPLETED.load(Ordering::Acquire));
        drop(shutdown);

        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
        DRAIN_COMPLETED.store(false, Ordering::Release);
    }
}
