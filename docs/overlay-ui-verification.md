# Overlay UI verification

This guide defines the layout contract and manual verification matrix for the
native selection overlay. The layout code remains presentation-only: it consumes
already-frozen selection geometry and does not participate in acquisition,
screenshot materialization, PNG/DIB preparation, or clipboard publication.

## Why the layout changed

The dimension badge was always centered inside the selection. That covered much
of a small region and could obscure the pointer and resize handles.

The toolbar and Options menu were DPI-aware, but their 100% baselines were
600 x 82 and 320 x 164 DIPs, with 21-DIP menu type, 62-DIP-tall tool controls,
large icons, and padding encoded separately in painting and hit-testing code.
DPI scaling correctly magnified an already oversized baseline.

The overlay layout module now owns deterministic geometry and small groups of
DPI-scaled tokens. Painting and hit testing consume the same rectangles.

## Region-dimension badge contract

The badge always shows the selected rectangle's exact physical-pixel width and
height. DPI affects only its typography, padding, gaps, and avoidance distances.

The placement algorithm:

1. Measures the final Ioskeley Mono text before choosing a position.
2. Uses the centered inside position when the selection has comfortable room
   beyond the label, padding, and resize-handle clearance.
3. Otherwise evaluates top, bottom, right, and left candidates.
4. Rejects candidates that cannot fit within the active monitor inset, then
   scores overlap with the toolbar, open Options menu, pointer/crosshair, and
   resize-handle clearance.
5. Clamps the winning badge to the active monitor.
6. Retains the current side while it remains valid and uses an 8-DIP
   inside/outside hysteresis band, preventing one-pixel placement jumps.

For an extremely small selection, the badge remains outside on the best valid
side. At a display edge, the unavailable side is rejected. If every ideal
candidate is constrained, the best candidate is clamped inside the monitor;
exact dimension text is never altered. Signed monitor origins are preserved.

The badge uses 15-DIP Ioskeley Mono type, 10-DIP horizontal padding, 5-DIP
vertical padding, an 8-DIP corner radius and selection gap, and DPI-scaled
resize-handle geometry.

## Compact toolbar contract

| Element | DIP baseline |
| --- | ---: |
| Toolbar | 418 x 56 |
| Tool control hit target | 44 x 44 |
| Options control | 100 x 44 |
| Capture control | 120 x 44 |
| Options menu | 248 x 172 |
| Menu row | 236 x 40 |
| Toolbar/menu type | 16 |
| Primary icon | 22 |
| Tooltip minimum height | 32 |

Expected physical sizes are:

| Scaling | DPI | Toolbar (px) | Options menu (px) |
| --- | ---: | ---: | ---: |
| 100% | 96 | 418 x 56 | 248 x 172 |
| 125% | 120 | 523 x 70 | 310 x 215 |
| 150% | 144 | 627 x 84 | 372 x 258 |
| 200% | 192 | 836 x 112 | 496 x 344 |

This is not a uniform shrink. Tool buttons retain 44-DIP pointer targets, while
the grip, separators, icons, corners, padding, and gaps use purpose-specific
tokens. Labels are centered in their text rectangles. Dropdown labels are
vertically centered and left-aligned beside checkmarks. Tooltips measure their
text, add tokenized padding, and clamp to the monitor work area.

The Options menu prefers the side of the toolbar with sufficient work-area
space, falls back to the other side, and clamps on both axes. Toolbar dragging
continues to persist its normalized work-area center and cannot move the toolbar
outside the active monitor's work area.

## Automated acceptance criteria

The test suite proves that:

- comfortable selections place the badge inside;
- tiny and edge-touching selections choose a valid outside side;
- toolbar/menu and pointer exclusions affect the initial side;
- a previous outside side is retained while valid;
- the inside transition uses hysteresis;
- negative monitor origins remain bounded;
- toolbar, menu, controls, and hit targets scale at 96, 120, 144, and 192 DPI;
- the popup chooses an available side and remains in a signed work area;
- toolbar persistence restores and clamps with compact bounds;
- switching away from Region and back restores the latest adjusted rectangle;
- cancellation persists the selected tool and latest region for the next overlay;
- resize-handle hit targets scale with monitor DPI;
- actual Ioskeley Mono glyph measurements fit Options, Capture, and every
  dropdown row at all four target DPI levels;
- the Options menu's rows tile it exactly, with no gap or overlap, at all four
  target DPI levels;
- snapping moves an edge at most the threshold and always onto a target edge,
  is idempotent, keeps a moved region's exact dimensions, and never overrules
  the display clamp or the minimum region size;
- a snapped right or bottom edge is the target's exclusive edge, and a guide is
  emitted only for a coordinate the surviving rectangle sits on;
- a distance tie resolves to the earlier (topmost) target, and the display
  never wins one;
- Ctrl and the disabled option both produce the unsnapped rectangle and no
  guides;
- arrow nudges stay inside the display and above the minimum size for any run
  of presses, never snap, and stop emitting repaints at a display edge.

Changes to these invariants belong in the pure layout helpers or their tests
before Win32 painting changes.

## Manual test matrix

For each row, exercise Region, Full Display, Window, Options, tooltip hover,
toolbar drag, capture confirmation, Escape cancellation, and right-click
cancellation. In Region mode, draw a large rectangle, draw one smaller than the
badge, resize through the inside/outside threshold one pixel at a time, move the
pointer around every side, touch all display edges, and use all eight handles.
Move or resize the region, switch to Window and Full Display and back, and verify
the exact live rectangle returns. Select Region, cancel without capturing, then
reopen the overlay and verify both Region and that rectangle are restored.

For region precision, on every display configuration below:

| Check | What to do | What must happen |
| --- | --- | --- |
| Snap to a window edge | Draw a region and bring its right edge to within a few pixels of a window's right border | The edge jumps onto the border, a hairline accent guide runs the full height of that window, and the badge's width is the one that puts the region's last column on the window's last column |
| Snap while resizing | Grab the east handle and approach the same border | Only the right edge moves; the left edge stays put even when it is also near a border |
| Snap while moving | Drag the whole region near a window's left border | The region lands on the border and the badge's dimensions do not change |
| Ctrl releases it | Hold Ctrl and repeat any of the above | Nothing is pulled anywhere, the guide disappears while Ctrl is down, and releasing the button keeps the pixel under the pointer |
| Guide lifetime | Release the button on a snapped edge | The guide disappears; the rectangle does not move |
| Turn it off | Options -> Snap to Edges, then repeat | No snapping and no guides; reopen the overlay and the row is still unchecked; `state.toml` contains `snap_to_windows = false` |
| Nudge | With a region selected, press the arrow keys, then with Shift, then with Ctrl | One pixel, ten pixels, and a resize of the right/bottom edges; the badge tracks each press; holding a key repeats; a key held at the display edge changes nothing |
| Nudge does not snap | Nudge an edge to one pixel off a window border | It stays exactly where it was put |

| Display configuration | Scaling | Required checks |
| --- | --- | --- |
| 1280 x 720 landscape | 100% | Controls remain readable; menu and tooltips fit; tiny edge selections keep a visible badge. |
| 1920 x 1080 landscape | 125% | Text is centered; no clipping; the retained badge side stays stable during slow resize. |
| 2560 x 1440 landscape | 150% | Icons and hover/selected states remain crisp; toolbar persistence restores proportionally. |
| 3840 x 2160 landscape | 200% | Hit targets remain comfortable; label, handles, menu, and tooltips scale together. |
| Rotated portrait display | 100% and 150% | Restored region follows rotation; badge sides and clamping remain correct. |
| Secondary left/above primary | mixed DPI | Negative origins do not displace badge/menu; each target uses its own DPI and saved position. |
| Small display, taskbar on each edge | native DPI | Toolbar/menu remain in the reduced work area; tooltips choose a visible side. |
| Large beside small high-DPI display | mixed DPI | Each invocation is stable and never inherits another monitor's pixels. |

Also verify that the badge matches the copied image's physical dimensions,
including one-pixel resizes. Compare a representative capture before and after
this change to ensure screenshot pixels, clipboard formats, and
selection/clipboard timing events are unchanged.

