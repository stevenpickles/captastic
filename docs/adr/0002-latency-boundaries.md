# ADR 0002: Latency boundaries

## Status

Accepted for Phase 0. Amended for Milestone 4 (destinations fan out).

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

## Amendment: destinations are parallel tracks, not later stages

Stages 1–4 are a pipeline: each genuinely happens after the one before it, for one capture, on one thread. Stages 5–7 were written as a continuation of that line, because when this was decided there was one destination and a linear ordering cost nothing to assume.

Milestone 4 adds a second. Clipboard publication and file output are independent destinations for the same frame, and its exit criteria require that they *stay* independent — "clipboard success remains independent of file-output failure and vice versa". Ordering them against each other would mean either serializing two things that have no reason to wait for one another, or recording an order that the implementation is free to violate on any given capture. Neither is a contract worth keeping.

So the pipeline **fans out after CPU-frame readiness**. Stages 1–4 remain a strict order. Beyond them, each destination is its own track:

- Every destination receives its own trace, carrying the shared prefix (stages 1–4, plus selection where it applies) followed by that destination's own events and its own `AttemptFinished`.
- Traces for one capture share a `capture_id` and a time origin, so they remain directly comparable and can be interleaved by a consumer that wants the whole picture.
- Ordering is enforced *within* a track. A clipboard event is never ranked against a file-output event, because there is no answer that is true for every capture.
- A capture delivered to two destinations therefore emits two complete traces. A consumer counting attempts must count distinct `capture_id`s rather than `AttemptFinished` events.

What does not change: disk, network, and compression stay forbidden before CPU-frame readiness, and no destination's latency is ever included in native-frame or CPU-frame latency. Those are the boundaries this ADR exists to defend, and fanning out does not touch them.
