# ADR 0012: Reviewing a recording before it is saved

## Status

Accepted for the Windows prototype, 2026-10-17. Extends ADR 0011 within Milestone 9, for 0.4.0.
Amended 2026-10-21: a recording in review is held in memory rather than in a drafts folder on disk,
at the maintainer's request; see *Amendment* below. From 0.5.0 the review window plays an MP4's
sound and a trimmed save keeps it, per [ADR 0015](0015-recording-system-audio.md).

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

**The recording is reviewed from memory, and read back from there.** A recording is encoded exactly
as ADR 0011 describes, but into memory rather than into a file: the encoded GIF or MP4, the size
the file would be, not its raw frames. The review window reads it back: GIF frames are kept in their
compressed form and decoded on demand, and MP4 frames are decoded by Media Foundation's source
reader, seeking from the nearest key frame. Memory stays near the size of the file however long the
recording is.

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
history, and raises the "Recording saved" notification. Saved unchanged, the recording's bytes are
written there as they are, with no encoding. Saved trimmed or cropped, the kept frames are decoded,
cropped and encoded again into memory by the writers ADR 0011 uses, and the result is written. The
window closes at
**Save**, and the "Recording saved" notification says when the file is ready; an edited save takes
as long as encoding the span kept.

**Discarding is always deliberate.** **Discard**, Escape and the window's close button all ask
first: *"Discard this recording? It will be deleted."* Only **OK** lets it go, and since it was
never written, nothing is left behind. Captastic exiting during a review saves the recording
unchanged, as exiting during a recording finishes it; an edited save still encoding when exit's
deadline passes saves the recording unchanged instead. A crash or a power cut during a review loses
the recording.

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

## Amendment (2026-10-21)

As first built, a recording in review was encoded into `.captastic-drafts`, a hidden folder in the
output directory, and a draft left there by a crash was saved at the next start. Trying it, the
maintainer asked that recordings in review not be kept on disk at all: a review either saves the
result or discards it entirely. Holding the encoded recording in memory does that, at the cost of
"nothing may be lost by accident" for a crash during a review, which is accepted. The memory held
is the encoded file's size, plus one copy while an MP4 is decoded for the review; a recording that
waits behind another review holds its own.

## Slices

As listed under Milestone 9 in `ROADMAP.md`: reading recordings back; holding, saving and discarding;
the review window with trim; crop.
