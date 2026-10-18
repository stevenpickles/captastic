# ADR 0011: Screen recording

## Status

Accepted for the Windows prototype, 2026-10-15. Governs Milestone 9, the 0.4.0 theme.
Trimming, ruled out of 0.4.0 below, was brought into it by [ADR 0012](0012-recording-review.md).

## Context

Milestone 9 records a region, window or display over time, where every earlier feature captured one
frame. The pieces are mostly there: DXGI duplication yields timed desktop frames for displays and
regions, Windows Graphics Capture yields them for windows, the overlay already chooses what to
capture, and the file worker already writes into the output directory and history. What is new is
everything that happens between the first frame and the last, and five questions shape it:

- **A recording runs while the user does other things.** A still capture is over before the user
  can look away; a recording is not, so it must be visible the whole time it runs, and it must be
  impossible to leave one running by accident.
- **Its size grows with time.** A 4K display at 30 frames a second is about 1 GB of raw pixels a
  second. Nothing may hold frames for the length of a recording; memory has to be bounded by the
  encoder, not the duration.
- **The capture engine is shared.** The DXGI readback pool serves still captures (ADR 0010 sized it
  around pins and editors that never hold a slot). A recording reads frames continuously and must
  not starve a still capture or be starved by one.
- **The formats pull in opposite directions.** GIF is what chats, issues and documents embed
  directly, but it has 256 colours a frame and no inter-frame compression beyond transparency, so it
  is large and best kept short and small. MP4/H.264 is small and smooth at any length, and Windows
  has an encoder for it.
- **Where it goes.** A clipboard holds pixels or file references; a recording only makes sense as a
  file.

## Decision

**A recording starts from the overlay's Record button, and nowhere else.** The toolbar gains
**Record** beside **Capture**, **Pin** and **Edit**. It applies to every tool — full display, window
and region — and records exactly what **Capture** would have captured, for as long as the recording
runs. There is no recording hotkey: the overlay is the one way in, so the choice of area is always
made, and seen, first. A run with no notification-area application behind it shows **Record** greyed,
as it does **Pin** and **Edit**.

**A recording is always visible, and the indicator is never in it.** While it runs, a thin red
border surrounds the recorded area, with a small control bar attached outside it: elapsed time and a
**Stop** button. The border and the bar are excluded from capture (`WDA_EXCLUDEFROMCAPTURE`, which
DXGI duplication and Windows Graphics Capture both honour), so they are on the screen and never in
the file. When the area reaches a display's edge, the bar moves to whichever side has room, so
**Stop** is always reachable.

**A recording stops at Stop or at its time limit.** **Stop** ends it; so does reaching
`recording.max_seconds` (60 by default, at most 600). Captastic exiting, a display change that
invalidates the area, or the window being recorded closing also end it, and each of these finalizes
what was recorded rather than discarding it. Only one recording runs at a time; the overlay hotkey
pressed during a recording does nothing but raise a balloon saying a recording is running.

**Both GIF and MP4 ship in 0.4.0.** The format is chosen in the overlay's **Options** menu with a
**Record As** row (GIF or MP4) that is remembered like the other options, defaulting to
`recording.format` (`gif`).

- **GIF** records at 15 frames a second, scaled down so its longer side is at most
  `recording.gif_max_dimension` (1280 by default), with a palette per frame from a permissively
  licensed quantizer (`color_quant`'s NeuQuant, not the GPL `imagequant`), unchanged pixels between
  frames left transparent so they cost nothing, and no dithering by default, since dithering defeats
  that.
- **MP4** records at 30 frames a second at the area's own size (rounded down to even dimensions, as
  H.264 requires), encoded to H.264 by Windows Media Foundation's sink writer, which uses the GPU's
  encoder where there is one and adds no dependency.

**A finished recording is a file, and only a file.** It is written to the output directory, under
the same filename template as stills with the format's extension, and recorded in capture history —
whether or not still-capture file output is on, since a recording has nowhere else to go. The
clipboard is untouched. A notification-area balloon names the file when it is saved, and clicking it
shows it in its folder.

**Frames are streamed, never accumulated.** A recording worker takes frames from the existing
sources at the format's rate into a bounded queue of a few frames, owned and detached copies like a
pin's (ADR 0010), and the encoder consumes them on its own thread, writing as it goes. If the encoder
falls behind, the newest frame replaces the oldest unencoded one and the drop is counted and logged;
a recording never grows memory to keep up. The pointer is composited into each frame as it is into
stills; there is no audio.

**Trimming is out of scope for 0.4.0.** A recording is saved as recorded. The editor stays a
still-image tool.

## Consequences

- The toolbar widens again, for **Record**, and its layout, hit testing and the DPI tables in
  `docs/overlay-ui-verification.md` change; the 200 % toolbar must still fit a 1920-pixel display.
- Still captures are refused for the length of a recording. That keeps the engine's two uses from
  competing for the readback pool, at the cost of not being able to take a screenshot mid-recording;
  a later release may lift it.
- A recording's quality is bounded on purpose: GIF at 15 fps and 1280 px is legible and small, not
  faithful. MP4 is the faithful option.
- `[recording]` joins the configuration (`format`, `max_seconds`, `gif_max_dimension`), and `Record
  As` joins the remembered UI state.
- Media Foundation is a Windows facility, so MP4 joins the platform-specific code ADR 0001 expects;
  GIF encoding is portable and lives where the other encoders do.
- No tray state changes during a recording: the border and its bar are the indicator, and **Stop**
  and the time limit are the ways out. If the overlay's area becomes unreachable — a display
  disappearing — the recording ends with it.

## As built (0.4.0)

What implementing this settled, for the next reader:

- **A window is recorded as its area on the screen.** Recording a window through Windows Graphics
  Capture would follow the window and see it when covered, but a session per frame is far too slow
  and a persistent one is a second capture engine. A window recording therefore records the
  rectangle the window occupied when **Record** was pressed, from the same desktop duplication as
  regions, and records whatever is in that rectangle - including another window moved over it.
- **Frames come from the daemon's capture thread**, one `RecordFrame` request at a time, so a
  recording shares the engine's display list and recovery. Each is a full CPU readback cropped to
  the area; reading back only the area on the GPU is a later optimization.
- **The queue is sized in bytes**, 128 MiB, between 2 and 90 frames, rather than a fixed four: the
  first H.264 frame can stall while the hardware encoder starts, and a small recording should ride
  that out without dropping frames. The clock starts once the encoder is ready.
- **The bar** shows the elapsed time against the limit and **Stop**, then "Saving..." until the
  file is finished; the border is 3 DIP, outside the area, and the bar goes below the area, above
  it, or inside its bottom edge, whichever keeps it on the screen.

## Slices

As listed under Milestone 9 in `ROADMAP.md`: the recording engine; GIF output; the Record button, the
excluded border and its Stop bar; MP4 output. All four gate 0.4.0.
