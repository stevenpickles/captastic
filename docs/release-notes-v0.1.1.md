# Draft — GitHub release body for v0.1.1

> **This file is a draft of the release body for the `v0.1.1` GitHub release.** It is not the release
> itself. Paste the content below the horizontal rule into the release description when the tag is
> published, confirming that the attached artifact names match. This file lives in the repository so
> the wording can be reviewed before it is published.

---

Captastic v0.1.1 is a patch release for people who move a laptop between docks. It carries one
change set, PR #83, on top of v0.1.0. Everything v0.1.0 said about what Captastic is and is not
still holds; see the [v0.1.0 release](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0)
for the full description, install steps, and known limitations. This release publishes no
performance numbers.

## What was wrong

After moving between two monitor setups, the first hotkey capture on the new arrangement could end
with nothing on the clipboard and nothing on screen to say so, and the per-monitor remembered tool,
region, and toolbar position did not come back until a later press. v0.1.0 already closed the two
main causes: display ids are derived from the panel's EDID rather than from how it is plugged in,
and a live selection validates the display list before the overlay opens. An adversarial trace of
that code found four narrower ways a first press could still be lost, three of them silent. v0.1.1
closes them.

## What changed

- **A confirmation capture that loses its display is reported.** When the capture engine is rebuilt
  underneath an open selection and comes back bound to a different display, or the selected display
  is no longer attached, the daemon now raises the same notification-area balloon a geometry
  mismatch raises. The check is keyed on the exact refusal, so a display that merely cannot be
  duplicated does not produce a false "layout changed" balloon.
- **A display change under an open overlay is its own outcome.** A monitor change while the overlay
  is open used to close it as an ordinary cancellation, indistinguishable from Escape. It is now
  logged at warn level with its reason, emits the JSON event `selection_display_changed` in place of
  `selection_cancelled` under the same `schema_version`, and raises a balloon whose wording separates
  a layout change from a DPI or work-area settings change.
- **Topology validation no longer depends on a tray window.** The display-configuration generation is
  only advanced by a window receiving `WM_DISPLAYCHANGE`, and the daemon's tray icon is created after
  the capture engine; a change landing in that gap, or with no tray at all, moved no counter. Both
  DXGI backends now also record the desktop arrangement — monitor rectangles and the primary flag,
  sampled without a window — when they enumerate, and compare it once per hotkey press on the
  validation path. An unreadable arrangement abstains, so a locked or disconnected session cannot
  cause a rebuild storm.
- **A display identified without its EDID is named in the log.** When Windows re-enumerates a panel
  without reading its EDID, which a DisplayPort link retrain right after docking can cause, the id
  falls back to a device-path hash and remembered state under the panel's real id is not found. That
  fallback was logged at debug level; it now warns once per change, saying what it costs and when it
  ends.

## What was not observed

The Win32 paths these changes add — sampling the arrangement with `EnumDisplayMonitors`, delivery of
`WM_DISPLAYCHANGE` to an open overlay, a live DXGI rebuild binding to a different display, and a
live no-EDID enumeration — are covered by unit tests on their pure seams. No dock or undock event has
been watched against this release on real hardware. If the first capture after a dock change is
still lost, the daemon log now says which of these paths it took.

## Install

Install steps, download verification, and the note on unsigned packages are unchanged from
[v0.1.0](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0); substitute `0.1.1` for
the version in the artifact names. Upgrading a v0.1.0 install resets remembered per-monitor state
once if that install predates the EDID-based display ids, because the ids change scheme; nothing else
in `~/.captastic` is touched.

The Chocolatey package is attached but not yet published to the community repository.
