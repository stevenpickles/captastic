# Draft — GitHub release body for v0.2.0

> **This file is a draft of the release body for the `v0.2.0` GitHub release.** It is not the
> release itself. Paste the content below the horizontal rule into the release description when the
> tag is published, confirming that the attached artifact names match and resolving the performance
> placeholder below. This file lives in the repository so the wording can be reviewed before it is
> published.

---

Captastic v0.2.0 is about placing a region on the pixel you mean, and about giving captures a real
home on disk. It carries PRs #87 through #92 on top of v0.1.1: region edges that snap to the windows
under them, arrow keys that move an edge a pixel at a time, a magnifier that shows the pixel the
pointer is covering, a view you can freeze and unfreeze with one key, file output in PNG, JPEG, or
BMP that the notification area can switch on without a restart, and the benchmark tooling a future
performance claim will have to rest on. Everything v0.1.0 said about what Captastic is and is not
still holds; see the
[v0.1.0 release](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0) for the full
description and its known limitations.

<!-- OPERATOR: run the procedure in benchmarks/README.md and replace the paragraph below with the
     measured figures and the fingerprint they came with, or delete the paragraph. Do not quote a
     millisecond without committed artifacts behind it. -->

**Performance:** this release publishes no absolute performance numbers. The tooling that would
support one — a host fingerprint in every report, comparability keyed on it, raw artifacts per
repeat run, and a comparison that refuses two runs that did not measure the same thing — ships here,
but the runs it exists to produce are an operator step that has not been taken. A figure would need
three compatible repeat sets measured from a console session on the documented host, per
[benchmarks/README.md](https://github.com/stevenpickles/captastic/blob/v0.2.0/benchmarks/README.md),
quoted with the fingerprint and the spread that came with them.

## What changed

- **Region edges snap to the windows under them.** While a region is drawn, moved, or resized, its
  edges snap to the visible frames of the windows on that display, to the work area, and to the
  display itself once they come within 8 DIPs — the same physical distance at every scaling. A
  snapped edge lands exactly on the target: a region snapped to a window's right edge ends on that
  window's last pixel column, never one short, and a hairline accent guide shows what it caught. The
  nearest edge wins and ties go to the topmost window. Hold **Ctrl** while dragging to place an edge
  exactly under the pointer instead; **Options → Snap to Edges** turns snapping off for good and is
  remembered.
- **The arrow keys nudge a region one pixel at a time**, ten with **Shift**, and resize its right
  and bottom edges instead of moving it with **Ctrl** held. Nudging never snaps, so it is how an
  edge reaches a pixel no window sits on.
- **A magnifier shows the pixel under the pointer.** Hold **Z** to bring it up; in its default mode
  it also appears by itself when the pointer slows to a deliberate pace during a region drag, and
  goes again as soon as the pointer travels. It shows the 31 physical pixels around the pointer
  enlarged six times (more at higher scaling), with a grid, the pointer's pixel outlined, the
  selection's edges on the exact boundaries they occupy, and the desktop coordinate underneath. It
  sits beside the pointer and never covers the pixels it is magnifying. **Options → Zoom** cycles
  Auto, Hold Z, and Off, and the choice is remembered.
- **Every hotkey press now captures the screen at the press, and you choose which picture to look
  at.** The overlay opens on the live desktop with those pixels held behind it. Press **F**, or use
  **Options → View**, to switch: the live view is the desktop as it is, changing; the frozen view is
  the frame from the press, and while it is showing a **FROZEN · pixels from hotkey press** tag sits
  above the toolbar. Confirming captures whichever view is on screen — the frozen view publishes the
  frame from the press, the live view captures again at the moment you confirm. The switch costs
  nothing and is not remembered: `selection.preview` now chooses only the view the overlay opens in.
  `auto` and `live` open live; `frozen` opens on the frame from the press. They differ only in what
  happens when the layered presenter cannot be established — `auto` reopens the same run in an
  opaque window showing that frame, with the View row greyed, while `live` fails rather than
  silently handing you trigger-time pixels. The Window tool is unaffected and its row is greyed
  there: clicking a preview has always rendered that window fresh.
- **File output writes PNG, JPEG, or BMP.** `output.format` is a real choice now. `png` stays the
  default and keeps the straight alpha of a window capture; `jpeg` is lossy and much smaller, takes
  a `jpeg_quality` between 1 and 100 (90 by default), and is written with a `.jpg` extension; `bmp`
  is uncompressed and keeps alpha. All three refuse a half-float or scRGB frame by name rather than
  narrowing it silently. **JPEG cannot carry an alpha channel**, so the transparent corners and
  shadow of a window capture are composited over opaque white, and the configuration comment, the
  README, and a debug log line each say so. JPEG encoding uses the `jpeg-encoder` crate; this
  software is based in part on the work of the Independent JPEG Group. With no `output.directory`
  set, captures go to `Captastic` inside the Pictures folder Windows shows you — Captastic asks the
  shell for it rather than assuming `%USERPROFILE%\Pictures`, so a Pictures folder moved by
  OneDrive's Known Folder Move, a roaming profile, or a group policy is where captures land, and the
  path settled on is logged at info when saving starts.
- **The notification area can turn file output on and off.** **Save Captures to Disk** sits directly
  above the history entries, is checked while captures are being written, and takes effect on the
  next capture: turning it on starts the file worker — creating the output directory and rejecting a
  bad filename template there and then — and turning it off stops it, leaving the clipboard
  untouched either way. The choice is written back to `output.enabled` in the configuration the
  daemon is running on, preserving the rest of the document and its comments, so it survives a
  restart. A start that fails leaves the item unchecked and says why. Everything else under
  `[output]` — format, directory, template — is still read at startup, so changing those still means
  a restart.
- **`captastic config validate` now rejects a filename template the daemon would refuse**, whether
  or not `output.enabled` is set, because enabling it is a menu click away. A one-shot `captastic
  capture` also accepts `--config <path>` at last, loaded strictly: a file that cannot be read is an
  error rather than a silent fall back to defaults.
- **Benchmark reports carry the host they describe.** Every report now includes an environment
  fingerprint — OS build, CPU, adapters with their driver versions, displays with scale and refresh,
  session, power source, power plan, and the whole build identity including its dirty flag — and two
  runs are comparable only if all of it matches, so two development builds a hundred commits apart
  no longer compare as the same software. `captastic benchmark --repeat N --output-dir <dir>` writes
  a full report per run, the per-capture event stream per run with `--raw-events`, and a
  `repeated.json` carrying every stage's spread at p50, p95, and p99;
  `captastic benchmark compare <baseline> <candidate>` reads either shape back and prints the
  per-stage deltas with a `within_noise`/`slower`/`faster`/`unmeasurable` verdict, or refuses with
  exit status 2 naming every host fact that differs. `doctor --json` gained the same fingerprint
  block. The report's `schema_version` is now 3 and `doctor --json`'s is 2. This matters to people
  who benchmark Captastic; it changes nothing about taking a screenshot.
- **`state.toml` gained two keys.** `snap_to_windows` and `region_zoom` are global preferences,
  written only once they have been changed, with the state schema version deliberately left alone.
  The caveat that comes with that choice: an older Captastic refuses a state file containing keys it
  does not know, which every read path treats as a warning and a fall back to defaults — so
  downgrading after changing either preference costs that run its remembered toolbar positions,
  tools, and regions, and nothing else. The view toggle is not persisted and adds no key.
- **The overlay is excluded from screen capture in both views.** `WDA_EXCLUDEFROMCAPTURE` used to be
  set only on the live presenter, so an overlay configured with `selection.preview = "frozen"` was
  never excluded and could appear in another application's screen recording. One layered window now
  serves both views, and the exclusion is applied where the window is created — including the opaque
  fallback window.
- **The capture engine's readback pool is sized for the frames a selection pins.** An open overlay
  holds the frame from its hotkey press for the whole interaction, which the fixed three slots could
  not cover: a full-display capture plus two open or queued selections left nothing, and the next
  press could die in the log alone. The pool is now `capture.buffer_slots` plus
  `selection.queue_capacity` plus one, so the default ceiling rises from three slots to five — at 4K
  a slot is 31.6 MiB, so from about 95 MiB to about 158 MiB — and slots are allocated on first use,
  so the larger ceiling costs nothing until that many frames really are live. `capture.buffer_slots`
  keeps its fixed value of 3 and is now documented as the base rather than the whole pool. Running
  the pool out on a press or a confirmation raises a notification-area balloon and emits
  `selection_failed` rather than a log line nobody sees.
- **The Options menu is taller**: 248 × 252 DIP at 100 % scaling, with rows for background dimming,
  edge snapping, zoom, and the view, above the clipboard destination and Cancel.
- **JSON changes worth knowing about if you parse it.** A selection reports `preview_mode` as the
  view that was showing at the confirmation (`live` or `frozen`) rather than a restatement of the
  anchor, `capture_anchor` as `trigger` or `confirmation` — the `fallback` value is gone along with
  the cross-thread fallback it named — and a new `view_switched`. A written capture reports its
  `format`.

## What was not observed

The overlay work in this release is the part a person sees, and none of it has been watched working.
No pointer was driven and no screen was read while it was built, and a second daemon cannot start
while the daily one holds the session control event. The following are covered by unit and property
tests on their pure seams, and by overlays that were opened, presented, and closed by an external
close request — and by nothing else:

- snapping to a real window's edge at any scaling, the accent guide appearing along it, and **Ctrl**
  releasing it mid-drag;
- the arrow keys moving or resizing a region;
- the magnifier appearing on **Z** or by itself on a slow drag, its placement near a display edge,
  and its sample in the frozen view;
- **the live-view magnifier sample coming back free of the overlay.** The sample is a plain desktop
  blit and the overlay is marked `WDA_EXCLUDEFROMCAPTURE`, so in principle it is bare desktop; the
  selection's edges are drawn synthetically, so correctness does not depend on that holding, but the
  appearance does;
- the two new Options rows and the **View** row rendering, greyed or otherwise, and the menu's new
  height on screen;
- a video stopping and resuming across the **F** toggle. Pressing **F** itself was watched on
  2026-09-17 against build `0.2.0-dev.479`, and it is what found the one defect this release's
  overlay work has had on a screen: the frozen view came up with see-through chrome and skewed
  colours, because the compositor did not treat the layered window as opaque; the frozen view now
  forces every pixel opaque before it is presented, and after that change the frozen view, its dim,
  outline, handles, badge, toolbar, and the **FROZEN · pixels from hotkey press** tag all rendered
  as intended;
- a confirmation of any kind through a running daemon, and therefore the new `preview_mode`,
  `capture_anchor`, and `view_switched` JSON from a real capture;
- either new preference surviving a daemon restart end to end;
- the presenter fallback firing, and therefore the opaque fallback window; and the
  `BufferExhausted` balloon, which the pool change is meant to make unnecessary.

**Nobody has clicked Save Captures to Disk.** The toggle, its persistence, and its failure paths are
proved by tests against the same code paths, but the menu item has not been used on a live tray, no
capture has landed on disk because of it, and neither of its rarer paths — a file worker held past
its stop deadline, and captures abandoned because they were queued faster than they could be encoded
— has been seen outside a test.

**The default output directory was wrong, and only running it found that.** On 2026-09-17 Steven
turned file output on and went looking for the captures. There was no `Pictures\Captastic` folder —
OneDrive's Known Folder Move had moved his Pictures folder to `C:\Users\Steven\OneDrive\Pictures`,
and Captastic, which built its default from `%USERPROFILE%`, had been writing into the empty
`C:\Users\Steven\Pictures` left behind. Every test passed throughout: the directory was created and
the file was written, exactly as asked, somewhere Explorer no longer calls Pictures. Captastic now
asks the shell for the Pictures folder and logs the directory it settled on when saving starts. The
fix was verified on that machine — a one-shot capture landed in
`C:\Users\Steven\OneDrive\Pictures\Captastic` — and the test file was removed afterwards. Redirected
known folders are now covered by tests; no other known folder has been looked at.

**A window capture's alpha has not been through the two new encoders.** Both were checked once on a
real capture: on 2026-09-17 a 3840 × 2160 full-display DXGI capture was written as JPEG at quality
85 and as BMP. The JPEG was opened and viewed — right side up, with the page's purple and the
browser chrome the colours they were on screen — and the BMP decoded through GDI+ as
`3840x2160 Format24bppRgb`, with a sampled pixel at (1200, 400) white in both files, within JPEG's
compression error. PNG output is unchanged from Milestone 4. What no run has produced is a
straight-alpha window capture through either new encoder, so JPEG's compositing over opaque white
and BMP's 32-bpp alpha header are exercised by unit tests alone.

**The benchmark claim run has not been made.** No `--backend dxgi` benchmark was taken for this
release, the absolute budget ceilings remain empty, and no baseline is committed. The fingerprint's
battery and Remote Desktop tokens are asserted in tests rather than observed.

**And still outstanding from v0.1.1:** no dock or undock event has been watched against a release.
If a first capture after a dock change is lost, the daemon log now says which path it took.

## Known limitations

These are behaviours that are understood and deliberately left as they are in this release.

- **`Alt` chords in the overlay do nothing.** Alt+arrow and Alt+Z arrive as `WM_SYSKEYDOWN`, which
  the overlay does not handle, so those combinations do nothing rather than doing the unmodified
  thing.
- **The toolbar can paint over the magnifier.** The magnifier keeps clear of its own source square
  and the dimension badge moves out of the magnifier's way, but a slow drag near the bottom of the
  display can put the magnifier under the toolbar, which is drawn last.
- **The window list is snapshotted at the first Region press.** A window moved, raised, or closed
  while the overlay is open is still snapped to where it was when the overlay opened, and the Window
  chooser shares that snapshot. Re-enumerating on every pointer move would cost far more than the
  staleness; close and reopen the overlay to refresh it.
- **Pressing or releasing Ctrl between the last pointer move and the button-up commits a rectangle
  other than the painted one.** The release recomputes the geometry with the modifiers held at that
  instant, so a Ctrl change in that gap commits a rectangle you never saw. Rare, and the alternative
  has its own failure mode.
- **A display change during the presenter fallback is lost.** When the layered presenter fails to
  establish itself, the overlay is destroyed and re-created opaque; a `WM_DISPLAYCHANGE` arriving in
  that instant has no window to arrive at, so the replacement is built from the geometry computed
  before it. The overlay still closes on the next display change it does see.
- **Captures queued when file output is switched off are not written.** That is inherent to stopping
  a destination; each one is now named in the log with its id and counted as abandoned rather than
  disappearing.
- **JPEG flattens alpha over white.** Choosing `format = "jpeg"` means a window capture's
  transparent corners and shadow are composited over opaque white, because the format has nowhere to
  put an alpha channel. Use `png` or `bmp` to keep them.

The v0.1.0 limitations are unchanged: multi-adapter virtual desktops are not composed, HDR sources
are captured as SDR, everything is scoped to one user session, and the packages are not
Authenticode-signed.

## Install

Install steps, download verification, and the note on unsigned packages are unchanged from
[v0.1.0](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0); substitute `0.2.0` for the
version in the artifact names. Upgrading keeps everything in `~/.captastic`.

One thing to know before downgrading: `state.toml` gains `snap_to_windows` and `region_zoom` the
first time either preference is changed, and a build older than v0.2.0 refuses a state file
containing keys it does not know. That refusal is a logged warning and a fall back to defaults
rather than a failure to start — the cost is the remembered toolbar position, tool, and region for
that run.

The Chocolatey package is attached but not yet published to the community repository.
