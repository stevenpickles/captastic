//! Playing a recording's sound in its review (ADR 0015), through a WASAPI shared-mode render
//! client on the default output device.
//!
//! [`SoundPlayer`] holds the recording's sound, decoded, and plays a span of it on a thread of its
//! own when asked. Windows converts the 48 kHz 16-bit stereo it is given to whatever the device
//! takes. Playing is best effort: with no output device, or one that will not open, nothing is
//! heard and the review carries on without sound.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};

/// WASAPI's buffer, and how far ahead of the speaker the thread writes.
const BUFFER: Duration = Duration::from_millis(100);
/// How often the thread tops the buffer up.
const POLL: Duration = Duration::from_millis(10);

/// A recording's sound, ready to be played from anywhere in it.
pub struct SoundPlayer {
    samples: Arc<[i16]>,
    playing: Option<Playing>,
}

struct Playing {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

impl SoundPlayer {
    /// `samples` are interleaved stereo at [`AUDIO_SAMPLE_RATE`], as [`crate::mp4_sound`] decodes
    /// a recording's.
    pub fn new(samples: Vec<i16>) -> Self {
        Self {
            samples: samples.into(),
            playing: None,
        }
    }

    /// How long the sound lasts.
    pub fn duration(&self) -> Duration {
        duration_at((self.samples.len() / usize::from(AUDIO_CHANNELS)) as u64)
    }

    /// Plays from `from` until `until`, stopping whatever was playing first. Returns at once.
    pub fn play(&mut self, from: Duration, until: Duration) {
        self.stop();
        let channels = usize::from(AUDIO_CHANNELS);
        let start = (frames_at(from) as usize * channels).min(self.samples.len());
        let end = (frames_at(until) as usize * channels).clamp(start, self.samples.len());
        if start == end {
            return;
        }
        let samples = self.samples.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        match std::thread::Builder::new()
            .name("captastic-review-sound".to_owned())
            .spawn(move || native::play(&samples[start..end], &thread_stop))
        {
            Ok(join) => self.playing = Some(Playing { stop, join }),
            Err(error) => log::warn!("the review's sound could not be played: {error}"),
        }
    }

    /// Stops playing at once. Nothing happens when nothing plays.
    pub fn stop(&mut self) {
        if let Some(playing) = self.playing.take() {
            playing.stop.store(true, Ordering::Release);
            let _ = playing.join.join();
        }
    }

    /// Whether sound is still playing: false once a span has played out, or was stopped.
    pub fn is_playing(&self) -> bool {
        self.playing
            .as_ref()
            .is_some_and(|playing| !playing.join.is_finished())
    }
}

impl Drop for SoundPlayer {
    fn drop(&mut self) {
        self.stop();
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
    use super::*;
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    const WAVE_FORMAT_PCM: u16 = 1;

    pub(super) fn play(samples: &[i16], stop: &AtomicBool) {
        // SAFETY: This thread's own apartment, balanced below.
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        if let Err(error) = render(samples, stop) {
            log::info!("the review's sound was not played: {error}");
        }
        if com {
            // SAFETY: Balances the successful CoInitializeEx above, after every object is gone.
            unsafe { CoUninitialize() };
        }
    }

    fn render(samples: &[i16], stop: &AtomicBool) -> windows::core::Result<()> {
        let channels = usize::from(AUDIO_CHANNELS);
        let block = AUDIO_CHANNELS * 2;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: AUDIO_CHANNELS,
            nSamplesPerSec: AUDIO_SAMPLE_RATE,
            nAvgBytesPerSec: AUDIO_SAMPLE_RATE * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        // SAFETY: Every object is this thread's; the buffer is written within the frames WASAPI
        // hands out and released before the next.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                (BUFFER.as_nanos() / 100) as i64,
                0,
                &format,
                None,
            )?;
            let buffer_frames = client.GetBufferSize()? as usize;
            let renderer: IAudioRenderClient = client.GetService()?;
            let total = samples.len() / channels;
            let mut written = 0_usize;
            let mut started = false;
            while !stop.load(Ordering::Acquire) {
                let padding = client.GetCurrentPadding()? as usize;
                let room = buffer_frames.saturating_sub(padding).min(total - written);
                if room > 0 {
                    let data = renderer.GetBuffer(room as u32)?;
                    std::ptr::copy_nonoverlapping(
                        samples[written * channels..].as_ptr().cast::<u8>(),
                        data,
                        room * channels * 2,
                    );
                    renderer.ReleaseBuffer(room as u32, 0)?;
                    written += room;
                }
                if !started {
                    client.Start()?;
                    started = true;
                }
                // Everything written and played out: done.
                if written == total && padding == 0 {
                    break;
                }
                std::thread::sleep(POLL);
            }
            let _ = client.Stop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_player_knows_how_long_its_sound_is_and_plays_nothing_from_its_end() {
        let mut player = SoundPlayer::new(vec![0; 48_000 * 2]);
        assert_eq!(player.duration(), Duration::from_secs(1));
        player.play(Duration::from_secs(2), Duration::from_secs(3));
        assert!(!player.is_playing(), "nothing is left to play past the end");
        player.stop();
    }

    #[test]
    #[ignore = "plays a tone through the default output device for half a second"]
    fn a_span_plays_out_and_can_be_stopped() {
        let rate = AUDIO_SAMPLE_RATE as usize;
        let samples: Vec<i16> = (0..rate)
            .flat_map(|n| {
                let value = (3_000.0
                    * (2.0 * std::f64::consts::PI * 440.0 * n as f64 / rate as f64).sin())
                    as i16;
                [value, value]
            })
            .collect();
        let mut player = SoundPlayer::new(samples);
        // Heard back through loopback: what the player writes reaches the output device.
        let (sender, receiver) = std::sync::mpsc::sync_channel(1_000);
        let loopback =
            crate::LoopbackCapture::start(std::time::Instant::now(), sender).expect("loopback");
        std::thread::sleep(Duration::from_millis(200));
        player.play(Duration::ZERO, Duration::from_millis(300));
        assert!(player.is_playing());
        std::thread::sleep(Duration::from_millis(700));
        assert!(!player.is_playing(), "a 300 ms span has played out");
        let summary = loopback.stop();
        let heard: Vec<i16> = receiver
            .try_iter()
            .flat_map(|packet| packet.samples)
            .collect();
        let loudest = heard
            .chunks(960)
            .map(|chunk| {
                (chunk.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / chunk.len() as f64)
                    .sqrt()
            })
            .fold(0.0, f64::max);
        eprintln!("heard back at RMS {loudest:.0}: {summary:?}");
        assert!(
            summary.device_opened && loudest > 300.0,
            "the tone was not heard back"
        );
        player.play(Duration::ZERO, Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(100));
        let stopped = std::time::Instant::now();
        player.stop();
        assert!(
            stopped.elapsed() < Duration::from_millis(100),
            "stopping is prompt"
        );
        assert!(!player.is_playing());
    }
}
