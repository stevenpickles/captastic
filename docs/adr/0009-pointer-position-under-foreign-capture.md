# ADR 0009: Pointer position when something else owns the mouse

## Status

Accepted for the Windows prototype.

## Context

Captastic uses the pointer for two different things, and until cursor composition landed (#46) only
the first existed:

- **choosing a display.** The `pointer` policy — the intended default, because it follows where the
  user is looking — resolves the physical cursor position against the enumerated displays.
- **drawing a cursor.** A composited capture draws the pointer shape at the position DXGI reported
  for the frame being captured.

Three kinds of software take the mouse away from the ordinary arrangement where those two agree:

- **A software KVM** (Synergy, Barrier, Input Leap, Logitech Flow) forwards input to another
  machine. Windows keeps reporting a cursor position on *this* machine — usually parked at a screen
  edge — while the user's attention, and their pointer, are somewhere else entirely.
- **A remote-desktop client** in windowed mode has its own cursor inside its window, and the host's
  physical cursor is over the client window rather than over what the user perceives as the pointer.
- **An application with exclusive pointer capture** — a game, a 3D viewport, a drag with
  `SetCapture` — moves a virtual cursor while the physical one is clipped or hidden.

The two sources can disagree during these. `GetPhysicalCursorPos` reports where Windows believes the
physical cursor is. `DXGI_OUTDUPL_FRAME_INFO.PointerPosition` reports where the compositor drew the
hardware cursor for a particular frame, along with whether it was visible at all. They are answering
different questions, and both are correct answers to their own.

ADR 0003's amendment sharpens one case. A retained frame may be republished after being *verified
current*, and it carries the pointer position that belonged to its pixels. If the pointer moved —
or moved to another machine — between capture and materialization, that recorded position is the
one consistent with the image and inconsistent with the world.

## Decision

**Each use takes the source that answers its own question, and Captastic promises nothing about
where a foreign owner has put the mouse.**

**Display selection uses the physical cursor.** It is asking "which screen is the user at", and the
physical cursor is Windows' answer to that. Under a software KVM the answer will sometimes be the
screen edge the pointer was parked at when input was forwarded away. That is not wrong so much as
unanswerable: the user is at another machine, and no local API knows which screen they would have
chosen. Capturing the display the pointer was last on is a defensible answer, and it is already
softened by the `PointerOutsideDisplays` fallback to the primary display.

**Cursor composition uses the frame's own pointer position, or draws nothing.** The composited
pixels must be internally consistent — a cursor drawn where the pointer is *now* over pixels from
before it moved is a picture of a moment that never existed. When the compositor reports the pointer
as not visible, nothing is drawn and `CursorAbsence::NotVisible` records why. An application holding
exclusive capture typically hides the hardware cursor, so this is the common outcome and it is the
correct one: the cursor the user sees is being drawn by that application into the desktop image, and
it is already in the capture.

**The disagreement is not measured and not reported.** Sampling both and comparing would produce a
number with no defined meaning: they differ legitimately during any drag, any hidden-cursor
application, and any frame retained across pointer motion. A diagnostic that fires constantly during
normal use trains people to ignore it. `CursorCapture` already records the outcome that matters —
composited at a position, or absent for a stated reason.

**No KVM detection.** There is no reliable way to detect a software KVM: they are ordinary user-mode
processes injecting input, and they neither announce themselves nor share a signature. A heuristic
would be wrong in both directions, and behaviour that changes based on a guess about what other
software is running is worse than behaviour that is simply consistent.

## Consequences

A user of a software KVM who triggers a capture on the machine they are *not* currently driving gets
a capture of whichever display their pointer was last on there. That is the behaviour, and it is
worth saying plainly in documentation rather than presenting `pointer` as if it always follows
attention. `display = "primary"` is the policy for a machine that is regularly driven from
elsewhere.

Captures taken during a drag, or in an application that hides the cursor, will usually contain no
composited pointer. That is not a failure and is recorded as `NotVisible` rather than as an error.

The mouse-capture behaviour Captastic *does* control — `SetCapture` during an overlay drag, released
at button-up, cancelling the drag if capture is lost — is unchanged by this decision and is
documented with the overlay. Its interaction with a software KVM remains untested on real KVM
hardware, and this ADR does not claim otherwise: what is decided here is what Captastic promises,
not what has been verified against every input stack.

Nothing here is enforced by a test, because every claim is about behaviour under third-party
software that is not present on the development machine. The decisions are recorded so that a future
change to the pointer paths is made deliberately rather than by accident.
