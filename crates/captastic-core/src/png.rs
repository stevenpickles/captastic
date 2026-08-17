//! PNG encoding for captured frames.
//!
//! Captastic's clipboard publisher carries its own hand-rolled encoder that emits *stored*
//! (uncompressed) DEFLATE — a defensible trade when the destination is an in-memory clipboard
//! handle and the only cost is RAM. On disk it is not a trade at all: a 4K capture lands as a
//! ~33 MB "PNG". This encoder is the one file output uses, and it compresses.

use std::io::Write;

use thiserror::Error;

use crate::frame::{ColorSpace, CpuFrame, FrameAlpha, FrameOrigin, PixelEncoding, PixelFormat};

/// How hard the DEFLATE backend works on a frame.
///
/// The distinction exists because Captastic's two destinations have opposite priorities: a
/// clipboard publish sits in the hotkey path and is measured in milliseconds a user can feel,
/// while a file write happens on a worker thread where bytes on disk matter more than
/// microseconds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PngEffort {
    /// Favor encode latency. Still real DEFLATE, unlike the stored-block clipboard encoder.
    Fast,
    /// Favor output size. The default for anything written to disk.
    #[default]
    Compact,
}

impl PngEffort {
    fn compression(self) -> png::Compression {
        match self {
            Self::Fast => png::Compression::Fast,
            Self::Compact => png::Compression::Balanced,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PngError {
    #[error("PNG encoding requires top-left pixels, not {origin:?}")]
    UnsupportedOrigin { origin: FrameOrigin },
    /// Rejected rather than converted. Narrowing a wide-gamut or high-dynamic-range pixel to eight
    /// bits is tone mapping, not a cast: done naively it clips every highlight the format existed
    /// to carry, and silently. Until Captastic has a tone-mapping stage worth naming, refusing is
    /// the honest answer.
    #[error("PNG encoding requires 8-bit pixels, and {format:?} is not")]
    UnsupportedFormat { format: PixelFormat },
    /// Also rejected rather than converted, and for the same reason: mapping scRGB into sRGB is a
    /// tone-mapping decision, and making it silently would publish a washed-out or clipped image
    /// that looks like a capture bug rather than a missing feature.
    #[error("PNG encoding cannot describe {color_space:?} samples")]
    UnsupportedColorSpace { color_space: ColorSpace },
    #[error("frame dimensions must be non-zero")]
    EmptyDimensions,
    #[error("stride {stride} is smaller than the minimum row size {minimum}")]
    InvalidStride { stride: u32, minimum: u32 },
    #[error("frame size calculation overflowed")]
    SizeOverflow,
    #[error("pixel buffer contains {actual} bytes but requires at least {required}")]
    BufferTooShort { actual: usize, required: usize },
    /// The `png` writer rejected the stream. Carried as text because `png::EncodingError` is
    /// neither `Clone` nor `PartialEq`, and nothing downstream can act on the distinction.
    #[error("PNG writer failed: {message}")]
    Writer { message: String },
}

/// Encodes a frame as a PNG using the caller's latency-versus-size preference.
///
/// Opaque frames are written as 8-bit RGB rather than RGBA. A desktop capture is opaque in the
/// overwhelming majority of cases, and dropping a channel that carries no information takes a
/// quarter off the pixel data before it ever reaches the compressor.
pub fn encode_frame(frame: &CpuFrame, effort: PngEffort) -> Result<Vec<u8>, PngError> {
    let layout = FrameLayout::inspect(frame)?;
    // A rough starting point: real screen content compresses well, and over-reserving a 4K frame
    // wastes more memory than the reallocations save.
    let mut png = Vec::with_capacity(layout.encoded_capacity_hint());

    let mut encoder = png::Encoder::new(&mut png, frame.width(), frame.height());
    encoder.set_color(layout.color_type);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(effort.compression());
    let mut writer = encoder.write_header().map_err(writer_error)?;
    {
        // Borrowed rather than owned so the destination can stay a plain `&mut Vec`; the stream
        // has to be finished and dropped before the writer can emit IEND.
        let mut stream = writer.stream_writer().map_err(writer_error)?;
        // Rows are converted one at a time rather than materialising a second copy of the image:
        // a 4K frame is 33 MB, and the point of this path is to keep file output off the capture
        // thread's memory footprint as well as its clock.
        let mut row = vec![0_u8; layout.output_row_bytes];
        for source_row in 0..frame.height() as usize {
            let start = source_row * frame.stride_bytes() as usize;
            let source = &frame.pixels()[start..start + layout.source_row_bytes];
            layout.convert_row(source, &mut row);
            stream.write_all(&row).map_err(|error| PngError::Writer {
                message: error.to_string(),
            })?;
        }
        stream.finish().map_err(writer_error)?;
    }
    writer.finish().map_err(writer_error)?;
    Ok(png)
}

fn writer_error(error: png::EncodingError) -> PngError {
    PngError::Writer {
        message: error.to_string(),
    }
}

/// The validated shape of a frame, resolved once so the per-row loop stays branch-light.
struct FrameLayout {
    color_type: png::ColorType,
    /// Bytes read from each source row; the rest of the stride is padding.
    source_row_bytes: usize,
    /// Bytes written to each PNG row: 3 per pixel for opaque frames, 4 otherwise.
    output_row_bytes: usize,
    height: usize,
    /// Whether the source stores blue first and needs swapping with red.
    swizzle: bool,
    keep_alpha: bool,
}

impl FrameLayout {
    fn inspect(frame: &CpuFrame) -> Result<Self, PngError> {
        if frame.origin != FrameOrigin::TopLeft {
            return Err(PngError::UnsupportedOrigin {
                origin: frame.origin,
            });
        }
        if frame.width() == 0 || frame.height() == 0 {
            return Err(PngError::EmptyDimensions);
        }
        // Every path below reads four bytes per pixel and writes eight bits per channel, so the
        // encoding is settled before any of the arithmetic that assumes it.
        let swizzle = match frame.format().encoding() {
            PixelEncoding::EightBitRgba { blue_first } => blue_first,
            PixelEncoding::HalfFloatRgba => {
                return Err(PngError::UnsupportedFormat {
                    format: frame.format(),
                })
            }
        };
        // An 8-bit PNG carries no way to say "these samples are linear and 1.0 is not the
        // maximum", short of an embedded ICC profile. Writing one anyway would produce a file that
        // every viewer reads as sRGB, which is a picture of the right pixels interpreted wrongly.
        match frame.color_space {
            ColorSpace::Srgb | ColorSpace::Unknown => {}
            ColorSpace::ScRgb => {
                return Err(PngError::UnsupportedColorSpace {
                    color_space: frame.color_space,
                })
            }
        }
        let source_row_bytes = usize::try_from(frame.width())
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(PngError::SizeOverflow)?;
        let minimum = u32::try_from(source_row_bytes).map_err(|_| PngError::SizeOverflow)?;
        if frame.stride_bytes() < minimum {
            return Err(PngError::InvalidStride {
                stride: frame.stride_bytes(),
                minimum,
            });
        }
        // The last row is only required to hold its pixels, not a full stride of padding, but
        // `CpuFrame::new` already validates against the full stride and every producer allocates
        // that way. Requiring it here keeps the row loop's slicing unconditional.
        let required = (frame.stride_bytes() as usize)
            .checked_mul(frame.height() as usize)
            .ok_or(PngError::SizeOverflow)?;
        if frame.pixels().len() < required {
            return Err(PngError::BufferTooShort {
                actual: frame.pixels().len(),
                required,
            });
        }
        let keep_alpha = frame.alpha() == FrameAlpha::Straight;
        let output_row_bytes = usize::try_from(frame.width())
            .ok()
            .and_then(|width| width.checked_mul(if keep_alpha { 4 } else { 3 }))
            .ok_or(PngError::SizeOverflow)?;
        Ok(Self {
            color_type: if keep_alpha {
                png::ColorType::Rgba
            } else {
                png::ColorType::Rgb
            },
            source_row_bytes,
            output_row_bytes,
            height: frame.height() as usize,
            swizzle,
            keep_alpha,
        })
    }

    fn convert_row(&self, source: &[u8], destination: &mut [u8]) {
        let (red, blue) = if self.swizzle { (2, 0) } else { (0, 2) };
        if self.keep_alpha {
            for (pixel, out) in source.chunks_exact(4).zip(destination.chunks_exact_mut(4)) {
                out[0] = pixel[red];
                out[1] = pixel[1];
                out[2] = pixel[blue];
                out[3] = pixel[3];
            }
        } else {
            for (pixel, out) in source.chunks_exact(4).zip(destination.chunks_exact_mut(3)) {
                out[0] = pixel[red];
                out[1] = pixel[1];
                out[2] = pixel[blue];
            }
        }
    }

    fn encoded_capacity_hint(&self) -> usize {
        // Screen content routinely compresses past 10:1, and a wrong guess only costs a realloc.
        (self.output_row_bytes.saturating_mul(self.height) / 8).max(1024)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::capture::{CaptureId, CaptureMode};
    use crate::display::{DisplayId, Rect};
    use crate::frame::{FrameMetadata, TimingProvenance};
    use crate::FrameError;

    fn frame(width: u32, height: u32, stride_bytes: u32, pixels: Vec<u8>) -> CpuFrame {
        frame_in(PixelFormat::Bgra8Unorm, width, height, stride_bytes, pixels)
    }

    fn frame_in(
        format: PixelFormat,
        width: u32,
        height: u32,
        stride_bytes: u32,
        pixels: Vec<u8>,
    ) -> CpuFrame {
        build_frame_in(format, width, height, stride_bytes, pixels).expect("test frame is valid")
    }

    fn build_frame(
        width: u32,
        height: u32,
        stride_bytes: u32,
        pixels: Vec<u8>,
    ) -> Result<CpuFrame, FrameError> {
        build_frame_in(PixelFormat::Bgra8Unorm, width, height, stride_bytes, pixels)
    }

    fn build_frame_in(
        format: PixelFormat,
        width: u32,
        height: u32,
        stride_bytes: u32,
        pixels: Vec<u8>,
    ) -> Result<CpuFrame, FrameError> {
        let metadata = FrameMetadata {
            capture_id: CaptureId(1),
            backend: "test".to_owned(),
            display_id: DisplayId::primary(),
            source_rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            rotation_degrees: 0,
            capture_mode: CaptureMode::Latest { max_age_ms: None },
            presentation_offset_ns: None,
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: 0,
            cpu_ready_offset_ns: Some(0),
            frame_age_ns: Some(0),
            verified_current_offset_ns: None,
            frame_generation: Some(1),
            copy_count: 0,
            pool_slot: None,
            cursor: None,
        };
        CpuFrame::new(
            Arc::from(pixels),
            width,
            height,
            stride_bytes,
            format,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
    }

    /// Decodes a PNG back to interleaved 8-bit samples plus its color type.
    fn decode(png_bytes: &[u8]) -> (png::ColorType, u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().expect("valid PNG header");
        let mut buffer = vec![0; reader.output_buffer_size().expect("bounded buffer")];
        let info = reader.next_frame(&mut buffer).expect("valid PNG data");
        buffer.truncate(info.buffer_size());
        (info.color_type, info.width, info.height, buffer)
    }

    #[test]
    fn opaque_bgra_is_published_as_rgb_in_source_order() {
        // Two pixels: one pure blue, one pure red, in BGRA byte order.
        let pixels = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let encoded = encode_frame(&frame(2, 1, 8, pixels), PngEffort::Compact).expect("encode");

        let (color_type, width, height, samples) = decode(&encoded);
        assert_eq!(color_type, png::ColorType::Rgb);
        assert_eq!((width, height), (2, 1));
        assert_eq!(samples, [0, 0, 255, 255, 0, 0]);
    }

    #[test]
    fn straight_alpha_frames_keep_their_alpha_channel() {
        let pixels = vec![10, 20, 30, 128];
        let translucent = frame(1, 1, 4, pixels).with_alpha(FrameAlpha::Straight);
        let encoded = encode_frame(&translucent, PngEffort::Compact).expect("encode");

        let (color_type, _, _, samples) = decode(&encoded);
        assert_eq!(color_type, png::ColorType::Rgba);
        assert_eq!(samples, [30, 20, 10, 128]);
    }

    #[test]
    fn rgba_sources_are_not_swizzled() {
        let source = frame_in(PixelFormat::Rgba8Unorm, 1, 1, 4, vec![10, 20, 30, 255]);
        let encoded = encode_frame(&source, PngEffort::Compact).expect("encode");

        let (_, _, _, samples) = decode(&encoded);
        assert_eq!(samples, [10, 20, 30]);
    }

    #[test]
    fn row_padding_is_skipped_rather_than_encoded() {
        // Two 1-pixel rows in an 8-byte stride: the trailing 4 bytes of each row are padding that
        // a naive encoder would publish as a second column.
        let pixels = vec![
            255, 0, 0, 255, 0xDE, 0xAD, 0xBE, 0xEF, // row 0: blue + padding
            0, 0, 255, 255, 0xDE, 0xAD, 0xBE, 0xEF, // row 1: red + padding
        ];
        let encoded = encode_frame(&frame(1, 2, 8, pixels), PngEffort::Compact).expect("encode");

        let (_, width, height, samples) = decode(&encoded);
        assert_eq!((width, height), (1, 2));
        assert_eq!(samples, [0, 0, 255, 255, 0, 0]);
    }

    #[test]
    fn a_compressible_frame_is_far_smaller_than_its_pixels() {
        // The regression this whole module exists for: a stored-DEFLATE encoder emits slightly
        // *more* than the raw pixel count, so a flat frame that fails this assertion means the
        // compressor is not running.
        let pixels = vec![0x40; 256 * 256 * 4];
        let encoded =
            encode_frame(&frame(256, 256, 256 * 4, pixels), PngEffort::Compact).expect("encode");
        assert!(
            encoded.len() < 256 * 256 / 4,
            "a uniform frame encoded to {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn both_efforts_round_trip_the_same_pixels() {
        // Effort may change the byte count; it must never change the image.
        let pixels: Vec<u8> = (0..64 * 64 * 4).map(|byte| (byte % 251) as u8).collect();
        let source = frame(64, 64, 64 * 4, pixels);
        let compact = encode_frame(&source, PngEffort::Compact).expect("encode compact");
        let fast = encode_frame(&source, PngEffort::Fast).expect("encode fast");
        assert_eq!(decode(&compact).3, decode(&fast).3);
    }

    /// These two used to build a valid frame and then corrupt it - a narrower stride, a shorter
    /// buffer - to reach the encoder's own layout checks. Neither is expressible any more, which
    /// was the point of privatizing the fields: the encoder cannot be handed a frame whose stride
    /// and buffer disagree, because no such `CpuFrame` can be built.
    ///
    /// The rejection is now where the caller actually meets it.
    /// The refusal exists because the alternative is worse than an error: an unrecognized format
    /// would otherwise take the RGBA branch and be encoded as though its bytes meant something
    /// else, producing a valid PNG of the wrong picture.
    #[test]
    fn half_float_frames_are_refused_rather_than_encoded_as_eight_bit() {
        let frame = frame_in(PixelFormat::Rgba16Float, 1, 1, 8, vec![0; 8]);
        assert_eq!(
            encode_frame(&frame, PngEffort::Compact),
            Err(PngError::UnsupportedFormat {
                format: PixelFormat::Rgba16Float
            })
        );
    }

    #[test]
    fn scrgb_samples_are_refused_rather_than_published_as_srgb() {
        let mut frame = frame(1, 1, 4, vec![10, 20, 30, 255]);
        frame.color_space = ColorSpace::ScRgb;
        assert_eq!(
            encode_frame(&frame, PngEffort::Compact),
            Err(PngError::UnsupportedColorSpace {
                color_space: ColorSpace::ScRgb
            })
        );
    }

    #[test]
    fn a_stride_narrower_than_the_row_cannot_become_a_frame() {
        assert_eq!(
            build_frame(2, 1, 7, vec![0; 8]).err(),
            Some(FrameError::InvalidStride {
                stride: 7,
                minimum: 8
            })
        );
    }

    #[test]
    fn a_truncated_pixel_buffer_cannot_become_a_frame() {
        assert_eq!(
            build_frame(2, 2, 8, vec![0; 12]).err(),
            Some(FrameError::BufferTooShort {
                actual: 12,
                required: 16
            })
        );
    }
}
