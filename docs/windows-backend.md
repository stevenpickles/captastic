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

The current path deliberately rejects:

- cursor inclusion, because DXGI may supply a separate hardware cursor that must be composed deliberately;
- multi-adapter virtual-desktop topologies, which return a structured unsupported error.

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

### A session without a desktop

Displays asleep, unplugged mid-session, a disconnected RDP session, or a desktop owned by the lock screen all present to DXGI the same way: no attached outputs. Enumerating nothing is therefore `CaptureErrorKind::DesktopUnavailable` whatever the cause — it means "not now", where `SourceUnavailable` keeps its narrower meaning of "displays exist, but not the one configured", which stays fatal because waiting cannot fix a `display =` naming absent hardware.

Captastic asks the session to *explain* the condition rather than to decide it, using `OpenInputDesktop` and the WTS connect state; neither call answers alone, because a locked session stays `WTSActive` and a disconnected one may still have an openable desktop. Measured on the development host, a plain `Win+L` lock does not stop enumeration or duplication at all, so a fix keyed on the lock would not have addressed the failure that prompted this work (issue #51).

The daemon treats the condition as a wait: it starts, registers hotkeys, reports `capture_engine: "waiting_for_desktop"` in its ready event, and builds the capture engine once there is something to capture. Each wait is polled at the cost of asking — 500 ms for the two-syscall session probe, 2 s for an enumeration, and never by rebuilding DXGI on the engine-recovery schedule, which would be roughly 1,800 device initializations an hour for the length of a lock screen.

`CAPTASTIC_TEST_NO_DISPLAYS_MS` reports no attached outputs for that many milliseconds from process start, so the wait-and-recover path can be exercised without detaching a display. Debug builds only; `scripts/verify-no-display-recovery.ps1` drives it.

## Safety invariants

- COM initialization is balanced on the creating thread.
- Adapter and output interfaces remain alive through device and duplication creation.
- Successful `AcquireNextFrame` calls immediately enter an acquired-frame guard.
- Explicit release marks the guard released before calling `ReleaseFrame`, preventing double release on an error.
- Guard drop releases an outstanding frame on every early-return path.
- Windows output structures are initialized writable storage of the exact binding type.
