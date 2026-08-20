# Draft — GitHub release body for v0.1.0

> **This file is a draft of the release body for the `v0.1.0` GitHub release.** It is not the release
> itself. Paste the content below the horizontal rule into the release description when the tag is
> published, replacing `<version>` where it appears and confirming that the attached artifact names
> match. This file lives in the repository so the wording can be reviewed before it is published.

---

Captastic is a fast, native screenshot tool for Windows. It runs as a resident daemon behind
configurable global hotkeys, opens a live overlay for full-display, window, or resizable-region
selection, and publishes the result to the Windows clipboard, to a PNG file, or to both. This is the
first formal release: the Windows capture engine, the selection overlay, the notification-area
experience, and current-user installation are complete and in daily-driver shape. Windows is the only
supported platform, by design — capture, overlay, hotkeys, and clipboard output are native Windows
implementations rather than a portable abstraction.

## Capture

- A persistent DXGI capture engine with two honest, deliberately different modes. `latest` is the
  resident-daemon default: it drains any immediately available desktop frame and otherwise reuses the
  last retained image, so the daemon performs no DXGI acquisition while idle. `fresh` waits for a
  desktop frame presented after the trigger. Both report frame age and timing provenance.
- Disk, network, compression, and configuration work stay off the path between the hotkey and the
  frame, and the daemon holds its capture resources warm so a hotkey press does not initialize a
  device.
- GPU-backed region materialization with a checked CPU fallback; confirmed regions read back only the
  selected pixels.
- Optional cursor composition, including DXGI pointer shapes, hotspots, and visibility.

## Selection overlay

- A native live overlay with a floating toolbar for full-display, window, and resizable-region
  modes, an Options menu, and a Capture button. Region and full-display selection leave the desktop
  visible and changing until confirmation.
- Window mode arranges eligible windows as aspect-correct DWM thumbnails, so animations and video
  keep updating in the chooser; clicking one performs a fresh isolated native window render.
  `PrintWindow` is tried first, with programmatic Windows Graphics Capture as the fallback for
  windows Windows integrity isolation refuses — so Task Manager and elevated shells remain selectable
  without elevating Captastic.
- Region mode supports drawing, moving, and resizing with eight handles and reports exact physical
  pixel dimensions.
- `selection.preview` chooses between the live presenter (`auto`, `live`) and a bounded frozen
  capture (`frozen`), with `auto` falling back if live overlay setup fails.

## Output

- Clipboard publication as uncompressed DIBV5, with a registered PNG compatibility representation for
  straight-alpha window captures. Captures are marked so Windows keeps them out of the Win+V
  clipboard history and off the sync to the signed-in Microsoft account; `clipboard.allow_history`
  and `clipboard.allow_cloud_sync` opt back in.
- Optional PNG file output with a configurable directory, sanitized filename templates
  (`{timestamp}`, `{application}`, `{title}`, `{display}`, `{mode}`, `{width}`, `{height}`, and
  more), collision-safe atomic finalization, and explicit queue-full behavior. Encoding and file I/O
  never occur before frame readiness and never block the capture or overlay threads, and clipboard
  success is independent of file-output failure.
- A bounded capture history with configurable item, age, and storage retention, reachable from the
  tray as **Open Last Capture** and **Show in Folder**.

## Displays and hotkeys

- Configured, primary, and pointer-targeted display policies, plus same-adapter virtual-desktop
  composition. The pointer display is resolved once per capture; there is no cursor polling loop.
- Persistent per-display UI state: an independent toolbar position, last selected tool, and last
  adjusted region for every monitor, in physical-pixel coordinates, scaled for each monitor's
  effective DPI, and restored across resolution, scaling, and rotation changes.
- Configurable action hotkeys for the remembered workflow, Region, Window, direct full-display
  capture, and repeat-last-region. The full set registers atomically and reports the exact
  conflicting chord and action on failure.
- Bounded topology recovery: display addition, removal, resolution, rotation, scaling, adapter, and
  session changes rebuild only the affected state.

## Desktop experience and operations

- A notification-area icon with capture, hotkey pause/resume, open configuration, open log, **Start
  with Windows**, and clean exit. It restores itself if Windows Explorer restarts, and a tray
  initialization failure is logged without disabling capture.
- `captastic-desktop.exe`, a console-free launcher for shortcuts and login startup.
- Current-user install, upgrade, and uninstall from the portable archive — which needs no
  administrator privileges — or from Chocolatey, with `~/.captastic` preserved unless its removal is
  explicitly requested.
- Structured logging to stderr and a rotating persistent file, in a compact colorized format or JSON,
  with machine-readable command results kept on stdout so `--json` can be redirected or parsed
  cleanly.
- A startup-recoverable default configuration profile: damaged TOML is quarantined beside the
  original, Captastic starts from safe defaults, and the notification area reports the recovery. An
  explicit `--config <path>` is always strict and never quarantined.
- Diagnostics: `captastic doctor`, `displays`, `status`, `version --json`, `config validate`, and
  `benchmark`.

## Install

**Portable archive.** Download `captastic-<version>-windows-x86_64.zip` and its `.sha256` file, then:

```powershell
Unblock-File .\captastic-<version>-windows-x86_64.zip
# Extract the archive after unblocking it, then from the extracted directory:
.\install.ps1
```

`install.ps1 -StartWithWindows` also registers launch at login; `-NoLaunch` installs without starting
the tray application. The installer copies both executables to `%LOCALAPPDATA%\Programs\Captastic`,
creates a per-user Start Menu shortcut, and requires no administrator privileges. `uninstall.ps1`
reverses it and preserves `~/.captastic` unless `-RemoveSettings` is passed.

Unblock the archive *before* extracting it: files extracted from a marked archive inherit the Mark of
the Web. If it was already extracted, run `Get-ChildItem -Recurse | Unblock-File` inside the extracted
directory instead.

**Verify the download.** Compare the archive against the published checksum before running it:

```powershell
$archive = '.\captastic-<version>-windows-x86_64.zip'
$expected = (Get-Content -LiteralPath "$archive.sha256" -Raw).Trim().Split()[0]
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash

if ($actual -ieq $expected) { "match: $archive" } else { "MISMATCH: $archive" }
```

`artifacts.json`, also attached, carries a SHA-256 hash for every attached file plus the exact commit
and build identity the packages were produced from. See
[docs/unsigned-releases.md](https://github.com/stevenpickles/captastic/blob/v0.1.0/docs/unsigned-releases.md) for the manifest-wide verification loop.

**Chocolatey.** The package is built and attached to this release, but `choco install captastic` does
not work against the default community source yet: the first submission has to pass Chocolatey
community validation and moderation, and the push is performed manually after the tagged release and
its download URLs are confirmed. The package is self-contained — the executables are embedded, so
installation downloads and executes no second installer — and it can be installed from a local
directory in the meantime. See [docs/chocolatey.md](https://github.com/stevenpickles/captastic/blob/v0.1.0/docs/chocolatey.md).

## Known limitations

- **Multi-adapter virtual desktops are not composed.** `daemon.display = "virtual_desktop"` composes
  every display only when all outputs share one DXGI adapter. A topology spanning more than one
  adapter returns an explicit structured unsupported-topology error rather than a partial or
  silently substituted composite.
- **HDR sources are captured as SDR.** Captastic asks the compositor for 8-bit BGRA and lets Windows
  perform the conversion, so an HDR desktop is capturable and its screenshot matches what other tools
  produce; Captastic implements no tone-mapping curve of its own. Preserving high dynamic range end
  to end is deliberately not addressed and needs an output format that can carry it (ADR 0006).
- **Everything is scoped to one user session.** Installation, launch-at-login registration,
  configuration, and the daemon control signal belong to the installing interactive user. Deployment
  as `SYSTEM`, elevation with a different administrator account, and multi-user or
  fast-user-switching migration are not supported.
- **Recovery coverage is uneven, and the roadmap says which is which.** The recovery paths that have
  been verified live — including lock/unlock recovery, display hot-plug and the no-display case, and
  the driver-restart limb of GPU-reset recovery — are listed in
  [ROADMAP.md](https://github.com/stevenpickles/captastic/blob/v0.1.0/ROADMAP.md) with what each run measured. Sleep/wake, Remote Desktop, and GPU
  device-removal recovery are covered by unit tests and the fake backend but have had limited live
  verification; they are expected to work and have not been watched working on real hardware.

## A note on signing

These packages are **not Authenticode-signed**. Windows will show a *Windows protected your PC*
SmartScreen prompt the first time the downloaded executable runs (choose **More info** →
**Run anyway**), and browsers may warn on the download. This is a deliberate sequencing choice: a
long-lived signing key is not kept in the repository or in ordinary CI secrets, and the release
workflow is structured so signing can be inserted between the final build and packaging once a
public-trust provider with HSM or managed signing and RFC 3161 timestamps is in place. Until then,
releases ship deterministic packages with SHA-256 checksums and an `artifacts.json` manifest.

[docs/unsigned-releases.md](https://github.com/stevenpickles/captastic/blob/v0.1.0/docs/unsigned-releases.md) covers what the prompts look like, why
`Unblock-File` is needed, and how to verify every attached file.
