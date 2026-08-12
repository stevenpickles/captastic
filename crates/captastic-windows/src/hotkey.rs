use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use captastic_config::{HotkeyAction, HotkeyBinding, HotkeyChord, HotkeyKey};
use captastic_core::{CaptureError, CaptureErrorKind};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_HOTKEY, WM_QUIT,
};

const CAPTASTIC_HOTKEY_ID: i32 = 0x4341;
const THREAD_STOP_TIMEOUT: Duration = Duration::from_secs(1);
const THREAD_STOP_POLL: Duration = Duration::from_millis(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HotkeySpec {
    binding: HotkeyBinding,
    modifiers: HOT_KEY_MODIFIERS,
    virtual_key: u32,
    id: i32,
}

impl HotkeySpec {
    pub fn from_binding(binding: HotkeyBinding) -> Self {
        let configured = binding.chord.modifiers();
        let mut modifiers = MOD_NOREPEAT;
        if configured.ctrl() {
            modifiers |= MOD_CONTROL;
        }
        if configured.alt() {
            modifiers |= MOD_ALT;
        }
        if configured.shift() {
            modifiers |= MOD_SHIFT;
        }
        if configured.win() {
            modifiers |= MOD_WIN;
        }
        let virtual_key = match binding.chord.key() {
            HotkeyKey::Letter(value) | HotkeyKey::Digit(value) => u32::from(value),
            HotkeyKey::Function(number) => 0x70 + u32::from(number - 1),
        };
        Self {
            binding,
            modifiers,
            virtual_key,
            id: CAPTASTIC_HOTKEY_ID + binding.action.registration_index(),
        }
    }

    pub fn action(self) -> HotkeyAction {
        self.binding.action
    }

    pub fn chord(self) -> HotkeyChord {
        self.binding.chord
    }

    pub fn label(self) -> String {
        self.binding.chord.to_string()
    }
}

pub struct HotkeyListener {
    thread_id: u32,
    stop_requested: bool,
    join: Option<JoinHandle<()>>,
}

impl HotkeyListener {
    pub fn start<F>(bindings: &[HotkeyBinding], mut on_hotkey: F) -> Result<Self, CaptureError>
    where
        F: FnMut(HotkeyAction, HotkeyChord, Instant) + Send + 'static,
    {
        let specs = bindings
            .iter()
            .copied()
            .map(HotkeySpec::from_binding)
            .collect::<Vec<_>>();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("captastic-hotkey".to_owned())
            .spawn(move || {
                // SAFETY: This call has no preconditions and returns the current OS thread ID.
                let thread_id = unsafe { GetCurrentThreadId() };
                let mut message = MSG::default();
                // SAFETY: A zero-range, no-remove peek initializes this thread's message queue.
                let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
                let mut registry = NativeRegistry;
                let registered = match register_all(&specs, &mut registry) {
                    Ok(registered) => registered,
                    Err(failure) => {
                        let _ = ready_sender.send(Err(registration_error(failure)));
                        return;
                    }
                };
                if ready_sender.send(Ok(thread_id)).is_err() {
                    unregister_all(&registered, &mut registry);
                    return;
                }

                loop {
                    // SAFETY: message is valid writable storage. This thread owns its message loop.
                    let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
                    if result.0 == -1 || result.0 == 0 || message.message == WM_QUIT {
                        break;
                    }
                    if message.message == WM_HOTKEY {
                        let received_at = Instant::now();
                        if let Some(spec) = registered
                            .iter()
                            .find(|spec| message.wParam.0 == spec.id as usize)
                        {
                            let action = spec.action();
                            let chord = spec.chord();
                            let _ = catch_unwind(AssertUnwindSafe(|| {
                                on_hotkey(action, chord, received_at);
                            }));
                        }
                    }
                }

                unregister_all(&registered, &mut registry);
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
            stop_requested: false,
            join: Some(join),
        })
    }

    pub fn stop(self) -> Result<(), CaptureError> {
        self.stop_before(Instant::now() + THREAD_STOP_TIMEOUT)
    }

    pub fn stop_before(mut self, deadline: Instant) -> Result<(), CaptureError> {
        let stop_result = self.request_stop();
        self.join_thread_until(deadline);
        stop_result
    }

    pub fn request_stop(&mut self) -> Result<(), CaptureError> {
        if self.stop_requested {
            return Ok(());
        }
        // SAFETY: thread_id identifies the live message-loop thread, whose queue is initialized.
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
            .map_err(|error| hotkey_error("stop_hotkey_thread", error))?;
        self.stop_requested = true;
        Ok(())
    }

    fn join_thread_until(&mut self, deadline: Instant) {
        if let Some(join) = self.join.take() {
            if !join_hotkey_worker_until(join, deadline) {
                log::error!(
                    "hotkey worker did not stop before its shutdown deadline; detaching it so shutdown can continue"
                );
            }
        }
    }
}

impl Drop for HotkeyListener {
    fn drop(&mut self) {
        let _ = self.request_stop();
        self.join_thread_until(Instant::now() + THREAD_STOP_TIMEOUT);
    }
}

fn join_hotkey_worker_until(join: JoinHandle<()>, deadline: Instant) -> bool {
    while !join.is_finished() && Instant::now() < deadline {
        thread::sleep(THREAD_STOP_POLL);
    }
    if join.is_finished() {
        let _ = join.join();
        true
    } else {
        false
    }
}

trait Registry {
    type Error;

    fn register(&mut self, spec: HotkeySpec) -> Result<(), Self::Error>;
    fn unregister(&mut self, spec: HotkeySpec);
}

struct NativeRegistry;

impl Registry for NativeRegistry {
    type Error = windows::core::Error;

    fn register(&mut self, spec: HotkeySpec) -> Result<(), Self::Error> {
        // SAFETY: The hotkey is registered to this thread's message queue with a process-local ID.
        unsafe { RegisterHotKey(None, spec.id, spec.modifiers, spec.virtual_key) }
    }

    fn unregister(&mut self, spec: HotkeySpec) {
        // SAFETY: Balances a successful registration made by this registry on the same thread.
        let _ = unsafe { UnregisterHotKey(None, spec.id) };
    }
}

#[derive(Debug)]
struct RegistrationFailure<E> {
    spec: HotkeySpec,
    error: E,
}

fn register_all<R: Registry>(
    specs: &[HotkeySpec],
    registry: &mut R,
) -> Result<Vec<HotkeySpec>, RegistrationFailure<R::Error>> {
    let mut registered = Vec::with_capacity(specs.len());
    for spec in specs.iter().copied() {
        if let Err(error) = registry.register(spec) {
            unregister_all(&registered, registry);
            return Err(RegistrationFailure { spec, error });
        }
        registered.push(spec);
    }
    Ok(registered)
}

fn unregister_all<R: Registry>(registered: &[HotkeySpec], registry: &mut R) {
    for spec in registered.iter().rev().copied() {
        registry.unregister(spec);
    }
}

fn registration_error(failure: RegistrationFailure<windows::core::Error>) -> CaptureError {
    let action = failure.spec.action();
    let chord = failure.spec.label();
    let error = failure.error;
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-hotkey",
        operation: "register_hotkey",
        message: format!("failed to register action {action} with chord {chord}: {error}"),
        retryable: false,
        native_code: Some(i64::from(error.code().0)),
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
    use std::collections::BTreeSet;

    fn binding(action: HotkeyAction, chord: &str) -> HotkeyBinding {
        HotkeyBinding {
            action,
            chord: chord.parse().expect("test chord"),
        }
    }

    #[derive(Default)]
    struct FakeRegistry {
        fail_id: Option<i32>,
        registered: Vec<i32>,
        unregistered: Vec<i32>,
    }

    impl Registry for FakeRegistry {
        type Error = &'static str;

        fn register(&mut self, spec: HotkeySpec) -> Result<(), Self::Error> {
            if self.fail_id == Some(spec.id) {
                return Err("conflict");
            }
            self.registered.push(spec.id);
            Ok(())
        }

        fn unregister(&mut self, spec: HotkeySpec) {
            self.unregistered.push(spec.id);
        }
    }

    #[test]
    fn stable_unique_ids_map_to_actions() {
        let specs = HotkeyAction::ALL
            .iter()
            .enumerate()
            .map(|(index, action)| {
                HotkeySpec::from_binding(binding(*action, &format!("Ctrl+F{}", index + 1)))
            })
            .collect::<Vec<_>>();
        let ids = specs.iter().map(|spec| spec.id).collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), specs.len());
        for spec in specs {
            assert_eq!(
                spec.id,
                CAPTASTIC_HOTKEY_ID + spec.action().registration_index()
            );
        }
    }

    #[test]
    fn partial_registration_failure_rolls_back_and_reports_binding() {
        let specs = [
            HotkeySpec::from_binding(binding(HotkeyAction::LastWorkflow, "Ctrl+F9")),
            HotkeySpec::from_binding(binding(HotkeyAction::Region, "Ctrl+F10")),
        ];
        let mut registry = FakeRegistry {
            fail_id: Some(specs[1].id),
            ..FakeRegistry::default()
        };
        let failure = register_all(&specs, &mut registry).expect_err("second binding conflicts");
        assert_eq!(failure.spec.action(), HotkeyAction::Region);
        assert_eq!(failure.spec.label(), "Ctrl+F10");
        assert_eq!(registry.registered, vec![specs[0].id]);
        assert_eq!(registry.unregistered, vec![specs[0].id]);
    }

    #[test]
    fn shutdown_unregisters_every_binding_in_reverse_order() {
        let specs = [
            HotkeySpec::from_binding(binding(HotkeyAction::LastWorkflow, "Ctrl+F9")),
            HotkeySpec::from_binding(binding(HotkeyAction::FullDisplay, "Alt+F10")),
        ];
        let mut registry = FakeRegistry::default();
        let registered = register_all(&specs, &mut registry).expect("registrations succeed");
        unregister_all(&registered, &mut registry);
        assert_eq!(registry.unregistered, vec![specs[1].id, specs[0].id]);
    }

    #[test]
    fn hotkey_worker_join_obeys_the_shared_deadline() {
        let (release_sender, release_receiver) = mpsc::channel();
        let join = thread::spawn(move || {
            let _ = release_receiver.recv();
        });
        let started = Instant::now();

        assert!(!join_hotkey_worker_until(
            join,
            Instant::now() + Duration::from_millis(10)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        release_sender
            .send(())
            .expect("release detached hotkey test worker");
    }

    #[test]
    fn every_binding_suppresses_os_key_repeat() {
        let spec = HotkeySpec::from_binding(binding(HotkeyAction::Window, "Win+Shift+W"));
        assert_ne!(spec.modifiers & MOD_NOREPEAT, HOT_KEY_MODIFIERS(0));
    }
    #[test]
    fn registration_conflict_names_the_action_and_canonical_chord() {
        let spec = HotkeySpec::from_binding(binding(HotkeyAction::RepeatLastRegion, "shift+win+r"));
        let error = registration_error(RegistrationFailure {
            spec,
            error: windows::core::Error::from_win32(),
        });
        assert!(error.message.contains("repeat_last_region"));
        assert!(error.message.contains("Shift+Win+R"));
    }
}
