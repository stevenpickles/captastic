# Draft — GitHub release body for v0.3.0

> **This file is a draft of the release body for the `v0.3.0` GitHub release.** It is not the
> release itself. Paste the content below the horizontal rule into the release description when the
> tag is published, confirming that the attached artifact names match. This file lives in the
> repository so the wording can be reviewed before it is published.

---

Captastic v0.3.0 adds the first tools that act on a capture after it is taken: **Pin** keeps one on
screen, and **Edit** crops, redacts and marks one up before anything is copied. It also ships as a
single windowless `captastic.exe` that starts at login, and is the first release offered through the
Chocolatey community repository. It carries PRs #99 through #107 on top of v0.2.0. Everything v0.1.0
said about what Captastic is and is not still holds; see the
[v0.1.0 release](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0) for the full
description and its known limitations.

**Performance:** this release publishes no absolute performance numbers, as before. Pinning and
editing were built not to touch the capture path: a capture confirmed with **Capture** does exactly
what it did in v0.2.0, and the thread that hosts pins and editors does not exist until the first Pin
or Edit.

## If you are upgrading from 0.2

- **There is one program now.** `captastic.exe` is the notification-area application, with no
  console, wherever it is started from: the Start Menu, login, or `captastic` on `PATH`. The
  command-line tool of earlier releases is now `captastic-dev.exe`, a developer tool built from the
  repository and not shipped. `captastic-desktop.exe`, the 0.2 launcher, is gone.
- **Your login entry is moved for you.** The install repoints a **Start with Windows** entry that
  named `captastic-desktop.exe` at `captastic.exe`, and `captastic.exe` repeats that repair when it
  starts. The old shim is removed.
- **`captastic.exe` has no command line beyond the daemon's options.** `--help`, `--version` and an
  argument error are shown in a dialog. A daemon that cannot start — an invalid configuration, a
  hotkey another program holds — now says so in a dialog instead of vanishing at login.
- **Chocolatey upgrades now bring Captastic back.** An upgrade that had to stop a running Captastic
  starts it again afterwards, as the signed-in user and never elevated. This takes effect from the
  next upgrade *after* 0.3.0: the hook that runs during an upgrade is the old package's, and 0.2's
  predates it, so after upgrading from 0.2 start Captastic from the Start Menu once.

## What changed

- **Pin a capture.** **Pin** sits beside **Capture** on the overlay toolbar, with **Shift+Enter**,
  Shift+click on **Capture**, Shift+double-click on a selection, and Shift+click on a window in the
  Window tool. It copies the selection exactly as **Capture** does — to the clipboard, and to disk if
  file output is on — and also keeps it on screen in an always-on-top window at its physical pixel
  size, in the place it was taken, inside a thin blue border. Drag a pin from anywhere; the mouse
  wheel or its right-click menu makes it translucent; **Click Through** lets clicks pass to whatever
  is beneath it; Escape (once it has been clicked) or **Close** closes it. Any number can be open,
  and they stay open while you take further captures. The notification area gains **Close All
  Pins**, and **Make Pins Clickable**, the way back from a click-through pin. Pins appear in display
  and region captures like anything else on screen, but are never offered by the window chooser or
  snapped to.
- **Edit a capture before it is copied.** **Edit** (**Ctrl+Enter**, or Ctrl with any of the clicks
  above) opens the selection in an editor window instead of copying it. Nothing from that capture
  reaches the clipboard, its history, or the disk until **Done** (Enter), and then only the edited
  image does; **Cancel** (Escape) or closing the window discards it, asking first if you had made
  changes. The editor offers:
  - **Crop** (C), with edges, corners and handles to move and resize it;
  - **Redact** (R), opaque black boxes that replace the pixels under them;
  - **Box** (B), a coloured outline with slightly rounded corners;
  - **Pen** (P) and **Marker** (M), thin and thick freehand lines;
  - **Highlighter** (H), a wide stroke that tints what is under it without hiding it;
  - seven colours (1–7: red, yellow, green, blue, magenta, black, white), remembered per tool;
  - **Undo** and **Redo** (Ctrl+Z, and Ctrl+Y or Ctrl+Shift+Z) across every crop and mark.

  Editors open on the display the capture came from, any number at a time, and one still open when
  Captastic exits closes without copying anything. The edited image keeps the original capture's
  identity, so its filename and history entry read as that capture.
- **Redact through Edit, not after Capture.** A secret that has already been copied is not hidden by
  anything an editor does afterwards, so only **Edit** withholds a capture; **Capture** and **Pin**
  publish at once, as they always have. Redaction replaces the covered pixels outright rather than
  blurring or pixelating them, because both of those can often be reversed or guessed, and a mark
  drawn over a redaction sits on top of the black without uncovering anything.
- **The overlay toolbar is wider**, 514 × 56 DIP at 100 % scaling, to hold **Pin** and **Edit**
  beside **Capture**. A run with no notification-area application behind it — the developer CLI's
  one-shot capture — shows both greyed.
- **The Options menu states both destinations.** **Copy to Clipboard** is ticked only when the
  clipboard really is a destination; it used to be ticked unconditionally. **Save to Disk** is new
  and is the same setting as the notification area's **Save Captures to Disk**: switching it starts
  or stops the file worker and writes `output.enabled` back, and the capture you are about to
  confirm goes to the set as you left it. The menu is 248 × 292 DIP at 100 %.
- **JSON changes worth knowing about if you parse it.** `selection_complete` gains `intent`
  (`capture`, `pin` or `edit`), and an edit reports how it ended in a new `edit_finished` event
  (`done`, `cancelled`, `discarded` or `failed`). These come from `captastic-dev`; `captastic.exe`
  writes no structured output.

## What was observed

On 2026-10-15, on one 3840 × 2160 display at 150 % scaling, against development builds of this
release:

- the Chocolatey upgrade from the formal 0.2.0 migrating the login entry and removing the old shim;
  the Start Menu start with no console; start at login; and the `--version`, argument-error and
  "Captastic stopped" dialogs;
- an upgrade of a running 0.3 build stopping it and starting it again, its process token confirmed
  not elevated;
- pinning, including the pin's right-click menu;
- the editor opened with Ctrl+Enter, a crop published through **Done** at its cropped size, and the
  drawing tools in use.

Those walk-throughs found two defects, both fixed before the tag:

- **Ctrl and Shift were ignored on a click of Capture and on a double-click of the selection**, so
  only Enter and a window click could pin or edit (PR #106).
- **Right-clicking a pin did nothing.** A pin is draggable from anywhere because it answers as a
  caption, and Windows swallowed a right-click on a caption before the menu could open (PR #105).

An off-screen render of the editor found a third weakness before anyone met it: a finished mark
appeared in the editor's view only because it had already been drawn while the pointer moved. It is
now redrawn when it is committed, however it got there (PR #107).

## What was not observed

- **The disposable-VM checklist was not run for this release.** Its steps are in
  [docs/chocolatey.md](https://github.com/stevenpickles/captastic/blob/v0.3.0/docs/chocolatey.md): a
  clean machine's portable `install.ps1` and `uninstall.ps1`, and the Chocolatey install, upgrade
  and uninstall. The portable installer has therefore not been watched end to end on 0.3, and the
  Chocolatey paths were watched only on a machine that already had Captastic; hosted CI covers the
  silent install, upgrade and uninstall without a desktop.
- Pins and editors at any scaling other than 150 %, on more than one display, or moved between
  displays of different scaling.
- Drawing responsiveness on a 4K capture with many marks: a stroke redraws only the pixels it
  covers, but no one has timed it.
- The editor's failure paths (an editor that cannot open raises the dropped-capture balloon), which
  are exercised by tests alone.

## Known limitations

- **Escape closes a pin only after it has been clicked.** A new pin does not take keyboard focus, so
  it never steals typing from the window you were working in.
- **Pins close when Captastic exits** and are not restored at the next start.
- **There is no arrow tool and no text tool yet**, and a mark cannot be selected, moved or recoloured
  once drawn; undo it and draw it again.
- **Only eight-bit captures can be marked up.** An HDR (half-float) capture can still be cropped and
  redacted; the drawing tools refuse it.
- **A capture taken with Capture or Pin cannot be opened in the editor afterwards.** Reopening a
  capture from history is later work.
- **The editor needs a window at least 760 × 320 DIP**, to fit its toolbar.

The v0.1.0 limitations are unchanged: multi-adapter virtual desktops are not composed, HDR sources
are captured as SDR, everything is scoped to one user session, and the packages are not
Authenticode-signed.

## Install

Install steps, download verification, and the note on unsigned packages are unchanged from
[v0.1.0](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0); substitute `0.3.0` for the
version in the artifact names. Upgrading keeps everything in `~/.captastic`.

This is the first release submitted to the Chocolatey community repository. Until it has passed
validation and moderation there, install the attached package from a local source:

```powershell
choco install captastic --version 0.3.0 --source <folder containing the .nupkg>
```
