//! Reading a recording back, for reviewing it before it is saved (ADR 0012).
//!
//! A recording is read back from its draft file rather than held in memory: ADR 0011 keeps a
//! recording's memory bounded by its encoder, and a review that kept every frame would undo that.
//! [`RecordingFrames`] is what a reader offers - the recording's [`Timeline`] and any frame of it
//! as BGRA pixels - and [`GifReader`] is the reader for GIF; MP4's is in `captastic-windows`,
//! since it is Media Foundation's.
//!
//! [`RecordingEdit`] is what a review can change, a trim and a crop, and the arithmetic of
//! applying it: which frames are kept, when each is shown, and which pixels of it.

use std::time::Duration;

use thiserror::Error;

/// Composited frames are kept every so often so a jump backwards does not replay the recording
/// from its start, with no more of them than fits in this many bytes.
const SNAPSHOT_BUDGET_BYTES: usize = 192 * 1024 * 1024;
/// How far apart snapshots are at least, in frames: replaying a few compressed frames is cheap.
const MINIMUM_SNAPSHOT_SPACING: usize = 8;

#[derive(Debug, Error, Clone, Eq, PartialEq)]
#[error("{0}")]
pub struct PlaybackError(pub String);

/// When each frame of a recording starts, and when the recording ends. Frame 0 starts at zero.
///
/// A recording's frames are not evenly spaced: a frame that changed nothing was not written, and
/// the frame before it was shown for longer instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Timeline {
    starts: Vec<Duration>,
    duration: Duration,
}

impl Timeline {
    /// `starts` must begin at zero and rise; `duration` must be after the last of them.
    pub fn new(starts: Vec<Duration>, duration: Duration) -> Result<Self, PlaybackError> {
        if starts.first() != Some(&Duration::ZERO) {
            return Err(PlaybackError(
                "a recording's first frame must start at zero".to_owned(),
            ));
        }
        if starts.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(PlaybackError(
                "a recording's frames must start in order".to_owned(),
            ));
        }
        if starts.last().is_some_and(|last| *last >= duration) {
            return Err(PlaybackError(
                "a recording must end after its last frame starts".to_owned(),
            ));
        }
        Ok(Self { starts, duration })
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn start(&self, index: usize) -> Duration {
        self.starts[index]
    }

    /// When frame `index` stops being shown: the next frame's start, or the end.
    pub fn end(&self, index: usize) -> Duration {
        self.starts.get(index + 1).copied().unwrap_or(self.duration)
    }

    /// The frame on screen at `at`. At or after the end, the last frame.
    pub fn index_at(&self, at: Duration) -> usize {
        self.starts.partition_point(|start| *start <= at).max(1) - 1
    }

    /// The last change on screen before `at`: the start of the frame shown at `at` if `at` is
    /// past it, or else the start of the frame before.
    pub fn previous_change(&self, at: Duration) -> Duration {
        let index = self.index_at(at);
        if self.starts[index] < at {
            self.starts[index]
        } else {
            self.starts[index.saturating_sub(1)]
        }
    }

    /// The next change on screen after `at`, or the end when there is none.
    pub fn next_change(&self, at: Duration) -> Duration {
        let index = self.index_at(at);
        self.starts
            .get(index + 1)
            .copied()
            .unwrap_or(self.duration)
            .max(at.min(self.duration))
    }
}

/// A recording that can be played back: its size, its timeline, and any frame of it.
pub trait RecordingFrames {
    /// Width and height in pixels; every frame is this size.
    fn size(&self) -> (u32, u32);
    fn timeline(&self) -> &Timeline;
    /// Frame `index`, as tightly packed opaque BGRA pixels. Reading frames in order is the fast
    /// case; any other order is allowed.
    fn frame(&mut self, index: usize) -> Result<&[u8], PlaybackError>;
}

/// A rectangle of a recording's pixels, right and bottom exclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// What a review changed: the span kept and the rectangle kept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingEdit {
    /// Where the kept span starts.
    pub start: Duration,
    /// Where it ends; after `start`.
    pub end: Duration,
    /// The pixels kept, or `None` for all of them.
    pub crop: Option<PixelRect>,
}

/// One frame of an edited recording: which source frame, and when it is shown in the result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeptFrame {
    pub index: usize,
    pub at: Duration,
}

impl RecordingEdit {
    /// The edit that changes nothing.
    pub fn none(duration: Duration) -> Self {
        Self {
            start: Duration::ZERO,
            end: duration,
            crop: None,
        }
    }

    /// Whether saving with this edit is saving the recording as it is.
    pub fn changes_nothing(&self, timeline: &Timeline, size: (u32, u32)) -> bool {
        let whole_crop = self.crop.is_none_or(|crop| {
            crop == PixelRect {
                x: 0,
                y: 0,
                width: size.0,
                height: size.1,
            }
        });
        self.start == Duration::ZERO && self.end >= timeline.duration() && whole_crop
    }

    /// How long the result lasts.
    pub fn length(&self) -> Duration {
        self.end.saturating_sub(self.start)
    }

    /// The frames on screen during the kept span, each with when it appears in the result. The
    /// first appears at zero even when the span starts partway through it.
    pub fn kept_frames(&self, timeline: &Timeline) -> Vec<KeptFrame> {
        if self.end <= self.start || timeline.is_empty() {
            return Vec::new();
        }
        let first = timeline.index_at(self.start);
        (first..timeline.len())
            .take_while(|&index| timeline.start(index) < self.end)
            .map(|index| KeptFrame {
                index,
                at: timeline.start(index).max(self.start) - self.start,
            })
            .collect()
    }

    /// The size of the result.
    pub fn output_size(&self, size: (u32, u32)) -> (u32, u32) {
        self.crop.map_or(size, |crop| (crop.width, crop.height))
    }

    /// The kept pixels of one `width`-wide BGRA frame.
    pub fn crop_pixels(&self, pixels: &[u8], width: u32) -> Vec<u8> {
        let Some(crop) = self.crop else {
            return pixels.to_vec();
        };
        let row_bytes = width as usize * 4;
        let crop_bytes = crop.width as usize * 4;
        let mut out = Vec::with_capacity(crop_bytes * crop.height as usize);
        for y in crop.y..crop.y + crop.height {
            let start = y as usize * row_bytes + crop.x as usize * 4;
            out.extend_from_slice(&pixels[start..start + crop_bytes]);
        }
        out
    }
}

/// A frame of a GIF as it is stored: still LZW-compressed, with where it goes.
struct StoredFrame {
    frame: gif::Frame<'static>,
}

/// Reads an animated GIF back for playback, holding its frames compressed and decoding them as
/// they are asked for. Memory is the file's size, one composited frame and a bounded number of
/// snapshots, however long the GIF is.
///
/// Frames are drawn over one another, as a GIF viewer draws them. "Restore to previous" disposal,
/// which Captastic's writer never uses, is treated as keeping the frame.
pub struct GifReader {
    width: u32,
    height: u32,
    frames: Vec<StoredFrame>,
    timeline: Timeline,
    decoder: gif::streaming_decoder::FrameDecoder,
    /// Decoded RGBA for the frame being drawn, reused between frames.
    scratch: Vec<u8>,
    /// The composited picture after `canvas_index`, as BGRA.
    canvas: Vec<u8>,
    canvas_index: Option<usize>,
    /// Snapshots of the canvas after every `snapshot_spacing`th frame, taken as they are passed.
    snapshots: Vec<Option<Vec<u8>>>,
    snapshot_spacing: usize,
}

impl GifReader {
    /// Reads `bytes`, a whole GIF, into compressed frames and their timeline.
    pub fn new(bytes: &[u8]) -> Result<Self, PlaybackError> {
        let decoding =
            |error: gif::DecodingError| PlaybackError(format!("reading the GIF: {error}"));
        let mut options = gif::DecodeOptions::new();
        options.skip_frame_decoding(true);
        options.set_memory_limit(gif::MemoryLimit::Unlimited);
        let mut decoder = options.read_info(bytes).map_err(decoding)?;
        let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
        let global_palette = decoder.global_palette().map(<[u8]>::to_vec);
        let mut frames = Vec::new();
        let mut starts = Vec::new();
        let mut elapsed_cs = 0_u64;
        while let Some(frame) = decoder.read_next_frame().map_err(decoding)? {
            starts.push(Duration::from_millis(elapsed_cs * 10));
            // A zero delay is shown by viewers as a short one; counted as one centisecond so the
            // timeline still rises.
            elapsed_cs += u64::from(frame.delay.max(1));
            frames.push(StoredFrame {
                frame: frame.clone(),
            });
        }
        if frames.is_empty() || width == 0 || height == 0 {
            return Err(PlaybackError("the GIF has no frames".to_owned()));
        }
        let timeline = Timeline::new(starts, Duration::from_millis(elapsed_cs * 10))?;
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        options.set_memory_limit(gif::MemoryLimit::Unlimited);
        let mut frame_decoder = gif::streaming_decoder::FrameDecoder::new(options);
        if let Some(palette) = global_palette {
            frame_decoder.set_global_palette(palette);
        }
        let frame_bytes = width as usize * height as usize * 4;
        let snapshot_spacing = (frames.len() * frame_bytes)
            .div_ceil(SNAPSHOT_BUDGET_BYTES)
            .max(MINIMUM_SNAPSHOT_SPACING);
        let snapshot_count = frames.len().div_ceil(snapshot_spacing);
        Ok(Self {
            width,
            height,
            frames,
            timeline,
            decoder: frame_decoder,
            scratch: Vec::new(),
            canvas: opaque_black(frame_bytes),
            canvas_index: None,
            snapshots: vec![None; snapshot_count],
            snapshot_spacing,
        })
    }

    /// Draws frame `index` over the canvas, which holds frame `index - 1` (or nothing, for 0).
    fn apply(&mut self, index: usize) -> Result<(), PlaybackError> {
        if index > 0 {
            let previous = &self.frames[index - 1].frame;
            if previous.dispose == gif::DisposalMethod::Background {
                fill_rect(&mut self.canvas, self.width, self.height, previous);
            }
        }
        let frame = &self.frames[index].frame;
        let size = self.decoder.buffer_size(frame);
        self.scratch.resize(size, 0);
        self.decoder
            .decode_lzw_encoded_frame_into_buffer(frame, &mut self.scratch)
            .map_err(|error| PlaybackError(format!("decoding GIF frame {index}: {error}")))?;
        let (left, top) = (u32::from(frame.left), u32::from(frame.top));
        let (frame_width, frame_height) = (u32::from(frame.width), u32::from(frame.height));
        for y in 0..frame_height {
            let canvas_y = top + y;
            if canvas_y >= self.height {
                break;
            }
            for x in 0..frame_width {
                let canvas_x = left + x;
                if canvas_x >= self.width {
                    break;
                }
                let from = (y * frame_width + x) as usize * 4;
                let rgba = &self.scratch[from..from + 4];
                if rgba[3] == 0 {
                    continue;
                }
                let to = (canvas_y * self.width + canvas_x) as usize * 4;
                self.canvas[to..to + 4].copy_from_slice(&[rgba[2], rgba[1], rgba[0], u8::MAX]);
            }
        }
        self.canvas_index = Some(index);
        if index.is_multiple_of(self.snapshot_spacing) {
            let slot = index / self.snapshot_spacing;
            if self.snapshots[slot].is_none() {
                self.snapshots[slot] = Some(self.canvas.clone());
            }
        }
        Ok(())
    }
}

impl RecordingFrames for GifReader {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    fn frame(&mut self, index: usize) -> Result<&[u8], PlaybackError> {
        if index >= self.frames.len() {
            return Err(PlaybackError(format!(
                "frame {index} is past the GIF's {} frames",
                self.frames.len()
            )));
        }
        let resume = match self.canvas_index {
            Some(current) if current == index => return Ok(&self.canvas),
            // Forwards from where the canvas is, unless a snapshot is nearer.
            Some(current) if current < index => Some(current),
            _ => None,
        };
        let nearest_snapshot = (0..=index / self.snapshot_spacing)
            .rev()
            .find(|&slot| self.snapshots[slot].is_some())
            .map(|slot| slot * self.snapshot_spacing);
        let from = match (resume, nearest_snapshot) {
            (Some(current), Some(snapshot)) if snapshot <= current => current + 1,
            (_, Some(snapshot)) => {
                let slot = snapshot / self.snapshot_spacing;
                if let Some(pixels) = &self.snapshots[slot] {
                    self.canvas.copy_from_slice(pixels);
                }
                self.canvas_index = Some(snapshot);
                snapshot + 1
            }
            (Some(current), None) => current + 1,
            (None, None) => {
                self.canvas = opaque_black(self.canvas.len());
                0
            }
        };
        for next in from..=index {
            self.apply(next)?;
        }
        Ok(&self.canvas)
    }
}

fn opaque_black(bytes: usize) -> Vec<u8> {
    let mut pixels = vec![0_u8; bytes];
    for pixel in pixels.chunks_exact_mut(4) {
        pixel[3] = u8::MAX;
    }
    pixels
}

/// Clears `frame`'s rectangle of the canvas to black, for "restore to background" disposal.
fn fill_rect(canvas: &mut [u8], width: u32, height: u32, frame: &gif::Frame<'_>) {
    let right = (u32::from(frame.left) + u32::from(frame.width)).min(width);
    let bottom = (u32::from(frame.top) + u32::from(frame.height)).min(height);
    for y in u32::from(frame.top)..bottom {
        for x in u32::from(frame.left)..right {
            let at = (y * width + x) as usize * 4;
            canvas[at..at + 4].copy_from_slice(&[0, 0, 0, u8::MAX]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GifWriter;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn timeline() -> Timeline {
        Timeline::new(vec![ms(0), ms(100), ms(1_000)], ms(1_500)).expect("timeline")
    }

    #[test]
    fn a_timeline_finds_the_frame_on_screen_and_the_changes_around_it() {
        let timeline = timeline();
        assert_eq!(timeline.index_at(ms(0)), 0);
        assert_eq!(timeline.index_at(ms(99)), 0);
        assert_eq!(timeline.index_at(ms(100)), 1);
        assert_eq!(timeline.index_at(ms(5_000)), 2);
        assert_eq!(timeline.end(1), ms(1_000));
        assert_eq!(timeline.end(2), ms(1_500));
        assert_eq!(timeline.previous_change(ms(500)), ms(100));
        assert_eq!(timeline.previous_change(ms(100)), ms(0));
        assert_eq!(timeline.previous_change(ms(0)), ms(0));
        assert_eq!(timeline.next_change(ms(500)), ms(1_000));
        assert_eq!(timeline.next_change(ms(1_200)), ms(1_500));
    }

    #[test]
    fn a_timeline_must_start_at_zero_rise_and_end_after_its_last_frame() {
        assert!(Timeline::new(vec![ms(10)], ms(20)).is_err());
        assert!(Timeline::new(vec![ms(0), ms(50), ms(50)], ms(90)).is_err());
        assert!(Timeline::new(vec![ms(0), ms(50)], ms(50)).is_err());
        assert!(Timeline::new(Vec::new(), ms(50)).is_err());
    }

    #[test]
    fn a_trim_keeps_the_frames_on_screen_in_its_span_and_starts_them_at_zero() {
        let edit = RecordingEdit {
            start: ms(400),
            end: ms(1_200),
            crop: None,
        };
        // Frame 1 was already on screen at 400 ms, so it opens the result; frame 2 follows 600 ms
        // later.
        assert_eq!(
            edit.kept_frames(&timeline()),
            vec![
                KeptFrame {
                    index: 1,
                    at: ms(0)
                },
                KeptFrame {
                    index: 2,
                    at: ms(600)
                },
            ]
        );
        assert_eq!(edit.length(), ms(800));
        // A span ending before frame 2 starts leaves it out.
        let edit = RecordingEdit {
            start: ms(0),
            end: ms(1_000),
            crop: None,
        };
        assert_eq!(
            edit.kept_frames(&timeline())
                .iter()
                .map(|kept| kept.index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn only_the_whole_span_and_the_whole_picture_change_nothing() {
        let timeline = timeline();
        let whole = RecordingEdit::none(timeline.duration());
        assert!(whole.changes_nothing(&timeline, (40, 30)));
        let full_crop = RecordingEdit {
            crop: Some(PixelRect {
                x: 0,
                y: 0,
                width: 40,
                height: 30,
            }),
            ..whole
        };
        assert!(full_crop.changes_nothing(&timeline, (40, 30)));
        let trimmed = RecordingEdit {
            start: ms(10),
            ..whole
        };
        assert!(!trimmed.changes_nothing(&timeline, (40, 30)));
        let cropped = RecordingEdit {
            crop: Some(PixelRect {
                x: 1,
                y: 0,
                width: 39,
                height: 30,
            }),
            ..whole
        };
        assert!(!cropped.changes_nothing(&timeline, (40, 30)));
    }

    #[test]
    fn a_crop_keeps_its_rectangle_of_pixels() {
        // A 3x2 frame whose pixels are numbered 0 to 5 in their blue channel.
        let pixels: Vec<u8> = (0..6).flat_map(|value| [value, 0, 0, 255]).collect();
        let edit = RecordingEdit {
            start: ms(0),
            end: ms(1),
            crop: Some(PixelRect {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            }),
        };
        let kept: Vec<u8> = edit
            .crop_pixels(&pixels, 3)
            .chunks_exact(4)
            .map(|pixel| pixel[0])
            .collect();
        assert_eq!(kept, vec![1, 2, 4, 5]);
        assert_eq!(edit.output_size((3, 2)), (2, 2));
    }

    fn solid(width: u32, height: u32, bgr: [u8; 3]) -> Vec<u8> {
        (0..width * height)
            .flat_map(|_| [bgr[0], bgr[1], bgr[2], 255])
            .collect()
    }

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
        let at = ((y * width + x) * 4) as usize;
        [pixels[at], pixels[at + 1], pixels[at + 2]]
    }

    /// A GIF of 40 frames, 100 ms apart, where frame `n` paints pixel (n, 0) white over grey, so
    /// each frame's picture says which frame it is.
    fn counting_gif() -> (Vec<u8>, u32, u32) {
        let (width, height) = (48, 8);
        let mut writer = GifWriter::new(Vec::new(), width, height).expect("writer");
        let mut picture = solid(width, height, [90, 90, 90]);
        for n in 0..40_u32 {
            let at = (n * 4) as usize;
            picture[at..at + 3].copy_from_slice(&[255, 255, 255]);
            writer
                .push(&picture, Duration::from_millis(u64::from(n) * 100))
                .expect("push");
        }
        let (bytes, _) = writer.finish(Duration::from_secs(4)).expect("finish");
        (bytes, width, height)
    }

    /// How many of the first row's pixels are white: the frame number plus one.
    fn white_count(pixels: &[u8], width: u32) -> u32 {
        (0..width)
            .filter(|&x| pixel(pixels, width, x, 0) == [255, 255, 255])
            .count() as u32
    }

    #[test]
    fn a_gif_reads_back_with_its_timing_and_pictures_in_any_order() {
        let (bytes, width, _) = counting_gif();
        let mut reader = GifReader::new(&bytes).expect("reader");
        assert_eq!(reader.size(), (48, 8));
        let timeline = reader.timeline().clone();
        assert_eq!(timeline.len(), 40);
        assert_eq!(timeline.start(7), ms(700));
        assert_eq!(timeline.duration(), ms(4_000));
        // In order, then backwards, then jumping: each picture is its own frame's.
        for index in (0..40).chain((0..40).rev()).chain([3, 31, 9, 39, 0]) {
            let frame = reader.frame(index).expect("frame");
            assert_eq!(white_count(frame, width), index as u32 + 1, "frame {index}");
            assert_eq!(pixel(frame, width, 47, 7), [90, 90, 90]);
        }
        assert!(reader.frame(40).is_err());
    }

    #[test]
    fn a_still_stretch_reads_back_as_one_long_frame() {
        let (width, height) = (8, 8);
        let mut writer = GifWriter::new(Vec::new(), width, height).expect("writer");
        let grey = solid(width, height, [90, 90, 90]);
        let blue = solid(width, height, [200, 0, 0]);
        for step in 0..20_u64 {
            let picture = if step < 15 { &grey } else { &blue };
            writer.push(picture, ms(step * 100)).expect("push");
        }
        let (bytes, _) = writer.finish(ms(2_000)).expect("finish");
        let mut reader = GifReader::new(&bytes).expect("reader");
        let timeline = reader.timeline().clone();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline.start(1), ms(1_500));
        // A cut in the middle of the still stretch is a place like any other.
        let edit = RecordingEdit {
            start: ms(700),
            end: ms(1_800),
            crop: None,
        };
        let kept = edit.kept_frames(&timeline);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[1].at, ms(800));
        assert_eq!(
            pixel(reader.frame(1).expect("frame"), width, 3, 3),
            [200, 0, 0]
        );
    }

    #[test]
    fn something_that_is_not_a_gif_is_refused() {
        assert!(GifReader::new(b"not a gif").is_err());
    }
}
