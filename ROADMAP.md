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
than blockers for the next display milestone.

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

**Outcome:** Captastic handles the remaining pixel formats and Windows lifecycle transitions with
explicit, tested behavior.

- Add optional cursor composition, including DXGI pointer shapes, hotspots, visibility, clipping, and
  WGC-equivalent semantics.
- Complete rotation coverage discovered during the multi-monitor milestone.
- Detect HDR/scRGB sources and implement a documented SDR clipboard/file tone-mapping policy.
- Investigate ICC/color-profile awareness and preserve color metadata where output formats support it.
- Add recovery tests for display hot-plugging, sleep/wake, lock/unlock, GPU reset, Remote Desktop, and
  rapid session changes.
- Build the controlled sequence-marker workload for freshness, orientation, crop, and cursor tests.
- Collect environment fingerprints and automate warm-up, raw artifacts, repeat runs, and compatible
  baseline comparison.
- Enforce relative and absolute performance budgets only on a documented physical benchmark host;
  hosted CI should continue enforcing correctness rather than GPU timing.

### Exit criteria

- Cursor-on and cursor-off output are pixel-correct and separately measured.
- HDR input never produces silently clipped or incorrectly tagged SDR output.
- A 1,000-capture acceptance soak and 10,000-capture endurance soak show no unbounded handle or
  memory growth.
- Three compatible repeat runs support every published performance claim.

## Milestone 6 — Annotation and pinning

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

1. Finish rotated-output normalization and virtual-desktop composition.
2. Complete WGC retention/provenance follow-ups.
3. Add asynchronous file output and capture history.
4. Build capture-quality completeness and performance evidence.
5. Add annotation/pinning or cross-platform work, based on audience demand.

## Recommended next branch

Use `feature/8/normalize-rotated-displays` and normalize rotated single-display outputs before
starting virtual-desktop composition:

1. Define and test raw-texture to top-left BGRA transforms for 0, 90, 180, and 270 degrees.
2. Apply the transform directly into pooled CPU readback buffers without adding an intermediate
   full-frame allocation.
3. Map normalized region coordinates back into raw DXGI texture coordinates, then normalize the
   copied GPU region before publication.
4. Verify negative desktop origins, per-display saved regions, direct hotkeys, and dimension
   metadata remain in normalized physical pixels.
5. Follow with same-adapter virtual-desktop bounds and composition, then explicitly support or reject
   multi-adapter and mixed-mode layouts.
