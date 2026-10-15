//! Screen recording to an animated GIF or an MP4 (ADR 0011).
//!
//! [`record`] runs on the caller's thread, which owns the frame source - for the daemon, a channel
//! to its capture thread; for `captastic-dev record`, a capture backend - and asks it for a frame at
//! the format's rate. Each frame is cropped to the recorded area, sized for the format, and handed
//! to an encoder thread through a queue of a few frames. When the encoder falls behind, the oldest
//! waiting frame is dropped and counted rather than the queue growing: a recording's memory is the
//! queue's, however long it runs, and every frame carries the time it was captured, so a dropped
//! frame lengthens the one before it rather than speeding the recording up.
//!
//! A recording ends when it is asked to stop or reaches its time limit, and either way it is
//! finished: the encoder writes what it has and closes the file. A recording that fails - the
//! source gone, the encoder refusing, the disk full - removes its file, so it leaves a valid
//! recording or nothing.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use captastic_config::RecordingFormat;
use captastic_core::{CaptureError, CpuFrame, PixelEncoding, Rect};
use serde::Serialize;
use thiserror::Error;

/// What the frames waiting for the encoder may occupy. Enough for small recordings to ride out
/// the encoder's start - the first H.264 frame can take a few hundred milliseconds while the
/// hardware encoder wakes - and few enough frames of a 4K display to stay a bounded cost.
const QUEUE_BYTES: usize = 128 * 1024 * 1024;
/// However small or large the frames, the queue holds between these many.
const QUEUE_FRAMES: (usize, usize) = (2, 90);

/// How many frames of `bytes` each the queue holds.
fn queue_capacity(bytes: usize) -> usize {
    (QUEUE_BYTES / bytes.max(1)).clamp(QUEUE_FRAMES.0, QUEUE_FRAMES.1)
}
/// How long the pacing loop sleeps at a time while it waits for the next frame, so a stop request
/// is noticed promptly.
const STOP_POLL: Duration = Duration::from_millis(15);
/// Consecutive failed captures a recording rides out before it ends, so one dropped frame does not
/// cost the whole recording.
const SOURCE_FAILURES_TOLERATED: u32 = 5;
/// How many names a recording tries before giving up on finding an unused one.
const MAX_NAME_ATTEMPTS: u32 = 100;

/// Where a recording's frames come from.
pub trait FrameSource {
    /// The newest frame of the display the recording is on, whole.
    fn next_frame(&mut self) -> Result<CpuFrame, CaptureError>;
}

#[derive(Clone, Copy, Debug)]
pub struct RecordingSettings {
    pub format: RecordingFormat,
    pub max_duration: Duration,
    pub gif_max_dimension: u32,
}

impl RecordingSettings {
    pub fn from_config(config: &captastic_config::RecordingConfig) -> Self {
        Self {
            format: config.format,
            max_duration: config.max_duration(),
            gif_max_dimension: config.gif_max_dimension,
        }
    }
}

/// Why a recording that succeeded ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingEnd {
    Stopped,
    TimeLimit,
}

/// A finished recording.
#[derive(Clone, Debug, Serialize)]
pub struct RecordingReport {
    pub path: PathBuf,
    pub format: RecordingFormat,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    #[serde(serialize_with = "serialize_millis")]
    pub duration: Duration,
    /// Frames taken from the source.
    pub frames_captured: u64,
    /// Frames the file holds; the rest repeated the frame before them.
    pub frames_written: u64,
    /// Frames dropped because the encoder was behind.
    pub frames_dropped: u64,
    pub ended: RecordingEnd,
}

fn serialize_millis<S: serde::Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u128(duration.as_millis())
}

#[derive(Debug, Error)]
pub enum RecordingError {
    #[error("the recording could not capture the screen: {0}")]
    Source(CaptureError),
    #[error("the recording could not be encoded: {0}")]
    Encode(String),
    #[error("the recording could not be saved: {0}")]
    Io(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("the recorded area {area:?} is not on the display being recorded ({display:?})")]
    OutsideDisplay { area: Rect, display: Rect },
}

/// Records `area` of the desktop from `source` into `path` until `stop` is set or the time limit
/// passes. `path` should be a file this recording owns - [`reserve_path`] makes one - and is
/// replaced; on failure it is removed.
pub fn record(
    source: &mut dyn FrameSource,
    area: Rect,
    settings: RecordingSettings,
    path: &Path,
    stop: &AtomicBool,
) -> Result<RecordingReport, RecordingError> {
    let result = record_into(source, area, settings, path, stop);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn record_into(
    source: &mut dyn FrameSource,
    area: Rect,
    settings: RecordingSettings,
    path: &Path,
    stop: &AtomicBool,
) -> Result<RecordingReport, RecordingError> {
    let started = Instant::now();
    let interval = Duration::from_secs(1) / settings.format.frame_rate();
    let first = source.next_frame().map_err(RecordingError::Source)?;
    let area = recorded_area(&first, area)?;
    let (width, height) = output_size(settings, area.width, area.height);
    let queue = Arc::new(FrameQueue::with_capacity(queue_capacity(
        width as usize * height as usize * 4,
    )));
    let encoder = {
        let queue = queue.clone();
        let path = path.to_owned();
        let format = settings.format;
        thread::Builder::new()
            .name("captastic-recording-encoder".to_owned())
            .spawn(move || encode(&queue, format, &path, width, height))
            .map_err(|error| RecordingError::Encode(error.to_string()))?
    };
    queue.push(frame_pixels(&first, area, width, height)?, Duration::ZERO);
    let mut captured = 1_u64;
    let mut failures = 0_u32;
    let mut next = started + interval;
    let ended = loop {
        // Wait for the next frame's time, noticing a stop or the limit while waiting.
        let ended = loop {
            if stop.load(Ordering::Acquire) {
                break Some(RecordingEnd::Stopped);
            }
            if started.elapsed() >= settings.max_duration {
                break Some(RecordingEnd::TimeLimit);
            }
            let now = Instant::now();
            if now >= next {
                break None;
            }
            thread::sleep((next - now).min(STOP_POLL));
        };
        if let Some(ended) = ended {
            break ended;
        }
        if queue.failed() {
            break RecordingEnd::Stopped;
        }
        // Paced from the schedule, not from when the last frame arrived, so a slow capture does
        // not slow every frame after it; one that ran past a whole interval skips to the next.
        next += interval;
        let now = Instant::now();
        if next < now {
            next = now + interval;
        }
        match source.next_frame() {
            Ok(frame) => {
                failures = 0;
                captured += 1;
                let at = started.elapsed();
                queue.push(frame_pixels(&frame, area, width, height)?, at);
            }
            Err(error) => {
                failures += 1;
                log::debug!(
                    "a recording frame could not be captured ({failures} in a row): {error}"
                );
                if failures > SOURCE_FAILURES_TOLERATED {
                    queue.close(started.elapsed());
                    let _ = encoder.join();
                    return Err(RecordingError::Source(error));
                }
            }
        }
    };
    let duration = started.elapsed().min(settings.max_duration);
    queue.close(duration);
    let written = encoder
        .join()
        .map_err(|_| RecordingError::Encode("the encoder thread panicked".to_owned()))??;
    let bytes = std::fs::metadata(path)
        .map_err(|error| RecordingError::Io(error.to_string()))?
        .len();
    Ok(RecordingReport {
        path: path.to_owned(),
        format: settings.format,
        bytes,
        width,
        height,
        duration,
        frames_captured: captured,
        frames_written: written,
        frames_dropped: queue.dropped(),
        ended,
    })
}

/// The part of `area` on the frame's display, in desktop coordinates.
fn recorded_area(frame: &CpuFrame, area: Rect) -> Result<Rect, RecordingError> {
    let display = frame.metadata.source_rect;
    area.intersection(display)
        .ok_or(RecordingError::OutsideDisplay { area, display })
}

/// The size a recording of `width` by `height` is written at.
pub fn output_size(settings: RecordingSettings, width: u32, height: u32) -> (u32, u32) {
    match settings.format {
        RecordingFormat::Gif => {
            captastic_core::fit_within(width, height, settings.gif_max_dimension)
        }
        RecordingFormat::Mp4 => captastic_windows::mp4_frame_size(width, height),
    }
}

/// The recorded area of `frame` as tightly packed BGRA at `width` by `height`.
fn frame_pixels(
    frame: &CpuFrame,
    area: Rect,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, RecordingError> {
    let blue_first = match frame.format().encoding() {
        PixelEncoding::EightBitRgba { blue_first } => blue_first,
        PixelEncoding::HalfFloatRgba => {
            return Err(RecordingError::Unsupported(
                "recording an HDR display is not supported yet".to_owned(),
            ))
        }
    };
    let area =
        area.intersection(frame.metadata.source_rect)
            .ok_or(RecordingError::OutsideDisplay {
                area,
                display: frame.metadata.source_rect,
            })?;
    let cropped = frame
        .crop(area)
        .map_err(|error| RecordingError::Encode(error.to_string()))?;
    let mut pixels = cropped.pixels().to_vec();
    if !blue_first {
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    let (from_width, from_height) = (cropped.width(), cropped.height());
    if (width, height) == (from_width, from_height) {
        return Ok(pixels);
    }
    if width <= from_width
        && height <= from_height
        && from_width - width <= 1
        && from_height - height <= 1
    {
        // Only trimmed to even sides for video: drop the last column or row rather than resample.
        let row = width as usize * 4;
        let stride = from_width as usize * 4;
        let mut trimmed = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            trimmed.extend_from_slice(&pixels[y * stride..y * stride + row]);
        }
        return Ok(trimmed);
    }
    Ok(captastic_core::shrink_bgra(
        &pixels,
        from_width,
        from_height,
        from_width as usize * 4,
        width,
        height,
    ))
}

/// The encoder thread: everything the queue hands it, into `path`, until the queue closes.
/// Returns how many frames the file holds.
fn encode(
    queue: &FrameQueue,
    format: RecordingFormat,
    path: &Path,
    width: u32,
    height: u32,
) -> Result<u64, RecordingError> {
    let result = match format {
        RecordingFormat::Gif => encode_gif(queue, path, width, height),
        RecordingFormat::Mp4 => encode_mp4(queue, path, width, height),
    };
    if result.is_err() {
        queue.fail();
    }
    result
}

fn encode_gif(
    queue: &FrameQueue,
    path: &Path,
    width: u32,
    height: u32,
) -> Result<u64, RecordingError> {
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| RecordingError::Io(format!("{}: {error}", path.display())))?;
    let mut writer = captastic_core::GifWriter::new(BufWriter::new(file), width, height)
        .map_err(|error| RecordingError::Encode(error.to_string()))?;
    let end = loop {
        match queue.pop() {
            Next::Frame(pixels, at) => writer
                .push(&pixels, at)
                .map_err(|error| RecordingError::Encode(error.to_string()))?,
            Next::Closed(end) => break end,
        }
    };
    let (file, summary) = writer
        .finish(end)
        .map_err(|error| RecordingError::Encode(error.to_string()))?;
    file.into_inner()
        .map_err(|error| RecordingError::Io(error.to_string()))?
        .sync_all()
        .map_err(|error| RecordingError::Io(error.to_string()))?;
    Ok(summary.frames_written)
}

fn encode_mp4(
    queue: &FrameQueue,
    path: &Path,
    width: u32,
    height: u32,
) -> Result<u64, RecordingError> {
    let mut writer = captastic_windows::Mp4Writer::create(
        path,
        width,
        height,
        RecordingFormat::Mp4.frame_rate(),
    )
    .map_err(|error| RecordingError::Encode(error.to_string()))?;
    let end = loop {
        match queue.pop() {
            Next::Frame(pixels, at) => writer
                .push(&pixels, at)
                .map_err(|error| RecordingError::Encode(error.to_string()))?,
            Next::Closed(end) => break end,
        }
    };
    let summary = writer
        .finish(end)
        .map_err(|error| RecordingError::Encode(error.to_string()))?;
    Ok(summary.frames_written)
}

/// Creates an empty file for a recording in `directory`, named `stem` with `extension` or, if
/// that is taken, `stem-2`, `stem-3` and so on. Never replaces a file it did not create.
pub fn reserve_path(
    directory: &Path,
    stem: &str,
    extension: &str,
) -> Result<PathBuf, RecordingError> {
    std::fs::create_dir_all(directory)
        .map_err(|error| RecordingError::Io(format!("{}: {error}", directory.display())))?;
    for attempt in 0..MAX_NAME_ATTEMPTS {
        let candidate = if attempt == 0 {
            stem.to_owned()
        } else {
            format!("{stem}-{}", attempt + 1)
        };
        let Some(path) = crate::filename_template::resolve(directory, &candidate, extension) else {
            return Err(RecordingError::Io(format!(
                "refusing to write {candidate}.{extension}: it would land outside {}",
                directory.display()
            )));
        };
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(RecordingError::Io(format!("{}: {error}", path.display()))),
        }
    }
    Err(RecordingError::Io(format!(
        "no unused name for a recording in {} after {MAX_NAME_ATTEMPTS} attempts",
        directory.display()
    )))
}

/// What the encoder takes next.
enum Next {
    Frame(Vec<u8>, Duration),
    Closed(Duration),
}

struct QueueState {
    frames: VecDeque<(Vec<u8>, Duration)>,
    closed_at: Option<Duration>,
    dropped: u64,
    failed: bool,
}

/// Frames between the pacing loop and the encoder, at most `capacity` of them.
struct FrameQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
    capacity: usize,
}

impl FrameQueue {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            state: Mutex::new(QueueState {
                frames: VecDeque::with_capacity(capacity),
                closed_at: None,
                dropped: 0,
                failed: false,
            }),
            ready: Condvar::new(),
            capacity,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adds a frame, dropping the oldest waiting one if the encoder is that far behind.
    fn push(&self, pixels: Vec<u8>, at: Duration) {
        let mut state = self.lock();
        if state.failed {
            return;
        }
        if state.frames.len() >= self.capacity {
            state.frames.pop_front();
            state.dropped += 1;
        }
        state.frames.push_back((pixels, at));
        self.ready.notify_one();
    }

    /// No more frames: the recording ended `at`.
    fn close(&self, at: Duration) {
        self.lock().closed_at = Some(at);
        self.ready.notify_one();
    }

    /// The encoder stopped; frames pushed from now on go nowhere.
    fn fail(&self) {
        let mut state = self.lock();
        state.failed = true;
        state.frames.clear();
    }

    fn failed(&self) -> bool {
        self.lock().failed
    }

    fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    fn pop(&self) -> Next {
        let mut state = self.lock();
        loop {
            if let Some((pixels, at)) = state.frames.pop_front() {
                return Next::Frame(pixels, at);
            }
            if let Some(end) = state.closed_at {
                return Next::Closed(end);
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use captastic_core::{
        CaptureId, CaptureMode, ColorSpace, DisplayId, FrameMetadata, FrameOrigin, PixelFormat,
        TimingProvenance,
    };

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// A display at `bounds` whose every pixel is `value`.
    fn frame(bounds: Rect, value: u8) -> CpuFrame {
        CpuFrame::new(
            Arc::from(vec![value; (bounds.width * bounds.height * 4) as usize]),
            bounds.width,
            bounds.height,
            bounds.width * 4,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            FrameMetadata {
                capture_id: CaptureId(1),
                backend: "test".to_owned(),
                display_id: DisplayId::primary(),
                source_rect: bounds,
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
                pool_slot: None,
                cursor: None,
            },
        )
        .expect("frame")
    }

    /// A display whose colour changes every frame, and which can be told to fail.
    struct Changing {
        bounds: Rect,
        count: u8,
        fail_after: Option<u8>,
    }

    impl FrameSource for Changing {
        fn next_frame(&mut self) -> Result<CpuFrame, CaptureError> {
            if self.fail_after.is_some_and(|limit| self.count >= limit) {
                return Err(CaptureError::synthetic("the display went away"));
            }
            self.count = self.count.wrapping_add(17);
            Ok(frame(self.bounds, self.count))
        }
    }

    fn directory(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("captastic-recording-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        directory
    }

    fn settings(format: RecordingFormat, seconds: f64) -> RecordingSettings {
        RecordingSettings {
            format,
            max_duration: Duration::from_secs_f64(seconds),
            gif_max_dimension: 64,
        }
    }

    #[test]
    fn a_recording_runs_to_its_limit_and_leaves_a_gif_of_the_area() {
        let directory = directory("limit");
        let path = reserve_path(&directory, "clip", "gif").expect("reserve");
        let mut source = Changing {
            bounds: rect(100, 50, 200, 100),
            count: 0,
            fail_after: None,
        };
        let stop = AtomicBool::new(false);
        let report = record(
            &mut source,
            rect(120, 60, 100, 50),
            settings(RecordingFormat::Gif, 0.5),
            &path,
            &stop,
        )
        .expect("record");
        assert_eq!(report.ended, RecordingEnd::TimeLimit);
        // Scaled to fit 64 pixels on its longer side.
        assert_eq!((report.width, report.height), (64, 32));
        // About half a second at 15 frames a second.
        assert!((6..=9).contains(&report.frames_captured), "{report:?}");
        assert_eq!(
            report.frames_written,
            report.frames_captured - report.frames_dropped
        );
        assert!(report.bytes > 0);
        let bytes = std::fs::read(&path).expect("read");
        assert_eq!(&bytes[..6], b"GIF89a");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_stop_ends_the_recording_early_and_still_finishes_the_file() {
        let directory = directory("stop");
        let path = reserve_path(&directory, "clip", "gif").expect("reserve");
        let mut source = Changing {
            bounds: rect(0, 0, 40, 30),
            count: 0,
            fail_after: None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = stop.clone();
        let waiter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            stopper.store(true, Ordering::Release);
        });
        let report = record(
            &mut source,
            rect(0, 0, 40, 30),
            settings(RecordingFormat::Gif, 30.0),
            &path,
            &stop,
        )
        .expect("record");
        waiter.join().expect("stopper");
        assert_eq!(report.ended, RecordingEnd::Stopped);
        assert!(report.duration < Duration::from_secs(2), "{report:?}");
        assert_eq!(&std::fs::read(&path).expect("read")[..6], b"GIF89a");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_source_that_keeps_failing_ends_the_recording_and_leaves_no_file() {
        let directory = directory("fail");
        let path = reserve_path(&directory, "clip", "gif").expect("reserve");
        let mut source = Changing {
            bounds: rect(0, 0, 40, 30),
            count: 0,
            fail_after: Some(50),
        };
        let stop = AtomicBool::new(false);
        let result = record(
            &mut source,
            rect(0, 0, 40, 30),
            settings(RecordingFormat::Gif, 30.0),
            &path,
            &stop,
        );
        assert!(
            matches!(result, Err(RecordingError::Source(_))),
            "{result:?}"
        );
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_area_off_the_display_is_refused() {
        let directory = directory("outside");
        let path = reserve_path(&directory, "clip", "gif").expect("reserve");
        let mut source = Changing {
            bounds: rect(0, 0, 40, 30),
            count: 0,
            fail_after: None,
        };
        let result = record(
            &mut source,
            rect(500, 500, 10, 10),
            settings(RecordingFormat::Gif, 1.0),
            &path,
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(RecordingError::OutsideDisplay { .. })));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn names_are_reserved_without_replacing_anything() {
        let directory = directory("names");
        let first = reserve_path(&directory, "clip", "gif").expect("first");
        let second = reserve_path(&directory, "clip", "gif").expect("second");
        assert_ne!(first, second);
        assert!(second
            .file_name()
            .expect("name")
            .to_string_lossy()
            .starts_with("clip-2"));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_slow_encoder_loses_the_oldest_frames_not_memory() {
        let queue = FrameQueue::with_capacity(4);
        for step in 0..10_u64 {
            queue.push(vec![step as u8], Duration::from_millis(step));
        }
        assert_eq!(queue.dropped(), 6);
        // The newest four are what remain, oldest first.
        match queue.pop() {
            Next::Frame(pixels, _) => assert_eq!(pixels, vec![6]),
            Next::Closed(_) => panic!("expected a frame"),
        }
    }

    #[test]
    fn the_queue_holds_more_small_frames_than_large_ones() {
        // A 4K frame is about 32 MiB: four of them, and never fewer than two.
        assert_eq!(queue_capacity(3840 * 2160 * 4), 4);
        assert_eq!(queue_capacity(7680 * 4320 * 4), 2);
        // A small GIF frame gets the most the queue allows.
        assert_eq!(queue_capacity(320 * 240 * 4), 90);
    }

    #[test]
    fn video_frames_are_trimmed_to_even_sides_and_gifs_scaled_to_fit() {
        let bounds = rect(0, 0, 9, 7);
        let pixels = frame_pixels(&frame(bounds, 5), bounds, 8, 6).expect("pixels");
        assert_eq!(pixels.len(), 8 * 6 * 4);
        assert_eq!(
            output_size(settings(RecordingFormat::Gif, 1.0), 640, 320),
            (64, 32)
        );
    }
}
