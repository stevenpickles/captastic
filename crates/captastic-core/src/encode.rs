//! What a capture is encoded as, and the checks every encoder makes before it starts.
//!
//! Captastic writes one picture through three encoders. The differences between them are real —
//! PNG compresses losslessly and carries alpha, JPEG carries neither, BMP carries alpha and
//! compresses nothing — but the questions they ask of a frame first are identical: are these
//! eight-bit pixels, are they sRGB, are they top-left, and is the buffer as long as its stride
//! claims. [`SourceLayout`] asks them once so three encoders cannot answer them three ways.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::frame::{ColorSpace, CpuFrame, FrameAlpha, FrameOrigin, PixelEncoding, PixelFormat};

/// The image format a capture is written in.
///
/// Serialized in lowercase because this is what a user types into `output.format`, and reported in
/// the same spelling in JSON so what they wrote and what they read back are the same word.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Lossless, alpha-carrying, and the default: a screenshot is usually kept to be read.
    #[default]
    Png,
    /// Uncompressed, alpha-carrying, and universally readable by Windows tooling.
    Bmp,
}

impl OutputFormat {
    pub const ALL: [Self; 2] = [Self::Png, Self::Bmp];

    /// The spelling used in configuration and in JSON output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Bmp => "bmp",
        }
    }

    /// The format's name as it appears in prose and in error messages.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Bmp => "BMP",
        }
    }

    /// The extension a capture in this format is written with.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Bmp => "bmp",
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The knobs that only matter to some of the formats.
///
/// Passed whole rather than per-format so a caller threads one value through and each encoder
/// picks what applies to it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EncodeOptions {
    pub png_effort: crate::png::PngEffort,
}

/// One encoded capture, and what it has to be called.
pub struct EncodedCapture {
    pub bytes: Vec<u8>,
    /// The extension the bytes must be written under, so callers never map format to suffix again.
    pub extension: &'static str,
}

/// Reports the size rather than the bytes: a debug line that prints a 33 MB image is not a debug
/// line, and the length is the only part of it anyone reads.
impl fmt::Debug for EncodedCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EncodedCapture")
            .field("bytes", &self.bytes.len())
            .field("extension", &self.extension)
            .finish()
    }
}

/// Why a frame could not be encoded.
///
/// Shared by the BMP and JPEG encoders, and wrapping the PNG encoder's own error rather than
/// restating it: `PngError` is public API the clipboard publisher already matches on.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum EncodeError {
    #[error(transparent)]
    Png(#[from] crate::png::PngError),
    #[error("{} encoding requires top-left pixels, not {origin:?}", .format.label())]
    UnsupportedOrigin {
        format: OutputFormat,
        origin: FrameOrigin,
    },
    /// Rejected rather than converted, for the reason `png.rs` gives: narrowing a wide-gamut or
    /// high-dynamic-range pixel to eight bits is tone mapping, not a cast, and doing it naively
    /// clips every highlight the format existed to carry.
    #[error("{} encoding requires 8-bit pixels, and {pixel_format:?} is not", .format.label())]
    UnsupportedFormat {
        format: OutputFormat,
        pixel_format: PixelFormat,
    },
    /// Also rejected rather than converted: mapping scRGB into sRGB is a tone-mapping decision,
    /// and making it silently publishes a washed-out image that looks like a capture bug.
    #[error("{} encoding cannot describe {color_space:?} samples", .format.label())]
    UnsupportedColorSpace {
        format: OutputFormat,
        color_space: ColorSpace,
    },
    #[error("frame dimensions must be non-zero")]
    EmptyDimensions,
    #[error("stride {stride} is smaller than the minimum row size {minimum}")]
    InvalidStride { stride: u32, minimum: u32 },
    #[error("frame size calculation overflowed")]
    SizeOverflow,
    #[error("pixel buffer contains {actual} bytes but requires at least {required}")]
    BufferTooShort { actual: usize, required: usize },
    /// A frame larger than the container can describe. A capture that exceeds the limit is
    /// refused rather than wrapped into a picture of the wrong size.
    #[error("{} cannot describe a {width}x{height} image; the limit is {limit} per side", .format.label())]
    DimensionsTooLarge {
        format: OutputFormat,
        width: u32,
        height: u32,
        limit: u32,
    },
    /// The encoder backend rejected the stream. Carried as text because the underlying errors are
    /// neither `Clone` nor `PartialEq`, and nothing downstream can act on the distinction.
    #[error("{} writer failed: {message}", .format.label())]
    Writer {
        format: OutputFormat,
        message: String,
    },
}

/// Encodes a capture in the requested format.
///
/// The one entry point every destination uses, so adding a format is a match arm here rather than
/// a branch at each call site.
pub fn encode_capture(
    frame: &CpuFrame,
    format: OutputFormat,
    options: &EncodeOptions,
) -> Result<EncodedCapture, EncodeError> {
    let bytes = match format {
        OutputFormat::Png => crate::png::encode_frame(frame, options.png_effort)?,
        OutputFormat::Bmp => crate::bmp::encode_frame(frame)?,
    };
    Ok(EncodedCapture {
        bytes,
        extension: format.extension(),
    })
}

/// The validated shape of a frame, resolved once so the per-row loops stay branch-light.
///
/// Mirrors `png.rs`'s `FrameLayout::inspect` deliberately: the same frames are refused by every
/// encoder, with the same wording, so a user who switches format never discovers a different set
/// of rules.
pub(crate) struct SourceLayout {
    /// Bytes read from each source row; the rest of the stride is padding.
    pub(crate) source_row_bytes: usize,
    /// Whether the source stores blue first and needs swapping with red.
    pub(crate) swizzle: bool,
    /// Whether the frame's fourth channel means anything.
    pub(crate) straight_alpha: bool,
}

impl SourceLayout {
    pub(crate) fn inspect(frame: &CpuFrame, format: OutputFormat) -> Result<Self, EncodeError> {
        if frame.origin != FrameOrigin::TopLeft {
            return Err(EncodeError::UnsupportedOrigin {
                format,
                origin: frame.origin,
            });
        }
        if frame.width() == 0 || frame.height() == 0 {
            return Err(EncodeError::EmptyDimensions);
        }
        // Every path below reads four bytes per pixel and writes eight bits per channel, so the
        // encoding is settled before any of the arithmetic that assumes it.
        let swizzle = match frame.format().encoding() {
            PixelEncoding::EightBitRgba { blue_first } => blue_first,
            PixelEncoding::HalfFloatRgba => {
                return Err(EncodeError::UnsupportedFormat {
                    format,
                    pixel_format: frame.format(),
                })
            }
        };
        match frame.color_space {
            ColorSpace::Srgb | ColorSpace::Unknown => {}
            ColorSpace::ScRgb => {
                return Err(EncodeError::UnsupportedColorSpace {
                    format,
                    color_space: frame.color_space,
                })
            }
        }
        let source_row_bytes = usize::try_from(frame.width())
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(EncodeError::SizeOverflow)?;
        let minimum = u32::try_from(source_row_bytes).map_err(|_| EncodeError::SizeOverflow)?;
        if frame.stride_bytes() < minimum {
            return Err(EncodeError::InvalidStride {
                stride: frame.stride_bytes(),
                minimum,
            });
        }
        let required = (frame.stride_bytes() as usize)
            .checked_mul(frame.height() as usize)
            .ok_or(EncodeError::SizeOverflow)?;
        if frame.pixels().len() < required {
            return Err(EncodeError::BufferTooShort {
                actual: frame.pixels().len(),
                required,
            });
        }
        Ok(Self {
            source_row_bytes,
            swizzle,
            straight_alpha: frame.alpha() == FrameAlpha::Straight,
        })
    }

    /// The pixels of one source row, without the stride padding that follows them.
    pub(crate) fn source_row<'frame>(&self, frame: &'frame CpuFrame, row: usize) -> &'frame [u8] {
        let start = row * frame.stride_bytes() as usize;
        &frame.pixels()[start..start + self.source_row_bytes]
    }

    /// Where red and blue sit in a source pixel, so the row loops index rather than branch.
    pub(crate) const fn channels(&self) -> (usize, usize) {
        if self.swizzle {
            (2, 0)
        } else {
            (0, 2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_frames::{frame, frame_in};

    #[test]
    fn a_format_names_itself_the_same_way_configuration_does() {
        // What a user writes in `output.format` is what JSON and logs report back, so a script can
        // compare the two without a translation table. The serialized spelling is pinned where the
        // configuration is parsed; this pins the spelling everything else prints.
        for format in OutputFormat::ALL {
            assert_eq!(format.to_string(), format.as_str());
        }
        assert_eq!(OutputFormat::default(), OutputFormat::Png);
        assert_eq!(OutputFormat::ALL.map(OutputFormat::as_str), ["png", "bmp"]);
    }

    #[test]
    fn every_format_refuses_the_same_frames_by_name() {
        // A user who changes `output.format` should not discover a different set of rules. Both
        // refusals name what was refused, because "encoding failed" is not something a user can
        // act on.
        for format in OutputFormat::ALL {
            let half_float = frame_in(PixelFormat::Rgba16Float, 1, 1, 8, vec![0; 8]);
            let error = encode_capture(&half_float, format, &EncodeOptions::default())
                .expect_err("half-float pixels are refused");
            assert!(
                error.to_string().contains("Rgba16Float"),
                "{format}: {error}"
            );

            let mut wide_gamut = frame(1, 1, 4, vec![10, 20, 30, 255]);
            wide_gamut.color_space = ColorSpace::ScRgb;
            let error = encode_capture(&wide_gamut, format, &EncodeOptions::default())
                .expect_err("scRGB samples are refused");
            assert!(error.to_string().contains("ScRgb"), "{format}: {error}");
        }
    }

    #[test]
    fn each_format_produces_its_own_bytes_under_its_own_extension() {
        let pixels = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let source = frame(2, 1, 8, pixels);
        for format in OutputFormat::ALL {
            let encoded = encode_capture(&source, format, &EncodeOptions::default())
                .unwrap_or_else(|error| panic!("{format} encode: {error}"));
            assert_eq!(encoded.extension, format.extension());
            assert!(!encoded.bytes.is_empty(), "{format} produced no bytes");
        }
    }
}
