# ADR 0015: Recording system audio into MP4

## Status

Accepted for the Windows prototype, 2026-10-21. Part of 0.5.0, asked for by the maintainer on 2026-10-21. Amends
[ADR 0011](0011-screen-recording.md), which said a recording has no audio, and
[ADR 0012](0012-recording-review.md), whose review window plays silently.

## Context

0.4.0's notes promised "No audio, ever," and that was true of what shipped: a recording holds
pixels and nothing else. The maintainer now wants MP4 recordings to be able to carry the computer's
sound, such as a video playing, an application's alerts, or a call's far end.

Windows makes this possible without a driver. WASAPI's loopback mode reads, from the shared-mode
audio engine, the mix being sent to an output device, the same sound the user hears. Media
Foundation's sink writer, which already writes the MP4's H.264 stream, takes an AAC stream beside it
through Windows' own AAC encoder. So no new dependency is needed.

GIF has no audio track, so this concerns MP4 only. The questions are when sound is recorded, how it
stays in step with the picture, what happens when the sound or the output device changes, and what
the review window does with it.

## Decision

**Sound is off unless the user turns it on, and only MP4 carries it.** A new setting,
`recording.audio` (`false` by default), is shown as a **Record Sound** row in the overlay's
**Options** menu and remembered like **Record As**. With **Record As** set to GIF, the row is shown
greyed, with a tooltip saying sound needs MP4. The setting is off by default because the system mix
holds everything the computer plays, including a notification, a message's chime or another
application's call. A recording should never carry those unless the user asked.

**What is recorded is the system mix of the default output device.** It comes from a WASAPI
shared-mode loopback client on the default render endpoint, as the user hears it after Windows'
volume and mute, so a muted computer records silence. The microphone is out of scope: it is a
different consent question and a different device. Recording only the recorded window's application
(WASAPI process loopback, Windows 10 2004 and later) is attractive for window recordings, but it is
deferred. This ADR records the whole mix for every MP4 recording with sound, and leaves per-application
sound to a later decision, without changing this setting's meaning.

**The audio is captured on its own thread and joins the MP4 as AAC.** An event-driven loopback client
runs on its own thread for the length of the recording. Its samples are converted to 16-bit PCM,
stereo, at the device's rate (48 kHz or 44.1 kHz). Anything with more channels is downmixed. Then
they pass through a small bounded queue to the encoder thread, which writes them as an AAC-LC stream
at 160 kbit/s beside the H.264 stream on the same sink writer. Audio is about 20 KB a second encoded,
so it does not change Milestone 9's memory ceiling, and the queue's bound is seconds, not megabytes.

**The picture and the sound share one clock.** Both are stamped against the recording's start on
the performance counter: video frames by the engine's tick, as now, and audio packets by the
performance-counter position WASAPI reports with each packet. The sink writer muxes by timestamp, so
the two stay in step however the frame rate and the audio engine's period differ. Loopback delivers
nothing while nothing is playing, so the capture thread writes silence for those stretches. The
audio track is continuous from the first frame to the last, and a player never sees a gap it might
treat as the end of the sound.

**When the sound cannot be recorded, the picture still is.** If the loopback client cannot start
(no output device, or the audio service is unavailable), the recording continues without sound, and
the saved-recording notification says so. If the default output device changes during a recording,
say headphones plugged in, the capture thread opens the new device and fills the changeover with
silence. If the device disappears with no replacement, the rest of the track is silence. Neither ends
the recording.

**The indicator says when sound is being recorded.** The **Stop** bar shows a speaker glyph beside
the elapsed time while sound is recorded, so the user is never unsure whether it is on.

**The review window plays the sound, and editing keeps it.** ADR 0012's review window reads the AAC
stream back with Media Foundation's source reader, as it reads the video, and plays it through the
default output device in step with the playhead. It is silent while the playhead is stepped or
scrubbed, and it has a mute button. Saved unchanged, the recording's bytes are written as they are,
sound included. Saved trimmed, the kept span's sound is decoded and encoded again, cut at the same
millisecond as the picture. A crop changes nothing about the sound.

## Consequences

- `[recording] audio` joins the configuration, and **Record Sound** joins the remembered UI state and
  the Options menu, whose layout and DPI tables in `docs/overlay-ui-verification.md` change.
- The MP4 writer and reader gain a second stream, and the edited-save path gains an audio re-encode.
  A recording without sound is written exactly as in 0.4.0.
- A trimmed recording's sound is compressed twice, as its picture already is. AAC at 160 kbit/s
  makes the loss inaudible for ordinary use.
- The review window becomes the first part of Captastic that plays sound.
- The release notes stop saying "No audio, ever." They say: off by default, MP4 only, the whole
  system mix and never the microphone, and how to tell it is on.
- GIF recordings are unchanged.
- The command line of [ADR 0014](0014-command-line-capture.md) records nothing in 0.5.0, so it
  raises no audio question yet.

## Slices

1. This ADR.
2. **Loopback capture**: the loopback client, conversion and downmix, the silence fill, device
   changes, and its tests. A test that needs a real output device is ignored on hosted CI.
3. **Sound in the MP4**: the AAC stream on the sink writer, the shared clock, `recording.audio`, the
   **Record Sound** row and the bar's glyph.
4. **Sound in the review**: reading the AAC stream back, playback in step with the playhead and its
   mute button, and the trimmed save's audio re-encode.
