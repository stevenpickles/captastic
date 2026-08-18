# Captastic roadmap

Captastic now has a fast Windows-native DXGI capture engine, clipboard output, region and window
selection, persistent UI state, a notification-area desktop experience, current-user installation,
and release packaging. The next work should make Captastic more useful on real workstations while
preserving the low-latency, native design.

This roadmap is ordered by user impact and architectural dependency. Authenticode signing remains
important for a future public release, but it does not gate the capture milestones below.

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
  output uses three reusable CPU slots and rejects layouts larger than the 512 MiB frame limit.
- Multi-adapter topologies currently return a structured unsupported error. A later slice must define
  cross-adapter transfer/synchronization, mixed-refresh freshness semantics, and mixed color/HDR
  behavior without adding unbounded copies or capture-engine initialization to the hotkey path.

### Validation

- Add unit and property tests for signed-coordinate transforms, rotation, region restoration, and
  virtual-desktop bounds.
- Exercise 1080p and 4K displays where available, 100/125/150/200 percent scaling, negative desktop
  coordinates, portrait rotation, mixed-DPI layouts, and hot-plugging.
- Verify pointer-display selection at boundaries and while the topology changes.
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

The first exit criterion to plan around is the soak. It is the one that needs a machine left alone
for a long time rather than a design decision, and the detach ledger is what makes its result
readable.

**Outcome:** Captastic handles the remaining pixel formats and Windows lifecycle transitions with
explicit, tested behavior.

- Add optional cursor composition, including DXGI pointer shapes, hotspots, visibility, clipping, and
  WGC-equivalent semantics.
- Complete rotation coverage discovered during the multi-monitor milestone.
- ~~Detect HDR/scRGB sources and implement a documented SDR clipboard/file tone-mapping policy.~~
  **Done** (ADR 0006): the compositor is asked for 8-bit BGRA and performs the conversion, so an HDR
  desktop is capturable and its screenshot matches what other tools produce. Captastic implements no
  curve of its own. Preserving high dynamic range end to end is deliberately not addressed and needs
  an output format that can carry it.
- Investigate ICC/color-profile awareness and preserve color metadata where output formats support it.
- Add recovery tests for display hot-plugging, sleep/wake, lock/unlock, GPU reset, Remote Desktop, and
  rapid session changes. Hot-plug, unplug and primary-promotion are covered deterministically
  through the daemon's rebuild seam; the no-source path is covered end to end (#56). Sleep/wake,
  GPU reset and Remote Desktop still need a machine rather than a fake.
- Build the controlled sequence-marker workload for freshness, orientation, crop, and cursor tests.
- Collect environment fingerprints and automate warm-up, raw artifacts, repeat runs, and compatible
  baseline comparison.
- Enforce relative and absolute performance budgets only on a documented physical benchmark host;
  hosted CI should continue enforcing correctness rather than GPU timing.

### Exit criteria

- Cursor-on and cursor-off output are pixel-correct and separately measured.
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

  A DXGI leg followed (2026-08-17, 2,000 attempts at 3840×2160 to both destinations, 9 minutes):
  memory flat throughout at ~248 MB private, and the counters flat for the final six minutes — but
  with one unexplained step of +22 GDI, +21 USER and +65 handles at the three-minute mark, tracked as
  #53. Nine minutes is not long enough to know whether that step recurs, so the DXGI side of this
  criterion is **not** claimed as met.

  That run also measured the sustainable rate for large frames, which nothing had: at 250 ms with
  8.3 MP frames going to clipboard and file, 787 of 2,000 attempts were refused with
  `BufferExhausted` — three CPU frame slots against a ~13 ms readback plus a 33 MB clipboard payload
  and a 3 MB PNG per capture. The bound behaves as designed and reports every refusal; the number is
  worth knowing before promising a capture rate.
- Three compatible repeat runs support every published performance claim.

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

Rotated-output normalization and same-adapter virtual-desktop composition shipped in PRs #8–#9.
The 2026-08 code, architecture, and roadmap review is fully remediated: the three confirmed High
defects and the silent-drop, sticky-failure, config-write and dormant-surface findings were fixed on
the review branch, and the Medium findings that were deferred into batched issues — window-capture
alpha and geometry parity, timeout budgets, worker exhaustion, log-rotation coexistence, the
clipboard's stored-DEFLATE encoder, unlocked state writes, and the unreachable schema error — closed
alongside the overlay extraction and Milestone 4. The order below reflects what remains.

1. Complete Milestone 1 (multi-adapter composition and the hardware validation matrix). The only
   milestone in progress, and the one whose remaining work needs hardware rather than design.
2. Build capture-quality completeness and performance evidence (Milestone 5), starting with its
   pre-work: a documented detach budget, FakeBackend contract fidelity, and the pixel-format
   extension.
3. Settle the outstanding design decisions — latest-mode currency on an idle desktop,
   `fresh` + `virtual_desktop`, and control-event hardening. Milestone 4's two product questions are
   answered (ADR 0008), and the mouse-capture/software-KVM contract is settled in ADR 0009: each use
   takes the pointer source that answers its own question, and Captastic promises nothing about
   where a foreign owner has put the mouse.
4. Add annotation/pinning (Milestone 6, now ungated) or cross-platform work (Milestone 7), based on
   audience demand (existing decision gate).

### The 4K DXGI resource step (#53), narrowed

A 40-minute DXGI soak at 3840×2160 with the clipboard and file output **off** — 4,513 captures, no
errors, no refusals, display sleep suppressed so it could not confound the counters — held GDI at
exactly 10 for all 478 samples, USER within one, handles within a ±5 band netting −3, and private
bytes within 0.3 MB. An 8-minute idle control was equally flat.

Two further legs restored the rest of the original configuration: the clipboard alone at 250 ms
(5,359 captures, no refusals), then both destinations at 250 ms (2,548 captures, 2,407 files,
4.17 GB, **141 `BufferExhausted` refusals**). GDI held at exactly 10 in every sample of all three
runs — 12,420 captures across 79 minutes — against an original that stepped 10 → 33 and held.

Every Captastic-side suspect is therefore exonerated: the capture path, both destinations, and the
refusal path. What differed in all three runs is display power, uncontrolled in the original and
suppressed here, and it fits the shape exactly — a one-time event allocating a batch of GDI and
USER objects and then holding flat, unrelated to capture volume.

A fourth run powered the display down for 30 seconds with captures running — the monitor entering
sleep confirmed by observation rather than inferred — and GDI and USER did not move at all. That
eliminates the last suspect: **12,472 captures across four runs and 81 minutes, with GDI at exactly
10 in every sample of every run.** The step in #53 is not reproducible under any Captastic-side
condition, and the remaining candidate is the lock transition that occurred during the original
soak, which has never been measured with these counters.

Two incidental findings. The refusals were never "250 ms is too fast for 4K": both destinations
lease from the same three-slot CPU pool and the file worker holds its lease across a `Compact`
encode plus the write, so the refusals are the two destinations contending — the clipboard alone at
that rate refused none of 5,359. And Desktop Duplication keeps producing full-resolution frames
while the monitor is asleep, so a sleeping display is still capturable and does not detach from
enumeration.

### Lifecycle recovery: a daemon with nothing to capture

The daemon no longer exits when enumeration finds no displays. Locked, disconnected, asleep or
unplugged all reach DXGI as an empty output list, so they share one kind — `DesktopUnavailable`,
"not now" — and the daemon waits, registers its hotkeys, and builds the capture engine when a
display appears. Verified end to end on 2026-08-17 with an injected blackout
(`CAPTASTIC_TEST_NO_DISPLAYS_MS`, debug builds only): start with nothing attached, seven triggers
refused with an accurate reason, engine built unattended, 4K captures following.

The measurement that shaped it is worth keeping, with a later correction. A lock does **not** stop
enumeration: displays enumerate with their persistent identities throughout, so a lock is not what
produced the empty display list in #51, and a fix keyed on the lock would have missed the failure it
was filed about. That original condition — an empty list *and* a denied `QueryDisplayConfig` — has
still not been reproduced on demand.

The first run of that test also had a fresh daemon build a duplication 0.3 s after the lock engaged,
which led to the overly strong claim that a lock does not break DXGI at all. A later run with a
daemon already holding a duplication showed otherwise: at the lock it takes `AccessLost` (`the keyed
mutex was abandoned`), and the rebuild is refused with `DesktopUnavailable in dxgi/duplicate_output:
the session is locked or a secure prompt owns the desktop`. It recovered by itself two seconds later
at the unlock. So duplication *is* refused while the lock screen owns the desktop; there is simply a
brief transitional window in which it can still be acquired.

## Recommended next branch

Milestone 5's lifecycle-recovery and soak work, now that its pre-work (issue #19) is complete.
Display hot-plug, sleep/wake, lock/unlock and GPU reset are the tests most likely to find something,
because they exercise the recovery paths that shipped with the least live verification — and they
are testable on this machine, unlike the multi-adapter work and unlike HDR tone mapping, which needs
an HDR display to judge rather than merely to compile.

FakeBackend fidelity keeps its leverage beyond this milestone and is worth extending as gaps appear:
much of Milestone 4 shipped with daemon-side behaviour verified only by unit tests, because a second
daemon cannot start while one holds the session control event, and a fake backend that honestly
reproduces the real contract is what would close that gap for everything after it.

Milestone 1's remaining work is the higher priority by roadmap order, but it is gated on hardware
rather than effort: multi-adapter composition cannot be honestly validated on a single-adapter,
single-monitor machine. Schedule it when the hardware matrix is available.
