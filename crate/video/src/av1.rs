use rav1e::prelude::{
    ChromaSampling, ColorDescription, ColorPrimaries, Config, Context, EncoderConfig,
    EncoderStatus, Frame, FrameParameters, FrameType, FrameTypeOverride, MatrixCoefficients,
    Opaque, PixelRange, Rational, SceneDetectionSpeed, TransferCharacteristics, Tune,
};
use std::time::Instant;

use crate::{EncodedVideoFrame, PixelData, VideoCodec, VideoError, VideoFrame, VideoFrameTiming};

mod convert;

use convert::convert_simd;
#[cfg(test)]
use convert::rgb_to_limited_bt709;

/// Hard cap for one encoded AV1 temporal unit.
///
/// Bounds encoder output and transport reassembly of untrusted remote frames.
pub const MAX_AV1_TEMPORAL_UNIT_BYTES: usize = 16 * 1024 * 1024;
const MIN_AV1_BITRATE_BPS: u32 = 150_000;
const MAX_AV1_BITRATE_BPS: u32 = 10_000_000;
const MAX_VIDEO_PIXELS: usize = 4096 * 2160;
const MAX_VIDEO_DIMENSION: usize = 4096;
const MAX_RAV1E_THREADS: usize = 16;
/// Camera and screen encoders may run together. Divide CPU budget between both
/// and leave at least one logical CPU for audio/device work.
const MAX_CONCURRENT_ENCODERS: usize = 2;

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn encoder_thread_count(parallelism: usize) -> usize {
    let shared_budget = parallelism.saturating_sub(1) / MAX_CONCURRENT_ENCODERS;
    shared_budget.clamp(1, MAX_RAV1E_THREADS)
}

/// Content mode used to tune realtime AV1 encoding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum Av1EncoderUsage {
    #[default]
    Camera,
    Screen,
}

struct Av1EncoderState {
    width: u32,
    height: u32,
    context: Context<u8>,
}

/// Stateful low-latency rav1e encoder.
pub struct Av1Encoder {
    usage: Av1EncoderUsage,
    frames_per_second: u32,
    bitrate_bps: u32,
    state: Option<Av1EncoderState>,
    force_keyframe: bool,
    frame_index: u64,
}

impl Av1Encoder {
    /// Create realtime camera encoder configured for 30 FPS.
    pub fn new() -> Result<Self, VideoError> {
        Self::new_for(Av1EncoderUsage::Camera)
    }

    /// Create realtime encoder tuned for camera or screen content.
    pub fn new_for(usage: Av1EncoderUsage) -> Result<Self, VideoError> {
        let frames_per_second = match usage {
            Av1EncoderUsage::Camera => 30,
            Av1EncoderUsage::Screen => 15,
        };
        Self::new_for_frame_rate(usage, frames_per_second)
    }

    /// Create realtime encoder using explicit capture and rate-control cadence.
    pub fn new_for_frame_rate(
        usage: Av1EncoderUsage,
        frames_per_second: u32,
    ) -> Result<Self, VideoError> {
        let bitrate_bps = Self::recommended_bitrate_bps(usage, frames_per_second);
        Self::new_for_frame_rate_and_bitrate(usage, frames_per_second, bitrate_bps)
    }

    /// Recommended fixed target for a content mode and cadence.
    #[must_use]
    pub fn recommended_bitrate_bps(usage: Av1EncoderUsage, frames_per_second: u32) -> u32 {
        match usage {
            Av1EncoderUsage::Camera => 900_000,
            Av1EncoderUsage::Screen if frames_per_second > 15 => 2_000_000,
            Av1EncoderUsage::Screen => 1_200_000,
        }
    }

    /// Create realtime encoder with an explicit congestion-controlled target.
    pub fn new_for_frame_rate_and_bitrate(
        usage: Av1EncoderUsage,
        frames_per_second: u32,
        bitrate_bps: u32,
    ) -> Result<Self, VideoError> {
        Ok(Self {
            usage,
            frames_per_second: frames_per_second.clamp(1, 60),
            bitrate_bps: bitrate_bps.clamp(MIN_AV1_BITRATE_BPS, MAX_AV1_BITRATE_BPS),
            state: None,
            force_keyframe: false,
            frame_index: 0,
        })
    }

    /// Current encoder rate target, also suitable for RTP packet pacing.
    #[must_use]
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// Encode one raw frame to an AV1 low-overhead temporal unit.
    ///
    /// Returns `None` while rav1e gathers enough lookahead to emit a packet.
    pub fn encode(&mut self, frame: &VideoFrame) -> Result<Option<EncodedVideoFrame>, VideoError> {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let total_started = timing_enabled.then(Instant::now);
        let encoder_sequence = self.frame_index;
        self.frame_index = self.frame_index.wrapping_add(1);
        let submitted_timing = frame.timing();
        let (width, height) = (frame.width as usize, frame.height as usize);
        checked_pixels(width, height)?;
        frame.validate()?;

        let force_keyframe = self.force_keyframe;
        let yuv_started = timing_enabled.then(Instant::now);
        let mut input = self
            .ensure_state(frame.width, frame.height)?
            .context
            .new_frame();
        fill_yuv420(frame, &mut input);
        let rgb_to_yuv_us = yuv_started.map(elapsed_micros).unwrap_or_default();
        let parameters = FrameParameters {
            frame_type_override: if force_keyframe {
                FrameTypeOverride::Key
            } else {
                FrameTypeOverride::No
            },
            opaque: submitted_timing.map(Opaque::new),
            ..FrameParameters::default()
        };

        let codec_started = timing_enabled.then(Instant::now);
        let receive_result = {
            let state = self
                .state
                .as_mut()
                .expect("AV1 encoder state must exist after ensure_state");
            state.context.send_frame((input, parameters))?;
            loop {
                match state.context.receive_packet() {
                    Err(EncoderStatus::Encoded) => continue,
                    result => break result,
                }
            }
        };
        self.force_keyframe = false;
        let codec_us = codec_started.map(elapsed_micros).unwrap_or_default();

        let mut packet = match receive_result {
            Ok(packet) => Some(packet),
            Err(EncoderStatus::NeedMoreData) => None,
            Err(error) => return Err(error.into()),
        };
        let emitted_timing = packet
            .as_mut()
            .and_then(|packet| packet.opaque.take())
            .and_then(|opaque| opaque.downcast::<VideoFrameTiming>().ok())
            .map(|timing| *timing);
        let frame_id = submitted_timing.map_or(encoder_sequence, |timing| timing.id());
        let output_bytes = packet.as_ref().map_or(0, |packet| packet.data.len());
        if output_bytes > MAX_AV1_TEMPORAL_UNIT_BYTES {
            return Err(VideoError::EncodedFrameTooLarge {
                limit: MAX_AV1_TEMPORAL_UNIT_BYTES,
                actual: output_bytes,
            });
        }
        let emitted = packet.is_some();
        if let Some(total_started) = total_started {
            tracing::trace!(
                target: "video_timing",
                stage = "av1.encode",
                frame_id,
                emitted_frame_id = emitted_timing.map_or(0, |timing| timing.id()),
                encoder_sequence,
                pipeline_elapsed_us = submitted_timing.map_or(0, |timing| timing.elapsed_micros()),
                width,
                height,
                input_bytes = frame.pixel_data.as_bytes().len(),
                output_bytes,
                rgb_to_yuv_us,
                codec_us,
                total_us = elapsed_micros(total_started),
                emitted,
                "video stage complete"
            );
        }
        Ok(packet.map(|packet| {
            EncodedVideoFrame::with_timing(
                VideoCodec::Av1,
                packet.frame_type == FrameType::KEY,
                packet.data,
                emitted_timing,
            )
        }))
    }

    /// Force next submitted frame to be an AV1 keyframe.
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn ensure_state(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<&mut Av1EncoderState, VideoError> {
        let needs_rebuild = self
            .state
            .as_ref()
            .is_none_or(|state| state.width != width || state.height != height);
        if needs_rebuild {
            let state = create_encoder_state(
                self.usage,
                self.frames_per_second,
                self.bitrate_bps,
                width,
                height,
            )?;
            tracing::debug!(
                codec = "av1",
                width,
                height,
                frames_per_second = self.frames_per_second,
                bitrate_bps = self.bitrate_bps,
                "video encoder configured"
            );
            self.state = Some(state);
        }
        Ok(self
            .state
            .as_mut()
            .expect("AV1 encoder state must exist after successful construction"))
    }
}

fn create_encoder_state(
    usage: Av1EncoderUsage,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
) -> Result<Av1EncoderState, VideoError> {
    let encoder_threads = std::thread::available_parallelism()
        .map(|parallelism| encoder_thread_count(parallelism.get()))
        .unwrap_or(1);
    let mut encoder_config = EncoderConfig::with_speed_preset(10);
    encoder_config.width = width as usize;
    encoder_config.height = height as usize;
    encoder_config.time_base = Rational::new(1, u64::from(frames_per_second));
    encoder_config.bit_depth = 8;
    encoder_config.chroma_sampling = ChromaSampling::Cs420;
    encoder_config.pixel_range = PixelRange::Limited;
    encoder_config.color_description = Some(ColorDescription {
        color_primaries: ColorPrimaries::BT709,
        transfer_characteristics: TransferCharacteristics::BT709,
        matrix_coefficients: MatrixCoefficients::BT709,
    });
    encoder_config.low_latency = true;
    encoder_config.error_resilient = true;
    // Psychovisual tune disables rav1e's fast transform-domain distortion
    // path. PSNR tune is substantially cheaper and fits realtime rate control.
    encoder_config.tune = Tune::Psnr;
    encoder_config.bitrate = i32::try_from(bitrate_bps).unwrap_or(i32::MAX);
    encoder_config.min_key_frame_interval = u64::from(frames_per_second / 2).max(1);
    encoder_config.max_key_frame_interval =
        u64::from(frames_per_second).saturating_mul(match usage {
            Av1EncoderUsage::Camera => 2,
            Av1EncoderUsage::Screen => 4,
        });
    // rav1e gets within-frame parallelism from tiles. A thread pool alone
    // leaves its dominant encode stage on one worker and can turn 720p screen
    // frames into second-long stalls.
    encoder_config.tiles = encoder_threads;
    // Minimize temporal RDO lookahead. Low-latency mode already disables frame
    // reordering; rav1e requires at least one analysis frame.
    encoder_config.speed_settings.rdo_lookahead_frames = 1;
    // Coordinator owns periodic/PLI keyframes. Scene detection only adds
    // analysis latency and duplicate intra frames on this realtime path.
    encoder_config.speed_settings.scene_detection_mode = SceneDetectionSpeed::None;
    // Speed preset 10 leaves both enabled. Their cost breaks realtime pacing
    // on high-motion screen content; rate control absorbs the quality tradeoff.
    encoder_config.speed_settings.cdef = false;
    encoder_config
        .speed_settings
        .prediction
        .fine_directional_intra = false;

    let context = Config::new()
        .with_encoder_config(encoder_config)
        .with_threads(encoder_threads)
        .new_context::<u8>()?;
    Ok(Av1EncoderState {
        width,
        height,
        context,
    })
}

fn checked_pixels(width: usize, height: usize) -> Result<usize, VideoError> {
    (width >= 16 && height >= 16 && width <= MAX_VIDEO_DIMENSION && height <= MAX_VIDEO_DIMENSION)
        .then(|| width.checked_mul(height))
        .flatten()
        .filter(|pixels| *pixels <= MAX_VIDEO_PIXELS)
        .ok_or(VideoError::InvalidDimensions)
}

fn fill_yuv420(source: &VideoFrame, target: &mut Frame<u8>) {
    let width = source.width as usize;
    let height = source.height as usize;
    let (pixels, bytes_per_pixel) = match &source.pixel_data {
        PixelData::Rgba8(data) => (data.as_slice(), 4),
        PixelData::Rgb8(data) => (data.as_slice(), 3),
        PixelData::SharedRgba8(data) => (data.as_ref(), 4),
        PixelData::SharedRgb8(data) => (data.as_ref(), 3),
    };

    let [y_plane, u_plane, v_plane] = &mut target.planes;
    let y_stride = y_plane.cfg.stride;
    let u_stride = u_plane.cfg.stride;
    let v_stride = v_plane.cfg.stride;
    convert_simd(
        pixels,
        width,
        height,
        bytes_per_pixel,
        y_plane.data_origin_mut(),
        y_stride,
        u_plane.data_origin_mut(),
        u_stride,
        v_plane.data_origin_mut(),
        v_stride,
    );

    for plane in &mut target.planes {
        plane.pad(width, height);
    }
}

#[cfg(feature = "bench-internals")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_contiguous_scalar(
    source: &[u8],
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let chroma_width = width.div_ceil(2);
    convert::convert_scalar(
        source,
        width,
        height,
        bytes_per_pixel,
        y,
        width,
        u,
        chroma_width,
        v,
        chroma_width,
    );
}

#[cfg(feature = "bench-internals")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_contiguous_simd(
    source: &[u8],
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
) {
    let chroma_width = width.div_ceil(2);
    convert::convert_simd(
        source,
        width,
        height,
        bytes_per_pixel,
        y,
        width,
        u,
        chroma_width,
        v,
        chroma_width,
    );
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn encode_until_packet(encoder: &mut Av1Encoder, frame: &VideoFrame) -> EncodedVideoFrame {
        for _ in 0..8 {
            if let Some(encoded) = encoder.encode(frame).unwrap() {
                return encoded;
            }
        }
        panic!("low-latency encoder did not emit within eight frames");
    }

    #[test]
    fn limited_bt709_maps_black_and_white() {
        assert_eq!(rgb_to_limited_bt709(0, 0, 0), (16, 128, 128));
        assert_eq!(rgb_to_limited_bt709(255, 255, 255), (235, 128, 128));
    }

    #[test]
    fn encoder_threads_reserve_realtime_capacity() {
        assert_eq!(encoder_thread_count(1), 1);
        assert_eq!(encoder_thread_count(2), 1);
        assert_eq!(encoder_thread_count(4), 1);
        assert_eq!(encoder_thread_count(8), 3);
        assert_eq!(encoder_thread_count(32), 15);
        assert_eq!(encoder_thread_count(64), MAX_RAV1E_THREADS);
    }

    #[test]
    fn encoder_accepts_rgb8_frame() {
        let mut encoder = Av1Encoder::new().unwrap();
        let frame = VideoFrame::new(16, 16, PixelData::Rgb8(vec![32; 16 * 16 * 3]));

        let encoded = encode_until_packet(&mut encoder, &frame);

        assert_eq!(encoded.codec, VideoCodec::Av1);
        assert!(encoded.keyframe);
        assert!(!encoded.data.is_empty());
    }

    #[test]
    fn encoder_rejects_mismatched_frame_size() {
        let mut encoder = Av1Encoder::new().unwrap();
        let error = encoder
            .encode(&VideoFrame::new(
                16,
                16,
                PixelData::Rgba8(vec![0; 16 * 16 * 4 - 1]),
            ))
            .unwrap_err();
        assert!(matches!(error, VideoError::FrameSizeMismatch { .. }));
    }

    #[test]
    fn encoder_rejects_dimensions_outside_rav1e_bounds() {
        let mut encoder = Av1Encoder::new().unwrap();
        let error = encoder
            .encode(&VideoFrame::new(8, 8, PixelData::Rgb8(vec![0; 8 * 8 * 3])))
            .unwrap_err();
        assert!(matches!(error, VideoError::InvalidDimensions));
    }

    #[test]
    fn explicit_bitrate_is_retained_and_bounded() {
        let encoder =
            Av1Encoder::new_for_frame_rate_and_bitrate(Av1EncoderUsage::Screen, 30, 100_000)
                .unwrap();
        assert_eq!(encoder.bitrate_bps(), MIN_AV1_BITRATE_BPS);
    }

    #[test]
    fn requested_keyframe_is_reported() {
        let mut encoder = Av1Encoder::new().unwrap();
        let frame = VideoFrame::new(16, 16, PixelData::Rgba8(vec![96; 16 * 16 * 4]));
        let _ = encode_until_packet(&mut encoder, &frame);
        encoder.request_keyframe();

        let mut keyframe = false;
        for _ in 0..8 {
            keyframe |= encoder
                .encode(&frame)
                .unwrap()
                .is_some_and(|encoded| encoded.keyframe);
            if keyframe {
                break;
            }
        }

        assert!(keyframe, "requested keyframe was not emitted");
    }

    #[test]
    fn delayed_packets_keep_submitted_frame_timing() {
        let mut encoder = Av1Encoder::new().unwrap();
        let mut pending = VecDeque::new();
        let mut emitted = 0;

        for value in 0..8 {
            let timing = VideoFrameTiming::start();
            pending.push_back(timing);
            let frame = VideoFrame::with_timing(
                16,
                16,
                PixelData::Rgb8(vec![value; 16 * 16 * 3]),
                Some(timing),
            );
            if let Some(encoded) = encoder.encode(&frame).unwrap() {
                assert_eq!(encoded.timing(), pending.pop_front());
                emitted += 1;
            }
        }

        assert!(emitted > 0);
    }
}
