# ADR 0010: Pinning and annotation

## Status

Accepted for the Windows prototype, 2026-10-15. Governs Milestone 6, the 0.3.0 theme.

## Context

Milestone 6 adds the first tools that act on a capture after it is taken: a pinned window that keeps
a capture on screen, and an editor that crops, redacts and annotates one. Both are downstream of the
capture engine and must leave it unchanged when they are not used. Before any code, five questions
shape every slice after them, and the code answers some of them already:

- **The overlay toolbar is full.** `TOOLBAR_WIDTH` is 418 DIP and **Capture** ends 6 DIP from its
  right edge (`overlay/layout.rs`). A new action means a wider toolbar, an Options row, or a key.
- **Nothing hosts a window that outlives the overlay.** The overlay's message loop runs on the
  `captastic-selection` worker and ends when the overlay is destroyed; the tray and the hotkey own
  their own threads; the daemon's main loop has no message pump.
- **A confirmed full-display selection shares a pool slot.** The CPU readback pool has three slots
  (`DEFAULT_CPU_BUFFER_SLOTS`), and a slot is reusable only when its `Arc` has no other owner.
  `CpuFrame::crop` and GPU region materialization allocate fresh buffers, but a full-display
  confirmation is the snapshot itself. The v0.2.0 soak showed what a long-held lease costs:
  `BufferExhausted` refusals.
- **Captastic's own windows are already invisible to its window chooser and edge snapping.**
  `window_enumeration.rs` rejects every window of the current process before any other filter.
- **Only file output writes history.** A clipboard-only capture leaves no history entry to reopen.

## Decision

**Pin and Edit are actions on the overlay, alongside Capture.** The toolbar gains a **Pin** and an
**Edit** button beside **Capture**, widening the toolbar rather than burying them in Options.
**Shift+Enter** pins and **Ctrl+Enter** edits, as **Enter** captures. Both apply to every tool —
full display, window and region. A run with no daemon behind it (the developer CLI's one-shot
`capture --selection true`) shows them greyed, as it does the **Save to Disk** row, because nothing
would host the window afterwards.

**Pin publishes the capture as usual and also pins it.** Pinning is a way of looking at a capture,
not a reason to withhold it, so a pinned capture goes to the clipboard and to disk exactly as
**Capture** would have sent it.

**Edit withholds the capture until the edit is done.** Choosing **Edit** publishes nothing. **Done**
in the editor publishes the edited image through the destination set as that capture, and
cancelling the editor publishes nothing at all. This is what makes redaction worth having: a secret
that has already reached the clipboard, the clipboard history of another tool, or a file on disk is
not redacted by anything an editor does afterwards.

**Pins and editors are concurrent.** Any number of pinned windows, and any number of editors, may be
open while the hotkey keeps working, and a new overlay run neither waits for them nor closes them.
They live on one new UI thread, `captastic-desktop-ui`, with its own message pump. It is started on
the first Pin or Edit and not before, so a daemon that never pins or edits never creates it. It is
registered with the worker registry and stopped at shutdown under ADR 0005's detach rules like any
other worker.

**Pins are visible on screen and in screenshots, but never a capture target.** A pinned window does
not use `WDA_EXCLUDEFROMCAPTURE`: it appears in display and region captures like anything else on
screen, and in other applications' screen sharing and recording, which exclusion would also blind.
It is never offered by the window chooser and never snapped to, which the process-ID filter already
guarantees; a test pins that guarantee so a future change to the filter cannot quietly drop it.

**Pins and editors own copies.** The selection worker deep-copies the confirmed frame into a freshly
allocated buffer before handing it to the UI thread, whatever tool produced it. Neither a pin nor an
editor ever holds a CPU readback slot, so neither can cause a `BufferExhausted` refusal however long
it stays open.

### What each window offers

- **A pinned window** shows the capture at its physical pixel size, always on top, and can be moved,
  closed, made translucent, and made click-through. A click-through window cannot be clicked to undo
  that, so the notification area gains **Close All Pins** and **Make Pins Clickable**; a pin is never
  left impossible to get rid of.
- **The editor** opens on the capture with crop and destructive redaction first (slice 2), then
  arrows, rectangles and highlights (slice 3), then text (slice 4). Redaction overwrites the
  exported pixels with an opaque fill. It does not blur or pixelate: both leave enough of the
  original to be reversed or guessed, and a redaction that can be undone is not one.
- **History integration** (slice 5) needs a capture to reopen. Because only file output records
  history, reopening a clipboard-only capture is out of scope until that slice decides whether the
  last capture is kept in memory.

### What an edited capture is

The edited image is published as the capture it came from — the same capture ID, display, source
window and timestamp — so the filename template, history, and logs describe it the way they would
have described the original. Its dimensions are the edited dimensions. It is logged as edited, and
the original's pixels are not written anywhere.

## Consequences

- The toolbar becomes wider. Its layout, hit testing, and the DPI tables in
  `docs/overlay-ui-verification.md` all change in slice 1, and the 200 % toolbar must still fit on a
  1920-pixel-wide display.
- A daemon with open pins or editors holds one more thread and a full-resolution copy of each
  capture. That memory is the price of a window that cannot starve the capture path.
- Shutdown must close pins and editors. An editor with unpublished work is closed without
  publishing, which loses it; a later slice may ask first, but shutdown never waits on a person.
- The capture metrics of a press that ends in **Capture** are unchanged: the UI thread does not
  exist until the first Pin or Edit, and nothing on the capture path waits for it afterwards.
- A redacted capture is only as private as its route. If the user chooses **Capture** instead of
  **Edit**, nothing is withheld; the release notes and README say so plainly.

## Slices

As listed under Milestone 6 in `ROADMAP.md`. Slice 1 (pinning) builds the UI thread, the frame
copy, the toolbar widening and the shortcuts. Slice 2 (the editor with crop and redaction) adds the
withheld publish and the editor model.
