//! The computer's sound, for a recording (ADR 0015): the mix Windows sends to the default output
//! device, read back through WASAPI's shared-mode loopback, as the user hears it.
//!
//! [`LoopbackCapture`] runs on its own thread for the length of a recording and hands over
//! [`AudioPacket`]s: 16-bit stereo at [`AUDIO_SAMPLE_RATE`], on the recording's clock, and
//! continuous from its start. Loopback delivers nothing while nothing plays, a device change leaves
//! a gap, and a device can vanish; every gap is filled with silence, so the track that reaches the
//! MP4 has no holes for a player to mistake for its end. Whatever the device's own rate, the packets
//! are resampled to one rate, so a device change partway cannot change the track's.
//!
//! The conversion, downmix, resampling and gap filling are plain functions over samples, tested
//! without a device. Only the thread that talks to WASAPI needs one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The rate every packet is at, whatever the device's.
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
/// Channels in every packet: left and right, interleaved.
pub const AUDIO_CHANNELS: u16 = 2;

/// How far behind the clock silence is filled while nothing arrives, so a packet that is merely
/// late is not mistaken for a gap and then written over silence already sent.
const IDLE_FILL_LAG: Duration = Duration::from_millis(100);
/// A packet starting this much later than the track has reached is a gap to fill.
const GAP_TOLERANCE_FRAMES: u64 = AUDIO_SAMPLE_RATE as u64 / 100;
/// How often the default device is checked for a change, and a missing one looked for again.
const DEVICE_CHECK: Duration = Duration::from_millis(500);
/// How long the thread sleeps between reads: a tenth of WASAPI's buffer.
const POLL: Duration = Duration::from_millis(10);

/// A stretch of the recording's sound.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioPacket {
    /// When it starts, from the recording's start.
    pub start: Duration,
    /// Left and right samples, interleaved, at [`AUDIO_SAMPLE_RATE`].
    pub samples: Vec<i16>,
}

impl AudioPacket {
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(AUDIO_CHANNELS)
    }
}

/// What happened to the sound over a recording.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoopbackSummary {
    /// Whether any output device was ever read. When not, the track is all silence and `error`
    /// says why.
    pub device_opened: bool,
    /// How many times the default device changed, or vanished and came back.
    pub device_changes: u32,
    /// Frames sent, sound and silence together.
    pub frames: u64,
    /// Frames of silence sent to fill a gap.
    pub silent_frames: u64,
    /// Packets the queue had no room for, which leave a hole in the track.
    pub packets_dropped: u64,
    /// Why no device could be read, if none ever was.
    pub error: Option<String>,
}

/// Reads the default output device's mix on its own thread until stopped.
pub struct LoopbackCapture {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<LoopbackSummary>>,
}

impl LoopbackCapture {
    /// Starts reading, with packets timed from `started`, the instant the recording's frames are
    /// timed from. Never fails: a device that cannot be read is retried, silence is sent meanwhile,
    /// and [`stop`](Self::stop)'s summary says so. The picture is recorded either way (ADR 0015).
    pub fn start(started: Instant, packets: SyncSender<AudioPacket>) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let join = std::thread::Builder::new()
            .name("captastic-audio".to_owned())
            .spawn(move || native::run(started, &packets, &thread_stop))?;
        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    /// Stops reading and returns what happened. The thread fills silence to the moment it stops,
    /// so the track reaches the end of the picture.
    pub fn stop(mut self) -> LoopbackSummary {
        self.stop.store(true, Ordering::Release);
        self.join
            .take()
            .and_then(|join| join.join().ok())
            .unwrap_or_else(|| LoopbackSummary {
                error: Some("the audio thread ended abnormally".to_owned()),
                ..LoopbackSummary::default()
            })
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// How a device's samples are laid out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SampleFormat {
    F32,
    I16,
    /// 24 bits in three bytes.
    I24,
    /// 32-bit containers, whether all 32 bits are used or only the top 24.
    I32,
}

impl SampleFormat {
    fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::F32 | Self::I32 => 4,
        }
    }
}

/// A device's mix format, as far as the conversion needs it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeviceFormat {
    pub(crate) sample: SampleFormat,
    pub(crate) channels: u16,
    pub(crate) rate: u32,
}

/// Turns a device's frames into stereo samples between -1 and 1. Mono is copied to both sides.
/// More channels, in Windows' order (front left, front right, centre, low frequency, then
/// left-right pairs), are folded into two: the centre and the surround pairs at -3 dB, the low
/// frequency dropped, and the whole scaled so a full-scale mix cannot clip.
pub(crate) fn to_stereo(bytes: &[u8], format: DeviceFormat) -> Vec<[f32; 2]> {
    let channels = usize::from(format.channels.max(1));
    let width = format.sample.bytes();
    let frame_bytes = channels * width;
    let sample = |frame: &[u8], channel: usize| -> f32 {
        let at = &frame[channel * width..channel * width + width];
        match format.sample {
            SampleFormat::F32 => f32::from_le_bytes([at[0], at[1], at[2], at[3]]),
            SampleFormat::I16 => f32::from(i16::from_le_bytes([at[0], at[1]])) / 32_768.0,
            SampleFormat::I24 => {
                let value = i32::from_le_bytes([0, at[0], at[1], at[2]]) >> 8;
                value as f32 / 8_388_608.0
            }
            SampleFormat::I32 => {
                i32::from_le_bytes([at[0], at[1], at[2], at[3]]) as f32 / 2_147_483_648.0
            }
        }
    };
    const SIDE: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let pairs = channels.saturating_sub(4) / 2;
    let surround_scale = 1.0 / (1.0 + SIDE + SIDE * pairs as f32);
    bytes
        .chunks_exact(frame_bytes)
        .map(|frame| match channels {
            1 => {
                let value = sample(frame, 0);
                [value, value]
            }
            2 => [sample(frame, 0), sample(frame, 1)],
            3 => {
                // Left, right and centre.
                let centre = sample(frame, 2) * SIDE;
                let scale = 1.0 / (1.0 + SIDE);
                [
                    (sample(frame, 0) + centre) * scale,
                    (sample(frame, 1) + centre) * scale,
                ]
            }
            _ => {
                let centre = sample(frame, 2) * SIDE;
                let mut left = sample(frame, 0) + centre;
                let mut right = sample(frame, 1) + centre;
                for pair in 0..pairs {
                    left += sample(frame, 4 + pair * 2) * SIDE;
                    right += sample(frame, 5 + pair * 2) * SIDE;
                }
                [left * surround_scale, right * surround_scale]
            }
        })
        .collect()
}

/// Linear resampling to [`AUDIO_SAMPLE_RATE`], carried across packets so the seams are smooth.
pub(crate) struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Where the next output frame falls, in input frames from the start of the pending input.
    position: f64,
    /// The last input frame of the packet before, which the next one continues from.
    previous: [f32; 2],
}

impl Resampler {
    pub(crate) fn new(input_rate: u32) -> Self {
        Self {
            step: f64::from(input_rate.max(1)) / f64::from(AUDIO_SAMPLE_RATE),
            position: 0.0,
            previous: [0.0, 0.0],
        }
    }

    pub(crate) fn process(&mut self, input: &[[f32; 2]]) -> Vec<[f32; 2]> {
        if (self.step - 1.0).abs() < f64::EPSILON {
            return input.to_vec();
        }
        let mut output = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        // Positions are counted from the previous packet's last frame, at -1.
        while self.position < input.len() as f64 - 1.0 + 1.0 {
            let index = self.position.floor();
            let fraction = (self.position - index) as f32;
            let at = |i: f64| -> [f32; 2] {
                if i < 0.0 {
                    self.previous
                } else {
                    input[i as usize]
                }
            };
            let before = at(index - 1.0);
            let after = at(index);
            output.push([
                before[0] + (after[0] - before[0]) * fraction,
                before[1] + (after[1] - before[1]) * fraction,
            ]);
            self.position += self.step;
        }
        self.position -= input.len() as f64;
        if let Some(last) = input.last() {
            self.previous = *last;
        }
        output
    }
}

pub(crate) fn to_i16(frames: &[[f32; 2]]) -> Vec<i16> {
    frames
        .iter()
        .flat_map(|frame| frame.map(|value| (value.clamp(-1.0, 1.0) * 32_767.0).round() as i16))
        .collect()
}

/// Keeps the track continuous: places each stretch of sound where its time says, with silence
/// before it when it starts after the track's end, and silence up to the clock when nothing comes.
pub(crate) struct Track {
    /// Frames sent so far, at [`AUDIO_SAMPLE_RATE`].
    written: u64,
    silent: u64,
}

impl Track {
    pub(crate) fn new() -> Self {
        Self {
            written: 0,
            silent: 0,
        }
    }

    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    pub(crate) fn silent(&self) -> u64 {
        self.silent
    }

    /// The packets for `samples` (interleaved stereo) that start at `at` on the recording's clock:
    /// silence first if `at` is past the track's end, then the sound. Sound that starts before the
    /// track's end is appended where the track is, since what has been sent cannot be taken back.
    pub(crate) fn place(&mut self, at: Duration, samples: Vec<i16>) -> Vec<AudioPacket> {
        let mut packets = Vec::with_capacity(2);
        let at_frame = frames_at(at);
        if at_frame > self.written + GAP_TOLERANCE_FRAMES {
            packets.push(self.silence(at_frame - self.written));
        }
        if !samples.is_empty() {
            let start = duration_at(self.written);
            self.written += (samples.len() / usize::from(AUDIO_CHANNELS)) as u64;
            packets.push(AudioPacket { start, samples });
        }
        packets
    }

    /// Silence up to `now`, less a margin for a packet still on its way, when the track is behind.
    pub(crate) fn fill_to(&mut self, now: Duration, lag: Duration) -> Option<AudioPacket> {
        let target = frames_at(now.saturating_sub(lag));
        (target > self.written + GAP_TOLERANCE_FRAMES).then(|| self.silence(target - self.written))
    }

    fn silence(&mut self, frames: u64) -> AudioPacket {
        let start = duration_at(self.written);
        self.written += frames;
        self.silent += frames;
        AudioPacket {
            start,
            samples: vec![0; frames as usize * usize::from(AUDIO_CHANNELS)],
        }
    }
}

fn frames_at(at: Duration) -> u64 {
    (at.as_nanos() * u128::from(AUDIO_SAMPLE_RATE) / 1_000_000_000) as u64
}

fn duration_at(frames: u64) -> Duration {
    Duration::from_nanos(
        (u128::from(frames) * 1_000_000_000 / u128::from(AUDIO_SAMPLE_RATE)) as u64,
    )
}

mod native {
    //! The thread that talks to WASAPI.

    use super::*;
    use windows::core::GUID;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED,
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    };
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    /// The shared tail of the KSDATAFORMAT_SUBTYPE GUIDs; `data1` says which.
    const SUBTYPE_TAIL: (u16, u16, [u8; 8]) = (
        0x0000,
        0x0010,
        [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71],
    );
    /// WASAPI's buffer: long enough that a 10 ms poll never lets it overflow.
    const BUFFER_100NS: i64 = 1_000_000;

    struct Device {
        id: String,
        client: IAudioClient,
        capture: IAudioCaptureClient,
        format: DeviceFormat,
        resampler: Resampler,
    }

    impl Drop for Device {
        fn drop(&mut self) {
            // SAFETY: Stops this device's own client; a client already stopped refuses harmlessly.
            let _ = unsafe { self.client.Stop() };
        }
    }

    pub(super) fn run(
        started: Instant,
        packets: &SyncSender<AudioPacket>,
        stop: &AtomicBool,
    ) -> LoopbackSummary {
        // SAFETY: This thread's own apartment, balanced below.
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let summary = capture(started, packets, stop);
        if com {
            // SAFETY: Balances the successful CoInitializeEx above, after every object is gone.
            unsafe { CoUninitialize() };
        }
        summary
    }

    fn capture(
        started: Instant,
        packets: &SyncSender<AudioPacket>,
        stop: &AtomicBool,
    ) -> LoopbackSummary {
        let mut summary = LoopbackSummary::default();
        let mut track = Track::new();
        let send = |packet: AudioPacket, summary: &mut LoopbackSummary| {
            if packets.try_send(packet).is_err() {
                summary.packets_dropped += 1;
            }
        };
        // The performance counter at `started`, in 100 ns: WASAPI stamps packets on that clock.
        let clock = Clock::new(started);
        // SAFETY: Creates the enumerator in this thread's apartment.
        let enumerator: Option<IMMDeviceEnumerator> =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| summary.error = Some(format!("no audio devices: {error}")))
                .ok();
        let mut device: Option<Device> = None;
        let mut last_check: Option<Instant> = None;
        while !stop.load(Ordering::Acquire) {
            if let Some(enumerator) = &enumerator {
                if last_check.is_none_or(|checked| checked.elapsed() >= DEVICE_CHECK) {
                    last_check = Some(Instant::now());
                    let default = default_device_id(enumerator);
                    // Said whenever there is none and never has been, not only when one goes:
                    // a computer with no output device at all records silence, and says why.
                    if default.is_none() && !summary.device_opened {
                        summary.error = Some("there is no output device".to_owned());
                    }
                    let current = device.as_ref().map(|open| open.id.clone());
                    if default != current {
                        if device.take().is_some() {
                            summary.device_changes += 1;
                        }
                        if default.is_some() {
                            match open(enumerator) {
                                Ok(opened) => {
                                    log::info!(
                                        "recording sound from the default output device ({} Hz, {} channels)",
                                        opened.format.rate,
                                        opened.format.channels
                                    );
                                    summary.device_opened = true;
                                    summary.error = None;
                                    device = Some(opened);
                                }
                                Err(error) => {
                                    if !summary.device_opened {
                                        summary.error = Some(error.clone());
                                    }
                                    log::warn!("the output device could not be recorded: {error}");
                                }
                            }
                        }
                    }
                }
            }
            if let Some(open) = device.as_mut() {
                match read(open, &clock) {
                    Ok(stretches) => {
                        for (at, samples) in stretches {
                            for packet in track.place(at, samples) {
                                send(packet, &mut summary);
                            }
                        }
                    }
                    Err(error) => {
                        // Gone (unplugged, or its format changed): look for the default again now.
                        log::info!("the output device stopped being recordable: {error}");
                        device = None;
                        summary.device_changes += 1;
                        last_check = None;
                    }
                }
            }
            if let Some(packet) = track.fill_to(started.elapsed(), IDLE_FILL_LAG) {
                send(packet, &mut summary);
            }
            std::thread::sleep(POLL);
        }
        drop(device);
        if let Some(packet) = track.fill_to(started.elapsed(), Duration::ZERO) {
            send(packet, &mut summary);
        }
        summary.frames = track.written();
        summary.silent_frames = track.silent();
        summary
    }

    /// The performance counter as the recording's clock.
    struct Clock {
        /// The counter at the recording's start, in 100 ns.
        origin_100ns: i64,
    }

    impl Clock {
        fn new(started: Instant) -> Self {
            let mut counter = 0_i64;
            let mut frequency = 0_i64;
            // SAFETY: Both outputs are locals, written by these calls.
            unsafe {
                let _ = QueryPerformanceCounter(&mut counter);
                let _ = QueryPerformanceFrequency(&mut frequency);
            }
            let now_100ns =
                (i128::from(counter) * 10_000_000 / i128::from(frequency.max(1))) as i64;
            let since = started.elapsed().as_nanos() as i64 / 100;
            Self {
                origin_100ns: now_100ns - since,
            }
        }

        fn at(&self, qpc_100ns: u64) -> Duration {
            let ticks = (qpc_100ns as i64).saturating_sub(self.origin_100ns).max(0);
            Duration::from_nanos(ticks as u64 * 100)
        }
    }

    fn default_device_id(enumerator: &IMMDeviceEnumerator) -> Option<String> {
        // SAFETY: Asks this thread's enumerator; the id string is freed below.
        unsafe {
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            device_id(&device)
        }
    }

    /// # Safety
    /// `device` must be this thread's.
    unsafe fn device_id(device: &IMMDevice) -> Option<String> {
        // SAFETY: The id is a string WASAPI allocated for us, read once and then freed.
        unsafe {
            let id = device.GetId().ok()?;
            let text = id.to_string().ok();
            CoTaskMemFree(Some(id.0 as *const _));
            text
        }
    }

    fn open(enumerator: &IMMDeviceEnumerator) -> Result<Device, String> {
        // SAFETY: Every object is this thread's; the mix format is read and then freed.
        unsafe {
            let endpoint = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|error| format!("no default output device: {error}"))?;
            let id = device_id(&endpoint).unwrap_or_default();
            let client: IAudioClient = endpoint
                .Activate(CLSCTX_ALL, None)
                .map_err(|error| format!("activating the device: {error}"))?;
            let mix = client
                .GetMixFormat()
                .map_err(|error| format!("reading the device's format: {error}"))?;
            let format = device_format(mix);
            let initialized = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_100NS,
                0,
                mix,
                None,
            );
            CoTaskMemFree(Some(mix as *const _));
            let format = format?;
            initialized.map_err(|error| format!("opening loopback: {error}"))?;
            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(|error| format!("getting the capture client: {error}"))?;
            client
                .Start()
                .map_err(|error| format!("starting loopback: {error}"))?;
            Ok(Device {
                id,
                client,
                capture,
                resampler: Resampler::new(format.rate),
                format,
            })
        }
    }

    /// The parts of a mix format the conversion needs, or why it cannot read it.
    ///
    /// # Safety
    /// `mix` must point at a format from `GetMixFormat`, which is extensible when its tag says so.
    unsafe fn device_format(mix: *const WAVEFORMATEX) -> Result<DeviceFormat, String> {
        // SAFETY: As the caller promises; the structures are packed, so read unaligned.
        let base = unsafe { std::ptr::read_unaligned(mix) };
        let (tag, bits) = if base.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            // SAFETY: The tag says the format is the extensible one.
            let extensible =
                unsafe { std::ptr::read_unaligned(mix.cast::<WAVEFORMATEXTENSIBLE>()) };
            let subformat: GUID = extensible.SubFormat;
            let (data2, data3, data4) = SUBTYPE_TAIL;
            if subformat.data2 != data2 || subformat.data3 != data3 || subformat.data4 != data4 {
                return Err("the device's sample format is not PCM or float".to_owned());
            }
            (subformat.data1 as u16, base.wBitsPerSample)
        } else {
            (base.wFormatTag, base.wBitsPerSample)
        };
        let sample = match (tag, bits) {
            (WAVE_FORMAT_IEEE_FLOAT, 32) => SampleFormat::F32,
            (WAVE_FORMAT_PCM, 16) => SampleFormat::I16,
            (WAVE_FORMAT_PCM, 24) => SampleFormat::I24,
            (WAVE_FORMAT_PCM, 32) => SampleFormat::I32,
            _ => {
                return Err(format!(
                    "the device's {bits}-bit format {tag} cannot be read"
                ))
            }
        };
        Ok(DeviceFormat {
            sample,
            channels: base.nChannels,
            rate: base.nSamplesPerSec,
        })
    }

    /// Everything the device has ready: each stretch's time on the recording's clock and its
    /// samples at the track's rate.
    fn read(device: &mut Device, clock: &Clock) -> Result<Vec<(Duration, Vec<i16>)>, String> {
        let mut stretches = Vec::new();
        loop {
            // SAFETY: The capture client is this thread's.
            let ready = unsafe { device.capture.GetNextPacketSize() }.map_err(describe)?;
            if ready == 0 {
                return Ok(stretches);
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0_u32;
            let mut flags = 0_u32;
            let mut qpc = 0_u64;
            // SAFETY: The outputs are locals; the buffer is read before it is released below.
            unsafe {
                device
                    .capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))
            }
            .map_err(describe)?;
            let frame_bytes = usize::from(device.format.channels) * device.format.sample.bytes();
            let stereo = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                vec![[0.0, 0.0]; frames as usize]
            } else {
                // SAFETY: WASAPI hands over `frames` frames of the mix format at `data`.
                let bytes =
                    unsafe { std::slice::from_raw_parts(data, frames as usize * frame_bytes) };
                to_stereo(bytes, device.format)
            };
            // SAFETY: Releases exactly the frames this GetBuffer returned.
            unsafe { device.capture.ReleaseBuffer(frames) }.map_err(describe)?;
            let resampled = device.resampler.process(&stereo);
            stretches.push((clock.at(qpc), to_i16(&resampled)));
        }
    }

    fn describe(error: windows::core::Error) -> String {
        if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
            "the device was removed or changed".to_owned()
        } else {
            error.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_f32(values: &[f32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn format(sample: SampleFormat, channels: u16) -> DeviceFormat {
        DeviceFormat {
            sample,
            channels,
            rate: 48_000,
        }
    }

    #[test]
    fn every_sample_format_reads_as_the_same_level() {
        let float = to_stereo(&bytes_f32(&[0.5, -0.25]), format(SampleFormat::F32, 2));
        let pcm16: Vec<u8> = [16_384_i16, -8_192]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let int16 = to_stereo(&pcm16, format(SampleFormat::I16, 2));
        let pcm24: Vec<u8> = [4_194_304_i32, -2_097_152]
            .iter()
            .flat_map(|value| value.to_le_bytes()[..3].to_vec())
            .collect();
        let int24 = to_stereo(&pcm24, format(SampleFormat::I24, 2));
        let pcm32: Vec<u8> = [1_073_741_824_i32, -536_870_912]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        let int32 = to_stereo(&pcm32, format(SampleFormat::I32, 2));
        for stereo in [float, int16, int24, int32] {
            assert!((stereo[0][0] - 0.5).abs() < 1e-4, "{stereo:?}");
            assert!((stereo[0][1] + 0.25).abs() < 1e-4, "{stereo:?}");
        }
    }

    #[test]
    fn mono_is_both_sides_and_surround_folds_into_two_without_clipping() {
        let mono = to_stereo(&bytes_f32(&[0.3]), format(SampleFormat::F32, 1));
        assert_eq!(mono, vec![[0.3, 0.3]]);
        // 5.1 at full scale on every channel: left and right stay within range, and LFE is dropped.
        let full = to_stereo(&bytes_f32(&[1.0; 6]), format(SampleFormat::F32, 6));
        assert!(full[0][0] <= 1.0 + 1e-6 && full[0][0] > 0.9, "{full:?}");
        let lfe_only = to_stereo(
            &bytes_f32(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
            format(SampleFormat::F32, 6),
        );
        assert_eq!(lfe_only, vec![[0.0, 0.0]]);
        let left_only = to_stereo(
            &bytes_f32(&[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0]),
            format(SampleFormat::F32, 8),
        );
        assert!(
            left_only[0][0] > 0.5 && left_only[0][1] == 0.0,
            "{left_only:?}"
        );
    }

    #[test]
    fn resampling_keeps_the_length_in_time_across_packets() {
        let mut resampler = Resampler::new(44_100);
        let mut out = 0;
        // One second at 44.1 kHz, in uneven packets.
        let mut sent = 0;
        for size in [441, 1000, 4410, 33_000, 5249] {
            out += resampler.process(&vec![[0.25, -0.25]; size]).len();
            sent += size;
        }
        assert_eq!(sent, 44_100);
        assert!((out as i64 - 48_000).abs() <= 2, "{out}");
        // A constant level stays that level.
        let level = resampler.process(&vec![[0.25, -0.25]; 100]);
        assert!(level.iter().all(|frame| (frame[0] - 0.25).abs() < 1e-6));

        let mut same = Resampler::new(48_000);
        assert_eq!(same.process(&[[0.1, 0.2]; 7]).len(), 7);
    }

    #[test]
    fn samples_become_sixteen_bit_and_clip_rather_than_wrap() {
        assert_eq!(
            to_i16(&[[1.0, -1.0], [2.0, -2.0], [0.0, 0.5]]),
            vec![32_767, -32_767, 32_767, -32_767, 0, 16_384]
        );
    }

    #[test]
    fn the_track_is_continuous_with_silence_in_every_gap() {
        let mut track = Track::new();
        let ms = |value: u64| Duration::from_millis(value);
        let sound = |frames: usize| vec![100_i16; frames * 2];
        // Nothing played for the first 200 ms; then 10 ms of sound arrives at 200 ms.
        let first = track.place(ms(200), sound(480));
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].start, Duration::ZERO);
        assert_eq!(first[0].frames(), 9_600);
        assert!(first[0].samples.iter().all(|&sample| sample == 0));
        assert_eq!(first[1].start, ms(200));
        // The next packet follows on, even a little early or late.
        let next = track.place(ms(209), sound(480));
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].start, ms(210));
        // Then nothing for a second: silence follows the clock, a margin behind it.
        let idle = track.fill_to(ms(1_220), ms(100)).expect("silence");
        assert_eq!(idle.start, ms(220));
        assert_eq!(idle.frames(), 48_000 * 9 / 10);
        assert!(
            track.fill_to(ms(1_225), ms(100)).is_none(),
            "within tolerance"
        );
        // Every frame is accounted for, once.
        assert_eq!(track.written(), 48_000 * 112 / 100);
        assert_eq!(track.silent(), 9_600 + 43_200);
    }

    #[test]
    #[ignore = "needs an audio output device, which a hosted CI runner may lack"]
    fn the_default_device_is_read_for_as_long_as_it_runs() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1_000);
        let started = Instant::now();
        let capture = LoopbackCapture::start(started, sender).expect("thread");
        std::thread::sleep(Duration::from_millis(1_500));
        let summary = capture.stop();
        let packets: Vec<AudioPacket> = receiver.try_iter().collect();
        if !summary.device_opened {
            // No output device: the track is still continuous silence, and says why.
            assert_eq!(summary.error.as_deref(), Some("there is no output device"));
            assert_eq!(summary.silent_frames, summary.frames);
        }
        assert_eq!(summary.packets_dropped, 0, "{summary:?}");
        // Continuous: each packet starts where the one before ended.
        let mut end = Duration::ZERO;
        for packet in &packets {
            assert!(
                packet.start.abs_diff(end) < Duration::from_micros(50),
                "{:?} after {end:?}",
                packet.start
            );
            end = packet.start + duration_at(packet.frames() as u64);
        }
        assert!(end >= Duration::from_millis(1_400), "{end:?} {summary:?}");
    }
}
