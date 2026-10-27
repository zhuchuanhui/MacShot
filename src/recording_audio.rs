use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

use cpal::{
    Data, Sample, SampleFormat,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
#[cfg(target_os = "macos")]
use objc2_foundation::NSProcessInfo;
use scrcap::{AudioConfig, AudioFrame, CaptureConfig, CaptureDescriptor, Target, VideoConfig};
use zvidlib_aac_encoder::native_aac_audio_encoder_factory;
use zvidlib_core::{
    AudioBuffer, AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, Codec, CodecProfile,
    FrameIndex, Limits, SampleRange,
};

const OUTPUT_RATE: u32 = 48_000;
const OUTPUT_CHANNELS: u16 = 2;
const AAC_BITRATE: u32 = 160_000;
const AAC_BLOCK_SAMPLES: usize = 1_024;
const MIX_LOOKAHEAD_NS: i64 = 120_000_000;
const CHANNEL_CAPACITY: usize = 128;
const MICROPHONE_SIGNAL_THRESHOLD: f32 = 0.002;
const MICROPHONE_SILENCE_WARNING_AFTER: Duration = Duration::from_secs(3);
#[cfg(target_os = "macos")]
const MICROPHONE_TEST_BASELINE_WINDOW: Duration = Duration::from_secs(1);
#[cfg(any(target_os = "macos", test))]
const MICROPHONE_TEST_SIGNAL_MULTIPLIER: f32 = 2.5;
#[cfg(any(target_os = "macos", test))]
const MICROPHONE_TEST_SIGNAL_FLOOR: f32 = 0.003;

struct PcmChunk {
    start_sample: u64,
    samples: Vec<f32>,
}

pub struct AudioCaptureSession {
    system_capture: scrcap::CaptureDesc,
    system_rx: crossbeam_channel::Receiver<AudioFrame>,
    mic_stream: Option<MicrophoneStream>,
    mic_rx: Option<Receiver<PcmChunk>>,
    mic_error_rx: Option<Receiver<String>>,
    mic_dropped: Option<Arc<AtomicBool>>,
    mic_started_at: Option<Instant>,
    mic_last_data_at: Option<Instant>,
    mic_last_signal_at: Option<Instant>,
    microphone_warning: Option<String>,
}

#[cfg(target_os = "macos")]
pub enum MicrophoneTestEvent {
    Level(f32),
    Finished { peak: f32, detected: bool },
    Failed(String),
}

struct MicrophoneCapture {
    stream: MicrophoneStream,
    receiver: Receiver<PcmChunk>,
    error_receiver: Receiver<String>,
    dropped: Arc<AtomicBool>,
}

enum MicrophoneStream {
    Cpal(cpal::Stream),
    #[cfg(target_os = "macos")]
    ScreenCaptureKit(screencapturekit::stream::SCStream),
}

impl MicrophoneStream {
    fn pause(&self) {
        match self {
            Self::Cpal(stream) => {
                let _ = stream.pause();
            }
            #[cfg(target_os = "macos")]
            Self::ScreenCaptureKit(stream) => {
                stream.stop_capture().ok();
            }
        }
    }
}

impl AudioCaptureSession {
    pub fn start(include_microphone: bool) -> Result<Self, String> {
        let system_capture = CaptureConfig {
            video: VideoConfig {
                channel_capacity: 1,
                hide: Vec::new(),
                target: Target::Primary,
                fps: Some(1),
            },
            audio: Some(AudioConfig {
                channel_capacity: CHANNEL_CAPACITY,
            }),
        }
        .create()
        .map_err(|error| format!("システム音声を開始できません: {error}"))?;
        let system_rx = system_capture
            .audio()
            .cloned()
            .ok_or_else(|| "システム音声の入力を開始できません。".to_string())?;

        let (microphone, microphone_warning) = if include_microphone {
            match start_microphone() {
                Ok(microphone) => (Some(microphone), None),
                Err(error) => (
                    None,
                    Some(format!(
                        "{error}。システム音声のみを録音しています。マイク設定を確認してください。"
                    )),
                ),
            }
        } else {
            (
                None,
                Some("マイク未許可のため、システム音声のみを録音しています。".into()),
            )
        };

        let mic_started_at = microphone.as_ref().map(|_| Instant::now());
        let (mic_stream, mic_rx, mic_error_rx, mic_dropped) = match microphone {
            Some(capture) => (
                Some(capture.stream),
                Some(capture.receiver),
                Some(capture.error_receiver),
                Some(capture.dropped),
            ),
            None => (None, None, None, None),
        };
        Ok(Self {
            system_capture,
            system_rx,
            mic_stream,
            mic_rx,
            mic_error_rx,
            mic_dropped,
            mic_started_at,
            mic_last_data_at: None,
            mic_last_signal_at: None,
            microphone_warning,
        })
    }

    pub fn microphone_warning(&self) -> Option<&str> {
        self.microphone_warning.as_deref()
    }

    fn disable_microphone(&mut self, warning: String) {
        if let Some(stream) = self.mic_stream.take() {
            stream.pause();
        }
        self.mic_rx = None;
        self.mic_error_rx = None;
        self.mic_dropped = None;
        self.microphone_warning = Some(format!("{warning}。システム音声のみを録音しています。"));
    }

    fn check_health(&mut self) {
        if self
            .mic_dropped
            .as_ref()
            .is_some_and(|dropped| dropped.load(Ordering::Relaxed))
        {
            self.disable_microphone("マイク音声が混み合い、取り込みを継続できません".into());
            return;
        }
        let microphone_error = self
            .mic_error_rx
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some(error) = microphone_error {
            self.microphone_warning = Some(format!(
                "{error}。マイク入力の復帰を待ちながら録画を続けています。"
            ));
        }
    }

    pub fn drain_into(&mut self, mixer: &mut AudioMixer) -> Result<(), String> {
        self.check_health();
        while let Ok(frame) = self.system_rx.try_recv() {
            mixer.push_system(frame)?;
        }
        while let Some(chunk) = self
            .mic_rx
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            let now = Instant::now();
            self.mic_last_data_at = Some(now);
            if microphone_chunk_has_signal(&chunk.samples) {
                self.mic_last_signal_at = Some(now);
            }
            mixer.push_mic(chunk);
        }
        let last_mic_activity = self.mic_last_data_at.or(self.mic_started_at);
        if self.mic_stream.is_some()
            && last_mic_activity.is_some_and(|last| last.elapsed() > Duration::from_secs(2))
        {
            self.disable_microphone("マイク音声を2秒以上受信できませんでした".into());
            return Ok(());
        }
        Ok(())
    }

    fn report_silent_microphone(&mut self) {
        let elapsed = self
            .mic_started_at
            .map_or(Duration::ZERO, |started| started.elapsed());
        if should_warn_about_silent_microphone(
            self.mic_stream.is_some(),
            elapsed,
            self.mic_last_signal_at.is_some(),
        ) {
            self.microphone_warning = Some(
                "マイクに音声が届きませんでした。入力デバイスと入力音量を確認してください".into(),
            );
        }
    }

    fn stop(&self) {
        if let Some(stream) = &self.mic_stream {
            stream.pause();
        }
        self.system_capture.terminate();
    }
}

fn start_microphone() -> Result<MicrophoneCapture, String> {
    #[cfg(target_os = "macos")]
    {
        start_cpal_microphone().or_else(|cpal_error| {
            start_screen_capturekit_microphone().map_err(|screen_capture_error| {
                format!("CoreAudio入力を開始できません: {cpal_error}。ScreenCaptureKitも開始できません: {screen_capture_error}")
            })
        })
    }
    #[cfg(not(target_os = "macos"))]
    start_cpal_microphone()
}

fn microphone_chunk_has_signal(samples: &[f32]) -> bool {
    samples
        .iter()
        .any(|sample| sample.is_finite() && sample.abs() >= MICROPHONE_SIGNAL_THRESHOLD)
}

#[cfg(target_os = "macos")]
pub fn start_microphone_level_test() -> Receiver<MicrophoneTestEvent> {
    let (event_tx, event_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let capture = match start_microphone() {
            Ok(capture) => capture,
            Err(error) => {
                let _ = event_tx.send(MicrophoneTestEvent::Failed(error));
                return;
            }
        };
        let MicrophoneCapture {
            stream,
            receiver,
            error_receiver,
            ..
        } = capture;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(5);
        let mut peak = 0.0_f32;
        let mut baseline_sum = 0.0_f64;
        let mut baseline_chunks = 0_u64;
        let mut signal_chunks = 0_u8;
        while Instant::now() < deadline {
            while let Ok(chunk) = receiver.try_recv() {
                let chunk_peak = pcm_peak(&chunk.samples);
                peak = peak.max(chunk_peak);
                let rms = pcm_rms(&chunk.samples);
                if started.elapsed() < MICROPHONE_TEST_BASELINE_WINDOW {
                    baseline_sum += f64::from(rms);
                    baseline_chunks += 1;
                } else {
                    let baseline = if baseline_chunks == 0 {
                        MICROPHONE_TEST_SIGNAL_FLOOR
                    } else {
                        (baseline_sum / baseline_chunks as f64) as f32
                    };
                    if microphone_test_signal_detected(rms, baseline) {
                        signal_chunks = signal_chunks.saturating_add(1);
                    }
                }
            }
            if let Ok(error) = error_receiver.try_recv() {
                stream.pause();
                let _ = event_tx.send(MicrophoneTestEvent::Failed(error));
                return;
            }
            if event_tx.send(MicrophoneTestEvent::Level(peak)).is_err() {
                stream.pause();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        stream.pause();
        let _ = event_tx.send(MicrophoneTestEvent::Finished {
            peak,
            detected: signal_chunks >= 3,
        });
    });
    event_rx
}

#[cfg(target_os = "macos")]
fn pcm_peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .copied()
        .filter(|sample| sample.is_finite())
        .map(f32::abs)
        .fold(0.0_f32, f32::max)
}

#[cfg(target_os = "macos")]
fn pcm_rms(samples: &[f32]) -> f32 {
    let (sum, count) = samples
        .iter()
        .copied()
        .filter(|sample| sample.is_finite())
        .fold((0.0_f64, 0_u64), |(sum, count), sample| {
            (sum + f64::from(sample * sample), count + 1)
        });
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt() as f32
    }
}

#[cfg(any(target_os = "macos", test))]
fn microphone_test_signal_detected(rms: f32, ambient_rms: f32) -> bool {
    let threshold = (ambient_rms.max(0.0) * MICROPHONE_TEST_SIGNAL_MULTIPLIER)
        .max(MICROPHONE_TEST_SIGNAL_FLOOR);
    rms.is_finite() && rms >= threshold
}

fn should_warn_about_silent_microphone(
    microphone_started: bool,
    elapsed: Duration,
    signal_seen: bool,
) -> bool {
    microphone_started && elapsed >= MICROPHONE_SILENCE_WARNING_AFTER && !signal_seen
}

#[cfg(target_os = "macos")]
fn start_screen_capturekit_microphone() -> Result<MicrophoneCapture, String> {
    use screencapturekit::{
        prelude::{SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration},
        stream::output_type::SCStreamOutputType,
    };

    let system_major_version = NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion;
    if !screen_capturekit_microphone_supported(system_major_version) {
        return Err("ScreenCaptureKitのマイク入力にはmacOS 15以降が必要です".into());
    }

    let content = SCShareableContent::get()
        .map_err(|error| format!("ScreenCaptureKitの画面一覧を取得できません: {error}"))?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .ok_or_else(|| "ScreenCaptureKitで画面が見つかりません。".to_string())?;
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&[])
        .build()
        .map_err(|error| format!("マイク入力を準備できません: {error}"))?;
    // ScreenCaptureKit expects a normal video stream configuration even when
    // the app only registers the microphone output handler. Tiny 2×2 output
    // dimensions can start successfully but fail to deliver microphone buffers
    // on some macOS versions.
    let config = SCStreamConfiguration::new()
        .with_width(320)
        .with_height(240)
        .with_captures_audio(true)
        .with_sample_rate(OUTPUT_RATE as i32)
        .with_channel_count(OUTPUT_CHANNELS as i32)
        .with_captures_microphone(true)
        .map_err(|error| format!("このmacOSではScreenCaptureKitマイク入力を使えません: {error}"))?;
    let (mic_tx, mic_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
    let mic_dropped = Arc::new(AtomicBool::new(false));
    let dropped_flag = mic_dropped.clone();
    let callback_tx = mic_tx.clone();
    let mut stream = SCStream::new(&filter, &config)
        .map_err(|error| format!("ScreenCaptureKitマイクを準備できません: {error}"))?;
    stream
        .add_output_handler(
            move |sample, output_type| {
                if output_type != SCStreamOutputType::Microphone {
                    return;
                }
                let Some(samples) = screen_capturekit_sample_to_stereo_48khz(&sample) else {
                    return;
                };
                if samples.is_empty()
                    || callback_tx
                        .try_send(PcmChunk {
                            start_sample: 0,
                            samples,
                        })
                        .is_err()
                {
                    dropped_flag.store(true, Ordering::Relaxed);
                }
            },
            SCStreamOutputType::Microphone,
        )
        .map_err(|error| format!("ScreenCaptureKitマイクを接続できません: {error}"))?;
    stream
        .start_capture()
        .map_err(|error| format!("ScreenCaptureKitマイクを開始できません: {error}"))?;
    let (_error_tx, error_rx) = mpsc::channel();
    Ok(MicrophoneCapture {
        stream: MicrophoneStream::ScreenCaptureKit(stream),
        receiver: mic_rx,
        error_receiver: error_rx,
        dropped: mic_dropped,
    })
}

#[cfg(any(target_os = "macos", test))]
fn screen_capturekit_microphone_supported(major_version: isize) -> bool {
    major_version >= 15
}

#[cfg(target_os = "macos")]
fn screen_capturekit_sample_to_stereo_48khz(
    sample: &screencapturekit::CMSampleBuffer,
) -> Option<Vec<f32>> {
    let format = sample.format_description()?;
    if !format.audio_is_float() || format.audio_bits_per_channel()? != 32 {
        return None;
    }
    let channels = format.audio_channel_count()? as usize;
    let sample_rate = format.audio_sample_rate()? as u32;
    let list = sample.audio_buffer_list().ok()?;
    let buffers = list
        .iter()
        .map(|buffer| {
            let bytes = buffer.data();
            let (sample_bytes, _) = bytes.as_chunks::<4>();
            let samples = sample_bytes
                .iter()
                .map(|chunk| f32::from_ne_bytes(*chunk))
                .collect::<Vec<_>>();
            (buffer.number_channels as usize, samples)
        })
        .collect::<Vec<_>>();
    let interleaved = interleave_audio_buffers(&buffers, channels)?;
    Some(convert_to_stereo_48khz(&interleaved, channels, sample_rate))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn interleave_audio_buffers(buffers: &[(usize, Vec<f32>)], channels: usize) -> Option<Vec<f32>> {
    if channels == 0 || buffers.is_empty() {
        return None;
    }
    if buffers.len() == 1 && buffers[0].0 == channels {
        return Some(buffers[0].1.clone());
    }
    if buffers.len() != channels || buffers.iter().any(|(count, _)| *count != 1) {
        return None;
    }
    let frames = buffers.iter().map(|(_, samples)| samples.len()).min()?;
    let mut interleaved = Vec::with_capacity(frames * channels);
    for frame in 0..frames {
        for (_, samples) in buffers {
            interleaved.push(samples[frame]);
        }
    }
    Some(interleaved)
}

fn start_cpal_microphone() -> Result<MicrophoneCapture, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "マイク入力デバイスが見つかりません。".to_string())?;
    let supported = device
        .default_input_config()
        .map_err(|error| format!("マイクの設定を取得できません: {error}"))?;
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    if channels == 0 || sample_rate == 0 {
        return Err("マイクの音声形式が不正です。".into());
    }

    let (mic_tx, mic_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
    let (mic_error_tx, mic_error_rx) = mpsc::channel();
    let mic_dropped = Arc::new(AtomicBool::new(false));
    let dropped_flag = mic_dropped.clone();
    let callback_tx = mic_tx.clone();
    let callback_error_tx = mic_error_tx.clone();
    let sample_format = supported.sample_format();
    let stream = device
        .build_input_stream_raw(
            supported.config(),
            sample_format,
            move |data, _info| {
                let Some(raw) = sample_data_as_f32(data, sample_format) else {
                    let _ = callback_error_tx.send("マイクの音声形式を変換できません。".into());
                    return;
                };
                let samples = convert_to_stereo_48khz(&raw, channels, sample_rate);
                if samples.is_empty() {
                    return;
                }
                if callback_tx
                    .try_send(PcmChunk {
                        start_sample: 0,
                        samples,
                    })
                    .is_err()
                {
                    dropped_flag.store(true, Ordering::Relaxed);
                }
            },
            move |error| {
                let _ = mic_error_tx.send(format!("マイク収録が停止しました: {error}"));
            },
            None,
        )
        .map_err(|error| format!("マイクを開始できません: {error}"))?;
    stream
        .play()
        .map_err(|error| format!("マイクを開始できません: {error}"))?;
    Ok(MicrophoneCapture {
        stream: MicrophoneStream::Cpal(stream),
        receiver: mic_rx,
        error_receiver: mic_error_rx,
        dropped: mic_dropped,
    })
}

pub struct AudioMixer {
    next_system_sample: u64,
    next_microphone_sample: u64,
    system: VecDeque<PcmChunk>,
    microphone: VecDeque<PcmChunk>,
    latest_system_ns: Option<i64>,
    latest_microphone_ns: Option<i64>,
    mixed_samples: u64,
}

impl AudioMixer {
    pub fn new() -> Self {
        Self {
            next_system_sample: 0,
            next_microphone_sample: 0,
            system: VecDeque::new(),
            microphone: VecDeque::new(),
            latest_system_ns: None,
            latest_microphone_ns: None,
            mixed_samples: 0,
        }
    }

    fn push_system(&mut self, frame: AudioFrame) -> Result<(), String> {
        let samples = audio_frame_to_stereo_48khz(&frame)?;
        self.push_chunk(
            true,
            PcmChunk {
                start_sample: 0,
                samples,
            },
        );
        Ok(())
    }

    fn push_mic(&mut self, chunk: PcmChunk) {
        self.push_chunk(false, chunk);
    }

    fn push_chunk(&mut self, system: bool, mut chunk: PcmChunk) {
        if chunk.samples.is_empty() {
            return;
        }
        let next_sample = if system {
            &mut self.next_system_sample
        } else {
            &mut self.next_microphone_sample
        };
        chunk.start_sample = *next_sample;
        *next_sample =
            next_sample.saturating_add((chunk.samples.len() / OUTPUT_CHANNELS as usize) as u64);
        let end_ns = (*next_sample as u128 * 1_000_000_000u128 / OUTPUT_RATE as u128)
            .min(i64::MAX as u128) as i64;
        if system {
            self.latest_system_ns = Some(self.latest_system_ns.unwrap_or(i64::MIN).max(end_ns));
            insert_timed_chunk(&mut self.system, chunk);
        } else {
            self.latest_microphone_ns =
                Some(self.latest_microphone_ns.unwrap_or(i64::MIN).max(end_ns));
            insert_timed_chunk(&mut self.microphone, chunk);
        }
    }

    pub fn take_ready_through(&mut self, max_elapsed_ns: i64) -> Vec<(u64, Vec<f32>)> {
        let Some(latest_ns) = self
            .latest_system_ns
            .into_iter()
            .chain(self.latest_microphone_ns)
            .max()
        else {
            return Vec::new();
        };
        self.take_through(
            latest_ns
                .saturating_sub(MIX_LOOKAHEAD_NS)
                .min(max_elapsed_ns.max(0)),
        )
    }

    fn take_through(&mut self, watermark_ns: i64) -> Vec<(u64, Vec<f32>)> {
        let max_samples = if watermark_ns <= 0 {
            0
        } else {
            (watermark_ns as u128 * OUTPUT_RATE as u128 / 1_000_000_000).min(u64::MAX as u128)
                as u64
        };
        let mut blocks = Vec::new();
        while max_samples.saturating_sub(self.mixed_samples) >= AAC_BLOCK_SAMPLES as u64 {
            let start = self.mixed_samples;
            let end = start + AAC_BLOCK_SAMPLES as u64;
            let mut samples = vec![0.0f32; AAC_BLOCK_SAMPLES * OUTPUT_CHANNELS as usize];
            mix_queue(&mut self.system, start, end, &mut samples);
            mix_queue(&mut self.microphone, start, end, &mut samples);
            for sample in &mut samples {
                *sample = (*sample * 0.72).clamp(-1.0, 1.0);
            }
            blocks.push((start, samples));
            self.mixed_samples = end;
        }
        blocks
    }

    pub fn finish(
        &mut self,
        capture: &mut AudioCaptureSession,
        max_elapsed_ns: i64,
    ) -> Result<Vec<(u64, Vec<f32>)>, String> {
        capture.stop();
        capture.drain_into(self)?;
        capture.report_silent_microphone();
        let end_ns = self
            .latest_system_ns
            .into_iter()
            .chain(self.latest_microphone_ns)
            .max()
            .ok_or_else(|| "録音した音声データがありません。".to_string())?
            .min(max_elapsed_ns.max(0));
        let mut blocks = self.take_through(end_ns);
        let remaining = end_ns;
        {
            let samples = if remaining <= 0 {
                0
            } else {
                (remaining as u128 * OUTPUT_RATE as u128 / 1_000_000_000).min(u64::MAX as u128)
                    as u64
            };
            let pending = samples.saturating_sub(self.mixed_samples) as usize;
            if pending > 0 {
                let mut tail = vec![0.0f32; AAC_BLOCK_SAMPLES * OUTPUT_CHANNELS as usize];
                let end = self.mixed_samples + pending as u64;
                mix_queue(&mut self.system, self.mixed_samples, end, &mut tail);
                mix_queue(&mut self.microphone, self.mixed_samples, end, &mut tail);
                for sample in &mut tail {
                    *sample = (*sample * 0.72).clamp(-1.0, 1.0);
                }
                blocks.push((self.mixed_samples, tail));
                self.mixed_samples = end;
            }
        }
        Ok(blocks)
    }
}

pub fn create_aac_encoder() -> Result<Box<dyn AudioEncoder>, String> {
    let config = AudioEncoderConfig {
        codec: Codec::Aac,
        profile: CodecProfile::AacLowComplexity,
        sample_rate: OUTPUT_RATE,
        channels: OUTPUT_CHANNELS,
        timescale: OUTPUT_RATE,
        configuration: AAC_BITRATE.to_be_bytes().to_vec(),
    };
    native_aac_audio_encoder_factory()
        .create(&config, &Limits::default())
        .map_err(|error| format!("AAC音声エンコーダーを開始できません: {error}"))
}

pub fn encode_block(
    encoder: &mut dyn AudioEncoder,
    start: u64,
    samples: Vec<f32>,
) -> Result<Vec<zvidlib_core::EncodedSample>, String> {
    let sample_count = samples.len() / OUTPUT_CHANNELS as usize;
    let end = start
        .checked_add(sample_count as u64)
        .ok_or_else(|| "音声の時間情報が上限を超えました。".to_string())?;
    let buffer = AudioBuffer::new(
        SampleRange::new(start, end).map_err(|error| error.to_string())?,
        OUTPUT_RATE,
        OUTPUT_CHANNELS,
        samples,
        &Limits::default(),
    )
    .map_err(|error| error.to_string())?;
    pollster::block_on(encoder.encode(FrameIndex(start), buffer))
        .map_err(|error| format!("AAC音声をエンコードできません: {error}"))
}

pub fn finish_encoder(
    encoder: &mut dyn AudioEncoder,
) -> Result<Vec<zvidlib_core::EncodedSample>, String> {
    pollster::block_on(encoder.finish())
        .map(|drain| drain.samples)
        .map_err(|error| format!("AAC音声を確定できません: {error}"))
}

pub const fn bitrate() -> u32 {
    AAC_BITRATE
}

pub const fn sample_rate() -> u32 {
    OUTPUT_RATE
}

fn insert_timed_chunk(queue: &mut VecDeque<PcmChunk>, chunk: PcmChunk) {
    if queue.is_empty()
        || queue
            .back()
            .is_some_and(|last| last.start_sample <= chunk.start_sample)
    {
        queue.push_back(chunk);
        return;
    }
    let position = queue
        .iter()
        .position(|queued| queued.start_sample > chunk.start_sample)
        .unwrap_or(queue.len());
    queue.insert(position, chunk);
}

fn mix_queue(queue: &mut VecDeque<PcmChunk>, start: u64, end: u64, output: &mut [f32]) {
    while queue
        .front()
        .is_some_and(|chunk| chunk_end_sample(chunk) <= start)
    {
        queue.pop_front();
    }
    for chunk in queue.iter() {
        let chunk_start = chunk_start_sample(chunk);
        let chunk_end = chunk_end_sample(chunk);
        if chunk_start >= end {
            break;
        }
        let overlap_start = start.max(chunk_start);
        let overlap_end = end.min(chunk_end);
        if overlap_start >= overlap_end {
            continue;
        }
        let from = (overlap_start - chunk_start) as usize * OUTPUT_CHANNELS as usize;
        let to = (overlap_end - chunk_start) as usize * OUTPUT_CHANNELS as usize;
        let output_from = (overlap_start - start) as usize * OUTPUT_CHANNELS as usize;
        for (index, sample) in chunk.samples[from..to].iter().enumerate() {
            output[output_from + index] += sample;
        }
    }
}

fn chunk_start_sample(chunk: &PcmChunk) -> u64 {
    chunk.start_sample
}

fn chunk_end_sample(chunk: &PcmChunk) -> u64 {
    chunk_start_sample(chunk) + (chunk.samples.len() / OUTPUT_CHANNELS as usize) as u64
}

fn audio_frame_to_stereo_48khz(frame: &AudioFrame) -> Result<Vec<f32>, String> {
    let channels = usize::try_from(frame.nb_channels)
        .ok()
        .filter(|channels| *channels > 0)
        .ok_or_else(|| "システム音声のチャンネル数が不正です。".to_string())?;
    let samples_per_channel = usize::try_from(frame.nb_samples)
        .map_err(|_| "システム音声のサンプル数が不正です。".to_string())?;
    let decoded = decode_pcm(
        &frame.aframe,
        samples_per_channel,
        channels,
        frame.sample_fmt,
    )?;
    Ok(convert_to_stereo_48khz(
        &decoded,
        channels,
        frame.sample_rate as u32,
    ))
}

fn decode_pcm(
    bytes: &[u8],
    samples_per_channel: usize,
    channels: usize,
    format: scrcap::SampleFmt,
) -> Result<Vec<f32>, String> {
    let bytes_per_sample = format.bytes_per_sample();
    let planar = format.is_planar();
    let required = samples_per_channel
        .checked_mul(channels)
        .and_then(|samples| samples.checked_mul(bytes_per_sample))
        .ok_or_else(|| "システム音声のサイズが上限を超えました。".to_string())?;
    if bytes.len() < required {
        return Err("システム音声データが途中で切れています。".into());
    }
    let mut output = Vec::with_capacity(samples_per_channel * channels);
    for sample in 0..samples_per_channel {
        for channel in 0..channels {
            let index = if planar {
                (channel * samples_per_channel + sample) * bytes_per_sample
            } else {
                (sample * channels + channel) * bytes_per_sample
            };
            let raw = &bytes[index..index + bytes_per_sample];
            output.push(match format {
                scrcap::SampleFmt::U8 | scrcap::SampleFmt::U8P => (raw[0] as f32 - 128.0) / 128.0,
                scrcap::SampleFmt::S16 | scrcap::SampleFmt::S16P => {
                    i16::from_ne_bytes([raw[0], raw[1]]) as f32 / 32_768.0
                }
                scrcap::SampleFmt::S32 | scrcap::SampleFmt::S32P => {
                    i32::from_ne_bytes(raw.try_into().unwrap()) as f32 / 2_147_483_648.0
                }
                scrcap::SampleFmt::F32 | scrcap::SampleFmt::F32P => {
                    f32::from_ne_bytes(raw.try_into().unwrap())
                }
                scrcap::SampleFmt::F64 | scrcap::SampleFmt::F64P => {
                    f64::from_ne_bytes(raw.try_into().unwrap()) as f32
                }
                _ => return Err("システム音声の音声形式に対応していません。".into()),
            });
        }
    }
    Ok(output)
}

fn convert_to_stereo_48khz(input: &[f32], channels: usize, input_rate: u32) -> Vec<f32> {
    if channels == 0 || input_rate == 0 || input.len() < channels {
        return Vec::new();
    }
    let input_frames = input.len() / channels;
    let output_frames = (input_frames as u128 * OUTPUT_RATE as u128 / input_rate as u128) as usize;
    if output_frames == 0 {
        return Vec::new();
    }
    let mut output = Vec::with_capacity(output_frames * OUTPUT_CHANNELS as usize);
    for output_index in 0..output_frames {
        let source_position = output_index as f64 * input_rate as f64 / OUTPUT_RATE as f64;
        let source_start = (source_position.floor() as usize).min(input_frames - 1);
        let source_end = (source_start + 1).min(input_frames - 1);
        let fraction = (source_position - source_start as f64) as f32;
        let left_a = input[source_start * channels];
        let left_b = input[source_end * channels];
        let right_a = if channels > 1 {
            input[source_start * channels + 1]
        } else {
            left_a
        };
        let right_b = if channels > 1 {
            input[source_end * channels + 1]
        } else {
            left_b
        };
        output.push(left_a + (left_b - left_a) * fraction);
        output.push(right_a + (right_b - right_a) * fraction);
    }
    output
}

fn sample_data_as_f32(data: &Data, format: SampleFormat) -> Option<Vec<f32>> {
    macro_rules! convert {
        ($sample:ty) => {
            data.as_slice::<$sample>().map(|samples| {
                samples
                    .iter()
                    .map(|sample| sample.to_sample::<f32>())
                    .collect()
            })
        };
    }
    match format {
        SampleFormat::I8 => convert!(i8),
        SampleFormat::I16 => convert!(i16),
        SampleFormat::I24 => convert!(cpal::I24),
        SampleFormat::I32 => convert!(i32),
        SampleFormat::I64 => convert!(i64),
        SampleFormat::U8 => convert!(u8),
        SampleFormat::U16 => convert!(u16),
        SampleFormat::U24 => convert!(cpal::U24),
        SampleFormat::U32 => convert!(u32),
        SampleFormat::U64 => convert!(u64),
        SampleFormat::F32 => convert!(f32),
        SampleFormat::F64 => convert!(f64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_signal_detection_rejects_silence_and_non_finite_samples() {
        assert!(!microphone_chunk_has_signal(&[0.0, -0.001, f32::NAN]));
        assert!(microphone_chunk_has_signal(&[
            0.0,
            MICROPHONE_SIGNAL_THRESHOLD
        ]));
    }

    #[test]
    fn microphone_test_ignores_ambient_level_and_requires_clearer_input() {
        assert!(!microphone_test_signal_detected(0.05, 0.03));
        assert!(microphone_test_signal_detected(0.08, 0.03));
        assert!(!microphone_test_signal_detected(f32::NAN, 0.03));
    }

    #[test]
    fn silent_microphone_warning_requires_a_started_stream_and_minimum_duration() {
        assert!(!should_warn_about_silent_microphone(
            false,
            Duration::from_secs(5),
            false
        ));
        assert!(!should_warn_about_silent_microphone(
            true,
            Duration::from_secs(2),
            false
        ));
        assert!(!should_warn_about_silent_microphone(
            true,
            Duration::from_secs(5),
            true
        ));
        assert!(should_warn_about_silent_microphone(
            true,
            Duration::from_secs(3),
            false
        ));
    }

    #[test]
    fn screen_capturekit_microphone_fallback_requires_macos_15() {
        assert!(!screen_capturekit_microphone_supported(14));
        assert!(screen_capturekit_microphone_supported(15));
        assert!(screen_capturekit_microphone_supported(16));
    }

    #[test]
    fn stereo_resampler_duplicates_mono_and_preserves_stereo() {
        assert_eq!(
            convert_to_stereo_48khz(&[0.25, -0.5], 1, 48_000),
            vec![0.25, 0.25, -0.5, -0.5]
        );
        assert_eq!(
            convert_to_stereo_48khz(&[0.1, -0.1, 0.4, -0.4], 2, 48_000),
            vec![0.1, -0.1, 0.4, -0.4]
        );
    }

    #[test]
    fn screen_capturekit_pcm_keeps_interleaved_stereo_samples() {
        let buffers = vec![(2, vec![0.1, -0.1, 0.4, -0.4])];

        assert_eq!(
            interleave_audio_buffers(&buffers, 2),
            Some(vec![0.1, -0.1, 0.4, -0.4])
        );
    }

    #[test]
    fn screen_capturekit_pcm_interleaves_separate_channel_buffers() {
        let buffers = vec![(1, vec![0.1, 0.4]), (1, vec![-0.1, -0.4])];

        assert_eq!(
            interleave_audio_buffers(&buffers, 2),
            Some(vec![0.1, -0.1, 0.4, -0.4])
        );
    }

    #[test]
    fn screen_capturekit_pcm_rejects_unexpected_channel_layouts() {
        assert_eq!(interleave_audio_buffers(&[], 2), None);
        assert_eq!(interleave_audio_buffers(&[(1, vec![0.1])], 2), None);
        assert_eq!(
            interleave_audio_buffers(&[(2, vec![0.1]), (1, vec![-0.1])], 2),
            None
        );
    }

    #[test]
    fn mixer_sums_both_sources_and_clamps_the_result() {
        let mut mixer = AudioMixer::new();
        mixer.push_chunk(
            true,
            PcmChunk {
                start_sample: 0,
                samples: vec![0.8; AAC_BLOCK_SAMPLES * 2],
            },
        );
        mixer.push_chunk(
            false,
            PcmChunk {
                start_sample: 0,
                samples: vec![0.8; AAC_BLOCK_SAMPLES * 2],
            },
        );
        mixer.latest_system_ns = Some(500_000_000);
        mixer.latest_microphone_ns = Some(500_000_000);
        let blocks = mixer.take_through(500_000_000);
        assert!(!blocks.is_empty());
        assert!(
            blocks[0]
                .1
                .iter()
                .all(|sample| *sample <= 1.0 && *sample > 0.99)
        );
    }

    #[test]
    fn mixer_aligns_sources_without_relying_on_their_clock_origins() {
        let mut mixer = AudioMixer::new();
        mixer.push_chunk(
            true,
            PcmChunk {
                start_sample: 0,
                samples: vec![0.25; AAC_BLOCK_SAMPLES * 2],
            },
        );
        mixer.push_chunk(
            false,
            PcmChunk {
                start_sample: 0,
                samples: vec![0.25; AAC_BLOCK_SAMPLES * 2],
            },
        );
        mixer.latest_system_ns = Some(500_000_000);
        mixer.latest_microphone_ns = Some(500_000_000);
        let blocks = mixer.take_through(500_000_000);
        assert!(!blocks.is_empty());
        assert!((blocks[0].1[0] - 0.36).abs() < 0.001);
        assert!((blocks[0].1[960] - 0.36).abs() < 0.001);
    }

    #[test]
    fn mixer_can_progress_when_only_one_audio_source_is_available() {
        let mut mixer = AudioMixer::new();
        mixer.push_chunk(
            false,
            PcmChunk {
                start_sample: 0,
                samples: vec![0.2; AAC_BLOCK_SAMPLES * 2 * 20],
            },
        );
        let blocks = mixer.take_ready_through(i64::MAX);
        assert!(!blocks.is_empty());
        assert!(blocks[0].1.iter().any(|sample| *sample > 0.0));
    }

    #[test]
    fn mixer_never_emits_audio_beyond_real_elapsed_time() {
        let mut mixer = AudioMixer::new();
        mixer.push_chunk(
            true,
            PcmChunk {
                start_sample: 0,
                samples: vec![0.2; OUTPUT_RATE as usize * OUTPUT_CHANNELS as usize],
            },
        );
        let max_elapsed_ns = 300_000_000;
        let blocks = mixer.take_ready_through(max_elapsed_ns);
        let emitted_samples = blocks
            .iter()
            .map(|(_, samples)| samples.len() / 2)
            .sum::<usize>();
        assert!(
            emitted_samples as u128 * 1_000_000_000 / OUTPUT_RATE as u128 <= max_elapsed_ns as u128
        );
    }

    #[test]
    fn native_aac_encoder_outputs_packets_for_stereo_pcm() {
        let mut encoder = create_aac_encoder().unwrap();
        let pcm = (0..AAC_BLOCK_SAMPLES)
            .flat_map(|sample| {
                let value = (sample as f32 * 440.0 * std::f32::consts::TAU / OUTPUT_RATE as f32)
                    .sin()
                    * 0.2;
                [value, value]
            })
            .collect();
        let mut packets = encode_block(&mut *encoder, 0, pcm).unwrap();
        packets.extend(finish_encoder(&mut *encoder).unwrap());
        assert!(!packets.is_empty());
        assert!(packets.iter().all(|packet| packet.duration > 0));
        assert!(packets.iter().all(|packet| !packet.data.is_empty()));
    }
}
