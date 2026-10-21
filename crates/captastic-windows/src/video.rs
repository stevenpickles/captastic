//! MP4 for screen recordings, through Windows Media Foundation (ADR 0011).
//!
//! [`Mp4Writer`] hands captured BGRA frames to Media Foundation's sink writer, which converts them
//! and encodes H.264 into an MP4 file as it goes, on the GPU's encoder where the machine has one.
//! It adds no dependency: Media Foundation ships with Windows.
//!
//! Frames carry the time they were captured, so the video's timing is the capture's: a frame is
//! written when the next one arrives, for exactly as long as it was on screen, and a frame
//! identical to the one before it is not encoded again but lengthens it. A still screen therefore
//! costs one frame however long it is recorded.
//!
//! An MP4 is written into a file or, for a recording that is reviewed before it is saved, into
//! memory (ADR 0012), from where [`Mp4Writer::finish_in_memory`] hands back its bytes.

use std::path::Path;
use std::time::Duration;

use captastic_core::{CaptureError, CaptureErrorKind};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFByteStream, IMFMediaType, IMFSample, IMFSinkWriter, MFCreateAttributes,
    MFCreateMFByteStreamOnStream, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFMediaType_Video, MFShutdown, MFStartup,
    MFTranscodeContainerType_MPEG4, MFVideoFormat_H264, MFVideoFormat_RGB32,
    MFVideoInterlace_Progressive, MFSTARTUP_FULL, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MAX_KEYFRAME_SPACING, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_TRANSCODE_CONTAINERTYPE, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::SHCreateMemStream;

/// The largest frame the Windows H.264 encoder accepts (level 5.1). Larger recordings are scaled
/// to fit inside it.
pub const MP4_MAXIMUM_SIZE: (u32, u32) = (4096, 2304);
/// Bits per pixel per frame for the target bitrate. Screen content is mostly flat and mostly
/// still, so this is generous for text and well under what camera footage needs.
const BITS_PER_PIXEL: f64 = 0.08;
const MINIMUM_BITRATE: u32 = 1_000_000;
const MAXIMUM_BITRATE: u32 = 40_000_000;
/// Media Foundation counts time in 100-nanosecond units.
const TICKS_PER_SECOND: u128 = 10_000_000;
/// The most copied into or out of a byte stream in one call.
const BYTE_STREAM_CHUNK: usize = 16 * 1024 * 1024;

/// The size a `width` by `height` recording is encoded at: scaled down, keeping its shape, to fit
/// [`MP4_MAXIMUM_SIZE`], then rounded down to even sides, which H.264's 4:2:0 sampling needs.
pub fn mp4_frame_size(width: u32, height: u32) -> (u32, u32) {
    let (limit_width, limit_height) = MP4_MAXIMUM_SIZE;
    let scale = (f64::from(limit_width) / f64::from(width.max(1)))
        .min(f64::from(limit_height) / f64::from(height.max(1)))
        .min(1.0);
    let scaled = |value: u32| ((f64::from(value) * scale) as u32 & !1).max(2);
    (scaled(width), scaled(height))
}

/// What a finished MP4 contains.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoSummary {
    pub frames_received: u64,
    /// Frames encoded; the rest repeated the frame before and lengthened it.
    pub frames_written: u64,
    pub duration: Duration,
}

/// Media Foundation and COM, started for this thread and stopped when dropped.
pub(crate) struct MediaFoundation {
    uninitialize_com: bool,
}

impl MediaFoundation {
    pub(crate) fn start() -> Result<Self, CaptureError> {
        // SAFETY: Initializes COM for this thread; a thread already in another apartment answers
        // RPC_E_CHANGED_MODE, which still leaves COM usable and must not be balanced.
        let uninitialize_com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        // SAFETY: Starts Media Foundation for this process; balanced by MFShutdown in Drop.
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            if uninitialize_com {
                // SAFETY: Balances the successful CoInitializeEx above.
                unsafe { CoUninitialize() };
            }
            return Err(video_error("start_media_foundation", error));
        }
        Ok(Self { uninitialize_com })
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        // SAFETY: Balances the MFStartup in start, after every Media Foundation object this
        // thread made has been released.
        let _ = unsafe { MFShutdown() };
        if self.uninitialize_com {
            // SAFETY: Balances the successful CoInitializeEx in start.
            unsafe { CoUninitialize() };
        }
    }
}

/// A frame waiting for the next to learn how long it was on screen.
struct Pending {
    sample: IMFSample,
    at: i64,
}

/// Writes an MP4 one captured frame at a time. Lives on one thread from creation to
/// [`finish`](Self::finish): Media Foundation is started for the thread that creates it.
pub struct Mp4Writer {
    // Declared before `_media_foundation` so the writer is released before Media Foundation shuts
    // down.
    writer: IMFSinkWriter,
    pending: Option<Pending>,
    previous: Option<Vec<u8>>,
    width: u32,
    height: u32,
    stream: u32,
    first_at: Option<Duration>,
    summary: VideoSummary,
    /// Where the MP4 is written when it is written into memory.
    memory: Option<IMFByteStream>,
    _media_foundation: MediaFoundation,
}

impl Mp4Writer {
    /// Creates `path` for an H.264 MP4 of `width` by `height` pixels, which must already be a
    /// [`mp4_frame_size`]. `frame_rate` is the rate frames are captured at, used for the bitrate
    /// and the stream's nominal rate; each frame's real timing is its own.
    pub fn create(
        path: &Path,
        width: u32,
        height: u32,
        frame_rate: u32,
    ) -> Result<Self, CaptureError> {
        Self::create_into(Some(path), width, height, frame_rate)
    }

    /// As [`create`](Self::create), but writes the MP4 into memory, for
    /// [`finish_in_memory`](Self::finish_in_memory) to hand back.
    pub fn create_in_memory(
        width: u32,
        height: u32,
        frame_rate: u32,
    ) -> Result<Self, CaptureError> {
        Self::create_into(None, width, height, frame_rate)
    }

    fn create_into(
        path: Option<&Path>,
        width: u32,
        height: u32,
        frame_rate: u32,
    ) -> Result<Self, CaptureError> {
        if (width, height) != mp4_frame_size(width, height) {
            return Err(video_failure(
                "mp4_frame_size",
                format!("{width}x{height} is not an encodable size"),
            ));
        }
        let media_foundation = MediaFoundation::start()?;
        let frame_size = (u64::from(width) << 32) | u64::from(height);
        let rate = (u64::from(frame_rate.max(1)) << 32) | 1;
        let bitrate =
            (f64::from(width) * f64::from(height) * f64::from(frame_rate) * BITS_PER_PIXEL)
                .clamp(f64::from(MINIMUM_BITRATE), f64::from(MAXIMUM_BITRATE)) as u32;
        let memory = match path {
            Some(_) => None,
            None => {
                Some(memory_byte_stream(&[]).map_err(|error| video_error("create_memory", error))?)
            }
        };
        // SAFETY: Each call below configures COM objects created here, on this thread, with GUIDs
        // and values that outlive the call.
        let writer = unsafe {
            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 2)
                .map_err(|error| video_error("create_attributes", error))?;
            let attributes = attributes.expect("MFCreateAttributes returns attributes on success");
            attributes
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .map_err(|error| video_error("enable_hardware_transforms", error))?;
            // MP4 whatever the file is called.
            attributes
                .SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)
                .map_err(|error| video_error("set_container_type", error))?;
            let writer = match path {
                Some(path) => {
                    let url = HSTRING::from(path.as_os_str());
                    MFCreateSinkWriterFromURL(PCWSTR(url.as_ptr()), None, &attributes)
                }
                None => MFCreateSinkWriterFromURL(PCWSTR::null(), memory.as_ref(), &attributes),
            }
            .map_err(|error| video_error("create_sink_writer", error))?;

            let output =
                MFCreateMediaType().map_err(|error| video_error("create_output_type", error))?;
            set_video_type(&output, &MFVideoFormat_H264, frame_size, rate)?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
                .map_err(|error| video_error("set_bitrate", error))?;
            // A key frame at least every second of written frames, so a review seeking in the
            // file decodes at most that much to reach any frame (ADR 0012).
            output
                .SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, frame_rate.max(1))
                .map_err(|error| video_error("set_keyframe_spacing", error))?;
            let stream = writer
                .AddStream(&output)
                .map_err(|error| video_error("add_video_stream", error))?;

            let input =
                MFCreateMediaType().map_err(|error| video_error("create_input_type", error))?;
            set_video_type(&input, &MFVideoFormat_RGB32, frame_size, rate)?;
            // A positive stride says the rows are top-down, as the capture's are; RGB in Media
            // Foundation is bottom-up unless told otherwise.
            input
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4)
                .map_err(|error| video_error("set_input_stride", error))?;
            writer
                .SetInputMediaType(stream, &input, None)
                .map_err(|error| video_error("set_input_type", error))?;
            writer
                .BeginWriting()
                .map_err(|error| video_error("begin_writing", error))?;
            (writer, stream)
        };
        Ok(Self {
            writer: writer.0,
            stream: writer.1,
            pending: None,
            previous: None,
            width,
            height,
            first_at: None,
            summary: VideoSummary::default(),
            memory,
            _media_foundation: media_foundation,
        })
    }

    /// Adds a frame of tightly packed BGRA pixels at the writer's size, captured `at` after the
    /// recording started.
    pub fn push(&mut self, bgra: &[u8], at: Duration) -> Result<(), CaptureError> {
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() != expected {
            return Err(video_failure(
                "mp4_frame_bytes",
                format!("expected {expected} bytes of BGRA, got {}", bgra.len()),
            ));
        }
        self.summary.frames_received += 1;
        let first_at = *self.first_at.get_or_insert(at);
        let at = ticks(at.saturating_sub(first_at));
        if self.previous.as_deref() == Some(bgra) {
            return Ok(());
        }
        let sample = self.sample(bgra)?;
        if let Some(pending) = self.pending.take() {
            self.write(pending, at)?;
        }
        self.pending = Some(Pending { sample, at });
        self.previous = Some(bgra.to_vec());
        Ok(())
    }

    /// Writes the last frame, shown until `end` after the recording started, and finalizes the
    /// file. A writer dropped without this leaves an unplayable file.
    pub fn finish(mut self, end: Duration) -> Result<VideoSummary, CaptureError> {
        self.finalize(end)
    }

    /// As [`finish`](Self::finish), for a writer from [`create_in_memory`](Self::create_in_memory):
    /// returns the finished MP4's bytes with what it contains.
    pub fn finish_in_memory(
        mut self,
        end: Duration,
    ) -> Result<(Vec<u8>, VideoSummary), CaptureError> {
        let summary = self.finalize(end)?;
        let memory = self.memory.as_ref().ok_or_else(|| {
            video_failure("mp4_in_memory", "the MP4 was written to a file".to_owned())
        })?;
        let bytes = read_byte_stream(memory).map_err(|error| video_error("read_memory", error))?;
        Ok((bytes, summary))
    }

    fn finalize(&mut self, end: Duration) -> Result<VideoSummary, CaptureError> {
        let first_at = self.first_at.unwrap_or(end);
        let end_duration = end.saturating_sub(first_at);
        if let Some(pending) = self.pending.take() {
            let end = ticks(end_duration).max(pending.at + 1);
            self.write(pending, end)?;
        }
        // SAFETY: The writer is this thread's, and every sample has been written.
        unsafe { self.writer.Finalize() }.map_err(|error| video_error("finalize_mp4", error))?;
        self.summary.duration = end_duration;
        Ok(self.summary)
    }

    fn sample(&self, bgra: &[u8]) -> Result<IMFSample, CaptureError> {
        let length = u32::try_from(bgra.len())
            .map_err(|_| video_failure("mp4_frame_bytes", "frame too large".to_owned()))?;
        // SAFETY: The buffer is created with room for `length` bytes, locked for exactly the copy
        // and unlocked before it is handed to the sample.
        unsafe {
            let buffer = MFCreateMemoryBuffer(length)
                .map_err(|error| video_error("create_buffer", error))?;
            let mut data = std::ptr::null_mut();
            buffer
                .Lock(&mut data, None, None)
                .map_err(|error| video_error("lock_buffer", error))?;
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), data, bgra.len());
            buffer
                .Unlock()
                .map_err(|error| video_error("unlock_buffer", error))?;
            buffer
                .SetCurrentLength(length)
                .map_err(|error| video_error("set_buffer_length", error))?;
            let sample = MFCreateSample().map_err(|error| video_error("create_sample", error))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|error| video_error("add_buffer", error))?;
            Ok(sample)
        }
    }

    fn write(&mut self, pending: Pending, until: i64) -> Result<(), CaptureError> {
        // SAFETY: The sample and writer are this thread's; times are in 100-ns units.
        unsafe {
            pending
                .sample
                .SetSampleTime(pending.at)
                .map_err(|error| video_error("set_sample_time", error))?;
            pending
                .sample
                .SetSampleDuration((until - pending.at).max(1))
                .map_err(|error| video_error("set_sample_duration", error))?;
            self.writer
                .WriteSample(self.stream, &pending.sample)
                .map_err(|error| video_error("write_sample", error))?;
        }
        self.summary.frames_written += 1;
        Ok(())
    }
}

/// Sets what every video type here shares: progressive frames of `frame_size` at `rate`, square
/// pixels, in `subtype`.
///
/// # Safety
/// `media_type` must be a live media type on this thread.
unsafe fn set_video_type(
    media_type: &IMFMediaType,
    subtype: &windows::core::GUID,
    frame_size: u64,
    rate: u64,
) -> Result<(), CaptureError> {
    // SAFETY: Per this function's contract; the GUIDs are statics.
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|error| video_error("set_major_type", error))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, subtype)
            .map_err(|error| video_error("set_subtype", error))?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|error| video_error("set_interlace_mode", error))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, frame_size)
            .map_err(|error| video_error("set_frame_size", error))?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, rate)
            .map_err(|error| video_error("set_frame_rate", error))?;
        media_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)
            .map_err(|error| video_error("set_pixel_aspect_ratio", error))?;
    }
    Ok(())
}

/// A growable byte stream in memory holding a copy of `bytes`, positioned at its start: where an
/// MP4 in memory is written, or read from.
///
/// A shell memory stream, because Media Foundation sees an `HGLOBAL` stream as read only.
pub(crate) fn memory_byte_stream(bytes: &[u8]) -> windows::core::Result<IMFByteStream> {
    // SAFETY: The stream copies `bytes` and owns its memory, released with it.
    unsafe {
        let stream = SHCreateMemStream((!bytes.is_empty()).then_some(bytes))
            .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY))?;
        MFCreateMFByteStreamOnStream(&stream)
    }
}

/// Everything in `byte_stream`, from its start.
fn read_byte_stream(byte_stream: &IMFByteStream) -> windows::core::Result<Vec<u8>> {
    // SAFETY: The stream is this thread's and the buffer has room for each read.
    unsafe {
        let length = usize::try_from(byte_stream.GetLength()?)
            .map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY))?;
        byte_stream.SetCurrentPosition(0)?;
        let mut bytes = vec![0; length];
        let mut filled = 0;
        while filled < length {
            let end = (filled + BYTE_STREAM_CHUNK).min(length);
            let mut read = 0_u32;
            byte_stream.Read(&mut bytes[filled..end], &mut read)?;
            if read == 0 {
                bytes.truncate(filled);
                break;
            }
            filled += read as usize;
        }
        Ok(bytes)
    }
}

fn ticks(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos() * TICKS_PER_SECOND / 1_000_000_000).unwrap_or(i64::MAX)
}

fn video_error(operation: &'static str, error: windows::core::Error) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-media-foundation",
        operation,
        message: error.message().to_string(),
        retryable: false,
        native_code: Some(i64::from(error.code().0)),
    }
}

fn video_failure(operation: &'static str, message: String) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::InvalidFrame,
        backend: "windows-media-foundation",
        operation,
        message,
        retryable: false,
        native_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_even_and_fit_the_encoder() {
        assert_eq!(mp4_frame_size(1920, 1080), (1920, 1080));
        assert_eq!(mp4_frame_size(1001, 601), (1000, 600));
        assert_eq!(mp4_frame_size(5120, 2880), (4096, 2304));
        assert_eq!(mp4_frame_size(3840, 2400), (3686, 2304));
        assert_eq!(mp4_frame_size(1, 1), (2, 2));
    }

    /// A `width` by `height` BGRA frame, red above the middle and blue below, or all green.
    fn frame(width: u32, height: u32, green: bool) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for _ in 0..width {
                let bgr = match (green, y < height / 2) {
                    (true, _) => [0, 200, 0],
                    (false, true) => [0, 0, 220],
                    (false, false) => [220, 0, 0],
                };
                pixels.extend_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
            }
        }
        pixels
    }

    #[test]
    #[ignore = "needs Media Foundation's H.264 encoder, which a Windows Server CI image may lack"]
    fn a_recording_round_trips_through_an_mp4_with_its_timing_and_orientation() {
        use windows::Win32::Media::MediaFoundation::{
            IMFAttributes, MFCreateSourceReaderFromURL, MF_SOURCE_READERF_ENDOFSTREAM,
            MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
        };

        let (width, height) = (128_u32, 96_u32);
        let path = std::env::temp_dir().join(format!("captastic-mp4-{}.mp4", std::process::id()));
        let mut writer = Mp4Writer::create(&path, width, height, 15).expect("writer");
        // Two seconds at 15 frames a second: the first second still, the second green.
        for step in 0..30_u64 {
            let at = Duration::from_micros(step * 66_667);
            writer
                .push(&frame(width, height, step >= 15), at)
                .expect("push");
        }
        let summary = writer.finish(Duration::from_secs(2)).expect("finish");
        // A still screen is one frame however long it lasts.
        assert_eq!(summary.frames_received, 30);
        assert_eq!(summary.frames_written, 2);

        let _media_foundation = MediaFoundation::start().expect("media foundation");
        let url = HSTRING::from(path.as_os_str());
        let mut samples = Vec::new();
        let mut first_frame = None;
        let stride;
        // SAFETY: Reads the file just written, on this thread, through objects created here.
        unsafe {
            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 1).expect("attributes");
            let attributes = attributes.expect("attributes");
            attributes
                .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
                .expect("video processing");
            let reader = MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), &attributes)
                .expect("source reader");
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let native = reader.GetNativeMediaType(stream, 0).expect("native type");
            let size = native.GetUINT64(&MF_MT_FRAME_SIZE).expect("frame size");
            assert_eq!(((size >> 32) as u32, size as u32), (width, height));
            let wanted = MFCreateMediaType().expect("type");
            wanted
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .expect("major");
            wanted
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .expect("subtype");
            reader
                .SetCurrentMediaType(stream, None, &wanted)
                .expect("decode to RGB32");
            let current = reader.GetCurrentMediaType(stream).expect("current type");
            stride = current
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map_or(width as i32 * 4, |stride| stride as i32);
            loop {
                let mut flags = 0_u32;
                let mut timestamp = 0_i64;
                let mut sample = None;
                reader
                    .ReadSample(
                        stream,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .expect("read sample");
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
                let Some(sample) = sample else { continue };
                let duration = sample.GetSampleDuration().unwrap_or(0);
                samples.push((timestamp, duration));
                if first_frame.is_none() {
                    let buffer = sample.ConvertToContiguousBuffer().expect("buffer");
                    let mut data = std::ptr::null_mut();
                    let mut length = 0_u32;
                    buffer
                        .Lock(&mut data, None, Some(&mut length))
                        .expect("lock");
                    let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
                    buffer.Unlock().expect("unlock");
                    first_frame = Some(bytes);
                }
            }
        }
        let _ = std::fs::remove_file(&path);
        // The first frame decodes the right way up: red at the top, blue at the bottom.
        let bytes = first_frame.expect("a decoded frame");
        let row = |y: u32| {
            let row_bytes = width as usize * 4;
            if stride < 0 {
                (height - 1 - y) as usize * row_bytes
            } else {
                y as usize * row_bytes
            }
        };
        let top = &bytes[row(4) + 64 * 4..row(4) + 64 * 4 + 3];
        let bottom = &bytes[row(height - 5) + 64 * 4..row(height - 5) + 64 * 4 + 3];
        assert!(top[2] > 150 && top[0] < 80, "top {top:?}");
        assert!(bottom[0] > 150 && bottom[2] < 80, "bottom {bottom:?}");
        // And the timing is the capture's: the samples together cover the two seconds.
        let end = samples
            .iter()
            .map(|(at, duration)| at + duration)
            .max()
            .expect("samples");
        assert!(
            (end - 20_000_000).abs() < 1_000_000,
            "the video ends at {end} ticks: {samples:?}"
        );
    }

    #[test]
    fn capture_time_becomes_media_foundation_ticks() {
        assert_eq!(ticks(Duration::from_millis(1)), 10_000);
        assert_eq!(ticks(Duration::from_secs(2)), 20_000_000);
    }
}
