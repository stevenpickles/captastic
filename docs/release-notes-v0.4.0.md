# Draft — GitHub release body for v0.4.0

> **This file is a draft of the release body for the `v0.4.0` GitHub release.** It is not the
> release itself. Paste the content below the horizontal rule into the release description when the
> tag is published, confirming that the attached artifact names match. This file lives in the
> repository so the wording can be reviewed before it is published.

---

Captastic v0.4.0 records the screen. **Record** turns a selection into an animated GIF or an MP4,
and the recording opens in a review window where it can be trimmed and cropped before it is saved.
The editor gains **Arrow** and **Text**. The release carries PRs #113 through #131 on top of v0.3.0.
Everything v0.1.0 said about what Captastic is and is not still holds; see the
[v0.1.0 release](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0) for the full
description and its known limitations.

**Performance:** as before, this release publishes no absolute performance numbers. Recording was
built not to touch the still-capture path: a capture confirmed with **Capture**, **Pin** or **Edit**
does what it did in v0.3.0. A recording's memory is bounded by a frame queue of at most 128 MiB,
however long it runs, and a recording in review holds about the size of its file.

## If you are upgrading from 0.3

- **Nothing to migrate.** Recording settings live in a new `[recording]` table, and the defaults
  apply where it is absent.
- **Chocolatey brings Captastic back after the upgrade.** The upgrade stops a running Captastic and
  starts it again as the signed-in user. 0.3.0's package hook does this, so an upgrade from 0.3
  restarts Captastic, unlike the upgrade from 0.2.

## What changed

- **Record a region, a window or a display.** **Record** sits beside **Edit** on the overlay
  toolbar. While a recording runs, a red border surrounds the recorded area, and a bar beside it
  shows the elapsed time and a **Stop** button. **The border and the bar never appear in the
  recording**, and the bar never takes focus from the window you are working in. A recording ends at
  **Stop** or when it reaches `recording.max_seconds` (60 by default, at most 600), and either way it
  is kept.
- **What a recording contains:** the pixels of the recorded area, and the pointer if
  `[capture] cursor` includes it for stills. **Never audio.**
- **Two formats.** Choose between them with **Options -> Record As**, which is remembered as
  `recording.format`:
  - **GIF** (the default): 15 frames a second, scaled so its longer side is at most
    `recording.gif_max_dimension` (1280 by default). It stores only what changes between frames, so
    it stays small, and it embeds directly in chats, issues and documents.
  - **MP4**: H.264 at 30 frames a second at the recorded size, encoded through Windows Media
    Foundation (on the GPU where the machine has an encoder). It is far smaller and smoother for
    anything long or large.
- **Recording a window.** Under the Window tool a click captures a window at once, so press
  **Record** first. It stays held down, and the next window you click is recorded; press it again
  to go back to capturing. The window you click is brought to the front as the recording starts.
- **Review before saving.** When a recording ends it opens in a **review window** (on by default;
  turn it off with `recording.review = false` or the notification area's **Review Recordings**).
  - **Playback and trim:** it plays the recording, and handles on a timeline trim either end. For
    finer control, **Left** and **Right** step one frame, with **Shift** one second and with
    **Ctrl** to the previous or next change on screen. **I** and **O** set the start and end at the
    playhead, and times read to the millisecond.
  - **Crop:** drag on the picture to crop it, then move or resize the crop as in the image editor.
    **Reset Crop** (R) restores the whole picture. An MP4 crop is kept to even sides.
  - **Save** (Enter) saves it. Saved unchanged, the recording is written exactly as recorded;
    trimmed or cropped, the part kept is encoded again.
  - **Discard**, Escape and the close button all ask first.
- **Nothing reaches the disk before Save.** A recording in review is held in memory, so a discarded
  recording leaves no file behind. Exiting Captastic during a review saves the recording unchanged.
- **Where recordings go.** A recording is always saved to the output directory, named by the same
  template as stills and remembered in capture history, whether or not still captures are being
  saved. **It never goes to the clipboard.**
- **Recording notifications.** A notification names the saved file, and clicking it opens Explorer
  with the file selected.
- **One recording at a time.** While a recording runs, the capture hotkey and still captures are
  refused, and a notification says why.
- **The editor can point and label.**
  - **Arrow** (A) draws a straight arrow to where the drag ends. With **Arrow** in hand, every arrow
    shows a handle at each end, and dragging one moves that end.
  - **Text** (T) types where you click: **Enter** finishes, **Shift+Enter** starts a new line, and
    **Escape** throws it away. Clicking existing text with **Text** in hand edits it in place, and
    emptying it removes it. Text is rendered in Segoe UI at the capture's scaling, so the saved
    pixels are the ones shown while typing.
  - **Smaller** and **Larger** (Ctrl+Minus, Ctrl+Plus) step the size of arrows and text, and resize
    the arrow just drawn.
- **One colour for every tool.** The colour is now shared by every drawing tool rather than kept per
  tool. It and the arrow and text sizes carry over to the next editor until Captastic exits.
- **Sharper editor icons.** The pen, marker, highlighter and swatch icons are antialiased.
- **Notification-area changes.**
  - **Open Captures Folder** opens the output directory, creating it if needed.
  - **Show in Folder** now selects the file.
  - **Show in Folder** and **Open Last Capture** no longer stay greyed after a recording when
    **Save Captures to Disk** is off.
- **JSON events, if you parse `captastic-dev` output:** `recording_started`, `recording_complete`,
  `recording_failed` and `recording_discarded`, and `captastic-dev record` records a display or
  region from the command line. `captastic.exe` writes no structured output.

## What was observed

Walked through by hand on the maintainer's desktop against development builds of this release:

- on 2026-10-15:
  - recording regions and displays to GIF and MP4 on a real screen, with the border and bar absent
    from the result;
  - the arrow and text tools, re-editing text, moving arrow ends, sizes and the shared colour;
  - revealing a saved recording from its notification, and the new tray entry;
- on 2026-10-21, on build `0.4.0-dev.582`:
  - recording a window, including one partly covered by another;
  - the review window's trim (handles, I/O, both formats), crop, Save unchanged and edited, and
    Discard;
  - nothing on disk while a review is open;
  - exiting with a review open;
  - **Review Recordings** surviving a restart.

Those walk-throughs found two defects and one change of design, all in before the tag:

- **A window could not be recorded at all.** A click under the Window tool confirmed at once, so
  **Record** never had a window to act on (PR #124).
- **Recording a window that did not have focus recorded whatever was over it.** Closing the overlay
  gave focus back to the previously active window, which then covered the chosen one (PR #130).
- **Recordings in review were first kept in a hidden drafts folder** in the output directory and
  recovered from there after a crash. At the maintainer's request they are now held in memory and
  never written before **Save** (PR #131, ADR 0012 amended).

**The disposable-VM checklist passed** on 2026-10-21. Its steps are in
[docs/chocolatey.md](https://github.com/stevenpickles/captastic/blob/v0.4.0/docs/chocolatey.md):
- on a clean machine, the portable `install.ps1` and `uninstall.ps1`, a capture, **Pin** and
  **Edit**, and start at login;
- then the Chocolatey install, an upgrade of a running Captastic that came back by itself without
  elevation, and the uninstall.

## What was not observed

- Recording on more than one display, on a display moved or rescaled during a recording, or at a
  scaling other than the maintainer's.
- A recording near the 600-second limit, or a large GIF held in review for a long time.
- MP4 encoding on a machine without a hardware H.264 encoder (Media Foundation falls back to
  software; hosted CI skips the encoder tests because Windows Server images may lack it).
- Recording on Windows on ARM: the ARM64 build is compiled in CI but has not been run.

## Known limitations

- **A window is recorded as its area on the screen.** The window is brought to the front when the
  recording starts, but anything moved over it during the recording, and always-on-top windows, are
  recorded too. Moving the window during a recording does not move the recorded area.
- **A crash or power cut during a review loses that recording**, since it has not been saved yet.
- **Recording an HDR (half-float) display is refused.**
- **No audio, ever.**
- **No hotkey starts or stops a recording.** **Record** is on the overlay, and **Stop** is on the
  recording's bar.
- **A trimmed or cropped recording is compressed twice.** The loss is negligible for GIF and slight
  for MP4.
- **The editor's colour and sizes last until Captastic exits** and are not saved in the
  configuration.
- **A mark still cannot be selected, moved or recoloured once drawn**, except that text can be
  re-edited and an arrow's ends moved; undo it and draw it again.
- The v0.3.0 editor limitations otherwise stand: only eight-bit captures can be marked up, a capture
  taken with **Capture** or **Pin** cannot be opened in the editor afterwards, and the editor needs a
  window at least 760 × 320 DIP.

The v0.1.0 limitations are unchanged: multi-adapter virtual desktops are not composed, HDR sources
are captured as SDR, everything is scoped to one user session, and the packages are not
Authenticode-signed.

## Install

Install steps, download verification, and the note on unsigned packages are unchanged from
[v0.1.0](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0); substitute `0.4.0` for the
version in the artifact names. Upgrading keeps everything in `~/.captastic`.

Captastic is in the Chocolatey community repository from 0.3.0. Once 0.4.0 has passed moderation
there, `choco upgrade captastic` picks it up. Until then, upgrade from the attached package:

```powershell
choco upgrade captastic --version 0.4.0 --source <folder containing the .nupkg>
```
