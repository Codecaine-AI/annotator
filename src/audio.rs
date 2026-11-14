use std::io::Cursor;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const SAMPLE_RATE: u32 = 16_000;

#[derive(Clone, Debug)]
pub struct RecordedAudio {
    pub sample_count: usize,
    pub duration_ms: u64,
    pub wav_bytes: Vec<u8>,
    /// How long after the capture clock started the first sample was heard: the microphone
    /// opens on its own thread, some hundred ms after the recording begins. Sample `n` was
    /// heard at `offset_ms + n / 16` ms on the capture clock.
    pub offset_ms: u64,
    /// The transcription that ran while recording, if one did (`live_transcribe`).
    pub live: crate::live_transcribe::Slot,
}

impl RecordedAudio {
    /// Takes a paused recording's pauses out of the audio (`crate::pause`), sample for sample.
    /// `cuts` are on the capture clock; the samples are counted from the first one heard,
    /// `offset_ms` later, so the pauses are moved onto that clock first, as a movie's are.
    /// A transcription that ran while recording heard the pauses, so it is dropped and the
    /// words come from the cut audio.
    pub fn cut(self, cuts: &crate::pause::Cuts) -> Result<Self> {
        if cuts.is_empty() {
            return Ok(self);
        }
        let samples = cuts
            .on_clock_from(self.offset_ms)
            .cut_samples(&decode_wav_16k(&self.wav_bytes)?, u64::from(SAMPLE_RATE));
        drop(self.live.take());
        Ok(Self {
            sample_count: samples.len(),
            duration_ms: samples.len() as u64 * 1_000 / u64::from(SAMPLE_RATE),
            wav_bytes: encode_wav_16k(&samples)?,
            offset_ms: cuts.shift(self.offset_ms),
            live: Default::default(),
        })
    }
}

pub struct AudioRecorder {
    worker: Option<std::thread::JoinHandle<Result<()>>>,
    stop_sender: Option<std::sync::mpsc::Sender<()>>,
    samples: Arc<Mutex<Vec<f32>>>,
    /// When the capture clock started, and when the first sample was heard on it.
    clock_started: Instant,
    heard_at: Arc<OnceLock<Instant>>,
    error: Arc<Mutex<Option<String>>>,
    live: Option<crate::live_transcribe::Live>,
    /// A live transcription only for showing the words (`liveWords`), dropped at stop.
    preview: Option<crate::live_transcribe::Live>,
    /// The capsule's view of this recording (`live_audio::begin`).
    shown: Option<u64>,
}

impl AudioRecorder {
    /// `clock_started`: when the capture clock began (`SharedCaptureTelemetry::started_at`),
    /// which is before the microphone opens.
    pub fn start(clock_started: Instant) -> Result<Self> {
        Self::open(true, clock_started)
    }

    /// The microphone only for the capsule's waveform and words, while something else records the
    /// narration (the iPhone mirror records it into its movie). `None` when live audio is off.
    pub fn listen() -> Option<Self> {
        if !crate::live_audio::wanted() {
            return None;
        }
        Self::open(false, Instant::now())
            .inspect_err(|error| crate::capture::scrawl_log(format!("Live audio: {error:#}")))
            .ok()
    }

    /// `recording`: this is the capture's own audio, so the live transcription experiment runs on it.
    fn open(recording: bool, clock_started: Instant) -> Result<Self> {
        let started = std::time::Instant::now();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let heard_at = Arc::new(OnceLock::new());
        let error = Arc::new(Mutex::new(None));
        let (stop_sender, stop_receiver) = std::sync::mpsc::channel();
        let worker_samples = Arc::clone(&samples);
        let worker_heard_at = Arc::clone(&heard_at);
        let worker_error = Arc::clone(&error);
        let worker = std::thread::Builder::new()
            .name("scrawl-audio".into())
            .spawn(move || {
                let stream = Self::open_stream(worker_samples, worker_heard_at, worker_error)
                    .inspect_err(|error| {
                        crate::capture::scrawl_log(format!("Microphone startup failed: {error:#}"));
                    })?;
                if recording {
                    crate::ui::start_metric("audio_ready_ms", started);
                }
                // Stream never crosses threads. Disconnect also stops abandoned recorders.
                let _ = stop_receiver.recv();
                drop(stream);
                Ok(())
            })
            .context("Could not spawn microphone worker")?;
        let show = crate::live_audio::wanted();
        // The Apple Speech stream for the words runs only with `liveWords` on.
        let caption = (show && crate::live_audio::captions_possible())
            .then(|| Arc::new(Mutex::new(crate::live_audio::Caption::default())));
        let log = |error: anyhow::Error| {
            crate::capture::scrawl_log(format!("Live transcription: {error:#}"));
        };
        // The experiment's stream shows the words too; without it, one only for showing them.
        let (live, preview) = if recording && crate::live_transcribe::wanted() {
            let live = crate::live_transcribe::Live::start(Arc::clone(&samples), caption.clone());
            (live.map_err(log).ok(), None)
        } else {
            let preview = caption.clone().map(|caption| {
                crate::live_transcribe::Live::preview(Arc::clone(&samples), caption)
            });
            (None, preview.and_then(|p| p.map_err(log).ok()))
        };
        let shown = show.then(|| crate::live_audio::begin(Arc::clone(&samples), caption));
        Ok(Self {
            worker: Some(worker),
            stop_sender: Some(stop_sender),
            samples,
            clock_started,
            heard_at,
            error,
            live,
            preview,
            shown,
        })
    }

    /// The chosen microphone (`mic`), else the macOS default. A chosen device that is missing or
    /// won't open never stops the recording.
    fn open_stream(
        samples: Arc<Mutex<Vec<f32>>>,
        heard_at: Arc<OnceLock<Instant>>,
        error: Arc<Mutex<Option<String>>>,
    ) -> Result<cpal::Stream> {
        let choice = crate::mic::choice();
        if let Some(choice) = &choice {
            if let Some(device) = crate::mic::device(&choice.id) {
                let input = crate::mic::Input {
                    id: choice.id.clone(),
                    name: device_name(&device).unwrap_or_else(|| choice.name.clone()),
                };
                crate::mic::note_pick(&crate::mic::Pick::Chosen(&input));
                match Self::open_device(&device, samples.clone(), heard_at.clone(), error.clone()) {
                    Ok(stream) => {
                        crate::capture::scrawl_log(format!("Microphone: {}", input.name));
                        return Ok(stream);
                    }
                    Err(e) => crate::capture::scrawl_log(format!(
                        "Microphone: could not open {} ({e:#}); using the default",
                        input.name
                    )),
                }
            } else {
                let default = cpal::default_host()
                    .default_input_device()
                    .and_then(|device| {
                        Some(crate::mic::Input {
                            id: device.id().ok()?.id().to_owned(),
                            name: device_name(&device)?,
                        })
                    });
                let inputs = crate::mic::Inputs {
                    default: default.as_ref().map(|d| d.id.clone()),
                    devices: default.into_iter().collect(),
                };
                crate::mic::note_pick(&crate::mic::Pick::new(Some(choice), &inputs));
            }
        }
        let device = cpal::default_host()
            .default_input_device()
            .context("No microphone is available")?;
        let stream = Self::open_device(&device, samples, heard_at, error)?;
        crate::capture::scrawl_log(format!(
            "Microphone: {} (default)",
            device_name(&device).unwrap_or_default()
        ));
        Ok(stream)
    }

    fn open_device(
        device: &cpal::Device,
        samples: Arc<Mutex<Vec<f32>>>,
        heard_at: Arc<OnceLock<Instant>>,
        error: Arc<Mutex<Option<String>>>,
    ) -> Result<cpal::Stream> {
        let supported = device.default_input_config()?;
        let config = supported.config();
        crate::capture::scrawl_log(format!(
            "Microphone: {} Hz, {} ch, {:?}",
            config.sample_rate,
            config.channels,
            supported.sample_format()
        ));
        ensure!(
            config.channels > 0 && config.sample_rate > 0,
            "Invalid microphone format"
        );
        macro_rules! stream {
            ($ty:ty) => {
                input_stream::<$ty>(
                    &device,
                    config,
                    samples.clone(),
                    heard_at.clone(),
                    error.clone(),
                )?
            };
        }
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => stream!(f32),
            cpal::SampleFormat::F64 => stream!(f64),
            cpal::SampleFormat::I8 => stream!(i8),
            cpal::SampleFormat::I16 => stream!(i16),
            cpal::SampleFormat::I24 => stream!(cpal::I24),
            cpal::SampleFormat::I32 => stream!(i32),
            cpal::SampleFormat::I64 => stream!(i64),
            cpal::SampleFormat::U8 => stream!(u8),
            cpal::SampleFormat::U16 => stream!(u16),
            cpal::SampleFormat::U24 => stream!(cpal::U24),
            cpal::SampleFormat::U32 => stream!(u32),
            cpal::SampleFormat::U64 => stream!(u64),
            format => bail!("Unsupported microphone format: {format}"),
        };
        stream.play().context("Could not start microphone")?;
        Ok(stream)
    }

    fn join_worker(&mut self) -> Result<()> {
        self.stop_sender.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow!("Microphone worker panicked"))??;
        }
        Ok(())
    }

    pub fn stop(mut self) -> Result<RecordedAudio> {
        self.stop_showing();
        // Joining drops the stream and stops its callback before taking the buffer,
        // including a stop requested while the device was still being initialized.
        self.join_worker()?;
        let error = self
            .error
            .lock()
            .map_err(|_| anyhow!("Microphone error lock poisoned"))?
            .take();
        let samples_16k = std::mem::take(
            &mut *self
                .samples
                .lock()
                .map_err(|_| anyhow!("Microphone buffer lock poisoned"))?,
        );
        // An overload, or a device unplugged partway, keeps what was heard before it.
        if let Some(error) = error {
            if samples_16k.is_empty() {
                bail!("Microphone capture failed: {error}");
            }
            crate::capture::scrawl_log(format!("Microphone: {error}; keeping what it heard"));
        }
        let sample_count = samples_16k.len();
        let duration_ms = sample_count as u64 * 1_000 / SAMPLE_RATE as u64;
        let wav_bytes = encode_wav_16k(&samples_16k)?;
        let offset_ms = self.heard_at.get().map_or(0, |heard_at| {
            heard_at
                .saturating_duration_since(self.clock_started)
                .as_millis() as u64
        });
        Ok(RecordedAudio {
            sample_count,
            duration_ms,
            wav_bytes,
            offset_ms,
            live: crate::live_transcribe::Slot::new(self.live.take()),
        })
    }
}

impl AudioRecorder {
    /// Takes the recording out of the capsule and stops the words' own transcription.
    fn stop_showing(&mut self) {
        if let Some(id) = self.shown.take() {
            crate::live_audio::end(id);
        }
        self.preview.take();
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        self.stop_showing();
        let _ = self.join_worker();
    }
}

fn device_name(device: &cpal::Device) -> Option<String> {
    Some(device.description().ok()?.name().to_owned())
}

fn input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    samples: Arc<Mutex<Vec<f32>>>,
    heard_at: Arc<OnceLock<Instant>>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    let mut phase = 0_u32;
    let mut sum = 0.0_f64;
    Ok(device.build_input_stream::<T, _, _>(
        config,
        move |data, _| {
            if !data.is_empty() {
                heard_at.get_or_init(|| first_heard(Instant::now(), data.len() / channels, rate));
            }
            let Ok(mut output) = samples.lock() else {
                return;
            };
            for frame in data.chunks_exact(channels) {
                let mono = frame
                    .iter()
                    .map(|sample| sample.to_sample::<f32>() as f64)
                    .sum::<f64>()
                    / channels as f64;
                let mono = if mono.is_finite() { mono } else { 0.0 };
                // Area averaging gives a continuous-phase downmix/decimator,
                // including non-integer ratios such as 44.1 kHz -> 16 kHz.
                let mut remaining = SAMPLE_RATE;
                while remaining > 0 {
                    let weight = remaining.min(rate - phase);
                    sum += mono * weight as f64;
                    phase += weight;
                    remaining -= weight;
                    if phase == rate {
                        output.push((sum / rate as f64) as f32);
                        phase = 0;
                        sum = 0.0;
                    }
                }
            }
        },
        move |err| {
            // A buffer under- or overrun drops a few samples; the recording goes on.
            if err.kind() == cpal::ErrorKind::Xrun {
                return;
            }
            if let Ok(mut error) = error.lock() {
                *error = Some(err.to_string());
            }
        },
        None,
    )?)
}

/// When the first sample of the stream's first buffer was heard: the buffer of `frames` frames
/// at `rate` Hz reached the callback at `delivered`, once its last frame was in.
fn first_heard(delivered: Instant, frames: usize, rate: u32) -> Instant {
    delivered
        .checked_sub(Duration::from_secs_f64(
            frames as f64 / f64::from(rate.max(1)),
        ))
        .unwrap_or(delivered)
}

/// Reads a 16 kHz mono WAV (as written by `scrawl-helper audio`) back into a recording.
pub fn recorded_audio_from_wav(wav_bytes: Vec<u8>) -> Result<RecordedAudio> {
    let sample_count = wav_16k_reader(&wav_bytes)?.duration() as usize;
    Ok(RecordedAudio {
        sample_count,
        duration_ms: sample_count as u64 * 1_000 / u64::from(SAMPLE_RATE),
        wav_bytes,
        offset_ms: 0,
        live: Default::default(),
    })
}

/// Decodes a 16 kHz mono 16-bit WAV (this module's own format) into samples.
pub fn decode_wav_16k(wav_bytes: &[u8]) -> Result<Vec<f32>> {
    wav_16k_reader(wav_bytes)?
        .into_samples::<i16>()
        .map(|sample| Ok(f32::from(sample?) / 32_768.0))
        .collect()
}

/// Decodes a 16 kHz mono 16-bit WAV into its raw samples.
pub fn decode_wav_pcm16(wav_bytes: &[u8]) -> Result<Vec<i16>> {
    Ok(wav_16k_reader(wav_bytes)?
        .into_samples::<i16>()
        .collect::<Result<_, _>>()?)
}

fn wav_16k_reader(wav_bytes: &[u8]) -> Result<hound::WavReader<Cursor<&[u8]>>> {
    let reader = hound::WavReader::new(Cursor::new(wav_bytes))?;
    let spec = reader.spec();
    ensure!(
        spec.sample_rate == SAMPLE_RATE
            && spec.channels == 1
            && spec.bits_per_sample == 16
            && spec.sample_format == hound::SampleFormat::Int,
        "Expected 16 kHz mono 16-bit audio"
    );
    Ok(reader)
}

const ISLAND_WINDOW_MS: u64 = 50;
const ISLAND_MERGE_SILENCE_MS: u64 = 350;

/// Energy-based speech spans in milliseconds, quantized to 50 ms. These are not recognizer
/// timestamps. The speech threshold follows the recording's own noise floor (10th percentile of
/// window energy) so a quiet microphone and a noisy room both find their speech.
pub fn speech_islands(samples_16k: &[f32]) -> Vec<(u64, u64)> {
    let window = (u64::from(SAMPLE_RATE) * ISLAND_WINDOW_MS / 1_000) as usize;
    let rms: Vec<f64> = samples_16k
        .chunks(window)
        .map(|chunk| {
            (chunk
                .iter()
                .map(|&x| {
                    if x.is_finite() {
                        f64::from(x).powi(2)
                    } else {
                        0.0
                    }
                })
                .sum::<f64>()
                / chunk.len() as f64)
                .sqrt()
        })
        .collect();
    if rms.is_empty() {
        return Vec::new();
    }
    let mut sorted = rms.clone();
    sorted.sort_by(f64::total_cmp);
    let floor = sorted[sorted.len() / 10];
    let threshold = (floor * 3.0).clamp(0.005, 0.02);
    let mut islands: Vec<(u64, u64)> = Vec::new();
    for (index, &level) in rms.iter().enumerate() {
        if level <= threshold {
            continue;
        }
        let start = index as u64 * ISLAND_WINDOW_MS;
        let end = start + ISLAND_WINDOW_MS;
        match islands.last_mut() {
            Some(last) if start.saturating_sub(last.1) < ISLAND_MERGE_SILENCE_MS => last.1 = end,
            _ => islands.push((start, end)),
        }
    }
    let duration = samples_16k.len() as u64 * 1_000 / u64::from(SAMPLE_RATE);
    for island in &mut islands {
        island.1 = island.1.min(duration);
    }
    islands.retain(|&(a, b)| b > a);
    islands
}

/// One sample as `audio.wav` stores it.
pub fn pcm16(sample: f32) -> i16 {
    let sample = if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    (sample * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16
}

pub fn encode_wav_16k(samples_16k: &[f32]) -> Result<Vec<u8>> {
    let mut bytes = Cursor::new(Vec::with_capacity(44 + samples_16k.len() * 2));
    {
        let mut writer = hound::WavWriter::new(
            &mut bytes,
            hound::WavSpec {
                channels: 1,
                sample_rate: SAMPLE_RATE,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?;
        let mut i16_writer = writer.get_i16_writer(samples_16k.len() as u32);
        for &sample in samples_16k {
            i16_writer.write_sample(pcm16(sample));
        }
        i16_writer.flush()?;
        writer.finalize()?;
    }
    Ok(bytes.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording paused from 2 s to 5 s: `audio.wav` is 3 s shorter and the sound at 5 s
    /// follows the sound at 2 s.
    #[test]
    fn a_paused_recordings_audio_loses_the_pauses() {
        // Each sample holds which tenth of a second it was heard in.
        let heard_at = |index: usize| (index / 1_600) as f32 / 100.0;
        let samples: Vec<f32> = (0..8 * 16_000).map(heard_at).collect();
        let audio = recorded_audio_from_wav(encode_wav_16k(&samples).unwrap()).unwrap();
        let cuts = crate::pause::Cuts::new(vec![crate::pause::Span {
            start_ms: 2_000,
            end_ms: 5_000,
        }]);
        let cut = audio.cut(&cuts).unwrap();
        assert_eq!((cut.duration_ms, cut.sample_count), (5_000, 80_000));
        let kept = decode_wav_16k(&cut.wav_bytes).unwrap();
        assert_eq!(kept.len(), 80_000);
        assert!((kept[31_999] - 0.19).abs() < 0.001, "{}", kept[31_999]);
        assert!((kept[32_000] - 0.50).abs() < 0.001, "{}", kept[32_000]);
        assert!((kept[79_999] - 0.79).abs() < 0.001, "{}", kept[79_999]);
    }

    /// The microphone opens a little after the capture clock starts (here 200 ms), and the
    /// pauses are on that clock: a pause from 5 s to 7 s is 4.8 s to 6.8 s into the audio.
    #[test]
    fn audio_that_started_late_loses_exactly_the_paused_samples() {
        // Each sample holds the millisecond on the capture clock it was heard in, from 200 ms
        // to 9 s.
        let heard_ms = |index: usize| 200 + index / 16;
        let paused = 5_000..7_000;
        let samples: Vec<f32> = (0..8_800 * 16)
            .map(|index| heard_ms(index) as f32 / 32_768.0)
            .collect();
        let audio = RecordedAudio {
            offset_ms: 200,
            ..recorded_audio_from_wav(encode_wav_16k(&samples).unwrap()).unwrap()
        };
        let cuts = crate::pause::Cuts::new(vec![crate::pause::Span {
            start_ms: paused.start as u64,
            end_ms: paused.end as u64,
        }]);
        let cut = audio.cut(&cuts).unwrap();
        let kept: Vec<usize> = decode_wav_pcm16(&cut.wav_bytes)
            .unwrap()
            .into_iter()
            .map(|ms| ms as usize)
            .collect();
        let stayed: Vec<usize> = kept
            .iter()
            .copied()
            .filter(|ms| paused.contains(ms))
            .collect();
        assert!(
            stayed.is_empty(),
            "{} paused samples stayed, heard from {:?} to {:?} ms",
            stayed.len(),
            stayed.first(),
            stayed.last()
        );
        let expected: Vec<usize> = (0..8_800 * 16)
            .map(heard_ms)
            .filter(|ms| !paused.contains(ms))
            .collect();
        assert_eq!(kept.len(), expected.len(), "kept samples were lost");
        assert!(
            kept == expected,
            "the kept samples are not the unpaused ones"
        );
        // It still starts 200 ms into the capture, which is before the pause.
        assert_eq!((cut.duration_ms, cut.offset_ms), (6_800, 200));
    }

    #[test]
    fn stop_joins_worker_before_reading_its_last_samples() {
        let samples = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&samples);
        let clock_started = Instant::now();
        let (stop_sender, stop_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // Model a final callback flushed while the worker drops its stream.
            assert!(matches!(
                stop_receiver.recv_timeout(Duration::from_secs(2)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ));
            output.lock().unwrap().extend([0.1, 0.2, 0.3]);
            Ok(())
        });
        let audio = AudioRecorder {
            worker: Some(worker),
            stop_sender: Some(stop_sender),
            samples,
            clock_started,
            heard_at: Arc::new(OnceLock::from(first_heard(
                clock_started + Duration::from_millis(230),
                480,
                48_000,
            ))),
            error: Arc::new(Mutex::new(None)),
            live: None,
            preview: None,
            shown: None,
        }
        .stop()
        .unwrap();
        assert_eq!(audio.sample_count, 3);
        // The first buffer (480 frames at 48 kHz, 10 ms) arrived 230 ms into the capture, so
        // its first sample was heard at 220 ms.
        assert_eq!(audio.offset_ms, 220);
        assert_eq!(
            hound::WavReader::new(Cursor::new(audio.wav_bytes))
                .unwrap()
                .duration(),
            3
        );
    }

    #[test]
    fn a_device_lost_partway_keeps_what_it_heard() {
        let recorder = |samples: Vec<f32>| AudioRecorder {
            worker: None,
            stop_sender: None,
            samples: Arc::new(Mutex::new(samples)),
            clock_started: Instant::now(),
            heard_at: Default::default(),
            error: Arc::new(Mutex::new(Some("Device disconnected".into()))),
            live: None,
            preview: None,
            shown: None,
        };
        assert_eq!(recorder(vec![0.1; 160]).stop().unwrap().sample_count, 160);
        assert!(recorder(Vec::new()).stop().is_err());
    }

    #[test]
    fn stop_propagates_background_initialization_failure() {
        let recorder = AudioRecorder {
            worker: Some(std::thread::spawn(|| bail!("device unavailable"))),
            stop_sender: None,
            samples: Arc::new(Mutex::new(Vec::new())),
            clock_started: Instant::now(),
            heard_at: Default::default(),
            error: Arc::new(Mutex::new(None)),
            live: None,
            preview: None,
            shown: None,
        };
        assert!(
            recorder
                .stop()
                .unwrap_err()
                .to_string()
                .contains("device unavailable")
        );
    }

    #[test]
    fn speech_islands_merge_only_short_silences() {
        let mut samples = vec![0.0; 32_000];
        samples[1_600..4_800].fill(0.2); // 100..300
        samples[8_800..12_800].fill(0.2); // 550..800: merge 250 ms gap
        samples[18_400..20_800].fill(0.2); // 1150..1300: keep 350 ms gap
        assert_eq!(speech_islands(&samples), vec![(100, 800), (1_150, 1_300)]);
        assert!(speech_islands(&[0.001; 800]).is_empty());
        assert!(speech_islands(&[]).is_empty());
    }

    #[test]
    fn speech_islands_adapt_to_a_noisy_floor() {
        let mut samples: Vec<f32> = (0..32_000)
            .map(|i| if i % 2 == 0 { 0.006 } else { -0.006 })
            .collect();
        samples[8_000..16_000]
            .iter_mut()
            .for_each(|s| *s = if *s > 0.0 { 0.15 } else { -0.15 });
        assert_eq!(speech_islands(&samples), vec![(500, 1_000)]);
    }

    #[test]
    fn decoded_wav_round_trips() {
        let wav = encode_wav_16k(&[0.0, 0.5, -0.5]).unwrap();
        assert_eq!(decode_wav_16k(&wav).unwrap(), vec![0.0, 0.5, -0.5]);
        assert!(decode_wav_16k(b"nope").is_err());
    }

    #[test]
    fn wav_is_mono_16k_pcm_and_clips_safely() {
        let bytes = encode_wav_16k(&[-2.0, -0.5, 0.0, 0.5, 2.0, f32::NAN]).unwrap();
        let mut reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
        assert_eq!(reader.spec().channels, 1);
        assert_eq!(reader.spec().sample_rate, 16_000);
        assert_eq!(reader.spec().bits_per_sample, 16);
        assert_eq!(reader.spec().sample_format, hound::SampleFormat::Int);
        assert_eq!(
            reader
                .samples::<i16>()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap(),
            vec![-32_768, -16_384, 0, 16_384, 32_767, 0]
        );
    }
}
