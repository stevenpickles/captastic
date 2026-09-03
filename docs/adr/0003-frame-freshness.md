# ADR 0003: Frame freshness

## Status

Accepted for Phase 0. Amended 2026-08-17: verified currency (below).

## Decision

Captastic exposes two modes and never merges their measurements:

- `fresh` waits for a qualifying post-trigger frame within a timeout;
- `latest` drains an immediately available frame at trigger time, otherwise reuses the retained frame, and reports its age.

The daemon performs no Desktop Duplication acquisition while idle. A fast latest-frame result without frame-age data is invalid. Presentation-time provenance is recorded so OS timestamps, inferred arrival timestamps, and synthetic timestamps cannot be confused.

## Amendment: a frame proven unchanged is current

Desktop duplication yields a frame only when the desktop image changes. A zero-timeout acquisition
that finds nothing pending is therefore not a failure to find a frame — it is positive evidence that
nothing has been presented since the retained one, which means the retained frame is pixel-identical
to what the screen shows at the moment of the probe.

Captastic records that instant as `verified_current_offset_ns`, alongside — never instead of —
`frame_age_ns`. The two answer different questions and neither is derived from the other: one says
how long ago the frame was presented, the other says when it was last proven to be what the screen
shows. On a busy desktop they nearly agree. On a static desktop they diverge without limit, and it is
the second that a user asking about currency means.

Two behaviours follow, and both replace a documented behaviour that made Captastic least useful
exactly where the desktop was quiet.

**A staleness limit tests currency, not age.** `max_frame_age_ms` accepts a frame that has been
proven current however long ago it was presented. Rejecting such a frame refused it on the strength
of a number describing something else, and made the opt-in unusable on a static desktop — the case
where a stale frame is most likely and also where staleness is least possible.

**`fresh` accepts a frame it can prove current.** When the timeout expires with nothing presented,
Captastic probes; if the probe proves the retained frame current, that frame is returned. It
satisfies what `fresh` asks for in every way a caller can observe, and without it `fresh` fails on any
idle desktop — and `fresh` + `virtual_desktop` fails whenever *any* display is idle, because
composition applies the per-output contract to every output. That combination was near-unusable on a
real multi-monitor desk and nothing said why.

This applies uniformly, not only inside composition. One output behaving differently depending on
whether it was captured alone or as part of a composite would be a worse contract than either
behaviour on its own.

A composite is only as verified as its least-recently-verified output, and is not verified at all
unless every output is: one freshly-checked display cannot vouch for a stale neighbour it knows
nothing about.

### What verified currency does not cover

It is a claim about the **desktop image**. The pointer moves without dirtying that image — which is
precisely why the compositor reports pointer position separately from frame content — so a
verified-current frame may carry a pointer position that has since changed. The position composited
into such a frame is the one that belonged to those pixels, which keeps the image self-consistent at
the cost of a cursor that may no longer be where the mouse is. Anything needing the live pointer
position must ask for it separately.

The initial-frame timeout, raised when nothing has ever been retained and the desktop has not
changed, is not retryable. Duplication produces nothing until something repaints, so a caller
retrying in a tight loop learns nothing while spending its deadline.
