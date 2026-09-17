//! JPEG encoding for captured frames.
//!
//! The lossy choice, and the only one here that changes the picture. A screenshot is mostly flat
//! colour and text, which is the content JPEG handles worst — its ringing shows up along exactly
//! the hard edges a user is usually capturing — so it is neither the default nor a recommendation.
//! It exists because a long capture of a photograph, a map, or a video frame is an order of
//! magnitude smaller this way, and that is a real trade for someone making it knowingly.
//!
//! **JPEG cannot carry alpha.** A window capture with rounded corners and a drop shadow arrives
//! here as straight alpha, and the format has nowhere to put it. Rather than refuse the capture,
//! this composites it over opaque white — the same background a viewer would show through a
//! transparent corner — and says so: [`crate::EncodedCapture::alpha_flattened`] carries the fact
//! to the destination, which logs it. ADR 0008's standard is that a user can see what their
//! capture is doing, and this is a format-inherent consequence of a choice they made, not a
//! conversion Captastic performs behind them.

use jpeg_encoder::{ColorType, Encoder};

use crate::encode::{EncodeError, OutputFormat, SourceLayout};
use crate::frame::CpuFrame;

/// JPEG states a frame's dimensions in sixteen bits, so this is the widest picture the container
/// can describe at all — well past any display Windows enumerates, and checked rather than assumed
/// because the alternative is a valid file of the wrong size.
const MAX_SIDE: u32 = u16::MAX as u32;

/// Encodes a frame as a baseline JPEG at `quality` (1..=100).
///
/// Straight-alpha frames are composited over opaque white on the way in; see the module comment.
pub fn encode_frame(frame: &CpuFrame, quality: u8) -> Result<Vec<u8>, EncodeError> {
    let layout = SourceLayout::inspect(frame, OutputFormat::Jpeg)?;
    if frame.width() > MAX_SIDE || frame.height() > MAX_SIDE {
        return Err(EncodeError::DimensionsTooLarge {
            format: OutputFormat::Jpeg,
            width: frame.width(),
            height: frame.height(),
            limit: MAX_SIDE,
        });
    }
    let rgb = rgb_bytes(frame, &layout)?;
    // Configuration validates the range, so a value outside it means a caller built `EncodeOptions`
    // by hand. Clamping rather than failing keeps a programming mistake from costing a capture, and
    // both ends of the range are still a picture.
    let quality = quality.clamp(1, 100);
    // Screen content at these qualities lands well under a tenth of its pixels, and a wrong guess
    // only costs a realloc.
    let mut bytes = Vec::with_capacity((rgb.len() / 8).max(1024));
    let encoder = Encoder::new(&mut bytes, quality);
    encoder
        .encode(
            &rgb,
            frame.width() as u16,
            frame.height() as u16,
            ColorType::Rgb,
        )
        .map_err(|error| EncodeError::Writer {
            format: OutputFormat::Jpeg,
            message: error.to_string(),
        })?;
    Ok(bytes)
}

/// Converts a frame to the tight, top-down, three-channel buffer the encoder reads.
///
/// Materialized whole rather than streamed a row at a time, unlike the other two encoders: this
/// one's API takes the image in a single call. Three bytes per pixel rather than four keeps that
/// copy to three quarters of the frame it came from.
fn rgb_bytes(frame: &CpuFrame, layout: &SourceLayout) -> Result<Vec<u8>, EncodeError> {
    let row_bytes = usize::try_from(frame.width())
        .ok()
        .and_then(|width| width.checked_mul(3))
        .ok_or(EncodeError::SizeOverflow)?;
    let total = row_bytes
        .checked_mul(frame.height() as usize)
        .ok_or(EncodeError::SizeOverflow)?;
    let mut rgb = vec![0_u8; total];
    let (red, blue) = layout.channels();
    for row in 0..frame.height() as usize {
        let source = layout.source_row(frame, row);
        let destination = &mut rgb[row * row_bytes..(row + 1) * row_bytes];
        if layout.straight_alpha {
            for (pixel, out) in source.chunks_exact(4).zip(destination.chunks_exact_mut(3)) {
                let alpha = u32::from(pixel[3]);
                out[0] = over_white(pixel[red], alpha);
                out[1] = over_white(pixel[1], alpha);
                out[2] = over_white(pixel[blue], alpha);
            }
        } else {
            for (pixel, out) in source.chunks_exact(4).zip(destination.chunks_exact_mut(3)) {
                out[0] = pixel[red];
                out[1] = pixel[1];
                out[2] = pixel[blue];
            }
        }
    }
    Ok(rgb)
}

/// Composites one straight-alpha sample over opaque white, rounding rather than truncating.
///
/// White rather than black: a transparent window corner is showing whatever is behind it, and a
/// black halo around a rounded corner reads as a rendering fault where a white one reads as paper.
fn over_white(sample: u8, alpha: u32) -> u8 {
    let scaled = u32::from(sample) * alpha + 255 * (255 - alpha);
    ((scaled + 127) / 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FrameAlpha, PixelFormat};
    use crate::test_frames::{frame, frame_in};

    /// A frame that does not compress away, so a size comparison measures quality rather than
    /// luck. Deterministic, because a flaky size assertion is worse than no assertion.
    fn noisy_frame(side: u32) -> CpuFrame {
        let mut pixels = Vec::with_capacity((side * side * 4) as usize);
        let mut state = 0x1234_5678_u32;
        for _ in 0..side * side * 4 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            pixels.push((state >> 16) as u8);
        }
        frame(side, side, side * 4, pixels)
    }

    #[test]
    fn the_output_is_a_complete_jpeg_stream() {
        // No decoder is available to this crate, so the assertion is the one a decoder makes
        // first: a stream that begins with SOI and ends with EOI was finished rather than
        // truncated by a failed write.
        let encoded = encode_frame(&noisy_frame(16), 90).expect("encode");
        assert_eq!(&encoded[..2], &[0xFF, 0xD8], "SOI");
        assert_eq!(&encoded[encoded.len() - 2..], &[0xFF, 0xD9], "EOI");
    }

    #[test]
    fn a_lower_quality_costs_fewer_bytes() {
        // The whole reason this format is offered. If quality did not reach the encoder every
        // setting would produce the same file, and the configuration would be decorative.
        let source = noisy_frame(64);
        let high = encode_frame(&source, 95).expect("encode high");
        let low = encode_frame(&source, 20).expect("encode low");
        assert!(
            low.len() < high.len(),
            "quality 20 produced {} bytes and quality 95 produced {}",
            low.len(),
            high.len()
        );
    }

    #[test]
    fn a_transparent_pixel_is_composited_over_opaque_white() {
        // The documented consequence of choosing JPEG for a window capture, pinned as a fact: a
        // fully transparent pixel becomes white, a fully opaque one is untouched, and half alpha
        // lands between them. Asserted on the buffer handed to the encoder rather than on decoded
        // output, because JPEG is lossy and this is about what was handed over.
        let pixels = vec![
            0, 0, 0, 0, // transparent black: becomes white
            10, 20, 30, 255, // opaque, in BGRA source order: unchanged
            0, 0, 0, 128, // half-transparent black: 128/255 of the way to white
            255, 255, 255, 0, // transparent white: still white
        ];
        let translucent = frame(2, 2, 8, pixels).with_alpha(FrameAlpha::Straight);
        let layout = SourceLayout::inspect(&translucent, OutputFormat::Jpeg).expect("layout");
        let rgb = rgb_bytes(&translucent, &layout).expect("convert");

        assert_eq!(&rgb[0..3], &[255, 255, 255], "alpha 0 is white");
        assert_eq!(&rgb[3..6], &[30, 20, 10], "an opaque pixel is untouched");
        assert_eq!(
            &rgb[6..9],
            &[127, 127, 127],
            "alpha 128 is 255 - 128, exactly"
        );
        assert_eq!(&rgb[9..12], &[255, 255, 255], "white stays white");
    }

    #[test]
    fn an_opaque_frame_is_converted_without_touching_its_samples() {
        let pixels = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let source = frame(2, 1, 8, pixels);
        let layout = SourceLayout::inspect(&source, OutputFormat::Jpeg).expect("layout");
        assert_eq!(
            rgb_bytes(&source, &layout).expect("convert"),
            [0, 0, 255, 255, 0, 0]
        );
    }

    #[test]
    fn rgba_sources_are_not_swizzled() {
        let source = frame_in(PixelFormat::Rgba8Unorm, 1, 1, 4, vec![10, 20, 30, 255]);
        let layout = SourceLayout::inspect(&source, OutputFormat::Jpeg).expect("layout");
        assert_eq!(rgb_bytes(&source, &layout).expect("convert"), [10, 20, 30]);
    }

    #[test]
    fn a_frame_wider_than_the_container_is_refused_rather_than_wrapped() {
        // JPEG states its dimensions in sixteen bits. A 65536-pixel-wide frame that reached the
        // encoder unchecked would be described as a zero-width one.
        let too_wide = frame(65_536, 1, 65_536 * 4, vec![0; 65_536 * 4]);
        let error = encode_frame(&too_wide, 90).expect_err("an oversized frame is refused");
        assert!(error.to_string().contains("65535"), "{error}");
    }
}
