use std::{path::Path, time::Duration};

use image::{DynamicImage, ImageBuffer};
use objc2_av_foundation::{AVAsset, AVAssetImageGenerator};
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{CGDataProvider, CGImage};
use objc2_core_media::CMTime;
use objc2_foundation_06::{NSString, NSURL};

const FRAME_INTERVAL: Duration = Duration::from_millis(33);

pub struct MovFrameReader {
    generator: objc2_06::rc::Retained<AVAssetImageGenerator>,
    duration: Duration,
    position: Duration,
}

impl MovFrameReader {
    pub fn open(path: &Path, position: Duration) -> Result<Self, String> {
        let path = path
            .to_str()
            .ok_or_else(|| "MOVファイルのパスを読み取れません。".to_string())?;
        let path = NSString::from_str(path);
        let url = NSURL::fileURLWithPath(&path);
        // SAFETY: The URL refers to a local file and AVAsset is used read-only.
        let asset = unsafe { AVAsset::assetWithURL(&url) };
        // SAFETY: Reading the media duration is a read-only AVFoundation operation.
        let duration_seconds = unsafe { asset.duration().seconds() };
        if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
            return Err("MOVの再生時間を読み取れません。".into());
        }
        let duration = Duration::try_from_secs_f64(duration_seconds)
            .map_err(|_| "MOVの再生時間が範囲外です。".to_string())?;
        // SAFETY: AVFoundation retains the asset for this image generator.
        let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset) };
        // SAFETY: This configures the generator to apply the video's display orientation.
        unsafe { generator.setAppliesPreferredTrackTransform(true) };
        // SAFETY: A finite maximum size keeps decoded preview frames bounded in memory.
        unsafe { generator.setMaximumSize(CGSize::new(1280.0, 720.0)) };
        Ok(Self {
            generator,
            duration,
            position: position.min(duration),
        })
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn next_frame(&mut self) -> Result<Option<(DynamicImage, Duration, Duration)>, String> {
        if self.position >= self.duration {
            return Ok(None);
        }
        let frame_position = self.position;
        // SAFETY: The value is bounded to i64 and the timescale is positive.
        let requested_time = unsafe {
            CMTime::new(
                frame_position.as_millis().min(i64::MAX as u128) as i64,
                1000,
            )
        };
        // SAFETY: The request is bounded by the asset duration and uses a valid output pointer.
        #[allow(deprecated)]
        let cg_image = unsafe {
            self.generator
                .copyCGImageAtTime_actualTime_error(requested_time, std::ptr::null_mut())
        }
        .map_err(|error| format!("MOVの映像フレームを読み取れません: {error}"))?;
        let image = cg_image_to_rgb(&cg_image)?;
        self.position = self.position.saturating_add(FRAME_INTERVAL);
        Ok(Some((
            DynamicImage::ImageRgb8(image),
            FRAME_INTERVAL,
            frame_position,
        )))
    }
}

fn cg_image_to_rgb(image: &CGImage) -> Result<image::RgbImage, String> {
    let width = CGImage::width(Some(image));
    let height = CGImage::height(Some(image));
    let bits_per_component = CGImage::bits_per_component(Some(image));
    let bits_per_pixel = CGImage::bits_per_pixel(Some(image));
    let bytes_per_row = CGImage::bytes_per_row(Some(image));
    let (bytes_per_pixel, channel_offsets) = match (bits_per_component, bits_per_pixel) {
        (16, 64) => (8, [0, 2, 4]),
        (8, 32) => {
            let first_component_is_alpha = matches!(CGImage::alpha_info(Some(image)).0, 2 | 4 | 6);
            let little_endian = CGImage::byte_order_info(Some(image)).0 == 0x2000;
            let offsets = match (little_endian, first_component_is_alpha) {
                (true, true) => [2, 1, 0],
                (true, false) => [3, 2, 1],
                (false, true) => [1, 2, 3],
                (false, false) => [0, 1, 2],
            };
            (4, offsets)
        }
        _ => {
            return Err("このMOVの色形式はアプリ内プレビューに対応していません。".into());
        }
    };
    if width == 0 || height == 0 {
        return Err("このMOVの色形式はアプリ内プレビューに対応していません。".into());
    }
    let minimum_row_bytes = width
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| "MOVフレームのサイズが範囲外です。".to_string())?;
    if bytes_per_row < minimum_row_bytes {
        return Err("MOVフレームの画素データが不完全です。".into());
    }
    let provider = CGImage::data_provider(Some(image))
        .ok_or_else(|| "MOVフレームの画素データを読み取れません。".to_string())?;
    let data = CGDataProvider::data(Some(&provider))
        .ok_or_else(|| "MOVフレームの画素データを読み取れません。".to_string())?;
    let data_len = usize::try_from(data.length())
        .map_err(|_| "MOVフレームの画素データが範囲外です。".to_string())?;
    let required_len = bytes_per_row
        .checked_mul(height)
        .ok_or_else(|| "MOVフレームのサイズが範囲外です。".to_string())?;
    if data_len < required_len {
        return Err("MOVフレームの画素データが不完全です。".into());
    }
    // SAFETY: CFData retains this immutable buffer for the duration of the slice use.
    let source = unsafe { std::slice::from_raw_parts(data.byte_ptr(), data_len) };
    let output_len = width
        .checked_mul(height)
        .ok_or_else(|| "MOVフレームのサイズが範囲外です。".to_string())?;
    let output_bytes = output_len
        .checked_mul(3)
        .ok_or_else(|| "MOVフレームのサイズが範囲外です。".to_string())?;
    let mut output = Vec::with_capacity(output_bytes);
    for y in 0..height {
        let row_start = y * bytes_per_row;
        for x in 0..width {
            let pixel_start = row_start + x * bytes_per_pixel;
            if bits_per_component == 16 {
                let channel = |offset| {
                    u16::from_le_bytes([
                        source[pixel_start + offset],
                        source[pixel_start + offset + 1],
                    ]) >> 8
                };
                output.extend_from_slice(&[
                    channel(channel_offsets[0]) as u8,
                    channel(channel_offsets[1]) as u8,
                    channel(channel_offsets[2]) as u8,
                ]);
            } else {
                output.extend_from_slice(&[
                    source[pixel_start + channel_offsets[0]],
                    source[pixel_start + channel_offsets[1]],
                    source[pixel_start + channel_offsets[2]],
                ]);
            }
        }
    }
    ImageBuffer::from_raw(width as u32, height as u32, output)
        .ok_or_else(|| "MOVのプレビュー画像を作成できません。".into())
}
