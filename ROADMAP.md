# Captastic roadmap

Captastic now has a fast Windows-native DXGI capture engine, clipboard output, region and window
selection, persistent UI state, a notification-area desktop experience, current-user installation,
and release packaging. v0.1.0 and v0.1.1 have shipped and v0.2.0 is merged to `dev` awaiting its
release branch; the work after it should make Captastic more useful on real workstations while
preserving the low-latency, native design.

This roadmap is ordered by user impact and architectural dependency, with release readiness placed
first because it is the only time-bound section. Closed investigations have moved to
[docs/investigations.md](docs/investigations.md) so this file carries planning signal rather than
narrative; every fact they recorded survives there. Authenticode signing remains important for a
public release, but it gates neither v0.1.0 nor the capture milestones below.

## Product principles

- Keep disk, network, compression, configuration parsing, and resource initialization outside the
  hotkey-to-frame critical path.
- Measure native-frame, CPU-readback, selection, clipboard, encoding, and file output as separate
  stages.
- Prefer explicit native backends over a lowest-common-denominator screenshot abstraction.
- Never hide freshness, fallback, permission, topology, or capture-source differences behind one
  latency number.
- Keep annotation, history, and other product features downstream of the capture engine.

## Completed foundation

- Persistent Windows DXGI engine with honest `latest` and `fresh` modes.
- GPU-backed region materialization with checked CPU fallback.
- Native full-display, window, and resizable-region overlay workflows.
- Clipboard publication, transparent native-window output, and persistent workflow state.
- Notification-area controls, launch-at-login management, installation, upgrade, and uninstall.
- Structured logging, bounded workers, CI, HTML coverage, and portable release packaging.
- Configured, primary, and pointer-targeted multi-monitor capture with persistent per-display UI
  state, physical-pixel coordinates, effective-DPI scaling, and bounded topology recovery.
- Configurable action hotkeys for remembered, Region, Window, full-display, and repeat-last-region
  workflows, including validated per-display confirmed-region state.
- Programmatic Windows Graphics Capture fallback for windows rejected by `PrintWindow`, with bounded
  worker isolation and GPU readback.
- Live selection previews (`selection.preview = auto|live|frozen`) with confirmation-anchored
  capture, per-pixel-alpha live overlays, and a DWM-thumbnail window chooser with static fallback
  surfaces (ADR 0004, PR #14).
- Pixel-precise region adjustment: edges snapping to window frames, the work area, and the display
  with a Ctrl override and accent guides, arrow-key nudging, and a pointer magnifier, both
  preferences remembered globally in `state.toml` (PR #89).
- A capture taken at every overlay press, with `F` or Options -> View switching between the live
  desktop and that frame and confirmation materializing from the view on screen (ADR 0004
  amendment, PR #92).
- PNG, JPEG, and BMP file output with a validated filename template and a notification-area
  **Save Captures to Disk** toggle that starts and stops the file worker without a restart
  (PRs #88 and #90).
- Benchmark evidence tooling: a host fingerprint in every report, run compatibility and budget
  hosts keyed on it, raw per-repeat artifacts, and `benchmark compare` (PRs #87 and #91).

## Release readiness — v0.1.0

**Status:** v0.1.0 shipped on 2026-09-03: `release/v0.1.0` merged to `main` by PR #81, the tag on
that merge commit ran the tagged path of the release workflow, and the GitHub release carries the
archive, checksum, Chocolatey package, and manifest. The manual Chocolatey community push has not
been made. v0.1.1 shipped on 2026-09-16, a patch release cut from `dev` after PR #83 (the
dock-switch first-capture fixes) under the same branch model; its notes are
`docs/release-notes-v0.1.1.md`. The gates below are kept as the record of what v0.1.0 had to
satisfy.
v0.1.0 is a deliberately unsigned first tag whose purpose is to exercise the release mechanics end
to end and to describe honestly what Captastic is and is not. It is not gated on the capture
milestones below, and it makes no performance claim.

**Branch model for every formal release.** Cut a `release/v<version>` branch from `dev`, merge it
to `main` by pull request, tag `v<version>` on `main`, and then merge the release branch back to
`dev` by pull request. Nothing is pushed to `main` or `dev` directly; both are covered by the
repository ruleset that requires pull requests. `main` does not exist before v0.1.0. One
consequence to know about: `build.rs` and `scripts/build-packages.ps1` measure the revision count
from the newest `v*` tag reachable from `HEAD`, and the tag sits on a merge commit that only
`main` contains, so `dev` builds keep counting from the previous reachable tag or the repository
root. That is cosmetic; if it ever matters, open the back-merge pull request from `main` instead
of from the release branch.

### Gates — before the tag

- **Unsigned-release documentation and README repositioning.** Landing alongside this refresh as
  `docs/unsigned-releases.md`, which states what an unsigned release means for the person
  installing it and how a download is verified against the published SHA-256, and as a README that
  positions Captastic for a first release rather than for a development audience. Nothing is
  advertised that signing would be needed to claim.
- **A release-notes draft (`docs/release-notes-v0.1.0.md`) that publishes no absolute performance
  numbers.** The decision, recorded here rather than left implicit: Milestone 5's exit criterion
  *"Three compatible repeat runs support every published performance claim"* is **deferred past
  v0.1.0**, and is satisfied vacuously at v0.1.0 by publishing no absolute claims at all — with no
  published number there is nothing for repeat runs to support. The criterion binds again the
  moment an absolute figure is published, and publishing one waits on the repeat-run and
  environment-fingerprint automation Milestone 5 still lists as outstanding, and on the absolute
  budgets deliberately left unset until measured on the documented benchmark host. Release notes
  may describe the design and its latency boundaries; they may not quote a millisecond.
- **Release mechanics never yet exercised live.** Four of them, in the order they occur:
  - The **tag-triggered path of the release workflow**. The one successful workflow run was a
    `workflow_dispatch`, which takes the CI-prerelease branch (`-PrereleaseLabel ci.…`). The
    `-ReleaseTag` branch of `scripts/build-packages.ps1` has never run: it additionally proves the
    matching tag points at `HEAD`, the source is clean, the Cargo version matches, and both
    executables embed the exact formal version, and only then marks the manifest publishable and
    records a direct tagged-release URL in `VERIFICATION.txt`. Every one of those proofs is
    untested against a real tag.
  - The **disposable-VM checklist** in [docs/chocolatey.md](docs/chocolatey.md): Start Menu entry
    launches the tray application, the global hotkey captures, an upgrade stops a running daemon
    cleanly, and `~/.captastic` survives uninstall. Hosted CI covers silent install, shims,
    shortcut, upgrade and uninstall cleanup, but has no interactive desktop, so this cannot be
    delegated to it.
  - The **manual `choco push`**, only after the tagged GitHub release exists, its direct download
    URLs work, and its manifest hashes match. A first submission also has to pass Chocolatey
    community validation and moderation before `choco install captastic` works against the default
    source, so it is not finished when the push returns.
  - The **immediate post-tag workspace version bump** on `dev`, once the release branch has been
    merged back, so subsequent `dev` and `ci` identifiers are prereleases of the next release
    rather than of the one just published.

### Operator-run verification — recommended before the tag, formally deferrable

These need a human at the machine, and they are the recovery paths that shipped with the least live
verification. Recommended before the tag; deferrable past it only if the release notes disclose
that they are unverified.

- Live **sleep/wake** recovery.
- Live **Remote Desktop** recovery.
- The **`DEVICE_REMOVED`/`DEVICE_RESET` limb** of GPU-reset recovery, via the elevated adapter
  cycle. Its `AccessLost` limb is already measured against a real driver restart.

### Explicitly not gating

- **Multi-adapter composition** (Milestone 1) — hardware-gated, and the structured unsupported
  error is an honest answer rather than a silent wrong one.
- **HDR verification on a real HDR display** — hardware; the sinks already refuse what they cannot
  describe.
- **Authenticode signing** — deliberately sequenced after a stable release channel exists, which is
  what v0.1.0 creates.
- **Milestone 3's follow-ups** (retained sessions, the backend trait, richer metrics) — internal
  quality, no user-visible contract.
- **Milestones 6 and 7** — post-release by the existing decision gate.

## Release readiness — v0.2.0

**Status:** every v0.2.0 slice is merged to `dev` as of 2026-09-17 — PRs #87 and #91 (benchmark
evidence), #88 (output formats), #89 (region precision), #90 (the notification-area file-output
toggle), and #92 (the frozen-view toggle). The release branch has not been cut, and the notes are
a draft: [docs/release-notes-v0.2.0.md](docs/release-notes-v0.2.0.md). What remains needs a human
at the machine, which is why none of it was done by the work that merged.

### Gates — before the tag

- **The manual checklists in PRs #89, #90 and #92.** None of the overlay behaviour this release is
  about has been watched working: no agent drove a pointer or read the screen, and no second daemon
  could take the session control event from the running one. The region-precision and frozen-view
  matrices are in [docs/overlay-ui-verification.md](docs/overlay-ui-verification.md); the
  file-output toggle's steps are in PR #90. The one check nothing else can substitute for is the
  live-view magnifier sample coming back free of the overlay, which rests on
  `WDA_EXCLUDEFROMCAPTURE` rather than on a test.
- **The benchmark claim run is optional and gates only a number.** Running
  [benchmarks/README.md](benchmarks/README.md) produces the accepted sets a published figure has to
  rest on; skipping it is fine, and then the notes quote no figure. It is not a release blocker
  either way.

### Then the release itself

The v0.1.0 branch model, unchanged: cut `release/v0.2.0` from `dev`, merge it to `main` by pull
request, tag `v0.2.0` on `main`, run the disposable-VM checklist in
[docs/chocolatey.md](docs/chocolatey.md), push to Chocolatey manually once the release URLs and
hashes verify, merge the release branch back to `dev`, and bump the workspace version to 0.3.0.

**The standing rule, restated because v0.2.0 is the first release with the tooling to break it:**
release notes quote no millisecond unless the claim procedure has been run and the sets that
support the figure are committed. As drafted, v0.2.0 publishes no absolute number, which satisfies
Milestone 5's deferred criterion the same way v0.1.0 did — vacuously, and on purpose.

## Milestone 1 — Multi-monitor and topology support

**Status:** In progress. Configured/primary/pointer/virtual-desktop policies, persistent display
identity, per-display workflow state, mixed-DPI overlay placement, topology-triggered backend
recovery, rotated-output normalization, and same-adapter virtual-desktop composition are complete.
Multi-adapter composition and broader hardware validation remain.

**Outcome:** Captastic behaves predictably across workstation display layouts and can capture the
display the user intends without initializing a capture engine after the hotkey is pressed.

### Display selection

- Honor a configured display instead of always constructing the primary-display backend.
- Support display policies for `primary`, `pointer`, a stable configured display ID, and the complete
  virtual desktop.
- Keep required per-output DXGI engines warm, or initialize them before readiness, so pointer-based
  selection does not move device creation into the measured hotkey path.
- Expose display identity, desktop coordinates, scale, rotation, adapter, refresh rate, and active
  policy through `captastic displays`, `status`, logs, and structured capture results.
- Define a clear fallback when a configured display is missing; never silently capture an unrelated
  display.

### Per-display workflow state

- Store the last region and toolbar position per stable display identity rather than globally.
- Restore a saved region only when it fits the current display geometry; otherwise clamp or create
  the documented centered half-display default.
- Associate direct-capture state with a topology generation so stale coordinates cannot target a
  newly arranged display accidentally.
- Preserve negative virtual-desktop coordinates and physical-pixel semantics across every overlay
  and crop operation.

### Topology and rotation

- Detect display addition, removal, resolution changes, rotation, scaling changes, adapter changes,
  and session transitions.
- Rebuild only affected duplication/device state while keeping shutdown and pending captures bounded.
- Normalize all DXGI rotations into the top-left BGRA frame contract instead of rejecting portrait
  outputs.
- Same-adapter virtual-desktop composition preserves each output's physical pixels without scaling,
  normalizes rotated outputs before placement, fills topology gaps with opaque black, and resolves
  overlapping bounds by stable display ID so enumeration order cannot change the result. Composite
  output uses a bounded pool of reusable CPU slots — the same count as a single display's readback
  pool, since an overlay pins a composed frame exactly as long as it pins any other — and rejects
  layouts larger than the 512 MiB frame limit.
- Multi-adapter topologies currently return a structured unsupported error. A later slice must define
  cross-adapter transfer/synchronization, mixed-refresh freshness semantics, and mixed color/HDR
  behavior without adding unbounded copies or capture-engine initialization to the hotkey path.

### Validation

- Add unit and property tests for signed-coordinate transforms, rotation, region restoration, and
  virtual-desktop bounds.
- Exercise 1080p and 4K displays where available, 100/125/150/200 percent scaling, negative desktop
  coordinates, portrait rotation, mixed-DPI layouts, and hot-plugging.
- Verify pointer-display selection at boundaries and while the topology changes. Live selection now
  validates the capture engine's display list against the display-configuration generation *and*
  against a monitor-arrangement fingerprint that needs no window to notice a change, and rebuilds
  before placing the overlay, so this reduces to confirming pointer-boundary behavior; a stale
  arrangement can no longer reach the overlay, including on a daemon whose tray icon never started
  and on the first press after a dock event that landed during startup.
- Run lock/unlock, sleep/wake, Explorer restart, GPU-reset, Remote Desktop, and KVM/Synergy checks.

### Exit criteria

- Pointer, configured-display, and primary-display policies always capture the documented output.
- Regions and toolbar positions restore independently on each display.
- Portrait outputs produce correctly oriented pixels and coordinates.
- Display removal or rearrangement produces bounded recovery or an actionable error, never a hang.
- No display discovery or engine initialization occurs from hotkey receipt to native-frame readiness.

## Milestone 2 — Configurable and direct hotkeys

**Status:** Complete. The canonical action map, atomic registration, direct full-display capture,
validated repeat-last-region path, bounded fallback, and structured action logging shipped in PR #7.

**Outcome:** Frequent workflows can bypass unnecessary overlay interaction while retaining the warm
capture path and explicit failure behavior.

- Replace the single hard-coded chord with a validated action-to-hotkey map in `captastic.toml`.
- Support actions for:
  - opening the last-used workflow;
  - opening Region mode directly;
  - opening Window mode directly;
  - copying the active/full display immediately;
  - repeating the last confirmed region immediately without opening the overlay.
- Support configurable modifier/key combinations while retaining `MOD_NOREPEAT` behavior.
- Register the complete hotkey set atomically and report the exact conflicting chord and action.
- Reject duplicate or unsupported bindings during configuration validation.
- Route every hotkey through the same bounded trigger coordinator with the selected action encoded in
  its fixed-size event.
- For repeat-last-region, validate the saved display/topology generation before using the GPU region
  path; provide an actionable error or open Region mode when the saved target no longer exists.
- Record hotkey action, queue delay, materialization path, and outcome in existing metrics and logs.

### Exit criteria

- Every configured action registers, triggers only its assigned workflow, and shuts down cleanly.
- A conflicting system/global binding fails with a useful chord-specific diagnostic.
- Repeat-last-region performs no overlay construction and no disk/config read after the trigger.
- Direct full-display and last-region capture preserve the existing clipboard and latency boundaries.

## Milestone 3 — Windows Graphics Capture for windows

**Status:** Functional baseline complete. Captastic already falls back from `PrintWindow` to
programmatic WGC with bounded frame wait, D3D11 staging readback, and no occluded-desktop fallback.
Retained-session optimization and richer backend/fallback metrics remain quality follow-ups rather
than blockers for the next display milestone. The narrow window-capture backend trait described
below has not been introduced yet — window capture currently ships as free functions — so that
bullet remains open alongside the retention and metrics follow-ups.

**Outcome:** Window mode captures GPU-rendered and modern applications that do not render reliably
through `PrintWindow`.

- Introduce a narrow window-capture backend trait; do not turn it into a generic display screenshot
  abstraction.
- Move the existing bounded `PrintWindow` implementation behind that trait without changing current
  behavior.
- Implement Windows Graphics Capture with retained D3D11 resources, capture items, frame pools, and
  sessions owned by the Windows backend.
- Prefer WGC where it is supported and permitted, with `PrintWindow` retained as an explicit fallback.
- Define fallback ordering and terminal failures; never replace a failed native-window capture with
  an occluded desktop crop.
- Record backend choice, initialization, frame wait, GPU copy, readback, fallback reason, and result
  provenance in metrics and logs.
- Reuse captured GPU surfaces for chooser previews and perform full-resolution or selective readback
  only after confirmation.
- Preserve physical bounds, DPI behavior, alpha, rounded corners, resizing, closure, minimized state,
  and protected-content diagnostics.

### Exit criteria

- Eligible GPU-heavy windows that fail `PrintWindow` copy correctly through WGC when Windows permits
  capture.
- Window pixels remain independent of foreground occlusion.
- The chooser does not retain duplicate full-resolution CPU and GPU surfaces unnecessarily.
- Ordinary display/region capture performs no WGC window initialization.
- Backend and fallback provenance are visible in structured output and support logs.

## Milestone 4 — Asynchronous file output and capture history

**Status:** Complete. The pre-work landed first: `png` replaced the hand-rolled stored-DEFLATE
encoder, app-owned state moved out of `captastic.toml` into `state.toml`, an output-sink seam made
destinations independent, and a worker registry took over shutdown before a third worker existed.
The milestone itself then added the file worker, filename templates, capture history, and the
history commands. Every exit criterion below is met — three by test, and the latency criterion by
measurement (CPU-frame p50 1.065 ms with file output off against 1.060 ms with it on).

ADR 0002 was amended during this work: destinations are parallel tracks after CPU-frame readiness
rather than later stages of one pipeline, so each records its own trace. The two product decisions
this milestone deliberately left open (issue #44) were settled afterwards and are recorded in
ADR 0008: captures now decline both Windows clipboard-retention paths by default, and the default
filename names the window a capture came from. They point in opposite directions on purpose — the
question each time was what the user can undo, and clipboard retention is the one that cannot be.

**Outcome:** Captastic can save and revisit captures without adding disk or compression work to the
capture critical path.

- Activate the existing `[output]` configuration with clipboard-only, file-only, and
  clipboard-plus-file destinations.
- Add a bounded output worker that receives owned frames only after CPU-frame readiness or selection
  materialization.
- Implement PNG encoding, configurable screenshot directories, collision-safe atomic finalization,
  and explicit queue-full behavior.
- Support sanitized filename templates using timestamp, application, window title, display, mode,
  and dimensions.
- Record encode time, write time, bytes, destination, collision handling, and output failure
  separately from capture metrics.
- Add a bounded recent-capture history with configurable item/age/storage retention.
- Store only the metadata required for history navigation; never put raw pixels or clipboard contents
  in logs.
- Add **Open Last Capture**, **Show in Folder**, and history pruning commands before considering a
  larger history UI.
- ~~Make `output.format` a real choice rather than a value with one legal spelling.~~ **Done in
  v0.2.0** (PR #88): `png`, `jpeg` with `jpeg_quality`, or `bmp`, encoded through one entry point
  that asks the frame the same questions once; JPEG cannot carry alpha, so a window capture's
  transparent pixels are composited over opaque white and the configuration comment, the README,
  and a debug line all say so (ADR 0008 addendum). `captastic config validate` now also rejects a
  filename template the daemon would refuse at startup, and a one-shot `capture` honours `--config`.
- ~~Let the destination be switched without a restart.~~ **Done in v0.2.0** (PR #90):
  **Save Captures to Disk** in the notification area starts and stops the file worker on the daemon
  thread and writes the choice back to `output.enabled`, preserving the rest of the document. The
  rest of `[output]` is still read at startup. A runtime toggle makes a missed stop deadline
  repeatable rather than a once-per-process shutdown event, so ADR 0005 gains
  `DetachKind::FileOutputWorker` with a ceiling of one: while a written-off worker may still be
  writing into the output directory, starting file output again is refused — with the reason in the
  notification area — until its thread returns and gives the slot back. Captures queued when the
  destination stops are abandoned, but now named at warn level and counted rather than dropped
  silently.

### Exit criteria

- Encoding and file I/O never occur before frame readiness and never block capture or overlay threads.
- Clipboard success remains independent of file-output failure and vice versa.
- Filename input cannot escape the configured output directory.
- Retention remains bounded and is testable without depending on wall-clock sleeps.
- Enabling file output does not materially change native-frame or CPU-frame latency distributions.

## Milestone 5 — Capture quality and resilience

**Status:** Pre-work complete (issue #19); the milestone proper is not started. Timed-out capture
and window-render workers are counted against documented per-kind ceilings under ADR 0005, so the
handle and memory accounting the soak criteria depend on is explicit. The core pixel-format
contract covers 16-bit float and scRGB, and every sink refuses them by name rather than
misinterpreting their bytes — so the exit criterion that HDR input is never silently clipped is
already met for the destinations, and the remaining work is detecting HDR sources and deciding what
tone mapping should do. `FakeBackend` honours the freshness contract the recovery tests depend on.

The soak criteria are met on both backends, and the detach ledger is what made their results
readable. What is left to plan around is evidence rather than resilience: the sequence-marker
workload, deferred past v0.2.0, and the operator runs that the deferred published-claim criterion
now waits on — the fingerprint and repeat-artifact automation behind it shipped in v0.2.0. The live
lifecycle verifications that remain — sleep/wake, Remote Desktop, and the
`DEVICE_REMOVED`/`DEVICE_RESET` limb of GPU reset — are listed under
[Release readiness](#release-readiness--v010) because they are recommended before the tag.

**Outcome:** Captastic handles the remaining pixel formats and Windows lifecycle transitions with
explicit, tested behavior.

- ~~Add optional cursor composition, including DXGI pointer shapes, hotspots, visibility, clipping,
  and WGC-equivalent semantics.~~ **Done, after finding it had never once worked.** Two independent
  bugs sat between the request and the implementation: a guard left over from the native-frame
  milestone that rejected `CursorMode::Include` outright, and DXGI's incremental per-acquisition
  pointer reporting, whose unfilled defaults read as an invisible pointer at the origin. Every
  acquisition now feeds the pointer cache before anything decides whether to keep the frame, and
  `CursorAbsence::PositionNotYetKnown` separates "has not been told" from "has been told it is
  hidden"; rotation turned out to need no work at all, so `RotatedDisplayUnverified` is gone. Full
  narrative:
  [docs/investigations.md](docs/investigations.md#cursor-composition-and-the-two-bugs-behind-it).
- Complete rotation coverage discovered during the multi-monitor milestone.
- ~~Detect HDR/scRGB sources and implement a documented SDR clipboard/file tone-mapping policy.~~
  **Done** (ADR 0006): the compositor is asked for 8-bit BGRA and performs the conversion, so an HDR
  desktop is capturable and its screenshot matches what other tools produce. Captastic implements no
  curve of its own. Preserving high dynamic range end to end is deliberately not addressed and needs
  an output format that can carry it.
- ~~Investigate ICC/color-profile awareness and preserve color metadata where output formats support
  it.~~ **Done, and the answer was smaller than the question.** Captured pixels are sRGB by
  construction — the compositor converts, the encoder refuses anything else (ADR 0006) — so the
  metadata worth preserving is the tag saying so, not an embedded profile. Embedding the *display's*
  ICC profile would be actively wrong: it describes the panel, not the numbers in the file.

  PNG output now carries `sRGB` with the perceptual intent the specification names for photographic
  content, plus the `gAMA` and `cHRM` fallbacks so a decoder predating `sRGB` still gets the right
  transfer curve and primaries. The clipboard already declared it (`bV5CSType = LCS_sRGB`), so the
  same capture used to be self-describing through one destination and silent through the other.

  Not addressed, and out of scope until an output format can carry it: a wide-gamut or HDR source
  preserved end to end. That needs the pixels, not the tag (ADR 0006).
- Add recovery tests for display hot-plugging, sleep/wake, lock/unlock, GPU reset, Remote Desktop, and
  rapid session changes. Hot-plug, unplug and primary-promotion are covered deterministically
  through the daemon's rebuild seam; the no-source path is covered end to end (#56).
  **Lock/unlock is closed at both ends:** diagnosis by a live harness
  (`a_locked_session_explains_every_duplication_failure` in
  `crates/captastic-windows/src/session.rs`, `#[ignore]`d because it locks the machine for ~8 minutes
  and cannot unlock itself), and recovery through lock → displays asleep → unlock by
  `scripts/measure-lock-unlock-recovery.ps1`, which measured an unattended rebuild 0.6 s after the
  unlock ending a 229 s lock, with no adapter walk while it waited. Five call sites that could
  return a bare `PermissionDenied` — `duplicate_output`, `get_physical_cursor_position`,
  `QueryDisplayConfig`, `create_d3d11_device`, and the two window-capture backends — now share one
  session check in `session.rs`, so a denial the session can account for is reported as
  `DesktopUnavailable` naming the session state and the original error survives when it cannot;
  `create_d3d11_device` is the only one whose behaviour changed rather than its wording, and the
  three most recently converted denials are unit-tested but unreproduced on this host. Measurements
  and the denial survey:
  [docs/investigations.md](docs/investigations.md#lock-phases-and-the-denials-the-session-can-now-explain).
  GPU-reset recovery is now measured against real hardware for one of its two limbs: a driver
  restart took all three of the daemon's retained DXGI sessions away with `DXGI_ERROR_ACCESS_LOST`,
  and the daemon classified it, rebuilt all three, and finished the capture unattended in one
  attempt and about 1.5 s. `DEVICE_REMOVED`/`DEVICE_RESET` recovery stays unverified: no trigger
  available without elevation removes the device on this host, and the elevated adapter cycle that
  would has not been run. Sleep/wake and Remote Desktop still need a machine rather than a fake.
- Build the controlled sequence-marker workload for freshness, orientation, crop, and cursor tests.
  **Deferred past v0.2.0**, because every payoff test it enables needs a desk, a mouse and a
  rotatable display, and it gates no release claim; the seed a future codec should build on already
  exists, since `FakeBackend` stamps `request.id & 0xff` into every pixel it produces
  (`crates/captastic-core/src/fake.rs`), which is what makes "which frame was materialized"
  provable at all today.
- ~~Collect environment fingerprints and automate warm-up, raw artifacts, repeat runs, and
  compatible baseline comparison.~~ **Done** (PRs #87 and #91). Warm-up discard and `--repeat N`
  against a fresh backend per run were already there. Every report now carries an
  `EnvironmentFingerprint` identifying the host it describes — OS build, CPU, adapters with driver
  versions, displays with scale and refresh, session, power source, power plan, and the whole build
  including its dirty flag — and is deserializable, so a report can be a baseline rather than only
  a printout. `RunCompatibility` keys on that fingerprint, so two development builds a hundred
  commits apart no longer compare as the same software, and `HostMatch` lets a budget name the same
  facts.

  `--output-dir` writes the raw artifacts of a whole repeat set: `run-N.json` per run,
  `run-N.events.jsonl` when `--raw-events` is given (which under `--repeat` used to be accepted and
  silently do nothing), and a typed `repeated.json` carrying the set, its compatibility, its
  per-stage agreement at p50/p95/p99 for all five latency stages, and the budget verdict.
  `captastic benchmark compare <baseline> <candidate>` reads either shape back and reports the
  per-stage deltas with a `within_noise`/`slower`/`faster`/`unmeasurable` verdict, or refuses with
  exit status 2 naming every host fact that differs. The operator procedure that turns this into a
  publishable claim is [benchmarks/README.md](benchmarks/README.md).
- ~~Enforce relative and absolute performance budgets only on a documented physical benchmark host;
  hosted CI should continue enforcing correctness rather than GPU timing.~~ **Mechanism done.**
  `captastic benchmark --budgets benchmarks/budgets.toml` judges a run, and a budget names the host
  it describes: a run anywhere else is skipped *loudly* — every mismatch named, the measurements
  still reported, exit status unchanged — because a GPU budget evaluated on a CI runner fails every
  time and a check that always fails is one nobody reads. A breach on a matching host fails the
  command. The relative budgets are populated, since a ratio needs no calibration; the absolute
  ceilings are deliberately left unset until measured on the host from a console session, because
  an invented threshold either passes trivially or gets deleted.

### Exit criteria

- Cursor-on and cursor-off output are pixel-correct and separately measured. **Pixel-correctness is
  met**, which first required fixing composition (above). The check compares a cursor-on and a
  cursor-off capture of the *same retained frame*, so composition is the only difference between
  them rather than two moments of a desktop that repainted in between — which is what makes
  "the change is confined to the pointer" provable at all. The latest run differed in 280 of the
  2,304 pixels inside the reported 48×48 pointer rectangle and in **none** outside it: an arrow with
  transparent corners, blended where the pointer is and nowhere else.

  **Separately measured is not met, and the figures that claimed it were wrong.** Two cursor-on
  benchmark runs were reported at +171 and −157 microseconds before per-capture outcome counting
  showed that all 600 captures in each had returned `absent_not_visible` — they had measured
  cursor-off twice under another name. Those counts are now part of every benchmark report precisely
  so that a run which composited nothing says so instead of producing a plausible number. A real
  figure needs the pointer visible for a whole run, which depends on whether a human is touching the
  mouse, and a 2,304-pixel blend against an 8.3-megapixel readback sits well below the 1.7–6.6%
  run-to-run spread. The measurement worth building is therefore of the blend itself, which the code
  already times at debug level, and not of the pipeline around it.
- HDR input never produces silently clipped or incorrectly tagged SDR output. **Met:** the sinks
  refuse what they cannot describe (#41) and the capture path no longer produces it (ADR 0006), so
  there is no path by which wide-gamut samples reach an 8-bit destination unconverted. Unverified on
  an HDR display, which is what would be needed to judge the compositor's conversion rather than
  merely confirm one happened.
- ~~A 1,000-capture acceptance soak and 10,000-capture endurance soak show no unbounded handle or
  memory growth.~~ **Met** for both, on the fake backend with clipboard and file output enabled.
  Acceptance (2026-08-16, 1,000 captures): handles, GDI and USER objects exactly flat at 186/10/9,
  private bytes plateauing at 4.77 MB for the final 440 captures. Endurance (2026-08-17, 10,000
  captures over 20 minutes, 83 samples): GDI exactly 10 and USER exactly 9 at every sample, kernel
  handles in a 186–188 band with no trend, private bytes flat from the first quarter onward
  (Q1 mean 4.83 MB, Q4 mean 5.40 MB).

  The DXGI side is **also met**, established across four runs on 2026-08-17 and 2026-08-18 after a
  first 9-minute leg showed one unexplained step of +22 GDI, +21 USER and +65 handles (#53). Rather
  than repeat that run for longer — a step that is flat afterwards is one-time initialisation, and a
  longer run mostly re-observes it — each suspect was removed in turn: 4,513 captures with both
  destinations off, 5,359 with the clipboard on at 250 ms, 2,548 with both destinations on including
  141 `BufferExhausted` refusals, and a verified monitor sleep and a verified lock transition.
  **12,670 captures across 81 minutes with GDI at exactly 10 in every sample of every run.** #53 is
  closed as not reproducible; the harnesses are `scripts/soak-resource-step.ps1` and the two probes
  beside it, `scripts/probe-display-power.ps1` and `scripts/probe-session-lock.ps1`. Run-by-run
  detail:
  [docs/investigations.md](docs/investigations.md#the-4k-dxgi-resource-step-53-narrowed).

  Those runs also measured the sustainable rate for large frames, which nothing had, and corrected
  what it means. At 250 ms with 8.3 MP frames going to clipboard **and** file, 787 of 2,000 attempts
  were refused with `BufferExhausted`; with the clipboard alone at the same rate, **none of 5,359
  were**. The refusals are not a frame-rate ceiling but the two destinations contending for the same
  three CPU slots, and the file worker holds its lease across a `Compact` encode plus the write —
  far longer than the clipboard holds one. The bound behaves as designed and reports every refusal;
  the figure to quote is per destination set, not per interval.
- Three compatible repeat runs support every published performance claim. **The automation now
  exists; the runs are an operator step.** `captastic benchmark --repeat 3 --output-dir …` produces
  the three runs, proves they are compatible by fingerprint rather than by assertion, reports every
  stage's spread, and writes the artifacts a claim would be committed with; `benchmark compare`
  holds a later run against them and refuses when the hosts differ. What is left is not code: three
  accepted sets have to be measured on the documented host from a console session on AC power with
  the display repainting, per [benchmarks/README.md](benchmarks/README.md), and no absolute
  performance claim is published until they are. v0.2.0 publishes none, so this stays satisfied
  vacuously — but now by choice rather than for want of a way to satisfy it properly.

## Milestone 6 — Annotation and pinning

**Status:** Not started, and no longer gated. The overlay/tray message-state-machine extraction it
waited on is complete, so the compose/render layer and window-shell utilities annotation needs now
exist as separable modules. Remains behind Milestones 1 and 5 by priority, not by dependency.

**Outcome:** Optional post-capture tools add communication value without changing Captastic's
capture-engine identity.

- Keep annotation in a downstream editor/model that receives an already owned frame.
- Support crop adjustment, arrows, rectangles, text, highlights, and destructive redaction.
- Copy or save the annotated result through the existing output workers.
- Add an always-on-top pinned capture window with explicit close, opacity, and click-through controls.
- Keep annotation resources lazy and absent from direct clipboard/file hotkeys.
- Define whether editing is modal or concurrent before adding capture-history integration.

### Exit criteria

- Unedited capture metrics and memory use are unchanged when annotation is not invoked.
- Redaction modifies exported pixels rather than storing a reversible overlay.
- Pinned windows cannot be mistaken for capture candidates unless explicitly requested.

## Milestone 7 — Native macOS and Linux implementations

**Status:** Not started. Before the first non-Windows backend, extract session-oriented platform
seams (overlay session, hotkey source, tray port) from the by-then-stable Windows call sites and
move the selection/clipboard worker logic out from under `cfg(windows)` so the pipeline compiles
and tests on every CI leg; port the contracts, not the Windows behaviors.

**Outcome:** Extend Captastic's contracts through independent native backends rather than presenting
platforms as equivalent when they are not.

### macOS

- Use ScreenCaptureKit with a retained stream/session, permission diagnostics, native overlay,
  platform hotkeys, CPU-frame normalization, and pasteboard output.
- Map latest/fresh-equivalent behavior honestly and document timing provenance the API can supply.
- Compile in hosted macOS CI while running consent/capture tests on a logged-in machine.

### Linux Wayland

- Use XDG Desktop Portal and PipeWire with explicit user consent and restore tokens where supported.
- Document compositor-dependent global-hotkey behavior and fallbacks.
- Keep portal and PipeWire state inside the Linux backend.

### Linux X11

- Implement X11 capture and window enumeration separately, comparing plain image capture with shared
  memory extensions.
- Implement desktop-specific hotkeys and clipboard ownership for the lifetime required by consumers.
- Keep Wayland and X11 benchmark results separate.

### Decision gate

After capture quality and history are stable, choose annotation/pinning or the first cross-platform
proof based on the intended audience. Neither should delay the Windows workstation milestones.

## Milestone 8 — Precision region selection

**Status:** Complete (v0.2.0, PRs #89 and #92). Shipped with its exit criteria met by test and
unobserved on a desktop; the manual matrices are listed under
[Release readiness — v0.2.0](#release-readiness--v020).

**Outcome:** A region can be placed on the exact pixel the user means, over a picture that holds
still when the desktop will not.

- Region edges snap to the visible frames of the windows on that display, to the work area, and to
  the display itself, within 8 DIPs scaled for the monitor's DPI so the reach is the same physical
  distance at every scaling. A snapped edge is the target's exclusive edge — a region snapped to a
  window's right border ends on that window's last pixel column, never one short — and an accent
  hairline along the target says what it caught. The nearest edge wins, ties go to the topmost
  window, and the display can only win when nothing else is in reach. **Ctrl** suppresses snapping
  for the rest of a drag; **Options -> Snap to Edges** turns it off for good.
- The arrow keys nudge the region one physical pixel, ten with **Shift**, and resize its right and
  bottom edges with **Ctrl**. Nudging never snaps, which is how an edge reaches a pixel no window
  sits on.
- A magnifier shows the 31 physical pixels around the pointer enlarged six times (more at higher
  scaling), with a grid, the pointer's pixel outlined, the selection's edges drawn on the exact
  boundaries they occupy, and the desktop coordinate underneath. It is held up on **Z** and, in its
  default mode, also appears by itself when the pointer slows to a deliberate pace during a drag.
  It samples whichever view is showing, so it never enlarges pixels the capture will not contain,
  and it never covers the pixels it is magnifying. **Options -> Zoom** cycles Auto, Hold Z, and Off.
- Every overlay press captures the screen at the press. The overlay opens in the view
  `selection.preview` names and **F** or **Options -> View** switches between the live desktop and
  the frame from the press, with a **FROZEN · pixels from hotkey press** tag while the latter is
  showing; confirmation materializes from the view that was on screen. One layered window now
  serves both views, so `WDA_EXCLUDEFROMCAPTURE` covers a `frozen`-configured overlay that was
  never excluded before, and a failed first present falls back in place instead of re-dispatching
  the whole selection across threads (ADR 0004 amendment).
- The snap and zoom preferences are global in `state.toml` (`snap_to_windows`, `region_zoom`),
  written with `skip_serializing_if` and no `STATE_SCHEMA_VERSION` bump, so an older binary that
  refuses the newer file loses that run's remembered layout and nothing else. The view toggle is
  deliberately not persisted and has no configuration knob: which view suits a capture is a
  property of that capture.

### Exit criteria

- A snapped edge lands on the target's exact pixel column or row and the guide describes the edge
  the region really sits on. **Met by test** — unit and property coverage pins the half-open
  relationship, including the case where the minimum-size clamp invalidates a guide and it is
  dropped. **Not yet observed live** (PR #89 checklist 1–4).
- The keyboard can place an edge the pointer cannot, and never fights the snap that put it there.
  **Met by test** — nudges of any length and order leave a legal region, never snap, and stop
  repainting at a display edge. **Not yet observed live** (PR #89 checklist 5–6).
- The magnifier shows the pixels the capture will contain, stays on the monitor, and never covers
  its own source square. **Met by test** at all four scalings, including a real GDI blit proving
  the nearest-neighbour enlargement invents no colour. **Not yet observed live**, and the live-view
  sample coming back with no trace of the overlay in it is the one check nothing here substitutes
  for (PR #89 checklist 7–10).
- The user can stop the picture without losing the press, and gets the picture they were looking
  at. **Met by test** — the fake backend stamps `request.id & 0xff` into every pixel, so a
  frozen-view confirmation is proved to crop the capture taken at the press and a live-view
  confirmation the capture taken at the confirmation. **Not yet observed live** (PR #92
  checklist 1–6).
- Both preferences survive a daemon restart and the view toggle does not. **Met by test** at the
  store and the live-cache level, including a display this process has never drawn on. **The full
  round trip through a running daemon is not yet observed** (PR #89 checklist 11, PR #92
  checklist 7).

## Architecture hardening backlog

These items were deliberately separated from the code-review remediation branch because each
changes a public contract or a broad subsystem boundary. They should land as independently reviewed
branches before the affected subsystem grows further.

### Extract overlay and tray message state machines

**Status:** Complete. Overlay and tray transitions are pure functions with table-driven coverage,
`overlay.rs` is decomposed along tested ownership boundaries into window enumeration, the software
rasterizer, and the win32 window shell, and native procedures now translate messages and apply
declared effects rather than owning product state.

**Why:** Recent defects arose from reentrant Win32 messages mutating state that was also being
consumed by the originating handler. The large message procedures made transition coverage and
teardown reasoning unnecessarily difficult.

- Extract drag ownership, capture loss, button release, and toolbar persistence decisions into a
  pure overlay transition function.
- Extract tray session-ending, callback reentrancy, and notification-routing decisions into a pure
  transition function.
- Keep Win32 procedures responsible for translating messages and applying declared effects, not for
  owning product state transitions.
- Add table-driven coverage for normal ordering, self-generated `WM_CAPTURECHANGED`, externally
  stolen capture, canceled shutdown, committed session shutdown, and callback reentrancy.
- Decompose `overlay.rs` only along tested ownership boundaries; avoid a mechanical file split that
  preserves hidden coupling.

**Exit criteria:** Every drag and session-shutdown transition is testable without creating a native
window, native handlers contain no duplicated state cleanup, and reentrant messages cannot consume
state required by their caller.

### Resolve dormant configuration and telemetry surfaces

**Status:** Largely complete. Configuration validation now rejects non-default values for
`[output]`, `hotkey.repeat = "coalesce"`, `capture.cursor = "include"`, and non-default
`capture.buffer_slots` with actionable not-implemented-yet errors, covered by unit tests, and the
write-only `backend_duration` field was removed from the capture outcome. Each rejection lifts when
its milestone implements the feature — `[output]` accordingly became live in Milestone 4. The legacy
UI-state save/load entry points superseded by `UiStateStore` were removed rather than inventoried,
since they wrote UI state into the user's configuration file, which that work stopped doing.

**Why:** Accepted-but-unused settings create false product contracts and make cross-platform
backends inherit behavior that does not exist. Write-only telemetry has the same maintenance cost
without diagnostic value.

**Exit criteria:** Every documented configuration value changes observable behavior, every public
metric has a consumer and timing definition, and platform backends do not need to emulate dead
Windows-era surfaces.

### Decide the mouse-capture and software-KVM contract

**Why:** Win32 mouse capture reliably completes drags outside the overlay, but software KVM and
mouse-sharing tools may depend on retaining input ownership. This is a product interoperability
decision, not an implementation-only cleanup.

- Test toolbar, draw, move, and resize drags with the supported multi-monitor matrix and at least
  two representative software-KVM/input-sharing products.
- Compare the current active-drag-only `SetCapture` design with a capture-free design that infers
  release/cancellation from raw input, polling, or focus/cursor transitions.
- Record the chosen behavior in an ADR, including behavior at overlay boundaries, capture theft,
  remote sessions, and disconnected KVM peers.
- If Win32 capture remains, document tested compatibility and retain bounded self-release and
  external-capture-loss state-machine tests. If capture-free behavior wins, remove `SetCapture`
  without regressing cross-monitor drag completion.

**Exit criteria:** The selected input-ownership contract is validated on real software-KVM and
multi-monitor setups, documented as an explicit product choice, and enforced by transition tests.

## Release signing and distribution backlog

Authenticode is deliberately not on the near-term critical path. Until signing is provisioned:

- Continue publishing deterministic packages with SHA-256 checksums and clear unsigned-release
  documentation.
- Do not store a long-lived PFX/private key in repository or ordinary CI secrets.
- Keep the release workflow structured so signing can be inserted after final build and before
  packaging.
- When ready, choose a public-trust provider, use HSM/managed signing, apply an RFC 3161 SHA-256
  timestamp, and fail releases whose signatures do not verify.
- Evaluate MSI/MSIX packaging and automatic updates only after signing identity and release channels
  are stable.

## Recommended implementation order

Rotated-output normalization and same-adapter virtual-desktop composition shipped in PRs #8–#9, and
the 2026-08 code, architecture, and roadmap review is fully remediated — its findings and their
disposition are recorded in
[docs/investigations.md](docs/investigations.md#the-2026-08-code-architecture-and-roadmap-review).
The order below reflects what remains.

1. Ship v0.2.0: the manual checklists from PRs #89, #90 and #92, the optional benchmark claim run,
   then `release/v0.2.0` to `main`, the tag, the disposable-VM check, the manual `choco push`, the
   back-merge, and the bump to 0.3.0. First because it is time-bound and because everything the
   release exercises — the checklists especially — is exercised by nothing else.
2. Complete Milestone 1 (multi-adapter composition and the hardware validation matrix). The only
   milestone in progress, and the one whose remaining work needs hardware rather than design.
3. Finish Milestone 5's evidence work, which is now measurement rather than code: the operator
   claim runs from [benchmarks/README.md](benchmarks/README.md), after which the absolute budget
   ceilings can be set and a figure can be published. The sequence-marker workload and Milestone 3's
   window-capture backend trait stay deferred — the first needs a desk, a mouse and a rotatable
   display and gates no release claim; the second changes no user-visible contract.
4. Settle the outstanding design decisions — latest-mode currency on an idle desktop,
   `fresh` + `virtual_desktop`, and control-event hardening. Milestone 4's two product questions are
   answered (ADR 0008), and the mouse-capture/software-KVM contract is settled in ADR 0009: each use
   takes the pointer source that answers its own question, and Captastic promises nothing about
   where a foreign owner has put the mouse.
5. Add annotation/pinning (Milestone 6, now ungated) or cross-platform work (Milestone 7), based on
   audience demand (existing decision gate).

### Lifecycle recovery: what the investigation settled

The daemon no longer exits when enumeration finds no displays: locked, disconnected, asleep and
unplugged all reach DXGI as an empty output list, so they share one `DesktopUnavailable` "not now"
kind, and the daemon waits, registers its hotkeys, and builds the capture engine when a display
appears (verified end to end on 2026-08-17 with an injected blackout). What ends duplication is not
the lock screen owning the desktop but the **display power-down**, and `WTSSessionInfoEx` is the
only signal that answers in every lock phase, so it is what the session probe keys on. The
narrative, including the two claims that were corrected along the way and the #51 condition still
not reproduced on demand, is in
[docs/investigations.md](docs/investigations.md#lifecycle-recovery-a-daemon-with-nothing-to-capture).

## Recommended next branch

`release/v0.2.0`, cut from `dev` once the manual checklists have been run. Everything v0.2.0
contains is merged; the branch exists only to carry the release through `main`, the tag, and the
merge back to `dev` as described under
[Release readiness — v0.2.0](#release-readiness--v020), after which the workspace version moves to
the 0.3.0 line.

Before cutting it, the checklists are the work most likely to find something, because this is a
release about overlay behaviour and none of that behaviour has been watched working: snapping and
its guide, the arrow keys, the magnifier — the live-view sample above all — the two new Options
rows, the `F` toggle and its tag, and the notification-area file-output toggle with a capture
landing on disk after it. Each is a table in
[docs/overlay-ui-verification.md](docs/overlay-ui-verification.md) or a numbered list in its pull
request. The operator-run live verifications from v0.1.0 — sleep/wake, Remote Desktop, and the
`DEVICE_REMOVED`/`DEVICE_RESET` limb of GPU-reset recovery through the elevated adapter cycle — are
still outstanding and still deferrable, and the benchmark claim run is optional and gates only
whether the notes may quote a number.

Then the release itself: the release branch to `main` by pull request, the tag on `main`, the
disposable-VM checklist, the manual `choco push` once the release URLs and hashes verify, the
pull request merging the release branch back to `dev`, and the version bump.

FakeBackend fidelity keeps its leverage beyond this milestone and is worth extending as gaps appear:
much of Milestone 4 shipped with daemon-side behaviour verified only by unit tests, because a second
daemon cannot start while one holds the session control event, and a fake backend that honestly
reproduces the real contract is what would close that gap for everything after it.

Milestone 1's remaining work is the higher priority by roadmap order, but it is gated on hardware
rather than effort: multi-adapter composition cannot be honestly validated on a single-adapter,
single-monitor machine. Schedule it when the hardware matrix is available.
