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

Every legal event order is preserved and labelled:

```text
frozen:   trigger -> capture -> selection -> confirmation -> materialization
live:     trigger -> selection -> confirmation -> capture -> materialization
snapshot: trigger -> capture -> selection -> confirmation -> capture -> materialization
```

The third order is what a run takes when it captures at the hotkey so the user can look at those
pixels, and the user confirms in the live view instead. Its opening is indistinguishable from
`frozen`; the capture request arriving after the confirmation is what tells the two apart, and it
re-anchors the trace to the `live` table from the confirmation onwards, where both orders describe
the same confirmation-anchored capture. A confirmation arriving after that point is still an
out-of-order event.

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

## Amendment (v0.2.0): the view is the user's, taken at the hotkey

### Context

The decision above settled which pixels a selection would publish before the overlay appeared.
`frozen` captured at the trigger and cropped that frame; `live` captured nothing until the user
confirmed. Both are defensible and each is wrong for a different task: choosing a region over a
playing video needs the picture to stop, and choosing one over a menu that closes when the overlay
takes focus needs it to keep going. The user could not ask for either, because by the time they
were looking at anything the choice had already been made for them — once, in a configuration file.

Two facts made the split unnecessary. The overlay's live presenter already runs the whole compose →
`UpdateLayeredWindow` cycle, takes pointer capture, and registers DWM thumbnails against a layered
window; nothing in the frozen branch depended on non-layered semantics. And the trigger-time
capture the frozen path always took costs, in `latest` mode, one `CopyResource` plus readback and a
memcpy into the DIB — about a millisecond at 4K, already measured and reported as
`overlay_preparation_ns` and `cpu_ready_offset_ns`.

### Decision

Every overlay trigger captures at the press. One layered window serves both views. The overlay
opens in the view `selection.preview` names and the user switches with `F` or **Options → View**;
confirmation materializes from whichever view was on screen — the frozen view from the trigger
snapshot, the live view from a capture taken at the confirmation.

`selection.preview` therefore chooses only the opening view. `auto` and `live` open live and differ
only in whether an unavailable layered presenter is a fallback or an error.

The switch is per run. It is not persisted, and there is **no new configuration knob** for it:
which view suits a capture is a property of that capture, not of the machine, and a remembered
answer would be wrong about half the time while adding a setting whose effect the `F` key already
has. If a measurement ever shows the trigger capture is too expensive to take on every press — on a
much larger desktop, or a machine where `CopyResource` is not close to free — the escape hatch is a
`selection.snapshot = false` that suppresses it, leaving `live` exactly as it behaves today with
the toggle unavailable. It is not added now because nothing has measured a reason for it.

The Window tool is unaffected. Clicking a preview has always requested a fresh full-resolution
native render of that window, so the view behind the chooser changes nothing the user would get;
`F` is inert there and the Options row is greyed.

`CaptureCommand::FrozenSelectionFallback` is removed. A live presenter that will not establish
itself no longer costs a cross-thread round trip and a second full capture to reopen frozen: the
overlay already holds the trigger snapshot, so it destroys its window, drains the latched quit, and
re-creates opaque around the pixels it has, with the view locked and the toggle retracted. The
`fallback` capture anchor goes with the capture it named.

Per press, memory is what the frozen path has always used: one CPU frame, plus the retained GPU
texture that every overlay press other than `full_display` asks for — `window` included, because a
tool switch inside the overlay can still land on Region and need it — held for the life of the
overlay. A live-view confirmation releases both before the job returns to the capture thread, so
the snapshot and the confirmation capture are never resident at once.

What did change is how long that frame is pinned, and therefore how many the capture engine's
readback pool must cover. A pooled slot is recycled only when nothing else holds it, and an
overlay now holds one for the length of a human interaction rather than the length of a capture.
The pool is sized from `capture.buffer_slots` plus `selection.queue_capacity` plus one for the
overlay on screen, instead of the fixed three that a `full_display` capture and two open or queued
selections could exhaust between them. Slots are allocated on first use, so the larger ceiling
costs nothing until that many frames really are in flight; at 4K each is about 32 MiB, and the
default configuration raises the ceiling from three slots to five. Exhausting the pool on a press
or a confirmation is now reported to the notification area and as `selection_failed`, rather than
to the log alone: an overlay left open is something the user can see and close.

### JSON

`preview_mode` is now the view that was showing at the confirmation (`live` or `frozen`) rather
than a restatement of the anchor. `capture_anchor` is `trigger` or `confirmation`; `fallback` is
gone. `view_switched` is appended, because a run that ends in the view it opened in cannot
otherwise be told from one that was switched and switched back. `requested_preview_mode` and field
order are unchanged.
