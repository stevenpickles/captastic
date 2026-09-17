# Captastic

Captastic is a fast, native screenshot tool for Windows, written in Rust. It runs as a resident
daemon behind configurable global hotkeys, opens a live overlay for full-display, window, or
resizable-region selection, and publishes the result to the Windows clipboard, to a PNG file, or to
both. It keeps a bounded history of recent captures, targets the display the user means across
multi-monitor and mixed-DPI layouts, and installs, upgrades, and uninstalls for the current user from
a portable archive that needs no administrator privileges, or from a Chocolatey package.
Windows is the only supported platform: capture, overlay, hotkeys, and clipboard output are native
Windows implementations rather than a portable abstraction, and other platforms are roadmap work
rather than shipped code.

The capture engine is what the rest is built to stay out of the way of. Disk, network, compression,
and configuration work are kept off the path between the hotkey and the frame, and the daemon holds
its capture resources warm so that pressing a hotkey does not initialize a device. Each stage —
native frame, CPU readback, selection, clipboard, encoding, and file output — is reported separately
rather than collapsed into one number, and every capture carries its own freshness and timing
provenance.

The DXGI backend supports two deliberately different modes. `latest` is the resident-daemon default: when a capture is triggered, it drains any immediately available desktop frame and otherwise reuses the last retained image, so the daemon performs no DXGI acquisition while idle. Only the first capture may wait briefly when no retained image exists yet. `fresh` waits for a desktop frame presented after the trigger and is intended for controlled latency experiments. Both modes report frame age/timing provenance, and BGRA8 CPU readback uses preallocated staging and CPU buffers. By default, the resident daemon opens a native live overlay before acquiring pixels. Its floating toolbar provides full-display, window, and resizable-region modes, an Options menu, and a Capture button. Region and full-display selection leave the desktop visible and changing until confirmation, then remove the overlay and request the output frame. Window mode uses DWM compositor relationships so animations and video keep updating in the chooser; clicking still performs a fresh isolated native window render. Results are published to the Windows clipboard as uncompressed DIBV5 images; straight-alpha window captures also include a registered PNG compatibility representation. Captures are marked so Windows keeps them out of the Win+V clipboard history and off the sync to the signed-in Microsoft account; `clipboard.allow_history` and `clipboard.allow_cloud_sync` opt back in.

## Build and verify

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p captastic-app -- benchmark --backend fake --iterations 500 --json
```

## Build identity

The workspace version in `Cargo.toml` is the next intended formal release. Captastic augments that
release core with source provenance for every untagged build:

- Local builds use `<version>-dev.<revision-count>.g<short-commit>` and append `.dirty` when the
  worktree has changes.
- GitHub Actions builds use `<version>-ci.<run-number>.<run-attempt>.g<short-commit>`.
- A clean build made from the matching `v<version>` tag uses the plain formal version.

The revision count is the number of commits since the newest version tag, or the repository-wide
count before the first version tag exists. The commit keeps builds from divergent branches
unambiguous. After publishing a release, advance the workspace version to the next intended release
before accepting further development; otherwise a prerelease would sort before the version that was
just published.

Use `captastic --version` for the compact identity or `captastic version --json` for the release
version, full commit, revision count, channel, dirty state, target, profile, and CI provenance.
Windows builds carry the same version and commit in their executable properties, and daemon startup
logs and benchmark reports include the embedded identity.

## Benchmark evidence

Every benchmark report carries an environment fingerprint — OS build, CPU, adapters with their
driver versions, displays with scale and refresh, session, power source, and the full build
identity including its dirty flag — and two runs are comparable only if all of that matches. A
driver update or a hundred commits between two runs moves a latency figure without moving anything
the numbers say, so a mismatch stops the comparison and names every differing field instead of
producing a percentage that reads like a regression.

`captastic benchmark --repeat 3 --output-dir <dir>` writes the artifacts a claim rests on: a full
report per run, the raw per-capture event stream per run with `--raw-events`, and a `repeated.json`
carrying every stage's spread at p50, p95, and p99. `captastic benchmark compare <baseline>
<candidate>` holds a later run against a committed one and prints the per-stage deltas with a
`within_noise`/`slower`/`faster` verdict. The operator procedure for turning those into a
publishable number — console session, AC power, a repainting display, the acceptance criteria, and
where accepted sets are committed — is [benchmarks/README.md](benchmarks/README.md).

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
cargo run -p captastic-app -- version --json
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
the icon to capture, or right-click it to capture, turn **Save Captures to Disk** on or off,
open the most recent capture or show it in its folder (**Open Last Capture** and **Show in
Folder**), pause/resume the global hotkey, open `captastic.toml`, open the persistent log, toggle
**Start with Windows**, or exit cleanly. If Windows
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
`captastic.<version>.nupkg` Chocolatey package. The repository's canonical local build command is
`./scripts/build-packages.ps1`; it also writes `dist/artifacts.json` with the package version,
embedded build identity, source provenance, filenames, and hashes used by CI and releases. The
packager verifies that both executables came from the current commit before consuming a skipped
build. Chocolatey output is currently x86_64-only, while
the portable package builder also supports ARM64. Extract the portable archive and run its
current-user installer from PowerShell:

```powershell
Unblock-File .\captastic-<version>-windows-x86_64.zip
# Extract the archive after unblocking it, then run:
.\install.ps1
.\install.ps1 -StartWithWindows
.\install.ps1 -NoLaunch
```

If the archive was extracted before it was unblocked, run
`Get-ChildItem -Recurse | Unblock-File` inside the extracted directory before launching the scripts.

Current releases are not Authenticode-signed, so Windows marks the download and SmartScreen shows a
*Windows protected your PC* prompt the first time the downloaded executable runs. See
[Unsigned releases](docs/unsigned-releases.md) for why signing is sequenced later, what the prompts
look like, and how to verify a download against its published SHA-256 checksum and `artifacts.json`
hashes.

The installer copies the CLI and console-free desktop launcher to
`%LOCALAPPDATA%\Programs\Captastic`, creates a per-user Start Menu shortcut, and starts the tray
application; pass `-NoLaunch` to install without starting it.
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

The implicit default profile is startup-recoverable: syntactically damaged TOML or invalid UTF-8
is renamed beside the original with a `.corrupt-*` suffix, Captastic starts from safe defaults,
and the notification area reports the recovery. Well-formed files with unknown keys, wrong types,
or values that fail validation remain in place and fail startup so operator mistakes are not
silently discarded. An explicit `--config <path>` is always strict and is never quarantined.
Captastic retains the five newest corrupt backups and removes abandoned atomic-write `.tmp-*`
siblings after seven days, preventing recovery artifacts from growing without bound.

When the daemon is started with `--config <path>`, that path is also the sole destination for tray
Open Config, the **Save Captures to Disk** setting, and managed UI-state updates; the default
profile is not read or written. The daemon
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
policy and `latest`/`fresh` mode. Daemon triggers, selection, clipboard, and logging use bounded
worker queues. UI-state changes update the controller-owned in-memory snapshot synchronously and
use a dedicated unbounded disk channel; its traffic is limited to compact overlay session-end
events, and the persistence worker coalesces equivalent updates before writing.

## File output

Captastic can write every capture to disk as well as, or instead of, the clipboard. It is off by
default, and the notification area's **Save Captures to Disk** turns it on and off without a
restart:

```toml
[output]
enabled = true
format = "png"
jpeg_quality = 90
# directory = 'C:\Users\you\Pictures\Captastic'
filename_template = "{timestamp}-{application}-{title}"
```

`directory` must be absolute — a daemon's working directory is whatever launched it, so a relative
path would put captures somewhere unpredictable — and defaults to `<home>\Pictures\Captastic`. It
is created when the daemon starts rather than at the first capture, so a directory that cannot be
created is a startup error instead of a surprise at the hotkey.

`format` chooses the encoder:

- **`png`** — lossless, compressed, and it keeps the straight alpha of a window capture. The
  default, and the right answer for a screenshot of text or an interface.
- **`jpeg`** — lossy and much smaller, written with a `.jpg` extension. `jpeg_quality` is 1–100 and
  defaults to 90. **JPEG cannot carry an alpha channel**, so the transparent corners and shadow of
  a window capture are composited over opaque white. That is inherent to the format rather than
  something Captastic decides on your behalf, and the log says so at debug level each time it
  happens; use `png` or `bmp` to keep them.
- **`bmp`** — uncompressed and alpha-preserving: a `BITMAPV5HEADER` with an alpha mask when the
  capture has alpha, a plain 24-bpp bitmap when it does not. A 4K capture is 33 MB and stays that
  way.

All three refuse a half-float or scRGB frame by name rather than narrowing it silently; see
[ADR 0006](docs/adr/0006-hdr-source-handling.md).

`filename_template` names each capture, without its extension. The tokens are `{timestamp}`,
`{date}`, `{time}`, `{display}`, `{mode}`, `{width}`, `{height}`, `{application}`, and `{title}`;
the two window tokens expand to nothing for a display or region capture, and the separators around
them collapse rather than leaving gaps behind. Values that come from a window are sanitized —
forbidden characters, control characters, parent hops, reserved device names, and length — and a
name can never place a capture outside the output directory.
`captastic config validate --path <file>` rejects an unknown token, a path separator, or a template
with no token at all, which is the same check the daemon makes at startup. A window title is
content; see [ADR 0008](docs/adr/0008-what-a-capture-reveals-by-default.md) for why the default
names it anyway.

A capture never overwrites a file it did not create: a name already taken is retried as `name-2`,
`name-3`, and so on, and the write itself refuses rather than replaces. Each written capture is
reported with its format, path, byte count, and encode/write timings — as a log line, or as a
`file_output_written` JSON event under `--json`.

**Save Captures to Disk** in the notification-area menu is checked while captures are being
written, and takes effect on the next capture rather than on the next start: turning it on starts
the file worker — creating the output directory and rejecting a bad `filename_template` there and
then — and turning it off stops it, leaving the clipboard untouched either way. The choice is
written back to `output.enabled` in the configuration this daemon is running on, preserving the
rest of the document and its comments, so it survives a restart; the file is created if the
default profile has never been written. If that write fails the notification area says so and the
setting still applies for this session. A start that fails — an output directory that cannot be
created, say — leaves the item unchecked and reports why, because the checkmark states where the
next capture will go. Everything else under `[output]` (format, directory, template) is read at
startup as before, so changing those still means a restart.

Captastic remembers recent captures under `[history]` (`max_items`, `max_age_days`,
`max_total_bytes`; `max_items = 0` turns it off). The notification-area menu uses that history for
**Open Last Capture**, **Show in Folder**, and **Prune Capture History**, each greyed until there
is something to open.

One-shot captures write to disk from the same configuration: `captastic capture --backend dxgi`
honours `[output]`, and `--config <file>` points it at a configuration other than the default.

JPEG encoding uses the [`jpeg-encoder`](https://crates.io/crates/jpeg-encoder) crate, which is
MIT or Apache-2.0 licensed with IJG-licensed portions: this software is based in part on the work
of the Independent JPEG Group.

## Logging and diagnostics

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
actionable error listing the IDs that remain attached. Note that only the daemon defaults to the
pointer display; the one-shot `capture` and `benchmark` commands default `--display` to `primary`.

Selection and clipboard output are enabled by default. Choose full display, window, or region from the toolbar. The `selection.preview` policy defaults to `auto`: it prefers the live presenter and reopens with a bounded frozen capture if live overlay setup fails. `live` requires confirmation-time behavior; `frozen` preserves trigger-time selection. A live selection first validates the capture engine's display list — against the display-configuration generation, and against a fingerprint of the monitor arrangement that is re-sampled from Windows on each press and so notices a dock or undock that no window of Captastic's was running to be told about — and rebuilds the engine before placing the overlay, so a monitor change (dock/undock, resolution, or arrangement) cannot open it on a stale arrangement; a confirmation capture that comes back describing different display bounds or rotation than the overlay covered is refused with a notification-area balloon rather than cropped incorrectly. A confirmation capture that fails outright because the layout moved under it — the engine refusing a display list a monitor change has outdated, or an engine rebuilt onto a display the overlay was not drawn on — raises the same balloon instead of ending in the log. A display change while the overlay is still open closes it, which is not a cancellation and is no longer reported as one: the log and the balloon name what changed — the display layout when a monitor arrives, leaves, or moves, the display settings for a DPI or work-area change such as a taskbar resizing — because the press is lost and pressing the hotkey again is the only remedy. In `--json` output that press reports the event `selection_display_changed` rather than `selection_cancelled`, under the same `schema_version`, so a script that counts cancellations does not silently absorb it. Each monitor restores its last selected tool across daemon restarts, including a selection followed by cancellation. Region mode likewise restores that monitor's last adjusted rectangle whether or not it was captured; when no region has been adjusted on it yet, Captastic starts with a rectangle centered on the display at half its width and half its height. Saved rectangles keep their pixel dimensions and relative center after a resolution change; rotating a monitor rotates the center and swaps width and height before clamping the result to the new bounds. Switching away from Region mode preserves the live rectangle, and switching back restores it immediately. Drag the three-dot grip or any empty toolbar background to reposition the toolbar. Captastic stores its normalized center within that monitor's work area, scales the controls for the monitor's effective DPI, avoids taskbars, and restores the relative placement across resolution or scaling changes. Window mode arranges eligible application windows as independent, aspect-correct DWM thumbnails. A per-window static surface remains available when DWM registration fails; those fallback surfaces are capped at 1.2 megapixels to bound memory. Clicking a preview requests a fresh full-resolution native frame for clipboard output. DWM-cloaked placeholders, shell surfaces, the desktop, minimized windows, and windows rejected by both native capture backends are excluded. Captastic first requests `PrintWindow`; when Windows integrity isolation rejects that request, it uses programmatic Windows Graphics Capture so Task Manager and elevated command shells remain unoccluded and selectable without elevating Captastic. Region mode supports drawing, moving, and resizing with eight side/corner handles and displays exact pixel dimensions. While a region is being drawn, moved, or resized its edges snap to the edges of the windows on that display, to the work area, and to the display itself once they come within 8 DIPs (scaled for the monitor's DPI, so it is the same physical distance at every scaling). A snapped edge lands exactly on the target: a region snapped to a window's right edge ends on that window's last pixel column, never one short, and a hairline accent guide along the target edge shows what it snapped to. The nearest edge wins, ties go to the topmost window, and the display — which every region is inside of — can only win when nothing else is in reach. Hold **Ctrl** while dragging to suppress snapping and place the edge exactly under the pointer; **Options -> Snap to Edges** turns it off for good and is remembered across restarts. The window list is enumerated once per overlay run, so a window moved or closed while the overlay is open is still snapped to where it was when the overlay opened; close and reopen the overlay to refresh it. The arrow keys nudge the selected region one physical pixel at a time, ten with **Shift** held, and resize its right and bottom edges instead of moving it with **Ctrl** held; holding a key repeats. Nudging never snaps, so it is the way to place an edge on a pixel no window sits on. Because the pointer covers the pixel an edge is landing on, a magnifier shows it: hold **Z** to bring it up, and in its default mode it also appears by itself whenever the pointer slows to a deliberate pace during a region drag, disappearing again as soon as the pointer travels. It shows the 31 physical pixels around the pointer enlarged six times (more at higher scaling), with a grid between them, the pixel under the pointer outlined, the selection's edges drawn at the exact pixel boundaries they occupy, and the pointer's desktop coordinate underneath. It sits beside the pointer and never covers the pixels it is magnifying. **Options -> Zoom** cycles Auto, Hold Z, and Off, and the choice is remembered across restarts. Click **Capture** or press Enter to copy the selection; Esc or right-click cancels without discarding the selected tool or adjusted region. **Options** can toggle background dimming, toggle edge snapping, choose when the magnifier appears, or cancel capture. Captastic takes Win32 mouse capture only for an active toolbar, draw, move, or resize drag and releases it at button-up; losing capture cancels the unfinished drag. This preserves completion when the pointer crosses the overlay edge, but software KVM behavior should be verified for the deployed input stack. Selection, materialization, PNG/DIB clipboard preparation, and clipboard timing remain outside native/CPU capture latency. Window rendering is isolated behind a 700 ms timeout and a two-slot active-work admission gate. A timed-out foreign call is detached and its active slot reclaimed so one bad target cannot permanently disable later captures. A separate eight-worker lifetime cap includes detached calls and rejects additional native renders until a worker exits, preventing permanently hung targets from creating an unbounded thread backlog. The WGC fallback waits for its first frame and performs bounded GPU readback entirely within that worker.

The region-dimension badge reports exact physical pixels. It treats the magnifier as an obstacle and moves out of its way, because the badge is the only one of the two with anywhere else to go. It remains inside a comfortable selection, moves to a stable outside side for a small selection, avoids the pointer, resize handles, and capture controls where practical, and stays clamped to the active monitor. See the [overlay UI verification guide](docs/overlay-ui-verification.md) for the layout contract and DPI/monitor matrix.

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

Ctrl+C or `captastic stop` requests an orderly shutdown, stops admitting capture triggers, and gives the daemon three seconds to drain before overdue workers are detached. Windows close, logoff, and shutdown notifications request the same bounded teardown; the native handler waits up to four seconds for its completion signal, while a canceled shutdown query leaves the daemon running. Configuration recovery, persistence failures, clipboard failures, and Open Config errors detected during normal operation are reported through context-specific notification-area messages. The persistent log is authoritative for failures and timeouts detected during final teardown because the notification icon is being removed. `captastic status` reports whether the per-session daemon is running on Windows and reports `unsupported` on platforms without a native daemon. DXGI access/device loss drops the abandoned session before replacement construction and retries the same capture up to three times with bounded backoff. If those attempts fail, background reinitialization continues without acquiring another frame until the next capture.

## Known limitations

- **Multi-adapter virtual desktops are not composed.** `daemon.display = "virtual_desktop"` composes
  every display only when all outputs share one DXGI adapter. A topology whose outputs span more
  than one adapter returns an explicit structured unsupported-topology error rather than a partial
  or silently substituted composite. Cross-adapter transfer, mixed-refresh freshness, and mixed
  color behavior are unresolved design questions, not an implementation gap.
- **HDR sources are captured as SDR.** Captastic asks the compositor for 8-bit BGRA and lets Windows
  perform the conversion, so an HDR desktop is capturable and its screenshot matches what other
  tools produce; Captastic implements no tone-mapping curve of its own. Preserving high dynamic
  range end to end is deliberately not addressed and needs an output format that can carry it. See
  [ADR 0006](docs/adr/0006-hdr-source-handling.md).
- **Everything is scoped to one user session.** Installation, launch-at-login registration,
  configuration, and the daemon control signal belong to the installing interactive user.
  Deployment as `SYSTEM`, elevation with a different administrator account, and multi-user or
  fast-user-switching migration are not supported; managed deployment tooling should stop Captastic
  in each affected user session before modifying files. See
  [Chocolatey packaging](docs/chocolatey.md).

## Performance implementation notes

- The magnifier samples 31x31 pixels per paint and enlarges them with GDI, and its rest timer only runs during a region drag with Zoom set to Auto. A run that never adjusts a region never allocates its sample surface and never arms a timer.
- Region-mode overlay startup constructs neither the window list nor the blurred chooser background. Enumeration is deferred until the Window tool is selected or the first region adjustment needs edges to snap to, whichever comes first, and then happens once per run; the blurred backdrop is built only for the Window tool. A run with snapping switched off never enumerates at all.
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

See [ROADMAP.md](ROADMAP.md) for prioritized work after v0.1.0 — the remaining milestones, their
status, and the release signing and distribution backlog — and
[outputs/Captastic-Specification.md](outputs/Captastic-Specification.md) for the complete original
implementation plan.
