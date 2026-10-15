//! What an editor does to a capture, independent of any window that shows it (ADR 0010).
//!
//! An [`Edit`] holds a capture and the operations made on it so far: one crop and any number of
//! [`Mark`]s - redactions, box outlines and pen, marker and highlighter strokes - each undoable.
//! Nothing touches the capture's pixels until [`Edit::render`], which makes the image that will be
//! published, painting the marks in the order they were made.
//!
//! Redaction is destructive in that image: the pixels under a redaction are overwritten with an
//! opaque fill, never blurred and never kept underneath, so nothing that receives it can recover
//! what was there. A mark made after a redaction is painted over the fill, so it can cover a
//! redaction but never uncover one.
//!
//! Coordinates are the capture's own pixels, with the top-left pixel at `(0, 0)`, whatever part
//! of the desktop it was taken from.

use std::sync::Arc;

use crate::{CpuFrame, FrameAlpha, FrameError, PixelEncoding, PixelFormat, Rect};

/// The fill a redaction leaves, for an eight-bit pixel: opaque black in either channel order.
const REDACTION_EIGHT_BIT: [u8; 4] = [0, 0, 0, u8::MAX];
/// The same for a half-float pixel: zero in every colour channel and 1.0 in alpha, little-endian.
const REDACTION_HALF_FLOAT: [u8; 8] = [0, 0, 0, 0, 0, 0, 0x00, 0x3C];
/// How strongly a highlighter tints at full coverage. Less than one, so even a dark highlighter
/// leaves what is under it readable.
const HIGHLIGHTER_STRENGTH: f32 = 0.85;

/// An opaque colour for a mark.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Color {
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

/// How a freehand stroke is laid on the capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrokeStyle {
    /// Opaque ink, for a pen or a marker; the two differ only in width.
    Ink,
    /// A tint that darkens toward the colour, as a highlighter does on paper: what is under it
    /// stays visible, and crossing a stroke over itself does not deepen it.
    Highlighter,
}

/// Something drawn on a capture.
#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    /// An opaque fill over everything beneath it.
    Redaction(Rect),
    /// A box outline, `width` pixels thick, drawn inside `rect`, its outer corners rounded by
    /// `radius` pixels. The inner corners follow, so the band is the same width all the way round.
    Rectangle {
        rect: Rect,
        color: Color,
        width: u32,
        radius: f32,
    },
    /// A freehand line through `points`, `width` pixels across, with round ends and joins.
    Stroke {
        points: Vec<(f32, f32)>,
        color: Color,
        width: f32,
        style: StrokeStyle,
    },
}

impl Mark {
    /// Every pixel the mark can change, unclipped.
    pub fn bounds(&self) -> Option<Rect> {
        match self {
            Self::Redaction(rect) | Self::Rectangle { rect, .. } => {
                (rect.width > 0 && rect.height > 0).then_some(*rect)
            }
            Self::Stroke { points, width, .. } => {
                let reach = width / 2.0 + 1.0;
                let (mut left, mut top) = (f32::MAX, f32::MAX);
                let (mut right, mut bottom) = (f32::MIN, f32::MIN);
                for &(x, y) in points {
                    left = left.min(x);
                    top = top.min(y);
                    right = right.max(x);
                    bottom = bottom.max(y);
                }
                if points.is_empty() || *width <= 0.0 {
                    return None;
                }
                Rect::from_edges(
                    (left - reach).floor() as i64,
                    (top - reach).floor() as i64,
                    (right + reach).ceil() as i64,
                    (bottom + reach).ceil() as i64,
                )
            }
        }
    }
}

/// One undoable operation.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Crop { before: Rect, after: Rect },
    Mark(Mark),
}

/// A capture being edited.
#[derive(Clone, Debug)]
pub struct Edit {
    source: CpuFrame,
    crop: Rect,
    marks: Vec<Mark>,
    undo: Vec<Step>,
    redo: Vec<Step>,
}

impl Edit {
    /// Starts editing `source`, uncropped and unmarked.
    ///
    /// The frame should already be [`CpuFrame::detached`]: an editor keeps it for as long as the
    /// person takes, and must never hold a buffer a capture backend wants back.
    pub fn new(source: CpuFrame) -> Self {
        let crop = full_bounds(&source);
        Self {
            source,
            crop,
            marks: Vec::new(),
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

    /// Every mark, in the order made, which is the order they are painted in. A mark is kept
    /// whole even where it falls outside the crop, so undoing a crop brings all of it back.
    pub fn marks(&self) -> &[Mark] {
        &self.marks
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
        self.add(Mark::Redaction(rect))
    }

    /// Adds a mark. Returns whether anything changed: a mark that would change no pixel of the
    /// capture is not an operation. A redaction is clipped to the capture; a drawn mark is kept as
    /// drawn, and only offered to a capture of eight-bit pixels, since nothing paints a colour
    /// into a wider one yet.
    pub fn add(&mut self, mark: Mark) -> bool {
        let bounds = self.bounds();
        let mark = match mark {
            Mark::Redaction(rect) => match rect.intersection(bounds) {
                Some(rect) => Mark::Redaction(rect),
                None => return false,
            },
            drawn => {
                let eight_bit = matches!(
                    self.source.format().encoding(),
                    PixelEncoding::EightBitRgba { .. }
                );
                let visible = drawn
                    .bounds()
                    .and_then(|reach| reach.intersection(bounds))
                    .is_some();
                let drawable = match &drawn {
                    Mark::Rectangle { width, .. } => *width > 0,
                    Mark::Stroke { width, .. } => *width > 0.0,
                    Mark::Redaction(_) => true,
                };
                if !eight_bit || !visible || !drawable {
                    return false;
                }
                drawn
            }
        };
        self.record(Step::Mark(mark));
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
        self.crop != self.bounds() || !self.marks.is_empty()
    }

    /// Takes back the last operation. Returns whether there was one.
    pub fn undo(&mut self) -> bool {
        let Some(step) = self.undo.pop() else {
            return false;
        };
        match &step {
            Step::Crop { before, .. } => self.crop = *before,
            Step::Mark(_) => {
                self.marks.pop();
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
        self.apply(step.clone());
        self.undo.push(step);
        true
    }

    fn record(&mut self, step: Step) {
        self.apply(step.clone());
        self.undo.push(step);
        // A new operation starts a new branch; what was undone before it cannot be redone onto it.
        self.redo.clear();
    }

    fn apply(&mut self, step: Step) {
        match step {
            Step::Crop { after, .. } => self.crop = after,
            Step::Mark(mark) => self.marks.push(mark),
        }
    }

    /// The image to publish: the crop, with every mark painted on it in order.
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
        for mark in &self.marks {
            paint_mark(
                mark,
                &mut pixels,
                row_bytes,
                crop,
                source.format(),
                source.alpha(),
            );
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

/// Paints `mark` into `pixels`, which hold the part of a capture covered by `area`, `stride`
/// bytes per row, in `format`. Only the pixels inside both the mark and `area` are touched, so a
/// window that redraws one mark at a time pays only for that mark.
///
/// A drawn mark in a capture of anything but eight-bit pixels is left unpainted; [`Edit::add`]
/// never accepts one.
pub fn paint_mark(
    mark: &Mark,
    pixels: &mut [u8],
    stride: usize,
    area: Rect,
    format: PixelFormat,
    alpha: FrameAlpha,
) {
    let Some(reach) = mark.bounds().and_then(|bounds| bounds.intersection(area)) else {
        return;
    };
    let bytes_per_pixel = format.bytes_per_pixel() as usize;
    let offset =
        |x: i32, y: i32| (y - area.y) as usize * stride + (x - area.x) as usize * bytes_per_pixel;
    let blue_first = match format.encoding() {
        PixelEncoding::EightBitRgba { blue_first } => blue_first,
        PixelEncoding::HalfFloatRgba => {
            if let Mark::Redaction(_) = mark {
                fill(
                    pixels,
                    reach,
                    offset,
                    bytes_per_pixel,
                    &REDACTION_HALF_FLOAT,
                );
            }
            return;
        }
    };
    match mark {
        Mark::Redaction(_) => fill(pixels, reach, offset, 4, &REDACTION_EIGHT_BIT),
        Mark::Rectangle {
            rect,
            color,
            width,
            radius,
        } => {
            let ink = ordered(*color, blue_first);
            for y in reach.y..(reach.bottom() as i32) {
                for x in reach.x..(reach.right() as i32) {
                    let cover = box_outline_coverage(*rect, *width as f32, *radius, x, y);
                    if cover > 0.0 {
                        let at = offset(x, y);
                        blend_over(&mut pixels[at..at + 4], &ink, cover, alpha);
                    }
                }
            }
        }
        Mark::Stroke {
            points,
            color,
            width,
            style,
        } => {
            let coverage = stroke_coverage(points, *width, reach);
            let ink = ordered(*color, blue_first);
            for y in 0..reach.height as usize {
                for x in 0..reach.width as usize {
                    let cover = coverage[y * reach.width as usize + x];
                    if cover <= 0.0 {
                        continue;
                    }
                    let at = offset(reach.x + x as i32, reach.y + y as i32);
                    let pixel = &mut pixels[at..at + 4];
                    match style {
                        StrokeStyle::Ink => blend_over(pixel, &ink, cover, alpha),
                        StrokeStyle::Highlighter => tint(pixel, &ink, cover * HIGHLIGHTER_STRENGTH),
                    }
                }
            }
        }
    }
}

/// How much of the pixel at `(x, y)` a box outline covers, from 0 to 1, antialiased along both
/// its outer and its inner edge.
fn box_outline_coverage(rect: Rect, width: f32, radius: f32, x: i32, y: i32) -> f32 {
    let half_width = rect.width as f32 / 2.0;
    let half_height = rect.height as f32 / 2.0;
    let radius = radius.clamp(0.0, half_width.min(half_height));
    let centre = (rect.x as f32 + half_width, rect.y as f32 + half_height);
    let point = (x as f32 + 0.5 - centre.0, y as f32 + 0.5 - centre.1);
    // The signed distance to the rounded outer edge: negative inside, positive outside.
    let q = (
        point.0.abs() - (half_width - radius),
        point.1.abs() - (half_height - radius),
    );
    let outside = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt();
    let distance = outside + q.0.max(q.1).min(0.0) - radius;
    let within_outer = (0.5 - distance).clamp(0.0, 1.0);
    let beyond_inner = (distance + width + 0.5).clamp(0.0, 1.0);
    within_outer.min(beyond_inner)
}

fn ordered(color: Color, blue_first: bool) -> [u8; 4] {
    if blue_first {
        [color.blue, color.green, color.red, u8::MAX]
    } else {
        [color.red, color.green, color.blue, u8::MAX]
    }
}

fn fill(
    pixels: &mut [u8],
    rect: Rect,
    offset: impl Fn(i32, i32) -> usize,
    bytes_per_pixel: usize,
    value: &[u8],
) {
    for y in rect.y..(rect.bottom() as i32) {
        let start = offset(rect.x, y);
        let span = &mut pixels[start..start + rect.width as usize * bytes_per_pixel];
        for pixel in span.chunks_exact_mut(bytes_per_pixel) {
            pixel.copy_from_slice(value);
        }
    }
}

/// How much of each pixel of `area` a stroke covers, from 0 to 1, row by row.
///
/// The largest coverage any one segment gives a pixel, not their sum, so the joins of a stroke
/// and a stroke crossing itself are no heavier than the rest of it. Each segment only visits the
/// pixels near it.
fn stroke_coverage(points: &[(f32, f32)], width: f32, area: Rect) -> Vec<f32> {
    let mut coverage = vec![0.0_f32; area.width as usize * area.height as usize];
    let radius = width / 2.0;
    let first = points.first().copied().unwrap_or_default();
    let segments = points
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .chain(std::iter::once((first, first)).take(usize::from(points.len() == 1)));
    for (start, end) in segments {
        let left = ((start.0.min(end.0) - radius - 1.0).floor() as i64).max(i64::from(area.x));
        let top = ((start.1.min(end.1) - radius - 1.0).floor() as i64).max(i64::from(area.y));
        let right = ((start.0.max(end.0) + radius + 1.0).ceil() as i64).min(area.right());
        let bottom = ((start.1.max(end.1) + radius + 1.0).ceil() as i64).min(area.bottom());
        for y in top..bottom {
            for x in left..right {
                let distance = distance_to_segment((x as f32 + 0.5, y as f32 + 0.5), start, end);
                let cover = (radius + 0.5 - distance).clamp(0.0, 1.0);
                let index = (y - i64::from(area.y)) as usize * area.width as usize
                    + (x - i64::from(area.x)) as usize;
                if cover > coverage[index] {
                    coverage[index] = cover;
                }
            }
        }
    }
    coverage
}

fn distance_to_segment(point: (f32, f32), start: (f32, f32), end: (f32, f32)) -> f32 {
    let (dx, dy) = (end.0 - start.0, end.1 - start.1);
    let length_squared = dx * dx + dy * dy;
    let along = if length_squared > 0.0 {
        (((point.0 - start.0) * dx + (point.1 - start.1) * dy) / length_squared).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (nearest_x, nearest_y) = (start.0 + along * dx, start.1 + along * dy);
    ((point.0 - nearest_x).powi(2) + (point.1 - nearest_y).powi(2)).sqrt()
}

/// Lays `ink` over `pixel` at `cover`. In a capture whose alpha is real the result is composited
/// properly, so a stroke across a transparent corner is opaque where it is drawn; otherwise the
/// alpha byte means nothing and is left opaque.
fn blend_over(pixel: &mut [u8], ink: &[u8; 4], cover: f32, alpha: FrameAlpha) {
    match alpha {
        FrameAlpha::Opaque => {
            for channel in 0..3 {
                pixel[channel] = mix(pixel[channel], ink[channel], cover);
            }
            pixel[3] = u8::MAX;
        }
        FrameAlpha::Straight => {
            let below = f32::from(pixel[3]) / 255.0;
            let result = cover + below * (1.0 - cover);
            if result <= 0.0 {
                return;
            }
            for channel in 0..3 {
                let value = (f32::from(ink[channel]) * cover
                    + f32::from(pixel[channel]) * below * (1.0 - cover))
                    / result;
                pixel[channel] = value.round().clamp(0.0, 255.0) as u8;
            }
            pixel[3] = (result * 255.0).round() as u8;
        }
    }
}

/// Darkens `pixel` toward `ink` by `amount`, as a translucent marker multiplies the paper under
/// it. Alpha is left alone: a highlighter colours what is there and adds nothing where nothing is.
fn tint(pixel: &mut [u8], ink: &[u8; 4], amount: f32) {
    for channel in 0..3 {
        let factor = 1.0 - amount * (1.0 - f32::from(ink[channel]) / 255.0);
        pixel[channel] = (f32::from(pixel[channel]) * factor)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
}

fn mix(below: u8, above: u8, amount: f32) -> u8 {
    (f32::from(below) * (1.0 - amount) + f32::from(above) * amount)
        .round()
        .clamp(0.0, 255.0) as u8
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

    /// A white, opaque desktop capture of `width` by `height`.
    fn white(width: u32, height: u32) -> CpuFrame {
        let mut metadata = frame().metadata;
        metadata.source_rect = rect(0, 0, width, height);
        CpuFrame::new(
            Arc::from(vec![u8::MAX; (width * height * 4) as usize]),
            width,
            height,
            width * 4,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("white frame")
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

    const RED: Color = Color::rgb(220, 30, 40);
    const YELLOW: Color = Color::rgb(250, 220, 0);

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
        assert_eq!(edit.marks(), [Mark::Redaction(rect(3, 2, 1, 1))]);
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
        assert!(edit.marks().is_empty());
        assert!(!edit.is_modified());
        assert!(!edit.can_undo());
        assert!(edit.redo());
        assert!(edit.redo());
        assert_eq!(edit.marks(), [Mark::Redaction(rect(0, 0, 1, 1))]);
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
        // Nothing paints a colour into half-float pixels yet, so a drawn mark is refused.
        assert!(!edit.add(Mark::Rectangle {
            rect: rect(0, 0, 2, 1),
            color: RED,
            width: 1,
            radius: 0.0,
        }));
        let rendered = edit.render().expect("render");
        assert_eq!(&rendered.pixels()[..8], &[0x11; 8]);
        assert_eq!(&rendered.pixels()[8..], &REDACTION_HALF_FLOAT);
    }

    #[test]
    fn a_box_is_an_outline_of_its_width_inside_its_rectangle() {
        let mut edit = Edit::new(white(8, 8));
        assert!(edit.add(Mark::Rectangle {
            rect: rect(1, 1, 6, 6),
            color: RED,
            width: 2,
            radius: 0.0,
        }));
        let rendered = edit.render().expect("render");
        let red = [RED.blue, RED.green, RED.red, 255];
        assert_eq!(pixel(&rendered, 1, 1), red);
        assert_eq!(pixel(&rendered, 2, 4), red);
        assert_eq!(pixel(&rendered, 6, 6), red);
        assert_eq!(pixel(&rendered, 5, 3), red);
        // The inside and the outside are untouched.
        assert_eq!(pixel(&rendered, 3, 3), [255; 4]);
        assert_eq!(pixel(&rendered, 0, 0), [255; 4]);
        assert_eq!(pixel(&rendered, 7, 7), [255; 4]);
    }

    #[test]
    fn a_rounded_box_leaves_its_corners_clear_and_keeps_its_sides() {
        let mut edit = Edit::new(white(20, 20));
        assert!(edit.add(Mark::Rectangle {
            rect: rect(0, 0, 20, 20),
            color: RED,
            width: 2,
            radius: 6.0,
        }));
        let rendered = edit.render().expect("render");
        let red = [RED.blue, RED.green, RED.red, 255];
        // The very corner is outside the curve.
        assert_eq!(pixel(&rendered, 0, 0), [255; 4]);
        // The middle of each side is solid.
        assert_eq!(pixel(&rendered, 10, 0), red);
        assert_eq!(pixel(&rendered, 0, 10), red);
        assert_eq!(pixel(&rendered, 19, 10), red);
        assert_eq!(pixel(&rendered, 10, 19), red);
        // The curve is antialiased: somewhere near the corner a pixel is part-covered.
        assert!((0..6).any(|i| {
            let value = pixel(&rendered, i, i);
            value != red && value != [255; 4]
        }));
        // And the inside is untouched.
        assert_eq!(pixel(&rendered, 10, 10), [255; 4]);
    }

    #[test]
    fn a_pen_stroke_inks_along_its_path_with_soft_edges_and_round_ends() {
        let mut edit = Edit::new(white(12, 7));
        assert!(edit.add(Mark::Stroke {
            points: vec![(2.0, 3.5), (9.0, 3.5)],
            color: RED,
            width: 3.0,
            style: StrokeStyle::Ink,
        }));
        let rendered = edit.render().expect("render");
        let red = [RED.blue, RED.green, RED.red, 255];
        // Solid along the middle of the path.
        assert_eq!(pixel(&rendered, 5, 3), red);
        assert_eq!(pixel(&rendered, 5, 2), red);
        // Clear well away from it.
        assert_eq!(pixel(&rendered, 5, 0), [255; 4]);
        assert_eq!(pixel(&rendered, 11, 3), [255; 4]);
        // The end is rounded, reaching past the last point by about half the width.
        assert_ne!(pixel(&rendered, 9, 3), [255; 4]);
        assert_eq!(pixel(&rendered, 11, 6), [255; 4]);
    }

    #[test]
    fn a_single_click_with_the_pen_leaves_a_dot() {
        let mut edit = Edit::new(white(6, 6));
        assert!(edit.add(Mark::Stroke {
            points: vec![(3.0, 3.0)],
            color: RED,
            width: 4.0,
            style: StrokeStyle::Ink,
        }));
        let rendered = edit.render().expect("render");
        assert_eq!(pixel(&rendered, 2, 2), [RED.blue, RED.green, RED.red, 255]);
        assert_eq!(pixel(&rendered, 0, 0), [255; 4]);
    }

    #[test]
    fn a_highlighter_tints_what_is_under_it_and_does_not_deepen_where_it_crosses_itself() {
        let mut edit = Edit::new(white(12, 12));
        assert!(edit.add(Mark::Stroke {
            // Out and back over the same line.
            points: vec![(1.0, 6.0), (10.0, 6.0), (1.0, 6.0)],
            color: YELLOW,
            width: 6.0,
            style: StrokeStyle::Highlighter,
        }));
        let rendered = edit.render().expect("render");
        let tinted = pixel(&rendered, 5, 6);
        // White under yellow becomes yellow-ish: red and green stay high, blue drops.
        assert!(tinted[0] < 100, "{tinted:?}");
        assert!(tinted[1] > 200 && tinted[2] > 200, "{tinted:?}");
        // Retracing changed nothing: every covered pixel matches a single pass.
        let mut once = Edit::new(white(12, 12));
        once.add(Mark::Stroke {
            points: vec![(1.0, 6.0), (10.0, 6.0)],
            color: YELLOW,
            width: 6.0,
            style: StrokeStyle::Highlighter,
        });
        assert_eq!(rendered.pixels(), once.render().expect("render").pixels());
    }

    #[test]
    fn a_highlighter_keeps_dark_text_dark() {
        let mut metadata = frame().metadata;
        metadata.source_rect = rect(0, 0, 3, 1);
        let text = CpuFrame::new(
            Arc::from(vec![20_u8, 20, 20, 255, 20, 20, 20, 255, 20, 20, 20, 255]),
            3,
            1,
            12,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("dark frame");
        let mut edit = Edit::new(text);
        edit.add(Mark::Stroke {
            points: vec![(0.0, 0.5), (3.0, 0.5)],
            color: YELLOW,
            width: 4.0,
            style: StrokeStyle::Highlighter,
        });
        let rendered = edit.render().expect("render");
        // Unlike ink, a highlighter can only darken: dark text stays as dark or darker.
        assert!(pixel(&rendered, 1, 0)[..3]
            .iter()
            .all(|channel| *channel <= 20));
    }

    #[test]
    fn ink_over_a_transparent_corner_is_opaque_where_it_is_drawn() {
        let mut metadata = frame().metadata;
        metadata.source_rect = rect(0, 0, 4, 4);
        let transparent = CpuFrame::new(
            Arc::from(vec![0_u8; 64]),
            4,
            4,
            16,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("transparent frame")
        .with_alpha(FrameAlpha::Straight);
        let mut edit = Edit::new(transparent);
        edit.add(Mark::Stroke {
            points: vec![(0.0, 2.0), (4.0, 2.0)],
            color: RED,
            width: 2.0,
            style: StrokeStyle::Ink,
        });
        let rendered = edit.render().expect("render");
        assert_eq!(pixel(&rendered, 2, 1), [RED.blue, RED.green, RED.red, 255]);
        assert_eq!(pixel(&rendered, 2, 3)[3], 0);
    }

    #[test]
    fn marks_paint_in_the_order_made_so_a_later_one_covers_a_redaction_but_cannot_uncover_it() {
        let mut edit = Edit::new(white(6, 6));
        edit.add(Mark::Rectangle {
            rect: rect(0, 0, 6, 6),
            color: RED,
            width: 6,
            radius: 0.0,
        });
        edit.redact(rect(1, 1, 2, 2));
        edit.add(Mark::Stroke {
            points: vec![(2.0, 0.0), (2.0, 6.0)],
            color: YELLOW,
            width: 1.0,
            style: StrokeStyle::Highlighter,
        });
        let rendered = edit.render().expect("render");
        // The redaction covers the earlier box, and the highlighter laid over it after cannot
        // lighten the fill back toward what was there.
        assert_eq!(pixel(&rendered, 1, 1), REDACTION_EIGHT_BIT);
        assert_eq!(pixel(&rendered, 2, 2), REDACTION_EIGHT_BIT);
        // And the ink inside a redaction is the fill or what was drawn over it, never the
        // capture: the box is red, so no pixel anywhere is the capture's white.
        assert!(!rendered
            .pixels()
            .chunks_exact(4)
            .any(|value| value == [255; 4]));
    }

    #[test]
    fn a_mark_that_misses_the_capture_or_has_no_width_is_refused() {
        let mut edit = Edit::new(white(4, 4));
        assert!(!edit.add(Mark::Rectangle {
            rect: rect(10, 10, 2, 2),
            color: RED,
            width: 1,
            radius: 0.0,
        }));
        assert!(!edit.add(Mark::Rectangle {
            rect: rect(0, 0, 2, 2),
            color: RED,
            width: 0,
            radius: 0.0,
        }));
        assert!(!edit.add(Mark::Stroke {
            points: Vec::new(),
            color: RED,
            width: 2.0,
            style: StrokeStyle::Ink,
        }));
        assert!(!edit.can_undo());
    }

    #[test]
    fn painting_one_mark_into_a_part_of_the_capture_touches_only_that_part() {
        // A window redraws one mark into its preview; area offsets and clipping must agree with
        // a full render.
        let mut whole = vec![u8::MAX; 8 * 8 * 4];
        let mark = Mark::Stroke {
            points: vec![(0.0, 0.0), (8.0, 8.0)],
            color: RED,
            width: 2.0,
            style: StrokeStyle::Ink,
        };
        paint_mark(
            &mark,
            &mut whole,
            32,
            rect(0, 0, 8, 8),
            PixelFormat::Bgra8Unorm,
            FrameAlpha::Opaque,
        );
        let mut part = vec![u8::MAX; 4 * 4 * 4];
        paint_mark(
            &mark,
            &mut part,
            16,
            rect(2, 2, 4, 4),
            PixelFormat::Bgra8Unorm,
            FrameAlpha::Opaque,
        );
        for y in 0..4 {
            for x in 0..4 {
                let from_whole = &whole[((y + 2) * 8 + x + 2) * 4..((y + 2) * 8 + x + 2) * 4 + 4];
                let from_part = &part[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
                assert_eq!(from_whole, from_part, "pixel {x},{y}");
            }
        }
    }
}
