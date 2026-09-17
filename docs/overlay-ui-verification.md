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
   scores overlap with the toolbar, open Options menu, magnifier, pointer/
   crosshair, and resize-handle clearance. The magnifier does not move for the
   badge: it is anchored to the pointer and the badge is not.
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
| Options menu | 248 x 252 |
| Menu row | 236 x 40 |
| Toolbar/menu type | 16 |
| Primary icon | 22 |
| Tooltip minimum height | 32 |

Expected physical sizes are:

| Scaling | DPI | Toolbar (px) | Options menu (px) |
| --- | ---: | ---: | ---: |
| 100% | 96 | 418 x 56 | 248 x 252 |
| 125% | 120 | 523 x 70 | 310 x 315 |
| 150% | 144 | 627 x 84 | 372 x 378 |
| 200% | 192 | 836 x 112 | 496 x 504 |

The magnifier is a fixed 31 x 31 square of physical pixels, enlarged 6, 8, 9
and 12 times at 100%, 125%, 150% and 200%. The sampled square is deliberately
not DPI-scaled: there are no more real pixels around the pointer at 200%, and
the point of the magnifier is to show the pixels the capture will contain.

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
  of presses, never snap, and stop emitting repaints at a display edge;
- the magnifier appears only after the pointer has been slow for the sustain
  period, hides at once above the fast threshold, and holds its state inside
  the band between the two, in both directions;
- Auto requires an adjustment in flight, the key does not, and Off answers no
  to both;
- a rest tick shows it for a pointer that has stopped, and a tick that changes
  nothing emits no repaint;
- the message clock wrapping does not make a fast pointer look stationary, and
  several messages sharing one millisecond accumulate instead of dividing by
  zero;
- the magnifier stays inside the monitor and never overlaps its own source
  square, for any pointer position on any monitor at all four scalings;
- enlargement is exact nearest-neighbour: a magnified pixel is a solid block of
  its source colour and no colour appears that was not in the source;
- a selection edge maps to the pixel boundary it occupies, and only edges
  inside the sampled square are drawn.

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
| Magnifier on the key | Hold Z in Region mode, with and without a drag in flight | It appears beside the pointer, never over it, and disappears on release |
| Magnifier on a slow drag | Drag an edge and slow to a deliberate pace | It appears by itself after about a tenth of a second and stays while the pointer is slow |
| Magnifier on a flick | Flick the pointer across the screen mid-drag | It disappears at once and does not flicker back on the way down |
| Magnifier at rest | Stop the pointer dead mid-drag without releasing | It appears; the mouse has stopped sending messages, so this is the rest timer doing its job |
| Magnifier and a snapped edge | Snap the region's right edge to a window's right border, then hold Z | The accent guide and the selection edge in the magnifier fall on the same pixel boundary, and the last pixel inside the selection is the window's last pixel |
| Magnifier placement | Take the pointer into each corner of the display while holding Z | It flips side and stays on screen; it never covers the square of pixels it is magnifying |
| Magnifier in the live view | With `selection.preview = "live"`, hold Z over moving content | The sample is undimmed desktop that updates, with **no trace of the overlay's own dimming, outline, toolbar or magnifier** in it |
| Magnifier vs the badge | Move the pointer so the magnifier would land on the dimension badge | The badge moves; the magnifier does not |
| Zoom modes | Options -> Zoom, pressing three times | Auto, Hold Z, Off, back to Auto; Hold Z ignores a slow drag, Off ignores the key; `state.toml` contains `region_zoom` and it survives a daemon restart |
| Alt+Tab with Z held | Hold Z, Alt+Tab away, release Z, Alt+Tab back | The magnifier is gone rather than stuck on screen |

For the frozen-view toggle, on every display configuration below:

| Check | What to do | What must happen |
| --- | --- | --- |
| F freezes the view | With a video playing, press the hotkey, then press **F** | The video stops, and a **FROZEN · pixels from hotkey press** tag appears above the toolbar |
| F returns to live | Press **F** again | The tag disappears and the video resumes from where it is now, not where it was |
| The frozen view captures the press | Freeze with **F**, draw a region over the video, confirm | The clipboard holds the frame from the moment the hotkey was pressed; JSON reports `preview_mode: "frozen"`, `capture_anchor: "trigger"`, `view_switched: true` |
| The live view captures the confirmation | Return to live, draw the same region, confirm | The clipboard holds the current frame; JSON reports `preview_mode: "live"`, `capture_anchor: "confirmation"` |
| Full Display too | Repeat both in Full Display mode | The same, with `frozen_display` and `confirmation_display` |
| The Window tool is unaffected | Switch to Window and press **F** | Nothing happens, no tag appears, and **Options -> View** is greyed |
| The row and the key agree | Open Options and click the **View** row repeatedly | The label alternates "View: Live" and "View: Frozen", the menu stays open, and the tag follows |
| Nothing is remembered | Confirm or cancel in the frozen view, then press the hotkey again | The overlay opens live again (or frozen under `selection.preview = "frozen"`); `state.toml` gains no new key |
| The configured opening view | Set `selection.preview = "frozen"` and press the hotkey | The overlay opens frozen with the tag up; **F** still switches to live |
| A greyed row means it | If the layered presenter ever fails (log: "layered selection presenter failed") | The overlay still opens frozen in an opaque window, the row is greyed, **F** does nothing, and JSON carries `preview_fallback_reason` |
| The magnifier follows the view | Hold **Z** over moving content in each view | Live: the sample updates with the desktop. Frozen: the sample is still, and matches the frozen picture underneath |

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

