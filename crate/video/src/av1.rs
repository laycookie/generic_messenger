use rav1e::prelude::{
    ChromaSampling, ColorDescription, ColorPrimaries, Config, Context, EncoderConfig,
    EncoderStatus, Frame, FrameParameters, FrameType, FrameTypeOverride, MatrixCoefficients,
    Opaque, Packet, PixelRange, Rational, SceneDetectionSpeed, TransferCharacteristics, Tune,
};
use std::{collections::VecDeque, time::Instant};

use crate::{
    EncodedVideoFrame, PixelData, VideoCodec, VideoError, VideoFrame, VideoFrameTiming,
    geometry::checked_codec_pixels, timing::elapsed_micros,
};

mod convert;

use convert::convert_simd;
#[cfg(test)]
use convert::rgb_to_limited_bt709;

/// Hard cap for one encoded AV1 temporal unit.
///
/// Bounds encoder output and transport reassembly of untrusted remote frames.
pub const MAX_AV1_TEMPORAL_UNIT_BYTES: usize = 16 * 1024 * 1024;
/// Lowest accepted realtime AV1 target bitrate.
pub const MIN_AV1_BITRATE_BPS: u32 = 150_000;
/// Highest accepted realtime AV1 target bitrate.
pub const MAX_AV1_BITRATE_BPS: u32 = 10_000_000;
/// Lowest accepted realtime AV1 frame rate.
pub const MIN_AV1_FRAMES_PER_SECOND: u32 = 1;
/// Highest accepted realtime AV1 frame rate.
pub const MAX_AV1_FRAMES_PER_SECOND: u32 = 60;
const MAX_RAV1E_THREADS: usize = 16;
/// Camera and screen encoders may run together. Divide CPU budget between both
/// and leave at least one logical CPU for audio/device work.
const MAX_CONCURRENT_ENCODERS: usize = 2;

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
    generation: u64,
    context: Context<u8>,
}

#[derive(Clone, Copy, Debug)]
struct Av1FrameMetadata {
    timing: Option<VideoFrameTiming>,
    submitted_sequence: u64,
    submitted_at: Instant,
    forced_keyframe: bool,
    context_generation: u64,
    context_rebuilt: bool,
    context_rebuild_us: u64,
}

struct Av1PendingPacket {
    frame: EncodedVideoFrame,
    metadata: Option<Av1FrameMetadata>,
    qp: u8,
    input_frameno: u64,
}

impl Av1PendingPacket {
    fn from_packet(mut packet: Packet<u8>) -> Result<Self, VideoError> {
        if packet.data.len() > MAX_AV1_TEMPORAL_UNIT_BYTES {
            return Err(VideoError::EncodedFrameTooLarge {
                limit: MAX_AV1_TEMPORAL_UNIT_BYTES,
                actual: packet.data.len(),
            });
        }
        let metadata = packet
            .opaque
            .take()
            .and_then(|opaque| opaque.downcast::<Av1FrameMetadata>().ok())
            .map(|metadata| *metadata);
        let timing = metadata.and_then(|metadata| metadata.timing);
        Ok(Self {
            frame: EncodedVideoFrame::with_timing(
                VideoCodec::Av1,
                packet.frame_type == FrameType::KEY,
                packet.data,
                timing,
            ),
            metadata,
            qp: packet.qp,
            input_frameno: packet.input_frameno,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Av1StateSetup {
    generation: u64,
    rebuilt: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Av1EncodeObservation {
    submitted_sequence: u64,
    context_generation: u64,
    context_rebuilt: bool,
    context_rebuild_us: u64,
    need_more_data_streak: u64,
    emitted_sequence: Option<u64>,
    emitted_context_generation: Option<u64>,
    emitted_context_rebuilt: bool,
    emitted_context_rebuild_us: u64,
    emitted_forced_keyframe: bool,
    lag_frames: Option<u64>,
    encoder_residence_us: Option<u64>,
}

/// Stateful low-latency rav1e encoder.
pub struct Av1Encoder {
    usage: Av1EncoderUsage,
    frames_per_second: u32,
    bitrate_bps: u32,
    state: Option<Av1EncoderState>,
    pending_packets: VecDeque<Av1PendingPacket>,
    force_keyframe: bool,
    frame_index: u64,
    need_more_data_streak: u64,
    #[cfg(test)]
    last_observation: Option<Av1EncodeObservation>,
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
        if !(MIN_AV1_FRAMES_PER_SECOND..=MAX_AV1_FRAMES_PER_SECOND).contains(&frames_per_second) {
            return Err(VideoError::InvalidFrameRate {
                minimum: MIN_AV1_FRAMES_PER_SECOND,
                maximum: MAX_AV1_FRAMES_PER_SECOND,
                actual: frames_per_second,
            });
        }
        if !(MIN_AV1_BITRATE_BPS..=MAX_AV1_BITRATE_BPS).contains(&bitrate_bps) {
            return Err(VideoError::InvalidBitrate {
                minimum: MIN_AV1_BITRATE_BPS,
                maximum: MAX_AV1_BITRATE_BPS,
                actual: bitrate_bps,
            });
        }
        Ok(Self {
            usage,
            frames_per_second,
            bitrate_bps,
            state: None,
            pending_packets: VecDeque::new(),
            force_keyframe: false,
            frame_index: 0,
            need_more_data_streak: 0,
            #[cfg(test)]
            last_observation: None,
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
        let metadata_enabled = timing_enabled || submitted_timing.is_some();
        let (width, height) = (frame.width as usize, frame.height as usize);
        checked_pixels(width, height)?;
        frame.validate()?;

        let force_keyframe = self.force_keyframe;
        let state_setup_started = metadata_enabled.then(Instant::now);
        let state_setup = self.ensure_state(frame.width, frame.height)?;
        let state_setup_us = state_setup_started.map(elapsed_micros).unwrap_or_default();
        let context_rebuild_us = if state_setup.rebuilt {
            state_setup_us
        } else {
            0
        };
        let frame_alloc_started = timing_enabled.then(Instant::now);
        let mut input = self.state_mut()?.context.new_frame();
        let frame_alloc_us = frame_alloc_started.map(elapsed_micros).unwrap_or_default();
        let yuv_started = timing_enabled.then(Instant::now);
        fill_yuv420(frame, &mut input);
        let rgb_to_yuv_us = yuv_started.map(elapsed_micros).unwrap_or_default();
        let parameters = FrameParameters {
            frame_type_override: if force_keyframe {
                FrameTypeOverride::Key
            } else {
                FrameTypeOverride::No
            },
            opaque: metadata_enabled.then(|| {
                Opaque::new(Av1FrameMetadata {
                    timing: submitted_timing,
                    submitted_sequence: encoder_sequence,
                    submitted_at: Instant::now(),
                    forced_keyframe: force_keyframe,
                    context_generation: state_setup.generation,
                    context_rebuilt: state_setup.rebuilt,
                    context_rebuild_us,
                })
            }),
            ..FrameParameters::default()
        };

        let (send_frame_us, receive_packet_us, encoded_loop_count, receive_result) = {
            let state = self.state_mut()?;
            let send_started = timing_enabled.then(Instant::now);
            state.context.send_frame((input, parameters))?;
            let send_frame_us = send_started.map(elapsed_micros).unwrap_or_default();
            let receive_started = timing_enabled.then(Instant::now);
            let mut encoded_loop_count = 0_u64;
            let receive_result = loop {
                match state.context.receive_packet() {
                    Err(EncoderStatus::Encoded) => {
                        encoded_loop_count = encoded_loop_count.saturating_add(1);
                    }
                    result => break result,
                }
            };
            (
                send_frame_us,
                receive_started.map(elapsed_micros).unwrap_or_default(),
                encoded_loop_count,
                receive_result,
            )
        };
        self.force_keyframe = false;
        let codec_us = send_frame_us.saturating_add(receive_packet_us);

        let receive_status = match receive_result {
            Ok(packet) => {
                self.need_more_data_streak = 0;
                self.pending_packets
                    .push_back(Av1PendingPacket::from_packet(packet)?);
                "packet"
            }
            Err(EncoderStatus::NeedMoreData) => {
                self.need_more_data_streak = self.need_more_data_streak.saturating_add(1);
                "need_more_data"
            }
            Err(error) => return Err(error.into()),
        };
        let emitted_packet = self.pending_packets.pop_front();
        let emitted_metadata = emitted_packet.as_ref().and_then(|pending| pending.metadata);
        let emitted_timing = emitted_packet
            .as_ref()
            .and_then(|pending| pending.frame.timing());
        let frame_id = submitted_timing.map_or(encoder_sequence, |timing| timing.id());
        let output_bytes = emitted_packet
            .as_ref()
            .map_or(0, |pending| pending.frame.data.len());
        let emitted = emitted_packet.is_some();
        let emitted_sequence = emitted_metadata.map(|metadata| metadata.submitted_sequence);
        let lag_frames = emitted_sequence.map(|sequence| encoder_sequence.wrapping_sub(sequence));
        let encoder_residence_us =
            emitted_metadata.map(|metadata| elapsed_micros(metadata.submitted_at));
        let emitted_context_generation =
            emitted_metadata.map(|metadata| metadata.context_generation);
        let emitted_context_rebuilt =
            emitted_metadata.is_some_and(|metadata| metadata.context_rebuilt);
        let emitted_context_rebuild_us =
            emitted_metadata.map_or(0, |metadata| metadata.context_rebuild_us);
        let emitted_forced_keyframe =
            emitted_metadata.is_some_and(|metadata| metadata.forced_keyframe);
        let emitted_keyframe = emitted_packet
            .as_ref()
            .is_some_and(|pending| pending.frame.keyframe);
        let emitted_qp = emitted_packet
            .as_ref()
            .map_or(0, |pending| u64::from(pending.qp));
        let emitted_input_frameno = emitted_packet
            .as_ref()
            .map_or(0, |pending| pending.input_frameno);
        #[cfg(test)]
        {
            self.last_observation = Some(Av1EncodeObservation {
                submitted_sequence: encoder_sequence,
                context_generation: state_setup.generation,
                context_rebuilt: state_setup.rebuilt,
                context_rebuild_us,
                need_more_data_streak: self.need_more_data_streak,
                emitted_sequence,
                emitted_context_generation,
                emitted_context_rebuilt,
                emitted_context_rebuild_us,
                emitted_forced_keyframe,
                lag_frames,
                encoder_residence_us,
            });
        }
        if let Some(total_started) = total_started {
            tracing::trace!(
                target: "video_timing",
                stage = "av1.encode",
                frame_id,
                emitted_frame_id = emitted_timing.map_or(0, |timing| timing.id()),
                encoder_sequence,
                submitted_sequence = encoder_sequence,
                emitted_sequence = emitted_sequence.unwrap_or_default(),
                emitted_sequence_known = emitted_sequence.is_some(),
                lag_frames = lag_frames.unwrap_or_default(),
                encoder_residence_us = encoder_residence_us.unwrap_or_default(),
                pipeline_elapsed_us = submitted_timing.map_or(0, |timing| timing.elapsed_micros()),
                width,
                height,
                input_bytes = frame.pixel_data.as_bytes().len(),
                output_bytes,
                context_generation = state_setup.generation,
                context_rebuilt = state_setup.rebuilt,
                context_rebuild_us,
                emitted_context_generation = emitted_context_generation.unwrap_or_default(),
                emitted_context_rebuilt,
                emitted_context_rebuild_us,
                submitted_forced_keyframe = force_keyframe,
                emitted_forced_keyframe,
                emitted_keyframe,
                emitted_qp,
                emitted_input_frameno,
                receive_status,
                need_more_data_streak = self.need_more_data_streak,
                encoded_loop_count,
                state_setup_us,
                frame_alloc_us,
                rgb_to_yuv_us,
                send_frame_us,
                receive_packet_us,
                codec_us,
                total_us = elapsed_micros(total_started),
                emitted,
                "video stage complete"
            );
        }
        Ok(emitted_packet.map(|pending| pending.frame))
    }

    /// Flush delayed temporal units and reset the rav1e context.
    ///
    /// Call this when an input stream ends. Every submitted frame which was not
    /// already returned by [`Self::encode`] is returned in presentation order.
    /// The encoder can be reused afterward; the next frame starts a new context.
    pub fn finish(&mut self) -> Result<Vec<EncodedVideoFrame>, VideoError> {
        if let Some(state) = self.state.as_mut() {
            drain_context(&mut state.context, &mut self.pending_packets)?;
        }
        // Retain a partially drained context when an error is returned above so
        // callers can retry without silently losing delayed frames.
        self.state = None;
        self.force_keyframe = false;
        self.need_more_data_streak = 0;
        Ok(self
            .pending_packets
            .drain(..)
            .map(|pending| pending.frame)
            .collect())
    }

    /// Force next submitted frame to be an AV1 keyframe.
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn state_mut(&mut self) -> Result<&mut Av1EncoderState, VideoError> {
        self.state
            .as_mut()
            .ok_or(VideoError::Av1Encoder(EncoderStatus::Failure))
    }

    fn ensure_state(&mut self, width: u32, height: u32) -> Result<Av1StateSetup, VideoError> {
        let needs_rebuild = self
            .state
            .as_ref()
            .is_none_or(|state| state.width != width || state.height != height);
        if needs_rebuild {
            let generation = self
                .state
                .as_ref()
                .map_or(1, |state| state.generation.saturating_add(1));
            let state = create_encoder_state(
                self.usage,
                self.frames_per_second,
                self.bitrate_bps,
                width,
                height,
                generation,
            )?;
            if let Some(previous) = self.state.as_mut() {
                // Drain in place: on failure, keep the old context and every
                // packet already recovered so a caller can retry the rebuild.
                drain_context(&mut previous.context, &mut self.pending_packets)?;
            }
            tracing::debug!(
                codec = "av1",
                width,
                height,
                frames_per_second = self.frames_per_second,
                bitrate_bps = self.bitrate_bps,
                context_generation = generation,
                "video encoder configured"
            );
            self.state = Some(state);
            self.need_more_data_streak = 0;
        }
        let generation = self.state_mut()?.generation;
        Ok(Av1StateSetup {
            generation,
            rebuilt: needs_rebuild,
        })
    }
}

fn drain_context(
    context: &mut Context<u8>,
    packets: &mut VecDeque<Av1PendingPacket>,
) -> Result<(), VideoError> {
    // Use fallible send_frame instead of Context::flush, whose convenience
    // implementation unwraps internally. Repeated flush requests are safe, so
    // a caller can retry after any transient drain error.
    context.send_frame(None)?;
    loop {
        match context.receive_packet() {
            Ok(packet) => packets.push_back(Av1PendingPacket::from_packet(packet)?),
            Err(EncoderStatus::Encoded | EncoderStatus::NeedMoreData) => continue,
            Err(EncoderStatus::LimitReached | EncoderStatus::EnoughData) => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn create_encoder_state(
    usage: Av1EncoderUsage,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
    generation: u64,
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
    encoder_config.bitrate = bitrate_bps as i32;
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
        generation,
        context,
    })
}

fn checked_pixels(width: usize, height: usize) -> Result<usize, VideoError> {
    if width < 16 || height < 16 {
        return Err(VideoError::InvalidDimensions);
    }
    checked_codec_pixels(width, height)
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
    fn explicit_rate_settings_are_validated_instead_of_silently_clamped() {
        assert!(matches!(
            Av1Encoder::new_for_frame_rate_and_bitrate(
                Av1EncoderUsage::Screen,
                MIN_AV1_FRAMES_PER_SECOND - 1,
                MIN_AV1_BITRATE_BPS,
            ),
            Err(VideoError::InvalidFrameRate { .. })
        ));
        assert!(matches!(
            Av1Encoder::new_for_frame_rate_and_bitrate(
                Av1EncoderUsage::Screen,
                30,
                MIN_AV1_BITRATE_BPS - 1,
            ),
            Err(VideoError::InvalidBitrate { .. })
        ));
        let encoder = Av1Encoder::new_for_frame_rate_and_bitrate(
            Av1EncoderUsage::Screen,
            30,
            MIN_AV1_BITRATE_BPS,
        )
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
    fn delayed_packets_keep_submitted_timing_and_provenance() {
        let mut encoder = Av1Encoder::new().unwrap();
        let mut pending = VecDeque::new();
        let mut emitted = 0;
        let mut saw_need_more_data = false;
        let mut saw_forced_keyframe = false;

        for value in 0..12 {
            let forced_keyframe = value == 2;
            if forced_keyframe {
                encoder.request_keyframe();
            }
            let timing = VideoFrameTiming::start();
            let frame = VideoFrame::with_timing(
                16,
                16,
                PixelData::Rgb8(vec![value; 16 * 16 * 3]),
                Some(timing),
            );
            let encoded = encoder.encode(&frame).unwrap();
            let observation = encoder
                .last_observation
                .expect("encode must retain test diagnostics");
            pending.push_back((
                observation.submitted_sequence,
                timing,
                forced_keyframe,
                observation.context_generation,
                observation.context_rebuilt,
                observation.context_rebuild_us,
            ));
            saw_need_more_data |= observation.need_more_data_streak > 0;

            if let Some(encoded) = encoded {
                let (sequence, expected_timing, forced, generation, rebuilt, rebuild_us) =
                    pending.pop_front().expect("emitted frame must be pending");
                assert_eq!(encoded.timing(), Some(expected_timing));
                assert_eq!(observation.emitted_sequence, Some(sequence));
                assert_eq!(observation.emitted_context_generation, Some(generation));
                assert_eq!(observation.emitted_context_rebuilt, rebuilt);
                assert_eq!(observation.emitted_context_rebuild_us, rebuild_us);
                assert_eq!(observation.emitted_forced_keyframe, forced);
                assert_eq!(
                    observation.lag_frames,
                    Some(observation.submitted_sequence.wrapping_sub(sequence))
                );
                assert!(observation.encoder_residence_us.is_some());
                assert_eq!(observation.need_more_data_streak, 0);
                if forced {
                    assert!(encoded.keyframe);
                    saw_forced_keyframe = true;
                }
                emitted += 1;
            }
        }

        assert!(emitted > 0);
        assert!(saw_need_more_data);
        assert!(saw_forced_keyframe);
    }

    #[test]
    fn finish_drains_every_delayed_frame_in_submission_order_and_allows_reuse() {
        let mut encoder = Av1Encoder::new().unwrap();
        let mut timings = Vec::new();
        let mut output = Vec::new();
        for value in 0..7 {
            let timing = VideoFrameTiming::start();
            timings.push(timing);
            let frame = VideoFrame::with_timing(
                16,
                16,
                PixelData::Rgb8(vec![value; 16 * 16 * 3]),
                Some(timing),
            );
            output.extend(encoder.encode(&frame).unwrap());
        }
        output.extend(encoder.finish().unwrap());

        assert_eq!(output.len(), timings.len());
        assert_eq!(
            output
                .iter()
                .map(EncodedVideoFrame::timing)
                .collect::<Vec<_>>(),
            timings.into_iter().map(Some).collect::<Vec<_>>()
        );
        assert!(encoder.finish().unwrap().is_empty());

        let frame = VideoFrame::new(16, 16, PixelData::Rgb8(vec![0; 16 * 16 * 3]));
        let mut reused = encoder
            .encode(&frame)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>();
        reused.extend(encoder.finish().unwrap());
        assert_eq!(reused.len(), 1);
    }

    #[test]
    fn dimension_change_drains_the_previous_context_without_losing_frames() {
        let mut encoder = Av1Encoder::new().unwrap();
        let first_timing = VideoFrameTiming::start();
        let second_timing = VideoFrameTiming::start();
        let first = VideoFrame::with_timing(
            16,
            16,
            PixelData::Rgb8(vec![1; 16 * 16 * 3]),
            Some(first_timing),
        );
        let second = VideoFrame::with_timing(
            32,
            16,
            PixelData::Rgb8(vec![2; 32 * 16 * 3]),
            Some(second_timing),
        );

        let mut output = encoder
            .encode(&first)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>();
        output.extend(encoder.encode(&second).unwrap());
        output.extend(encoder.finish().unwrap());

        assert_eq!(output.len(), 2);
        assert_eq!(output[0].timing(), Some(first_timing));
        assert_eq!(output[1].timing(), Some(second_timing));
    }

    #[test]
    fn dimension_rebuild_increments_context_generation() {
        let mut encoder = Av1Encoder::new().unwrap();
        let original = VideoFrame::new(16, 16, PixelData::Rgb8(vec![0; 16 * 16 * 3]));
        let _ = encode_until_packet(&mut encoder, &original);

        let mut emitted_rebuilt_frame = false;
        for value in 0..8 {
            let timing = VideoFrameTiming::start();
            let resized = VideoFrame::with_timing(
                32,
                16,
                PixelData::Rgb8(vec![value; 32 * 16 * 3]),
                Some(timing),
            );
            let encoded = encoder.encode(&resized).unwrap();
            let observation = encoder
                .last_observation
                .expect("encode must retain test diagnostics");

            assert_eq!(observation.context_generation, 2);
            assert_eq!(observation.context_rebuilt, value == 0);
            if encoded.is_some() && observation.emitted_context_generation == Some(2) {
                assert!(observation.emitted_context_rebuilt);
                emitted_rebuilt_frame = true;
                break;
            }
        }

        assert!(emitted_rebuilt_frame);
    }
}
