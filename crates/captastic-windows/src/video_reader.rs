//! Reading an MP4 recording back for review (ADR 0012), through Media Foundation's source reader.
//!
//! [`Mp4Reader`] reads the file twice over. Opening it reads every sample still compressed, which
//! is quick and gives the [`Timeline`]; frames are then decoded on demand by a second reader that
//! converts them to BGRA, reading on from the frame before when frames are asked for in order and
//! seeking to the key frame before the one wanted otherwise. [`crate::Mp4Writer`] puts a key frame
//! at least every second, so a jump decodes at most a second of video.
//!
//! The MP4 is read from a file or, for a recording still in review, from memory.

use std::path::Path;
use std::time::Duration;

use captastic_core::{PlaybackError, RecordingFrames, Timeline};
use windows::core::{w, ComInterface, GUID, HSTRING, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, IMFSourceReader, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromByteStream, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFVideoArea, MFVideoFormat_RGB32, MF_BYTESTREAM_CONTENT_TYPE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0};
use windows::Win32::System::Variant::VT_I8;

use crate::video::MediaFoundation;

/// Reading on decodes every frame between; past this many, seeking is quicker.
const READ_ON_LIMIT: usize = 45;

/// How the decoded frames are laid out, which can change while reading.
#[derive(Clone, Copy)]
struct DecodedLayout {
    /// The decoded frame, which can be taller than the picture: H.264 codes 16-row blocks.
    frame_height: u32,
    /// Bytes from one row to the next; negative when rows are stored bottom up.
    stride: i32,
}

/// Reads an MP4 back for playback. Lives on one thread from [`open`](Self::open) on, since Media
/// Foundation is started for the thread that opens it.
pub struct Mp4Reader {
    // Declared before `_media_foundation` so the reader is released before it shuts down.
    reader: IMFSourceReader,
    layout: DecodedLayout,
    width: u32,
    height: u32,
    timeline: Timeline,
    /// The first sample's time, which the timeline counts from.
    origin: i64,
    pixels: Vec<u8>,
    decoded: Option<usize>,
    at_end: bool,
    _media_foundation: MediaFoundation,
}

fn failure(operation: &str, error: impl std::fmt::Display) -> PlaybackError {
    PlaybackError(format!("{operation}: {error}"))
}

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// Where an MP4 is read from.
#[derive(Clone, Copy)]
enum Source<'a> {
    File(&'a Path),
    Memory(&'a [u8]),
}

impl Source<'_> {
    /// A source reader of the MP4, made with `attributes`.
    ///
    /// # Safety
    /// Media Foundation must be started on this thread, which the reader is then used on.
    unsafe fn reader(
        self,
        attributes: Option<&IMFAttributes>,
    ) -> windows::core::Result<IMFSourceReader> {
        // SAFETY: Per this function's contract; every object is made here.
        unsafe {
            match self {
                Self::File(path) => {
                    let url = HSTRING::from(path.as_os_str());
                    MFCreateSourceReaderFromURL(PCWSTR(url.as_ptr()), attributes)
                }
                Self::Memory(bytes) => {
                    let stream = crate::video::memory_byte_stream(bytes)?;
                    // Without a file name to go by, the stream says what it holds.
                    if let Ok(stream_attributes) = stream.cast::<IMFAttributes>() {
                        let _ = stream_attributes
                            .SetString(&MF_BYTESTREAM_CONTENT_TYPE, w!("video/mp4"));
                    }
                    MFCreateSourceReaderFromByteStream(&stream, attributes)
                }
            }
        }
    }
}

impl Mp4Reader {
    pub fn open(path: &Path) -> Result<Self, PlaybackError> {
        Self::open_source(Source::File(path))
    }

    /// Reads the MP4 held in `bytes`, which it copies.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PlaybackError> {
        Self::open_source(Source::Memory(bytes))
    }

    fn open_source(source: Source<'_>) -> Result<Self, PlaybackError> {
        let media_foundation = MediaFoundation::start()
            .map_err(|error| failure("starting Media Foundation", error))?;
        // SAFETY: Every object below is created here, on this thread, and used only by it.
        let (timeline, origin, width, height, reader, layout) = unsafe {
            let index = source
                .reader(None)
                .map_err(|error| failure("opening the MP4", error))?;
            let native = index
                .GetNativeMediaType(STREAM, 0)
                .map_err(|error| failure("reading the MP4's video type", error))?;
            let (width, height) = picture_size(&native)?;
            let mut times = Vec::new();
            let mut end = 0_i64;
            loop {
                let mut flags = 0_u32;
                let mut timestamp = 0_i64;
                let mut sample = None;
                index
                    .ReadSample(
                        STREAM,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .map_err(|error| failure("indexing the MP4", error))?;
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
                let Some(sample) = sample else { continue };
                let duration = sample.GetSampleDuration().unwrap_or(0);
                times.push(timestamp);
                end = end.max(timestamp + duration);
            }
            times.sort_unstable();
            times.dedup();
            let Some(&origin) = times.first() else {
                return Err(PlaybackError("the MP4 has no frames".to_owned()));
            };
            let starts: Vec<Duration> = times.iter().map(|time| duration(time - origin)).collect();
            let last = *times.last().unwrap_or(&origin);
            let timeline = Timeline::new(starts, duration(end.max(last + 1) - origin))?;

            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 1)
                .map_err(|error| failure("creating reader attributes", error))?;
            let attributes = attributes.expect("MFCreateAttributes returns attributes on success");
            attributes
                .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
                .map_err(|error| failure("enabling video processing", error))?;
            let reader = source
                .reader(Some(&attributes))
                .map_err(|error| failure("opening the MP4 to decode", error))?;
            let wanted =
                MFCreateMediaType().map_err(|error| failure("creating a media type", error))?;
            wanted
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|error| failure("setting the major type", error))?;
            wanted
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(|error| failure("setting the subtype", error))?;
            reader
                .SetCurrentMediaType(STREAM, None, &wanted)
                .map_err(|error| failure("decoding to BGRA", error))?;
            let layout = decoded_layout(&reader, width, height)?;
            (timeline, origin, width, height, reader, layout)
        };
        Ok(Self {
            reader,
            layout,
            width,
            height,
            timeline,
            origin,
            pixels: vec![0; width as usize * height as usize * 4],
            decoded: None,
            at_end: false,
            _media_foundation: media_foundation,
        })
    }

    fn seek(&mut self, index: usize) -> Result<(), PlaybackError> {
        let position = self.origin + ticks(self.timeline.start(index));
        let variant = PROPVARIANT {
            Anonymous: PROPVARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                    vt: VT_I8,
                    Anonymous: windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0_0_0 {
                        hVal: position,
                    },
                    ..Default::default()
                }),
            },
        };
        // SAFETY: The reader is this thread's; an all-zero GUID means 100-ns units, and the
        // variant holds a plain integer, so there is nothing for it to own or free.
        unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &variant) }
            .map_err(|error| failure("seeking in the MP4", error))?;
        self.decoded = None;
        self.at_end = false;
        Ok(())
    }

    /// Decodes the next sample into `pixels`, returning its frame, or `None` at the end.
    fn read_next(&mut self) -> Result<Option<usize>, PlaybackError> {
        if self.at_end {
            return Ok(None);
        }
        loop {
            let mut flags = 0_u32;
            let mut timestamp = 0_i64;
            let mut sample = None;
            // SAFETY: The reader is this thread's; the out-parameters live for the call.
            unsafe {
                self.reader.ReadSample(
                    STREAM,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
            }
            .map_err(|error| failure("decoding the MP4", error))?;
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                // SAFETY: As above.
                self.layout = unsafe { decoded_layout(&self.reader, self.width, self.height) }?;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                self.at_end = true;
                return Ok(None);
            }
            let Some(sample) = sample else { continue };
            // SAFETY: The buffer is locked for exactly the copy and unlocked after it.
            unsafe {
                let buffer = sample
                    .ConvertToContiguousBuffer()
                    .map_err(|error| failure("reading a decoded frame", error))?;
                let mut data = std::ptr::null_mut();
                let mut length = 0_u32;
                buffer
                    .Lock(&mut data, None, Some(&mut length))
                    .map_err(|error| failure("locking a decoded frame", error))?;
                let bytes = std::slice::from_raw_parts(data, length as usize);
                let copied = copy_rows(
                    bytes,
                    self.layout,
                    self.width,
                    self.height,
                    &mut self.pixels,
                );
                let _ = buffer.Unlock();
                copied?;
            }
            let index = self
                .timeline
                .index_at(duration(timestamp.saturating_sub(self.origin)));
            self.decoded = Some(index);
            return Ok(Some(index));
        }
    }
}

impl RecordingFrames for Mp4Reader {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn timeline(&self) -> &Timeline {
        &self.timeline
    }

    fn frame(&mut self, index: usize) -> Result<&[u8], PlaybackError> {
        if index >= self.timeline.len() {
            return Err(PlaybackError(format!(
                "frame {index} is past the MP4's {} frames",
                self.timeline.len()
            )));
        }
        let reads_on = matches!(self.decoded, Some(current) if current < index && index - current <= READ_ON_LIMIT);
        if self.decoded != Some(index) && !reads_on {
            self.seek(index)?;
        }
        while self.decoded.is_none_or(|current| current < index) {
            if self.read_next()?.is_none() {
                break;
            }
        }
        Ok(&self.pixels)
    }
}

/// The picture's size: the display aperture when the file has one, else the coded frame size.
///
/// # Safety
/// `media_type` must be a live media type on this thread.
unsafe fn picture_size(media_type: &IMFMediaType) -> Result<(u32, u32), PlaybackError> {
    // SAFETY: Per this function's contract; the blob is read into a buffer of its own size.
    unsafe {
        let mut area = MFVideoArea::default();
        let bytes = std::slice::from_raw_parts_mut(
            std::ptr::addr_of_mut!(area).cast::<u8>(),
            std::mem::size_of::<MFVideoArea>(),
        );
        if media_type
            .GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None)
            .is_ok()
            && area.Area.cx > 0
            && area.Area.cy > 0
        {
            return Ok((area.Area.cx as u32, area.Area.cy as u32));
        }
        let size = media_type
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(|error| failure("reading the MP4's frame size", error))?;
        Ok(((size >> 32) as u32, size as u32))
    }
}

/// # Safety
/// `reader` must be a live source reader on this thread.
unsafe fn decoded_layout(
    reader: &IMFSourceReader,
    width: u32,
    height: u32,
) -> Result<DecodedLayout, PlaybackError> {
    // SAFETY: Per this function's contract.
    unsafe {
        let current = reader
            .GetCurrentMediaType(STREAM)
            .map_err(|error| failure("reading the decoded type", error))?;
        let frame_height = current
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .map_or(height, |size| size as u32)
            .max(height);
        let stride = current
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map_or(width as i32 * 4, |stride| stride as i32);
        Ok(DecodedLayout {
            frame_height,
            stride,
        })
    }
}

/// Copies the picture's rows out of a decoded frame, top row first, as opaque BGRA.
fn copy_rows(
    bytes: &[u8],
    layout: DecodedLayout,
    width: u32,
    height: u32,
    out: &mut [u8],
) -> Result<(), PlaybackError> {
    let row_bytes = width as usize * 4;
    let stride = layout.stride.unsigned_abs() as usize;
    if stride < row_bytes || bytes.len() < stride * (layout.frame_height as usize - 1) + row_bytes {
        return Err(PlaybackError(format!(
            "a decoded frame of {} bytes is too small for {width}x{height}",
            bytes.len()
        )));
    }
    for y in 0..height as usize {
        let source_row = if layout.stride < 0 {
            layout.frame_height as usize - 1 - y
        } else {
            y
        };
        let from = &bytes[source_row * stride..source_row * stride + row_bytes];
        let to = &mut out[y * row_bytes..(y + 1) * row_bytes];
        to.copy_from_slice(from);
        for pixel in to.chunks_exact_mut(4) {
            pixel[3] = u8::MAX;
        }
    }
    Ok(())
}

/// Media Foundation's 100-nanosecond units as a duration; nothing before zero.
fn duration(ticks: i64) -> Duration {
    Duration::from_nanos(u64::try_from(ticks).unwrap_or(0) * 100)
}

fn ticks(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos() / 100).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_copied_top_first_whichever_way_up_they_are_stored() {
        // Two rows of one pixel, in a frame coded three rows tall with padding at the end.
        let layout = |stride| DecodedLayout {
            frame_height: 3,
            stride,
        };
        let stored = [1, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0];
        let mut out = [0_u8; 8];
        copy_rows(&stored, layout(6), 1, 2, &mut out).expect("top down");
        assert_eq!(out, [1, 0, 0, 255, 2, 0, 0, 255]);
        copy_rows(&stored, layout(-6), 1, 2, &mut out).expect("bottom up");
        assert_eq!(out, [3, 0, 0, 255, 2, 0, 0, 255]);
        assert!(copy_rows(&stored[..10], layout(6), 1, 2, &mut out).is_err());
    }

    #[test]
    fn ticks_and_durations_convert_both_ways() {
        assert_eq!(ticks(Duration::from_millis(1_500)), 15_000_000);
        assert_eq!(duration(15_000_000), Duration::from_millis(1_500));
        assert_eq!(duration(-5), Duration::ZERO);
    }

    /// A `width` by `height` frame whose top `filled` rows are white and the rest black, so a
    /// frame's picture says which it is.
    fn bars(width: u32, height: u32, filled: u32) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            let value = if y < filled { 255 } else { 0 };
            for _ in 0..width {
                pixels.extend_from_slice(&[value, value, value, 255]);
            }
        }
        pixels
    }

    fn white_rows(pixels: &[u8], width: u32, height: u32) -> u32 {
        (0..height)
            .filter(|&y| pixels[((y * width + width / 2) * 4) as usize] > 128)
            .count() as u32
    }

    #[test]
    #[ignore = "needs Media Foundation's H.264 encoder, which a Windows Server CI image may lack"]
    fn an_mp4_reads_back_with_its_timing_and_pictures_in_any_order() {
        let (width, height) = (160_u32, 96_u32);
        let path =
            std::env::temp_dir().join(format!("captastic-mp4-reader-{}.mp4", std::process::id()));
        let mut writer = crate::Mp4Writer::create(&path, width, height, 30).expect("writer");
        // Three seconds at 30 frames a second, every 8 frames adding 8 white rows.
        for step in 0..90_u32 {
            let filled = (step / 8 + 1) * 8;
            writer
                .push(
                    &bars(width, height, filled.min(height)),
                    Duration::from_micros(u64::from(step) * 33_333),
                )
                .expect("push");
        }
        writer.finish(Duration::from_secs(3)).expect("finish");

        let mut reader = Mp4Reader::open(&path).expect("reader");
        assert_eq!(reader.size(), (width, height));
        let timeline = reader.timeline().clone();
        // The encoder writes at a steady rate, so unchanged frames are in the file too; what
        // matters is that each frame's picture is the one captured at its time.
        assert!(timeline.len() >= 12, "{timeline:?}");
        assert!(
            timeline.duration().abs_diff(Duration::from_secs(3)) < Duration::from_millis(50),
            "{timeline:?}"
        );
        let expected = |index: usize| {
            let step = (timeline.start(index).as_micros() as f64 / 33_333.0).round() as u32;
            ((step / 8 + 1) * 8).min(height)
        };
        let last = timeline.len() - 1;
        for index in (0..=last)
            .chain((0..=last).rev())
            .chain([last / 2, 0, last, 7, last - 30])
        {
            let frame = reader.frame(index).expect("frame").to_vec();
            assert_eq!(
                white_rows(&frame, width, height),
                expected(index),
                "frame {index} at {:?}",
                timeline.start(index)
            );
        }
        drop(reader);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[ignore = "needs Media Foundation's H.264 encoder, which a Windows Server CI image may lack"]
    fn an_mp4_written_into_memory_reads_back_from_memory() {
        let (width, height) = (160_u32, 96_u32);
        let mut writer = crate::Mp4Writer::create_in_memory(width, height, 30).expect("writer");
        for step in 0..30_u32 {
            writer
                .push(
                    &bars(width, height, (step / 3 + 1) * 8),
                    Duration::from_micros(u64::from(step) * 33_333),
                )
                .expect("push");
        }
        let (bytes, summary) = writer
            .finish_in_memory(Duration::from_secs(1))
            .expect("finish");
        assert_eq!(
            &bytes[4..8],
            b"ftyp",
            "an MP4 starts with its file type box"
        );
        assert!(summary.frames_written >= 10, "{summary:?}");

        let mut reader = Mp4Reader::from_bytes(&bytes).expect("reader");
        assert_eq!(reader.size(), (width, height));
        let timeline = reader.timeline().clone();
        assert!(
            timeline.duration().abs_diff(Duration::from_secs(1)) < Duration::from_millis(50),
            "{timeline:?}"
        );
        let last = timeline.len() - 1;
        let frame = reader.frame(last).expect("last frame").to_vec();
        assert_eq!(white_rows(&frame, width, height), 80);
        let frame = reader.frame(0).expect("first frame").to_vec();
        assert_eq!(white_rows(&frame, width, height), 8);
    }
}
