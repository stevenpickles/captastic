# ADR 0002: Latency boundaries

## Status

Accepted for Phase 0.

## Decision

Captastic records these stages separately:

1. hotkey/trigger receipt;
2. bounded enqueue and capture-thread dequeue;
3. native capture request and native frame ready;
4. GPU/compositor readback and CPU frame ready;
5. clipboard publication;
6. encoding;
7. file writing.

Disk, network, compression, configuration parsing, display discovery, and resource creation are forbidden before CPU frame readiness. Clipboard, encoding, and file-output latency are never included in native-frame or CPU-frame latency.

Phase 0 enforces event order and uses typed `CpuFrame` values for output jobs. Native phases will add platform-specific sentinels and profiles.

