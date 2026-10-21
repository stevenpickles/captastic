//! Animated GIF for screen recordings (ADR 0011).
//!
//! [`GifWriter`] takes frames as they are captured and writes them as it goes, so a recording's
//! memory is one frame and the previous one, however long it runs. GIF stores up to 256 colours a
//! frame, and nothing like a video codec's motion search, so the writer keeps files small the way
//! GIF allows:
//!
//! - **Only what changed is written.** Each frame after the first covers just the bounding box of
//!   the pixels that differ from the frame before, and inside it an unchanged pixel is transparent,
//!   which costs almost nothing once compressed. A frame where nothing changed is not written at
//!   all; the frame before it is shown for longer instead.
//! - **Timing comes from the capture, not a nominal rate.** A frame's delay is the time until the
//!   next frame that changed anything, kept in absolute centiseconds so rounding never accumulates.
//! - **Each frame has its own palette**, built by NeuQuant (the `color_quant` crate, MIT), with no
//!   dithering, since dithering changes unchanged pixels and defeats the transparency above.

use std::io::Write;
use std::time::Duration;

use thiserror::Error;

/// The shortest delay written. Browsers and most viewers treat 0 and 1 centisecond as "as fast as
/// possible" and substitute 10, so a frame that came sooner than this is held for 2.
const MINIMUM_DELAY_CS: u64 = 2;
/// NeuQuant's sampling factor: 1 is slowest and best, 30 fastest. 10 is the crate's suggested
/// compromise, and a screen's flat colours lose little to it.
const QUANTIZER_SPEED: i32 = 10;

#[derive(Debug, Error)]
pub enum AnimationError {
    #[error("a GIF frame is at most 65535 pixels a side, not {width}x{height}")]
    TooLarge { width: u32, height: u32 },
    #[error("expected {expected} bytes of BGRA pixels, got {actual}")]
    WrongSize { expected: usize, actual: usize },
    #[error("writing the GIF failed: {0}")]
    Encoding(String),
}

impl From<gif::EncodingError> for AnimationError {
    fn from(error: gif::EncodingError) -> Self {
        Self::Encoding(error.to_string())
    }
}

/// What a finished GIF contains.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnimationSummary {
    /// Frames pushed.
    pub frames_received: u64,
    /// Frames written; the rest changed nothing and lengthened the frame before them.
    pub frames_written: u64,
    /// From the first frame to the end given to [`GifWriter::finish`].
    pub duration: Duration,
}

/// A frame waiting for the next change to learn how long it is shown.
struct Pending {
    frame: gif::Frame<'static>,
}

/// Writes an animated GIF one captured frame at a time.
pub struct GifWriter<W: Write> {
    encoder: gif::Encoder<W>,
    width: u32,
    height: u32,
    /// The BGRA pixels of the last frame pushed, to find what the next one changed.
    previous: Option<Vec<u8>>,
    pending: Option<Pending>,
    /// Centiseconds already written into earlier frames' delays.
    written_cs: u64,
    first_at: Option<Duration>,
    summary: AnimationSummary,
}

impl<W: Write> GifWriter<W> {
    /// Starts a looping GIF of `width` by `height` pixels.
    pub fn new(writer: W, width: u32, height: u32) -> Result<Self, AnimationError> {
        let (Ok(gif_width), Ok(gif_height)) = (u16::try_from(width), u16::try_from(height)) else {
            return Err(AnimationError::TooLarge { width, height });
        };
        if width == 0 || height == 0 {
            return Err(AnimationError::TooLarge { width, height });
        }
        let mut encoder = gif::Encoder::new(writer, gif_width, gif_height, &[])?;
        encoder.set_repeat(gif::Repeat::Infinite)?;
        Ok(Self {
            encoder,
            width,
            height,
            previous: None,
            pending: None,
            written_cs: 0,
            first_at: None,
            summary: AnimationSummary::default(),
        })
    }

    /// Adds a frame of tightly packed BGRA pixels, captured `at` after the recording started.
    /// Alpha is ignored: a recording is of the screen, which is opaque.
    pub fn push(&mut self, bgra: &[u8], at: Duration) -> Result<(), AnimationError> {
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() != expected {
            return Err(AnimationError::WrongSize {
                expected,
                actual: bgra.len(),
            });
        }
        self.summary.frames_received += 1;
        let first_at = *self.first_at.get_or_insert(at);
        let shown_at = at.saturating_sub(first_at);
        let changed = match &self.previous {
            None => Some(Box {
                left: 0,
                top: 0,
                right: self.width,
                bottom: self.height,
            }),
            Some(previous) => changed_box(previous, bgra, self.width, self.height),
        };
        let Some(region) = changed else {
            // Nothing moved: the frame on screen simply stays up longer.
            return Ok(());
        };
        let frame = self.frame_for(bgra, region);
        self.previous = Some(bgra.to_vec());
        if let Some(pending) = self.pending.take() {
            self.write(pending, shown_at)?;
        }
        self.pending = Some(Pending { frame });
        Ok(())
    }

    /// Writes the last frame, shown until `end` after the recording started, and closes the file.
    pub fn finish(mut self, end: Duration) -> Result<(W, AnimationSummary), AnimationError> {
        let first_at = self.first_at.unwrap_or(end);
        let end = end.saturating_sub(first_at);
        if let Some(pending) = self.pending.take() {
            self.write(pending, end)?;
        }
        self.summary.duration = end;
        let summary = self.summary;
        Ok((self.encoder.into_inner()?, summary))
    }

    /// Writes `pending`, shown until `until`.
    fn write(&mut self, mut pending: Pending, until: Duration) -> Result<(), AnimationError> {
        let until_cs = (until.as_millis() / 10) as u64;
        let delay = until_cs
            .saturating_sub(self.written_cs)
            .max(MINIMUM_DELAY_CS);
        self.written_cs += delay;
        pending.frame.delay = u16::try_from(delay).unwrap_or(u16::MAX);
        self.encoder.write_frame(&pending.frame)?;
        self.summary.frames_written += 1;
        Ok(())
    }

    /// The GIF frame for the pixels of `bgra` inside `region`, with every pixel unchanged since
    /// the previous frame left transparent.
    fn frame_for(&self, bgra: &[u8], region: Box) -> gif::Frame<'static> {
        let width = self.width as usize;
        let (region_width, region_height) = (region.width() as usize, region.height() as usize);
        let mut rgba = Vec::with_capacity(region_width * region_height * 4);
        for y in region.top as usize..region.bottom as usize {
            for x in region.left as usize..region.right as usize {
                let at = (y * width + x) * 4;
                let unchanged = self
                    .previous
                    .as_ref()
                    .is_some_and(|previous| previous[at..at + 3] == bgra[at..at + 3]);
                if unchanged {
                    rgba.extend_from_slice(&[0, 0, 0, 0]);
                } else {
                    rgba.extend_from_slice(&[bgra[at + 2], bgra[at + 1], bgra[at], u8::MAX]);
                }
            }
        }
        let mut frame = gif::Frame::from_rgba_speed(
            region.width() as u16,
            region.height() as u16,
            &mut rgba,
            QUANTIZER_SPEED,
        );
        frame.left = region.left as u16;
        frame.top = region.top as u16;
        // Each frame draws over the last, which is what makes its transparent pixels mean
        // "unchanged".
        frame.dispose = gif::DisposalMethod::Keep;
        frame
    }
}

/// A pixel rectangle, right and bottom exclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Box {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl Box {
    fn width(self) -> u32 {
        self.right - self.left
    }

    fn height(self) -> u32 {
        self.bottom - self.top
    }
}

/// The smallest box holding every pixel whose colour differs between the two frames, or `None`
/// when none does.
fn changed_box(previous: &[u8], current: &[u8], width: u32, height: u32) -> Option<Box> {
    let row_bytes = width as usize * 4;
    let mut found: Option<Box> = None;
    for y in 0..height {
        let start = y as usize * row_bytes;
        let (before, after) = (
            &previous[start..start + row_bytes],
            &current[start..start + row_bytes],
        );
        if before == after {
            continue;
        }
        let mut first = None;
        let mut last = 0;
        for x in 0..width as usize {
            if before[x * 4..x * 4 + 3] != after[x * 4..x * 4 + 3] {
                first.get_or_insert(x);
                last = x;
            }
        }
        let Some(first) = first else { continue };
        let (first, last) = (first as u32, last as u32 + 1);
        found = Some(match found {
            None => Box {
                left: first,
                top: y,
                right: last,
                bottom: y + 1,
            },
            Some(found) => Box {
                left: found.left.min(first),
                top: found.top,
                right: found.right.max(last),
                bottom: y + 1,
            },
        });
    }
    found
}

/// The size `width` by `height` scaled down, keeping its shape, so neither side exceeds `limit`.
/// Never scales up, and never below one pixel.
pub fn fit_within(width: u32, height: u32, limit: u32) -> (u32, u32) {
    let longer = width.max(height);
    if longer <= limit || limit == 0 {
        return (width, height);
    }
    let scale = f64::from(limit) / f64::from(longer);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// Shrinks BGRA pixels (`stride` bytes a row) to `to_width` by `to_height` by averaging the source
/// pixels each destination pixel covers, which keeps text legible where sampling would drop
/// strokes. The result is tightly packed.
pub fn shrink_bgra(
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    to_width: u32,
    to_height: u32,
) -> Vec<u8> {
    let mut out = vec![0_u8; to_width as usize * to_height as usize * 4];
    let x_step = f64::from(width) / f64::from(to_width.max(1));
    let y_step = f64::from(height) / f64::from(to_height.max(1));
    for y in 0..to_height as usize {
        let top = (y as f64 * y_step) as usize;
        let bottom = (((y + 1) as f64 * y_step).ceil() as usize).clamp(top + 1, height as usize);
        for x in 0..to_width as usize {
            let left = (x as f64 * x_step) as usize;
            let right = (((x + 1) as f64 * x_step).ceil() as usize).clamp(left + 1, width as usize);
            let mut sum = [0_u32; 4];
            for source_y in top..bottom {
                let row = &pixels[source_y * stride..];
                for source_x in left..right {
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(row[source_x * 4 + channel]);
                    }
                }
            }
            let count = ((bottom - top) * (right - left)) as u32;
            let at = (y * to_width as usize + x) * 4;
            for channel in 0..4 {
                out[at + channel] = ((sum[channel] + count / 2) / count) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `width` by `height` BGRA frame filled with one colour.
    fn solid(width: u32, height: u32, bgr: [u8; 3]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            pixels.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
        }
        pixels
    }

    fn set(pixels: &mut [u8], width: u32, x: u32, y: u32, bgr: [u8; 3]) {
        let at = ((y * width + x) * 4) as usize;
        pixels[at..at + 3].copy_from_slice(&bgr);
    }

    /// Decodes a GIF into its frames' placement, size and delay, and the composited canvas after
    /// the last frame.
    /// Each frame's left, top, width, height and delay.
    type Placements = Vec<(u16, u16, u16, u16, u16)>;

    fn decode(bytes: &[u8]) -> (Placements, Vec<u8>, u16, u16) {
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = options.read_info(bytes).expect("a valid GIF");
        let (width, height) = (decoder.width(), decoder.height());
        let mut canvas = vec![0_u8; width as usize * height as usize * 4];
        let mut frames = Vec::new();
        while let Some(frame) = decoder.read_next_frame().expect("a valid frame") {
            frames.push((
                frame.left,
                frame.top,
                frame.width,
                frame.height,
                frame.delay,
            ));
            for y in 0..frame.height as usize {
                for x in 0..frame.width as usize {
                    let source = (y * frame.width as usize + x) * 4;
                    if frame.buffer[source + 3] == 0 {
                        continue;
                    }
                    let target =
                        ((frame.top as usize + y) * width as usize + frame.left as usize + x) * 4;
                    canvas[target..target + 4].copy_from_slice(&frame.buffer[source..source + 4]);
                }
            }
        }
        (frames, canvas, width, height)
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn a_still_screen_is_one_frame_shown_for_the_whole_recording() {
        let mut writer = GifWriter::new(Vec::new(), 8, 4).expect("writer");
        let frame = solid(8, 4, [10, 20, 30]);
        for step in 0..10 {
            writer.push(&frame, ms(step * 66)).expect("push");
        }
        let (bytes, summary) = writer.finish(ms(2_000)).expect("finish");
        assert_eq!(summary.frames_received, 10);
        assert_eq!(summary.frames_written, 1);
        assert_eq!(summary.duration, ms(2_000));
        let (frames, canvas, width, height) = decode(&bytes);
        assert_eq!((width, height), (8, 4));
        assert_eq!(frames, vec![(0, 0, 8, 4, 200)]);
        assert_eq!(&canvas[0..4], &[30, 20, 10, 255]);
    }

    #[test]
    fn a_change_is_written_as_just_the_box_it_covers() {
        let mut writer = GifWriter::new(Vec::new(), 10, 10).expect("writer");
        let first = solid(10, 10, [255, 255, 255]);
        let mut second = first.clone();
        set(&mut second, 10, 3, 4, [0, 0, 255]);
        set(&mut second, 10, 6, 7, [0, 0, 255]);
        writer.push(&first, ms(0)).expect("push");
        writer.push(&second, ms(100)).expect("push");
        let (bytes, summary) = writer.finish(ms(300)).expect("finish");
        assert_eq!(summary.frames_written, 2);
        let (frames, canvas, _, _) = decode(&bytes);
        assert_eq!(frames[0], (0, 0, 10, 10, 10));
        // Only the box from (3, 4) to (6, 7) inclusive, shown until the end.
        assert_eq!(frames[1], (3, 4, 4, 4, 20));
        // The composited result is the second frame: the change, and white everywhere else.
        let pixel = |x: usize, y: usize| canvas[(y * 10 + x) * 4..(y * 10 + x) * 4 + 4].to_vec();
        assert_eq!(pixel(3, 4), vec![255, 0, 0, 255]);
        assert_eq!(pixel(6, 7), vec![255, 0, 0, 255]);
        assert_eq!(pixel(4, 5), vec![255, 255, 255, 255]);
        assert_eq!(pixel(0, 0), vec![255, 255, 255, 255]);
    }

    #[test]
    fn delays_follow_the_capture_times_without_drifting() {
        let mut writer = GifWriter::new(Vec::new(), 2, 1).expect("writer");
        // Frames 66.6 ms apart, as at 15 frames a second: 6.66 centiseconds each, which rounded
        // one at a time would lose a centisecond every three frames.
        for step in 0..30_u64 {
            let colour = if step % 2 == 0 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            };
            writer
                .push(&solid(2, 1, colour), Duration::from_micros(step * 66_667))
                .expect("push");
        }
        let (bytes, _) = writer.finish(ms(2_000)).expect("finish");
        let (frames, _, _, _) = decode(&bytes);
        let total: u32 = frames.iter().map(|frame| u32::from(frame.4)).sum();
        assert_eq!(frames.len(), 30);
        assert_eq!(total, 200);
    }

    #[test]
    fn frames_that_come_too_fast_are_held_for_the_shortest_delay_viewers_honour() {
        let mut writer = GifWriter::new(Vec::new(), 1, 1).expect("writer");
        writer.push(&solid(1, 1, [0, 0, 0]), ms(0)).expect("push");
        writer.push(&solid(1, 1, [9, 9, 9]), ms(5)).expect("push");
        let (bytes, _) = writer.finish(ms(5)).expect("finish");
        let (frames, _, _, _) = decode(&bytes);
        assert!(frames.iter().all(|frame| frame.4 >= 2));
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_refused() {
        let mut writer = GifWriter::new(Vec::new(), 4, 4).expect("writer");
        assert!(matches!(
            writer.push(&[0; 12], ms(0)),
            Err(AnimationError::WrongSize { .. })
        ));
        assert!(matches!(
            GifWriter::new(Vec::new(), 70_000, 10),
            Err(AnimationError::TooLarge { .. })
        ));
    }

    #[test]
    fn many_colours_still_make_a_valid_frame() {
        // More than 256 colours, so NeuQuant has to choose.
        let (width, height) = (64_u32, 64_u32);
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8, 255]);
            }
        }
        let mut writer = GifWriter::new(Vec::new(), width, height).expect("writer");
        writer.push(&pixels, ms(0)).expect("push");
        let (bytes, _) = writer.finish(ms(100)).expect("finish");
        let (_, canvas, _, _) = decode(&bytes);
        // Close to the source, though not exact.
        let at = ((32 * width + 32) * 4) as usize;
        let source = [pixels[at + 2], pixels[at + 1], pixels[at]];
        for channel in 0..3 {
            assert!(
                (i32::from(canvas[at + channel]) - i32::from(source[channel])).abs() < 24,
                "{:?} vs {:?}",
                &canvas[at..at + 3],
                source
            );
        }
    }

    #[test]
    fn fitting_never_enlarges_and_keeps_the_shape() {
        assert_eq!(fit_within(3840, 2160, 1280), (1280, 720));
        assert_eq!(fit_within(800, 600, 1280), (800, 600));
        assert_eq!(fit_within(100, 4000, 1000), (25, 1000));
        assert_eq!(fit_within(10_000, 1, 100), (100, 1));
    }

    #[test]
    fn shrinking_averages_what_each_pixel_covers() {
        // A 4x2 frame of alternating black and white columns, halved: every pixel is mid-grey.
        let mut pixels = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                let value = if x % 2 == 0 { 0 } else { 255 };
                pixels.extend_from_slice(&[value, value, value, 255]);
            }
        }
        let shrunk = shrink_bgra(&pixels, 4, 2, 16, 2, 1);
        assert_eq!(shrunk, vec![128, 128, 128, 255, 128, 128, 128, 255]);
    }
}
