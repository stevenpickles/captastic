# Windows DXGI backend

## Current scope

`captastic-windows` initializes COM, enumerates attached DXGI outputs, creates a D3D11 device on the selected output's adapter, and retains an `IDXGIOutputDuplication` session for the backend lifetime.

The current capture path supports:

- primary-display enumeration;
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

- rotated CPU frames, until rotation normalization is implemented;
- cursor inclusion, because DXGI may supply a separate hardware cursor that must be composed deliberately.

## Resident hotkey path

`captastic daemon` creates the DXGI backend on a dedicated capture thread, then blocks on its command channel while idle. A second Windows thread owns `RegisterHotKey` and a Win32 message loop. The `WM_HOTKEY` branch timestamps immediately, constructs a fixed trigger record, attempts a nonblocking send into a four-entry queue, and returns. For `latest`, the capture thread performs one nonblocking Desktop Duplication drain at trigger time and reuses the retained image when the desktop has not changed; only the first capture may wait up to 100 ms for an initial frame. Capture and CPU readback occur on the capture thread; native selection and clipboard publication run on their own serialized workers after CPU readiness.

The current binding is `Ctrl+Shift+F9` with `MOD_NOREPEAT`. Queue-full events are counted. The foreground prototype supports a maximum-capture limit, self-triggered lifecycle smoke mode, graceful Ctrl+C handling, and per-session `status`/`stop` commands through a named Windows event. Runtime TOML config controls backend, mode, freshness, clipboard/selection behavior, and queue capacities; explicit CLI values take precedence. Per-user configuration and overlay state share `%USERPROFILE%\.captastic\captastic.toml`, which is loaded automatically when `--config` is omitted. Logs remain under `%USERPROFILE%\.captastic\logs`.

## Clipboard path

The daemon enables clipboard publication by default. A configurable bounded queue connects the capture thread to a serialized clipboard worker. The worker creates a hidden message-only owner window, prepares top-down CF_DIBV5 and registered PNG representations, retries `OpenClipboard` for at most 50 ms, empties the clipboard, and transfers both movable allocations to Windows. Opaque DXGI frames omit the DIB alpha mask; deliberate native-window alpha advertises it. PNG improves transparent-corner interoperability.

`cpu_frame_ready`, `clipboard_started`, and `clipboard_committed` are distinct event boundaries. Allocation, DIB copying, contention waits, and clipboard API work do not change native-frame or CPU-frame latency. Use `--clipboard false` to disable the worker.

## Native selection overlay

Selection runs after Captastic acquires the frozen CPU frame. The per-monitor-v2 native overlay provides full-display, window, region, Options, and Capture controls. Region mode automatically restores the last confirmed rectangle. The overlay supports toolbar persistence, exact region dimensions, moving and eight-handle resizing, background dimming, a native high-contrast crosshair, privately registered Ioskeley Mono typography, cached paints, and KVM-safe input handling without `SetCapture`. Window mode retains aspect-correct preview surfaces capped at 1.2 megapixels; clicking requests a fresh full-resolution render.

Text uses the bundled hinted Ioskeley Mono Medium face at a 21-pixel height. `AddFontMemResourceEx` registers the embedded TTF as a process-private resource for the overlay lifetime, and `RemoveFontMemResourceEx` releases it on exit. Toolbar labels use bounded horizontal and vertical centering; dropdown labels use the same vertical centering with consistent left alignment beside their checkmarks. The dropdown is widened and its row height increased for the larger monospaced type. The SIL Open Font License 1.1 notice ships beside the source font asset.

The window chooser caches the complete static blurred background and window composition. `WM_MOUSEMOVE` skips invalidation while the hit target is unchanged. When the pointer crosses a target boundary, Captastic copies the cached DIB, draws only the selected/hovered antialiased accents, and calls `UpdateWindow` so the visual is presented before that mouse message returns.

Window enumeration and blur construction are lazy and therefore absent from ordinary region-mode overlay startup. Candidate renders run in ordered batches of two. Each native render downsamples its visible frame before returning the thumbnail, avoiding a full-resolution CPU-frame and GDI-surface pair per preview. Full-screen DIBs, the private font, and the native cursor are reused for captures within a 30-second activity window, then released on selection-worker idle timeout. Selection JSON exposes preparation, chooser construction, and retained-thumbnail memory measurements.

Window mode confirms on click. A valid preview click requests a fresh full-resolution native render, stores that exact frame in the selection result, and destroys the overlay immediately; the existing output worker then publishes it to the clipboard. Empty space or a failed fresh render leaves the overlay open. Full-display and region tools continue to use Capture or Enter for confirmation.

The window renderer carries the DWM-derived physical corner radius beside each thumbnail and full-resolution preview. Clipboard frames remain straight-alpha, while private paint surfaces are premultiplied once. Since `AlphaBlend` ignores high-quality stretch modes and GDI HALFTONE drops the alpha byte, Captastic resamples all four premultiplied BGRA channels together into the exact layout dimensions: separable area filtering when reducing and center-aligned bilinear filtering when enlarging. The exact-sized result is then composited 1:1 with `AlphaBlend(AC_SRC_ALPHA)`. The hover ring is an outside-only concentric stroke calculated from the same fitted image rectangle and scaled radius. This avoids blurred/pixelated scaling, double clipping, black corner fringes, mismatched borders, and unwanted rounding of square windows. Corner coverage uses an 8x8 sampling grid until the Direct2D compositor replaces this fallback.

Each short-lived `PrintWindow` worker explicitly enters the per-monitor-v2 DPI context before querying geometry. `GetWindowRect`, `DWMWA_EXTENDED_FRAME_BOUNDS`, and render-surface coordinates therefore remain physical and consistent even at 125/150/200 percent scaling. Captastic also queries `DWMWA_VISIBLE_FRAME_BORDER_THICKNESS`, safely insets that exact number of physical pixels on all sides, and reduces the corner radius by the same amount. Older Windows versions that do not expose the attribute retain the existing zero-inset behavior.

The overlay does not call `SetCapture`; this avoids displacing input ownership held by software KVM and mouse-sharing tools such as Synergy. Because the overlay covers the captured display, ordinary in-window mouse messages are sufficient while the pointer remains on that display. Captastic records the previous foreground window before showing the overlay and restores that window plus the standard arrow cursor after every normal or error exit.

Confirmed regions use absolute physical screen coordinates and are checked before a tight BGRA crop is allocated. Window candidates retain their native `HWND`; `PrintWindow(PW_RENDERFULLCONTENT)` rendering is isolated behind a 350 ms response timeout with at most two timed-out native calls left in flight. Failed windows are omitted and Captastic never substitutes an occluded desktop crop. Windows Graphics Capture remains the planned compatibility path for GPU-heavy, protected, or unsupported windows.

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

Use `--selection false` for direct full-display clipboard capture. The one-shot command `captastic capture --backend dxgi --selection true --clipboard true` exercises one overlay and exits after confirmation or cancellation.

## Smoke test

Run from an interactive, unlocked Windows session:

```powershell
cargo run -p captastic-app -- doctor --json
cargo run -p captastic-app -- displays --backend dxgi --json
cargo run --release -p captastic-app -- daemon --backend dxgi --mode latest --cpu-frame true
```

Use `--mode fresh` for controlled tests where the desktop is known to present after every trigger. Timeouts on an unchanged desktop are correct in that mode.

Desktop Duplication access can be denied from an isolated sandbox even when the same executable succeeds in the user's interactive session. `captastic doctor` preserves the native HRESULT/message so the two cases are distinguishable.

## Safety invariants

- COM initialization is balanced on the creating thread.
- Adapter and output interfaces remain alive through device and duplication creation.
- Successful `AcquireNextFrame` calls immediately enter an acquired-frame guard.
- Explicit release marks the guard released before calling `ReleaseFrame`, preventing double release on an error.
- Guard drop releases an outstanding frame on every early-return path.
- Windows output structures are initialized writable storage of the exact binding type.
