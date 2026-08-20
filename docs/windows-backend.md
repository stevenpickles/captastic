# Windows DXGI backend

## Current scope

`captastic-windows` initializes COM, enumerates attached DXGI outputs, creates a D3D11 device on the selected output's adapter, and retains an `IDXGIOutputDuplication` session for the backend lifetime.

The current capture path supports:

- pointer, primary, configured-display, and same-adapter virtual-desktop display policies, with the
  display manager retaining a separate warm backend per usable output;
- normalization of 0/90/180/270-degree display rotations into the top-left BGRA frame contract,
  including rotated GPU-region mapping;
- finite `AcquireNextFrame` waits;
- OS presentation timestamps converted from QueryPerformanceCounter units;
- rejection of frames presented before the trigger in `fresh` mode;
- trigger-time acquisition into a retained GPU texture with no idle DXGI polling;
- frame-age reporting for retained frames;
- texture-type and non-empty-dimension validation;
- exactly-once `ReleaseFrame` through an RAII guard;
- typed timeout, access-loss, and device-loss errors with drop-before-replace recovery and bounded same-capture retries;
- native-frame latency records;
- a preallocated D3D11 staging texture;
- a three-slot preallocated CPU buffer pool;
- stride-aware top-left BGRA8 readback;
- separate native-frame, CPU-frame, and readback latency summaries.
- asynchronous CF_DIBV5 plus registered PNG clipboard publication with top-down BGRA/RGBA layout;
- bounded clipboard-open retries and explicit native allocation ownership transfer.
- optional cursor composition, off by default, described below.

The current path deliberately rejects:

- multi-adapter virtual-desktop topologies, which return a structured unsupported error.

## Cursor composition

`cursor = "include"` blends the pointer into the CPU frame; `exclude`, the default, leaves the frame
as the desktop image DXGI hands over. A composited capture records where the pointer was drawn, and
one that draws nothing records why, so a caller can tell an absent pointer from an unasked question.

DXGI describes the pointer **incrementally and per acquisition**. `AcquireNextFrame` reports position
and shape only on a frame whose `LastMouseUpdateTime` is non-zero, and leaves those fields at their
defaults on every other frame — defaults indistinguishable from an invisible pointer at the origin.
Two consequences drive the implementation: the pointer state must be cached across acquisitions
rather than read from the current frame, and it must be recorded the moment a frame is acquired,
before any decision to keep or discard that frame, because a discarded frame takes its only copy of
the report with it. A pointer resting still over a repainting desktop is otherwise reported absent
indefinitely.

Shapes arrive as colour BGRA, monochrome double-height AND/XOR masks, or masked colour, and are
cached until DXGI replaces them. `PointerPosition` already reports the top-left of the shape rather
than the hotspot, so nothing is subtracted when positioning — the hotspot matters to whoever asks
where the user is pointing, not to where the bitmap goes. The pointer is composited before any crop,
so a pointer straddling a selection edge is clipped by that crop exactly as it is clipped by the edge
of the screen, leaving one bounds test rather than two that could disagree.

Rotation needs no transform, which is measured rather than assumed. Captured pixels are
rotation-normalized — a 270° panel's 3840×2160 surface becomes a 2160×3840 upright frame — so the
obvious expectation is that the pointer needs the same mapping. It does not. `PointerPosition` is
reported in upright desktop coordinates relative to the output's own origin, which is already the
normalized frame's coordinate space, and `GetFramePointerShape` hands back the logical cursor
bitmap the user sees rather than one turned to match the panel. Both were checked on a display
driven through 0°, 90°, 180° and 270°: against `GetCursorPos`, which is upright by definition, the
reported position was exact at every orientation across six spread-out sample points, while the
transposed reading would have been out by up to ~3,500 px; the delivered shape was the same upright
arrow every time. Composition therefore draws the shape at the reported position on a rotated
display exactly as it does on an upright one. Both wrong answers — a transposed position, a shape
turned to match the panel — produce a screenshot that looks plausible, so
`cursor_composition_on_a_rotated_display_is_upright_and_in_place` checks the position against
`GetCursorPos` and the shape against GDI's rendering of the same cursor, which has no orientation
of its own.

## Resident hotkey path

`captastic daemon` creates the DXGI backend on a dedicated capture thread, then blocks on its command channel while idle. A second Windows thread owns `RegisterHotKey` and a Win32 message loop. The `WM_HOTKEY` branch timestamps immediately, constructs a fixed trigger record, attempts a nonblocking send into a four-entry queue, and returns. For `latest`, the capture thread performs one nonblocking Desktop Duplication drain at trigger time and reuses the retained image when the desktop has not changed; only the first capture may wait up to 100 ms for an initial frame. Capture and CPU readback occur on the capture thread; native selection and clipboard publication run on their own serialized workers after CPU readiness.

The default binding is `Ctrl+Shift+F9` for `last_workflow`; additional Region, Window, direct full-display, and repeat-confirmed-region actions are optional. Every active chord uses a stable action-derived ID plus `MOD_NOREPEAT`. The listener registers the complete set before readiness, rolls back earlier registrations on any conflict, maps `WM_HOTKEY` directly to a fixed-size action, and unregisters every successful chord during bounded shutdown. Queue-full events are counted without blocking the message loop. The foreground prototype supports a maximum-capture limit, self-triggered lifecycle smoke mode, graceful Ctrl+C handling, and per-session `status`/`stop` commands through a named Windows event. Runtime TOML config controls backend, mode, freshness, clipboard/selection behavior, and queue capacities; explicit CLI values take precedence. Per-user configuration and overlay state share `%USERPROFILE%\.captastic\captastic.toml`, which is loaded automatically when `--config` is omitted. Logs remain under `%USERPROFILE%\.captastic\logs`.

## CPU readback retention

Each retained `DxgiBackend` owns one persistent staging texture and a bounded pool of three
lazily allocated CPU readback buffers. Pool selection first reuses a correctly sized buffer that
has no outstanding `CpuFrame` lease. It initializes another slot only when every compatible
allocation is leased, so repeated non-overlapping captures stabilize at one CPU allocation while
genuine overlap can expand through all three slots. A fourth simultaneous lease preserves the
existing `BufferExhausted` result.

The pool owns one `Arc<[u8]>` reference per initialized slot. Selection, crop, and clipboard jobs
hold additional references while they consume a frame; a slot is writable only when the pool is
again its sole owner. An incompatible allocation is never reused as storage for a differently
sized frame. Staging-texture dimension or format changes reset the pool before readback, and a
free incompatible slot is replaced only when no empty slot remains.

A 3840 by 2160 tight BGRA frame is 33,177,600 bytes (about 31.64 MiB). Sequential captures
therefore retain one such CPU allocation rather than three, reducing expected private bytes by
about 63.3 MiB. Allocator bookkeeping and working-set residency can make process counters differ
slightly. The display manager retains a separate backend for every usable output, so this bound
applies per display session.

The 30-second selection-worker timeout releases the separate overlay surface cache. It does not
shrink the CPU pool or release the persistent staging texture. Captastic deliberately keeps those
resources warm: adding idle shrinking would require separately measuring the memory benefit
against allocation and D3D resource creation latency on the first capture after idle.

## Reproducible memory measurement

Use the same build, interactive Windows session, display topology, and DXGI options for both
samples. For the 4K reference case, use the 3840 by 2160 primary display with `latest`, CPU
frames, selection, and clipboard enabled.

1. Stop any existing daemon, start a fresh release daemon, and record its PID and command line.
2. Before triggering capture, record private bytes, working set, handle count, thread count, and
   the Windows `GPU Process Memory` dedicated/shared counters for that PID.
3. Perform at least five captures or cancellations strictly sequentially. Confirm in the log that
   each selection completed or cancelled before triggering the next so the test does not
   intentionally exercise overlapping ownership.
4. Wait at least 35 seconds after the final selection to cross the overlay's 30-second idle
   timeout.
5. Take at least three process samples one second apart. Confirm private bytes, handles, and
   threads stabilize instead of continuing to grow.
6. If VMMap is available, compare the count of heap allocations near the full-frame byte size.
   Treat anonymous graphics allocations as unattributed unless an allocation stack or API trace
   identifies their owning D3D resource.
7. Exercise two and three simultaneous frame leases with the deterministic CPU-pool tests. The UI
   does not need to manufacture overlap for the steady-state measurement.
8. Repeat a representative run on a lower-resolution display when one is available.

PowerShell's `Get-Process` exposes the core process counters:

```powershell
$captasticProcess = Get-Process -Id <pid>
$captasticProcess | Select-Object Id, PrivateMemorySize64, WorkingSet64, HandleCount,
    @{Name = 'ThreadCount'; Expression = { $_.Threads.Count }}
```

Use `Get-Counter '\GPU Process Memory(*)\Dedicated Usage'` and filter the returned instance
name for `pid_<pid>_` when the GPU Process Memory counter set is available. GPU usage is recorded
for regression detection, but a CPU-pool policy change is not expected to reduce persistent D3D
allocations.

## Clipboard path

The daemon enables clipboard publication by default. A configurable bounded queue connects the capture thread to a serialized clipboard worker. The worker creates a hidden message-only owner window, prepares top-down CF_DIBV5 and registered PNG representations, retries `OpenClipboard` for at most 50 ms, empties the clipboard, and transfers both movable allocations to Windows. Opaque DXGI frames omit the DIB alpha mask; deliberate native-window alpha advertises it. PNG improves transparent-corner interoperability.

Every publish also declines what Windows would otherwise keep of the capture. `CanIncludeInClipboardHistory` and `CanUploadToCloudClipboard` are set to a `DWORD` of zero, which keeps the capture out of the Win+V history and off the sync to the signed-in Microsoft account. `clipboard.allow_history` and `clipboard.allow_cloud_sync` opt back in, one path at a time. The markers are transferred before the pixels, so no ordering exists in which the capture is on the clipboard without them, and a marker that cannot be set fails the publish rather than falling back to retention the user configured against.

`cpu_frame_ready`, `clipboard_started`, and `clipboard_committed` are distinct event boundaries. Allocation, DIB copying, contention waits, and clipboard API work do not change native-frame or CPU-frame latency. Use `--clipboard false` to disable the worker.

## Native selection overlay

With `selection.preview = "auto"` or `"live"`, selection begins from display metadata without acquiring a CPU frame. The per-monitor-v2 layered overlay exposes the changing desktop through the selected display or region; confirmation destroys it, flushes DWM composition, and sends the coordinates to the capture thread for confirmation-anchored acquisition. `auto` retries with a frozen presenter if live overlay construction fails, while `live` reports the failure and `frozen` retains the original capture-first behavior. The overlay provides full-display, window, region, Options, and Capture controls. It restores the last selected tool and last adjusted region even when the previous overlay was canceled. Tool changes preserve the live region for the rest of the session. The overlay supports toolbar persistence, exact region dimensions, moving and eight-handle resizing, background dimming, a native high-contrast crosshair, privately registered Ioskeley Mono typography, and cached paints. Active drags use `SetCapture` until button-up so crossing an overlay edge does not strand drag state; deployments using software KVM or mouse-sharing hooks should include that interaction in acceptance testing. Window mode registers aspect-correct DWM thumbnails and retains capped static surfaces as per-window fallbacks; clicking requests a fresh full-resolution render.

Region dimensions are painted by a deterministic layout helper after measuring the exact physical-pixel text. Comfortable selections keep the badge inside; small selections use a stable top, bottom, left, or right candidate scored against monitor space, the pointer, resize handles, toolbar, and open menu. The result is clamped to the active monitor and uses hysteresis to avoid one-pixel placement jumps while dragging or resizing.

Text uses the bundled hinted Ioskeley Mono Medium face. Toolbar and dropdown labels use a compact 16-DIP height; the region badge uses 15 DIP; window-overview text retains 21 DIP. `AddFontMemResourceEx` registers the embedded TTF as a process-private resource for the overlay lifetime, and `RemoveFontMemResourceEx` releases it on exit. Toolbar labels are bounded and centered, dropdown labels are vertically centered and left-aligned beside their checkmarks, and real glyph measurements are regression-tested at 100, 125, 150, and 200 percent scaling. The SIL Open Font License 1.1 notice ships beside the source font asset.

The window chooser caches the complete static blurred background and window composition. `WM_MOUSEMOVE` skips invalidation while the hit target is unchanged. When the pointer crosses a target boundary, Captastic copies the cached DIB, draws only the selected/hovered antialiased accents, and calls `UpdateWindow` so the visual is presented before that mouse message returns.

Window enumeration and blur construction are lazy and therefore absent from ordinary region-mode overlay startup. Candidate renders run in ordered batches of two. Each native render downsamples its visible frame before returning the thumbnail, avoiding a full-resolution CPU-frame and GDI-surface pair per preview. Full-screen DIBs, the private font, and the native cursor are reused for captures within a 30-second activity window, then released on selection-worker idle timeout. Selection JSON exposes preparation, chooser construction, and retained-thumbnail memory measurements.

Window mode confirms on click. A valid preview click requests a fresh full-resolution native render, stores that exact frame in the selection result, and destroys the overlay immediately; the existing output worker then publishes it to the clipboard. Empty space or a failed fresh render leaves the overlay open. Full-display and region tools continue to use Capture or Enter for confirmation.

The window renderer carries the DWM-derived physical corner radius beside each thumbnail and full-resolution preview. Clipboard frames remain straight-alpha, while private paint surfaces are premultiplied once. Since `AlphaBlend` ignores high-quality stretch modes and GDI HALFTONE drops the alpha byte, Captastic resamples all four premultiplied BGRA channels together into the exact layout dimensions: separable area filtering when reducing and center-aligned bilinear filtering when enlarging. The exact-sized result is then composited 1:1 with `AlphaBlend(AC_SRC_ALPHA)`. The hover ring is an outside-only concentric stroke calculated from the same fitted image rectangle and scaled radius. This avoids blurred/pixelated scaling, double clipping, black corner fringes, mismatched borders, and unwanted rounding of square windows. Corner coverage uses an 8x8 sampling grid until the Direct2D compositor replaces this fallback.

Each short-lived window-render worker explicitly enters the per-monitor-v2 DPI context before querying geometry. `GetWindowRect`, `DWMWA_EXTENDED_FRAME_BOUNDS`, and render-surface coordinates therefore remain physical and consistent even at 125/150/200 percent scaling. Captastic also queries `DWMWA_VISIBLE_FRAME_BORDER_THICKNESS`, safely insets that exact number of physical pixels on all sides, and reduces the corner radius by the same amount. Older Windows versions that do not expose the attribute retain the existing zero-inset behavior.

The overlay limits `SetCapture` to an active toolbar, drawing, moving, or resizing gesture and releases it before processing button-up. `WM_CAPTURECHANGED` distinguishes that self-release from genuine capture theft: self-release preserves the state needed to commit the gesture, while external loss clears unfinished drag state. Captastic records the previous foreground window before showing the overlay and restores that window plus the standard arrow cursor after every normal or error exit.

Confirmed regions use absolute physical screen coordinates and are checked before a tight BGRA crop is allocated. Window candidates retain their native `HWND`. Captastic first attempts `PrintWindow(PW_RENDERFULLCONTENT)` and falls back to programmatic Windows Graphics Capture when that API is denied, including across the normal-to-elevated integrity boundary. WGC creates a free-threaded frame pool, waits up to 300 ms for the first window-compositor frame, performs bounded D3D11 staging readback, and then feeds the same clean-border and rounded-alpha pipeline used by `PrintWindow`. The complete renderer remains isolated behind a 700 ms response timeout and a two-slot admission gate. A timed-out foreign call is detached and its active permit is reclaimed so other windows remain capturable, while a separate eight-worker hard cap prevents repeated requests against permanently blocked targets from creating an unbounded thread backlog. A window rejected by both backends is omitted; Captastic never substitutes an occluded desktop crop.

DXGI selection requests additionally retain an immutable default-usage D3D11 texture. The native
frame is type-erased at the shared-core boundary and is handed back only to `captastic-windows`.
Confirmed regions are converted from absolute display coordinates to texture coordinates, copied
with `CopySubresourceRegion` into a region-sized staging texture, and read back without first
copying the full desktop on the CPU. Immediate-context access is serialized between the capture
and selection workers. JSON reports `gpu_copy_submit_ns`, `map_wait_ns`, `cpu_copy_ns`,
`bytes_read`, full-frame/avoided bytes, row contiguity, and total GPU materialization time. Full-display and native-window
selection retain their existing paths, and any GPU-region failure falls back to the validated CPU
crop so capture reliability is unchanged.

`selection_started`, `selection_confirmed`, and the legacy-named `crop_finished` event are recorded separately; JSON output identifies materialization as `frozen_display`, `frozen_desktop_crop`, or `native_window_render`. A one-frame selection queue prevents the overlay from exhausting the three-slot CPU pool. Ctrl+C posts `WM_CLOSE` through an overlay controller so shutdown does not wait for user input.

Use `--selection false` for direct full-display clipboard capture. The one-shot command `captastic capture --backend dxgi --selection true --clipboard true` exercises one live overlay and exits after confirmation or cancellation. Pass `--selection-preview live` to require the live presenter or `--selection-preview frozen` to preserve trigger-time pixels; `auto` is the default.

## Smoke test

Run from an interactive, unlocked Windows session:

```powershell
cargo run -p captastic-app -- doctor --json
cargo run -p captastic-app -- displays --backend dxgi --json
cargo run --release -p captastic-app -- daemon --backend dxgi --mode latest --cpu-frame true
```

Use `--mode fresh` for controlled tests where the desktop is known to present after every trigger. Timeouts on an unchanged desktop are correct in that mode.

Desktop Duplication access can be denied from an isolated sandbox even when the same executable succeeds in the user's interactive session. `captastic doctor` preserves the native HRESULT/message so the two cases are distinguishable.

### Remote Desktop

Captastic cannot capture from inside a Remote Desktop session. Windows composes a remote session onto a virtual display adapter — `Microsoft Remote Display Adapter`, carrying a generic `Default_Monitor` — and DXGI will not duplicate one. The symptom is `duplicate_output: Access is denied` on a session that is unlocked, enumerating normally, and holding no other duplication, which reads like a permissions problem and is not one.

`WTSQuerySessionInformation(WTSClientProtocolType)` answers it in one call, so the condition is reported as `DesktopUnavailable` naming the protocol, and the daemon waits rather than exiting: connecting remotely no longer kills a daemon that was capturing happily at the console, and it resumes when the console session is used again.

Two consequences worth knowing. The active display *identity* changes when a remote session takes over, because it genuinely is a different output — a remote session's generic monitor is not the physical panel, and the per-display remembered state is keyed accordingly. And a benchmark or soak run started over Remote Desktop measures nothing; run those from the console.

### A session without a desktop

Displays asleep, unplugged mid-session, a disconnected RDP session, or a desktop owned by the lock screen all present to DXGI the same way: no attached outputs. Enumerating nothing is therefore `CaptureErrorKind::DesktopUnavailable` whatever the cause — it means "not now", where `SourceUnavailable` keeps its narrower meaning of "displays exist, but not the one configured", which stays fatal because waiting cannot fix a `display =` naming absent hardware.

Captastic asks the session to *explain* the condition rather than to decide it, using `OpenInputDesktop`, the WTS connect state, and the WTS session lock flag. No one call answers alone: a locked session stays `WTSActive`, a disconnected one may still have an openable desktop, and — the one that took longest to establish — **a locked session usually reports the ordinary `Default` desktop**. Windows 11's lock screen is an application rather than a secure desktop, so input only moves to `Winlogon` while the credential prompt is actually up. `WTSSessionInfoEx` is what answers in every phase, and it is why `DesktopState::Locked` exists separately from `NotOurs`.

Measured on the development host, a lock does not stop *enumeration*: displays still enumerate with their persistent identities while the lock screen is up, so a lock is not what produced the empty display list in issue #51. Duplication is more complicated than "refused while locked", and the sequence matters because two of its three phases look identical from `OpenInputDesktop`:

| Phase | `OpenInputDesktop` | WTS lock flag | `DuplicateOutput` |
|---|---|---|---|
| Unlocked | `Default` | unlocked | succeeds |
| Locked, lock screen lit | `Default` | locked | **succeeds** |
| Locked, credential prompt up | refused | locked | refused |
| Locked, displays powered down | `Default` | locked | refused |

An existing duplication is invalidated at the lock with `AccessLost` (`the keyed mutex was abandoned`). A new one can still be acquired for as long as the lock screen is lit — not a brief transitional window, but the whole of it: 12 seconds in one probe run, and it ends with the display power-down rather than with the lock. About three seconds after the displays sleep, `DuplicateOutput` begins refusing and keeps refusing; one 190-second run held that state for 125 seconds. That last row is the one worth naming, because the desktop probe cannot see it — every one of those 125 seconds reported `Access is denied` while `OpenInputDesktop` answered `Default`, which is the bare permissions-shaped error issue #51 exists to prevent. Note also that a sleeping display is capturable while *unlocked*; it is the combination that refuses.

The daemon treats the condition as a wait: it starts, registers hotkeys, reports `capture_engine: "waiting_for_desktop"` in its ready event, and builds the capture engine once there is something to capture. Each wait is polled at the cost of asking — 500 ms for the two-syscall session probe, 2 s for an enumeration, and never by rebuilding DXGI on the engine-recovery schedule, which would be roughly 1,800 device initializations an hour for the length of a lock screen. That last clause is now measured rather than asserted; see below.

### Measured: the daemon loses its engine to a locked, dark session and comes back on its own

2026-08-20 17:16:31–17:21:53Z, same three-display host, `scripts/measure-lock-unlock-recovery.ps1
-Confirmed`. The daemon self-triggered once a second with the clipboard and selection off; the
script locked the workstation, asked the displays to power down, and then did nothing at all until
the operator returned and signed in 229.3 seconds later. Every log line is judged against a lock
flag sampled once a second and read either side of it, so a line timestamped across a transition is
counted and not judged — two were.

```
17:17:01.6  (LockWorkStation)
17:17:02.8  capture 33 failed: PermissionDenied in windows/get_physical_cursor_position: Access is denied.
17:17:04.0  capture 35 lost the capture engine; retrying 1/3 in 50 ms: AccessLost in
            dxgi/acquire_next_frame: The keyed mutex was abandoned. (0x887A0026)   <- rebuild succeeded
17:17:04.5  capture 35 failed: Timeout ...            (20 timeouts against a lit, motionless lock screen)
17:17:24.5  capture 55 lost the capture engine; retrying 1/3 in 50 ms: AccessLost ...
17:17:25.5  capture engine reinitialization failed during capture 55:
            DesktopUnavailable in dxgi/initialize: the workstation is locked        <- rebuild refused
17:17:25.5  capture 56 ignored: there is no display to capture yet      (203 of these, all judged locked)
17:17:55.9  still waiting for a capture source: 59 source poll(s) over 30.4 s, 0 of which walked the adapter list
            ... six such lines, the last 354 polls over 182.0 s, every one of them 0 ...
17:20:52.2  (operator signs in)
17:20:52.8  a display is available again; capture engine initialized after 402 source poll(s)
            over 207.3 s, 1 of which walked the adapter list
17:20:52.9  59 successful captures follow, none refused, no further engine loss
```

What that measures, and what it does not:

- **Both halves of the lock, in one run.** The lock invalidated the held duplication with
  `AccessLost` and the rebuild *succeeded* — the lit-lock-screen row of the table above, reached
  through the daemon rather than through a raw duplication loop. Twenty seconds later, when the
  panels actually slept, the next `AccessLost` met a **refused** rebuild. The displays took about
  20.5 s to power down after being asked, so the two phases separated cleanly on their own.
- **The refusal explained itself.** `DesktopUnavailable in dxgi/initialize: the workstation is
  locked` is the issue #51 criterion met on the daemon's own recovery path: the kind routes it to
  the session wait instead of the engine-recovery curve, and the message names the cause.
- **The 500 ms session poll, and no adapter walk.** 402 polls over 207.3 s is 1.94 a second, which
  is `DESKTOP_POLL_INTERVAL`; **401 of them cost two syscalls and nothing else**, and the single
  adapter walk is the one that ended the wait. Before this run the claim rested on reading the
  code — a daemon waiting out a lock used to log nothing between the line saying it was waiting and
  the line saying it had stopped. The counters and the half-minute heartbeat exist to make it
  readable, and are the only production change this measurement needed.
- **Recovery took 0.6 s and nobody helped.** Engine rebuilt 0.6 s after the lock flag cleared,
  first capture with pixels 0.7 s after it, then 59 successes with no relapse. Nothing touched the
  daemon between the lock and the unlock; the script issued no commands in that window.
- **Dropped, not mis-described.** All 203 dropped captures took the "there is no display to capture
  yet" branch and none took "while the capture engine is recovering" — the distinction
  `daemon.rs` draws between the two absences held for the whole outage.
- **What it does not measure:** one lock, one host, one display policy. A lock that never darkens
  its displays would stop at the first phase, and the `PermissionDenied` below is a loose end this
  run found rather than one it closed — it has since been closed in code, but no live run has seen
  the fix.

### The cursor probe denied without explaining, and now explains itself

Two captures failed 1.2 s after the lock with
`PermissionDenied in windows/get_physical_cursor_position: Access is denied. (0x80070005)` — the
`pointer` display policy asking where the cursor is while the credential prompt owned input. That was
the bare, permissions-shaped `Access is denied` that issue #51 exists to prevent, arriving with no
session context at all: `PermissionDenied` is neither `DesktopUnavailable` nor a
`requires_backend_recovery` kind, so it was reported as a failed capture and nothing said the
workstation was locked. Transient, and the daemon recovered without help, so it cost a confusing pair
of log lines rather than a capture the user wanted — but the confusion is the whole of what #51 is
about.

That denial now takes the route `duplicate_output` has taken since #51. `desktop_obstacle` has been
split into the syscall and the decision (`session_obstacle`, `dxgi.rs`), the decision carrying its
backend name so the cursor query keeps reporting itself as `windows/get_physical_cursor_position`
rather than being rewritten to `dxgi` — the operation a log reader greps for is unchanged, only the
explanation is new. `cursor_query_error` (`display_manager.rs`) asks the session **only** when the
call came back `E_ACCESSDENIED`, so the four syscalls behind the probe stay off the path every
`pointer` capture takes. A session that explains the denial — locked, at a secure desktop, detached,
or remote — produces `DesktopUnavailable` naming the cause, which the daemon already routes to the
session wait and already declines to rebuild the capture engine for. A session that does not explain
it keeps the original `PermissionDenied`, message and native code; a genuine rights problem on an
unlocked console must not be swallowed by a comfortable message about a lock, and an unanswered
session probe counts as "does not explain it".

**Status: verified by unit tests, not re-measured live.** Five tests cover it — the locked case, the
other temporary session states, the two states that must preserve the original error, the probe
being paid only on a denial, and the daemon reading the re-classified kind as a desktop wait rather
than a broken engine. Nobody has watched a real lock produce the new message: the run above is still
the only live evidence, and it predates the fix. `scripts/measure-lock-unlock-recovery.ps1
-Confirmed` is the way to re-measure it when a live lock run is next approved.

`CAPTASTIC_TEST_NO_DISPLAYS_MS` reports no attached outputs for that many milliseconds from process start, so the wait-and-recover path can be exercised without detaching a display. Debug builds only; `scripts/verify-no-display-recovery.ps1` drives it.

## GPU device loss

Three error kinds send a capture back through a rebuild rather than out to the caller:
`AccessLost`, `DeviceRemoved`, and `TopologyChanged` (`requires_backend_recovery`,
`crates/captastic-app/src/daemon.rs:1912`). A capture that fails with one of them drops the backend,
waits `recovery_delay` — 50 ms doubling to a 2 s ceiling — and rebuilds, up to three times inside
the same capture (`capture_with_backend_recovery`, `daemon.rs:1922`). If those are spent, the
capture fails and the daemon keeps rebuilding in the background on the same curve, with no ceiling,
raising one notification-area outage notice on the third consecutive failure. A rebuild that fails
because there is no desktop at all switches to the session/display poll instead, which is slower on
purpose.

DXGI only ever produces two of those three. `map_windows_error` (`dxgi.rs:2561`) turns
`DXGI_ERROR_ACCESS_LOST` into `AccessLost` and both `DXGI_ERROR_DEVICE_REMOVED` and
`DXGI_ERROR_DEVICE_RESET` into `DeviceRemoved`; everything else becomes a non-retryable
`NativeFailure`, which is *not* rebuilt for. That matters because the removal *reason* behind a
device-removed HRESULT is usually `DEVICE_HUNG` or `DRIVER_INTERNAL_ERROR`, neither of which the
generic mapping recovers from — so reasons are routed through `device_removed_error`
(`dxgi.rs:2588`) instead, which reports every non-success reason as `DeviceRemoved` and keeps the
reason as the native code. `TopologyChanged` comes from the display-configuration generation and
from a readback whose dimensions disagree with the display, not from an HRESULT.

The daemon is idle between hotkeys, so a device lost while nothing is capturing is not noticed until
the next trigger; the first capture after a loss is the one that pays for it.

### Measuring it

Two harnesses, because the two questions are different. Neither runs by default.

`a_real_device_loss_routes_through_the_rebuild_path` (`crates/captastic-windows/src/dxgi.rs`,
`#[ignore]`d) measures raw duplication: it captures on a loop, brackets every attempt with
`GetDeviceRemovedReason`, and records which HRESULT arrived, at which `operation`, with which
removal reason behind it, then rebuilds on the daemon's own curve and records whether and when
duplication comes back. Samples are counted rather than timed, because a DXGI call against a
restarting driver can block for seconds. It writes `%TEMP%\captastic-gpu-reset.log`.

`scripts/measure-gpu-reset-recovery.ps1` measures the product: it runs the daemon self-triggering
twice a second with the clipboard and selection off, and reads its log for the recovery lines. This
is the one that answers whether the daemon's classification actually routes a real loss to the
rebuild path, because those lines are only written from that path.

### Measured: a driver restart does not remove the device

2026-08-20, Intel Arc iGPU, Windows 11 26200, three attached displays. The operator pressed
Ctrl+Win+Shift+B once during the sampling window; the screen blanked and the machine beeped, so the
restart genuinely happened. Duplication did not notice: **1800 captures out of 1800 samples, zero
lost devices, zero rebuilds, zero unexplained failures, over 194 seconds.** Aggregate capture cost
was about 14 s across all 1800 samples — roughly 8 ms each — which bounds any stall around the blank
well under the length of the blank itself, though that first run recorded no per-sample timings and
so could not locate one. It records them now.

The assumption this was hired to check — that Ctrl+Win+Shift+B raises `DXGI_ERROR_DEVICE_REMOVED`
on a duplication device — is therefore false on this hardware. Nothing was ever removed: 3600
`GetDeviceRemovedReason` calls, bracketing every sample, all returned success. Do not read this as
"device loss recovery works"; read it as "this trigger did not remove the device".

It is still worth running as a guard, with `CAPTASTIC_GPU_RESET_EXPECT=survival`, which asserts the
survival instead of the loss. That direction matters: a driver or Windows build that starts breaking
duplication on a restart would fail the test loudly rather than quietly changing what the product
does when a user hits the chord. Read the next section before treating a `survival` failure as a
regression, though — the second run of the day, against the daemon rather than against a single raw
duplication, did not survive.

### Measured: the same chord did take the daemon's sessions away, and they came back

2026-08-20 14:23:50–14:28:01Z, same host, same chord, `scripts/measure-gpu-reset-recovery.ps1`.
The daemon held **three** retained DXGI sessions (DELL U2723QE 3840×2160, internal SDC4196
3840×2400, DELL U2723QE 2160×3840, `pointer` policy) and self-triggered every 500 ms to a limit of
480 captures. The operator pressed Ctrl+Win+Shift+B once at about 14:24:06; the screen blanked and
the machine beeped, so the restart genuinely happened. **480 of 480 captures succeeded and the
daemon exited on its own.** The run's whole recovery story is five lines:

```
14:24:06.850783 WARN  capture 32 lost the capture engine; dropping it and retrying 1/3 in 50 ms:
                      AccessLost in dxgi/acquire_next_frame: The keyed mutex was abandoned. (0x887A0026)
14:24:06.873423 INFO  display configuration invalidated generation=2 reason=tray_display_changed
14:24:07.927167 INFO  retained DXGI session initialized display=windows-monitor-DEL4279-dp8   3840x2160+0+0
14:24:08.060463 INFO  retained DXGI session initialized display=windows-monitor-SDC4196-internal0 3840x2400+3840-255
14:24:08.250508 INFO  retained DXGI session initialized display=windows-monitor-DEL4279-dp4   2160x3840-2160-728
14:24:08.361031 INFO  capture engine recovered during capture 32 after 1 attempt(s)
```

What that measures, and what it does not:

- **`AccessLost`, not `DeviceRemoved`.** `0x887A0026` is `DXGI_ERROR_ACCESS_LOST`, and
  `map_windows_error` routed it to `AccessLost`, which `requires_backend_recovery` rebuilds for.
  So the classification half of the seam is now measured against a real driver restart rather than
  against an injected error. The message text is whatever the system message table returns for that
  HRESULT; it is not evidence that a keyed mutex was involved, and the classification is taken from
  the code, not the string.
- **One attempt, ~1.5 s, unattended.** Capture 32 was resolved at 14:24:06.850638 and failed
  145 µs later. It finished at 14:24:08.361082: **1510 ms end to end**, one retry of a budget of
  three, never past the first 50 ms step of `recovery_delay`. Most of that is the rebuild, not the
  back-off — after the 50 ms wait, the first session took about 1026 ms to come back (the driver was
  still restarting) and the remaining two 323 ms between them. The longest the daemon went without a
  delivered frame is capture 31 finishing at 14:24:06.346361 to capture 32 finishing at
  14:24:08.361082: **2015 ms**.
- **All three sessions came back**, with the same display ids and the same bounds as at startup —
  six `retained DXGI session initialized` lines in the run, three at 14:23:50 and three at
  14:24:07–08.
- **The topology route was absorbed by the access-loss rebuild.** `WM_DISPLAYCHANGE` reached the
  tray 22 ms after the failure and bumped the display generation to 2, which is the input that makes
  the next capture return `TopologyChanged`. It never did: the rebuild that the `AccessLost` retry
  had already started did not finish until 14:24:08.250, so the replacement backend was built after
  the generation moved and satisfied it. One outage, one rebuild, not two.
- **The desktop-wait cadence was never touched.** No `capture engine reinitialization failed` line,
  so the single rebuild succeeded first time, so nothing ever reached `waiting_for_desktop` and its
  slower 500 ms/2 s poll. No notification-area outage notice either: that needs
  `BACKEND_OUTAGE_NOTICE_ATTEMPTS` (3) consecutive failed rebuilds and there was one successful one.
- **One WARN in 980 log lines, no ERROR, and no second event** in the remaining three and a half
  minutes. `self-trigger soak enqueued 479 trigger(s); 0 were refused by a full trigger queue`.
- Captures 33 and 34 report `native 962 ms` and `native 409 ms` and are *not* slow captures. Both
  finished within 9 ms of the line before them. `native_ready_offset_ns` is measured from
  `triggered_at`, when the trigger was received, so those two figures are the two self-trigger ticks
  that fired during the outage waiting their turn. The backlog drained in 18 ms and capture 36 was
  back to 0.574 ms.

### Why the two runs disagree, which is not settled

The same chord, on the same host, on the same day, left one raw duplication of the primary output
completely undisturbed and took all three of the daemon's sessions away. Both facts are measured;
the reason for the difference is not, and this is the honest state of it:

- The harness holds a single `DxgiBackend::new_primary()` across every sample. The daemon holds
  three retained sessions, one of them on the internal panel. Whether the count, or that particular
  output, is what makes the difference is untested.
- The daemon's run recorded a display reconfiguration alongside the restart (generation 2,
  `WM_DISPLAYCHANGE`). The harness has no message loop and no tray, so it cannot say whether its own
  run reconfigured anything. A mode change is exactly what invalidates duplication with
  `ACCESS_LOST`, so "the restart sometimes reconfigures the display and sometimes does not" fits
  both runs — but nothing here demonstrates it.

So: do not treat either outcome as the chord's defined behaviour. Choose
`CAPTASTIC_GPU_RESET_EXPECT` to match the outcome the run is meant to guard, and read a failure of
the `survival` branch as this ambiguity rather than as a regression, until something explains it.

### Still unverified: `DEVICE_REMOVED` and `DEVICE_RESET`

The `AccessLost` limb of `requires_backend_recovery` is now measured end to end against real
hardware. The `DeviceRemoved` limb is not, and it is the limb that `device_removed_error` and the
removal-reason mapping exist for. No trigger available without elevation removes the device on this
host: the driver-restart chord is measured not to, and everything else on the list below either
needs an elevated shell or a new artifact. The adapter cycle was written up, was available, and was
deliberately not run — it takes every display to the basic display driver on a machine somebody
works on, and it was not worth that to a run whose other half had already produced a real loss.
Until it runs, `DEVICE_REMOVED`/`DEVICE_RESET` recovery rests on
`every_device_removal_reason_triggers_recovery` and the injected-error tests, not on measurement.

### Triggers, and what each costs

Do not assume they are equivalent — see above for one that has now been measured and did not do what
it was assumed to do.

**Ctrl+Win+Shift+B** restarts the display driver stack. The screen blanks for a second or two with
a beep. Needs no elevation and no shell, which made it the one to try first. It has never been seen
to *remove* the device, so it is not a route to `DeviceRemoved`; whether it invalidates duplication
at all depends on something the two runs above did not isolate, so it is a guard rather than a
dependable trigger in either direction.
Whether `SendInput` can inject it is still unverified, and no longer worth verifying for this
purpose: it is a win32k chord rather than a `RegisterHotKey` binding, and injected input carries
`LLKHF_INJECTED`, so the harness waits for a human hand unless `CAPTASTIC_GPU_RESET_SENDINPUT=1`
says otherwise.

**Disabling and re-enabling the display adapter** is the heavy one, and on a hybrid laptop the
choice of adapter decides whether it is a measurement or an outage. Find the adapter that actually
owns the outputs — duplication builds its D3D11 device on the adapter of the selected output
(`dxgi.rs:202`), so an adapter with no display children is not the one under test:

```powershell
Get-PnpDevice -Class Display | Select-Object Status, FriendlyName, InstanceId
(Get-PnpDeviceProperty -InstanceId '<adapter instance id>' -KeyName 'DEVPKEY_Device_Children').Data
```

On the development host that is the Intel Arc iGPU; the RTX 4060 has no display children at all,
so disabling it would prove nothing. Disabling the one with the monitors on it takes every display
down to the basic display driver, and the shell that has to re-enable it is a shell nobody can
see — so both commands belong in one elevated invocation with the sleep between them, never typed
one at a time:

```powershell
# ELEVATED. The screen goes black for the duration of the sleep.
pnputil /disable-device "<adapter instance id>"; Start-Sleep -Seconds 10; pnputil /enable-device "<adapter instance id>"
```

**A TDR** cannot be caused without either a deliberate hang shader or a registry change to
`HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers` (`TdrLevel`, `TdrDelay`) and a reboot. The
first is a GPU workload written to hang, which is a new artifact to build and maintain for one
measurement; the second changes global recovery behaviour on a machine somebody works on. Neither
is worth it while the other two triggers are available, and a TDR's observable result — device
removed with a `DEVICE_HUNG` reason — is already covered by
`every_device_removal_reason_triggers_recovery`.

## Safety invariants

- COM initialization is balanced on the creating thread.
- Adapter and output interfaces remain alive through device and duplication creation.
- Successful `AcquireNextFrame` calls immediately enter an acquired-frame guard.
- Explicit release marks the guard released before calling `ReleaseFrame`, preventing double release on an error.
- Guard drop releases an outstanding frame on every early-return path.
- Windows output structures are initialized writable storage of the exact binding type.
