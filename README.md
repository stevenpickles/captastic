# Captastic

Captastic is a Windows-first Rust prototype for measuring extremely fast screenshot capture and providing a native selection-to-clipboard workflow.

The DXGI backend supports two deliberately different modes. `latest` is the resident-daemon default: when a capture is triggered, it drains any immediately available desktop frame and otherwise reuses the last retained image, so the daemon performs no DXGI acquisition while idle. Only the first capture may wait briefly when no retained image exists yet. `fresh` waits for a desktop frame presented after the trigger and is intended for controlled latency experiments. Both modes report frame age/timing provenance, and BGRA8 CPU readback uses preallocated staging and CPU buffers. By default, the resident daemon opens a native frozen-frame overlay after capture. Its floating toolbar provides full-display, window, and resizable-region modes, an Options menu, and a Capture button. Results are published to the Windows clipboard as uncompressed DIBV5 images; straight-alpha window captures also include a registered PNG compatibility representation. Region selections crop the frozen desktop and automatically restore the last adjusted rectangle; window selections retain the native window identity and render that window independently so other windows covering it are not copied.

## Build and verify

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p captastic-app -- benchmark --backend fake --iterations 500 --json
```

## Continuous integration

GitHub Actions checks formatting, rejects compiler and Clippy warnings, runs the workspace tests,
and performs distribution builds on Windows, Ubuntu, and macOS. A separate Windows job instruments the workspace
with LLVM source coverage and uploads a browsable `captastic-coverage-html` artifact. Download that
artifact from the workflow run and open `index.html` to inspect line, function, and region coverage.
Interactive desktop and clipboard tests remain ignored in hosted CI because they require a live user session.

## Useful commands

```powershell
cargo run -p captastic-app -- doctor
cargo run -p captastic-app -- displays --json
cargo run -p captastic-app -- capture --backend fake --json
cargo run -p captastic-app -- config validate --path captastic.example.toml
cargo run -p captastic-app -- status --json
cargo run -p captastic-app -- stop
```

On an interactive Windows desktop:

```powershell
cargo run -p captastic-app -- doctor --json
cargo run -p captastic-app -- displays --backend dxgi --json
cargo run -p captastic-app -- capture --backend dxgi --mode fresh --cpu-frame true --json
cargo run --release -p captastic-app -- benchmark --backend dxgi --mode fresh --cpu-frame true --iterations 100 --json
```

Run the resident foreground daemon and press `Ctrl+Shift+F9`:

```powershell
cargo run --release -p captastic-app
cargo run --release -p captastic-app -- daemon --backend dxgi --mode latest --cpu-frame true
cargo run --release -p captastic-app -- daemon --config captastic.example.toml
```

Running Captastic without a subcommand starts the resident desktop capture daemon with the default
configuration. The explicit `daemon` form remains available for scripts, diagnostics, and CLI
overrides. A named per-session control event prevents more than one daemon instance from running.
While the daemon is active, Captastic places an icon in the Windows notification area. Double-click
the icon to capture, or right-click it to capture, pause/resume the global hotkey, open
`captastic.toml`, open the persistent log, toggle **Start with Windows**, or exit cleanly. If Windows
Explorer restarts, Captastic restores its notification icon automatically. Tray initialization
failures are logged and do not disable the capture daemon.

Release builds also contain `captastic-desktop.exe`, a console-free launcher intended for shortcuts
and login startup. It starts the sibling `captastic.exe` daemon without creating a terminal window
and exits immediately; launching it while the daemon is already running is a no-op. Launch at login
can also be managed explicitly:

```powershell
captastic startup enable
captastic startup status --json
captastic startup disable
```

## Install and release packages

Tagged releases and manually dispatched release workflows produce a
`captastic-<version>-windows-x86_64.zip` archive, its SHA-256 checksum, and a self-contained
`captastic.<version>.nupkg` Chocolatey package. Extract the portable archive and run its current-user
installer from PowerShell:

```powershell
Unblock-File .\captastic-<version>-windows-x86_64.zip
# Extract the archive after unblocking it, then run:
.\install.ps1
.\install.ps1 -StartWithWindows
```

If the archive was extracted before it was unblocked, run
`Get-ChildItem -Recurse | Unblock-File` inside the extracted directory before launching the scripts.

The installer copies the CLI and console-free desktop launcher to
`%LOCALAPPDATA%\Programs\Captastic`, creates a Start Menu shortcut, and starts the tray application.
It does not require administrator privileges. Run the installed `uninstall.ps1` to stop Captastic,
remove login startup and installed files, and preserve `~/.captastic` by default. Pass
`-RemoveSettings` only when configuration and logs should also be deleted.

After the package has been accepted by the Chocolatey community repository, install and update it
from an elevated PowerShell prompt with:

```powershell
choco install captastic
choco upgrade captastic
```

The Chocolatey package adds `captastic` and the console-free `captastic-desktop` launcher to `PATH`
and creates a Start Menu shortcut without launching the desktop application during first install. It
preserves `~/.captastic` on upgrades and uninstall. See
[Chocolatey packaging](docs/chocolatey.md) for local package testing, portable-install migration
behavior, and the manual community publishing procedure.

When `--config` is omitted, the daemon automatically loads
`%USERPROFILE%\.captastic\captastic.toml` if it exists. The same file stores Captastic-managed UI
state under `[ui.displays.<persistent-id>]`, including an independent toolbar position, last
selected capture tool, and last adjusted region for every monitor. These preferences are retained
after cancellation. Region coordinates are monitor-local, so negative origins do not leak into persisted state. Updates
preserve the rest of the TOML document and its comments. Existing global `[ui]` values remain a
backward-compatible fallback until that monitor records its own state.

When the daemon is started with `--config <path>`, that path is also the sole destination for tray
Open Config and managed UI-state updates; the default profile is not read or written. The daemon
loads behavioral and remembered UI settings at startup, so hand edits take effect after a restart.
Background UI saves re-read the current document and preserve unrelated edits and comments. The
one-shot `captastic capture --selection true` command uses the default profile and flushes its UI
updates before exiting.

## Configurable hotkeys

The canonical format keeps bindings under `[hotkey.bindings]`. Only `last_workflow` is enabled by
default, preserving the existing `Ctrl+Shift+F9` behavior; omit any other action to keep it disabled:

```toml
[hotkey]
repeat = "ignore"

[hotkey.bindings]
last_workflow = "Ctrl+Shift+F9"
region = "Ctrl+Shift+R"
window = "Ctrl+Shift+W"
full_display = "Ctrl+Shift+F10"
repeat_last_region = "Ctrl+Shift+F11"
```

Bindings are case-insensitive on input and logged canonically. A chord is `+`-separated, may use
`Ctrl`/`Control`, `Alt`, `Shift`, and `Win`/`Windows`, and must contain exactly one key from `A-Z`,
`0-9`, or `F1-F24`. Duplicate modifiers, empty tokens, multiple keys, unsupported keys, empty
action bindings, and one chord assigned to multiple actions are errors. Existing
`[hotkey] binding = "Ctrl+Shift+F9"` remains a compatibility alias for `last_workflow`; defining
both forms for that action is rejected as ambiguous.

`last_workflow`, `region`, and `window` open the frozen-frame overlay with the remembered, Region,
or Window tool respectively. `full_display` publishes the resolved display directly without
constructing overlay resources. `repeat_last_region` uses only that display's last confirmed Region
selection, validates its persistent display identity and source geometry, and uses GPU region
materialization with checked CPU fallback. Missing, stale, or invalid confirmed state opens Region
mode from daemon-cached restored/default UI state and logs a structured fallback reason; it never
captures unrelated state or reads TOML after the trigger. All actions retain the configured display
policy and `latest`/`fresh` mode. Daemon triggers enter one bounded command queue; selection,
clipboard, UI-state persistence, and logging each have their own bounded worker queue.
Daemon, capture, and benchmark commands write operational output through Rust's `log` facade to
both stderr and a persistent file. Read-only utility commands use stderr only unless `--log-file`
is supplied.
Capture, selection, clipboard, recovery, and daemon lifecycle messages therefore share one format
and filtering policy. The default compact format uses an RFC 3339 UTC timestamp with microsecond
precision, followed by the level, Rust module target, and message:

```text
2026-08-10T01:38:14.402172Z DEBUG captastic::daemon: capture engine resumed
```

Compact console output is colorized automatically: timestamps are gray, levels use
severity-specific colors, and module targets are cyan. Captastic adapts the output for the active
Windows terminal and strips all color escapes when stderr is redirected or color is disabled.
Persistent log files remain plain text. JSON logging is never colorized.

Machine-readable command results continue to use stdout, so `--json` can be redirected or parsed
without mixing in diagnostics. Captastic keeps its per-user configuration, UI state, and logs in
`%USERPROFILE%\.captastic` on Windows (or `$HOME/.captastic` elsewhere). Configuration and UI state
share `%USERPROFILE%\.captastic\captastic.toml`. The default log file is
`%USERPROFILE%\.captastic\logs\captastic.log`; the resolved path is logged when the process starts and
is included in daemon ready JSON. File writes run on a bounded background queue and never block the
capture or overlay threads. Configure `logging.level` (`off`, `error`, `warn`, `info`, `debug`, or
`trace`), `logging.format` (`compact` or `json`), and optional `logging.file` in TOML, or override
them with the global `--log-level`, `--log-format`, and `--log-file` flags. The active log rotates
at `logging.max_file_bytes` (5 MiB by default), retaining `logging.retained_files` archives (three
by default) as `captastic.log.1`, `captastic.log.2`, and `captastic.log.3`.

Daemon settings use TOML values first and explicit CLI flags second. `max_frame_age_ms = 0` preserves static-desktop `latest` behavior; set a positive value when a workflow must reject older retained frames.

The default `daemon.display = "pointer"` opens Captastic on the monitor containing the pointer when
the capture hotkey is dequeued. Captastic resolves the pointer once per capture and does not install
or run a cursor polling loop. Set `daemon.display = "primary"` to always follow the current Windows
primary monitor. Set `daemon.display = "virtual_desktop"` to compose the normalized physical-pixel
bounds of every display when all outputs share one DXGI adapter. Desktop gaps are opaque black and,
if Windows reports overlapping display bounds, the lexicographically smaller persistent display ID
wins. Multi-adapter virtual desktops return an explicit unsupported-topology error. To pin Captastic
to one physical monitor, list the attached displays without creating a capture session:

```powershell
captastic displays --backend dxgi --json
```

Copy the desired persistent ID into `captastic.toml` as
`display = "display:windows-monitor-0123456789abcdef"`. The same value can be tested without
editing configuration by passing `--display display:windows-monitor-0123456789abcdef` to
`daemon`, `capture`, or `benchmark`. A missing or disconnected configured display produces an
actionable error listing the IDs that remain attached.

Selection and clipboard output are enabled by default. Choose full display, window, or region from the toolbar. Each monitor restores its last selected tool across daemon restarts, including a selection followed by cancellation. Region mode likewise restores that monitor's last adjusted rectangle whether or not it was captured; when no region has been adjusted on it yet, Captastic starts with a rectangle centered on the display at half its width and half its height. Saved rectangles keep their pixel dimensions and relative center after a resolution change; rotating a monitor rotates the center and swaps width and height before clamping the result to the new bounds. Switching away from Region mode preserves the live rectangle, and switching back restores it immediately. Drag the three-dot grip or any empty toolbar background to reposition the toolbar. Captastic stores its normalized center within that monitor's work area, scales the controls for the monitor's effective DPI, avoids taskbars, and restores the relative placement across resolution or scaling changes. Window mode blurs and dims the frozen desktop, then arranges eligible application windows as independent, aspect-correct surfaces. Overview surfaces are capped at 1.2 megapixels to bound memory; clicking a preview still requests a fresh full-resolution native frame for clipboard output. DWM-cloaked placeholders, shell surfaces, the desktop, minimized windows, and windows rejected by both native capture backends are excluded. Captastic first requests `PrintWindow`; when Windows integrity isolation rejects that request, it uses programmatic Windows Graphics Capture so Task Manager and elevated command shells remain unoccluded and selectable without elevating Captastic. Region mode supports drawing, moving, and resizing with eight side/corner handles and displays exact pixel dimensions. Click **Capture** or press Enter to copy the selection; Esc or right-click cancels without discarding the selected tool or adjusted region. **Options** can toggle background dimming or cancel capture. Captastic avoids Win32 mouse capture so mouse-sharing/KVM software can retain input ownership. Selection, materialization, PNG/DIB clipboard preparation, and clipboard timing remain outside native/CPU capture latency. Window rendering is isolated behind a 700 ms timeout with at most two timed-out native calls in flight, so a nonresponsive target cannot block the overlay or shutdown. The WGC fallback waits for its first frame and performs bounded GPU readback entirely within that worker.

The region-dimension badge reports exact physical pixels. It remains inside a comfortable selection, moves to a stable outside side for a small selection, avoids the pointer, resize handles, and capture controls where practical, and stays clamped to the active monitor. See the [overlay UI verification guide](docs/overlay-ui-verification.md) for the layout contract and DPI/monitor matrix.

Window ownership is display-local and deterministic. The display with the largest visible window
intersection owns the chooser entry; an exact tie prefers the window's native monitor and then the
persistent display ID. A spanning window therefore appears in exactly one chooser while its
complete image remains available for preview and capture.

Window mode is single-action: clicking a valid preview immediately confirms that fresh native window frame, closes the overlay, and sends it to the clipboard worker. The Capture button remains the confirmation action for full-display and region modes. Empty chooser space and windows that fail their fresh render leave the chooser open.

Window previews preserve each window's DWM corner preference. Their straight-alpha capture pixels are converted to a private premultiplied paint surface, resampled once to the exact layout size with an area filter for reduction or bilinear filter for enlargement, and then composited 1:1 with `AlphaBlend`. This avoids both low-quality `AlphaBlend` stretching and GDI HALFTONE's loss of the alpha channel. Hover outlines use the same fitted bounds and scaled corner radius as the preview; square and custom-framed windows are no longer forced through a fixed rounded mask.

Native window rendering runs in a per-monitor-v2 DPI context and removes the artifact-prone border pixels reported by DWM, then reconstructs a clean light border at the same physical thickness. This prevents asymmetric black or white rows while preserving a visible frame in previews and copied windows.

Overlay typography uses the bundled hinted Ioskeley Mono Medium face. The compact toolbar and menu use a 16-DIP height, the adaptive region-dimension badge uses 15 DIP, and the window overview retains its 21-DIP height. Captastic registers the font only for the lifetime of the overlay process, so no system font installation is required. Ioskeley Mono is distributed under the SIL Open Font License 1.1; the bundled notice is in `crates/captastic-windows/assets/fonts/OFL-1.1.txt`.

Window mode precomposes its blurred backdrop and bounded-resolution overview surfaces into a static cache. Pointer movement that remains over the same target does not repaint; an actual hover transition copies the cache and draws only the rounded accent synchronously. Selecting a window captures and retains a fresh full-resolution native frame.

For an automated lifecycle smoke test that registers/unregisters the hotkey and performs one resident capture:

```powershell
cargo run -p captastic-app -- daemon --backend dxgi --mode latest --cpu-frame true --selection false --self-trigger --max-captures 1 --json
```

`latest` is the product-behavior mode and should also work on a static desktop. A `fresh` DXGI capture needs the desktop image itself to change after the trigger. A static desktop can legitimately time out; pointer-only updates do not count as a fresh desktop image.

Ctrl+C or `captastic stop` requests an orderly shutdown, unregisters the hotkey, and exits successfully. `captastic status` reports whether the per-session daemon is running on Windows and reports `unsupported` on platforms without a native daemon. DXGI access/device loss drops the abandoned session before replacement construction and retries the same capture up to three times with bounded backoff. If those attempts fail, background reinitialization continues without acquiring another frame until the next capture.

## Performance implementation notes

- Region-mode overlay startup does not enumerate application windows or construct the blurred chooser background; both are created only if Window is selected.
- Full-screen overlay DIBs, the private font registration, and the native region cursor are reused by the persistent selection worker for captures made within 30 seconds. The cache is released after an idle interval.
- Window thumbnails are rendered two at a time, downscaled inside the bounded native render worker, and never materialized as an additional full-resolution overview surface.
- Tightly pitched DXGI staging textures use one contiguous CPU copy; padded textures retain the checked row-copy fallback.
- Transparent-window PNG is streamed directly into its final byte vector with one 64 KiB scratch block. PNG and all clipboard allocations remain after CPU-frame readiness.
- Selection JSON reports `overlay_preparation_ns`, `window_overview_ns`, retained preview count/bytes, and clipboard JSON reports `png_encode_ns`.
- DXGI selection captures now retain an opt-in immutable GPU snapshot. Confirmed regions use
  `CopySubresourceRegion` and read back only the selected pixels; JSON identifies
  `dxgi_gpu_region` materialization and reports GPU copy submission, map wait, CPU copy, byte
  count, and total materialization time. A native error falls back to the checked CPU crop.

For a smaller distributable binary without changing the profiled release build, use `cargo build --profile dist -p captastic-app`.

See [ROADMAP.md](ROADMAP.md) for prioritized work after the Windows desktop milestone and
[outputs/Captastic-Specification.md](outputs/Captastic-Specification.md) for the complete original
implementation plan.
