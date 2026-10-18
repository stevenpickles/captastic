# ADR 0012: Reviewing a recording before it is saved

## Status

Accepted for the Windows prototype, 2026-10-17. Extends ADR 0011 within Milestone 9, for 0.4.0.

## Context

ADR 0011 saves a recording as recorded: whatever was on screen from **Record** to **Stop**,
including the reach for **Stop** at the end and anything that was not meant to be in it. The
remedy is a short review between the recording and the file, with the two edits that matter most
for a screen recording: trimming its ends and cropping its area. Four things shape it:

- **A recording is too large to hold.** ADR 0011 streams frames to the encoder so memory is bounded
  by the encoder, not the duration. A review that kept every frame to play back would undo that: a
  one-minute 1280-pixel GIF is over 3 GB of raw pixels.
- **Most recordings are kept as they are.** Saving one unchanged must not cost a second encode, a
  wait, or any loss of quality.
- **Trimming has to be exact.** The point of trimming is to cut at the frame where the interesting
  part starts, not somewhere near it.
- **Nothing may be lost by accident.** A recording is the one capture that cannot be taken again.

## Decision

**A recording is reviewed before it is saved, by default.** When a recording ends, it opens in a
**review window** instead of being saved. `recording.review` (`true` by default) turns this off; the
notification area offers the same switch as **Review Recordings**, written back like **Save
Captures to Disk**. With review off, a recording is saved as ADR 0011 describes.

**The recording is reviewed from a draft, and read back from it.** A recording is encoded exactly
as ADR 0011 describes, but into `.captastic-drafts`, a hidden folder in the output directory, rather
than into the output directory itself. The review window reads the draft back: GIF frames are kept
in their compressed form and decoded on demand, and MP4 frames are decoded by Media Foundation's
source reader, seeking from the nearest key frame. Memory stays near the size of the file however
long the recording is.

**The review window plays the recording and offers two edits, trim and crop.**

- **Trim** keeps a span of the recording, set with a start and an end handle on a timeline under
  the picture. Trimming is to the millisecond, not to the recorded frames: a still screen can be one
  frame lasting several seconds, and its middle must still be a place to cut. For control finer
  than a pointer, the keyboard moves the playhead one frame interval (← and →), one second (with
  Shift) or to the previous or next change on screen (with Ctrl), and **I** and **O** put the start
  or the end at the playhead. The window shows the playhead's time and the trimmed length to the
  millisecond.
- **Crop** keeps a rectangle of the picture, dragged and resized as in the image editor, in the
  recording's pixels. An MP4's crop is kept to even sides, as H.264 requires.

**Save** writes the recording into the output directory, named as before, records it in capture
history, and raises the "Recording saved" notification. Saved unchanged, the draft is moved there
as it is, with no encoding. Saved trimmed or cropped, the kept frames are decoded, cropped and
encoded again by the writers ADR 0011 uses, then the draft is removed. The window shows that the
save is in progress and closes when it is done.

**Discarding is always deliberate.** **Discard**, Escape and the window's close button all ask
first: *"Discard this recording? It will be deleted."* Only **OK** deletes the draft. Captastic
exiting during a review saves the recording unchanged, as exiting during a recording finishes it.
A draft left behind by Captastic ending without that chance, a crash or a power cut, is saved into
the output directory the next time Captastic starts, and a notification says so.

**One review at a time.** A recording finished while another is in review waits for that one to be
saved or discarded, so two review windows never compete for the same name or the same attention.
The capture hotkey and still captures are unaffected by a review.

## Consequences

- The recording no longer appears in the output directory or history until it is saved; the
  "Recording saved" notification comes after the review, not when **Stop** is pressed.
- A trimmed or cropped recording is compressed twice. For GIF the loss is negligible, since its
  colours were already reduced; for MP4 it is slight at the bitrate ADR 0011 chose.
- MP4 files gain a key frame at least every second, so seeking during review decodes at most a
  second of video.
- `recording.review` joins the configuration and **Review Recordings** the notification-area menu.
- Reading recordings back is new code: a GIF reader in `captastic-core` and an MP4 reader in
  `captastic-windows`, each tested by reading back what its writer wrote.

## Slices

As listed under Milestone 9 in `ROADMAP.md`: reading recordings back; drafts, saving and recovery;
the review window with trim; crop.
