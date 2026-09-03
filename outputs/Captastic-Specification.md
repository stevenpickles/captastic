# Captastic — Headless Rust Screenshot Latency Prototype

**Status:** Implementation specification  
**Primary milestone:** Windows latency proof  
**Language:** Rust  
**Product shape:** Resident, headless process plus command-line interface  
**Audience:** A developer or Codex implementation agent

---

## 1. Executive summary

Captastic is a headless Rust prototype for an extremely fast cross-platform screenshot capture tool. Its first purpose is not to become a complete screenshot application. Its first purpose is to prove, with reproducible measurements, how quickly a persistent native capture engine can turn a global-hotkey event into a usable frame.

The first implementation targets Windows and directly exercises native Windows capture APIs. The common Rust code defines commands, metadata, measurements, and CPU-frame contracts, while native GPU resources and API lifetimes remain owned by platform backends. Later milestones add macOS and Linux backends without forcing them into a lowest-common-denominator screenshot library.

The defining invariant is:

> The hotkey-to-frame critical path contains no process startup, disk I/O, network I/O, image compression, configuration parsing, display discovery, device creation, or avoidable allocation.

Clipboard publication is measured as a separate downstream stage. Optional file encoding and writing occur on a background worker after the frame is available; they never delay the hotkey-to-frame measurement.

## 2. Product principles

1. **Measure latency, do not infer it.** Every important transition receives a monotonic timestamp.
2. **Keep the engine warm.** Devices, capture sessions, display topology, threads, and reusable buffers are initialized before a hotkey can fire.
3. **Use native backends.** Share contracts across platforms, not capture implementations.
4. **Report frame age.** A cached frame is fast to retrieve but may predate the hotkey. Captastic must never present that as fresh on-demand capture.
5. **Separate the critical path.** Capture, clipboard, encoding, saving, and logging are distinct measured stages.
6. **Prefer bounded work.** Use bounded queues, explicit backpressure, fixed-capacity event storage, and buffer pools.
7. **Optimize only from evidence.** Keep copies and allocations visible in metrics; replace them after profiles identify a material cost.
8. **Isolate unsafe code.** Native calls and raw resource handling live in platform crates behind safe Rust APIs.

## 3. Goals

### 3.1 Prototype goals

- Run as a resident headless process.
- Register a system-wide capture hotkey.
- Keep a Windows capture engine initialized while idle.
- Capture one display at a time, beginning with the primary display.
- Support two explicitly named capture modes:
  - `fresh`: request/accept a frame whose presentation is at or after the trigger when the API permits.
  - `latest`: snapshot the most recent warm frame and report its age at trigger time.
- Produce a usable, tightly specified CPU image in BGRA8 for validation and clipboard publication.
- Optionally retain a native GPU frame long enough to measure time-to-native-frame separately from GPU-to-CPU readback.
- Copy an uncompressed image to the Windows clipboard without PNG/JPEG/WebP encoding.
- Optionally encode and write a file on a background worker.
- Record p50, p90, p95, p99, maximum, failures, timeouts, frame age, allocations where practical, and environment metadata.
- Compare at least one Windows-native backend and leave room for a second backend experiment.
- Make benchmark runs reproducible enough to detect regressions on the same machine.
- Define clean extension points for ScreenCaptureKit on macOS and portal/PipeWire plus X11 on Linux.

### 3.2 Longer-term architectural goals

- Full-display, virtual-desktop, window, and region capture capabilities where supported.
- Cursor inclusion as an explicit option and capability.
- Mixed-DPI and rotated-monitor correctness.
- SDR correctness first, followed by explicit HDR/color-space handling.
- A native overlay can consume a frozen frame without changing capture engine ownership.
- Multiple native implementations can be benchmarked behind the same scenario runner.

## 4. Non-goals

The prototype does not include:

- A polished cross-platform overlay, tray UI, editor, annotations, OCR, video, GIF, upload, account, or cloud feature. The Windows prototype includes the minimum native selection overlay needed to validate region and window workflows.
- A promise of identical behavior across all compositors and operating systems.
- A universal hard latency guarantee across arbitrary hardware.
- PNG, JPEG, WebP, filesystem, or network work before `frame_ready`.
- Cross-platform GPU-handle interoperability.
- A public stable Rust library API in the first milestone.
- Sandboxed packaging, store submission, installer UX, or auto-update.
- Capturing secure desktops, DRM-protected surfaces, login screens, UAC secure prompts, or content the OS intentionally withholds.
- Defeating system capture indicators, consent UI, privacy controls, or application capture exclusions.
- Starting from a generic screenshot crate and treating its one-call API as the product architecture.

## 5. Definitions and measurement boundaries

### 5.1 Terms

- **Trigger:** A global hotkey or an IPC/CLI request that asks the resident engine to capture.
- **Native frame:** The earliest stable platform-owned GPU or compositor buffer representing the requested source.
- **CPU frame:** Pixel bytes in a process-owned buffer with documented dimensions, stride, format, orientation, and color metadata.
- **Frame age:** `trigger_monotonic_time - frame_presentation_monotonic_time`. Negative values indicate a frame presented after the trigger.
- **Fresh capture:** A capture that waits for or selects a frame presented no earlier than the trigger, within a configured timeout.
- **Latest capture:** A capture that snapshots the warm engine's newest available frame, even if it predates the trigger.
- **Critical path:** `hotkey_received` through `native_frame_ready`, and separately through `cpu_frame_ready` when a CPU frame is requested.
- **Clipboard path:** `cpu_frame_ready` through `clipboard_committed`.
- **Persistence/warm-up:** Initialization performed before triggers, including device creation, source selection, capture-session creation, staging resource allocation, and thread startup.

### 5.2 Required timestamps

Each capture attempt has one `CaptureId` and records, where applicable:

| Event | Meaning |
|---|---|
| `hotkey_received` | First instruction in the platform hotkey callback/message handler |
| `trigger_enqueued` | Capture command accepted by the bounded queue |
| `trigger_dequeued` | Capture thread begins processing it |
| `capture_requested` | Native backend request or snapshot selection begins |
| `native_frame_ready` | Stable native frame lease is available |
| `readback_started` | GPU/compositor-to-CPU transfer begins |
| `cpu_frame_ready` | Complete CPU image is available |
| `clipboard_started` | Clipboard worker begins publication |
| `clipboard_committed` | OS clipboard accepts ownership/data |
| `encode_started` / `encode_finished` | Optional compression boundaries |
| `file_write_started` / `file_write_finished` | Optional filesystem boundaries |
| `attempt_finished` | Attempt reaches a terminal state |

Use a monotonic clock for durations. Record wall-clock time only for correlation and filenames. Do not calculate latency from wall time.

### 5.3 Honest latency modes

Captastic must emit the capture mode with every sample.

- In `fresh` mode, report the wait for a post-trigger frame. A static desktop may not produce a new DXGI frame; benchmark workloads must include a controlled visual change or moving test pattern.
- In `latest` mode, report both the near-immediate snapshot latency and frame age. A fast result with an old frame is not equivalent to fresh capture.
- Never combine these modes in one percentile distribution.
- Report native-frame and CPU-frame distributions separately.

## 6. User stories

### 6.1 Developer stories

- As a developer, I can start `captastic daemon` once and see that the capture engine is ready before testing a hotkey.
- As a developer, I can press the registered hotkey and receive a capture ID plus measured native-frame and CPU-frame latency.
- As a developer, I can run a repeatable benchmark without writing images to disk during timed iterations.
- As a developer, I can compare `fresh` and `latest` behavior without conflating their results.
- As a developer, I can list displays and their stable-in-session IDs, geometry, scale, rotation, adapter, and pixel format.
- As a developer, I can select a backend explicitly and learn why it is unavailable.
- As a developer, I can validate a frame using dimensions, stride, sampled pixels, and an optional checksum.
- As a developer, I can enable clipboard output and measure it separately.
- As a developer, I can request optional PNG output after capture and see encoding and write durations outside the capture metric.
- As a developer, I can collect a JSON or JSON Lines benchmark artifact containing enough environment data to compare like with like.

### 6.2 Future end-user stories protected by this architecture

- As a user, I can press a hotkey and have the visible screen freeze immediately.
- As a user, I can select a region from that frozen frame and copy it without waiting for a file save.
- As a user, I can opt into automatic saving without making the common clipboard workflow slower.
- As a user, I can understand and grant platform capture permissions rather than have the tool bypass them.

## 7. Requirements

Requirement keywords **MUST**, **SHOULD**, and **MAY** are normative.

### 7.1 Functional requirements

- **FR-001:** Captastic MUST expose a resident daemon mode.
- **FR-002:** The daemon MUST initialize its selected capture backend before reporting `ready`.
- **FR-003:** The Windows milestone MUST register a configurable global hotkey using a native message-loop thread.
- **FR-004:** A hotkey handler MUST only timestamp, construct a small fixed-size command, attempt a nonblocking/bounded enqueue, and return.
- **FR-005:** Captastic MUST support `fresh` and `latest` capture modes and label all results.
- **FR-006:** Captastic MUST capture the primary display and SHOULD accept an explicit display ID.
- **FR-007:** Captastic MUST expose display enumeration outside the critical path.
- **FR-008:** Captastic MUST provide a CPU BGRA8 frame path with explicit stride and top-left origin.
- **FR-009:** Captastic MUST offer uncompressed clipboard publication on Windows.
- **FR-010:** Optional file output MUST be dispatched after `cpu_frame_ready` and MUST not block the capture thread.
- **FR-011:** Captastic MUST emit structured benchmark output and a concise human-readable summary.
- **FR-012:** Captastic MUST reject a second daemon instance or connect to the existing instance.
- **FR-013:** CLI capture requests SHOULD use local IPC to the resident daemon. One-shot capture MAY exist but MUST be labeled `cold` and excluded from warm benchmarks.
- **FR-014:** Backend capabilities MUST be queryable rather than assumed.
- **FR-015:** Captastic MUST recover or clearly fail after display topology changes, device removal, session lock/unlock, and DXGI access loss.

### 7.2 Critical-path requirements

- **PR-001:** No file open/read/write operation may occur from `hotkey_received` through `cpu_frame_ready`.
- **PR-002:** No network API may be linked into or called by the capture path.
- **PR-003:** No image encoder or compressor may execute before `cpu_frame_ready`.
- **PR-004:** Configuration parsing and logging-subscriber initialization MUST finish before readiness.
- **PR-005:** Graphics devices, capture sessions, and normal-size staging buffers MUST be created before readiness.
- **PR-006:** Hot-path communication MUST use a bounded queue with a documented full-queue policy.
- **PR-007:** Formatting log strings, serializing JSON, or flushing output MUST NOT occur on the capture thread during timed work.
- **PR-008:** Normal repeated capture at unchanged topology and resolution SHOULD reuse buffers.
- **PR-009:** Every unavoidable full-frame copy MUST be named and timed.
- **PR-010:** The capture path MUST NOT wait for optional clipboard, encoding, or file workers.

### 7.3 Correctness requirements

- **CR-001:** Frame metadata MUST include width, height, row stride, pixel format, origin, display ID, desktop coordinates, rotation, capture mode, trigger time, and presentation time when supplied by the OS.
- **CR-002:** Checked arithmetic MUST guard byte-size and stride calculations.
- **CR-003:** The backend MUST release each acquired native frame exactly once, including error paths.
- **CR-004:** Rotated displays MUST either be normalized to top-left display orientation or rejected with a typed `UnsupportedRotation` error during the milestone; behavior may not be silent.
- **CR-005:** Cursor inclusion MUST be explicit. If disabled, Captastic must not accidentally compose a separately supplied cursor.
- **CR-006:** SDR output MUST declare the assumed color space. HDR sources MUST be reported as unsupported or converted by an explicitly measured later stage; silent clipping is not acceptable.
- **CR-007:** A capture ID MUST remain unique for the daemon lifetime.
- **CR-008:** Queue overflow, timeout, stale-frame policy, and dropped-frame counts MUST be observable.

### 7.4 Reliability requirements

- The daemon must remain responsive after failed captures.
- Recoverable backend failure must transition through a bounded reinitialization state.
- No panic may cross an FFI callback or COM boundary.
- Cleanup must be deterministic on normal shutdown.
- A stuck background encoder must not block hotkey capture; bounded output queues may drop or reject optional work according to configuration.

## 8. Architecture

### 8.1 High-level components

```text
                         captastic CLI
                            |
                     local IPC command
                            |
                            v
  native hotkey ---> Trigger Coordinator <--- benchmark driver
                            |
                      bounded command
                            |
                            v
                    Capture Engine Thread
                (owns native device/session/pool)
                            |
                 +----------+-----------+
                 |                      |
          native frame ready      CPU readback pool
                                        |
                                  Arc<CpuFrame>
                                  /           \
                                 v             v
                         Clipboard Worker   Output Worker
                                             encode -> file

  Timed path: hotkey -> coordinator -> capture engine -> native/CPU frame
  Untimed side effects: formatting logs, JSON output, compression, filesystem
```

### 8.2 Process model

The preferred prototype is one `captastic` executable with subcommands. `captastic daemon` becomes the resident single instance. Other invocations communicate through same-user local IPC:

- Windows: named pipe with an ACL restricted to the current user.
- macOS/Linux later: Unix-domain socket inside the per-user runtime directory, permissions `0600`.

The daemon contains several long-lived threads:

1. **Control/main thread:** startup, configuration, IPC, lifecycle, and status.
2. **Hotkey thread:** Windows message loop and `WM_HOTKEY`; does minimal bounded enqueue work.
3. **Capture thread:** owns D3D/COM capture resources and reusable GPU/CPU buffers.
4. **Clipboard thread:** owns or serializes clipboard operations as required by the OS.
5. **Output worker:** optional encoding and file writing.
6. **Metrics drain:** converts fixed-size measurement records into logs/results outside timed sections.

Do not introduce an async runtime into the Windows hot path. A later Linux portal backend may use async D-Bus internally, isolated inside that platform crate.

### 8.3 Engine state machine

```text
Uninitialized
    |
    v
Initializing ----failure----> Degraded
    |                            |
    v                            | retry/backoff or operator action
Ready <--------------------------+
  |  \
  |   \ topology/device/access loss
  |    v
  |  Reinitializing
  |    |
  +----+
  |
  +--> Capturing --> Ready
  |
  +--> ShuttingDown --> Stopped
```

Readiness is emitted only after hotkey registration, capture-session initialization, topology caching, buffer-pool sizing, worker startup, and a successful backend health check. A warm-up frame SHOULD be acquired where this does not create misleading permission or capture behavior.

### 8.4 Capability-oriented backend boundary

Avoid a broad interface that assumes every platform can capture every target the same way. Use explicit source kinds, modes, and capabilities.

Illustrative core types:

```rust
pub struct BackendCapabilities {
    pub sources: SourceCapabilities,
    pub modes: CaptureModeCapabilities,
    pub cursor: CursorCapabilities,
    pub hdr: HdrCapabilities,
    pub can_report_presentation_time: bool,
    pub can_keep_warm_stream: bool,
}

pub enum CaptureMode {
    Fresh { timeout: Duration },
    Latest { max_age: Option<Duration> },
}

pub enum CaptureSource {
    Display(DisplayId),
    // Added only when implemented and tested:
    Window(WindowId),
    VirtualDesktop,
}

pub struct CaptureRequest {
    pub id: CaptureId,
    pub triggered_at: Instant,
    pub source: CaptureSource,
    pub mode: CaptureMode,
    pub cpu_frame: bool,
    pub include_cursor: bool,
}
```

The platform implementation must own thread-affine native resources. The shared core should not attempt to store D3D textures, IOSurfaces, or PipeWire buffers in one public universal enum. Instead, the capture thread keeps native frames private and publishes:

- a `NativeFrameToken` meaningful only to that engine instance;
- portable `FrameMetadata`;
- a shared CPU frame only when requested;
- measurement events.

Illustrative service boundary:

```rust
pub trait CaptureEngine {
    fn capabilities(&self) -> &BackendCapabilities;
    fn displays(&self) -> &[DisplayInfo];
    fn capture(&mut self, request: CaptureRequest)
        -> Result<CaptureOutcome, CaptureError>;
    fn health(&self) -> EngineHealth;
    fn reinitialize(&mut self) -> Result<(), CaptureError>;
}
```

This trait standardizes orchestration and results, not native resource representation. Target-specific code may use concrete engine types instead of dynamic dispatch.

## 9. Rust workspace and repository layout

Start with a small workspace. Split crates when ownership or platform compilation requires it, not for every noun in the design.

```text
captastic/
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── README.md
├── LICENSE-APACHE
├── LICENSE-MIT
├── deny.toml
├── .github/
│   └── workflows/
│       ├── check.yml
│       └── windows-bench-smoke.yml
├── docs/
│   ├── architecture.md
│   ├── benchmarking.md
│   ├── windows-backend.md
│   └── adr/
│       ├── 0001-native-platform-backends.md
│       ├── 0002-latency-boundaries.md
│       └── 0003-frame-freshness.md
├── crates/
│   ├── captastic-core/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── capture.rs
│   │       ├── display.rs
│   │       ├── frame.rs
│   │       ├── metrics.rs
│   │       ├── protocol.rs
│   │       └── error.rs
│   ├── captastic-platform/
│   │   └── src/lib.rs
│   ├── captastic-windows/
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── com.rs
│   │       ├── d3d11.rs
│   │       ├── dxgi.rs
│   │       ├── capture_dxgi.rs
│   │       ├── capture_wgc.rs
│   │       ├── clipboard.rs
│   │       ├── hotkey.rs
│   │       ├── topology.rs
│   │       └── error.rs
│   ├── captastic-config/
│   │   └── src/lib.rs
│   └── captastic-app/
│       └── src/
│           ├── main.rs
│           ├── cli.rs
│           ├── daemon.rs
│           ├── ipc.rs
│           ├── coordinator.rs
│           └── benchmark.rs
├── tests/
│   ├── fixtures/
│   └── integration/
├── benches/
│   ├── buffer_pool.rs
│   ├── row_copy.rs
│   └── clipboard_layout.rs
└── scripts/
    ├── collect-windows-env.ps1
    └── run-windows-benchmark.ps1
```

Crate responsibilities:

- `captastic-core`: OS-neutral value types, commands, result records, measurement event schema, CPU-frame validation, and typed high-level errors. No native capture dependency.
- `captastic-platform`: platform selection and compile-time wiring; very small.
- `captastic-windows`: all Win32, COM, DXGI, D3D11, WGC, hotkey, clipboard, topology, and unsafe code.
- `captastic-config`: configuration schema, defaults, validation, and migration/version field.
- `captastic-app`: executable, daemon lifecycle, local IPC, CLI output, benchmark orchestration, and background workers.

Later add `captastic-macos` and `captastic-linux`; do not add empty placeholder crates in the first pull request.

### 9.1 Unsafe-code policy

- Set workspace lint `unsafe_op_in_unsafe_fn = "deny"`.
- Prefer `#![deny(unsafe_code)]` in OS-neutral crates.
- Allow unsafe code only in narrowly scoped platform modules.
- Every unsafe block must state its safety invariants.
- Wrap native handles with RAII types and implement `Drop` where ownership rules permit.
- Add tests for double-release, early-return, and reinitialization paths through safe fakes where native fault injection is impractical.

## 10. Windows-first backend strategy

### 10.1 Milestone backend: DXGI Desktop Duplication

Implement DXGI Desktop Duplication first for display capture because it exposes frames as DXGI surfaces, supports a persistent D3D11 device/session model, reports frame metadata, and makes GPU-to-CPU copies explicit.

Initialization, outside the critical path:

1. Initialize COM on the owning thread with the chosen apartment model.
2. Create and retain the DXGI factory and D3D11 device/context.
3. Enumerate adapters and outputs.
4. Map Captastic `DisplayId` values to adapter/output identities.
5. Create one duplication object for the selected output.
6. Allocate a small ring of staging textures sized for the current output.
7. Cache rotation, desktop bounds, format, and adapter metadata.
8. Start the acquisition loop or prepare on-demand acquisition according to mode.
9. Verify one frame/health cycle before reporting ready when feasible.

Capture behavior:

- Always pair successful `AcquireNextFrame` with `ReleaseFrame` through a guard.
- Use finite waits; never use an uninterruptible infinite wait.
- Treat `DXGI_ERROR_WAIT_TIMEOUT` as a typed timeout, not a crash.
- Treat `DXGI_ERROR_ACCESS_LOST` as a signal to recreate duplication after topology/session changes.
- Record `DXGI_OUTDUPL_FRAME_INFO` timing and pointer metadata.
- Account for output rotation. DXGI may return an unrotated surface containing rotated content.
- Default to `DXGI_FORMAT_B8G8R8A8_UNORM` semantics and verify the actual texture descriptor.
- For CPU output, copy the native texture to a reusable staging texture, map it, and perform one stride-aware copy into a pooled top-left BGRA8 CPU buffer if a stable owned buffer is needed.
- Measure GPU copy submission, synchronization/map wait, and CPU row copy separately.

#### Rolling acquisition for `latest`

A dedicated acquisition loop continuously accepts frames and retains the newest frame/texture slot plus presentation metadata. A trigger snapshots the newest completed slot. It must:

- use at least double buffering so readback cannot overwrite the latest retained frame;
- atomically publish a slot index/generation or exchange a small message with the capture thread;
- record frame age at the trigger;
- record dropped/replaced warm frames;
- avoid copying every warm frame to CPU unless the trigger requests it.

#### Post-trigger acquisition for `fresh`

The capture thread records the trigger time and accepts the first qualifying frame after it. Because a static desktop might not yield a new desktop image, benchmark runs must use a controlled animation or marker change and a finite timeout. The result reports whether presentation timing was OS-derived, inferred from arrival time, or unavailable.

### 10.2 Comparison backend: Windows Graphics Capture

Add Windows Graphics Capture (WGC) only after the DXGI vertical slice is measured. Keep it as a separate concrete implementation selectable with `--backend wgc`, not a branch-filled shared backend.

The WGC experiment should retain its D3D11 device, capture item, frame pool, and capture session. It should test:

- display capture latency;
- window capture potential;
- frame-pool buffering behavior;
- resize and device-loss behavior;
- system consent/picker or programmatic-access constraints for the supported Windows versions;
- system capture indicators/borders;
- presentation timestamp quality;
- GPU and CPU readback copy count.

Do not declare WGC or DXGI the permanent winner in advance. Record the workload, Windows build, GPU/driver, source type, and capture semantics, then select defaults from data and behavioral requirements.

### 10.3 Windows hotkey

The first implementation SHOULD call native `RegisterHotKey` and run a Win32 message loop on a dedicated thread, optionally using a message-only window for lifecycle messages.

Rules:

- Register only after validating configuration.
- Timestamp at the first line that handles `WM_HOTKEY`.
- Do not call capture APIs, clipboard APIs, logging formatters, or configuration code in the handler.
- Enqueue a compact `TriggerCommand` into a bounded channel using a nonblocking operation.
- On full queue, increment `trigger_dropped_queue_full` and return immediately.
- Unregister on clean shutdown.
- Report registration conflict with the exact binding and a remediation message.
- Ignore repeat events by default or coalesce them according to configuration.

The cross-platform `global-hotkey` crate MAY be used for an early comparison or later portability, but it must not obscure message-loop constraints or prevent direct timestamping. The Windows latency milestone should retain a native implementation as the reference path.

### 10.4 Windows clipboard

Implement a native Windows clipboard path for performance visibility and format control. `arboard` MAY serve as a functional baseline, not as an irreversible dependency.

Preferred prototype path:

1. Receive `Arc<CpuFrame>` on a dedicated serialized clipboard worker.
2. Validate BGRA8 dimensions, stride, and byte length.
3. Prepare `CF_DIBV5` or a deliberately chosen `CF_DIB` payload with correct orientation, channel masks, and color metadata.
4. Retry `OpenClipboard` on transient contention with a small bounded backoff and a total deadline.
5. Empty and set the clipboard.
6. Transfer allocation ownership exactly as required by `SetClipboardData`.
7. Record `clipboard_started`, contention duration/retries, conversion/copy time, and `clipboard_committed`.
8. Verify with a read-back integration test in a separate consumer path.

Do not encode PNG for the default Windows clipboard path. If a compressed clipboard representation is later added for compatibility, publish it as a separate asynchronous format and benchmark it separately.

## 11. macOS backend strategy (later milestone)

Use ScreenCaptureKit as the native capture foundation. Keep an `SCStream` warm for the chosen display where system policy permits, receive frame buffers through the stream-output callback, and retain the latest eligible `CMSampleBuffer`/pixel-buffer metadata inside the macOS backend.

Design constraints:

- Screen Recording permission and system selection behavior are product requirements, not errors to bypass.
- Objective-C objects may be thread-affine or not `Send`/`Sync`; the backend owns them on the required run-loop/dispatch context.
- Use generated Objective-C framework bindings such as the `objc2` family where they cover the required API; write narrow bindings only for missing surface area.
- Treat IOSurface/CVPixelBuffer storage as backend-private.
- Define CPU BGRA/RGBA normalization only at the readback boundary.
- Record ScreenCaptureKit frame status and timing metadata.
- Keep global hotkey and pasteboard implementations native to macOS behind their own small capability interfaces.
- CI can compile the backend on macOS, but capture/permission benchmarks require a logged-in interactive machine and should not be treated as normal hosted-CI tests.

## 12. Linux backend strategy (later milestone)

Linux requires two deliberate backends rather than one generic implementation.

### 12.1 Wayland

Use the XDG Desktop Portal ScreenCast interface to obtain an authorized PipeWire stream. Expect a user-visible source-selection/consent step during initial setup. A fully silent first-run headless flow is not a valid assumption.

- Use `ashpd` as the portal client candidate and `pipewire` bindings for the stream.
- Request persistence/restore tokens where the portal and desktop support them; treat tokens as sensitive local configuration.
- Prefer the portal-provided stable stream identity/serial mechanisms where available rather than assuming a PipeWire node ID is permanent.
- Keep the PipeWire loop and buffers inside the Linux backend.
- Measure warm-stream frame selection and frame age similarly to Windows `latest` mode.
- Report compositor, portal implementation/version, PipeWire version, pixel format, modifier, and memory type.
- Do not promise global hotkeys on all Wayland compositors. Detect the available desktop portal/protocol or require a desktop-specific integration.

### 12.2 X11

Use a direct X11 backend, likely through `x11rb`, and benchmark plain image capture versus shared-memory extensions where available. X11 and Wayland results must remain separate because their security and compositor models differ.

### 12.3 Linux clipboard

Clipboard ownership can require the daemon to remain alive to serve paste requests. Model clipboard lifetime explicitly. `arboard` is a reasonable baseline but its Wayland/X11 behavior and compositor protocol availability must be tested. Do not assume that setting data and immediately dropping the provider preserves clipboard contents.

## 13. Persistent initialized capture engine

### 13.1 Readiness contract

`captastic daemon` is ready only when:

- configuration is loaded and validated;
- the single-instance guard and IPC endpoint exist;
- the hotkey is registered;
- display topology is cached;
- the selected backend is initialized;
- native capture session/resources exist;
- buffer pools are sized for the selected display;
- clipboard/output workers are running if enabled;
- metrics storage is allocated;
- a backend health check succeeds.

Startup duration is recorded but excluded from warm capture latency.

### 13.2 Buffer pool

For the Windows single-display milestone:

- Allocate two or three GPU staging slots based on measured contention.
- Allocate two or three CPU frame slots sized to `stride * height` with checked arithmetic.
- Track slot state: `Free`, `GpuCopyPending`, `Mapped`, `CpuOwned`, `BackgroundUse`.
- A slot is reused only after all leases are released.
- If all slots are busy, apply a configured policy: reject capture, drop optional output, or allocate an explicitly counted emergency buffer. Default to rejecting optional output before capture.
- Rebuild pools on topology/size/format change outside the normal capture path.

### 13.3 Backpressure

Recommended bounded capacities:

- Trigger queue: 4.
- Clipboard queue: 2, newest-wins or reject-new based on user action semantics.
- Output queue: 2, reject optional saves when saturated unless the CLI explicitly waits.
- Metrics ring: enough for the configured benchmark sample count plus failures; otherwise increment a loss counter.

All policies must be visible in status and benchmark output.

## 14. Frame and buffer model

### 14.1 Portable CPU frame

```rust
pub struct CpuFrame {
    pub pixels: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub format: PixelFormat,
    pub origin: FrameOrigin,
    pub color_space: ColorSpace,
    pub metadata: FrameMetadata,
}

pub enum PixelFormat {
    Bgra8Unorm,
    Rgba8Unorm,
}

pub enum FrameOrigin {
    TopLeft,
}
```

For milestone one, `CpuFrame` MUST be top-left-origin BGRA8. `stride_bytes` may exceed `width * 4`. Consumers must respect stride.

### 14.2 Metadata

`FrameMetadata` should include:

- capture ID and backend name;
- display/source ID;
- source desktop rectangle with signed coordinates;
- physical width/height;
- scale factor and rotation;
- capture mode;
- trigger, native-ready, and CPU-ready monotonic offsets relative to the attempt start;
- presentation timestamp and provenance, if available;
- computed frame age;
- cursor mode;
- color space/dynamic range;
- buffer generation and pool slot ID for diagnostics;
- copy count known to Captastic;
- stale/fallback flags.

### 14.3 Native frame lifetime

Native frame handles remain private to the platform crate and capture thread. Do not place raw pointers or COM interfaces in cross-platform public structs. A token may identify a retained slot, but operations on it must be sent back to its owner thread.

### 14.4 Selection output materialization

The Windows prototype exposes explicit full-display, window, and region tools in a native floating toolbar. Full-display selection reuses the frozen frame, while region rectangles use absolute physical source pixels and perform a checked tight CPU crop only after confirmation. Window selection is a distinct operation: it retains the selected native window identity and asks that window to render off-screen, so the result is not a crop of the composed desktop and does not contain covering windows. Never silently substitute a desktop crop when native window rendering fails. The Options menu may contain only implemented behavior; the prototype currently supports dim-background toggling, clipboard destination status, and cancellation. A future backend may choose GPU crop/readback or Windows Graphics Capture while preserving these observable semantics.

## 15. Optional file output

File output is an asynchronous consumer of `Arc<CpuFrame>`.

- Default format: PNG for correctness and broad interoperability.
- Encoding begins only after `cpu_frame_ready`.
- File writes occur only on the output worker.
- Use a temporary file in the destination directory and atomic rename when supported.
- Define overwrite policy explicitly; default to `create_new` with collision suffixes.
- Sanitize filename-template substitutions.
- Record encoded byte count, encoder settings, encode duration, write duration, and total output duration.
- An explicit CLI command may wait for the file result, but the capture metric ends earlier and remains unchanged.
- Never include raw screen pixels in logs on encoding failure.

## 16. Performance instrumentation

### 16.1 Event representation

On timed threads, write compact fixed-size events to a preallocated ring or bounded channel:

```rust
pub struct PerfEvent {
    pub capture_id: u64,
    pub kind: PerfEventKind,
    pub ticks: u64,
    pub value: u64,
}
```

Convert ticks to durations and serialize them after the timed attempt. Avoid heap allocation and formatted strings in event emission.

### 16.2 Required derived metrics

- Hotkey handler duration.
- Hotkey to trigger enqueue.
- Queue wait.
- Trigger dequeue to capture request.
- Hotkey to native frame.
- Native frame acquisition wait.
- Native frame to CPU frame.
- GPU copy/map wait.
- CPU row-copy duration.
- Hotkey to CPU frame.
- CPU frame to clipboard commit.
- Hotkey to clipboard commit.
- Encode duration.
- File-write duration.
- Frame age at trigger and at native-ready.
- Timeout, dropped-trigger, stale-frame, device-reset, buffer-exhaustion, and clipboard-contention counts.

### 16.3 Allocation and copy accounting

At minimum, increment counters for:

- buffer-pool hit/miss;
- emergency allocation count and bytes;
- GPU copy count;
- full-frame CPU copy count and bytes;
- format-conversion copy count and bytes;
- clipboard payload allocation bytes.

An allocator-instrumented build MAY be added for benchmark diagnostics, but it must not be enabled in normal latency runs unless its overhead is characterized.

### 16.4 Output formats

- Human summary for interactive use.
- JSON document for one benchmark run.
- JSON Lines for individual attempts/events when requested.
- Optional CSV summary for plotting.

Include `schema_version` in machine-readable output.

## 17. Benchmark methodology

### 17.1 Benchmark classes

1. **Microbenchmarks:** row copy, swizzle/conversion, buffer-pool checkout, event write, DIB layout, checksum sampling. Use Criterion or a focused harness.
2. **Engine benchmarks:** resident daemon, synthetic trigger, native frame, optional readback. These are the primary measurements.
3. **Hotkey end-to-end benchmarks:** real `WM_HOTKEY` path through frame/clipboard.
4. **Cold-start diagnostic:** process start through first frame, reported separately and never mixed with warm results.
5. **Correctness soak:** topology changes, lock/unlock, repeated captures, queue saturation, and device recovery.

### 17.2 Controlled Windows scenarios

Run each backend/mode across:

- one 1080p SDR 60 Hz display;
- one 4K SDR 60 Hz display when available;
- multiple displays on one adapter;
- multiple adapters if available;
- static desktop;
- controlled 60 Hz changing test pattern;
- cursor stationary and moving;
- include-cursor on/off;
- latest/native only;
- latest/CPU readback;
- fresh/native only;
- fresh/CPU readback;
- CPU frame to clipboard;
- optional PNG output, reported separately.

Do not compare a changing-screen fresh test with a static-screen latest test as though they measure the same operation.

### 17.3 Run protocol

For a serious local comparison:

1. Build with `--release` and a locked dependency graph.
2. Record commit SHA, dirty-tree flag, Rust version, target triple, features, backend, configuration hash, Windows build, CPU, GPU, driver, display topology, refresh rates, scaling, power mode, HDR state, and foreground workload.
3. Keep the machine on AC power and use a documented power profile.
4. Avoid remote-desktop sessions unless that is the scenario under test.
5. Run a fixed warm-up period and discard warm-up samples.
6. Run at least 500 timed iterations for synthetic-trigger engine tests and at least 100 for manual/automated real-hotkey tests, unless the scenario cost justifies a documented lower count.
7. Randomize or alternate backend order to reduce thermal/order bias.
8. Record every failure and timeout; do not discard them silently.
9. Report p50, p90, p95, p99, maximum, mean, standard deviation, count, and failure count. Percentiles are primary.
10. Save raw results after the run, never during a timed attempt.
11. Repeat the complete run at least three times before claiming a regression or improvement.

### 17.4 Freshness validation

Use a small on-screen test-pattern program that changes a sequence number/color at a known monotonic time. The captured pixels reveal which presentation was acquired. Correlate that marker with Captastic's timestamps. This avoids treating arrival time as proof that pixels are post-trigger.

An optional high-speed-camera or photodiode/keyboard-injection test can validate user-perceived physical latency later, but software timestamps remain the routine regression tool.

### 17.5 Regression policy

- Store a machine-specific baseline artifact.
- Compare only compatible environment fingerprints by default.
- Flag a suspected regression when p95 hotkey-to-CPU-frame rises by both more than 10% and more than 2 ms across three runs, with no correctness improvement explaining it.
- Do not make hosted CI fail on absolute GPU latency; hosted runners are noisy and may lack an interactive desktop.
- A dedicated physical benchmark host MAY enforce absolute or relative budgets.

### 17.6 Initial performance budgets

These are engineering targets for a documented Windows reference machine, not universal product guarantees:

| Metric | Initial target |
|---|---:|
| Hotkey handler work, p95 | <= 0.25 ms |
| Hotkey to capture-thread dequeue, p95 | <= 2 ms |
| `latest` hotkey to native-frame token, p95 | <= 5 ms |
| `latest` frame age under 60 Hz changing workload, p95 | <= one refresh interval + 3 ms |
| `fresh` hotkey to native frame under 60 Hz changing workload, p95 | <= 25 ms |
| Hotkey to owned CPU BGRA frame at 4K SDR, p95 | <= 40 ms |
| CPU frame to clipboard commit at 4K SDR, p95 | <= 20 ms |

If the reference hardware cannot meet a target, preserve the result, profile it, document the bottleneck, and revise the budget through an architecture decision record. Do not manipulate semantics to make the number pass.

## 18. Logging and diagnostics

Use structured `tracing` spans/events outside the minimal performance event recorder.

Logging requirements:

- Default level `info`; no pixel data.
- `--log-format compact|json`.
- Capture ID on all attempt-related events.
- Backend, state transition, display ID, error category, HRESULT/native code, retry count, and duration fields.
- Redact paths in shared benchmark artifacts unless explicitly requested.
- Do not log clipboard contents or raw frame bytes.
- Do not synchronously flush files from the capture or hotkey thread.
- During benchmark mode, retain events in memory and write after timed iterations.
- Provide a small in-memory diagnostic ring so recent state can be dumped after a crash or fatal error without continuous hot-path disk writes.

`captastic doctor` should report backend availability, permission state where queryable, hotkey conflicts, display topology, capture engine health, output-directory writability, and relevant native error explanations.

## 19. CLI specification

```text
captastic daemon [--config <path>] [--backend auto|dxgi|wgc]
captastic status [--json]
captastic stop

captastic displays [--json]
captastic capture [OPTIONS]
captastic benchmark [OPTIONS]
captastic doctor [--json]
captastic config show [--effective] [--json]
captastic config validate [--path <path>]
```

### 19.1 `captastic daemon`

- Starts the resident engine and IPC endpoint.
- Fails clearly if another daemon owns the endpoint, or prints the existing daemon status with `--connect-existing`.
- Prints a one-line ready record including PID, backend, display, hotkey, and initialization time.
- `--foreground` is the prototype default. Service/tray startup is later work.

### 19.2 `captastic capture`

Suggested options:

```text
--display <id|primary>
--mode <fresh|latest>
--fresh-timeout <duration>
--max-frame-age <duration>
--cpu-frame <true|false>
--clipboard
--output <path>
--format <png>
--cursor <include|exclude>
--wait-for-output
--json
```

The command connects to the resident daemon by default. If no daemon exists, it returns an actionable error. A future `--cold` option may perform one-shot initialization but must label all measurements cold.

### 19.3 `captastic benchmark`

Suggested options:

```text
--backend <dxgi|wgc>
--mode <fresh|latest>
--display <id|primary>
--iterations <n>
--warmup <n>
--interval <duration>
--cpu-frame <true|false>
--clipboard
--workload <static|external-changing|marker>
--output-results <path>
--raw-events <path>
--compare <baseline.json>
--json
```

Benchmark mode must reject incompatible combinations, such as claiming `fresh` on a static workload without an explicit timeout policy.

### 19.4 Exit codes

| Code | Meaning |
|---:|---|
| 0 | Success |
| 2 | CLI/configuration error |
| 3 | Daemon unavailable or IPC failure |
| 4 | Backend unavailable/unsupported |
| 5 | Permission/consent denied |
| 6 | Capture failed or timed out |
| 7 | Clipboard failed |
| 8 | Optional output failed |
| 9 | Benchmark completed with invalid/incomplete samples |
| 10 | Internal invariant violation |

## 20. Configuration

Use versioned TOML. Load it once at daemon startup. Explicit reload is allowed but must rebuild affected resources outside a capture attempt.

Example:

```toml
schema_version = 1

[daemon]
backend = "auto"
display = "primary"
trigger_queue_capacity = 4

[hotkey]
binding = "Ctrl+Shift+F9"
repeat = "ignore"

[capture]
mode = "latest"
fresh_timeout_ms = 100
max_frame_age_ms = 25
cpu_frame = true
cursor = "exclude"
buffer_slots = 3

[clipboard]
enabled = true
format = "dibv5"
open_timeout_ms = 40
retry_interval_ms = 2

[output]
enabled = false
directory = "Pictures/Captastic"
format = "png"
filename = "captastic-{local_date}-{local_time}-{capture_id}.png"
queue_capacity = 2

[metrics]
enabled = true
ring_capacity = 10000
raw_events = false

[logging]
level = "info"
format = "compact"
```

Validation rules:

- Unknown fields SHOULD be errors during the prototype to catch typos.
- Durations and capacities must have safe upper bounds.
- Buffer count must be at least two for rolling latest mode.
- Output paths are expanded and validated outside the critical path.
- Hotkey syntax is parsed before native registration.
- `max_frame_age_ms` applies only to `latest`.
- Secrets are not expected in configuration. Future restore tokens must use restricted per-user storage.

Precedence: compiled defaults < config file < environment variables explicitly documented for development < CLI flags. `captastic config show --effective` displays the resolved configuration without sensitive values.

## 21. Error handling and recovery

Use typed library errors with source chaining. Use `anyhow` only at executable/reporting boundaries if desired.

Suggested categories:

```rust
pub enum CaptureErrorKind {
    Unsupported,
    PermissionDenied,
    SourceUnavailable,
    Timeout,
    AccessLost,
    DeviceRemoved,
    TopologyChanged,
    BufferExhausted,
    InvalidFrame,
    NativeFailure,
    ShuttingDown,
}
```

Each error record should include:

- stable category/code;
- human message;
- retryability;
- backend and operation;
- native HRESULT/error code where safe;
- capture ID if applicable;
- source error chain for diagnostics.

Recovery rules:

- `WAIT_TIMEOUT`: complete the attempt as timeout; engine remains ready.
- `ACCESS_LOST`, display change, lock/unlock: stop acquisition, release dependent resources, re-enumerate, rebuild pools, resume with bounded exponential backoff.
- Device removed/reset: recreate device plus all dependent resources.
- Hotkey conflict: startup failure unless hotkey is explicitly disabled.
- Clipboard busy: bounded retry on the clipboard thread only; capture remains successful even if clipboard fails.
- Output queue full: capture remains successful; return an optional-output rejection.
- Invalid frame metadata/size: reject frame and preserve engine health information; never perform unchecked allocation/copy.
- Panic in a worker: mark the subsystem failed, prevent unwind across FFI, and terminate cleanly or restart only where invariants are proven.

## 22. Security and privacy

- Use only documented OS capture and permission mechanisms.
- Never attempt to capture secure desktops or bypass protected-content behavior.
- Clearly surface capture permission/consent requirements.
- Keep IPC local and same-user authenticated. On Windows, restrict the named-pipe DACL to the current logon user and reject remote clients.
- Do not expose an unauthenticated TCP port.
- Validate all IPC message lengths, enum values, paths, and version fields.
- Use a maximum frame dimension and checked `stride * height` calculation before allocation/copy.
- Treat captured pixels as sensitive. Do not write them unless the user enables or requests output.
- Disable crash-dump inclusion of large frame buffers where feasible; document OS limitations.
- Do not log pixels, thumbnails, clipboard data, window titles, application names, or full private paths by default.
- Clear/reuse buffers deliberately. A future secure-memory option may zero buffers, but its cost must be measured and it is not a claim that GPU/OS copies are erased.
- Keep only the minimum number of retained frames; prototype default is no history beyond active buffer-pool leases.
- Prevent output path traversal through filename templates and create files with user-only permissions where supported.
- Track dependency advisories and licenses with `cargo audit`/`cargo deny` or equivalent CI steps.
- Document that administrators, debuggers, malware, the OS compositor, and GPU drivers may access process memory; Captastic is not a secure enclave.

## 23. Testing strategy

### 23.1 Unit tests

- Rectangle and signed desktop-coordinate math.
- Display ID/session identity mapping.
- Stride, byte-length, overflow, and alignment validation.
- BGRA row copy with padded source/destination strides.
- Rotation normalization for 0/90/180/270 degrees.
- DIB/DIBV5 header and orientation generation.
- Capture mode and frame-age rules.
- Config defaults, precedence, unknown fields, and invalid bounds.
- Error mapping and retryability.
- Buffer-pool checkout/return/generation behavior.
- Bounded-queue overflow policies.
- Metrics percentile and schema serialization.
- Filename-template sanitization and collision policy.

### 23.2 Property/fuzz tests

- Frame layout parser/validator over arbitrary dimensions, strides, and byte lengths.
- IPC decoder with malformed/truncated/oversized messages.
- Configuration parser.
- Crop/rotation coordinate transformations.
- DIB construction length and offset invariants.

### 23.3 Fake-backend integration tests

Create a deterministic fake engine that emits frames with known sequence markers and controllable delays/errors. Test:

- hotkey/IPC trigger through result;
- fresh versus latest semantics;
- frame-age rejection;
- queue saturation and coalescing;
- clipboard/output independence;
- timeout and recovery state transitions;
- metrics completeness;
- daemon single-instance and shutdown behavior.

### 23.4 Windows interactive integration tests

These run on a logged-in physical or interactive VM session:

- Enumerate and capture primary display.
- Validate known test-pattern pixels.
- Repeated acquisition/release without leaks.
- Native frame to top-left BGRA CPU frame.
- Clipboard publish and independent readback.
- Hotkey registration, event receipt, and conflict handling.
- Display resolution/rotation change and reinitialization.
- Session lock/unlock recovery where automation permits.
- Adapter/device reset simulation where practical.
- Multi-monitor negative coordinates and mixed DPI.
- 1,000- and 10,000-capture soak runs with stable handles/memory.

### 23.5 Performance tests

- Use the benchmark methodology in this specification.
- Keep raw artifacts for reference hosts.
- Test native-frame-only and CPU-readback paths separately.
- Assert event ordering and absence of forbidden critical-path stages.
- Add a test-only file/network/compressor sentinel that panics if invoked inside a marked critical-path scope.

### 23.6 Manual checks

- Protected/DRM content behavior is safe and documented.
- UAC secure prompt/session switching does not crash Captastic.
- Clipboard output pastes correctly into at least Paint, a browser, and an Office-style application.
- Anti-malware warnings and signing behavior are understood before distribution.

## 24. Continuous integration

### 24.1 Every pull request

Run on stable Rust unless the repository pins another toolchain:

- `cargo fmt --check`.
- `cargo check --workspace --all-targets` on supported hosts.
- `cargo clippy --workspace --all-targets -- -D warnings`.
- `cargo test --workspace`.
- Documentation build.
- Dependency/advisory and license policy checks.
- Minimal-feature and relevant feature-matrix checks.
- Windows cross-crate compilation on a Windows runner.
- macOS/Linux compilation after those backends exist.

### 24.2 CI limitations

Hosted CI generally does not provide a reliable interactive desktop, stable GPU, capture permission state, or deterministic latency. Therefore:

- normal CI tests correctness through fakes and compile checks;
- interactive native tests run on labeled self-hosted machines;
- absolute latency gates run only on dedicated reference hardware;
- hosted-runner timing is diagnostic, never a release claim.

### 24.3 Artifacts

For dedicated benchmark jobs, retain:

- summary JSON;
- raw attempt/event JSONL when enabled;
- environment fingerprint;
- logs without pixels/private paths;
- comparison report against the matching baseline.

Do not upload screenshot frames by default.

## 25. Recommended crates and dependency policy

Do not pin versions in this specification. Select current compatible releases, commit `Cargo.lock`, minimize features, review maintenance/security posture, and record why each dependency is needed.

| Area | Candidate | Guidance |
|---|---|---|
| Windows APIs | `windows` | Primary binding for Win32, COM, DXGI, D3D11, WGC, clipboard, and hotkey APIs. Enable only required feature namespaces. |
| CLI | `clap` with derive | Keep parsing at process/control boundaries. |
| Serialization | `serde`, `serde_json` | Config/result models and benchmark artifacts; never serialize on timed threads. |
| TOML config | `toml` | Parse once before readiness. |
| Library errors | `thiserror` | Stable typed error enums and source chains. |
| App errors | `anyhow` (optional) | Only at CLI/daemon reporting boundaries; do not erase typed backend errors internally. |
| Structured logs | `tracing`, `tracing-subscriber` | Use async/buffered sinks; keep fixed-size perf events separate. |
| Bounded channels | `crossbeam-channel` | Mature bounded queues and nonblocking `try_send`; benchmark against standard-library choices before optimizing. |
| Percentiles | `hdrhistogram` | Store/aggregate latency distributions; raw samples remain available. |
| Microbenchmarks | `criterion` | Good for pure CPU operations, not the main OS end-to-end harness. |
| PNG output | `png` | Simple initial encoder on the background worker. Compare faster encoders later only with data. |
| Image fixtures | `image` (dev/optional) | Useful for test decoding/fixtures; avoid pulling broad codecs into the capture core. |
| Bit flags | `bitflags` | Platform-independent capability flags if useful. |
| Byte casts | `bytemuck` (optional) | Only for types that soundly meet `Pod`/alignment requirements; ordinary row copy may be clearer. |
| Config directories | `directories` or `directories-next` | Resolve per-user config/data locations outside the critical path. |
| Property tests | `proptest` | Frame layout, transforms, and config invariants. |
| Temp files | `tempfile` | Output integration tests. |
| Concurrency model tests | `loom` (optional) | Use for small pool/state primitives if atomics become nontrivial. |
| Clipboard baseline | `arboard` | Functional comparison only; keep native platform clipboard path available for measurement/control. |
| Hotkey baseline | `global-hotkey` | Useful portability experiment; note event-loop and Linux X11 limitations. Native Windows path remains reference. |
| macOS bindings | `objc2`, `objc2-screen-capture-kit`, related framework crates | Candidate generated bindings; keep platform thread rules visible. |
| Wayland portal | `ashpd` | Candidate XDG Desktop Portal ScreenCast client. |
| PipeWire | `pipewire` | Candidate Rust bindings for authorized Wayland capture streams. |
| X11 | `x11rb` | Candidate direct X11 protocol/backend implementation, including feature-gated extensions. |

Avoid by default:

- a generic cross-platform screenshot crate as the core capture layer;
- a GUI framework in the headless milestone;
- an async runtime in the Windows hot path;
- a broad image-processing dependency for raw frame ownership;
- unbounded channels;
- dependencies whose only purpose is a trivial helper available in `std`;
- enabling every feature on platform binding crates.

Before adopting any abstraction crate, answer:

1. Can Captastic keep the native session initialized?
2. Can Captastic access native frame timing?
3. Can Captastic retain GPU buffers and control readback?
4. Can Captastic count copies and allocations?
5. Can Captastic recover from native access/device loss?
6. Can Captastic express frame freshness and age?
7. Can Captastic bypass compression and disk output?

If any answer is no, the crate may be useful for a baseline or tests but should not own Captastic's performance architecture.

## 26. Milestones and concrete implementation tasks

Each phase should end with tests, a short decision record, and a runnable command. Codex should implement one vertical slice at a time and avoid speculative abstractions.

### Phase 0 — Repository and measurement skeleton

Deliverable: Workspace builds on Windows and produces synthetic benchmark results.

- [x] Create workspace, crates, pinned toolchain, licenses, lints, and CI.
- [x] Define `CaptureId`, requests, modes, metadata, typed results, and errors.
- [x] Implement monotonic performance-event recorder with preallocated storage.
- [x] Implement JSON schema version 1 and human percentile summary.
- [x] Implement versioned TOML config with validation.
- [x] Implement CLI skeleton and exit-code mapping.
- [x] Add a fake backend with configurable delay, frame age, failure, and dimensions.
- [x] Add bounded trigger/clipboard/output queues and explicit overflow metrics.
- [x] Prove by test that encoding/output cannot run before `cpu_frame_ready`.
- [x] Document critical-path boundaries in ADR 0002.

Exit criteria:

- `captastic benchmark --backend fake --iterations 500 --json` succeeds.
- Event ordering, percentiles, failure counting, and queue policies are tested.
- OS-neutral crates deny unsafe code.

### Phase 1 — Windows native-frame vertical slice

Deliverable: Persistent DXGI engine captures the primary display and measures native-frame latency without readback.

- [x] Add minimal `windows` crate features for COM, DXGI, and D3D11.
- [x] Implement RAII wrappers for acquired frames and native handles.
- [x] Enumerate adapters/outputs and create stable-in-session display IDs.
- [x] Initialize D3D11 device/context and Desktop Duplication once.
- [ ] Implement finite acquisition loop and shutdown signaling.
- [x] Implement `fresh` semantics with timeout and timing provenance.
- [x] Implement rolling `latest` semantics with generation and frame-age metadata.
- [x] Handle `WAIT_TIMEOUT` and rebuild the backend after `ACCESS_LOST`/device removal.
- [ ] Add primary-display capture integration test with a known test pattern.
- [x] Add `captastic displays`, `captastic doctor`, and native-frame benchmark output.

Exit criteria:

- A 1,000-capture interactive soak completes without leaked acquired frames or unbounded handle/memory growth.
- Fresh and latest samples are separated and include frame age/timing provenance.
- No file, network, compression, or CPU readback occurs in the native-only path.

### Phase 2 — CPU frame and reusable buffers

Deliverable: Correct pooled top-left BGRA8 CPU frames with separately measured readback.

- [ ] Implement staging texture ring and pool state machine.
- [ ] Copy native texture to staging and map with explicit timing.
- [x] Implement stride-aware copy into reusable CPU slots.
- [x] Normalize or explicitly reject rotated outputs, then add all rotations.
- [x] Validate dimensions/stride/format with checked arithmetic.
- [ ] Add sequence-marker pixel correctness tests.
- [ ] Add pool saturation tests and counters.
- [ ] Report GPU copy, map wait, row copy, bytes, and total CPU latency.

Exit criteria:

- Known test-pattern pixels and orientation pass at 1080p and 4K where available.
- Normal steady-state capture reports zero emergency allocations.
- Native-frame and CPU-frame percentile tables are distinct.

### Phase 3 — Resident daemon and hotkey

Deliverable: Pressing a global hotkey triggers the already initialized engine.

- [x] Implement native `RegisterHotKey` message-loop thread.
- [x] Timestamp at `WM_HOTKEY` handler entry and nonblocking enqueue.
- [ ] Implement repeat/coalescing/full-queue behavior.
- [x] Add clean shutdown and FFI unwind protection.
- [ ] Add hotkey conflict tests.

Exit criteria:

- Hotkey capture performs no initialization work.
- The real-hotkey path emits complete event records and meets the reference hotkey-handler budget.

### Phase 4 — Clipboard-first output

Deliverable: Hotkey capture publishes an uncompressed image to the Windows clipboard, measured separately.

- [x] Implement serialized clipboard worker.
- [x] Implement and test DIBV5/DIB payload layout.
- [x] Implement bounded clipboard-open retry/deadline.
- [x] Transfer native allocation ownership safely.
- [x] Add independent clipboard read-back test.
- [x] Ensure capture success is independent of clipboard failure.
- [ ] Compare native path with `arboard` if useful; document the decision.

Exit criteria:

- Captures paste correctly into the manual compatibility applications.
- Clipboard failure never stalls or invalidates the captured frame.
- Clipboard latency and contention are reported outside native/CPU frame latency.

### Phase 5 — Native region and window selection

Deliverable: The hotkey freezes the captured display, then a native overlay copies a confirmed region or renders a confirmed native window independently of desktop occlusion.

- [x] Implement checked absolute-coordinate CPU-frame cropping.
- [x] Paint the frozen BGRA frame in a native topmost Win32 overlay.
- [x] Double-buffer all overlay visuals and present each update in one GDI copy.
- [x] Dim the frozen display while restoring the hovered window or dragged region at full brightness with a high-contrast outline.
- [x] Add editable region bounds with four corner handles, four side handles, directional cursors, source clamping, and a minimum size.
- [x] Add a floating native toolbar with original full-display, window, region, and capture icons.
- [x] Make toolbar mode controls, Capture, and the Options pop-up functional rather than decorative.
- [x] Provide enlarged native controls and a display-clamped draggable toolbar without Win32 mouse capture.
- [x] Persist the last toolbar position outside the measured hotkey-to-frame path.
- [x] Preview window mode with the same occlusion-independent native frame used for clipboard output.
- [x] Replace desktop hit-testing with a task-view-style chooser of eligible native window surfaces.
- [x] Exclude the desktop shell and omit windows that cannot provide off-screen preview pixels.
- [x] Blur the frozen desktop and arrange independent, aspect-correct window surfaces in centered rows without card chrome or external labels.
- [x] Keep window hit targets stable, remove drop shadows, and apply 4×4 supersampled antialiasing to matching surface and hover/selection corners.
- [x] Preserve full-resolution native preview surfaces until final halftone display scaling and optimize row count for larger previews.
- [x] Filter DWM-cloaked placeholders, shell surfaces, and non-task-switcher utility windows from the chooser.
- [x] Bundle Ioskeley Mono Medium under OFL-1.1, register it process-privately, enlarge overlay text, and center labels from their control bounds.
- [x] Precompose the static window chooser, suppress unchanged-target mouse repaints, and synchronously present hover-only accent updates.
- [x] Confirm a valid window preview on click and send its retained native frame directly to the clipboard workflow.
- [x] Reuse the frozen CPU frame for explicit full-display selections without allocating a crop.
- [x] Avoid Win32 mouse capture during overlay interaction and restore prior focus/cursor state for software-KVM compatibility.
- [x] Implement drag-to-select rectangular regions.
- [x] Enumerate visible top-level windows and implement hover/click selection.
- [x] Use per-monitor-v2 DPI coordinates for DXGI pixels, window bounds, and pointer input.
- [x] Add Enter confirmation and Esc/right-click cancellation.
- [x] Preserve the native `HWND` through hover, confirmation, and materialization.
- [x] Route a confirmed region crop or off-screen native window rendering to the clipboard worker.
- [x] Reject failed native window rendering instead of falling back to an occluded desktop crop.
- [x] Add a bounded selection queue and shutdown cancellation controller.
- [x] Measure selection interaction and output materialization independently of capture latency.
- [x] Verify with an automated occlusion scenario that a covering window is absent from the selected-window clipboard image.
- [ ] Validate manual behavior across multiple monitors, rotations, and mixed-DPI layouts.
- [ ] Add direct WGC window capture for applications that do not support reliable `PrintWindow` rendering.

Exit criteria:

- Region crops match requested physical-pixel coordinates and paste dimensions.
- Window selection matches enumerated native bounds and remains correct when another window covers the selected window.
- Overlay interaction cannot change native-frame or CPU-frame latency measurements.
- Cancellation leaves the previous clipboard contents unchanged.

### Phase 6 — Same-user IPC and single-instance control

Deliverable: CLI commands control the already initialized resident daemon.

- [ ] Implement same-user single-instance guard and versioned named-pipe protocol.
- [ ] Implement daemon readiness/status/stop commands.
- [ ] Route `captastic capture` through the resident daemon.
- [ ] Add malformed/unauthorized IPC tests.

Exit criteria:

- A second daemon cannot initialize another capture engine.
- `captastic status`, `captastic stop`, and CLI-triggered capture use the versioned same-user endpoint.
- Unauthorized or malformed local IPC is rejected without crashing.

### Phase 7 — Benchmark harness and performance report

Deliverable: Reproducible Windows report comparing capture semantics and, when ready, backends.

- [ ] Build sequence-marker workload.
- [ ] Collect environment fingerprint and configuration hash.
- [ ] Automate warm-up, timed iterations, raw artifact creation, and baseline comparison.
- [ ] Implement compatible-environment comparison checks.
- [ ] Add WGC persistent backend experiment.
- [ ] Run DXGI/WGC across the controlled scenario matrix.
- [ ] Profile the largest p95 contributors and document copies/allocations.
- [ ] Choose the default Windows backend per capability/scenario based on results.

Exit criteria:

- At least three repeat runs exist for the reference scenario.
- Report includes raw sample counts, failures, frame age, percentile tables, and environment details.
- Any performance claim identifies backend, mode, frame boundary, workload, resolution, refresh rate, and hardware.

### Phase 8 — Optional asynchronous PNG file output

Deliverable: A capture can be saved without changing capture latency semantics.

- [ ] Implement bounded output worker and PNG encoder.
- [ ] Implement safe filename templates and atomic finalization.
- [ ] Measure encode/write stages separately.
- [ ] Add output failure, queue-full, and collision tests.
- [ ] Demonstrate identical capture-stage results with output enabled and disabled within expected noise.

### Phase 9 — macOS proof

- [ ] Add `captastic-macos` only when implementation begins.
- [ ] Implement permission/consent diagnostics.
- [ ] Create and retain ScreenCaptureKit stream/session.
- [ ] Implement latest/fresh-equivalent semantics supported by the API and honest timing provenance.
- [ ] Add CPU-frame normalization and pasteboard path.
- [ ] Run the same benchmark schema and document non-equivalent platform semantics.

### Phase 10 — Linux proofs

- [ ] Add Wayland portal/PipeWire backend with explicit consent and restore behavior.
- [ ] Add X11 backend separately.
- [ ] Implement compositor/environment detection.
- [ ] Implement Linux clipboard lifetime ownership.
- [ ] Document global-hotkey availability and fallbacks by environment.
- [ ] Run backend-specific benchmarks without merging Wayland and X11 results.

## 27. Windows milestone acceptance criteria

The Windows-first prototype is accepted when all mandatory criteria below are met on a documented reference machine.

### 27.1 Functional

- [ ] `captastic daemon` becomes ready and remains resident.
- [ ] `captastic displays` identifies the primary display and correct physical dimensions.
- [ ] A registered global hotkey triggers capture without process or device initialization.
- [ ] `fresh` and `latest` modes both work and are labeled.
- [ ] Latest results include frame age; fresh results include timing provenance and timeout behavior.
- [ ] A correct top-left BGRA8 CPU frame is produced.
- [x] The frame is published to the clipboard without image compression.
- [ ] Optional PNG output runs on a background worker.
- [x] Topology/access loss produces recovery or a typed actionable error.
- [ ] All CLI commands and JSON outputs include a schema/version where relevant.

### 27.2 Critical-path integrity

- [ ] Trace/tests show no disk, network, compression, config parsing, display discovery, or resource creation from hotkey to frame.
- [ ] Hotkey handler performs only timestamp plus bounded enqueue.
- [ ] Normal captures reuse initialized sessions and buffers.
- [ ] Native frame, CPU readback, clipboard, encode, and file write have distinct boundaries.
- [ ] Queue overflow and every fallback allocation are counted.

### 27.3 Correctness and stability

- [ ] Known pattern, stride, orientation, and sampled pixel tests pass.
- [ ] 1,000 captures complete without acquired-frame leaks or unbounded resource growth.
- [ ] A longer 10,000-capture soak has no crash and no monotonic handle/memory leak beyond documented caches.
- [x] Clipboard readback matches the captured test pattern.
- [ ] Errors do not unwind across native callbacks.
- [ ] OS-neutral crates contain no unsafe code.

### 27.4 Measurement quality

- [ ] The benchmark artifact includes raw sample count, failures/timeouts, percentile tables, environment fingerprint, backend, mode, workload, and frame boundary.
- [ ] Warm-up samples are excluded and retained/identified separately if stored.
- [ ] Static and changing workloads are not combined.
- [ ] Latest latency and freshness are reported together.
- [ ] Three repeat runs support any claimed result.
- [ ] Initial performance budgets are met or revised transparently with a profile and ADR.

## 28. Codex implementation instructions

When using this specification as a Codex build prompt:

1. Ask Codex to implement only the next incomplete phase.
2. Require it to inspect the current repository and preserve existing work.
3. Require a short plan, then code, tests, and a verification summary.
4. Require native API references and safety comments for each unsafe boundary.
5. Require measurement fields to be added before optimizing a new operation.
6. Reject placeholder platform backends and speculative abstractions.
7. Keep each change runnable; the fake backend should remain usable when an interactive desktop is unavailable.
8. Do not accept a generic screenshot crate as a replacement for the Windows-native milestone.
9. Treat numeric improvements as unproven until raw before/after benchmark artifacts exist.
10. End each phase by updating the relevant checklist and ADR, without marking unmet acceptance criteria complete.

Suggested first build prompt:

> Implement Phase 0 of `Captastic-Specification.md`. Create the minimal Rust workspace, core request/result/frame/metrics types, versioned configuration, CLI skeleton, bounded queues, deterministic fake backend, and synthetic benchmark command. Add tests for event ordering, frame-age semantics, queue overflow, and the rule that output work cannot occur before `cpu_frame_ready`. Do not add native capture or empty future-platform crates yet. Run formatting, linting, and tests, then report created files and remaining Phase 0 criteria.

## 29. Decision log required before expansion

Create ADRs for:

- Native platform backends rather than a generic screenshot abstraction.
- Definitions of native-frame, CPU-frame, fresh capture, latest capture, and frame age.
- DXGI versus WGC selection after measurements.
- Windows clipboard format and ownership strategy.
- Buffer-pool size and saturation policy.
- IPC authentication and message versioning.
- HDR/color-space policy before claiming HDR support.
- Wayland global-hotkey and consent behavior before promising feature parity.

## 30. Authoritative technical references

- Microsoft: [Desktop Duplication API](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)
- Microsoft: [`IDXGIOutputDuplication::AcquireNextFrame`](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/nf-dxgi1_2-idxgioutputduplication-acquirenextframe)
- Microsoft: [Windows Graphics Capture overview](https://learn.microsoft.com/en-us/windows/uwp/audio-video-camera/screen-capture)
- Microsoft: [Rust for Windows](https://github.com/microsoft/windows-rs)
- Apple: [ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit)
- XDG Desktop Portal: [ScreenCast interface](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
- XDG Desktop Portal: [PipeWire integration](https://flatpak.github.io/xdg-desktop-portal/docs/pipewire.html)
- Rust candidates: [`global-hotkey`](https://docs.rs/global-hotkey/), [`arboard`](https://docs.rs/arboard/), [`ashpd`](https://docs.rs/ashpd/), [`pipewire`](https://docs.rs/pipewire/), [`x11rb`](https://docs.rs/x11rb/), [`objc2-screen-capture-kit`](https://docs.rs/objc2-screen-capture-kit/)

---

## 31. Final scope statement

Captastic milestone one succeeds when it produces trustworthy latency evidence from a warm Windows-native capture engine. A correct frame, honest freshness measurement, explicit copy/readback costs, and a reproducible benchmark are more important than feature count. Cross-platform support begins with narrow shared contracts and independent native backends; it does not begin with hiding platform behavior behind a convenient but opaque screenshot call.
