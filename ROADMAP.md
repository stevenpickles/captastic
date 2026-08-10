# Captastic roadmap

Captastic now has a Windows-first native capture engine, clipboard output, region and window
selection, persistent UI state, a notification-area desktop experience, current-user installation,
and release packaging. The next work should turn that capable prototype into a trustworthy Windows
release before broadening the product or adding another platform.

This roadmap is ordered by dependency and risk rather than feature count. Each milestone should
land through focused branches with tests, updated documentation, and a runnable acceptance command
or checklist.

## Product principles

- Keep disk, network, compression, configuration parsing, and resource initialization outside the
  hotkey-to-frame critical path.
- Measure native-frame, CPU-readback, selection, clipboard, encoding, and file output as separate
  stages.
- Prefer explicit native backends over a lowest-common-denominator screenshot abstraction.
- Never hide freshness, fallback, permission, or capture-source differences behind one latency
  number.
- Add product surface only when shutdown, recovery, privacy, and failure behavior are defined.

## Milestone 1 — Windows release candidate

**Outcome:** Publish a signed, reproducible Windows release candidate with documented behavior on
the display configurations Captastic intends to support.

### Release trust and distribution

- Select a public-trust Authenticode provider: apply to SignPath Foundation first, with Microsoft
  Artifact Signing as the managed fallback.
- Sign `captastic.exe` and `captastic-desktop.exe` after the final build and before packaging.
- Apply an RFC 3161 SHA-256 timestamp and fail the release if `signtool verify /pa` fails.
- Restrict signing to protected release tags or an approval-gated GitHub environment.
- Declare the minimum supported Windows version and architecture before the release candidate;
  validate every environment in that support policy.
- Continue publishing SHA-256 package checksums; add provenance/attestation and an SBOM when the
  signing workflow is stable.
- Produce and test a `v0.1.0-rc.1` portable package before the first stable tag.

### Windows acceptance matrix

- Exercise 1080p and 4K displays where available, 100/125/150/200 percent scaling, negative desktop
  coordinates, portrait rotation, mixed-DPI displays, and display hot-plugging.
- Verify full-display, region, and window capture on every supported topology.
- Verify lock/unlock, sleep/wake, Explorer restart, GPU/driver reset recovery, and KVM/Synergy use.
- Repeat installation, in-place upgrade, startup registration, and uninstall from a packaged build.
- Validate icon contrast on light and dark Windows themes; add a tray-specific or theme-aware asset
  if one monochrome resource cannot remain legible.

### Correctness and stability gates

- Add a controlled sequence-marker test pattern for pixel freshness, orientation, and crop accuracy.
- Normalize rotated DXGI outputs or keep them explicitly rejected with an actionable diagnostic
  until normalization is complete.
- Run a 1,000-capture acceptance soak and a 10,000-capture endurance soak while sampling process
  memory, handles, failures, retries, and dropped triggers.
- Verify clipboard contention, access loss, display topology changes, and shutdown during an active
  overlay.
- Reconcile the implementation specification checkboxes with current code so the checklist is
  evidence-backed rather than historical.

### Exit criteria

- All hosted CI jobs pass from a clean checkout.
- The packaged install/upgrade/uninstall checklist passes on every declared supported Windows
  release.
- The supported display matrix has recorded results and no unexplained pixel-orientation failures.
- Soak tests show no unbounded handle or memory growth.
- Both release executables carry valid timestamped Authenticode signatures.

## Milestone 2 — Reliable native window capture with WGC

**Outcome:** Window mode works for GPU-heavy and modern applications that do not render reliably
through `PrintWindow`, without regressing the fast DXGI display path.

- Implement Windows Graphics Capture as a distinct retained native backend, not branches inside the
  DXGI implementation.
- Retain the D3D11 device, frame pool, capture item, and session for the active window workflow.
- Preserve physical bounds, alpha, rounded corners, minimized/closed-window behavior, and explicit
  permission/support diagnostics.
- Define capability routing: DXGI remains the measured display/region path while WGC handles windows
  that need compositor-native capture.
- Keep `PrintWindow` as a bounded compatibility path only where it produces correct pixels.
- Never replace a failed native-window capture with an occluded desktop crop.
- Add correctness tests covering occlusion, resizing, DPI transitions, application closure, and
  unsupported/protected content.

### Exit criteria

- Previously unsupported eligible windows copy correctly when WGC permits capture.
- Window results remain independent of foreground occlusion.
- Backend choice, fallback, failure, and timing provenance are visible in logs and structured output.
- Ordinary region capture performs no WGC initialization or window-enumeration work.

## Milestone 3 — Reproducible performance evidence

**Outcome:** Captastic can detect meaningful regressions and choose Windows defaults from data.

- Collect a stable environment fingerprint: commit, configuration hash, Windows build, CPU, GPU,
  driver, topology, scaling, refresh rate, HDR state, and power mode.
- Automate warm-up, timed iterations, failure accounting, raw event artifacts, and compatible-run
  comparison.
- Add the sequence-marker workload to distinguish fresh pixels from low-latency stale frames.
- Report p50, p90, p95, p99, maximum, mean, deviation, failures, frame age, copies, and bytes.
- Store machine-specific baselines and flag a suspected p95 regression only when both the relative
  and absolute thresholds are crossed across repeat runs.
- Add a persistent WGC display experiment after the harness can compare equivalent semantics.
- Profile the largest p95 contributors and record backend/default decisions in ADRs.

### Exit criteria

- Three repeat runs exist for every claimed reference result.
- Static/latest, changing/latest, and changing/fresh workloads remain separate.
- Raw artifacts can reproduce every percentile table.
- DXGI/WGC defaults are chosen per source capability and measured behavior, not convenience.

## Milestone 4 — Versioned resident control plane

**Outcome:** CLI commands and future UI surfaces control the warm daemon safely instead of starting
parallel capture engines.

- Add a versioned same-user named-pipe protocol with an explicit Windows ACL.
- Route readiness, status, stop, capture, pause/resume, and effective-configuration queries through
  the protocol.
- Make `captastic capture` use the resident initialized engine by default; label any future one-shot
  path as cold.
- Preserve the existing single-instance guard during migration and report protocol-version mismatch
  clearly.
- Add malformed-message, unauthorized-client, disconnect, queue-full, and daemon-restart tests.

### Exit criteria

- A second process cannot initialize another resident engine.
- CLI-triggered capture has the same warm-path semantics as the registered hotkey.
- Unauthorized or malformed local requests cannot crash or control the daemon.

## Milestone 5 — Optional capture workflows

**Outcome:** Add frequently requested workflows without changing the default clipboard-first latency
boundary.

- Add bounded asynchronous PNG file output with safe templates, collision handling, and atomic
  finalization.
- Expose clipboard-only, file-only, and clipboard-plus-file destinations in configuration and the
  overlay Options menu.
- Add explicit cursor inclusion after DXGI pointer-shape composition and WGC behavior are correct and
  separately measured.
- Consider capture delay, destination folder shortcuts, and lightweight notifications only after
  the underlying worker and error behavior are complete.
- Evaluate MSI/MSIX packaging and automatic updates only after signing identity and release channels
  are stable.

### Exit criteria

- Output encoding and I/O never occur before CPU-frame readiness and never block capture/selection
  threads.
- Output failures do not invalidate clipboard success or captured pixels.
- Cursor and destination choices are explicit in configuration, logs, and structured results.

## Milestone 6 — macOS native proof

**Outcome:** Reuse Captastic's measurement and frame contracts around a native ScreenCaptureKit
implementation while preserving macOS-specific consent and timing semantics.

- Add a macOS crate only when ScreenCaptureKit implementation begins.
- Implement permission diagnostics, a retained stream/session, CPU normalization, native hotkey, and
  pasteboard output.
- Map latest/fresh-equivalent behavior honestly; do not claim semantic equivalence where the API does
  not provide it.
- Add macOS compilation CI while keeping interactive permission and capture tests on a logged-in
  machine.

## Milestone 7 — Linux native proofs

**Outcome:** Establish separate, honest Linux implementations instead of presenting Linux as one
capture environment.

- Implement Wayland through XDG Desktop Portal and PipeWire with explicit user consent and restore
  tokens where supported.
- Implement X11 separately and compare direct image capture with shared-memory extensions.
- Detect compositor/session capabilities and document global-hotkey limitations and fallbacks.
- Implement clipboard ownership for the lifetime required by Linux consumers.
- Keep Wayland and X11 benchmark results separate.

## Explicitly deferred

- A generic cross-platform screenshot crate around native capture engines.
- Electron or another heavyweight UI runtime for the overlay.
- Network upload, accounts, cloud history, or telemetry.
- Automatic updates before signed releases and rollback behavior exist.
- Cross-platform UI unification before each native backend proves its own capture and permission
  model.

## Recommended next branch

Start with `feature/4/windows-release-candidate` and keep its work ordered as follows:

1. Reconcile the Windows specification and acceptance checklist with current implementation.
2. Add the display/topology test matrix and sequence-marker workload.
3. Close rotation and mixed-DPI correctness gaps found by that matrix.
4. Add repeatable soak tooling and record a baseline run.
5. Provision the selected Authenticode service and add approval-gated signing/verification.
6. Produce and manually qualify `v0.1.0-rc.1`.

The signing-provider application can proceed in parallel because identity validation may take longer
than the local correctness work.
