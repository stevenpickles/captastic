# ADR 0006: HDR source handling

## Status

Accepted for the Windows prototype.

## Context

When HDR is enabled for any display, the Windows compositor composes that output in
`R16G16B16A16_FLOAT` with scRGB semantics: linear samples, sRGB primaries, and 1.0 at the sRGB white
point rather than at the maximum. Highlights exceed 1.0, which is the point of the format.

Nothing Captastic publishes to can carry that. A PNG has no way to say "these samples are linear and
1.0 is not the maximum" short of an embedded ICC profile, and the clipboard's DIBV5 payload has no
encoding for half-floats at all. ADR 0002's destinations therefore cannot accept the format, and
since #41 they refuse it by name rather than misinterpreting its bytes.

Until 2026-08-17 the capture path never got that far: `DuplicateOutput` returned the float surface,
readback rejected it, and the user saw `unsupported DXGI desktop format 10`. A display setting broke
the entire application and reported an integer.

Converting to 8-bit is not a cast. Mapping scRGB into sRGB is tone mapping, and the choice of curve
decides whether a bright window looks correct or washed out, and whether highlights clip or roll off.
The curve also depends on the display's SDR white level, which the user sets and which Windows
exposes separately.

## Decision

**Captastic asks the compositor for 8-bit BGRA and lets it do the conversion.**
`IDXGIOutput5::DuplicateOutput1` takes the formats the caller accepts; Captastic lists
`B8G8R8A8_UNORM` and nothing else.

Three consequences follow, and they are the decision as much as the sentence above.

**Only BGRA8 is listed.** Offering the float format as a fallback would let the compositor hand back
pixels the backend has decided not to interpret. The decision to let Windows own the tone mapping is
only coherent while there is no second path — otherwise the same desktop yields different screenshots
depending on what the OS felt like providing.

**Captastic does not implement a tone-mapping curve.** The conversion belongs to the compositor,
which is the same conversion it performs for anything else reading the desktop as SDR. That makes a
Captastic screenshot of an HDR desktop look like every other tool's screenshot of it rather than like
Captastic's opinion of it, and it means the SDR white level is accounted for by the component that
already knows it. The cost is that the algorithm is Microsoft's and cannot be tuned; that cost is
accepted.

**A mixed desktop composes as SDR.** Each output is converted as it is duplicated, so
virtual-desktop composition sees one uniform format — which is what the composer already requires,
so it needs no change. Enabling HDR on one display does not remove virtual-desktop capture from the
user who did it.

**The 16-float contract stays, unproduced.** `PixelFormat::Rgba16Float`, `ColorSpace::ScRgb` and the
sinks' refusals remain from #41. Nothing produces them now, and that is the intended state: the
contract is ready for an output format that can carry high dynamic range, and until then no frame can
reach a sink that cannot handle it — by construction rather than by care.

## Consequences

An HDR desktop is capturable, and the result is an SDR image whose appearance is the compositor's
choice rather than a documented curve of ours. A user comparing Captastic's output against another
capture tool's should see the same thing; a user comparing either against what their HDR display
shows them will not, and that is inherent to publishing SDR.

The fallback path remains. If `IDXGIOutput5` is unavailable or the BGRA8 request is refused,
Captastic duplicates the output as before and reports what actually arrived, naming the format and
what would restore capture instead of printing its number.

Preserving high dynamic range end to end is not addressed here. It needs an output format that can
carry it (AVIF, JXL, HEIF), a decision about what the clipboard receives when the file keeps more
than the clipboard can, and an HDR display to judge any of it on. When that work happens it will
need its own duplication path, and this ADR will need revisiting rather than extending.
