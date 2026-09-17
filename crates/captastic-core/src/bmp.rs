//! BMP encoding for captured frames.
//!
//! A Windows bitmap is the one format Captastic can write without a dependency at all: the bytes a
//! capture backend already hands over *are* the pixel array, and everything in front of them is a
//! header of fixed fields. That is the whole argument for offering it — no compression to spend
//! time on, no library to audit, and an image every Windows tool since 1990 opens — and also the
//! whole argument against making it the default: a 4K capture is 33 MB and stays that way.
//!
//! The two headers here are the two the clipboard publisher already chooses between
//! (`captastic-windows/src/clipboard.rs`): a plain `BITMAPINFOHEADER` when the frame is opaque,
//! and a `BITMAPV5HEADER` when it carries straight alpha, because nothing earlier than V5 can say
//! that a fourth channel means anything.

use crate::encode::{EncodeError, OutputFormat, SourceLayout};
use crate::frame::CpuFrame;

/// `BITMAPFILEHEADER`: the two magic bytes, the file size, two reserved words, and the offset the
/// pixels start at. Only a file has one; the clipboard's DIB begins at its info header.
const FILE_HEADER_BYTES: usize = 14;
/// `BITMAPINFOHEADER`.
const INFO_HEADER_BYTES: usize = 40;
/// `BITMAPV5HEADER`: the info header plus masks, a colour space, gamma, and a rendering intent.
const V5_HEADER_BYTES: usize = 124;
/// `BI_RGB`: uncompressed, channels in the layout the bit count implies.
const BI_RGB: u32 = 0;
/// `BI_BITFIELDS`: uncompressed, channels in the layout the masks state.
const BI_BITFIELDS: u32 = 3;
/// `LCS_sRGB`, which is the four characters `sRGB` read as a big-endian integer.
const LCS_SRGB: u32 = 0x7352_4742;
/// `LCS_GM_IMAGES`, the perceptual intent the specification names for photographic content — and a
/// screenshot is photographic content: a picture of what was on a screen.
const LCS_GM_IMAGES: u32 = 4;

/// Encodes a frame as a BMP.
///
/// Opaque frames are written as 24-bpp `BI_RGB` bottom-up rows, which is the shape every reader
/// supports without qualification. Straight-alpha frames — window captures with rounded corners
/// and shadows — are written as 32-bpp `BI_BITFIELDS` under a `BITMAPV5HEADER` whose alpha mask
/// says the fourth channel is real. That costs a third more bytes and buys the only thing BMP has
/// over JPEG here, so it is not offered as a choice.
pub fn encode_frame(frame: &CpuFrame) -> Result<Vec<u8>, EncodeError> {
    let layout = SourceLayout::inspect(frame, OutputFormat::Bmp)?;
    // A BMP measures its sides in signed 32-bit pixels, and this one negates nothing, so the
    // positive range is the limit.
    let (width, height) = match (i32::try_from(frame.width()), i32::try_from(frame.height())) {
        (Ok(width), Ok(height)) => (width, height),
        _ => {
            return Err(EncodeError::DimensionsTooLarge {
                format: OutputFormat::Bmp,
                width: frame.width(),
                height: frame.height(),
                limit: i32::MAX as u32,
            })
        }
    };
    let header_bytes = if layout.straight_alpha {
        V5_HEADER_BYTES
    } else {
        INFO_HEADER_BYTES
    };
    let bits_per_pixel: u16 = if layout.straight_alpha { 32 } else { 24 };
    let row_bytes = row_bytes(frame.width(), bits_per_pixel)?;
    let image_bytes = row_bytes
        .checked_mul(frame.height() as usize)
        .ok_or(EncodeError::SizeOverflow)?;
    let pixel_offset = FILE_HEADER_BYTES + header_bytes;
    let file_bytes = pixel_offset
        .checked_add(image_bytes)
        .ok_or(EncodeError::SizeOverflow)?;
    let size_image = u32::try_from(image_bytes).map_err(|_| EncodeError::SizeOverflow)?;
    let file_size = u32::try_from(file_bytes).map_err(|_| EncodeError::SizeOverflow)?;

    let mut bmp = Vec::with_capacity(file_bytes);
    bmp.extend_from_slice(b"BM");
    push_u32(&mut bmp, file_size);
    push_u16(&mut bmp, 0);
    push_u16(&mut bmp, 0);
    // Where the pixels begin, stated rather than implied. This is why `BI_BITFIELDS` is safe to
    // use here and was not on the clipboard: a DIB consumer has to infer whether a mask triple
    // follows the header, and a file consumer is told.
    push_u32(&mut bmp, u32::try_from(pixel_offset).unwrap_or(u32::MAX));

    push_u32(&mut bmp, u32::try_from(header_bytes).unwrap_or(u32::MAX));
    push_i32(&mut bmp, width);
    // Positive: bottom-up, the orientation every reader handles. A top-down BMP is legal and
    // widely mishandled, and the rows are being copied either way.
    push_i32(&mut bmp, height);
    push_u16(&mut bmp, 1);
    push_u16(&mut bmp, bits_per_pixel);
    push_u32(
        &mut bmp,
        if layout.straight_alpha {
            BI_BITFIELDS
        } else {
            BI_RGB
        },
    );
    push_u32(&mut bmp, size_image);
    // Pixels per metre, horizontal and vertical. Zero means "unstated": a capture's physical size
    // depends on the display it was taken from, and inventing a number would make a 4K screenshot
    // claim a print size nobody asked for.
    push_i32(&mut bmp, 0);
    push_i32(&mut bmp, 0);
    // No palette, so neither the used nor the important colour count says anything.
    push_u32(&mut bmp, 0);
    push_u32(&mut bmp, 0);
    if layout.straight_alpha {
        // The same channel layout the 32-bpp `BI_RGB` default implies — blue in the low byte —
        // stated explicitly, because the alpha mask below is the part that has to be believed.
        push_u32(&mut bmp, 0x00ff_0000);
        push_u32(&mut bmp, 0x0000_ff00);
        push_u32(&mut bmp, 0x0000_00ff);
        push_u32(&mut bmp, 0xff00_0000);
        // Say which colour space these numbers are in, for the reason `png.rs` writes an `sRGB`
        // chunk: an untagged image is interpreted by whatever default the viewer holds. The
        // encoder refuses anything that is not sRGB, so this is a fact rather than an assumption.
        push_u32(&mut bmp, LCS_SRGB);
        // `bV5Endpoints` and the three gamma fields describe a calibrated colour space, and are
        // ignored for `LCS_sRGB`, which already names its primaries and transfer curve.
        for _ in 0..9 {
            push_i32(&mut bmp, 0);
        }
        push_u32(&mut bmp, 0);
        push_u32(&mut bmp, 0);
        push_u32(&mut bmp, 0);
        push_u32(&mut bmp, LCS_GM_IMAGES);
        // No embedded profile, and the reserved field.
        push_u32(&mut bmp, 0);
        push_u32(&mut bmp, 0);
        push_u32(&mut bmp, 0);
    }
    debug_assert_eq!(bmp.len(), pixel_offset, "header size and layout disagree");

    // Rows are converted one at a time rather than materialising a second copy of the image: a 4K
    // frame is 33 MB, and this encoder's output is already the largest of the three.
    let (red, blue) = layout.channels();
    let mut row = vec![0_u8; row_bytes];
    for source_row in (0..frame.height() as usize).rev() {
        let source = layout.source_row(frame, source_row);
        if layout.straight_alpha {
            for (pixel, out) in source.chunks_exact(4).zip(row.chunks_exact_mut(4)) {
                out[0] = pixel[blue];
                out[1] = pixel[1];
                out[2] = pixel[red];
                out[3] = pixel[3];
            }
        } else {
            // `zip` stops at the source's width, so the row's trailing padding keeps the zeroes it
            // was allocated with rather than a fourth channel's worth of a neighbouring pixel.
            for (pixel, out) in source.chunks_exact(4).zip(row.chunks_exact_mut(3)) {
                out[0] = pixel[blue];
                out[1] = pixel[1];
                out[2] = pixel[red];
            }
        }
        bmp.extend_from_slice(&row);
    }
    Ok(bmp)
}

/// How many bytes one row occupies, including the padding that rounds it up to four.
fn row_bytes(width: u32, bits_per_pixel: u16) -> Result<usize, EncodeError> {
    usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(usize::from(bits_per_pixel) / 8))
        .and_then(|bytes| bytes.checked_add(3))
        .map(|bytes| bytes / 4 * 4)
        .ok_or(EncodeError::SizeOverflow)
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FrameAlpha, PixelFormat};
    use crate::test_frames::{frame, frame_in};

    /// Reads a little-endian `u32` from a header offset, the way a decoder would.
    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
    }

    fn u16_at(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("two bytes"))
    }

    fn i32_at(bytes: &[u8], offset: usize) -> i32 {
        i32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
    }

    #[test]
    fn an_opaque_frame_gets_a_plain_header_describing_the_file_it_is_in() {
        // Every field a reader consults before it touches a pixel. A header that disagrees with
        // the bytes after it produces a file that opens as garbage rather than failing to open.
        let pixels = vec![255, 0, 0, 255, 0, 0, 255, 255, 0, 255, 0, 255, 1, 2, 3, 255];
        let encoded = encode_frame(&frame(2, 2, 8, pixels)).expect("encode");

        assert_eq!(&encoded[..2], b"BM");
        assert_eq!(u32_at(&encoded, 2) as usize, encoded.len(), "file size");
        assert_eq!(u32_at(&encoded, 6), 0, "both reserved words");
        assert_eq!(
            u32_at(&encoded, 10),
            54,
            "pixels follow a 40-byte info header"
        );
        assert_eq!(u32_at(&encoded, 14), 40, "info header size");
        assert_eq!(i32_at(&encoded, 18), 2, "width");
        assert_eq!(i32_at(&encoded, 22), 2, "positive height is bottom-up");
        assert_eq!(u16_at(&encoded, 26), 1, "planes");
        assert_eq!(
            u16_at(&encoded, 28),
            24,
            "an opaque frame drops its fourth channel"
        );
        assert_eq!(u32_at(&encoded, 30), BI_RGB);
        assert_eq!(
            u32_at(&encoded, 34) as usize,
            encoded.len() - 54,
            "image size"
        );
        assert_eq!(u32_at(&encoded, 46), 0, "no palette");
    }

    #[test]
    fn rows_are_written_bottom_up_in_bgr_order() {
        // The round-trip that matters: a bottom-up file whose rows were written top-down is a
        // valid BMP of an upside-down screenshot, which no test of the header would catch.
        // Row 0 is blue, row 1 is red, in BGRA source order.
        let pixels = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let encoded = encode_frame(&frame(1, 2, 4, pixels)).expect("encode");

        let pixel_offset = u32_at(&encoded, 10) as usize;
        let rows = &encoded[pixel_offset..];
        // One 3-byte pixel padded to four bytes per row.
        assert_eq!(rows.len(), 8);
        assert_eq!(
            &rows[..3],
            &[0, 0, 255],
            "the bottom row comes first, and it is red"
        );
        assert_eq!(rows[3], 0, "the padding byte is not a pixel");
        assert_eq!(
            &rows[4..7],
            &[255, 0, 0],
            "the top row comes last, and it is blue"
        );
    }

    #[test]
    fn row_padding_rounds_up_to_four_bytes_without_reading_past_the_row() {
        // A three-pixel row is nine bytes of colour in a twelve-byte row. The three bytes that
        // follow belong to nobody, and a reader that trusted them would show a fourth column.
        let pixels = vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255];
        let encoded = encode_frame(&frame(3, 1, 12, pixels)).expect("encode");

        let pixel_offset = u32_at(&encoded, 10) as usize;
        let rows = &encoded[pixel_offset..];
        assert_eq!(rows.len(), 12);
        assert_eq!(&rows[9..], &[0, 0, 0], "padding stays zero");
        assert_eq!(row_bytes(3, 24).expect("row size"), 12);
        assert_eq!(row_bytes(4, 24).expect("row size"), 12, "already aligned");
    }

    #[test]
    fn stride_padding_in_the_source_is_skipped_rather_than_encoded() {
        // Two 1-pixel rows in an 8-byte stride: the trailing bytes of each row are padding a naive
        // encoder would publish as a second column.
        let pixels = vec![
            255, 0, 0, 255, 0xDE, 0xAD, 0xBE, 0xEF, // row 0: blue + padding
            0, 0, 255, 255, 0xDE, 0xAD, 0xBE, 0xEF, // row 1: red + padding
        ];
        let encoded = encode_frame(&frame(1, 2, 8, pixels)).expect("encode");
        let pixel_offset = u32_at(&encoded, 10) as usize;
        assert_eq!(&encoded[pixel_offset..pixel_offset + 3], &[0, 0, 255]);
        assert_eq!(&encoded[pixel_offset + 4..pixel_offset + 7], &[255, 0, 0]);
    }

    #[test]
    fn a_straight_alpha_frame_gets_a_v5_header_whose_mask_says_the_alpha_is_real() {
        // Nothing earlier than a `BITMAPV5HEADER` can say that a fourth channel means anything,
        // so a window capture written under a plain info header is a picture with its shadow
        // baked into whatever the reader assumed.
        let translucent = frame(1, 1, 4, vec![10, 20, 30, 128]).with_alpha(FrameAlpha::Straight);
        let encoded = encode_frame(&translucent).expect("encode");

        assert_eq!(
            u32_at(&encoded, 10),
            138,
            "pixels follow a 124-byte V5 header"
        );
        assert_eq!(u32_at(&encoded, 14), 124, "V5 header size");
        assert_eq!(u16_at(&encoded, 28), 32, "alpha needs a fourth channel");
        assert_eq!(u32_at(&encoded, 30), BI_BITFIELDS);
        assert_eq!(u32_at(&encoded, 54), 0x00ff_0000, "red mask");
        assert_eq!(u32_at(&encoded, 58), 0x0000_ff00, "green mask");
        assert_eq!(u32_at(&encoded, 62), 0x0000_00ff, "blue mask");
        assert_eq!(u32_at(&encoded, 66), 0xff00_0000, "alpha mask");
        assert_eq!(u32_at(&encoded, 70), LCS_SRGB, "colour space");
        assert_eq!(u32_at(&encoded, 122), LCS_GM_IMAGES, "rendering intent");
        assert_eq!(&encoded[138..], &[10, 20, 30, 128], "alpha survives");
        assert_eq!(u32_at(&encoded, 2) as usize, encoded.len(), "file size");
    }

    #[test]
    fn rgba_sources_are_swizzled_into_the_order_a_bmp_reader_expects() {
        // The source order is the backend's, not the format's: a frame that stores red first has
        // to be reordered or every capture comes out with its channels swapped.
        let source = frame_in(PixelFormat::Rgba8Unorm, 1, 1, 4, vec![10, 20, 30, 255]);
        let encoded = encode_frame(&source).expect("encode");
        let pixel_offset = u32_at(&encoded, 10) as usize;
        assert_eq!(&encoded[pixel_offset..pixel_offset + 3], &[30, 20, 10]);
    }
}
