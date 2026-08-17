use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{CaptureId, CaptureMode, DisplayId, FrameError, Rect};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    Bgra8Unorm,
    Rgba8Unorm,
    /// Four IEEE half-precision channels, red first: `DXGI_FORMAT_R16G16B16A16_FLOAT`.
    ///
    /// What a Windows desktop composes into when HDR is enabled on any attached display. Channel
    /// values are not confined to `0.0..=1.0`: in scRGB, 1.0 is the sRGB white point and brighter
    /// pixels exceed it, which is the whole reason the format is wider.
    Rgba16Float,
}

impl PixelFormat {
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Bgra8Unorm | Self::Rgba8Unorm => 4,
            Self::Rgba16Float => 8,
        }
    }

    /// How one pixel is actually laid out in memory.
    ///
    /// Consumers branch on this rather than on the format, and the difference matters at the point
    /// a new format is added. A second name for a four-byte 8-bit pixel needs no decision from a
    /// sink that already handles one; a pixel of a different width or a different numeric type
    /// needs a decision from every sink there is. Matching on the encoding is what makes the
    /// compiler ask only the second question.
    pub const fn encoding(self) -> PixelEncoding {
        match self {
            Self::Bgra8Unorm => PixelEncoding::EightBitRgba { blue_first: true },
            Self::Rgba8Unorm => PixelEncoding::EightBitRgba { blue_first: false },
            Self::Rgba16Float => PixelEncoding::HalfFloatRgba,
        }
    }
}

/// The memory layout of a single pixel, as distinct from the name of its format.
///
/// Deliberately not `#[non_exhaustive]`: adding a variant should stop every sink in the workspace
/// from compiling, which is the entire reason this type exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelEncoding {
    /// Four bytes: three 8-bit unsigned channels then 8 bits of alpha.
    EightBitRgba { blue_first: bool },
    /// Eight bytes: three 16-bit half-float channels, red first, then 16 bits of alpha.
    ///
    /// No sink consumes these yet. Every one of them refuses, which is the point of the variant
    /// existing before there is anything to produce it: the refusals are written and tested now,
    /// while the alternative to writing them is only a compile error, rather than later, when the
    /// alternative is a silently wrong image.
    HalfFloatRgba,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameOrigin {
    TopLeft,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorSpace {
    Srgb,
    /// Linear, sRGB primaries, with 1.0 at the sRGB white point and highlights above it.
    ///
    /// The colour space of a Windows HDR desktop, and inseparable in practice from
    /// [`PixelFormat::Rgba16Float`] - eight-bit channels have nowhere to put the values that
    /// distinguish it.
    ScRgb,
    Unknown,
}

/// Describes whether the fourth byte of each 32-bit pixel carries useful alpha.
///
/// Capture backends default to `Opaque` because several native APIs leave the alpha byte
/// undefined. A producer must opt in to `Straight` only after constructing a valid alpha plane.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameAlpha {
    Opaque,
    Straight,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingProvenance {
    OsPresentationTime,
    ArrivalTime,
    Synthetic,
    Unavailable,
}

/// Why a frame carries no cursor, when one was asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorAbsence {
    /// The pointer was not on the captured display, or was hidden.
    NotVisible,
    /// The source cannot produce a pointer at all — every window-capture backend.
    SourceCannotCompose,
    /// A live selection: the pointer at confirmation time was on Captastic's own overlay.
    SuppressedForSelection,
    /// The display is rotated, and where the pointer belongs on it has not been established.
    ///
    /// The position plainly needs the rotation normalization the pixels get; whether the shape
    /// arrives already rotated is not settled by the documentation and needs a rotated display to
    /// answer. Reported rather than guessed, because a cursor drawn in the wrong place is a worse
    /// answer than no cursor and a reason.
    RotatedDisplayUnverified,
    /// The pointer is visible but no shape has been received for it yet.
    ///
    /// A real state rather than an error. The compositor only sends a pointer shape when it
    /// changes, so a duplication that has just been created knows where the pointer is before it
    /// knows what it looks like.
    ShapeNotYetKnown,
}

/// What became of the cursor for one capture.
///
/// Recorded because "no cursor in the image" has several causes and only one of them is the user
/// not asking for one. A request that could not be honoured should be visible in the result rather
/// than indistinguishable from a request never made.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum CursorCapture {
    Excluded,
    /// Drawn into the frame over this rectangle, in frame coordinates before any crop.
    Composited {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
    Absent {
        reason: CursorAbsence,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameMetadata {
    pub capture_id: CaptureId,
    pub backend: String,
    pub display_id: DisplayId,
    pub source_rect: Rect,
    pub rotation_degrees: u16,
    pub capture_mode: CaptureMode,
    pub presentation_offset_ns: Option<i64>,
    pub timing_provenance: TimingProvenance,
    pub native_ready_offset_ns: u64,
    pub cpu_ready_offset_ns: Option<u64>,
    pub frame_age_ns: Option<u64>,
    pub frame_generation: Option<u64>,
    pub copy_count: u32,
    pub pool_slot: Option<u16>,
    /// Absent in frames produced before cursor composition existed, and in fixtures that do not
    /// care about it; `Excluded` and a missing field mean the same thing to a reader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CursorCapture>,
}

/// A frame of pixels whose layout has been validated against its buffer.
///
/// The layout-bearing fields are private, and that is the whole of what `CpuFrame` guarantees:
/// `pixels` really does hold `stride_bytes * height` bytes, `stride_bytes` really is wide enough
/// for `width` pixels of `format`. Every consumer that indexes into `pixels` relies on all three
/// agreeing, and while they were public the validation in [`CpuFrame::new`] described the frame at
/// the moment it was built rather than the frame in hand.
#[derive(Clone, Debug)]
pub struct CpuFrame {
    pixels: Arc<[u8]>,
    width: u32,
    height: u32,
    stride_bytes: u32,
    format: PixelFormat,
    pub origin: FrameOrigin,
    pub color_space: ColorSpace,
    alpha: FrameAlpha,
    pub metadata: FrameMetadata,
}

impl CpuFrame {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pixels: Arc<[u8]>,
        width: u32,
        height: u32,
        stride_bytes: u32,
        format: PixelFormat,
        origin: FrameOrigin,
        color_space: ColorSpace,
        metadata: FrameMetadata,
    ) -> Result<Self, FrameError> {
        validate_layout(pixels.len(), width, height, stride_bytes, format)?;
        Ok(Self {
            pixels,
            width,
            height,
            stride_bytes,
            format,
            origin,
            color_space,
            alpha: FrameAlpha::Opaque,
            metadata,
        })
    }

    /// Marks a frame whose pixel alpha has been deliberately initialized by its producer.
    pub fn with_alpha(mut self, alpha: FrameAlpha) -> Self {
        self.alpha = alpha;
        self
    }

    /// The validated pixel buffer.
    ///
    /// Read it with `stride_bytes`, never with `width * 4`: a frame may carry row padding, and
    /// not every format is four bytes per pixel.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// The same buffer as a handle that can be cloned without copying it.
    ///
    /// Separate from [`CpuFrame::pixels`] because sharing a frame and reading one are different
    /// intentions, and only the first has any reason to name `Arc`.
    pub fn pixels_shared(&self) -> &Arc<[u8]> {
        &self.pixels
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride_bytes(&self) -> u32 {
        self.stride_bytes
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    pub fn alpha(&self) -> FrameAlpha {
        self.alpha
    }

    pub fn required_bytes(&self) -> usize {
        self.stride_bytes as usize * self.height as usize
    }

    pub fn crop(&self, selection: Rect) -> Result<Self, FrameError> {
        if selection.width == 0 || selection.height == 0 {
            return Err(FrameError::EmptyCrop);
        }
        let source = self.metadata.source_rect;
        if source.width != self.width || source.height != self.height {
            return Err(FrameError::CropOutsideSource);
        }
        let source_right = i64::from(source.x) + i64::from(source.width);
        let source_bottom = i64::from(source.y) + i64::from(source.height);
        let selection_right = i64::from(selection.x) + i64::from(selection.width);
        let selection_bottom = i64::from(selection.y) + i64::from(selection.height);
        if selection.x < source.x
            || selection.y < source.y
            || selection_right > source_right
            || selection_bottom > source_bottom
        {
            return Err(FrameError::CropOutsideSource);
        }
        let local_x =
            u32::try_from(selection.x - source.x).map_err(|_| FrameError::CropOutsideSource)?;
        let local_y =
            u32::try_from(selection.y - source.y).map_err(|_| FrameError::CropOutsideSource)?;
        let bytes_per_pixel = self.format.bytes_per_pixel();
        let destination_stride = selection
            .width
            .checked_mul(bytes_per_pixel)
            .ok_or(FrameError::SizeOverflow)?;
        let destination_len = destination_stride
            .checked_mul(selection.height)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(FrameError::SizeOverflow)?;
        let source_x_bytes = local_x
            .checked_mul(bytes_per_pixel)
            .ok_or(FrameError::SizeOverflow)? as usize;
        let row_bytes = destination_stride as usize;
        let source_stride = self.stride_bytes as usize;
        let mut pixels = vec![0_u8; destination_len];
        for row in 0..selection.height as usize {
            let source_row = local_y as usize + row;
            let source_start = source_row
                .checked_mul(source_stride)
                .and_then(|value| value.checked_add(source_x_bytes))
                .ok_or(FrameError::SizeOverflow)?;
            let destination_start = row * row_bytes;
            pixels[destination_start..destination_start + row_bytes]
                .copy_from_slice(&self.pixels[source_start..source_start + row_bytes]);
        }
        let mut metadata = self.metadata.clone();
        metadata.source_rect = selection;
        metadata.copy_count = metadata.copy_count.saturating_add(1);
        metadata.pool_slot = None;
        Self::new(
            Arc::from(pixels),
            selection.width,
            selection.height,
            destination_stride,
            self.format,
            self.origin,
            self.color_space,
            metadata,
        )
        .map(|frame| frame.with_alpha(self.alpha))
    }
}

pub fn validate_layout(
    actual_bytes: usize,
    width: u32,
    height: u32,
    stride_bytes: u32,
    format: PixelFormat,
) -> Result<usize, FrameError> {
    if width == 0 || height == 0 {
        return Err(FrameError::EmptyDimensions);
    }
    let minimum_stride = width
        .checked_mul(format.bytes_per_pixel())
        .ok_or(FrameError::SizeOverflow)?;
    if stride_bytes < minimum_stride {
        return Err(FrameError::InvalidStride {
            stride: stride_bytes,
            minimum: minimum_stride,
        });
    }
    let required_u32 = stride_bytes
        .checked_mul(height)
        .ok_or(FrameError::SizeOverflow)?;
    let required = usize::try_from(required_u32).map_err(|_| FrameError::SizeOverflow)?;
    if actual_bytes < required {
        return Err(FrameError::BufferTooShort {
            actual: actual_bytes,
            required,
        });
    }
    Ok(required)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureMode, DisplayId, TimingProvenance};

    #[test]
    fn rejects_short_buffers() {
        assert_eq!(
            validate_layout(15, 2, 2, 8, PixelFormat::Bgra8Unorm),
            Err(FrameError::BufferTooShort {
                actual: 15,
                required: 16,
            })
        );
    }

    #[test]
    fn accepts_padded_stride() {
        assert_eq!(
            validate_layout(24, 2, 2, 12, PixelFormat::Bgra8Unorm),
            Ok(24)
        );
    }

    fn test_frame() -> CpuFrame {
        CpuFrame::new(
            Arc::from([
                1_u8, 2, 3, 4, 5, 6, 7, 8, 90, 91, 92, 93, 9, 10, 11, 12, 13, 14, 15, 16, 94, 95,
                96, 97,
            ]),
            2,
            2,
            12,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            FrameMetadata {
                capture_id: CaptureId(1),
                backend: "test".to_owned(),
                display_id: DisplayId::primary(),
                source_rect: Rect {
                    x: 100,
                    y: 200,
                    width: 2,
                    height: 2,
                },
                rotation_degrees: 0,
                capture_mode: CaptureMode::Latest { max_age_ms: None },
                presentation_offset_ns: Some(0),
                timing_provenance: TimingProvenance::Synthetic,
                native_ready_offset_ns: 1,
                cpu_ready_offset_ns: Some(2),
                frame_age_ns: Some(0),
                frame_generation: Some(1),
                copy_count: 1,
                pool_slot: Some(0),
                cursor: None,
            },
        )
        .expect("test frame")
    }

    #[test]
    fn crops_absolute_selection_and_removes_source_padding() {
        let cropped = test_frame()
            .crop(Rect {
                x: 101,
                y: 200,
                width: 1,
                height: 2,
            })
            .expect("valid crop");
        assert_eq!(cropped.width, 1);
        assert_eq!(cropped.height, 2);
        assert_eq!(cropped.stride_bytes, 4);
        assert_eq!(&*cropped.pixels, &[5, 6, 7, 8, 13, 14, 15, 16]);
        assert_eq!(cropped.metadata.source_rect.x, 101);
        assert_eq!(cropped.metadata.copy_count, 2);
        assert_eq!(cropped.metadata.pool_slot, None);
    }

    #[test]
    fn a_wider_pixel_widens_every_layout_rule_that_depends_on_it() {
        // Eight bytes per pixel, so a two-pixel row needs sixteen. The same numbers that describe
        // a valid four-byte frame describe an invalid eight-byte one, which is the whole reason
        // stride validation is a function of the format rather than a constant.
        assert_eq!(PixelFormat::Rgba16Float.bytes_per_pixel(), 8);
        assert_eq!(
            validate_layout(16, 2, 1, 8, PixelFormat::Rgba16Float),
            Err(FrameError::InvalidStride {
                stride: 8,
                minimum: 16
            })
        );
        assert_eq!(
            validate_layout(16, 2, 1, 16, PixelFormat::Rgba16Float),
            Ok(16)
        );
        assert_eq!(
            validate_layout(16, 2, 2, 16, PixelFormat::Rgba16Float),
            Err(FrameError::BufferTooShort {
                actual: 16,
                required: 32
            })
        );
    }

    #[test]
    fn a_half_float_frame_crops_by_its_own_pixel_width() {
        let mut metadata = test_frame().metadata.clone();
        metadata.source_rect = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        };
        // Two pixels of eight bytes, distinguishable by their first byte.
        let pixels: Vec<u8> = (0..16_u8).collect();
        let frame = CpuFrame::new(
            Arc::from(pixels),
            2,
            1,
            16,
            PixelFormat::Rgba16Float,
            FrameOrigin::TopLeft,
            ColorSpace::ScRgb,
            metadata,
        )
        .expect("a valid half-float frame");

        let cropped = frame
            .crop(Rect {
                x: 1,
                y: 0,
                width: 1,
                height: 1,
            })
            .expect("valid crop");

        // Eight bytes taken, not four: cropping reads the format rather than assuming BGRA8.
        assert_eq!(cropped.stride_bytes(), 8);
        assert_eq!(cropped.pixels(), &[8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(cropped.format(), PixelFormat::Rgba16Float);
        assert_eq!(cropped.color_space, ColorSpace::ScRgb);
    }

    #[test]
    fn rejects_crop_outside_source() {
        assert!(matches!(
            test_frame().crop(Rect {
                x: 99,
                y: 200,
                width: 1,
                height: 1,
            }),
            Err(FrameError::CropOutsideSource)
        ));
    }
}
