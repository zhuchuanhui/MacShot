use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
};

use cpal::{
    Data, Sample, SampleFormat,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
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

struct PcmChunk {
    timestamp_ns: i64,
    start_sample: u64,
    samples: Vec<f32>,
}

pub struct AudioCaptureSession {
    system_capture: scrcap::CaptureDesc,
    system_rx: crossbeam_channel::Receiver<AudioFrame>,
    mic_stream: cpal::Stream,
    mic_rx: Receiver<PcmChunk>,
    mic_error_rx: Receiver<String>,
    mic_dropped: Arc<AtomicBool>,
}

impl AudioCaptureSession {
    pub fn start() -> Result<Self, String> {
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
                move |data, info| {
                    let Some(raw) = sample_data_as_f32(data, sample_format) else {
                        let _ = callback_error_tx.send("マイクの音声形式を変換できません。".into());
                        return;
                    };
                    let samples = convert_to_stereo_48khz(&raw, channels, sample_rate);
                    if samples.is_empty() {
                        return;
                    }
                    let timestamp_ns =
                        info.timestamp().capture.as_nanos().min(i64::MAX as u128) as i64;
                    if callback_tx
                        .try_send(PcmChunk {
                            timestamp_ns,
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

        Ok(Self {
            system_capture,
            system_rx,
            mic_stream: stream,
            mic_rx,
            mic_error_rx,
            mic_dropped,
        })
    }

    fn check_health(&self) -> Result<(), String> {
        if self.mic_dropped.load(Ordering::Relaxed) {
            return Err("マイク音声が混み合って一部を受け取れませんでした。録画を停止して再試行してください。".into());
        }
        match self.mic_error_rx.try_recv() {
            Ok(error) => Err(error),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => Ok(()),
        }
    }

    pub fn drain_into(&self, mixer: &mut AudioMixer) -> Result<(), String> {
        self.check_health()?;
        while let Ok(frame) = self.system_rx.try_recv() {
            mixer.push_system(frame)?;
        }
        while let Ok(chunk) = self.mic_rx.try_recv() {
            mixer.push_mic(chunk);
        }
        Ok(())
    }

    fn stop(&self) {
        let _ = self.mic_stream.pause();
        self.system_capture.terminate();
    }
}

pub struct AudioMixer {
    base_ns: Option<i64>,
    system: VecDeque<PcmChunk>,
    microphone: VecDeque<PcmChunk>,
    latest_system_ns: Option<i64>,
    latest_microphone_ns: Option<i64>,
    mixed_samples: u64,
}

impl AudioMixer {
    pub fn new() -> Self {
        Self {
            base_ns: None,
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
                timestamp_ns: i64::try_from(frame.ts).unwrap_or(i64::MAX),
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
        if self.base_ns.is_none() {
            self.base_ns = Some(chunk.timestamp_ns);
        }
        let Some(base_ns) = self.base_ns else {
            return;
        };
        if chunk.timestamp_ns < base_ns {
            let trim = ((base_ns - chunk.timestamp_ns) as i128 * OUTPUT_RATE as i128
                / 1_000_000_000) as usize;
            let trim = trim
                .saturating_mul(OUTPUT_CHANNELS as usize)
                .min(chunk.samples.len());
            chunk.samples.drain(..trim);
            chunk.timestamp_ns = base_ns;
        }
        if chunk.samples.is_empty() {
            return;
        }
        let start_sample = if chunk.timestamp_ns <= base_ns {
            0
        } else {
            ((chunk.timestamp_ns - base_ns) as u128 * OUTPUT_RATE as u128 / 1_000_000_000)
                .min(u64::MAX as u128) as u64
        };
        chunk.start_sample = start_sample;
        let duration_ns = (chunk.samples.len() / OUTPUT_CHANNELS as usize) as i128
            * 1_000_000_000i128
            / OUTPUT_RATE as i128;
        let end_ns = chunk
            .timestamp_ns
            .saturating_add(duration_ns.min(i64::MAX as i128) as i64);
        if system {
            self.latest_system_ns = Some(self.latest_system_ns.unwrap_or(i64::MIN).max(end_ns));
            insert_timed_chunk(&mut self.system, chunk);
        } else {
            self.latest_microphone_ns =
                Some(self.latest_microphone_ns.unwrap_or(i64::MIN).max(end_ns));
            insert_timed_chunk(&mut self.microphone, chunk);
        }
    }

    pub fn take_ready(&mut self) -> Vec<(u64, Vec<f32>)> {
        let Some(latest_ns) = self
            .latest_system_ns
            .into_iter()
            .chain(self.latest_microphone_ns)
            .max()
        else {
            return Vec::new();
        };
        self.take_through(latest_ns.saturating_sub(MIX_LOOKAHEAD_NS))
    }

    fn take_through(&mut self, watermark_ns: i64) -> Vec<(u64, Vec<f32>)> {
        let Some(base_ns) = self.base_ns else {
            return Vec::new();
        };
        let max_samples = if watermark_ns <= base_ns {
            0
        } else {
            ((watermark_ns - base_ns) as u128 * OUTPUT_RATE as u128 / 1_000_000_000)
                .min(u64::MAX as u128) as u64
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
        capture: &AudioCaptureSession,
    ) -> Result<Vec<(u64, Vec<f32>)>, String> {
        capture.stop();
        capture.drain_into(self)?;
        let end_ns = self
            .latest_system_ns
            .into_iter()
            .chain(self.latest_microphone_ns)
            .max()
            .ok_or_else(|| "録音した音声データがありません。".to_string())?;
        let mut blocks = self.take_through(end_ns);
        let remaining = end_ns;
        if let Some(base_ns) = self.base_ns {
            let samples = if remaining <= base_ns {
                0
            } else {
                ((remaining - base_ns) as u128 * OUTPUT_RATE as u128 / 1_000_000_000)
                    .min(u64::MAX as u128) as u64
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
    fn mixer_sums_both_sources_and_clamps_the_result() {
        let mut mixer = AudioMixer::new();
        let start = 1_000_000_000;
        mixer.push_chunk(
            true,
            PcmChunk {
                timestamp_ns: start,
                start_sample: 0,
                samples: vec![0.8; AAC_BLOCK_SAMPLES * 2],
            },
        );
        mixer.push_chunk(
            false,
            PcmChunk {
                timestamp_ns: start,
                start_sample: 0,
                samples: vec![0.8; AAC_BLOCK_SAMPLES * 2],
            },
        );
        mixer.latest_system_ns = Some(start + 500_000_000);
        mixer.latest_microphone_ns = Some(start + 500_000_000);
        let blocks = mixer.take_through(start + 500_000_000);
        assert!(!blocks.is_empty());
        assert!(
            blocks[0]
                .1
                .iter()
                .all(|sample| *sample <= 1.0 && *sample > 0.99)
        );
    }

    #[test]
    fn mixer_preserves_late_source_alignment_and_inserts_silence() {
        let mut mixer = AudioMixer::new();
        let start = 5_000_000_000;
        mixer.push_chunk(
            true,
            PcmChunk {
                timestamp_ns: start,
                start_sample: 0,
                samples: vec![0.25; AAC_BLOCK_SAMPLES * 2],
            },
        );
        mixer.push_chunk(
            false,
            PcmChunk {
                timestamp_ns: start + 10_000_000,
                start_sample: 0,
                samples: vec![0.25; AAC_BLOCK_SAMPLES * 2],
            },
        );
        mixer.latest_system_ns = Some(start + 500_000_000);
        mixer.latest_microphone_ns = Some(start + 500_000_000);
        let blocks = mixer.take_through(start + 500_000_000);
        assert!(!blocks.is_empty());
        assert_eq!(blocks[0].1[0], 0.18);
        assert!(blocks[0].1[960] > blocks[0].1[0]);
    }

    #[test]
    fn mixer_can_progress_when_only_one_audio_source_is_available() {
        let mut mixer = AudioMixer::new();
        mixer.push_chunk(
            false,
            PcmChunk {
                timestamp_ns: 8_000_000_000,
                start_sample: 0,
                samples: vec![0.2; AAC_BLOCK_SAMPLES * 2 * 20],
            },
        );
        let blocks = mixer.take_ready();
        assert!(!blocks.is_empty());
        assert!(blocks[0].1.iter().any(|sample| *sample > 0.0));
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
