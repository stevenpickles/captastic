# Captastic

Captastic is a Windows-first Rust prototype for measuring extremely fast screenshot capture and providing a native selection-to-clipboard workflow.

The DXGI backend supports two deliberately different modes. `latest` is the resident-daemon default: when a capture is triggered, it drains any immediately available desktop frame and otherwise reuses the last retained image, so the daemon performs no DXGI acquisition while idle. Only the first capture may wait briefly when no retained image exists yet. `fresh` waits for a desktop frame presented after the trigger and is intended for controlled latency experiments. Both modes report frame age/timing provenance, and BGRA8 CPU readback uses preallocated staging and CPU buffers. By default, the resident daemon opens a native frozen-frame overlay after capture. Its floating toolbar provides full-display, window, and resizable-region modes, an Options menu, and a Capture button. Results are published to the Windows clipboard as uncompressed DIBV5 images plus a registered PNG compatibility representation. Region selections crop the frozen desktop and automatically restore the last confirmed rectangle; window selections retain the native window identity and render that window independently so other windows covering it are not copied.

## Build and verify

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p captastic-app -- benchmark --backend fake --iterations 500 --json
```

## Continuous integration

GitHub Actions checks formatting, rejects compiler and Clippy warnings, runs the workspace tests,
and performs release builds on Windows and Ubuntu. A separate Windows job instruments the workspace
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
.\install.ps1
.\install.ps1 -StartWithWindows
```

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
confirmed capture tool, and last confirmed region for every monitor. Region coordinates are local
to their monitor, so negative virtual-desktop coordinates do not leak into persisted state. Updates
preserve the rest of the TOML document and its comments. Existing global `[ui]` values remain a
backward-compatible fallback until that monitor records its own state.

Captastic writes operational output through Rust's `log` facade to both stderr and a persistent file.
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
primary monitor. To pin
Captastic to one physical monitor, list the attached displays without creating a capture session:

```powershell
captastic displays --backend dxgi --json
```

Copy the desired persistent ID into `captastic.toml` as
`display = "display:windows-monitor-0123456789abcdef"`. The same value can be tested without
editing configuration by passing `--display display:windows-monitor-0123456789abcdef` to
`daemon`, `capture`, or `benchmark`. A missing or disconnected configured display produces an
actionable error listing the IDs that remain attached.

Selection and clipboard output are enabled by default. Choose full display, window, or region from the toolbar. Each monitor restores its own last successfully used tool across daemon restarts. Region mode likewise restores that monitor's last confirmed rectangle; when no region has been captured on it yet, Captastic starts with a rectangle centered on the display at half its width and half its height. Saved rectangles keep their pixel dimensions and relative center after a resolution change; rotating a monitor rotates the center and swaps width and height before clamping the result to the new bounds. Switching from another tool into Region mode recalls that monitor's rectangle automatically. Drag the three-dot grip or any empty toolbar background to reposition the toolbar. Captastic stores its normalized center within that monitor's work area, scales the controls for the monitor's effective DPI, avoids taskbars, and restores the relative placement across resolution or scaling changes. Window mode blurs and dims the frozen desktop, then arranges eligible application windows as independent, aspect-correct surfaces. Overview surfaces are capped at 1.2 megapixels to bound memory; clicking a preview still requests a fresh full-resolution native frame for clipboard output. DWM-cloaked placeholders, shell surfaces, the desktop, minimized windows, and failed native renders are excluded. Region mode supports drawing, moving, and resizing with eight side/corner handles and displays exact pixel dimensions. Click **Capture** or press Enter to copy the selection; Esc or right-click cancels. **Options** can toggle background dimming or cancel capture. Captastic avoids Win32 mouse capture so mouse-sharing/KVM software can retain input ownership. Selection, materialization, PNG/DIB clipboard preparation, and clipboard timing remain outside native/CPU capture latency. `PrintWindow` rendering is isolated behind a 350 ms timeout with at most two timed-out native calls in flight, so a nonresponsive target cannot block the overlay or shutdown. Windows Graphics Capture remains planned for broader window compatibility.

Window ownership is display-local and deterministic. The display with the largest visible window
intersection owns the chooser entry; an exact tie prefers the window's native monitor and then the
persistent display ID. A spanning window therefore appears in exactly one chooser while its
complete image remains available for preview and capture.

Window mode is single-action: clicking a valid preview immediately confirms that fresh native window frame, closes the overlay, and sends it to the clipboard worker. The Capture button remains the confirmation action for full-display and region modes. Empty chooser space and windows that fail their fresh render leave the chooser open.

Window previews preserve each window's DWM corner preference. Their straight-alpha capture pixels are converted to a private premultiplied paint surface, resampled once to the exact layout size with an area filter for reduction or bilinear filter for enlargement, and then composited 1:1 with `AlphaBlend`. This avoids both low-quality `AlphaBlend` stretching and GDI HALFTONE's loss of the alpha channel. Hover outlines use the same fitted bounds and scaled corner radius as the preview; square and custom-framed windows are no longer forced through a fixed rounded mask.

Native window rendering runs in a per-monitor-v2 DPI context and removes the artifact-prone border pixels reported by DWM, then reconstructs a clean light border at the same physical thickness. This prevents asymmetric black or white rows while preserving a visible frame in previews and copied windows.

Overlay typography uses the bundled hinted Ioskeley Mono Medium face at a 21-pixel height. Captastic registers it only for the lifetime of the overlay process, so no system font installation is required. Ioskeley Mono is distributed under the SIL Open Font License 1.1; the bundled notice is in `crates/captastic-windows/assets/fonts/OFL-1.1.txt`.

Window mode precomposes its blurred backdrop and bounded-resolution overview surfaces into a static cache. Pointer movement that remains over the same target does not repaint; an actual hover transition copies the cache and draws only the rounded accent synchronously. Selecting a window captures and retains a fresh full-resolution native frame.

For an automated lifecycle smoke test that registers/unregisters the hotkey and performs one resident capture:

```powershell
cargo run -p captastic-app -- daemon --backend dxgi --mode latest --cpu-frame true --selection false --self-trigger --max-captures 1 --json
```

`latest` is the product-behavior mode and should also work on a static desktop. A `fresh` DXGI capture needs the desktop image itself to change after the trigger. A static desktop can legitimately time out; pointer-only updates do not count as a fresh desktop image.

Ctrl+C or `captastic stop` requests an orderly shutdown, unregisters the hotkey, and exits successfully. `captastic status` reports whether the per-session daemon is running. DXGI access/device loss drops the abandoned session before replacement construction and retries the same capture up to three times with bounded backoff. If those attempts fail, background reinitialization continues without acquiring another frame until the next capture.

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
