use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use captastic_core::{CaptureError, CaptureErrorKind};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
    VK_F9,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_HOTKEY, WM_QUIT,
};

const CAPTASTIC_HOTKEY_ID: i32 = 0x4341;
const THREAD_STOP_TIMEOUT: Duration = Duration::from_secs(1);
const THREAD_STOP_POLL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HotkeySpec {
    modifiers: HOT_KEY_MODIFIERS,
    virtual_key: u32,
    label: &'static str,
}

impl HotkeySpec {
    pub fn ctrl_shift_f9() -> Self {
        Self {
            modifiers: MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT,
            virtual_key: u32::from(VK_F9.0),
            label: "Ctrl+Shift+F9",
        }
    }

    pub fn label(self) -> &'static str {
        self.label
    }
}

pub struct HotkeyListener {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl HotkeyListener {
    pub fn start<F>(spec: HotkeySpec, mut on_hotkey: F) -> Result<Self, CaptureError>
    where
        F: FnMut(Instant) + Send + 'static,
    {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("captastic-hotkey".to_owned())
            .spawn(move || {
                // SAFETY: This call has no preconditions and returns the current OS thread ID.
                let thread_id = unsafe { GetCurrentThreadId() };
                let mut message = MSG::default();
                // SAFETY: A zero-range, no-remove peek initializes this thread's message queue.
                let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
                // SAFETY: The hotkey is registered to this thread's message queue with a process-
                // local ID. It is unregistered before the thread exits.
                let registration = unsafe {
                    RegisterHotKey(None, CAPTASTIC_HOTKEY_ID, spec.modifiers, spec.virtual_key)
                };
                if let Err(error) = registration {
                    let _ = ready_sender.send(Err(hotkey_error("register_hotkey", error)));
                    return;
                }
                if ready_sender.send(Ok(thread_id)).is_err() {
                    // SAFETY: Balances this thread's successful registration.
                    let _ = unsafe { UnregisterHotKey(None, CAPTASTIC_HOTKEY_ID) };
                    return;
                }

                loop {
                    // SAFETY: message is valid writable storage. This thread owns its message loop.
                    let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
                    if result.0 == -1 {
                        break;
                    }
                    if result.0 == 0 || message.message == WM_QUIT {
                        break;
                    }
                    if message.message == WM_HOTKEY
                        && message.wParam.0 == CAPTASTIC_HOTKEY_ID as usize
                    {
                        let received_at = Instant::now();
                        let _ = catch_unwind(AssertUnwindSafe(|| on_hotkey(received_at)));
                    }
                }

                // SAFETY: Balances this thread's successful registration.
                let _ = unsafe { UnregisterHotKey(None, CAPTASTIC_HOTKEY_ID) };
            })
            .map_err(|error| CaptureError {
                kind: CaptureErrorKind::NativeFailure,
                backend: "windows-hotkey",
                operation: "spawn_hotkey_thread",
                message: error.to_string(),
                retryable: false,
                native_code: None,
            })?;
        let thread_id = ready_receiver.recv().map_err(|error| CaptureError {
            kind: CaptureErrorKind::NativeFailure,
            backend: "windows-hotkey",
            operation: "start_hotkey_thread",
            message: error.to_string(),
            retryable: false,
            native_code: None,
        })??;
        Ok(Self {
            thread_id,
            join: Some(join),
        })
    }

    pub fn stop(mut self) -> Result<(), CaptureError> {
        self.request_stop()?;
        self.join_thread();
        Ok(())
    }

    fn request_stop(&self) -> Result<(), CaptureError> {
        // SAFETY: thread_id identifies the live message-loop thread, whose queue is initialized.
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
            .map_err(|error| hotkey_error("stop_hotkey_thread", error))
    }

    fn join_thread(&mut self) {
        if let Some(join) = self.join.take() {
            let started = Instant::now();
            while !join.is_finished() && started.elapsed() < THREAD_STOP_TIMEOUT {
                thread::sleep(THREAD_STOP_POLL);
            }
            if join.is_finished() {
                let _ = join.join();
            } else {
                log::error!(
                    "hotkey worker did not stop within {} ms; detaching it so shutdown can continue",
                    THREAD_STOP_TIMEOUT.as_millis()
                );
            }
        }
    }
}

impl Drop for HotkeyListener {
    fn drop(&mut self) {
        let _ = self.request_stop();
        self.join_thread();
    }
}

fn hotkey_error(operation: &'static str, error: windows::core::Error) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-hotkey",
        operation,
        message: error.to_string(),
        retryable: false,
        native_code: Some(i64::from(error.code().0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_binding_is_stable() {
        assert_eq!(HotkeySpec::ctrl_shift_f9().label(), "Ctrl+Shift+F9");
    }
}
