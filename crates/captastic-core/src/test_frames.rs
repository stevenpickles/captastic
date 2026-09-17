//! Frames for the encoders' tests.
//!
//! Every encoder asks the same questions of a frame, so every encoder's tests need the same frames
//! to ask them with. Built here once rather than three times, so a change to `CpuFrame`'s
//! construction is one edit and the three encoders keep testing the same thing.

use std::sync::Arc;

use crate::capture::{CaptureId, CaptureMode};
use crate::display::{DisplayId, Rect};
use crate::frame::{
    ColorSpace, CpuFrame, FrameMetadata, FrameOrigin, PixelFormat, TimingProvenance,
};
use crate::FrameError;

/// A BGRA frame, the shape every Windows capture backend produces.
pub(crate) fn frame(width: u32, height: u32, stride_bytes: u32, pixels: Vec<u8>) -> CpuFrame {
    frame_in(PixelFormat::Bgra8Unorm, width, height, stride_bytes, pixels)
}

pub(crate) fn frame_in(
    format: PixelFormat,
    width: u32,
    height: u32,
    stride_bytes: u32,
    pixels: Vec<u8>,
) -> CpuFrame {
    build_frame_in(format, width, height, stride_bytes, pixels).expect("test frame is valid")
}

pub(crate) fn build_frame(
    width: u32,
    height: u32,
    stride_bytes: u32,
    pixels: Vec<u8>,
) -> Result<CpuFrame, FrameError> {
    build_frame_in(PixelFormat::Bgra8Unorm, width, height, stride_bytes, pixels)
}

pub(crate) fn build_frame_in(
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
