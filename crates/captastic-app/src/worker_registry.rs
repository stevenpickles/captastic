//! The daemon's worker registry and shutdown coordinator.
//!
//! The daemon runs four things that have to be stopped together: a capture thread, a selection
//! worker, a clipboard worker, and a hotkey listener. Shutdown can begin from five places — a
//! console signal, the daemon control channel, a tray Exit, a Windows session ending, or the
//! capture worker finishing on its own — and every one of them has to pause input, set the
//! capture stop flag, ask each worker to wind down, and start one shared deadline.
//!
//! Written out at each site, that list was three near-copies plus a fourth variant at teardown.
//! The failure mode is not that any one copy is wrong; it is that adding a fifth worker means
//! finding all four, and a worker missed in one of them shuts down only when the daemon happens to
//! exit through the right branch. Milestone 4 adds that fifth worker.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long the whole shutdown may take before the daemon stops waiting and exits anyway.
pub(crate) const DAEMON_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// Slice of the shutdown budget held back from the capture-worker join for the worker teardown
/// that follows it. The selection worker's UI-state thread performs the final toolbar and region
/// write, so a capture worker that refuses to stop must not be able to spend the whole deadline
/// and take that write down with the process.
pub(crate) const WORKER_TEARDOWN_RESERVE: Duration = Duration::from_millis(500);
const CAPTURE_WORKER_STOP_POLL: Duration = Duration::from_millis(10);

/// Splits a shutdown budget: returns the deadline an earlier join must respect so that `reserve`
/// is left for the teardown that follows it. A budget already shorter than the reserve collapses
/// to `now`, which still lets an already-finished worker be joined without any waiting.
pub(crate) fn reserved_deadline(now: Instant, deadline: Instant, reserve: Duration) -> Instant {
    deadline.checked_sub(reserve).unwrap_or(now).max(now)
}

/// The other half of [`reserved_deadline`]: returns a deadline that is never sooner than
/// `now + minimum`, so a step that must run cannot be handed a budget of zero by everything that
/// ran before it. Reserving alone cannot promise that, because an earlier stage may already have
/// overrun the shared deadline.
pub(crate) fn guaranteed_deadline(now: Instant, deadline: Instant, minimum: Duration) -> Instant {
    deadline.max(now + minimum)
}

/// What a completed teardown has to report back to the daemon.
pub(crate) struct TeardownReport {
    pub clipboard_failures: Vec<crate::clipboard::ClipboardFailure>,
    pub file_output_failures: Vec<crate::file_output::FileOutputFailure>,
    pub persistence_failures: Vec<String>,
    pub hotkey_stop_error: Option<captastic_core::CaptureError>,
    /// Whether the capture worker was ever told to shut down through its command channel, which
    /// distinguishes a requested stop from one the daemon merely observed.
    pub shutdown_sent: bool,
}

/// The capture thread and the two handles used to stop it.
struct CaptureWorker {
    join: Option<JoinHandle<()>>,
    stop_requested: Arc<AtomicBool>,
    commands: SyncSender<crate::daemon::CaptureCommand>,
    shutdown_sent: bool,
}

pub(crate) struct WorkerRegistry {
    capture: CaptureWorker,
    selection: Option<crate::selection::SelectionWorker>,
    clipboard: Option<crate::clipboard::ClipboardWorker>,
    file_output: Option<crate::file_output::FileOutputWorker>,
    hotkey: Option<captastic_windows::HotkeyListener>,
    /// Set while shutting down so late hotkey and tray triggers stop producing work.
    paused: Arc<AtomicBool>,
    /// `Some` once shutdown has begun. Doubles as the "already shutting down" flag, which is what
    /// makes `begin_shutdown` safe to call from every source that can request one.
    deadline: Option<Instant>,
}

impl WorkerRegistry {
    pub(crate) fn new(
        capture_join: JoinHandle<()>,
        capture_stop_requested: Arc<AtomicBool>,
        commands: SyncSender<crate::daemon::CaptureCommand>,
        paused: Arc<AtomicBool>,
    ) -> Self {
        Self {
            capture: CaptureWorker {
                join: Some(capture_join),
                stop_requested: capture_stop_requested,
                commands,
                shutdown_sent: false,
            },
            selection: None,
            clipboard: None,
            file_output: None,
            hotkey: None,
            paused,
            deadline: None,
        }
    }

    pub(crate) fn register_selection(
        &mut self,
        worker: Option<crate::selection::SelectionWorker>,
    ) -> &mut Self {
        self.selection = worker;
        self
    }

    pub(crate) fn register_clipboard(
        &mut self,
        worker: Option<crate::clipboard::ClipboardWorker>,
    ) -> &mut Self {
        self.clipboard = worker;
        self
    }

    pub(crate) fn register_file_output(
        &mut self,
        worker: Option<crate::file_output::FileOutputWorker>,
    ) -> &mut Self {
        self.file_output = worker;
        self
    }

    pub(crate) fn register_hotkey(
        &mut self,
        listener: captastic_windows::HotkeyListener,
    ) -> &mut Self {
        self.hotkey = Some(listener);
        self
    }

    pub(crate) fn selection(&self) -> Option<&crate::selection::SelectionWorker> {
        self.selection.as_ref()
    }

    pub(crate) fn clipboard(&self) -> Option<&crate::clipboard::ClipboardWorker> {
        self.clipboard.as_ref()
    }

    pub(crate) fn file_output(&self) -> Option<&crate::file_output::FileOutputWorker> {
        self.file_output.as_ref()
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.deadline.is_some()
    }

    /// Begins a shutdown, or does nothing if one is already under way.
    ///
    /// Idempotent by design: every source that can request a shutdown calls this, several of them
    /// repeatedly (a tray Exit stays requested until the daemon acts on it), and the first call is
    /// the one that owns the deadline.
    pub(crate) fn begin_shutdown(&mut self, reason: &str) {
        if self.deadline.is_some() {
            return;
        }
        log::info!("{reason}; draining daemon workers");
        self.deadline = Some(Instant::now() + DAEMON_SHUTDOWN_TIMEOUT);
        self.request_stop_all();
    }

    /// Asks every worker to wind down without waiting for any of them.
    fn request_stop_all(&mut self) {
        self.paused.store(true, Ordering::Release);
        self.capture.stop_requested.store(true, Ordering::Release);
        if let Some(worker) = self.selection.as_mut() {
            worker.request_stop();
        }
        if let Some(worker) = self.clipboard.as_mut() {
            worker.request_stop();
        }
        if let Some(worker) = self.file_output.as_mut() {
            worker.request_stop();
        }
        if let Some(hotkey) = self.hotkey.as_mut() {
            if let Err(error) = hotkey.request_stop() {
                crate::logging::warn(format_args!("failed to request hotkey shutdown: {error}"));
            }
        }
    }

    /// True once the shutdown budget is spent and the daemon should stop waiting.
    pub(crate) fn deadline_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Delivers the capture worker's shutdown command, retrying on a later tick if the queue is
    /// full. A disconnected queue counts as delivered: the worker is already gone.
    pub(crate) fn send_capture_shutdown(&mut self) {
        if self.capture.shutdown_sent {
            return;
        }
        match self
            .capture
            .commands
            .try_send(crate::daemon::CaptureCommand::Shutdown)
        {
            Ok(()) | Err(TrySendError::Disconnected(_)) => self.capture.shutdown_sent = true,
            Err(TrySendError::Full(_)) => {}
        }
    }

    /// Stops every worker against one shared deadline and reports what they were still holding.
    ///
    /// The order matters and is the reason this is one function rather than four call sites: the
    /// hotkey listener stops first so no new work arrives, the capture worker joins against a
    /// deadline held back from the full budget, and the selection worker gets the remainder
    /// because it owns the final UI-state write.
    pub(crate) fn teardown(mut self) -> TeardownReport {
        let deadline = self
            .deadline
            .unwrap_or_else(|| Instant::now() + DAEMON_SHUTDOWN_TIMEOUT);
        // Repeated deliberately: teardown can be reached without a shutdown ever being requested,
        // when the capture worker finishes on its own.
        self.request_stop_all();
        let hotkey_stop_error = self
            .hotkey
            .take()
            .and_then(|hotkey| hotkey.stop_before(deadline).err());
        let _ = self
            .capture
            .commands
            .try_send(crate::daemon::CaptureCommand::Shutdown);
        if let Some(join) = self.capture.join.take() {
            join_capture_worker_until(
                join,
                reserved_deadline(Instant::now(), deadline, WORKER_TEARDOWN_RESERVE),
            );
        }
        let persistence_failures = self
            .selection
            .take()
            .map_or_else(Vec::new, |worker| worker.stop_before(deadline));
        let clipboard_failures = self
            .clipboard
            .take()
            .map_or_else(Vec::new, |worker| worker.stop_before(deadline));
        // Last, and against the full deadline: a capture that has been encoded but not yet
        // written is the one piece of work whose loss the user would actually see.
        let file_output_failures = self
            .file_output
            .take()
            .map_or_else(Vec::new, |worker| worker.stop_before(deadline));
        TeardownReport {
            clipboard_failures,
            file_output_failures,
            persistence_failures,
            hotkey_stop_error,
            shutdown_sent: self.capture.shutdown_sent,
        }
    }
}

fn join_capture_worker_until(join: JoinHandle<()>, deadline: Instant) {
    while !join.is_finished() && Instant::now() < deadline {
        thread::sleep(CAPTURE_WORKER_STOP_POLL);
    }
    if join.is_finished() {
        let _ = join.join();
        return;
    }
    // Left running on purpose. The capture worker can be blocked inside a foreign window
    // procedure, and a blocking join would hand the process's exit over to whatever is wedged.
    crate::logging::warn(format_args!(
        "capture worker did not stop within its shutdown budget; detaching it"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slow_capture_join_cannot_spend_the_whole_shutdown_budget() {
        let now = Instant::now();
        let teardown_deadline = now + DAEMON_SHUTDOWN_TIMEOUT;

        let capture_deadline = reserved_deadline(now, teardown_deadline, WORKER_TEARDOWN_RESERVE);

        // The capture join stops early enough that the selection worker - and behind it the
        // UI-state flush - still has the reserve to work with.
        assert_eq!(
            capture_deadline,
            teardown_deadline - WORKER_TEARDOWN_RESERVE
        );
        assert_eq!(
            teardown_deadline.saturating_duration_since(capture_deadline),
            WORKER_TEARDOWN_RESERVE
        );
        assert!(WORKER_TEARDOWN_RESERVE < DAEMON_SHUTDOWN_TIMEOUT);
    }

    #[test]
    fn an_exhausted_shutdown_budget_reserves_nothing_and_waits_for_nothing() {
        let base = Instant::now();
        // A deadline that has already passed, expressed without subtracting from a real Instant.
        let now = base + Duration::from_secs(10);

        // Neither an expired deadline nor a budget shorter than the reserve may produce a deadline
        // in the future or in the past: both collapse to now, which still joins finished workers.
        assert_eq!(reserved_deadline(now, base, WORKER_TEARDOWN_RESERVE), now);
        assert_eq!(
            reserved_deadline(
                now,
                now + WORKER_TEARDOWN_RESERVE / 2,
                WORKER_TEARDOWN_RESERVE
            ),
            now
        );
        assert_eq!(
            reserved_deadline(now, now + WORKER_TEARDOWN_RESERVE, WORKER_TEARDOWN_RESERVE),
            now
        );
    }

    #[test]
    fn beginning_a_shutdown_twice_keeps_the_first_deadline() {
        // Every shutdown source calls this, and several call it repeatedly: a tray Exit stays
        // requested until the daemon acts on it. A later call must not extend the budget, or a
        // repeating source could hold the daemon open indefinitely.
        let (commands, _receiver) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let mut registry =
            WorkerRegistry::new(thread::spawn(|| {}), stop.clone(), commands, paused.clone());

        assert!(!registry.is_shutting_down());
        registry.begin_shutdown("first reason");
        let first = registry.deadline.expect("a shutdown sets its deadline");
        assert!(registry.is_shutting_down());
        assert!(stop.load(Ordering::Acquire), "capture must be told to stop");
        assert!(paused.load(Ordering::Acquire), "input must be paused");

        registry.begin_shutdown("second reason");
        assert_eq!(
            registry.deadline,
            Some(first),
            "a repeated request must not extend the budget"
        );
    }

    #[test]
    fn the_capture_shutdown_command_is_sent_once_and_survives_a_full_queue() {
        let (commands, receiver) = std::sync::mpsc::sync_channel(1);
        let mut registry = WorkerRegistry::new(
            thread::spawn(|| {}),
            Arc::new(AtomicBool::new(false)),
            commands,
            Arc::new(AtomicBool::new(false)),
        );

        // Fill the queue: the command cannot be delivered yet, and must be retried rather than
        // dropped, or a busy daemon would never be told to stop.
        registry
            .capture
            .commands
            .try_send(crate::daemon::CaptureCommand::Shutdown)
            .expect("seed the queue");
        registry.send_capture_shutdown();
        assert!(
            !registry.capture.shutdown_sent,
            "a full queue must leave the command outstanding"
        );

        let _ = receiver.recv().expect("drain the seeded command");
        registry.send_capture_shutdown();
        assert!(registry.capture.shutdown_sent);

        // A second call is a no-op rather than a second command.
        registry.send_capture_shutdown();
        assert!(
            receiver.try_recv().is_ok(),
            "exactly one command was queued"
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn a_disconnected_capture_queue_counts_as_delivered() {
        // The worker is already gone, so there is nobody left to tell; retrying forever would
        // keep the daemon in its shutdown loop until the deadline expired.
        let (commands, receiver) = std::sync::mpsc::sync_channel(1);
        drop(receiver);
        let mut registry = WorkerRegistry::new(
            thread::spawn(|| {}),
            Arc::new(AtomicBool::new(false)),
            commands,
            Arc::new(AtomicBool::new(false)),
        );

        registry.send_capture_shutdown();
        assert!(registry.capture.shutdown_sent);
    }

    #[test]
    fn teardown_stops_every_worker_even_without_a_requested_shutdown() {
        // Reached when the capture worker finishes on its own: nothing called `begin_shutdown`,
        // so teardown has to issue the stop requests itself.
        let (commands, _receiver) = std::sync::mpsc::sync_channel(4);
        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(false));
        let registry =
            WorkerRegistry::new(thread::spawn(|| {}), stop.clone(), commands, paused.clone());

        let report = registry.teardown();

        assert!(stop.load(Ordering::Acquire));
        assert!(paused.load(Ordering::Acquire));
        assert!(report.clipboard_failures.is_empty());
        assert!(report.persistence_failures.is_empty());
        assert!(report.hotkey_stop_error.is_none());
        assert!(!report.shutdown_sent);
    }

    #[test]
    fn teardown_joins_a_worker_that_has_already_finished() {
        let (commands, _receiver) = std::sync::mpsc::sync_channel(4);
        let (started_sender, started) = std::sync::mpsc::channel();
        let join = thread::spawn(move || {
            let _ = started_sender.send(());
        });
        started.recv().expect("worker ran");

        let mut registry = WorkerRegistry::new(
            join,
            Arc::new(AtomicBool::new(false)),
            commands,
            Arc::new(AtomicBool::new(false)),
        );
        registry.begin_shutdown("test");
        let started_at = Instant::now();
        let _ = registry.teardown();
        assert!(
            started_at.elapsed() < DAEMON_SHUTDOWN_TIMEOUT,
            "a finished worker must not be waited on"
        );
    }
}
