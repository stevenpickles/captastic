//! What an editor does to a capture, independent of any window that shows it (ADR 0010).
//!
//! An [`Edit`] holds a capture and the operations made on it so far: one crop and any number of
//! redactions, each undoable. Nothing touches the capture's pixels until [`Edit::render`], which
//! makes the image that will be published. Redaction there is destructive: the pixels under a
//! redaction are overwritten with an opaque fill in the rendered image, never blurred and never
//! kept underneath, so nothing that receives it can recover what was there.
//!
//! Coordinates are the capture's own pixels, with the top-left pixel at `(0, 0)`, whatever part
//! of the desktop it was taken from.

use std::sync::Arc;

use crate::{CpuFrame, FrameError, PixelEncoding, Rect};

/// The fill a redaction leaves, for an eight-bit pixel: opaque black in either channel order.
const REDACTION_EIGHT_BIT: [u8; 4] = [0, 0, 0, u8::MAX];
/// The same for a half-float pixel: zero in every colour channel and 1.0 in alpha, little-endian.
const REDACTION_HALF_FLOAT: [u8; 8] = [0, 0, 0, 0, 0, 0, 0x00, 0x3C];

/// One undoable operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Crop { before: Rect, after: Rect },
    Redact(Rect),
}

/// A capture being edited.
#[derive(Clone, Debug)]
pub struct Edit {
    source: CpuFrame,
    crop: Rect,
    redactions: Vec<Rect>,
    undo: Vec<Step>,
    redo: Vec<Step>,
}

impl Edit {
    /// Starts editing `source`, uncropped and unredacted.
    ///
    /// The frame should already be [`CpuFrame::detached`]: an editor keeps it for as long as the
    /// person takes, and must never hold a buffer a capture backend wants back.
    pub fn new(source: CpuFrame) -> Self {
        let crop = full_bounds(&source);
        Self {
            source,
            crop,
            redactions: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn source(&self) -> &CpuFrame {
        &self.source
    }

    /// The whole capture, which is also where the crop starts.
    pub fn bounds(&self) -> Rect {
        full_bounds(&self.source)
    }

    /// The part of the capture that will be published.
    pub fn crop(&self) -> Rect {
        self.crop
    }

    /// Every redaction, in the order made. Each lies within the capture, though not necessarily
    /// within the crop: one made before a tighter crop is kept, and still applies if that crop is
    /// undone.
    pub fn redactions(&self) -> &[Rect] {
        &self.redactions
    }

    /// Crops to `rect`, clipped to the capture. Returns whether anything changed: a crop that is
    /// empty once clipped, or the same as the current one, is not an operation.
    pub fn set_crop(&mut self, rect: Rect) -> bool {
        let Some(after) = rect.intersection(self.bounds()) else {
            return false;
        };
        if after == self.crop {
            return false;
        }
        self.record(Step::Crop {
            before: self.crop,
            after,
        });
        true
    }

    /// Redacts `rect`, clipped to the capture. Returns whether anything changed: a redaction that
    /// misses the capture is not an operation.
    pub fn redact(&mut self, rect: Rect) -> bool {
        let Some(rect) = rect.intersection(self.bounds()) else {
            return false;
        };
        self.record(Step::Redact(rect));
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Whether publishing now would publish something other than the capture as taken.
    pub fn is_modified(&self) -> bool {
        self.crop != self.bounds() || !self.redactions.is_empty()
    }

    /// Takes back the last operation. Returns whether there was one.
    pub fn undo(&mut self) -> bool {
        let Some(step) = self.undo.pop() else {
            return false;
        };
        match step {
            Step::Crop { before, .. } => self.crop = before,
            Step::Redact(_) => {
                self.redactions.pop();
            }
        }
        self.redo.push(step);
        true
    }

    /// Makes again the last operation undone. Returns whether there was one.
    pub fn redo(&mut self) -> bool {
        let Some(step) = self.redo.pop() else {
            return false;
        };
        self.apply(step);
        self.undo.push(step);
        true
    }

    fn record(&mut self, step: Step) {
        self.apply(step);
        self.undo.push(step);
        // A new operation starts a new branch; what was undone before it cannot be redone onto it.
        self.redo.clear();
    }

    fn apply(&mut self, step: Step) {
        match step {
            Step::Crop { after, .. } => self.crop = after,
            Step::Redact(rect) => self.redactions.push(rect),
        }
    }

    /// The image to publish: the crop, with every redaction inside it filled opaque.
    ///
    /// A new buffer every time, tightly packed. Its metadata is the capture's own, so it is
    /// published as the capture it came from, except that the source rectangle is the part of the
    /// desktop the crop covers.
    pub fn render(&self) -> Result<CpuFrame, FrameError> {
        let source = &self.source;
        let bytes_per_pixel = source.format().bytes_per_pixel() as usize;
        let crop = self.crop;
        let row_bytes = crop.width as usize * bytes_per_pixel;
        let source_stride = source.stride_bytes() as usize;
        let left = crop.x as usize * bytes_per_pixel;
        let mut pixels = vec![0_u8; row_bytes * crop.height as usize];
        for (row, target) in pixels.chunks_exact_mut(row_bytes).enumerate() {
            let start = (crop.y as usize + row) * source_stride + left;
            target.copy_from_slice(&source.pixels()[start..start + row_bytes]);
        }
        let fill: &[u8] = match source.format().encoding() {
            PixelEncoding::EightBitRgba { .. } => &REDACTION_EIGHT_BIT,
            PixelEncoding::HalfFloatRgba => &REDACTION_HALF_FLOAT,
        };
        for redaction in &self.redactions {
            let Some(covered) = redaction.intersection(crop) else {
                continue;
            };
            let x = (covered.x - crop.x) as usize;
            let y = (covered.y - crop.y) as usize;
            for row in y..y + covered.height as usize {
                let start = row * row_bytes + x * bytes_per_pixel;
                let span = &mut pixels[start..start + covered.width as usize * bytes_per_pixel];
                for pixel in span.chunks_exact_mut(bytes_per_pixel) {
                    pixel.copy_from_slice(fill);
                }
            }
        }
        let mut metadata = source.metadata.clone();
        let origin = metadata.source_rect;
        metadata.source_rect = Rect {
            x: origin.x.saturating_add(crop.x),
            y: origin.y.saturating_add(crop.y),
            width: crop.width,
            height: crop.height,
        };
        metadata.copy_count = metadata.copy_count.saturating_add(1);
        metadata.pool_slot = None;
        CpuFrame::new(
            Arc::from(pixels),
            crop.width,
            crop.height,
            row_bytes as u32,
            source.format(),
            source.origin,
            source.color_space,
            metadata,
        )
        .map(|frame| frame.with_alpha(source.alpha()))
    }
}

fn full_bounds(frame: &CpuFrame) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: frame.width(),
        height: frame.height(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CaptureId, CaptureMode, ColorSpace, DisplayId, FrameAlpha, FrameMetadata, FrameOrigin,
        PixelFormat, TimingProvenance,
    };

    /// A four-by-three BGRA frame, with a padded stride, whose pixel at `(x, y)` is
    /// `[x, y, 9, 0x80]`, taken from `(100, 200)` on the desktop.
    fn frame() -> CpuFrame {
        let (width, height, stride) = (4_u32, 3_u32, 20_usize);
        let mut pixels = vec![0xEE_u8; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let at = y * stride + x * 4;
                pixels[at..at + 4].copy_from_slice(&[x as u8, y as u8, 9, 0x80]);
            }
        }
        CpuFrame::new(
            Arc::from(pixels),
            width,
            height,
            stride as u32,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            FrameMetadata {
                capture_id: CaptureId(7),
                backend: "test".to_owned(),
                display_id: DisplayId::primary(),
                source_rect: Rect {
                    x: 100,
                    y: 200,
                    width,
                    height,
                },
                rotation_degrees: 0,
                capture_mode: CaptureMode::Latest { max_age_ms: None },
                presentation_offset_ns: None,
                timing_provenance: TimingProvenance::Synthetic,
                native_ready_offset_ns: 0,
                cpu_ready_offset_ns: None,
                frame_age_ns: None,
                verified_current_offset_ns: None,
                frame_generation: None,
                copy_count: 1,
                pool_slot: Some(2),
                cursor: None,
            },
        )
        .expect("test frame")
        .with_alpha(FrameAlpha::Straight)
    }

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn pixel(frame: &CpuFrame, x: usize, y: usize) -> [u8; 4] {
        let at = y * frame.stride_bytes() as usize + x * 4;
        frame.pixels()[at..at + 4].try_into().expect("a pixel")
    }

    #[test]
    fn an_untouched_edit_renders_the_capture_as_taken() {
        let edit = Edit::new(frame());
        assert!(!edit.is_modified());
        let rendered = edit.render().expect("render");
        assert_eq!((rendered.width(), rendered.height()), (4, 3));
        // Tightly packed: the source's row padding is not carried into the published image.
        assert_eq!(rendered.stride_bytes(), 16);
        assert_eq!(pixel(&rendered, 3, 2), [3, 2, 9, 0x80]);
        assert_eq!(rendered.metadata.capture_id, CaptureId(7));
        assert_eq!(rendered.metadata.source_rect, rect(100, 200, 4, 3));
        assert_eq!(rendered.metadata.pool_slot, None);
        assert_eq!(rendered.alpha(), FrameAlpha::Straight);
    }

    #[test]
    fn a_crop_publishes_only_its_pixels_and_where_they_came_from() {
        let mut edit = Edit::new(frame());
        assert!(edit.set_crop(rect(1, 1, 2, 2)));
        let rendered = edit.render().expect("render");
        assert_eq!((rendered.width(), rendered.height()), (2, 2));
        assert_eq!(pixel(&rendered, 0, 0), [1, 1, 9, 0x80]);
        assert_eq!(pixel(&rendered, 1, 1), [2, 2, 9, 0x80]);
        assert_eq!(rendered.metadata.source_rect, rect(101, 201, 2, 2));
    }

    #[test]
    fn a_crop_is_clipped_to_the_capture_and_an_empty_one_is_refused() {
        let mut edit = Edit::new(frame());
        assert!(edit.set_crop(rect(-5, 2, 100, 100)));
        assert_eq!(edit.crop(), rect(0, 2, 4, 1));
        assert!(!edit.set_crop(rect(10, 10, 2, 2)));
        assert!(!edit.set_crop(rect(1, 1, 0, 2)));
        // The same crop again is not an operation, so it cannot fill the undo history.
        assert!(!edit.set_crop(rect(0, 2, 4, 1)));
        assert_eq!(edit.crop(), rect(0, 2, 4, 1));
    }

    #[test]
    fn a_redaction_overwrites_its_pixels_with_an_opaque_fill() {
        let mut edit = Edit::new(frame());
        assert!(edit.redact(rect(1, 0, 2, 2)));
        let rendered = edit.render().expect("render");
        assert_eq!(pixel(&rendered, 1, 0), REDACTION_EIGHT_BIT);
        assert_eq!(pixel(&rendered, 2, 1), REDACTION_EIGHT_BIT);
        // Opaque even in a capture whose alpha is real: a translucent fill would let whatever the
        // image is composited onto show what is not there, but a pixel's colour is gone either way.
        assert_eq!(pixel(&rendered, 1, 0)[3], u8::MAX);
        assert_eq!(pixel(&rendered, 0, 0), [0, 0, 9, 0x80]);
        assert_eq!(pixel(&rendered, 3, 1), [3, 1, 9, 0x80]);
        assert_eq!(pixel(&rendered, 1, 2), [1, 2, 9, 0x80]);
        // And nowhere in the published buffer does a redacted pixel's value survive.
        assert!(!rendered
            .pixels()
            .chunks_exact(4)
            .any(|value| value == [1, 0, 9, 0x80] || value == [2, 1, 9, 0x80]));
    }

    #[test]
    fn a_redaction_is_applied_through_the_crop_it_overlaps() {
        let mut edit = Edit::new(frame());
        assert!(edit.redact(rect(0, 0, 2, 3)));
        assert!(edit.set_crop(rect(1, 1, 3, 2)));
        let rendered = edit.render().expect("render");
        assert_eq!(pixel(&rendered, 0, 0), REDACTION_EIGHT_BIT);
        assert_eq!(pixel(&rendered, 0, 1), REDACTION_EIGHT_BIT);
        assert_eq!(pixel(&rendered, 1, 0), [2, 1, 9, 0x80]);
    }

    #[test]
    fn a_redaction_that_misses_the_capture_is_refused() {
        let mut edit = Edit::new(frame());
        assert!(!edit.redact(rect(4, 0, 3, 3)));
        assert!(!edit.redact(rect(0, 0, 0, 3)));
        assert!(!edit.can_undo());
        assert!(edit.redact(rect(3, 2, 10, 10)));
        assert_eq!(edit.redactions(), [rect(3, 2, 1, 1)]);
    }

    #[test]
    fn undo_and_redo_walk_the_operations_in_order() {
        let mut edit = Edit::new(frame());
        assert!(!edit.undo());
        edit.redact(rect(0, 0, 1, 1));
        edit.set_crop(rect(0, 0, 2, 2));
        edit.set_crop(rect(1, 1, 1, 1));
        assert!(edit.undo());
        assert_eq!(edit.crop(), rect(0, 0, 2, 2));
        assert!(edit.undo());
        assert_eq!(edit.crop(), rect(0, 0, 4, 3));
        assert!(edit.undo());
        assert!(edit.redactions().is_empty());
        assert!(!edit.is_modified());
        assert!(!edit.can_undo());
        assert!(edit.redo());
        assert!(edit.redo());
        assert_eq!(edit.redactions(), [rect(0, 0, 1, 1)]);
        assert_eq!(edit.crop(), rect(0, 0, 2, 2));
        // A new operation after an undo discards what could have been redone.
        edit.redact(rect(2, 2, 1, 1));
        assert!(!edit.can_redo());
        assert!(!edit.redo());
    }

    #[test]
    fn a_half_float_capture_is_redacted_to_opaque_black_in_its_own_encoding() {
        let mut metadata = frame().metadata.clone();
        metadata.source_rect = rect(0, 0, 2, 1);
        let frame = CpuFrame::new(
            Arc::from(vec![0x11_u8; 16]),
            2,
            1,
            16,
            PixelFormat::Rgba16Float,
            FrameOrigin::TopLeft,
            ColorSpace::ScRgb,
            metadata,
        )
        .expect("half-float frame");
        let mut edit = Edit::new(frame);
        edit.redact(rect(1, 0, 1, 1));
        let rendered = edit.render().expect("render");
        assert_eq!(&rendered.pixels()[..8], &[0x11; 8]);
        assert_eq!(&rendered.pixels()[8..], &REDACTION_HALF_FLOAT);
    }
}
