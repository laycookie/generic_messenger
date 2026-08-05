use openh264::{
    decoder::Decoder,
    encoder::{
        BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, Level, Profile,
        RateControlMode, UsageType,
    },
    formats::{RgbSliceU8, RgbaSliceU8, YUVBuffer},
};
use std::time::Instant;

use crate::{EncodedVideoFrame, PixelData, VideoCodec, VideoError, VideoFrame};

/// Hard cap for one encoded H264 access unit.
///
/// Bounds encoder output and transport reassembly of untrusted remote frames.
pub const MAX_H264_ACCESS_UNIT_BYTES: usize = 16 * 1024 * 1024;
const MIN_H264_BITRATE_BPS: u32 = 200_000;
const MAX_H264_BITRATE_BPS: u32 = 10_000_000;
const MAX_VIDEO_PIXELS: usize = 4096 * 2160;
const MAX_VIDEO_DIMENSION: usize = 4096;
const MAX_OPENH264_THREADS: usize = 4;
/// Size-limited slicing activates OpenH264's partitioned encoder. Without it,
/// OpenH264 forces single-slice mode and clamps any requested thread count to 1.
/// Keep slice count below DAVE's one-byte supplemental-metadata limit. Each
/// H264 slice adds an unencrypted range; 8 KiB slices can overflow that limit
/// on complex 720p IDRs. RTP already fragments large NALs with FU-A.
const OPENH264_MAX_SLICE_BYTES: u32 = 64 * 1024;
/// Camera and screen encoders may run together. Divide CPU budget between both
/// and leave at least one logical CPU for audio/device work.
const MAX_CONCURRENT_ENCODERS: usize = 2;

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn encoder_thread_count(parallelism: usize) -> u16 {
    let shared_budget = parallelism.saturating_sub(1) / MAX_CONCURRENT_ENCODERS;
    shared_budget.clamp(1, MAX_OPENH264_THREADS) as u16
}

/// Content mode used to tune realtime H264 encoding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum H264EncoderUsage {
    #[default]
    Camera,
    Screen,
}

/// Stateful OpenH264 decoder.
pub struct H264Decoder {
    decoder: Decoder,
    frame_index: u64,
}

impl H264Decoder {
    pub fn new() -> Result<Self, VideoError> {
        Ok(Self {
            decoder: Decoder::new()?,
            frame_index: 0,
        })
    }

    /// Decode one Annex-B access unit to RGBA8.
    ///
    /// Returns `None` while decoder buffers input or waits for a keyframe.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let total_started = timing_enabled.then(Instant::now);
        let codec_started = timing_enabled.then(Instant::now);
        let frame_id = self.frame_index;
        self.frame_index = self.frame_index.wrapping_add(1);
        let Some(yuv) = self.decoder.decode(access_unit)? else {
            if let Some(total_started) = total_started {
                tracing::trace!(
                    target: "video_timing",
                    stage = "h264.decode",
                    frame_id,
                    access_unit_bytes = access_unit.len(),
                    codec_us = codec_started.map(elapsed_micros).unwrap_or_default(),
                    rgba_us = 0_u64,
                    total_us = elapsed_micros(total_started),
                    emitted = false,
                    "video stage complete"
                );
            }
            return Ok(None);
        };
        let codec_us = codec_started.map(elapsed_micros).unwrap_or_default();
        let (width, height) = openh264::formats::YUVSource::dimensions(&yuv);
        let pixels = checked_pixels(width, height)?;
        let data_len = pixels.checked_mul(4).ok_or(VideoError::FrameSizeOverflow)?;
        let rgba_started = timing_enabled.then(Instant::now);
        let mut data = vec![0u8; data_len];
        yuv.write_rgba8(&mut data);
        let frame = VideoFrame::new(width as u32, height as u32, PixelData::Rgba8(data));
        if let Some(total_started) = total_started {
            tracing::trace!(
                target: "video_timing",
                stage = "h264.decode",
                frame_id,
                access_unit_bytes = access_unit.len(),
                width,
                height,
                output_bytes = data_len,
                codec_us,
                rgba_us = rgba_started.map(elapsed_micros).unwrap_or_default(),
                total_us = elapsed_micros(total_started),
                emitted = true,
                "video stage complete"
            );
        }
        Ok(Some(frame))
    }
}

/// Stateful OpenH264 encoder.
pub struct H264Encoder {
    encoder: Encoder,
    yuv: Option<(u32, u32, YUVBuffer)>,
    bitrate_bps: u32,
    frame_index: u64,
}

impl H264Encoder {
    /// Create baseline realtime camera encoder configured for 30 FPS.
    pub fn new() -> Result<Self, VideoError> {
        Self::new_for(H264EncoderUsage::Camera)
    }

    /// Create baseline realtime encoder tuned for camera or screen content.
    pub fn new_for(usage: H264EncoderUsage) -> Result<Self, VideoError> {
        let frames_per_second = match usage {
            H264EncoderUsage::Camera => 30,
            H264EncoderUsage::Screen => 15,
        };
        Self::new_for_frame_rate(usage, frames_per_second)
    }

    /// Create realtime encoder using explicit capture and rate-control cadence.
    pub fn new_for_frame_rate(
        usage: H264EncoderUsage,
        frames_per_second: u32,
    ) -> Result<Self, VideoError> {
        let bitrate_bps = Self::recommended_bitrate_bps(usage, frames_per_second);
        Self::new_for_frame_rate_and_bitrate(usage, frames_per_second, bitrate_bps)
    }

    /// Recommended fixed target for a content mode and cadence.
    #[must_use]
    pub fn recommended_bitrate_bps(usage: H264EncoderUsage, frames_per_second: u32) -> u32 {
        match usage {
            H264EncoderUsage::Camera => 1_200_000,
            H264EncoderUsage::Screen if frames_per_second > 15 => 2_500_000,
            H264EncoderUsage::Screen => 1_500_000,
        }
    }

    /// Create realtime encoder with an explicit congestion-controlled target.
    ///
    /// Rebuilding with this constructor is cheap enough for infrequent network
    /// quality tier changes and avoids relying on unsafe OpenH264 options.
    pub fn new_for_frame_rate_and_bitrate(
        usage: H264EncoderUsage,
        frames_per_second: u32,
        bitrate_bps: u32,
    ) -> Result<Self, VideoError> {
        let frames_per_second = frames_per_second.clamp(1, 60);
        let usage_type = match usage {
            H264EncoderUsage::Camera => UsageType::CameraVideoRealTime,
            H264EncoderUsage::Screen => UsageType::ScreenContentRealTime,
        };
        let bitrate_bps = bitrate_bps.clamp(MIN_H264_BITRATE_BPS, MAX_H264_BITRATE_BPS);
        let frames_per_second = frames_per_second as f32;
        let encoder_threads = std::thread::available_parallelism()
            .map(|parallelism| encoder_thread_count(parallelism.get()))
            .unwrap_or(1);
        let config = EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(frames_per_second))
            .bitrate(BitRate::from_bps(bitrate_bps))
            .num_threads(encoder_threads)
            // Bound transport bitrate. If realtime work falls behind, dropping
            // an encoded frame preserves current playback; sending every frame
            // instead grows Discord's network/jitter queue without bound.
            .skip_frames(true)
            .rate_control_mode(RateControlMode::Bitrate)
            .profile(Profile::Baseline)
            .level(Level::Level_3_1)
            .usage_type(usage_type)
            .complexity(Complexity::Low)
            // Prefer predictable realtime cost over optional quality analysis.
            .adaptive_quantization(false)
            .background_detection(false);
        let config = if encoder_threads > 1 {
            config.max_slice_len(OPENH264_MAX_SLICE_BYTES)
        } else {
            config
        };
        let config = match usage {
            // Transport forces regular keyframes and handles PLI/FIR, so an
            // additional camera scene-change pass is redundant.
            H264EncoderUsage::Camera => config.scene_change_detect(false),
            // OpenH264 requires scene-change detection in screen-content mode.
            H264EncoderUsage::Screen => config,
        };
        Ok(Self {
            encoder: Encoder::with_api_config(openh264::OpenH264API::from_source(), config)?,
            yuv: None,
            bitrate_bps,
            frame_index: 0,
        })
    }

    /// Current encoder rate target, also used by RTP packet pacing.
    #[must_use]
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// Encode one raw frame to an Annex-B H264 access unit.
    ///
    /// Returns `None` when rate control skips the frame.
    pub fn encode(&mut self, frame: &VideoFrame) -> Result<Option<EncodedVideoFrame>, VideoError> {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let total_started = timing_enabled.then(Instant::now);
        let encoder_sequence = self.frame_index;
        self.frame_index = self.frame_index.wrapping_add(1);
        let frame_timing = frame.timing();
        let frame_id = frame_timing.map_or(encoder_sequence, |timing| timing.id());
        let (width, height) = (frame.width as usize, frame.height as usize);
        checked_pixels(width, height)?;
        frame.validate()?;

        let fill_yuv = |buf: &mut YUVBuffer| -> Result<(), VideoError> {
            match &frame.pixel_data {
                PixelData::Rgba8(data) => {
                    buf.read_rgb(RgbaSliceU8::new(data, (width, height)));
                }
                PixelData::SharedRgba8(data) => {
                    buf.read_rgb(RgbaSliceU8::new(data, (width, height)));
                }
                PixelData::Rgb8(data) => {
                    buf.read_rgb8(RgbSliceU8::new(data, (width, height)));
                }
                PixelData::SharedRgb8(data) => {
                    buf.read_rgb8(RgbSliceU8::new(data, (width, height)));
                }
            }
            Ok(())
        };
        let yuv = match &mut self.yuv {
            Some((current_width, current_height, buffer))
                if *current_width == frame.width && *current_height == frame.height =>
            {
                buffer
            }
            slot => {
                *slot = Some((frame.width, frame.height, YUVBuffer::new(width, height)));
                &mut slot.as_mut().unwrap().2
            }
        };
        let yuv_started = timing_enabled.then(Instant::now);
        fill_yuv(yuv)?;
        let rgb_to_yuv_us = yuv_started.map(elapsed_micros).unwrap_or_default();

        let codec_started = timing_enabled.then(Instant::now);
        let encoded = self.encoder.encode(yuv)?;
        let codec_us = codec_started.map(elapsed_micros).unwrap_or_default();
        let keyframe = encoded.frame_type() == FrameType::IDR;
        let copy_started = timing_enabled.then(Instant::now);
        let data = encoded.to_vec();
        let output_copy_us = copy_started.map(elapsed_micros).unwrap_or_default();
        if data.len() > MAX_H264_ACCESS_UNIT_BYTES {
            return Err(VideoError::EncodedFrameTooLarge {
                limit: MAX_H264_ACCESS_UNIT_BYTES,
                actual: data.len(),
            });
        }
        let output_bytes = data.len();
        let emitted = !data.is_empty();
        if let Some(total_started) = total_started {
            tracing::trace!(
                target: "video_timing",
                stage = "h264.encode",
                frame_id,
                encoder_sequence,
                pipeline_elapsed_us = frame_timing.map_or(0, |timing| timing.elapsed_micros()),
                width,
                height,
                input_bytes = frame.pixel_data.as_bytes().len(),
                output_bytes,
                rgb_to_yuv_us,
                codec_us,
                output_copy_us,
                total_us = elapsed_micros(total_started),
                emitted,
                "video stage complete"
            );
        }
        Ok(emitted.then(|| {
            EncodedVideoFrame::with_timing(VideoCodec::H264, keyframe, data, frame_timing)
        }))
    }

    /// Force next encoded frame to be an IDR keyframe.
    pub fn request_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }
}

fn checked_pixels(width: usize, height: usize) -> Result<usize, VideoError> {
    (width <= MAX_VIDEO_DIMENSION && height <= MAX_VIDEO_DIMENSION)
        .then(|| width.checked_mul(height))
        .flatten()
        .filter(|pixels| *pixels > 0 && *pixels <= MAX_VIDEO_PIXELS)
        .ok_or(VideoError::InvalidDimensions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VideoFrameTiming;

    #[test]
    fn encoder_threads_reserve_realtime_capacity() {
        assert_eq!(encoder_thread_count(1), 1);
        assert_eq!(encoder_thread_count(2), 1);
        assert_eq!(encoder_thread_count(4), 1);
        assert_eq!(encoder_thread_count(8), 3);
        assert_eq!(encoder_thread_count(32), 4);
    }

    #[test]
    fn encoder_accepts_rgb8_frame() {
        let mut encoder = H264Encoder::new().unwrap();
        encoder.request_keyframe();
        let (width, height) = (640, 360);
        let timing = VideoFrameTiming::start();
        let frame = VideoFrame::with_timing(
            width,
            height,
            PixelData::Rgb8([0, 0, 0].repeat((width * height) as usize)),
            Some(timing),
        );
        let encoded = encoder.encode(&frame).unwrap().unwrap();
        assert_eq!(encoded.codec, VideoCodec::H264);
        assert!(encoded.keyframe);
        assert!(!encoded.data.is_empty());
        assert_eq!(encoded.timing(), Some(timing));
    }

    #[test]
    fn encoder_rejects_mismatched_frame_size() {
        let mut encoder = H264Encoder::new().unwrap();
        let error = encoder
            .encode(&VideoFrame::new(2, 2, PixelData::Rgba8(vec![0; 15])))
            .unwrap_err();
        assert!(matches!(error, VideoError::FrameSizeMismatch { .. }));
    }

    #[test]
    fn explicit_bitrate_is_retained_and_bounded() {
        let encoder =
            H264Encoder::new_for_frame_rate_and_bitrate(H264EncoderUsage::Screen, 30, 100_000)
                .unwrap();
        assert_eq!(encoder.bitrate_bps(), MIN_H264_BITRATE_BPS);
    }

    #[test]
    fn screen_encoder_sustains_720p_motion() {
        let frames_per_second = 30;
        let mut encoder =
            H264Encoder::new_for_frame_rate(H264EncoderUsage::Screen, frames_per_second).unwrap();
        encoder.request_keyframe();
        let (width, height) = (1280, 720);
        let mut emitted = 0;
        for frame_index in 0..frames_per_second {
            let mut pixels = vec![24; (width * height * 3) as usize];
            let left = frame_index * 16;
            for y in 200..440 {
                for x in left..left + 160 {
                    let pixel = ((y * width + x) * 3) as usize;
                    pixels[pixel..pixel + 3].copy_from_slice(&[0, 255, 255]);
                }
            }
            emitted += encoder
                .encode(&VideoFrame::new(width, height, PixelData::Rgb8(pixels)))
                .unwrap()
                .is_some() as u32;
        }

        assert_eq!(
            emitted, frames_per_second,
            "screen encoder skipped simple motion"
        );
    }

    #[test]
    fn complex_recovery_idr_keeps_dave_range_count_bounded() {
        let parallelism = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        if encoder_thread_count(parallelism) == 1 {
            // Single-slice OpenH264 cannot exercise partitioned output.
            return;
        }

        let (width, height) = (1280, 720);
        let mut encoder = H264Encoder::new_for_frame_rate(H264EncoderUsage::Screen, 30).unwrap();
        let initial = VideoFrame::new(
            width,
            height,
            PixelData::Rgb8(vec![0; (width * height * 3) as usize]),
        );
        encoder.encode(&initial).unwrap();

        // Deterministic high-entropy screen content maximizes IDR size and
        // therefore OpenH264's size-limited slice count.
        let mut random = 0x9E37_79B9_u32;
        let mut pixels = vec![0; (width * height * 3) as usize];
        for byte in &mut pixels {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            *byte = random as u8;
        }
        let frame = VideoFrame::new(width, height, PixelData::Rgb8(pixels));
        encoder.request_keyframe();
        let recovery = (0..8)
            .find_map(|_| encoder.encode(&frame).unwrap())
            .expect("forced recovery keyframe must be emitted");
        assert!(recovery.keyframe);

        let idr_slices = openh264::nal_units(&recovery.data)
            .filter_map(|nal| {
                nal.windows(3)
                    .position(|window| window == [0, 0, 1])
                    .and_then(|start| nal.get(start + 3))
                    .map(|header| header & 0x1F)
            })
            .filter(|nal_type| *nal_type == 5)
            .count();
        assert!(
            (1..=32).contains(&idr_slices),
            "{idr_slices} IDR slices fall outside DAVE-safe range"
        );
    }

    #[test]
    fn camera_encoder_emits_smooth_frame_sequence() {
        let mut encoder = H264Encoder::new_for_frame_rate(H264EncoderUsage::Camera, 30).unwrap();
        encoder.request_keyframe();
        let (width, height) = (640, 480);

        for value in 0..30 {
            let encoded = encoder
                .encode(&VideoFrame::new(
                    width,
                    height,
                    PixelData::Rgb8(
                        [value * 7, 255 - value * 7, value * 3].repeat((width * height) as usize),
                    ),
                ))
                .unwrap();
            assert!(encoded.is_some(), "encoder skipped frame {value}");
        }
    }
}
