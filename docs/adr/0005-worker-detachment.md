# ADR 0005: Worker detachment

## Status

Accepted for the Windows prototype.

## Context

Three of Captastic's operations can block for an unbounded time inside code Captastic does not
own, and none of them can be cancelled:

- **A window render.** `PrintWindow` sends `WM_PRINT` to the target window, so its duration is that
  window's message loop. A hung application does not return, and there is no timeout parameter and
  no cancellation.
- **The capture backend.** Desktop Duplication calls, and the release of the D3D device behind
  them, run inside the display driver.
- **A file write.** The file destination writes into a directory the user chose, which can be a
  network share, a removable disk, or a volume behind a filter driver. A write into one that has
  stopped answering blocks in the kernel, and there is no timeout and no cancellation.

Waiting on either is not an option. A window render is on the path of an interactive chooser, and
the capture backend is released on the path of process exit — where a blocking join would hand
Captastic's shutdown to whatever is stuck, and the visible symptom is a screenshot tool that will
not close.

The file destination arrived with the same shape as the capture worker — one worker, stopped once,
at shutdown — and gained a second shape when the notification area learned to switch it off: a
deadline that can be missed repeatedly, in a daemon that goes on running afterwards. That is the
case the accounting below has to cover, because a worker that was written off may still be writing
into the output directory, and a second worker started there would race it for names.

So Captastic already gave up waiting in the first two places. What it did not have was a policy
for what giving up costs, or any record that it had happened. The window-render count lived in a
private static that was decremented when a detached worker eventually returned, so it could only
report how many were stuck at that instant; a render that detached and finished a second later left nothing
behind. The capture worker counted nothing.

## Decision

**Detachment is deliberate, bounded, and recorded.**

A worker that misses its deadline is left running. Captastic never blocks on it, never kills it —
there is no safe way to terminate a thread inside a driver call or a foreign window procedure — and
never treats its eventual result as a capture.

Every detach is recorded against a process-wide ledger (`captastic-core::DetachLedger`) that keeps
two numbers per kind. `live` is the count still running, and is the pressure on a resource that a
later capture needs. `total` is every detach since the process started, and is what distinguishes a
wedged window from a Captastic bug — the first happens once, the second happens on every capture.

Each kind has a ceiling. The ledger states it; the component that owns the resource enforces it,
because that is the only place it can be enforced.

| kind | ceiling | enforced by |
| --- | --- | --- |
| window render | 8 concurrent | the render worker budget: a detached render holds one of eight slots, and one window may hold only one slot, so a wedged window pins a slot forever but can never take a second |
| capture worker | 1 per run | there is one capture worker, and detaching it is the last thing that happens before the process exits |
| file output worker | 1 concurrent | the daemon's file destination: a detached worker may still be writing into the output directory, so starting file output again is refused — in the notification area, with the reason — until it exits |

`MAX_WINDOW_RENDER_WORKERS` is asserted equal to the documented ceiling at compile time. A
documented ceiling that nothing imposes is worse than no ceiling at all, because it reads like a
guarantee.

Reaching the window-render ceiling is logged at error rather than warn. Below it, a detach costs one
wedged window; at it, every slot is held by a render that never came back and the next window
capture is refused outright with `WorkersExhausted`. Those are different failures and the line read
during an outage should say which one is happening.

**A detach the process outlives is released when the worker returns.** The file worker's thread
gives its slot back on the way out, so a stalled write that eventually completes restores the
daemon rather than costing it file output for the rest of the session. The decision to detach and
the thread's own exit claim the same cell and exactly one of them wins, because a detach recorded
for a thread that had already gone would hold the only slot there is forever. `total` never falls:
a detach that happened is history whether or not the worker came back.

**Nothing is detached that could simply have been waited for.** A recovery back-off doubles to two
seconds, against a capture-worker shutdown budget of 2.5 — so a plain sleep could spend the whole
budget and get the worker detached for no reason except that it was asleep. Waits on the shutdown
path poll the stop flag, and an interrupted back-off abandons the retry instead of building a D3D
device for a daemon that is leaving.

**Once detachment is decided, the resource is leaked on purpose.** The capture worker abandons its
backend rather than dropping it: the destructor is a driver call, it runs after the worker has
reported itself finished but before its thread ends, and the daemon spends shutdown budget watching
for a thread end it cannot influence. The kernel reclaims the device, the duplication and the
allocation at process exit regardless. Running the destructor can only cost time, and a driver slow
enough there gets the worker detached and leaks the same objects anyway — later, and while also
holding a thread.

That argument is exactly as strong as its premise, which is that the process is exiting. It applies
to the shutdown path and nowhere else. A backend dropped during recovery, in a daemon that keeps
running, is dropped normally.

## Consequences

A detached worker's thread, its device context or D3D device, and its share of GPU memory are held
until the process exits — or, for the file worker, until its write returns. That is accepted, and
it is why the ceilings exist: the worst case is eight wedged window renders, one capture backend, and
one file worker, all of which are released by process exit.

A refused file-output start is a message in the notification area rather than a silent no-op, and
the item stays unchecked, because the checkmark states where the next capture will go.

Detachment is visible. Each one is logged with both counts and its ceiling, and the daemon reports
the tally at shutdown — and reports nothing on the runs that detached nothing, which is nearly all
of them.

A ceiling is a real limit and Captastic degrades at it rather than growing a thread backlog:
`WorkersExhausted` is not retryable, because a retry against a ceiling held by wedged workers cannot
succeed and only produces another failure to log.

Detaching is never silent success. The capture that lost its worker returns the timeout that caused
it, and is reported as a failure. Captastic does not substitute a desktop crop, an older frame, or
anything else for the window it could not render.
