# Multi-monitor capture plan

This document defines the implementation order and invariants for Captastic's
multi-monitor capture work. Every slice must leave the workspace buildable and
must include tests for behavior that does not require an interactive desktop.

## Product behavior

Captastic supports four display policies:

- `primary`: target the current Windows primary display.
- `pointer`: target the display containing the pointer when a capture action is dequeued.
- `display:<persistent-id>`: target one configured physical display.
- `virtual_desktop`: capture the union of all attached desktop displays.

A single-display overlay is locked to its resolved target for the lifetime of
that capture action. Moving the pointer to another display does not move the
active overlay or toolbar. A topology change while an overlay is open cancels
the action without changing the clipboard, rebuilds the capture sessions, and
allows the next action after the engine reports ready.

## Coordinate and image invariants

- Desktop and monitor origins are signed physical-pixel coordinates. Negative
  coordinates are valid.
- Rectangle right and bottom edges are exclusive.
- Captured images preserve source pixels; Captastic does not implicitly scale
  one display to match another.
- Backends normalize 0, 90, 180, and 270 degree rotations into a top-left BGRA
  frame before downstream selection, preview, clipboard, or composition code.
- Virtual-desktop output is translated to an image origin of `(0, 0)`. Gaps in
  non-rectangular monitor layouts are opaque black.

## Persistent identity and topology

Persistent UI state is keyed by a Windows display device path obtained through
the Display Configuration API, not by a DXGI adapter/output enumeration index.
Runtime routing may additionally retain the adapter LUID and target ID, but
those values are not configuration keys.

Each immutable topology snapshot has a monotonically increasing generation.
Capture requests and overlay sessions retain the generation they resolved
against so stale work can be rejected after a display change.

## Per-display UI state

Each physical display independently remembers:

- last selected capture tool, including a selection followed by cancellation;
- last adjusted partial-region rectangle, whether or not it was captured;
- toolbar placement.

Tool and region interaction state is saved on both confirmation and cancellation.

Regions are stored in monitor-local physical pixels. Toolbar placement is stored
as its normalized center within the display work area so it survives DPI and
resolution changes. If a saved region no longer fits, retain its pixel size and
relative center when possible, then clamp it. A 90 or 270 degree rotation rotates
the center and swaps the region width and height. With no saved region, use a
centered rectangle covering half the display width and half the display height.

The toolbar behavior is:

1. Show exactly one toolbar on the target display.
2. Default to bottom-center above that display's work area edge.
3. Size text and hit targets in device-independent pixels using that display's DPI.
4. Clamp the toolbar fully inside the target work area.
5. Do not allow dragging it onto a different display during the same invocation.
6. Retain saved state for a disconnected display so reconnecting restores it.

Region dimension placement also uses display-local physical coordinates and the
target display's DPI. The measured badge remains inside a comfortable region;
small regions use a stable outside side selected from the space available on
that display. The badge is clamped to the display bounds and avoids the pointer,
resize handles, toolbar, and open menu where practical. Exact region dimensions
are never converted to DIPs.

The compact toolbar uses a 418 x 56 DIP shell with 44 x 44 DIP tool hit targets
and a 248 x 132 DIP Options menu. Painting, hit testing, popup placement, and
work-area clamping share the same per-monitor metrics.

For virtual-desktop capture, use one overlay window per display in a shared
overlay session. Show one toolbar on the display that contained the pointer at
invocation time and use that display's saved placement.

## Window ownership

Enumerate eligible windows once per window-mode invocation and assign every
window to exactly one display: the display with the largest visible intersection
area. Resolve an exact tie deterministically using the window's monitor and then
the stable display ID. A spanning window appears in one chooser but its complete
window image is previewed and captured. Window handles and window lists are
ephemeral and are never persisted.

## Implementation slices

1. **Core topology model**: rectangle operations, immutable topology snapshots,
   deterministic display resolution/ownership, and a source-aware multi-display
   fake backend.
2. **Configured display**: parse and validate display policy, honor configured
   fixed-display selection end to end, and report available persistent IDs.
3. **Pointer display manager**: retain DXGI resources for eligible outputs and
   resolve pointer targeting without initialization on the trigger path.
4. **Per-display UI state**: migrate toolbar, tool, and region persistence into
   display-keyed TOML tables and position the menu using work area and DPI.
5. **Window partitioning**: freeze one eligible-window snapshot and partition it
   by the ownership rule above.
6. **Rotation normalization**: normalize all supported DXGI rotations and record
   transform timing independently from native acquisition and readback.
7. **Topology recovery**: handle attach, removal, primary, resolution, work-area,
   and DPI changes with generation-based cancellation and resource rebuilds.
8. **Virtual desktop**: compose normalized per-display frames, report frame age
   and maximum timestamp skew, and expose full-virtual-desktop capture.

Cross-monitor arbitrary region selection follows full virtual-desktop capture;
it is not part of the first virtual-desktop milestone.

## Definition of done for every slice

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- Windows release build succeeds.
- New failure paths include actionable structured logs.
- No disk access, network access, image compression, display discovery, or GPU
  resource creation is added to the hotkey-to-retained-frame critical path.
- The slice is committed separately with a message explaining the behavioral or
  architectural reason for the change.
