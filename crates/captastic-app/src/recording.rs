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
//!
//! The daemon's recordings are reviewed before they are saved, unless `recording.review` is off
//! (ADR 0012): [`RecordingHost`] records into memory, opens the recording in the review window,
//! and acts on the answer - writing it into the output directory as it is, encoding the span and
//! rectangle kept and writing that, or letting it go. Nothing reaches the disk before **Save**.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use captastic_config::RecordingFormat;
use captastic_core::{
    CaptureError, CaptureId, CpuFrame, GifReader, PixelEncoding, PixelRect, RecordingEdit,
    RecordingFrames, Rect,
};
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
    /// Where the recording is saved; empty for one held in memory until it is.
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
    let result = record_into(source, area, settings, Some(path), stop);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    let (report, _) = result?;
    Ok(report)
}

/// As [`record`], but encodes the recording into memory and returns its bytes with the report,
/// whose path is empty. A recording to be reviewed is held like this until it is saved.
pub fn record_in_memory(
    source: &mut dyn FrameSource,
    area: Rect,
    settings: RecordingSettings,
    stop: &AtomicBool,
) -> Result<(RecordingReport, Vec<u8>), RecordingError> {
    let (report, bytes) = record_into(source, area, settings, None, stop)?;
    Ok((report, bytes.unwrap_or_default()))
}

/// Records into the file at `path`, or into memory without one, returning the bytes then.
fn record_into(
    source: &mut dyn FrameSource,
    area: Rect,
    settings: RecordingSettings,
    path: Option<&Path>,
    stop: &AtomicBool,
) -> Result<(RecordingReport, Option<Vec<u8>>), RecordingError> {
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
        let path = path.map(Path::to_owned);
        let format = settings.format;
        thread::Builder::new()
            .name("captastic-recording-encoder".to_owned())
            .spawn(move || encode(&queue, format, path.as_deref(), width, height))
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
    let (written, data) = encoder
        .join()
        .map_err(|_| RecordingError::Encode("the encoder thread panicked".to_owned()))??;
    let bytes = match (&data, path) {
        (Some(data), _) => data.len() as u64,
        (None, Some(path)) => std::fs::metadata(path)
            .map_err(|error| RecordingError::Io(error.to_string()))?
            .len(),
        (None, None) => 0,
    };
    let report = RecordingReport {
        path: path.map(Path::to_owned).unwrap_or_default(),
        format: settings.format,
        bytes,
        width,
        height,
        duration,
        frames_captured: captured,
        frames_written: written,
        frames_dropped: queue.dropped(),
        ended,
    };
    Ok((report, data))
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

/// The encoder thread: everything the queue hands it, into `path` or into memory without one,
/// until the queue closes. Returns how many frames the recording holds, and its bytes when they
/// are in memory.
fn encode(
    queue: &FrameQueue,
    format: RecordingFormat,
    path: Option<&Path>,
    width: u32,
    height: u32,
) -> Result<(u64, Option<Vec<u8>>), RecordingError> {
    let result = match format {
        RecordingFormat::Gif => encode_gif(queue, path, width, height),
        RecordingFormat::Mp4 => encode_mp4(queue, path, width, height),
    };
    if result.is_err() {
        queue.fail();
    }
    result
}

/// Hands `push` every frame the queue holds until it closes, and returns when it closed.
fn queued_frames(
    queue: &FrameQueue,
    mut push: impl FnMut(&[u8], Duration) -> Result<(), String>,
) -> Result<Duration, RecordingError> {
    loop {
        match queue.pop() {
            Next::Frame(pixels, at) => push(&pixels, at).map_err(RecordingError::Encode)?,
            Next::Closed(end) => return Ok(end),
        }
    }
}

fn encode_gif(
    queue: &FrameQueue,
    path: Option<&Path>,
    width: u32,
    height: u32,
) -> Result<(u64, Option<Vec<u8>>), RecordingError> {
    fn frames<W: Write>(
        queue: &FrameQueue,
        output: W,
        width: u32,
        height: u32,
    ) -> Result<(W, u64), RecordingError> {
        let mut writer = captastic_core::GifWriter::new(output, width, height)
            .map_err(|error| RecordingError::Encode(error.to_string()))?;
        let end = queued_frames(queue, |pixels, at| {
            writer.push(pixels, at).map_err(|error| error.to_string())
        })?;
        let (output, summary) = writer
            .finish(end)
            .map_err(|error| RecordingError::Encode(error.to_string()))?;
        Ok((output, summary.frames_written))
    }
    let Some(path) = path else {
        let (bytes, written) = frames(queue, Vec::new(), width, height)?;
        return Ok((written, Some(bytes)));
    };
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|error| RecordingError::Io(format!("{}: {error}", path.display())))?;
    let (file, written) = frames(queue, BufWriter::new(file), width, height)?;
    file.into_inner()
        .map_err(|error| RecordingError::Io(error.to_string()))?
        .sync_all()
        .map_err(|error| RecordingError::Io(error.to_string()))?;
    Ok((written, None))
}

fn encode_mp4(
    queue: &FrameQueue,
    path: Option<&Path>,
    width: u32,
    height: u32,
) -> Result<(u64, Option<Vec<u8>>), RecordingError> {
    let rate = RecordingFormat::Mp4.frame_rate();
    let mut writer = match path {
        Some(path) => captastic_windows::Mp4Writer::create(path, width, height, rate),
        None => captastic_windows::Mp4Writer::create_in_memory(width, height, rate),
    }
    .map_err(|error| RecordingError::Encode(error.to_string()))?;
    let end = queued_frames(queue, |pixels, at| {
        writer.push(pixels, at).map_err(|error| error.to_string())
    })?;
    if path.is_some() {
        let summary = writer
            .finish(end)
            .map_err(|error| RecordingError::Encode(error.to_string()))?;
        return Ok((summary.frames_written, None));
    }
    let (bytes, summary) = writer
        .finish_in_memory(end)
        .map_err(|error| RecordingError::Encode(error.to_string()))?;
    Ok((summary.frames_written, Some(bytes)))
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

/// Asks the daemon's capture thread for each frame, so a recording shares the capture engine the
/// still captures use - its display list, its recovery - rather than duplicating the desktop a
/// second time. Still captures are refused while a recording runs, so the two never compete for
/// the engine's readback buffers.
struct CaptureThreadFrames {
    sender: std::sync::mpsc::SyncSender<crate::daemon::CaptureCommand>,
    source: captastic_core::CaptureSource,
}

/// How long a recording waits for one frame before counting it as failed.
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

impl FrameSource for CaptureThreadFrames {
    fn next_frame(&mut self) -> Result<CpuFrame, CaptureError> {
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        let request = crate::daemon::CaptureCommand::RecordFrame(Box::new(
            crate::daemon::RecordFrameRequest {
                source: self.source.clone(),
                reply,
            },
        ));
        self.sender.try_send(request).map_err(|error| {
            CaptureError::synthetic(match error {
                std::sync::mpsc::TrySendError::Full(_) => "the capture thread is busy",
                std::sync::mpsc::TrySendError::Disconnected(_) => "the capture thread has stopped",
            })
        })?;
        answer
            .recv_timeout(FRAME_TIMEOUT)
            .map_err(|_| CaptureError::synthetic("the capture thread did not answer in time"))?
    }
}

/// What the overlay confirmed with **Record**.
pub struct RecordingRequest {
    pub capture_id: captastic_core::CaptureId,
    pub display_id: captastic_core::DisplayId,
    /// The recorded area, in desktop coordinates.
    pub area: Rect,
    pub window_title: Option<String>,
    pub window_application: Option<String>,
}

/// The daemon's recordings: at most one at a time, its indicator, and where it is saved.
///
/// Cheap to clone; every clone is the same host.
#[derive(Clone)]
pub struct RecordingHost {
    inner: Arc<HostInner>,
}

struct HostInner {
    /// Set from the moment a recording is asked for until its file is finished or removed, and
    /// read by the capture thread to refuse still captures meanwhile.
    active: Arc<AtomicBool>,
    stop: Mutex<Option<Arc<AtomicBool>>>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
    config: Mutex<captastic_config::RecordingConfig>,
    output_directory: PathBuf,
    filename_template: String,
    history: crate::file_output::HistoryRecorder,
    notices: std::sync::mpsc::SyncSender<crate::daemon::DaemonNotice>,
    capture_sender: std::sync::mpsc::SyncSender<crate::daemon::CaptureCommand>,
    /// Where `recording.format` is written back when **Record As** changes it.
    config_path: Option<PathBuf>,
    json_output: bool,
    /// Whether a recording started now is reviewed before it is saved.
    review: AtomicBool,
    /// The review open, if any, and the drafts waiting for it to end.
    reviews: Mutex<ReviewQueue>,
    /// Edited recordings being encoded, which exit waits for.
    savers: Mutex<Vec<Saver>>,
    /// Set as Captastic exits: a recording that finishes then is saved, not reviewed.
    exiting: AtomicBool,
}

/// One review at a time: a recording finished while another is in review waits its turn.
#[derive(Default)]
struct ReviewQueue {
    open: bool,
    waiting: VecDeque<Draft>,
}

/// A finished recording held in memory, waiting for its review or in it.
struct Draft {
    /// As recorded; its path is empty, since it is not saved yet.
    report: RecordingReport,
    /// The recording's file, as it would be saved unchanged.
    recording: Arc<[u8]>,
    /// What the saved file is named, before its extension.
    stem: String,
    capture_id: CaptureId,
    display: String,
    area: Rect,
}

/// An edited recording being encoded on a thread of its own.
struct Saver {
    join: thread::JoinHandle<()>,
    /// The draft, until the thread saves it, or exit saves it unchanged because the thread has not
    /// finished in time: whichever takes it saves it.
    unsaved: Arc<Mutex<Option<Draft>>>,
}

/// Where a recording goes as it is recorded.
enum Destination {
    /// Straight into this file.
    File(PathBuf),
    /// Into memory, for a review, then saved as `stem`.
    Review { stem: String },
}

impl RecordingHost {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: captastic_config::RecordingConfig,
        output_directory: PathBuf,
        filename_template: String,
        history: crate::file_output::HistoryRecorder,
        notices: std::sync::mpsc::SyncSender<crate::daemon::DaemonNotice>,
        capture_sender: std::sync::mpsc::SyncSender<crate::daemon::CaptureCommand>,
        config_path: Option<PathBuf>,
        json_output: bool,
    ) -> Self {
        let review = AtomicBool::new(config.review);
        Self {
            inner: Arc::new(HostInner {
                review,
                reviews: Mutex::new(ReviewQueue::default()),
                savers: Mutex::new(Vec::new()),
                exiting: AtomicBool::new(false),
                active: Arc::new(AtomicBool::new(false)),
                stop: Mutex::new(None),
                join: Mutex::new(None),
                config: Mutex::new(config),
                output_directory,
                filename_template,
                history,
                notices,
                capture_sender,
                config_path,
                json_output,
            }),
        }
    }

    /// Whether a recording is running, for the capture thread to refuse still captures by.
    pub fn active_flag(&self) -> Arc<AtomicBool> {
        self.inner.active.clone()
    }

    /// What **Record** records into now.
    pub fn format(&self) -> RecordingFormat {
        lock(&self.inner.config).format
    }

    /// Applies **Record As**: this and later recordings use `format`, and it is written back to
    /// the configuration as `recording.format` so it survives a restart.
    pub fn set_format(&self, format: RecordingFormat) {
        lock(&self.inner.config).format = format;
        log::info!("recordings will be saved as {format}");
        if let Some(path) = self.inner.config_path.as_deref() {
            if let Err(error) = captastic_config::set_recording_format(path, format) {
                crate::logging::warn(format_args!(
                    "the recording format could not be saved to {}: {error}",
                    path.display()
                ));
            }
        }
    }

    /// Whether recordings are reviewed before they are saved.
    pub fn review_enabled(&self) -> bool {
        self.inner.review.load(Ordering::Acquire)
    }

    /// Applies the notification area's **Review Recordings** switch to later recordings, and
    /// writes it back to the configuration as `recording.review`.
    pub fn set_review(&self, enabled: bool) {
        self.inner.review.store(enabled, Ordering::Release);
        lock(&self.inner.config).review = enabled;
        log::info!(
            "recordings will be {}",
            if enabled {
                "reviewed before they are saved"
            } else {
                "saved as soon as they end"
            }
        );
        if let Some(path) = self.inner.config_path.as_deref() {
            if let Err(error) = captastic_config::set_recording_review(path, enabled) {
                crate::logging::warn(format_args!(
                    "the review setting could not be saved to {}: {error}",
                    path.display()
                ));
            }
        }
    }

    /// Starts recording `request`'s area: shows the indicator through `desktop_ui` and records on
    /// a thread of its own until **Stop** or the time limit. Refused, with a notice, while another
    /// recording runs.
    pub fn start(
        &self,
        request: RecordingRequest,
        desktop_ui: &Arc<Mutex<Option<captastic_windows::DesktopUi>>>,
    ) {
        let inner = &self.inner;
        if inner.active.swap(true, Ordering::AcqRel) {
            let _ = inner
                .notices
                .try_send(crate::daemon::DaemonNotice::RecordingInProgress);
            return;
        }
        let config = lock(&inner.config).clone();
        let settings = RecordingSettings::from_config(&config);
        let stem = crate::filename_template::expand(
            &inner.filename_template,
            &crate::filename_template::TemplateContext {
                timestamp_micros: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_micros()),
                display: &request.display_id.0,
                mode: "recording",
                width: request.area.width,
                height: request.area.height,
                application: request.window_application.as_deref(),
                title: request.window_title.as_deref(),
            },
        );
        // A recording to be reviewed is held in memory and saved once the review is done.
        let destination = if inner.review.load(Ordering::Acquire) {
            Destination::Review { stem }
        } else {
            match reserve_path(&inner.output_directory, &stem, settings.format.extension()) {
                Ok(path) => Destination::File(path),
                Err(error) => {
                    inner.active.store(false, Ordering::Release);
                    self.report_failure(request.capture_id, &error);
                    return;
                }
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        *lock(&inner.stop) = Some(stop.clone());
        show_indicator(desktop_ui, request.area, &stop, settings.max_duration);
        log::info!(
            "recording {} started: {}x{} at {},{} as {} for at most {} s",
            request.capture_id.0,
            request.area.width,
            request.area.height,
            request.area.x,
            request.area.y,
            settings.format,
            settings.max_duration.as_secs()
        );
        if inner.json_output {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "event": "recording_started",
                    "capture_id": request.capture_id,
                    "format": settings.format,
                    "area": request.area,
                })
            );
        }
        let host = self.clone();
        let thread_desktop_ui = desktop_ui.clone();
        let spawned = thread::Builder::new()
            .name("captastic-recording".to_owned())
            .spawn(move || {
                host.run(request, settings, destination, stop, thread_desktop_ui);
            });
        match spawned {
            Ok(join) => *lock(&inner.join) = Some(join),
            Err(error) => {
                inner.active.store(false, Ordering::Release);
                hide_indicator(Some(desktop_ui));
                crate::logging::warn(format_args!(
                    "the recording thread could not start: {error}"
                ));
            }
        }
    }

    /// The recording thread: records, takes the indicator down, and says how it went - or, for a
    /// review, hands the recording to it.
    fn run(
        &self,
        request: RecordingRequest,
        settings: RecordingSettings,
        destination: Destination,
        stop: Arc<AtomicBool>,
        desktop_ui: Arc<Mutex<Option<captastic_windows::DesktopUi>>>,
    ) {
        let inner = &self.inner;
        let mut frames = CaptureThreadFrames {
            sender: inner.capture_sender.clone(),
            source: if request.display_id == captastic_core::DisplayId::virtual_desktop() {
                captastic_core::CaptureSource::VirtualDesktop
            } else {
                captastic_core::CaptureSource::Display(request.display_id.clone())
            },
        };
        let result = match destination {
            Destination::File(path) => record(&mut frames, request.area, settings, &path, &stop)
                .map(|report| (report, None)),
            Destination::Review { stem } => {
                record_in_memory(&mut frames, request.area, settings, &stop)
                    .map(|(report, recording)| (report, Some((stem, recording))))
            }
        };
        hide_indicator(Some(&desktop_ui));
        *lock(&inner.stop) = None;
        inner.active.store(false, Ordering::Release);
        match result {
            Ok((report, review)) => {
                log::info!(
                    "recording {} finished: {} s, {}x{}, {} bytes, {} frames captured, {} written, {} dropped, ended by {:?}",
                    request.capture_id.0,
                    report.duration.as_secs_f64(),
                    report.width,
                    report.height,
                    report.bytes,
                    report.frames_captured,
                    report.frames_written,
                    report.frames_dropped,
                    report.ended
                );
                match review {
                    Some((stem, recording)) => {
                        let draft = Draft {
                            report,
                            recording: recording.into(),
                            stem,
                            capture_id: request.capture_id,
                            display: request.display_id.0.clone(),
                            area: request.area,
                        };
                        if inner.exiting.load(Ordering::Acquire) {
                            // Stopped because Captastic is exiting: there is no one to review it.
                            self.save_unchanged(draft);
                        } else {
                            self.queue_review(draft, &desktop_ui);
                        }
                    }
                    None => {
                        let ended = Some(report.ended);
                        self.announce_saved(
                            request.capture_id,
                            &request.display_id.0,
                            &report,
                            ended,
                        );
                    }
                }
            }
            Err(error) => self.report_failure(request.capture_id, &error),
        }
    }

    /// Records a saved recording in history and says so. `ended` is how the recording ended, for
    /// one saved straight away; a reviewed recording's ending is old news by the time it is saved.
    fn announce_saved(
        &self,
        capture_id: CaptureId,
        display: &str,
        report: &RecordingReport,
        ended: Option<RecordingEnd>,
    ) {
        let inner = &self.inner;
        log::info!(
            "recording {} saved: {} s, {}x{}, {} bytes",
            capture_id.0,
            report.duration.as_secs_f64(),
            report.width,
            report.height,
            report.bytes
        );
        inner.history.record_recording(report, display);
        if inner.json_output {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "event": "recording_complete",
                    "capture_id": capture_id,
                    "report": report,
                })
            );
        }
        let _ = inner
            .notices
            .try_send(crate::daemon::DaemonNotice::RecordingSaved {
                file_name: file_name(&report.path),
                path: report.path.clone(),
                ended,
            });
    }

    /// Opens `draft`'s review, or queues it behind the one open.
    fn queue_review(
        &self,
        draft: Draft,
        desktop_ui: &Arc<Mutex<Option<captastic_windows::DesktopUi>>>,
    ) {
        {
            let mut reviews = lock(&self.inner.reviews);
            if reviews.open {
                log::info!(
                    "recording {} waits for the review already open",
                    draft.capture_id.0
                );
                reviews.waiting.push_back(draft);
                return;
            }
            reviews.open = true;
        }
        self.open_review(draft, desktop_ui);
    }

    fn open_review(
        &self,
        draft: Draft,
        desktop_ui: &Arc<Mutex<Option<captastic_windows::DesktopUi>>>,
    ) {
        // Shared between the request and its answer, since a request the thread never receives
        // drops the answer uncalled and the draft has to be saved here instead.
        let shared = Arc::new(Mutex::new(Some(draft)));
        let requested = {
            let host = lock(desktop_ui);
            let Some(ui) = host.as_ref() else {
                drop(host);
                if let Some(draft) = lock(&shared).take() {
                    self.conclude_review(
                        draft,
                        captastic_windows::ReviewOutcome::Closed,
                        desktop_ui,
                    );
                }
                return;
            };
            let (recording, format, area) = {
                let draft = lock(&shared);
                let draft = draft
                    .as_ref()
                    .expect("the draft is here until the review answers");
                (draft.recording.clone(), draft.report.format, draft.area)
            };
            let host = self.clone();
            let answer_draft = shared.clone();
            let answer_ui = desktop_ui.clone();
            ui.review(recording, format, area, move |outcome| {
                if let Some(draft) = lock(&answer_draft).take() {
                    host.conclude_review(draft, outcome, &answer_ui);
                }
            })
        };
        if let Err(error) = requested {
            crate::logging::warn(format_args!(
                "the review could not be opened, so the recording is saved as it is: {error}"
            ));
            if let Some(draft) = lock(&shared).take() {
                self.conclude_review(draft, captastic_windows::ReviewOutcome::Closed, desktop_ui);
            }
        }
    }

    /// Acts on a review's answer, then opens the next review waiting, if any.
    fn conclude_review(
        &self,
        draft: Draft,
        outcome: captastic_windows::ReviewOutcome,
        desktop_ui: &Arc<Mutex<Option<captastic_windows::DesktopUi>>>,
    ) {
        use captastic_windows::ReviewOutcome;
        let shutting_down = matches!(outcome, ReviewOutcome::Closed);
        match outcome {
            ReviewOutcome::Save(edit)
                if !edit.changes_nothing(&timeline_of(&draft.report), size_of(&draft.report)) =>
            {
                let host = self.clone();
                let unsaved = Arc::new(Mutex::new(Some(draft)));
                let saving = unsaved.clone();
                let spawned = thread::Builder::new()
                    .name("captastic-recording-save".to_owned())
                    .spawn(move || host.save_edited(&saving, edit));
                match spawned {
                    Ok(join) => {
                        let mut savers = lock(&self.inner.savers);
                        savers.retain(|saver| !saver.join.is_finished());
                        savers.push(Saver { join, unsaved });
                    }
                    Err(error) => {
                        if let Some(draft) = lock(&unsaved).take() {
                            self.edit_failed(draft, &error.to_string());
                        }
                    }
                }
            }
            ReviewOutcome::Save(_) | ReviewOutcome::Closed => self.save_unchanged(draft),
            ReviewOutcome::Failed(message) => {
                crate::logging::warn(format_args!(
                    "recording {} could not be reviewed, so it is saved as it is: {message}",
                    draft.capture_id.0
                ));
                self.save_unchanged(draft);
            }
            ReviewOutcome::Discard => self.discard(draft),
        }
        let next = {
            let mut reviews = lock(&self.inner.reviews);
            let next = reviews.waiting.pop_front();
            reviews.open = next.is_some();
            next
        };
        if let Some(next) = next {
            if shutting_down {
                // Captastic is exiting: what waited is saved as it is, like the review it followed.
                self.conclude_review(next, ReviewOutcome::Closed, desktop_ui);
            } else {
                self.open_review(next, desktop_ui);
            }
        }
    }

    /// Saves a draft into the output directory as it was recorded.
    fn save_unchanged(&self, draft: Draft) {
        let extension = draft.report.format.extension();
        match write_new(
            &self.inner.output_directory,
            &draft.stem,
            extension,
            &draft.recording,
        ) {
            Ok(path) => {
                let report = RecordingReport {
                    path,
                    ..draft.report
                };
                self.announce_saved(draft.capture_id, &draft.display, &report, None);
            }
            Err(error) => self.report_failure(draft.capture_id, &error),
        }
    }

    /// Encodes the span and rectangle `edit` keeps of the draft in `unsaved` and saves that into
    /// the output directory. If that fails, the draft is saved as it was recorded instead; if exit
    /// has taken the draft meanwhile, to save it unchanged, the edit is dropped.
    fn save_edited(&self, unsaved: &Mutex<Option<Draft>>, edit: RecordingEdit) {
        let (recording, format) = match lock(unsaved).as_ref() {
            Some(draft) => (draft.recording.clone(), draft.report.format),
            None => return,
        };
        let edited = encode_edited(&recording, format, edit);
        let Some(draft) = lock(unsaved).take() else {
            return;
        };
        let (bytes, summary) = match edited {
            Ok(edited) => edited,
            Err(error) => {
                self.edit_failed(draft, &error.to_string());
                return;
            }
        };
        match write_new(
            &self.inner.output_directory,
            &draft.stem,
            format.extension(),
            &bytes,
        ) {
            Ok(path) => {
                let report = RecordingReport {
                    path,
                    bytes: bytes.len() as u64,
                    width: summary.width,
                    height: summary.height,
                    duration: edit.length(),
                    frames_written: summary.frames_written,
                    ..draft.report
                };
                self.announce_saved(draft.capture_id, &draft.display, &report, None);
            }
            Err(error) => self.edit_failed(draft, &error.to_string()),
        }
    }

    fn edit_failed(&self, draft: Draft, message: &str) {
        crate::logging::warn(format_args!(
            "recording {} could not be trimmed or cropped, so it is saved as recorded: {message}",
            draft.capture_id.0
        ));
        let extension = draft.report.format.extension();
        match write_new(
            &self.inner.output_directory,
            &draft.stem,
            extension,
            &draft.recording,
        ) {
            Ok(path) => {
                let report = RecordingReport {
                    path,
                    ..draft.report
                };
                self.inner.history.record_recording(&report, &draft.display);
                let _ =
                    self.inner
                        .notices
                        .try_send(crate::daemon::DaemonNotice::RecordingEditFailed {
                            file_name: file_name(&report.path),
                            path: report.path.clone(),
                            message: message.to_owned(),
                        });
            }
            Err(error) => self.report_failure(draft.capture_id, &error),
        }
    }

    /// Lets a draft go: it was never on disk, so there is nothing to delete.
    fn discard(&self, draft: Draft) {
        log::info!("recording {} discarded", draft.capture_id.0);
        if self.inner.json_output {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "event": "recording_discarded",
                    "capture_id": draft.capture_id,
                })
            );
        }
    }

    fn report_failure(&self, capture_id: captastic_core::CaptureId, error: &RecordingError) {
        crate::logging::warn(format_args!(
            "recording {} failed and nothing was saved: {error}",
            capture_id.0
        ));
        if self.inner.json_output {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "event": "recording_failed",
                    "capture_id": capture_id,
                    "message": error.to_string(),
                })
            );
        }
        let _ = self
            .inner
            .notices
            .try_send(crate::daemon::DaemonNotice::RecordingFailed {
                message: error.to_string(),
            });
    }

    /// Stops a running recording and waits until `deadline` for its file to be finished.
    /// Captastic exiting finishes a recording rather than discarding it.
    pub fn stop_before(&self, deadline: Instant) {
        self.inner.exiting.store(true, Ordering::Release);
        if let Some(stop) = lock(&self.inner.stop).as_ref() {
            stop.store(true, Ordering::Release);
        }
        let recording = lock(&self.inner.join).take();
        if let Some(join) = recording {
            if !join_before(join, deadline) {
                log::warn!("the recording did not finish before the shutdown deadline");
                return;
            }
        }
        // And an edited recording still being encoded is given until the deadline to finish; one
        // that does not is saved as it was recorded instead, so exiting loses no recording.
        let savers = std::mem::take(&mut *lock(&self.inner.savers));
        for saver in savers {
            if !join_before(saver.join, deadline) {
                if let Some(draft) = lock(&saver.unsaved).take() {
                    log::warn!(
                        "recording {} was still being trimmed or cropped at shutdown, so it is saved as recorded",
                        draft.capture_id.0
                    );
                    self.save_unchanged(draft);
                }
            }
        }
    }
}

/// Waits for `handle`'s thread until `deadline`; whether it ended in time.
fn join_before(handle: thread::JoinHandle<()>, deadline: Instant) -> bool {
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = handle.join();
    true
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The timeline a review shows a report's recording on, for asking whether an edit keeps all of
/// it: only its duration matters for that.
fn timeline_of(report: &RecordingReport) -> captastic_core::Timeline {
    captastic_core::Timeline::new(
        vec![Duration::ZERO],
        report.duration.max(Duration::from_millis(1)),
    )
    .expect("a one-frame timeline that ends after it starts")
}

fn size_of(report: &RecordingReport) -> (u32, u32) {
    (report.width, report.height)
}

/// Writes `bytes` into `directory` as `stem.extension`, or the next unused name like it, and
/// returns where they went. Leaves nothing behind if the write fails.
fn write_new(
    directory: &Path,
    stem: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf, RecordingError> {
    let path = reserve_path(directory, stem, extension)?;
    let written = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        });
    if let Err(error) = written {
        let _ = std::fs::remove_file(&path);
        return Err(RecordingError::Io(format!("{}: {error}", path.display())));
    }
    Ok(path)
}

/// What an edited recording came out as.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EditedRecording {
    pub width: u32,
    pub height: u32,
    pub frames_written: u64,
}

/// Encodes what `edit` keeps of `recording`, a recording's file, into a new one in the same
/// format, returned with what it holds.
pub fn encode_edited(
    recording: &[u8],
    format: RecordingFormat,
    edit: RecordingEdit,
) -> Result<(Vec<u8>, EditedRecording), RecordingError> {
    let playback = |error: captastic_core::PlaybackError| RecordingError::Encode(error.0);
    let mut reader: Box<dyn RecordingFrames> = match format {
        RecordingFormat::Gif => Box::new(GifReader::new(recording).map_err(playback)?),
        RecordingFormat::Mp4 => {
            Box::new(captastic_windows::Mp4Reader::from_bytes(recording).map_err(playback)?)
        }
    };
    let size = reader.size();
    let mut edit = edit;
    if format == RecordingFormat::Mp4 {
        // H.264 needs even sides; a crop that is not loses its last column or row.
        let crop = edit.crop.unwrap_or(PixelRect {
            x: 0,
            y: 0,
            width: size.0,
            height: size.1,
        });
        let (width, height) = captastic_windows::mp4_frame_size(crop.width, crop.height);
        edit.crop = Some(PixelRect {
            width: width.min(crop.width),
            height: height.min(crop.height),
            ..crop
        });
    }
    let (width, height) = edit.output_size(size);
    let timeline = reader.timeline().clone();
    let kept = edit.kept_frames(&timeline);
    if kept.is_empty() {
        return Err(RecordingError::Encode("the span kept is empty".to_owned()));
    }
    let encode = |error: String| RecordingError::Encode(error);
    let (bytes, frames_written) = match format {
        RecordingFormat::Gif => {
            let mut writer = captastic_core::GifWriter::new(Vec::new(), width, height)
                .map_err(|error| encode(error.to_string()))?;
            for frame in &kept {
                let pixels = reader.frame(frame.index).map_err(playback)?;
                writer
                    .push(&edit.crop_pixels(pixels, size.0), frame.at)
                    .map_err(|error| encode(error.to_string()))?;
            }
            let (bytes, summary) = writer
                .finish(edit.length())
                .map_err(|error| encode(error.to_string()))?;
            (bytes, summary.frames_written)
        }
        RecordingFormat::Mp4 => {
            let mut writer =
                captastic_windows::Mp4Writer::create_in_memory(width, height, format.frame_rate())
                    .map_err(|error| encode(error.to_string()))?;
            for frame in &kept {
                let pixels = reader.frame(frame.index).map_err(playback)?;
                writer
                    .push(&edit.crop_pixels(pixels, size.0), frame.at)
                    .map_err(|error| encode(error.to_string()))?;
            }
            let (bytes, summary) = writer
                .finish_in_memory(edit.length())
                .map_err(|error| encode(error.to_string()))?;
            (bytes, summary.frames_written)
        }
    };
    Ok((
        bytes,
        EditedRecording {
            width,
            height,
            frames_written,
        },
    ))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn show_indicator(
    desktop_ui: &Arc<Mutex<Option<captastic_windows::DesktopUi>>>,
    area: Rect,
    stop: &Arc<AtomicBool>,
    limit: Duration,
) {
    let mut host = lock(desktop_ui);
    if host.is_none() {
        match captastic_windows::DesktopUi::start() {
            Ok(started) => *host = Some(started),
            Err(error) => {
                // Without the indicator a recording would run unseen, which ADR 0011 rules out;
                // it is stopped at once, and finishes as the shortest recording there is.
                crate::logging::warn(format_args!(
                    "the recording border could not be shown, so the recording is stopped: {error}"
                ));
                stop.store(true, Ordering::Release);
                return;
            }
        }
    }
    if let Some(host) = host.as_ref() {
        if let Err(error) = host.show_recording(area, stop.clone(), limit) {
            crate::logging::warn(format_args!(
                "the recording border could not be shown, so the recording is stopped: {error}"
            ));
            stop.store(true, Ordering::Release);
        }
    }
}

fn hide_indicator(desktop_ui: Option<&Arc<Mutex<Option<captastic_windows::DesktopUi>>>>) {
    let Some(desktop_ui) = desktop_ui else { return };
    if let Some(host) = lock(desktop_ui).as_ref() {
        let _ = host.hide_recording();
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
    #[ignore = "shows the recording border on the interactive desktop"]
    fn the_host_records_through_the_capture_thread_saves_remembers_and_says_so() {
        use crate::daemon::{CaptureCommand, DaemonNotice};

        let directory = directory("host");
        let (capture_sender, capture_receiver) = std::sync::mpsc::sync_channel(4);
        let (notice_sender, notices) = std::sync::mpsc::sync_channel(8);
        let display = rect(0, 0, 160, 120);
        // A stand-in capture thread that answers each frame request with a changing frame.
        let capture_thread = thread::spawn(move || {
            let mut value = 0_u8;
            while let Ok(command) = capture_receiver.recv() {
                if let CaptureCommand::RecordFrame(request) = command {
                    value = value.wrapping_add(9);
                    let _ = request.reply.send(Ok(frame(display, value)));
                }
            }
        });
        let history = captastic_config::HistoryStore::at(directory.join("history.toml"));
        let host = RecordingHost::new(
            captastic_config::RecordingConfig {
                format: RecordingFormat::Gif,
                max_seconds: 30,
                gif_max_dimension: 1280,
                review: false,
            },
            directory.join("captures"),
            "{timestamp}".to_owned(),
            crate::file_output::HistoryRecorder::new(
                history.clone(),
                captastic_config::HistoryConfig::default().retention(),
            ),
            notice_sender,
            capture_sender,
            None,
            false,
        );
        let desktop_ui = Arc::new(Mutex::new(None));
        host.start(
            RecordingRequest {
                capture_id: captastic_core::CaptureId(5),
                display_id: DisplayId::primary(),
                area: rect(20, 10, 100, 80),
                window_title: None,
                window_application: None,
            },
            &desktop_ui,
        );
        assert!(host.active_flag().load(Ordering::Acquire));
        // A second recording is refused while the first runs.
        host.start(
            RecordingRequest {
                capture_id: captastic_core::CaptureId(6),
                display_id: DisplayId::primary(),
                area: rect(0, 0, 10, 10),
                window_title: None,
                window_application: None,
            },
            &desktop_ui,
        );
        thread::sleep(Duration::from_millis(700));
        host.stop_before(Instant::now() + Duration::from_secs(5));
        assert!(!host.active_flag().load(Ordering::Acquire));
        let received: Vec<DaemonNotice> = notices.try_iter().collect();
        assert!(received
            .iter()
            .any(|notice| matches!(notice, DaemonNotice::RecordingInProgress)));
        assert!(
            received
                .iter()
                .any(|notice| matches!(notice, DaemonNotice::RecordingSaved { .. })),
            "no saved notice"
        );
        let saved: Vec<PathBuf> = std::fs::read_dir(directory.join("captures"))
            .expect("captures")
            .map(|entry| entry.expect("entry").path())
            .collect();
        assert_eq!(saved.len(), 1, "{saved:?}");
        assert_eq!(saved[0].extension().and_then(|e| e.to_str()), Some("gif"));
        let remembered = history.load().expect("history");
        assert_eq!(remembered.entries.len(), 1);
        assert_eq!(remembered.entries[0].mode, "recording");
        assert_eq!(
            (remembered.entries[0].width, remembered.entries[0].height),
            (100, 80)
        );
        if let Some(ui) = desktop_ui.lock().expect("ui").take() {
            ui.stop(Instant::now() + Duration::from_secs(2));
        }
        drop(host);
        let _ = capture_thread.join();
        let _ = std::fs::remove_dir_all(&directory);
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

    /// A GIF of `frames` frames, 100 ms apart, 40 by 30, whose frame `n` has its first `n + 1`
    /// pixels of the top row white, so each frame says which it is.
    fn counting_gif(frames: u32) -> Vec<u8> {
        let (width, height) = (40_u32, 30_u32);
        let mut writer = captastic_core::GifWriter::new(Vec::new(), width, height).expect("writer");
        let mut pixels = vec![80_u8; (width * height * 4) as usize];
        for n in 0..frames {
            let at = (n * 4) as usize;
            pixels[at..at + 4].copy_from_slice(&[255, 255, 255, 255]);
            writer
                .push(&pixels, Duration::from_millis(u64::from(n) * 100))
                .expect("push");
        }
        let (bytes, _) = writer
            .finish(Duration::from_millis(u64::from(frames) * 100))
            .expect("finish");
        bytes
    }

    fn white_count(pixels: &[u8]) -> usize {
        pixels
            .chunks_exact(4)
            .take(40)
            .filter(|pixel| pixel[0] > 200)
            .count()
    }

    #[test]
    fn an_edit_encodes_only_the_span_and_rectangle_kept() {
        let source = counting_gif(20);
        let edit = RecordingEdit {
            start: Duration::from_millis(550),
            end: Duration::from_millis(1_250),
            crop: Some(PixelRect {
                x: 2,
                y: 0,
                width: 20,
                height: 10,
            }),
        };
        let (bytes, edited) = encode_edited(&source, RecordingFormat::Gif, edit).expect("encode");
        assert_eq!((edited.width, edited.height), (20, 10));
        let mut reader = GifReader::new(&bytes).expect("reader");
        assert_eq!(reader.size(), (20, 10));
        let timeline = reader.timeline().clone();
        assert_eq!(timeline.duration(), Duration::from_millis(700));
        // Frame 5 was on screen at 550 ms, so it opens the result; frame 12 started at 1200 ms
        // and is the last. Cropping two columns away leaves frame n with n - 1 white pixels.
        assert_eq!(white_count(reader.frame(0).expect("first")), 4);
        let last = timeline.len() - 1;
        assert_eq!(white_count(reader.frame(last).expect("last")), 11);
    }

    /// A host that reviews, with its notices, and the history it remembers into.
    fn reviewing_host(
        directory: &Path,
    ) -> (
        RecordingHost,
        std::sync::mpsc::Receiver<crate::daemon::DaemonNotice>,
        captastic_config::HistoryStore,
    ) {
        let (capture_sender, _capture_receiver) = std::sync::mpsc::sync_channel(1);
        let (notice_sender, notices) = std::sync::mpsc::sync_channel(16);
        let history = captastic_config::HistoryStore::at(directory.join("history.toml"));
        let host = RecordingHost::new(
            captastic_config::RecordingConfig::default(),
            directory.join("captures"),
            "{timestamp}".to_owned(),
            crate::file_output::HistoryRecorder::new(
                history.clone(),
                captastic_config::HistoryConfig::default().retention(),
            ),
            notice_sender,
            capture_sender,
            None,
            false,
        );
        (host, notices, history)
    }

    /// A draft of a two-second counting GIF, held in memory.
    fn draft(name: &str) -> Draft {
        let recording = counting_gif(20);
        Draft {
            report: RecordingReport {
                bytes: recording.len() as u64,
                path: PathBuf::new(),
                format: RecordingFormat::Gif,
                width: 40,
                height: 30,
                duration: Duration::from_secs(2),
                frames_captured: 20,
                frames_written: 20,
                frames_dropped: 0,
                ended: RecordingEnd::Stopped,
            },
            recording: recording.into(),
            stem: name.to_owned(),
            capture_id: CaptureId(3),
            display: "primary".to_owned(),
            area: rect(0, 0, 40, 30),
        }
    }

    fn saved(directory: &Path) -> Vec<PathBuf> {
        let mut saved: Vec<PathBuf> = std::fs::read_dir(directory.join("captures"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file())
                    .collect()
            })
            .unwrap_or_default();
        saved.sort();
        saved
    }

    #[test]
    fn a_review_saves_unchanged_as_recorded_saves_an_edit_by_encoding_and_discards_by_writing_nothing(
    ) {
        use crate::daemon::DaemonNotice;
        use captastic_windows::ReviewOutcome;

        let directory = directory("review");
        let (host, notices, history) = reviewing_host(&directory);
        let no_ui = Arc::new(Mutex::new(None));

        // Nothing is on disk until a review saves it.
        assert!(saved(&directory).is_empty());

        // Saved as it is: exactly the recorded bytes, and the notice is not about how it ended.
        let unchanged = draft("unchanged");
        let recorded = unchanged.recording.clone();
        host.conclude_review(
            unchanged,
            ReviewOutcome::Save(RecordingEdit::none(Duration::from_secs(2))),
            &no_ui,
        );
        let files = saved(&directory);
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].file_name().and_then(|n| n.to_str()),
            Some("unchanged.gif")
        );
        assert_eq!(std::fs::read(&files[0]).expect("saved"), recorded.to_vec());
        assert!(matches!(
            notices.try_recv(),
            Ok(DaemonNotice::RecordingSaved { ended: None, .. })
        ));

        // Trimmed: encoded and saved on a thread exit waits for.
        let trimmed = draft("trimmed");
        host.conclude_review(
            trimmed,
            ReviewOutcome::Save(RecordingEdit {
                start: Duration::from_millis(500),
                end: Duration::from_secs(2),
                crop: None,
            }),
            &no_ui,
        );
        host.stop_before(Instant::now() + Duration::from_secs(10));
        let trimmed = directory.join("captures").join("trimmed.gif");
        let reader = GifReader::new(&std::fs::read(&trimmed).expect("trimmed")).expect("reader");
        assert_eq!(reader.timeline().duration(), Duration::from_millis(1_500));
        assert!(matches!(
            notices.try_recv(),
            Ok(DaemonNotice::RecordingSaved { .. })
        ));

        // Discarded: nothing more written, saved or said.
        host.conclude_review(draft("discarded"), ReviewOutcome::Discard, &no_ui);
        assert_eq!(saved(&directory).len(), 2);
        assert!(notices.try_recv().is_err());

        // Both saved recordings are remembered, the trimmed one at its new length's size.
        let remembered = history.load().expect("history");
        assert_eq!(remembered.entries.len(), 2);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn with_no_window_to_review_in_a_recording_is_saved_and_waiting_ones_follow() {
        use crate::daemon::DaemonNotice;

        let directory = directory("review-queue");
        let (host, notices, _history) = reviewing_host(&directory);
        let no_ui = Arc::new(Mutex::new(None));
        // One review is open, so the next waits; when the open one closes, Captastic exiting,
        // the waiting one is saved too rather than reviewed.
        lock(&host.inner.reviews).open = true;
        host.queue_review(draft("second"), &no_ui);
        assert_eq!(lock(&host.inner.reviews).waiting.len(), 1);
        host.conclude_review(
            draft("first"),
            captastic_windows::ReviewOutcome::Closed,
            &no_ui,
        );
        assert_eq!(saved(&directory).len(), 2);
        assert!(!lock(&host.inner.reviews).open);
        // And with no desktop UI at all, a review cannot open, so the recording is saved.
        host.queue_review(draft("third"), &no_ui);
        assert_eq!(saved(&directory).len(), 3);
        assert_eq!(
            notices
                .try_iter()
                .filter(|notice| matches!(notice, DaemonNotice::RecordingSaved { .. }))
                .count(),
            3
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn an_edit_still_being_encoded_at_exit_is_saved_as_recorded() {
        use crate::daemon::DaemonNotice;

        let directory = directory("exit-while-saving");
        let (host, notices, _history) = reviewing_host(&directory);
        // An edited save that will not finish before the deadline.
        let (_release, wait) = std::sync::mpsc::channel::<()>();
        let unsaved = Arc::new(Mutex::new(Some(draft("slow"))));
        let recorded = lock(&unsaved).as_ref().expect("draft").recording.clone();
        let join = thread::spawn(move || {
            let _ = wait.recv();
        });
        lock(&host.inner.savers).push(Saver {
            join,
            unsaved: unsaved.clone(),
        });
        host.stop_before(Instant::now() + Duration::from_millis(50));
        let files = saved(&directory);
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read(&files[0]).expect("saved"), recorded.to_vec());
        assert!(matches!(
            notices.try_recv(),
            Ok(DaemonNotice::RecordingSaved { .. })
        ));
        // The edit, finishing later, finds the draft taken and saves nothing more.
        assert!(lock(&unsaved).is_none());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
