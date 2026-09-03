//! Pointer shapes, and drawing one into a captured frame.
//!
//! Two things about the desktop-duplication pointer shape the rest of this module is arranged
//! around.
//!
//! The compositor sends a shape only when it *changes*. Every frame reports where the pointer is,
//! but `PointerShapeBufferSize` is zero unless the picture is different from last time — so a
//! backend that only drew what arrived with the current frame would draw a cursor roughly once per
//! shape change and nothing the rest of the time. [`PointerCache`] is what makes the position
//! useful on the other frames.
//!
//! And a pointer is not simply a small image. Three formats arrive, two of which can *invert* the
//! pixels underneath rather than replace them, which is how a text I-beam stays visible over a
//! background of any colour. Compositing those correctly is the difference between a screenshot of
//! a text field that looks right and one where the caret is a black bar on black text.

use captastic_core::{CursorAbsence, CursorCapture};

/// How a pointer shape's bytes describe its pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PointerShapeKind {
    /// 32 bits per pixel, BGRA, straight alpha.
    Color,
    /// 1 bit per pixel, and twice the declared height: an AND mask stacked over an XOR mask.
    ///
    /// Per pixel, the pair selects one of four outcomes — transparent, black, white, or invert
    /// what is underneath. The last is why this cannot be flattened to an image with alpha.
    Monochrome,
    /// 32 bits per pixel, where the alpha byte is a selector rather than a blend factor: zero
    /// copies the colour, anything else XORs it into the destination.
    MaskedColor,
}

/// A pointer shape, owned so it can outlive the frame that delivered it.
#[derive(Clone, Debug)]
pub(crate) struct PointerShape {
    pub(crate) kind: PointerShapeKind,
    pub(crate) width: u32,
    /// Pixel rows the shape occupies on screen. For [`PointerShapeKind::Monochrome`] the buffer
    /// holds twice this many rows.
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) pixels: Vec<u8>,
}

/// The last shape the compositor sent, held across frames because it will not send it again.
#[derive(Debug, Default)]
pub(crate) struct PointerCache {
    shape: Option<PointerShape>,
    /// The last position and visibility DXGI reported, and whether it has reported at all.
    ///
    /// DXGI describes the pointer incrementally: `DXGI_OUTDUPL_FRAME_INFO` carries a position and
    /// a visibility flag only on a frame where the pointer changed, signalled by a non-zero
    /// `LastMouseUpdateTime`, and leaves both at their defaults otherwise. Reading them
    /// unconditionally means a stationary pointer reads as `Visible = false` on every frame that
    /// was produced by the desktop repainting — which is nearly all of them, and is why
    /// composition never fired outside a test that moved the mouse at exactly the right moment.
    ///
    /// So the position is remembered here for the same reason the shape is: the compositor tells
    /// you what changed, and the caller is expected to know the rest.
    position: Option<PointerPosition>,
}

/// Where the pointer was, the last time the compositor said anything about it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PointerPosition {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) visible: bool,
}

impl PointerCache {
    pub(crate) fn store(&mut self, shape: PointerShape) {
        self.shape = Some(shape);
    }

    pub(crate) fn current(&self) -> Option<&PointerShape> {
        self.shape.as_ref()
    }

    /// Records what a frame reported about the pointer. Call only for a frame that reported.
    pub(crate) fn store_position(&mut self, x: i32, y: i32, visible: bool) {
        self.position = Some(PointerPosition { x, y, visible });
    }

    /// The last reported position, or `None` if the pointer has never been reported.
    pub(crate) fn position(&self) -> Option<PointerPosition> {
        self.position
    }
}

/// Where the pointer is, and what it looks like, for one captured frame.
#[derive(Debug)]
pub(crate) struct PointerSample<'a> {
    /// Top-left of the shape in frame coordinates, after rotation normalization.
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) shape: &'a PointerShape,
}

/// Draws a pointer into a top-left BGRA frame, clipped to its bounds.
///
/// Returns what to record about the cursor in the frame's metadata. Composition happens before any
/// crop, so a pointer straddling a selection edge is clipped by the crop exactly as it was clipped
/// by the edge of the screen — one path, and no bounds test that could disagree with the one the
/// crop already performs.
pub(crate) fn composite_pointer(
    destination: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    stride: usize,
    pointer: &PointerSample<'_>,
) -> CursorCapture {
    let shape = pointer.shape;
    if shape.width == 0 || shape.height == 0 {
        return CursorCapture::Absent {
            reason: CursorAbsence::ShapeNotYetKnown,
        };
    }

    for row in 0..shape.height {
        let Some(destination_y) = offset_row(pointer.y, row, frame_height) else {
            continue;
        };
        for column in 0..shape.width {
            let Some(destination_x) = offset_row(pointer.x, column, frame_width) else {
                continue;
            };
            let start = destination_y * stride + destination_x * 4;
            let Some(target) = destination.get_mut(start..start + 4) else {
                continue;
            };
            match shape.kind {
                PointerShapeKind::Color => blend_color(shape, row, column, target),
                PointerShapeKind::Monochrome => blend_monochrome(shape, row, column, target),
                PointerShapeKind::MaskedColor => blend_masked_color(shape, row, column, target),
            }
        }
    }

    CursorCapture::Composited {
        x: pointer.x,
        y: pointer.y,
        width: shape.width,
        height: shape.height,
    }
}

/// Maps one shape row or column onto the frame, or `None` when it falls outside.
///
/// Signed on purpose: a pointer near the top-left of the screen has a negative origin once its
/// hotspot is accounted for, and those rows are clipped rather than wrapped.
fn offset_row(origin: i32, offset: u32, limit: u32) -> Option<usize> {
    let position = i64::from(origin) + i64::from(offset);
    if position < 0 || position >= i64::from(limit) {
        return None;
    }
    usize::try_from(position).ok()
}

fn source_pixel(shape: &PointerShape, row: u32, column: u32) -> Option<[u8; 4]> {
    let start = (row as usize) * (shape.pitch as usize) + (column as usize) * 4;
    shape.pixels.get(start..start + 4)?.try_into().ok()
}

fn blend_color(shape: &PointerShape, row: u32, column: u32, target: &mut [u8]) {
    let Some(pixel) = source_pixel(shape, row, column) else {
        return;
    };
    let alpha = u32::from(pixel[3]);
    if alpha == 0 {
        return;
    }
    if alpha == 255 {
        target[..3].copy_from_slice(&pixel[..3]);
        return;
    }
    // Straight alpha, matching FrameAlpha::Straight elsewhere: the shape's colour is not
    // premultiplied, so it is scaled here rather than merely added.
    //
    // The destination's own alpha byte is never written, here or in any other blend. A capture is
    // opaque and stays opaque; copying a shape's alpha into it would punch a translucent
    // cursor-shaped hole in a screenshot that has nothing behind it.
    for (channel, source) in target[..3].iter_mut().zip(&pixel[..3]) {
        let source = u32::from(*source);
        let existing = u32::from(*channel);
        *channel = ((source * alpha + existing * (255 - alpha)) / 255) as u8;
    }
}

fn blend_masked_color(shape: &PointerShape, row: u32, column: u32, target: &mut [u8]) {
    let Some(pixel) = source_pixel(shape, row, column) else {
        return;
    };
    if pixel[3] == 0 {
        target[..3].copy_from_slice(&pixel[..3]);
    } else {
        // The alpha byte is a selector, not a blend factor: anything non-zero means XOR.
        for (channel, source) in target[..3].iter_mut().zip(&pixel[..3]) {
            *channel ^= *source;
        }
    }
}

/// Reads one bit from a 1bpp mask row, most significant bit first.
fn mask_bit(shape: &PointerShape, row: u32, column: u32) -> Option<bool> {
    let byte_index = (row as usize) * (shape.pitch as usize) + (column as usize) / 8;
    let byte = shape.pixels.get(byte_index)?;
    let shift = 7 - (column % 8);
    Some((byte >> shift) & 1 == 1)
}

fn blend_monochrome(shape: &PointerShape, row: u32, column: u32, target: &mut [u8]) {
    // The AND mask occupies the first `height` rows and the XOR mask the next `height`.
    let (Some(and_bit), Some(xor_bit)) = (
        mask_bit(shape, row, column),
        mask_bit(shape, row + shape.height, column),
    ) else {
        return;
    };
    match (and_bit, xor_bit) {
        // AND 0: the destination is replaced, by black or by white.
        (false, false) => target[..3].fill(0x00),
        (false, true) => target[..3].fill(0xff),
        // AND 1: the destination survives, either untouched or inverted.
        (true, false) => {}
        (true, true) => {
            for channel in &mut target[..3] {
                *channel = !*channel;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, fill: u8) -> Vec<u8> {
        vec![fill; (width * height * 4) as usize]
    }

    fn color_shape(width: u32, height: u32, pixels: Vec<u8>) -> PointerShape {
        PointerShape {
            kind: PointerShapeKind::Color,
            width,
            height,
            pitch: width * 4,
            pixels,
        }
    }

    fn pixel_at(frame: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        frame[y * stride + x * 4..y * stride + x * 4 + 4]
            .try_into()
            .unwrap()
    }

    #[test]
    fn an_opaque_color_pointer_replaces_the_pixels_it_covers() {
        let mut destination = frame(4, 4, 0x10);
        let shape = color_shape(2, 2, [0x80, 0x80, 0x80, 0xff].repeat(4));
        let outcome = composite_pointer(
            &mut destination,
            4,
            4,
            16,
            &PointerSample {
                x: 1,
                y: 1,
                shape: &shape,
            },
        );

        assert_eq!(
            outcome,
            CursorCapture::Composited {
                x: 1,
                y: 1,
                width: 2,
                height: 2
            }
        );
        // Colour replaced; the frame's own alpha byte deliberately left as it was.
        assert_eq!(pixel_at(&destination, 16, 1, 1), [0x80, 0x80, 0x80, 0x10]);
        assert_eq!(pixel_at(&destination, 16, 2, 2), [0x80, 0x80, 0x80, 0x10]);
        // Outside the shape, untouched. This is the property the whole exit criterion rests on:
        // a cursor-on capture must differ from a cursor-off one only where the pointer is.
        assert_eq!(pixel_at(&destination, 16, 0, 0), [0x10; 4]);
        assert_eq!(pixel_at(&destination, 16, 3, 3), [0x10; 4]);
    }

    #[test]
    fn a_transparent_pointer_pixel_leaves_the_frame_alone() {
        let mut destination = frame(2, 1, 0x10);
        // One fully transparent pixel beside one opaque pixel.
        let shape = color_shape(2, 1, vec![0xff, 0xff, 0xff, 0x00, 0x20, 0x20, 0x20, 0xff]);
        composite_pointer(
            &mut destination,
            2,
            1,
            8,
            &PointerSample {
                x: 0,
                y: 0,
                shape: &shape,
            },
        );

        assert_eq!(pixel_at(&destination, 8, 0, 0), [0x10; 4]);
        assert_eq!(pixel_at(&destination, 8, 1, 0)[..3], [0x20, 0x20, 0x20]);
    }

    #[test]
    fn a_half_transparent_pointer_pixel_blends_with_what_is_under_it() {
        let mut destination = frame(1, 1, 0x00);
        let shape = color_shape(1, 1, vec![0xff, 0xff, 0xff, 0x80]);
        composite_pointer(
            &mut destination,
            1,
            1,
            4,
            &PointerSample {
                x: 0,
                y: 0,
                shape: &shape,
            },
        );

        // 255 * 128 / 255 = 128, over black.
        assert_eq!(pixel_at(&destination, 4, 0, 0)[..3], [0x80, 0x80, 0x80]);
    }

    #[test]
    fn a_pointer_at_the_edge_is_clipped_rather_than_wrapped() {
        let mut destination = frame(4, 4, 0x10);
        let shape = color_shape(2, 2, [0x99, 0x99, 0x99, 0xff].repeat(4));

        // Three pixels of this shape fall outside the frame, on two different edges.
        let outcome = composite_pointer(
            &mut destination,
            4,
            4,
            16,
            &PointerSample {
                x: 3,
                y: 3,
                shape: &shape,
            },
        );

        assert_eq!(pixel_at(&destination, 16, 3, 3), [0x99, 0x99, 0x99, 0x10]);
        // Nothing wrapped onto the opposite edge, which is what an unsigned offset would have done.
        assert_eq!(pixel_at(&destination, 16, 0, 0), [0x10; 4]);
        assert_eq!(pixel_at(&destination, 16, 0, 3), [0x10; 4]);
        assert_eq!(pixel_at(&destination, 16, 3, 0), [0x10; 4]);
        // Still reported at its full size: the frame records where the pointer was, and the crop
        // that follows decides how much of it survives.
        assert_eq!(
            outcome,
            CursorCapture::Composited {
                x: 3,
                y: 3,
                width: 2,
                height: 2
            }
        );
    }

    #[test]
    fn a_pointer_whose_origin_is_off_screen_draws_only_its_visible_part() {
        let mut destination = frame(4, 4, 0x10);
        let shape = color_shape(2, 2, [0x99, 0x99, 0x99, 0xff].repeat(4));
        composite_pointer(
            &mut destination,
            4,
            4,
            16,
            &PointerSample {
                x: -1,
                y: -1,
                shape: &shape,
            },
        );

        assert_eq!(pixel_at(&destination, 16, 0, 0), [0x99, 0x99, 0x99, 0x10]);
        assert_eq!(pixel_at(&destination, 16, 1, 0), [0x10; 4]);
        assert_eq!(pixel_at(&destination, 16, 0, 1), [0x10; 4]);
    }

    #[test]
    fn a_monochrome_pointer_paints_black_white_transparent_and_inverted() {
        // One row of four pixels, one per (AND, XOR) combination:
        //   AND 0 0 1 1
        //   XOR 0 1 0 1
        // which is black, white, unchanged, inverted.
        let shape = PointerShape {
            kind: PointerShapeKind::Monochrome,
            width: 4,
            height: 1,
            pitch: 1,
            pixels: vec![0b0011_0000, 0b0101_0000],
        };
        let mut destination = vec![
            0x11, 0x22, 0x33, 0xff, // black
            0x11, 0x22, 0x33, 0xff, // white
            0x11, 0x22, 0x33, 0xff, // unchanged
            0x11, 0x22, 0x33, 0xff, // inverted
        ];

        composite_pointer(
            &mut destination,
            4,
            1,
            16,
            &PointerSample {
                x: 0,
                y: 0,
                shape: &shape,
            },
        );

        assert_eq!(pixel_at(&destination, 16, 0, 0)[..3], [0x00, 0x00, 0x00]);
        assert_eq!(pixel_at(&destination, 16, 1, 0)[..3], [0xff, 0xff, 0xff]);
        assert_eq!(pixel_at(&destination, 16, 2, 0)[..3], [0x11, 0x22, 0x33]);
        // The case that makes an I-beam legible over any background, and the reason this format
        // cannot be flattened into an image with an alpha channel.
        assert_eq!(pixel_at(&destination, 16, 3, 0)[..3], [0xee, 0xdd, 0xcc]);
    }

    #[test]
    fn a_masked_color_pointer_copies_or_inverts_per_pixel() {
        let shape = PointerShape {
            kind: PointerShapeKind::MaskedColor,
            width: 2,
            height: 1,
            pitch: 8,
            pixels: vec![
                0x40, 0x50, 0x60, 0x00, // mask 0: copy this colour
                0x0f, 0x0f, 0x0f, 0xff, // mask non-zero: XOR it in
            ],
        };
        let mut destination = vec![0x11, 0x22, 0x33, 0xff, 0x11, 0x22, 0x33, 0xff];

        composite_pointer(
            &mut destination,
            2,
            1,
            8,
            &PointerSample {
                x: 0,
                y: 0,
                shape: &shape,
            },
        );

        assert_eq!(pixel_at(&destination, 8, 0, 0)[..3], [0x40, 0x50, 0x60]);
        assert_eq!(pixel_at(&destination, 8, 1, 0)[..3], [0x1e, 0x2d, 0x3c]);
    }

    #[test]
    fn the_cache_answers_for_frames_that_carry_no_shape() {
        let mut cache = PointerCache::default();
        assert!(cache.current().is_none());

        cache.store(color_shape(2, 2, vec![0x11; 16]));

        // The point of the cache: the compositor sent this once, and every later frame reports a
        // position with no shape attached.
        assert_eq!(cache.current().map(|shape| shape.width), Some(2));
    }
}
