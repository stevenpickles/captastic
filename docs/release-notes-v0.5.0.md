# Draft — GitHub release body for v0.5.0

> **This file is a draft of the release body for the `v0.5.0` GitHub release.** It is not the
> release itself. Paste the content below the horizontal rule into the release description when the
> tag is published, confirming that the attached artifact names match. This file lives in the
> repository so the wording can be reviewed before it is published.
>
> **Before the tag:** the items marked _(open)_ under "What was observed" are gates in
> `ROADMAP.md` that had not been met when this draft was written. Each is either observed and moved
> into the list, or moved to "What was not observed", before publishing.

---

Captastic v0.5.0 records a window as itself, records the computer's sound into MP4 recordings, and
ships a command line, `captastic-cli`, that lets a script or an agent take a capture and read back
where it went. The release carries PRs #134 through #155 on top of v0.4.0. Everything v0.1.0 said
about what Captastic is and is not still holds; see the
[v0.1.0 release](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0) for the full
description and its known limitations.

**Performance:** as before, this release publishes no absolute performance numbers. None of the
three features touches the still-capture path: a capture confirmed with **Capture**, **Pin** or
**Edit** does what it did in v0.4.0. Sound adds about 20 KB a second to an MP4.

## If you are upgrading from 0.4

- **Nothing to migrate.** `[recording] audio` is new and off by default.
- **Capture history gains a field.** Captures taken by `captastic-cli` are marked
  `origin = "command-line"` in `~/.captastic/history.toml`. Older builds refuse a history file with
  that field, so going back to 0.4 after using the command line means deleting `history.toml`.
- **Chocolatey brings Captastic back after the upgrade**, as from 0.4.0.

## What changed

### Recording a window as itself

- **A window is recorded through Windows Graphics Capture** (ADR 0013), not as its area of the
  screen. A window moved over it, or moving it, changes nothing in the recording, and the red border
  and its bar follow it around.
- **What a window recording contains:** the window as Windows draws it, at the size it had when the
  recording started. A window resized during the recording is scaled to fit, its proportions kept,
  on black where it no longer fills the picture. A minimised window holds its last frame until it is
  restored. Closing the window ends the recording, as **Stop** would.
- **When Windows will not let a window be captured** (some protected and system windows), it is
  recorded as its area on the screen, as in 0.4.0, and the saved notification says so.
- The window is still brought to the front when the recording starts.

### Sound in MP4 recordings

- **Options -> Record Sound** adds the computer's sound to an MP4 recording (ADR 0015). It is
  **off by default** and remembered as `recording.audio`.
- **What is recorded is the whole system mix of the default output device**, as you hear it, after
  Windows' volume and mute: every application's sound, and any chime, notification or call the
  computer plays meanwhile. **Never the microphone.**
- **MP4 only.** GIF has no sound; with **Record As** set to GIF the row is greyed and reads
  **Record Sound (MP4 only)**.
- **How to tell it is on:** the recording's bar shows a speaker beside the elapsed time.
- **Changing output device during a recording** (plugging in headphones, say) carries on with the
  new device. With no output device at all, the recording is silent and the saved notification
  begins "Recorded without sound" with the reason.
- **The review plays the sound** in step with **Play**, is silent while the playhead is stepped or
  dragged, and has a **Mute** button (M). A trimmed or cropped save keeps the sound of the span
  kept, cut at the same moments as the picture.
- Sound is AAC at 160 kbit/s, 48 kHz stereo.

### `captastic-cli`, captures from the command line

- **A second executable, `captastic-cli.exe`, ships beside `captastic.exe`** (ADR 0014). Chocolatey
  puts it on `PATH`; the portable install puts it in the install folder.
- **Every answer is one line of JSON on standard output**, with exit codes 0 (captured), 1
  (failed), 2 (refused) and 64 (the command line was wrong).
- **Exactly what it can capture without a hand on the overlay:**
  - a whole display (`--display primary`, `pointer`, `virtual` or an id from `displays`);
  - a rectangle in desktop pixels (`--rect x,y,width,height`);
  - a window, by handle from `windows`, by a title it contains, or by its process name
    (`--window`, `--window-title`, `--process`). More than one match is refused with the matches
    listed, never guessed;
  - the region last chosen on the overlay (`--last-region`).
- **With a hand on the overlay:** `--interactive` opens the overlay for the user to choose, and
  waits up to two minutes. **Capture**, **Pin** and **Edit** (at **Done**) answer with what was
  chosen; **Record** and Esc are refused.
- **It works whether or not Captastic is running.** With Captastic running it asks Captastic over a
  named pipe scoped to the user and session; without it, it captures by itself with the same
  settings and output folder. Only `--interactive` needs Captastic running.
- **Every capture it takes is announced** by a notification, *Captured by a command-line request*,
  that names the file and shows it when clicked. **This cannot be turned off.** It is also marked in
  capture history.
- **The clipboard is left alone** unless `--clipboard` is given.
- [docs/command-line.md](https://github.com/stevenpickles/captastic/blob/v0.5.0/docs/command-line.md)
  documents every command, its JSON and its exit codes, written to be pasted into an agent's
  instructions.

### Fixes

- **Scrubbing an MP4 in review is up to ten times faster**, most of all backwards (PR #150).
- **A window that Windows scales for display** (one that is not per-monitor DPI aware, on a display
  at 125% or more) was captured at its unscaled size in the top-left corner of a larger black image.
  It is now captured whole, on the overlay and from the command line (PR #146).
- **From an adversarial review of the whole code base** before the tag (PRs #156 to #162):
  - **Recordings are not lost.** A recording running when Captastic exits gets up to 15 seconds
    to finish writing; a reviewed recording that cannot be saved in the output folder is saved in
    `~/.captastic/recordings` instead; an MP4 with sound no longer grows without limit in memory
    when the encoder falls behind; and a crop dragged to the picture's last row no longer loses
    the recording on **Save**.
  - **Every command-line capture is announced**, including a second one to the same file name.
  - **`fresh` capture mode** no longer returns the screen as it was at the previous capture when
    something was drawn just before the hotkey.
  - **The window chooser** no longer re-renders a window it cannot capture for as long as it is
    open.
  - **Delete-recording and discard-edits confirmations** default to the safe button.
  - **Chocolatey install** no longer runs the old per-user copy's uninstaller with administrator
    rights; the portable uninstaller refuses a folder that holds no Captastic.

## What was observed

Walked through on the maintainer's desktop against development builds of this release:

- on 2026-10-21 and 2026-10-25, on builds `0.5.0-dev.604` and `0.5.0-dev.605`: recording a window
  through WGC, the indicator following it, and the fallback notice;
- on 2026-11-22, by hand, on installed builds `0.5.0-dev.608` to `0.5.0-dev.617`: every command-line
  request against a running Captastic and without one, the announcement and its reveal, the
  clipboard left alone, window captures of partly covered windows, ambiguous and missing matches,
  and a capture refused during a recording;
- on 2026-11-22, by `scripts/check-command-line.ps1` (30 checks: test windows of its own, pixels of
  every capture checked, the overlay worked by mouse and keyboard for `--interactive`, the
  clipboard saved and restored around its leg) and `scripts/check-recording-audio.ps1` (a tone
  played during loopback capture and during a 3-second MP4 recording, and found in the file at its
  level); the review's sound and **Mute** heard back through loopback.

Those found three defects, all fixed before the tag: a window scaled for display captured at the
wrong size (PR #146), the one-shot command line waiting for its announcement to close (PR #145),
and a status answer whose daemon version overwrote the protocol version (PR #143).

- _(open)_ An agent's capture on the **installed** build, with and without Captastic running.
- _(open)_ A recording with sound while a video plays, with silence, and with headphones plugged in
  part-way through.
- _(open)_ **Record Sound** and the bar's speaker, seen on a real overlay.
- _(open)_ The disposable-VM checklist in
  [docs/chocolatey.md](https://github.com/stevenpickles/captastic/blob/v0.5.0/docs/chocolatey.md),
  now with `captastic-cli` and sound steps.

## What was not observed

- Recording a window spanning two displays, or on a display moved or rescaled during a recording.
- A GPU-rendered window (a browser or game) recorded for long.
- Sound from a device at 44.1 kHz or with more than two channels; the conversion is unit-tested.
- `captastic-cli` on a machine with more than one display.
- Windows on ARM: the ARM64 build is compiled in CI but has not been run.

## Known limitations

- **A window recording brings the window to the front** when it starts.
- **Sound is the whole system mix.** Recording one application's sound only is not offered yet.
- **Dragging the review's playhead backwards still pauses briefly once per second of video**,
  longer on very large recordings.
- **`captastic-cli` does not record**, and answers one request at a time: while `--interactive`
  waits for the user, other requests wait behind it.
- **A protected window cannot be captured from the command line**, as on the overlay; it is not
  listed by `windows`.
- **Left from the adversarial review, each with an unusual trigger:**
  - in `latest` mode the pointer can be drawn where it was at the last repaint, and twice on a
    virtual desktop;
  - opening a review of a long GIF, or jumping to its end, decodes on the window thread, which
    can hold up a running recording's **Stop** for a few seconds;
  - a GIF of fast-changing content can show ghosting in its darkest areas;
  - a pen stroke or crop drag is dropped if another window takes the mouse mid-drag;
  - a worker thread that panics is not restarted, so the hotkey can stop working silently until
    Captastic is restarted;
  - a file name template that is not unique stops saving after 100 files of the same name.
- The v0.4.0 limitations otherwise stand: a crash during a review loses that recording, an HDR
  display cannot be recorded, no hotkey starts or stops a recording, and a trimmed or cropped
  recording is compressed twice.

The v0.1.0 limitations are unchanged: multi-adapter virtual desktops are not composed, HDR sources
are captured as SDR, everything is scoped to one user session, and the packages are not
Authenticode-signed.

## Install

Install steps, download verification, and the note on unsigned packages are unchanged from
[v0.1.0](https://github.com/stevenpickles/captastic/releases/tag/v0.1.0); substitute `0.5.0` for the
version in the artifact names. Upgrading keeps everything in `~/.captastic`.

Once 0.5.0 has passed Chocolatey moderation, `choco upgrade captastic` picks it up. Until then,
upgrade from the attached package:

```powershell
choco upgrade captastic --version 0.5.0 --source <folder containing the .nupkg>
```
