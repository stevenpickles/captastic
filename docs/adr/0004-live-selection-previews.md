# ADR 0004: Live selection previews

## Status

Accepted for the Windows prototype.

## Context

Captastic currently captures a desktop frame before it opens the selection overlay. That makes
the preview and resulting pixels identical, but it also freezes video, animation, and other
desktop changes while the user chooses a region. The window chooser similarly stores CPU-rendered
thumbnails even though Desktop Window Manager can compose live thumbnail relationships.

The two preview kinds need different native mechanisms. A region preview is the real desktop seen
through a transparent selection surface. A window preview is a DWM-owned rendering of one source
window into Captastic's destination window. Neither mechanism should change which pixels are
published after confirmation.

## Decision

Selection preview behavior is explicit:

- `frozen` captures at the trigger and selects from that immutable frame;
- `live` selects against current desktop composition and captures after confirmation;
- `auto` prefers `live` and falls back to `frozen` when the required compositor behavior is not
  available.

Live region and full-display selections use display metadata to construct the overlay without a
CPU frame. The overlay is removed from composition before capture is requested. The confirmation
time becomes the capture trigger, and the configured `fresh` or `latest` policy is evaluated from
that time. Captastic records both the original interaction trigger and the confirmation-anchored
capture latency. Cancellation performs no pixel capture.

The live overlay is excluded from supported Windows capture APIs as defense in depth. Correctness
does not rely on exclusion alone: Captastic destroys the overlay and synchronizes its compositor
updates before acquiring the confirmed pixels. A display topology or bounds change invalidates the
selection instead of silently translating it to a different source.

Window chooser previews use DWM thumbnail registrations where possible. These thumbnails are
display-only compositor relationships; Captastic does not treat them as captured pixels. A click
still requests a fresh full-resolution native window capture. Static `PrintWindow` or Windows
Graphics Capture thumbnails remain the per-window fallback when DWM registration is unavailable.

`repeat_last_region` remains an immediate, overlay-free capture. It has no preview phase and uses
the request-time frame just as it does today.

## Metrics

Both legal event orders are preserved and labelled:

```text
frozen: trigger -> capture -> selection -> confirmation -> materialization
live:   trigger -> selection -> confirmation -> capture -> materialization
```

Every result reports its effective preview mode, any fallback reason, and whether capture timing is
anchored to the trigger or confirmation. Human interaction time is never reported as native capture
latency.

## Consequences

The capture thread remains the sole owner of persistent DXGI resources. The selection worker sends
confirmed coordinates back to that thread rather than acquiring frames itself. Overlay rendering
must support both an opaque frozen-frame presenter and a transparent live presenter while sharing
input, layout, DPI, persistence, and shutdown behavior.

Explicit `live` mode fails when its required behavior cannot be established. `auto` may reopen the
selection with the frozen presenter after a bounded fallback capture. Protected, cloaked, closed,
or otherwise unavailable window sources may use static previews or be omitted, but Captastic never
substitutes an occluded desktop crop for native window capture.
