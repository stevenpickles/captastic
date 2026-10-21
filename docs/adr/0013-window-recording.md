# ADR 0013: Recording a window through Windows Graphics Capture

## Status

Accepted for the Windows prototype, 2026-10-21. Governs Milestone 9's 0.5.0 section. The user-facing choices (resizing, and
what happens when a window cannot be captured) were made by the maintainer on 2026-10-21. Amends the
first *As built* note of [ADR 0011](0011-screen-recording.md).

## Context

ADR 0011 records a window as the rectangle it occupied when **Record** was pressed, read from the
same desktop duplication as regions. That was a deliberate shortcut: one capture engine for every
recording. Its cost is 0.4.0's main known limitation, stated in its release notes. Whatever is in the
rectangle is recorded: another window moved over it, an always-on-top window, and the desktop behind
the window if it moves.

Windows Graphics Capture (WGC) captures a window's own pixels, wherever it is and however it is
covered. Captastic already uses it, one frame at a time, as the fallback for still window captures
(`window_capture_wgc.rs`): build a device, a capture item, a frame pool and a session, wait for one
frame, read it back, tear it all down. That costs tens of milliseconds, fine once and far too slow
thirty times a second. Recording through WGC means keeping one session open for the whole
recording. The questions are what owns it, how its frames meet the recording's clock, and what
happens when the window changes size, disappears or cannot be captured.

## Decision

**A window recording keeps one WGC session for its whole length.** A new recording-only source in
`captastic-windows`, `WgcWindowSession`, owns the device, capture item, free-threaded frame pool and
session for the life of one recording. It runs on its own thread with its own WinRT apartment, and it
keeps the uncached-factory rule that `window_capture_wgc.rs` documents. Nothing about it touches the
daemon's capture thread, so display and region recordings and still captures initialise no WGC at
all. It implements the recording engine's existing `FrameSource`, so pacing, the bounded queue, the
encoders and the review are unchanged.

Milestone 3's window-capture backend trait is not introduced here. The still path and the recording
path have different lifetimes (one frame against a session), and a trait over both would be the
generic abstraction Milestone 3 warns against. If a later change retains a session for still captures
too, that is when the trait earns its place.

**The recording keeps its own clock; WGC keeps the newest frame.** WGC delivers a frame only when the
window's contents change. The session keeps the newest frame that has arrived and drops older ones. At
each tick of the recording's 15 or 30 fps the engine takes whatever is newest. When nothing new has
arrived, it repeats the previous frame, which the GIF writer already stores as transparent unchanged
pixels and H.264 encodes for almost nothing. A recording of a window that does not change is still a
recording of the full length.

**The output size is fixed at the start; a resized window is scaled to fit.** A GIF or MP4 has one
size. It is set from the window's size when the recording starts, after the same GIF limit and MP4
even-dimension rules ADR 0011 applies. When the window is resized, the session recreates its frame
pool at the new size (`Recreate`). Each frame is then scaled to fit inside the output size with its
proportions kept, centred, with black bars where the shapes differ. A window that returns to its
starting size is recorded at 1:1 again. This is the maintainer's choice of 2026-10-21, over keeping
pixels at 1:1 and cropping or padding: the whole window is always visible.

**What is in a frame is the window, and only the window.** Its own pixels at their physical size,
including the parts off-screen or on another display. The pointer is included only when `[capture]
cursor` includes it for stills, through the session's `IsCursorCaptureEnabled`, rather than
composited by Captastic. The yellow capture border Windows draws is turned off with
`IsBorderRequired(false)` where Windows allows it, as for stills. Where Windows insists on the border,
the border is on the screen, not in the frame. Captastic says nothing about it, since it is Windows
telling the user a capture is running.

**Minimised, hidden and closed windows.** A minimised window delivers no frames. The recording keeps
running, repeating the last frame, and the indicator stays where the window was, so **Stop** remains
reachable. When the window is restored, frames resume. A window that closes raises the capture item's
`Closed` event, and the recording ends and keeps what was recorded, as ADR 0011 already promises for
a closed window.

**The indicator follows the window.** The red border and the **Stop** bar track the window's
rectangle as it moves and is resized, polled at the recording's frame rate from the window's frame
bounds (`DWMWA_EXTENDED_FRAME_BOUNDS`). They keep ADR 0011's placement rules for the bar and stay
excluded from capture. WGC captures the window, not the screen, so the indicator could not appear in
the recording even without that exclusion.

**When WGC cannot capture the window, the recording falls back to its area.** WGC may be unsupported,
the window may refuse capture (protected content, or a window elevated above Captastic), or the
session may fail to deliver a first frame within the still path's first-frame timeout. In each case
the recording proceeds as in 0.4.0: the window's area through desktop duplication, with the window
brought to the front. The fallback and its reason are logged, put in the `recording_started` JSON
event, and stated in the saved-recording notification ("Recorded the window's area on the screen:
Windows did not allow capturing the window itself"). This is the maintainer's choice of 2026-10-21,
over refusing the recording. A fallback after the first frame is not attempted: a session that fails
mid-recording ends the recording and keeps what it has, as any other source failure does.

## Consequences

- A window recording no longer depends on the window being in front, so the 0.4.0 behaviour of
  bringing it forward is kept only for the fallback.
- WGC frames are GPU textures. Each one is read back to the CPU as the still path does, at up to 30 a
  second. That is the cost Milestone 9's memory ceiling and the still-capture latency criterion are
  checked against. A GPU-side scale or crop before readback is a later optimization, as it is for
  desktop duplication.
- Scaling a resized window is done on the CPU, after readback, in the same place the GIF's downscale
  is done today, so there is one resampling step, not two.
- A window spanning two displays, or partly off-screen, is recorded whole, where 0.4.0 cut it at the
  display's edge or refused it.
- The recording's size and the window's size can now differ, so the review window, the notification
  and history report the output size, which is what they already report.
- ADR 0011's first *As built* note is superseded for windows WGC can capture.

## As built (0.5.0)

What implementing this settled, for the next reader:

- **One MTA reference for the process.** Every WGC thread joins the multithreaded apartment, and
  when the last one leaves, Windows tears the apartment down while capture callbacks can still be
  running, which crashed one session in two in back-to-back tests. The first session takes a
  `CoIncrementMTAUsage` reference that is never released, so the apartment outlives every session.
- **Closing is noticed two ways.** The capture item's `Closed` event is not always raised for a
  window that is destroyed, so the session also checks `IsWindow` each time it wakes.
- **The decision is made on the recording thread**, after the overlay has closed: the session is
  started there, and the indicator, already showing around the window's starting rectangle, is told
  to follow the window only once it has. So the `recording_started` JSON event is printed from that
  thread, once the source is known, and carries `window_source` (`windows_graphics_capture` or
  `screen_area`) for a window recording, with `fallback_reason` beside it for a fallback.
- **The window is still brought to the front** for every window recording, not only the fallback:
  the overlay hands focus over as it closes, before anyone knows whether WGC will capture the window,
  and bringing forward a window recorded as itself costs nothing.
- **The indicator follows the window's visible frame**, read every 33 ms on the desktop UI thread,
  which is what a WGC recording records. While the window is minimised it stays where it was.

## Slices

As listed under Milestone 9's 0.5.0 section in `ROADMAP.md`:

1. This ADR.
2. `WgcWindowSession` and its tests: newest-frame handoff, resizing, minimised and closed windows, and
   teardown. The desktop tests are ignored on hosted CI, as the existing WGC tests are.
3. Window recordings through it: the `FrameSource`, the fixed output size and scaling, the pointer
   setting, and the fallback.
4. The indicator following the window, the notices, and the README, release notes and ADR 0011
   updated.
