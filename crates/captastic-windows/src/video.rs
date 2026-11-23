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
//!
//! A writer made with sound also carries an AAC stream beside the H.264 one (ADR 0015): the
//! computer's sound as [`crate::LoopbackCapture`] reads it, 48 kHz stereo at 160 kbit/s, on the
//! same clock as the frames. A writer without sound writes exactly what it always has.

use std::path::Path;
use std::time::{Duration, Instant};

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
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_DISABLE_THROTTLING,
    MF_TRANSCODE_CONTAINERTYPE, MF_VERSION,
};
use windows::Win32::Media::MediaFoundation::{
    MFAudioFormat_AAC, MFAudioFormat_PCM, MFMediaType_Audio, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
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
/// AAC at 160 kbit/s: 20,000 bytes a second, one of the four rates Windows' encoder takes.
const AAC_BYTES_PER_SECOND: u32 = 20_000;
/// How many frames may wait on the H.264 encoder in an unthrottled writer before new ones are
/// skipped: a second at 30 frames a second, at most about a gigabyte at 4K.
const MAXIMUM_ENCODER_BACKLOG: u64 = 30;
/// How long the encoder may encode nothing while frames are held back before it is taken to be
/// waiting for more input (a lookahead holding frames in flight) rather than falling behind.
const ENCODER_STALL: Duration = Duration::from_millis(250);

/// Decides whether an unthrottled writer's encoder is behind. Past [`MAXIMUM_ENCODER_BACKLOG`]
/// frames waiting it is, while it keeps encoding. One that encodes nothing for [`ENCODER_STALL`]
/// while frames are held back is waiting for input instead: frames it holds in flight (a
/// lookahead) that only more input releases. That many become its depth, the backlog it is allowed
/// on top of the maximum from then on, so it is not held back again a frame at a time.
#[derive(Debug, Default)]
struct EncoderPacer {
    /// Frames the encoder holds in flight, learned from a stall.
    depth: u64,
    /// While frames are held back: the encoder's count of encoded frames, and when it last moved.
    holding_back: Option<(u64, Instant)>,
}

impl EncoderPacer {
    /// Whether to hold back a frame, given `backlog` frames waiting and `encoded` encoded so far.
    fn is_behind(&mut self, backlog: u64, encoded: u64, now: Instant) -> bool {
        if backlog <= self.depth.saturating_add(MAXIMUM_ENCODER_BACKLOG) {
            return false;
        }
        match self.holding_back {
            Some((last, since)) if last == encoded => {
                if now.saturating_duration_since(since) < ENCODER_STALL {
                    return true;
                }
                self.depth = backlog.saturating_sub(MAXIMUM_ENCODER_BACKLOG);
                self.holding_back = None;
                false
            }
            _ => {
                self.holding_back = Some((encoded, now));
                true
            }
        }
    }

    /// A frame went to the encoder.
    fn fed(&mut self) {
        self.holding_back = None;
    }
}

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
    /// How much sound was written, when the MP4 has a sound stream.
    pub audio_duration: Duration,
    /// Frames skipped because the encoder was a second behind; the frame before lasted longer.
    pub frames_skipped: u64,
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
    /// The sound stream, for a writer made with sound.
    audio_stream: Option<u32>,
    /// Sound that arrived before the first frame, which the timeline starts at.
    early_audio: Vec<crate::AudioPacket>,
    /// Sound frames written, at [`crate::AUDIO_SAMPLE_RATE`].
    audio_frames: u64,
    first_at: Option<Duration>,
    summary: VideoSummary,
    /// Where the MP4 is written when it is written into memory.
    memory: Option<IMFByteStream>,
    /// Waits for a behind encoder instead of skipping frames: for re-encoding a recording, where
    /// every frame is already there and none may be lost, rather than capturing one live.
    paced: bool,
    pacer: EncoderPacer,
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
        Self::create_into(Some(path), width, height, frame_rate, false)
    }

    /// As [`create`](Self::create) or [`create_in_memory`](Self::create_in_memory) (`path`
    /// `None`), with a sound stream when `sound` is set, for [`push_audio`](Self::push_audio).
    pub fn create_recording(
        path: Option<&Path>,
        width: u32,
        height: u32,
        frame_rate: u32,
        sound: bool,
    ) -> Result<Self, CaptureError> {
        Self::create_into(path, width, height, frame_rate, sound)
    }

    /// As [`create`](Self::create), but writes the MP4 into memory, for
    /// [`finish_in_memory`](Self::finish_in_memory) to hand back.
    pub fn create_in_memory(
        width: u32,
        height: u32,
        frame_rate: u32,
    ) -> Result<Self, CaptureError> {
        Self::create_into(None, width, height, frame_rate, false)
    }

    fn create_into(
        path: Option<&Path>,
        width: u32,
        height: u32,
        frame_rate: u32,
        sound: bool,
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
            MFCreateAttributes(&mut attributes, 3)
                .map_err(|error| video_error("create_attributes", error))?;
            let attributes = attributes.expect("MFCreateAttributes returns attributes on success");
            attributes
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
                .map_err(|error| video_error("enable_hardware_transforms", error))?;
            // While the picture is still, no frame reaches the sink writer, but sound does, from
            // this same thread. Throttled, the writer would block the sound waiting for a frame
            // that only this thread could write, and the recording would never stop. Unthrottled,
            // it holds the sound until the next frame: about 20 KB a second.
            if sound {
                attributes
                    .SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)
                    .map_err(|error| video_error("disable_throttling", error))?;
            }
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
            let audio_stream = if sound {
                Some(add_audio_stream(&writer)?)
            } else {
                None
            };
            writer
                .BeginWriting()
                .map_err(|error| video_error("begin_writing", error))?;
            (writer, stream, audio_stream)
        };
        Ok(Self {
            writer: writer.0,
            stream: writer.1,
            audio_stream: writer.2,
            early_audio: Vec::new(),
            audio_frames: 0,
            pending: None,
            previous: None,
            width,
            height,
            first_at: None,
            summary: VideoSummary::default(),
            memory,
            paced: false,
            pacer: EncoderPacer::default(),
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
        let first = self.first_at.is_none();
        let first_at = *self.first_at.get_or_insert(at);
        if first {
            for packet in std::mem::take(&mut self.early_audio) {
                self.push_audio(&packet, None)?;
            }
        }
        let at = ticks(at.saturating_sub(first_at));
        if self.previous.as_deref() == Some(bgra) {
            return Ok(());
        }
        // An unthrottled writer (one with sound) never pushes back, so a slow encoder would queue
        // uncompressed frames inside Media Foundation without limit. Past a second of backlog a
        // live recording skips the frame, as its own queue drops frames when the encoder is
        // behind; a paced writer waits instead.
        if self.audio_stream.is_some() && self.encoder_is_behind() {
            if !self.paced {
                self.summary.frames_skipped += 1;
                return Ok(());
            }
            while self.encoder_is_behind() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        self.pacer.fed();
        let sample = self.sample(bgra)?;
        if let Some(pending) = self.pending.take() {
            self.write(pending, at)?;
        }
        self.pending = Some(Pending { sample, at });
        self.previous = Some(bgra.to_vec());
        Ok(())
    }

    /// Makes the writer wait for a behind encoder rather than skip frames, for re-encoding a
    /// recording that is already whole.
    pub fn pace_to_encoder(&mut self) {
        self.paced = true;
    }

    /// Whether the encoder is behind, as [`EncoderPacer`] decides.
    fn encoder_is_behind(&mut self) -> bool {
        let (backlog, encoded) = self.encoder_progress();
        self.pacer.is_behind(backlog, encoded, Instant::now())
    }

    /// Frames handed to the sink writer that its encoder has not yet encoded.
    #[cfg(test)]
    fn encoder_backlog(&self) -> u64 {
        self.encoder_progress().0
    }

    /// Frames waiting on the encoder, and frames it has encoded.
    fn encoder_progress(&self) -> (u64, u64) {
        let mut statistics = windows::Win32::Media::MediaFoundation::MF_SINK_WRITER_STATISTICS {
            cb: std::mem::size_of::<windows::Win32::Media::MediaFoundation::MF_SINK_WRITER_STATISTICS>(
            ) as u32,
            ..Default::default()
        };
        // SAFETY: The writer is this thread's and the statistics are a local of the size given.
        match unsafe { self.writer.GetStatistics(self.stream, &mut statistics) } {
            Ok(()) => (
                statistics
                    .qwNumSamplesReceived
                    .saturating_sub(statistics.qwNumSamplesEncoded),
                statistics.qwNumSamplesEncoded,
            ),
            Err(_) => (0, 0),
        }
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

    /// Adds a stretch of sound, timed from the recording's start as the frames are. Sound before
    /// the first frame is cut, as is sound past `end` once the recording's end is known. A writer
    /// made without sound ignores it.
    pub fn push_audio(
        &mut self,
        packet: &crate::AudioPacket,
        end: Option<Duration>,
    ) -> Result<(), CaptureError> {
        let Some(stream) = self.audio_stream else {
            return Ok(());
        };
        let Some(first_at) = self.first_at else {
            self.early_audio.push(packet.clone());
            return Ok(());
        };
        let channels = usize::from(crate::AUDIO_CHANNELS);
        let rate = u64::from(crate::AUDIO_SAMPLE_RATE);
        let frames_at = |at: Duration| (at.as_nanos() * u128::from(rate) / 1_000_000_000) as u64;
        let packet_start = frames_at(packet.start);
        let packet_end = packet_start + packet.frames() as u64;
        let from = packet_start.max(frames_at(first_at));
        let until = end.map_or(packet_end, |end| packet_end.min(frames_at(end)));
        if until <= from {
            return Ok(());
        }
        let samples = &packet.samples
            [(from - packet_start) as usize * channels..(until - packet_start) as usize * channels];
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        let sample = self.sample(&bytes)?;
        let start = ticks(Duration::from_nanos(
            ((from as u128 * 1_000_000_000) / u128::from(rate)) as u64,
        )) - ticks(first_at);
        let length = ((until - from) as u128 * TICKS_PER_SECOND / u128::from(rate)) as i64;
        // SAFETY: The sample and writer are this thread's; times are in 100-ns units.
        unsafe {
            sample
                .SetSampleTime(start.max(0))
                .map_err(|error| video_error("set_audio_time", error))?;
            sample
                .SetSampleDuration(length.max(1))
                .map_err(|error| video_error("set_audio_duration", error))?;
            self.writer
                .WriteSample(stream, &sample)
                .map_err(|error| video_error("write_audio", error))?;
        }
        self.audio_frames += until - from;
        Ok(())
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
        self.summary.audio_duration = Duration::from_nanos(
            (u128::from(self.audio_frames) * 1_000_000_000 / u128::from(crate::AUDIO_SAMPLE_RATE))
                as u64,
        );
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

/// Adds the AAC stream, fed 16-bit PCM at the loopback's rate and channels, to `writer`.
///
/// # Safety
/// `writer` must be a live sink writer on this thread that has not begun writing.
unsafe fn add_audio_stream(writer: &IMFSinkWriter) -> Result<u32, CaptureError> {
    let rate = crate::AUDIO_SAMPLE_RATE;
    let channels = u32::from(crate::AUDIO_CHANNELS);
    // SAFETY: Per this function's contract; the types are created here and the GUIDs are statics.
    unsafe {
        let set = |media_type: &IMFMediaType, subtype| -> Result<(), CaptureError> {
            media_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|error| video_error("set_audio_major_type", error))?;
            media_type
                .SetGUID(&MF_MT_SUBTYPE, subtype)
                .map_err(|error| video_error("set_audio_subtype", error))?;
            media_type
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(|error| video_error("set_audio_bits", error))?;
            media_type
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)
                .map_err(|error| video_error("set_audio_rate", error))?;
            media_type
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)
                .map_err(|error| video_error("set_audio_channels", error))?;
            Ok(())
        };
        let output =
            MFCreateMediaType().map_err(|error| video_error("create_audio_output_type", error))?;
        set(&output, &MFAudioFormat_AAC)?;
        output
            .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)
            .map_err(|error| video_error("set_audio_bitrate", error))?;
        // Raw AAC, as MP4 carries it.
        output
            .SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
            .map_err(|error| video_error("set_aac_payload", error))?;
        let stream = writer
            .AddStream(&output)
            .map_err(|error| video_error("add_audio_stream", error))?;
        let input =
            MFCreateMediaType().map_err(|error| video_error("create_audio_input_type", error))?;
        set(&input, &MFAudioFormat_PCM)?;
        input
            .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, channels * 2)
            .map_err(|error| video_error("set_audio_block_alignment", error))?;
        input
            .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, rate * channels * 2)
            .map_err(|error| video_error("set_audio_input_rate", error))?;
        writer
            .SetInputMediaType(stream, &input, None)
            .map_err(|error| video_error("set_audio_input_type", error))?;
        Ok(stream)
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
    fn an_encoder_that_holds_frames_in_flight_is_held_back_once_not_a_frame_at_a_time() {
        let start = Instant::now();
        let later = |ms| start + Duration::from_millis(ms);
        let mut pacer = EncoderPacer::default();
        let deep = MAXIMUM_ENCODER_BACKLOG + 10;
        assert!(!pacer.is_behind(MAXIMUM_ENCODER_BACKLOG, 0, start));
        // Behind, and nothing encoded meanwhile: held back until the stall, then let through.
        assert!(pacer.is_behind(deep, 0, start));
        assert!(pacer.is_behind(deep, 0, later(100)));
        assert!(!pacer.is_behind(deep, 0, later(250)));
        pacer.fed();
        // From then on its depth is allowed: as it gives back a frame for each one fed, none is
        // held back again.
        for frame in 1..=MAXIMUM_ENCODER_BACKLOG {
            assert!(!pacer.is_behind(deep, frame, later(260 + frame)), "{frame}");
            pacer.fed();
        }
        // But a backlog beyond its depth and the maximum still is, while it keeps encoding.
        let behind = deep + MAXIMUM_ENCODER_BACKLOG;
        assert!(pacer.is_behind(behind, 40, later(400)));
        assert!(pacer.is_behind(behind, 41, later(500)));
        assert!(pacer.is_behind(behind, 42, later(900)));
    }

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

    #[test]
    #[ignore = "needs Media Foundation's H.264 and AAC encoders, which a Windows Server CI image may lack"]
    fn an_mp4_with_sound_carries_it_in_step_with_the_frames() {
        let (width, height) = (160_u32, 96_u32);
        let mut writer =
            Mp4Writer::create_recording(None, width, height, 30, true).expect("writer");
        let frame = vec![128_u8; (width * height * 4) as usize];
        // A second of silence, then a second of a 440 Hz tone, in 10 ms packets; the frames
        // arrive with them, as a recording's do.
        let rate = crate::AUDIO_SAMPLE_RATE as usize;
        let packet_frames = rate / 100;
        for packet in 0..200 {
            let start = Duration::from_millis(packet as u64 * 10);
            let samples: Vec<i16> = (0..packet_frames)
                .flat_map(|i| {
                    let n = packet * packet_frames + i;
                    let value = if packet >= 100 {
                        (12_000.0
                            * (2.0 * std::f64::consts::PI * 440.0 * n as f64 / rate as f64).sin())
                            as i16
                    } else {
                        0
                    };
                    [value, value]
                })
                .collect();
            if packet % 3 == 0 {
                writer.push(&frame, start).expect("frame");
            }
            writer
                .push_audio(&crate::AudioPacket { start, samples }, None)
                .expect("sound");
        }
        let (bytes, summary) = writer
            .finish_in_memory(Duration::from_secs(2))
            .expect("finish");
        assert!(
            summary.audio_duration.abs_diff(Duration::from_secs(2)) < Duration::from_millis(15),
            "{summary:?}"
        );
        let sound = crate::mp4_sound(&bytes)
            .expect("decoded")
            .expect("the MP4 has a sound stream");
        let seconds = sound.len() as f64 / 2.0 / rate as f64;
        assert!((seconds - 2.0).abs() < 0.1, "{seconds} s of sound decoded");
        let rms = |part: &[i16]| {
            (part.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / part.len().max(1) as f64)
                .sqrt()
        };
        // The first second stays quiet and the second carries the tone, at about its level.
        let half = sound.len() / 2;
        let quiet = rms(&sound[rate / 10 * 2..half - rate / 10 * 2]);
        let tone = rms(&sound[half + rate / 10 * 2..sound.len() - rate / 10 * 2]);
        assert!(quiet < 100.0, "silence decoded at RMS {quiet}");
        assert!(
            (tone - 8_485.0).abs() < 1_500.0,
            "tone decoded at RMS {tone}"
        );
    }

    #[test]
    #[ignore = "needs Media Foundation's H.264 and AAC encoders, which a Windows Server CI image may lack"]
    fn a_writer_with_sound_bounds_what_waits_on_its_encoder() {
        // 4K frames that all differ, pushed far faster than real time, with sound: the writer
        // asks how far its encoder is behind and skips frames past a second of backlog.
        let (width, height) = (3840_u32, 2160_u32);
        let mut writer =
            Mp4Writer::create_recording(None, width, height, 30, true).expect("writer");
        let rate = crate::AUDIO_SAMPLE_RATE as usize;
        for index in 0..240_u64 {
            let at = Duration::from_millis(index * 33);
            let frame = vec![(index % 250) as u8; (width * height * 4) as usize];
            writer.push(&frame, at).expect("frame");
            assert!(writer.encoder_backlog() <= MAXIMUM_ENCODER_BACKLOG + 1);
            writer
                .push_audio(
                    &crate::AudioPacket {
                        start: at,
                        samples: vec![0; rate / 30 * 2],
                    },
                    None,
                )
                .expect("sound");
        }
        let (_, summary) = writer
            .finish_in_memory(Duration::from_millis(240 * 33))
            .expect("finish");
        eprintln!("{summary:?}");
        assert_eq!(summary.frames_received, 240);
        assert_eq!(summary.frames_written + summary.frames_skipped, 240);
    }

    #[test]
    #[ignore = "needs Media Foundation's H.264 and AAC encoders, which a Windows Server CI image may lack"]
    fn a_paced_writer_with_sound_keeps_every_frame() {
        // As re-encoding an edited recording does: frames far faster than real time, none skipped.
        let (width, height) = (1920_u32, 1080_u32);
        let mut writer =
            Mp4Writer::create_recording(None, width, height, 30, true).expect("writer");
        writer.pace_to_encoder();
        for index in 0..120_u64 {
            let frame = vec![(index % 250) as u8; (width * height * 4) as usize];
            writer
                .push(&frame, Duration::from_millis(index * 33))
                .expect("frame");
        }
        let (_, summary) = writer
            .finish_in_memory(Duration::from_millis(120 * 33))
            .expect("finish");
        assert_eq!(summary.frames_skipped, 0, "{summary:?}");
        assert_eq!(summary.frames_written, 120, "{summary:?}");
    }

    #[test]
    #[ignore = "needs Media Foundation's H.264 encoder, which a Windows Server CI image may lack"]
    fn sound_is_ignored_by_a_writer_made_without_it() {
        let mut writer = Mp4Writer::create_in_memory(160, 96, 30).expect("writer");
        writer
            .push(&vec![0_u8; 160 * 96 * 4], Duration::ZERO)
            .expect("frame");
        writer
            .push_audio(
                &crate::AudioPacket {
                    start: Duration::ZERO,
                    samples: vec![1_000; 960],
                },
                None,
            )
            .expect("ignored");
        let (_, summary) = writer
            .finish_in_memory(Duration::from_millis(100))
            .expect("finish");
        assert_eq!(summary.audio_duration, Duration::ZERO);
    }

    #[test]
    #[ignore = "needs Media Foundation's H.264 and AAC encoders, which a Windows Server CI image may lack"]
    fn a_still_picture_with_long_sound_does_not_block_and_keeps_its_sound() {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (width, height) = (640_u32, 360_u32);
            let mut writer =
                Mp4Writer::create_recording(None, width, height, 30, true).expect("writer");
            let frame = vec![90_u8; (width * height * 4) as usize];
            let rate = crate::AUDIO_SAMPLE_RATE as usize;
            // 90 s: a frame every 33 ms (all identical), sound every 10 ms.
            for tick in 0..9_000_u64 {
                let at = Duration::from_millis(tick * 10);
                if tick % 3 == 0 {
                    writer.push(&frame, at).expect("frame");
                }
                let samples = vec![500_i16; rate / 100 * 2];
                writer
                    .push_audio(&crate::AudioPacket { start: at, samples }, None)
                    .expect("sound");
            }
            let (bytes, summary) = writer
                .finish_in_memory(Duration::from_secs(90))
                .expect("finish");
            let _ = done.send((bytes, summary));
        });
        // Before the fix the writer blocked within the first ten seconds of sound.
        let (bytes, summary) = finished
            .recv_timeout(Duration::from_secs(60))
            .expect("the writer blocked on a still picture with sound");
        assert_eq!(summary.frames_written, 1, "{summary:?}");
        let sound = crate::mp4_sound(&bytes)
            .expect("decoded")
            .expect("a sound stream");
        let seconds = sound.len() as f64 / 2.0 / f64::from(crate::AUDIO_SAMPLE_RATE);
        assert!((seconds - 90.0).abs() < 0.2, "{seconds} s of sound");
    }
}
