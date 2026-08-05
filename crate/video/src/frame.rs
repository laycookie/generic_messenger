use std::{
    simd::{simd_swizzle, u8x16},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::VideoError;

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

static NEXT_VIDEO_FRAME_ID: AtomicU64 = AtomicU64::new(1);

/// Correlation metadata carried with one raw frame through capture, queues,
/// scaling, encoding, and transport.
///
/// Creating this value is cheap, but capture backends should do so only when
/// `video_timing` TRACE logging is enabled. Frames without metadata receive it
/// when first published through a [`crate::FrameProducer`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoFrameTiming {
    id: u64,
    started_at: Instant,
}

impl VideoFrameTiming {
    /// Start timing one frame at its earliest observable capture boundary.
    pub fn start() -> Self {
        Self {
            id: NEXT_VIDEO_FRAME_ID.fetch_add(1, Ordering::Relaxed),
            started_at: Instant::now(),
        }
    }

    /// Process-wide frame correlation ID.
    pub const fn id(self) -> u64 {
        self.id
    }

    /// Time since this frame entered pipeline.
    pub fn elapsed(self) -> Duration {
        self.started_at.elapsed()
    }

    /// Saturating microsecond form of [`Self::elapsed`], suitable for logs.
    pub fn elapsed_micros(self) -> u64 {
        u64::try_from(self.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

/// Decoded pixel data for one video frame.
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PixelData {
    /// RGBA8, row-major, `width * height * 4` bytes.
    Rgba8(Vec<u8>),
    /// RGB8, row-major, `width * height * 3` bytes.
    Rgb8(Vec<u8>),
    /// Immutable shared RGBA8. Useful for static/procedural sources whose
    /// pixels repeat across many frames without copying the full image.
    SharedRgba8(Arc<[u8]>),
    /// Immutable shared RGB8.
    SharedRgb8(Arc<[u8]>),
}

impl PixelData {
    fn bytes_per_pixel(&self) -> usize {
        match self {
            Self::Rgba8(_) | Self::SharedRgba8(_) => 4,
            Self::Rgb8(_) | Self::SharedRgb8(_) => 3,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Rgba8(data) | Self::Rgb8(data) => data,
            Self::SharedRgba8(data) | Self::SharedRgb8(data) => data,
        }
    }
}

/// One decoded, raw video frame.
///
/// Raw frames stay distinct from [`EncodedVideoFrame`], preventing compressed
/// codec data from crossing APIs that promise pixels.
#[derive(Debug, Eq, PartialEq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub pixel_data: PixelData,
    timing: Option<VideoFrameTiming>,
}

impl VideoFrame {
    /// Build a frame without capture timing metadata.
    pub fn new(width: u32, height: u32, pixel_data: PixelData) -> Self {
        Self {
            width,
            height,
            pixel_data,
            timing: None,
        }
    }

    /// Build a frame carrying timing started at its capture boundary.
    pub fn with_timing(
        width: u32,
        height: u32,
        pixel_data: PixelData,
        timing: Option<VideoFrameTiming>,
    ) -> Self {
        Self {
            width,
            height,
            pixel_data,
            timing,
        }
    }

    /// Correlation metadata when timing is enabled for this frame.
    pub const fn timing(&self) -> Option<VideoFrameTiming> {
        self.timing
    }

    /// Attach correlation metadata created before this frame was materialized.
    #[must_use]
    pub fn with_pipeline_timing(mut self, timing: Option<VideoFrameTiming>) -> Self {
        self.timing = timing;
        self
    }

    pub(crate) fn ensure_timing(&mut self) -> VideoFrameTiming {
        *self.timing.get_or_insert_with(VideoFrameTiming::start)
    }

    /// Validate dimensions and byte length for the frame's pixel format.
    pub fn validate(&self) -> Result<(), VideoError> {
        let pixels = (self.width as usize)
            .checked_mul(self.height as usize)
            .filter(|pixels| *pixels > 0)
            .ok_or(VideoError::InvalidDimensions)?;
        let bytes_per_pixel = self.pixel_data.bytes_per_pixel();
        let actual = self.pixel_data.as_bytes().len();
        let expected = pixels
            .checked_mul(bytes_per_pixel)
            .ok_or(VideoError::FrameSizeOverflow)?;
        if actual != expected {
            return Err(VideoError::FrameSizeMismatch { expected, actual });
        }
        Ok(())
    }

    /// Consume this frame and return RGBA8 pixels, adding opaque alpha to RGB8.
    pub fn into_rgba8(self) -> Result<Vec<u8>, VideoError> {
        self.validate()?;
        match self.pixel_data {
            PixelData::Rgba8(data) => Ok(data),
            PixelData::SharedRgba8(data) => Ok(data.as_ref().to_vec()),
            PixelData::Rgb8(data) => rgb_to_rgba(&data),
            PixelData::SharedRgb8(data) => rgb_to_rgba(&data),
        }
    }

    /// Dimensions produced when this frame is downscaled to fit inside the
    /// supplied bounds. Validates the frame without allocating or scaling it.
    pub fn dimensions_to_fit(
        &self,
        max_width: u32,
        max_height: u32,
    ) -> Result<(u32, u32), VideoError> {
        self.validate()?;
        if max_width == 0 || max_height == 0 {
            return Err(VideoError::InvalidDimensions);
        }
        if self.width <= max_width && self.height <= max_height {
            return Ok((self.width, self.height));
        }
        Ok(fitted_dimensions(
            self.width,
            self.height,
            max_width,
            max_height,
        ))
    }

    /// Downscale this frame using reusable scaler state.
    pub fn downscale_to_fit_with(
        self,
        scaler: &mut FrameScaler,
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, VideoError> {
        scaler.scale_to_fit(self, max_width, max_height)
    }

    /// Downscale this frame to fit inside `max_width` × `max_height`.
    ///
    /// Use [`Self::downscale_to_fit_with`] in frame loops. It caches sampling
    /// maps and recycles consumed source allocations between calls.
    pub fn downscale_to_fit(self, max_width: u32, max_height: u32) -> Result<Self, VideoError> {
        FrameScaler::default().scale_to_fit(self, max_width, max_height)
    }
}

fn rgb_to_rgba(data: &[u8]) -> Result<Vec<u8>, VideoError> {
    let pixel_count = data.len() / 3;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    rgb_to_rgba_simd(data, &mut rgba);
    Ok(rgba)
}

pub(crate) fn rgb_to_rgba_simd(data: &[u8], rgba: &mut Vec<u8>) {
    rgba.clear();
    rgba.reserve(data.len() / 3 * 4);
    let mut chunks = data.chunks_exact(12);
    for chunk in &mut chunks {
        let mut lanes = [0_u8; 16];
        lanes[..12].copy_from_slice(chunk);
        lanes[12] = 255;
        let input = u8x16::from_array(lanes);
        let output: u8x16 = simd_swizzle!(
            input,
            [0, 1, 2, 12, 3, 4, 5, 12, 6, 7, 8, 12, 9, 10, 11, 12]
        );
        rgba.extend_from_slice(&output.to_array());
    }
    for pixel in chunks.remainder().chunks_exact(3) {
        rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
    }
}

#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn rgb_to_rgba_scalar(data: &[u8], rgba: &mut Vec<u8>) {
    rgba.clear();
    rgba.reserve(data.len() / 3 * 4);
    for pixel in data.chunks_exact(3) {
        rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
    }
}

/// Stateful nearest-neighbor scaler for realtime frame loops.
///
/// Sampling indices are rebuilt only when geometry/format changes. For owned
/// input, consumed source allocation becomes next call's output scratch, so
/// steady-state scaling performs no scaler-owned allocation after warmup.
#[derive(Default)]
pub struct FrameScaler {
    output: Vec<u8>,
    source_x: Vec<usize>,
    map_key: Option<ScaleMapKey>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ScaleMapKey {
    source_width: u32,
    width: u32,
    bytes_per_pixel: usize,
}

impl FrameScaler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scale `frame` within bounds while preserving aspect ratio and format.
    pub fn scale_to_fit(
        &mut self,
        frame: VideoFrame,
        max_width: u32,
        max_height: u32,
    ) -> Result<VideoFrame, VideoError> {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let started = timing_enabled.then(Instant::now);
        let source_width = frame.width;
        let source_height = frame.height;
        let source_bytes = frame.pixel_data.as_bytes().len();
        let (width, height) = frame.dimensions_to_fit(max_width, max_height)?;
        if (frame.width, frame.height) == (width, height) {
            if let Some(started) = started {
                tracing::trace!(
                    target: "video_timing",
                    stage = "frame.scale",
                    frame_id = frame.timing.map_or(0, VideoFrameTiming::id),
                    pipeline_elapsed_us = frame
                        .timing
                        .map_or(0, VideoFrameTiming::elapsed_micros),
                    elapsed_us = elapsed_micros(started),
                    source_width,
                    source_height,
                    output_width = width,
                    output_height = height,
                    source_bytes,
                    output_bytes = source_bytes,
                    scaled = false,
                    "video stage complete"
                );
            }
            return Ok(frame);
        }
        let pixel_data = match frame.pixel_data {
            PixelData::Rgba8(data) => PixelData::Rgba8(self.scale_owned(
                data,
                frame.width,
                frame.height,
                width,
                height,
                4,
            )?),
            PixelData::Rgb8(data) => PixelData::Rgb8(self.scale_owned(
                data,
                frame.width,
                frame.height,
                width,
                height,
                3,
            )?),
            PixelData::SharedRgba8(data) => PixelData::Rgba8(self.scale_borrowed(
                &data,
                frame.width,
                frame.height,
                width,
                height,
                4,
            )?),
            PixelData::SharedRgb8(data) => PixelData::Rgb8(self.scale_borrowed(
                &data,
                frame.width,
                frame.height,
                width,
                height,
                3,
            )?),
        };
        let output_bytes = pixel_data.as_bytes().len();
        let frame = VideoFrame::with_timing(width, height, pixel_data, frame.timing);
        if let Some(started) = started {
            tracing::trace!(
                target: "video_timing",
                stage = "frame.scale",
                frame_id = frame.timing.map_or(0, VideoFrameTiming::id),
                pipeline_elapsed_us = frame
                    .timing
                    .map_or(0, VideoFrameTiming::elapsed_micros),
                elapsed_us = elapsed_micros(started),
                source_width,
                source_height,
                output_width = width,
                output_height = height,
                source_bytes,
                output_bytes,
                scaled = true,
                "video stage complete"
            );
        }
        Ok(frame)
    }

    fn prepare_map(&mut self, source_width: u32, width: u32, bytes_per_pixel: usize) {
        let key = ScaleMapKey {
            source_width,
            width,
            bytes_per_pixel,
        };
        if self.map_key == Some(key) {
            return;
        }
        self.source_x.clear();
        self.source_x.extend((0..width).map(|x| {
            (u64::from(x) * u64::from(source_width) / u64::from(width)) as usize * bytes_per_pixel
        }));
        self.map_key = Some(key);
    }

    fn fill_output(
        &mut self,
        source: &[u8],
        source_width: u32,
        source_height: u32,
        width: u32,
        height: u32,
        bytes_per_pixel: usize,
    ) -> Result<(), VideoError> {
        let output_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
            .ok_or(VideoError::FrameSizeOverflow)?;
        self.prepare_map(source_width, width, bytes_per_pixel);
        self.output.resize(output_len, 0);
        let output_row_len = width as usize * bytes_per_pixel;
        for (y, output_row) in self.output.chunks_exact_mut(output_row_len).enumerate() {
            let source_y = (y as u64 * u64::from(source_height) / u64::from(height)) as usize;
            let source_row = &source[source_y * source_width as usize * bytes_per_pixel..];
            for (pixel, source_x) in output_row
                .chunks_exact_mut(bytes_per_pixel)
                .zip(&self.source_x)
            {
                pixel.copy_from_slice(&source_row[*source_x..*source_x + bytes_per_pixel]);
            }
        }
        debug_assert_eq!(self.output.len(), output_len);
        Ok(())
    }

    fn scale_owned(
        &mut self,
        source: Vec<u8>,
        source_width: u32,
        source_height: u32,
        width: u32,
        height: u32,
        bytes_per_pixel: usize,
    ) -> Result<Vec<u8>, VideoError> {
        self.fill_output(
            &source,
            source_width,
            source_height,
            width,
            height,
            bytes_per_pixel,
        )?;
        // Return filled scratch and retain consumed source allocation. Native
        // capture owns a fresh source Vec anyway; scaler needs no second fresh
        // pixel allocation on following frames.
        Ok(std::mem::replace(&mut self.output, source))
    }

    fn scale_borrowed(
        &mut self,
        source: &[u8],
        source_width: u32,
        source_height: u32,
        width: u32,
        height: u32,
        bytes_per_pixel: usize,
    ) -> Result<Vec<u8>, VideoError> {
        self.fill_output(
            source,
            source_width,
            source_height,
            width,
            height,
            bytes_per_pixel,
        )?;
        Ok(std::mem::take(&mut self.output))
    }
}

fn fitted_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let (mut width, mut height) =
        if u64::from(max_width) * u64::from(height) <= u64::from(max_height) * u64::from(width) {
            (
                max_width,
                (u64::from(height) * u64::from(max_width) / u64::from(width)) as u32,
            )
        } else {
            (
                (u64::from(width) * u64::from(max_height) / u64::from(height)) as u32,
                max_height,
            )
        };
    width = width.max(1);
    height = height.max(1);
    // YUV420 encoders require even dimensions. Captures are normally much
    // larger than two pixels, so rounding down is safe and avoids padding.
    if width > 1 {
        width &= !1;
    }
    if height > 1 {
        height &= !1;
    }
    (width, height)
}

/// Compressed video codec carried by an [`EncodedVideoFrame`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum VideoCodec {
    H264,
    Av1,
}

/// One compressed video frame/access unit.
#[derive(Debug, Eq, PartialEq)]
pub struct EncodedVideoFrame {
    pub codec: VideoCodec,
    pub keyframe: bool,
    pub data: Vec<u8>,
    timing: Option<VideoFrameTiming>,
}

impl EncodedVideoFrame {
    /// Build compressed frame without raw-pipeline timing metadata.
    pub fn new(codec: VideoCodec, keyframe: bool, data: Vec<u8>) -> Self {
        Self {
            codec,
            keyframe,
            data,
            timing: None,
        }
    }

    #[cfg(any(feature = "h264", feature = "av1"))]
    pub(crate) fn with_timing(
        codec: VideoCodec,
        keyframe: bool,
        data: Vec<u8>,
        timing: Option<VideoFrameTiming>,
    ) -> Self {
        Self {
            codec,
            keyframe,
            data,
            timing,
        }
    }

    /// Correlation metadata copied from raw frame that produced this access unit.
    pub const fn timing(&self) -> Option<VideoFrameTiming> {
        self.timing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitted_dimensions_match_scaler_without_allocating() {
        let frame = VideoFrame::new(
            1920,
            1080,
            PixelData::SharedRgba8(vec![0; 1920 * 1080 * 4].into()),
        );

        assert_eq!(frame.dimensions_to_fit(1280, 720).unwrap(), (1280, 720));
        assert_eq!(frame.dimensions_to_fit(853, 480).unwrap(), (852, 478));
        assert!(matches!(
            frame.dimensions_to_fit(0, 720),
            Err(VideoError::InvalidDimensions)
        ));
    }

    #[test]
    fn rgb8_converts_to_opaque_rgba8() {
        let frame = VideoFrame::new(2, 1, PixelData::Rgb8(vec![1, 2, 3, 4, 5, 6]));
        assert_eq!(
            frame.into_rgba8().unwrap(),
            vec![1, 2, 3, 255, 4, 5, 6, 255]
        );
    }

    #[test]
    fn simd_rgb_expansion_matches_scalar_for_tails_and_alignments() {
        let mut state = 0x1234_5678_u64;
        for offset in 0..16 {
            for pixels in 0..=65 {
                let len = pixels * 3;
                let mut storage = vec![0_u8; offset + len + 16];
                for value in &mut storage {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    *value = (state >> 56) as u8;
                }
                let source = &storage[offset..offset + len];
                let mut scalar = Vec::new();
                let mut simd = Vec::new();

                rgb_to_rgba_scalar(source, &mut scalar);
                rgb_to_rgba_simd(source, &mut simd);

                assert_eq!(simd, scalar, "offset={offset} pixels={pixels}");
            }
        }
    }

    #[test]
    fn validation_rejects_mismatched_pixel_length() {
        let frame = VideoFrame::new(1, 1, PixelData::Rgba8(vec![0; 3]));
        assert!(matches!(
            frame.validate(),
            Err(VideoError::FrameSizeMismatch {
                expected: 4,
                actual: 3
            })
        ));
    }

    #[test]
    fn downscale_preserves_rgb_format_and_aspect_ratio() {
        let frame = VideoFrame::new(
            4,
            2,
            PixelData::Rgb8(
                [
                    1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0, 5, 0, 0, 6, 0, 0, 7, 0, 0, 8, 0, 0,
                ]
                .to_vec(),
            ),
        );

        let scaled = frame.downscale_to_fit(2, 2).unwrap();

        assert_eq!((scaled.width, scaled.height), (2, 1));
        assert_eq!(scaled.pixel_data, PixelData::Rgb8(vec![1, 0, 0, 3, 0, 0]));
    }

    #[test]
    fn downscale_rounds_encoder_dimensions_to_even_values() {
        let frame = VideoFrame::new(1920, 1080, PixelData::Rgba8(vec![0; 1920 * 1080 * 4]));

        let scaled = frame.downscale_to_fit(853, 480).unwrap();

        assert_eq!((scaled.width, scaled.height), (852, 478));
    }

    #[test]
    fn scaler_reuses_consumed_source_allocation_and_sampling_map() {
        let mut scaler = FrameScaler::new();
        let first_source = vec![7; 8 * 4 * 3];
        let first_source_ptr = first_source.as_ptr();
        let first = scaler
            .scale_to_fit(VideoFrame::new(8, 4, PixelData::Rgb8(first_source)), 4, 2)
            .unwrap();
        assert_eq!((first.width, first.height), (4, 2));
        let map_ptr = scaler.source_x.as_ptr();

        let second = scaler
            .scale_to_fit(
                VideoFrame::new(8, 4, PixelData::Rgb8(vec![9; 8 * 4 * 3])),
                4,
                2,
            )
            .unwrap();
        let PixelData::Rgb8(second_pixels) = second.pixel_data else {
            panic!("RGB8 input must retain format");
        };
        assert_eq!(second_pixels.as_ptr(), first_source_ptr);
        assert_eq!(scaler.source_x.as_ptr(), map_ptr);
        assert!(second_pixels.iter().all(|value| *value == 9));
    }

    #[test]
    fn shared_frame_inside_bounds_keeps_same_pixel_storage() {
        let pixels: Arc<[u8]> = vec![1; 4 * 2 * 4].into();
        let frame = VideoFrame::new(4, 2, PixelData::SharedRgba8(pixels.clone()));

        let scaled = FrameScaler::new().scale_to_fit(frame, 4, 2).unwrap();

        let PixelData::SharedRgba8(scaled_pixels) = scaled.pixel_data else {
            panic!("unscaled shared frame must remain shared");
        };
        assert!(Arc::ptr_eq(&pixels, &scaled_pixels));
    }

    #[test]
    fn scaler_preserves_pipeline_timing() {
        let timing = VideoFrameTiming::start();
        let frame =
            VideoFrame::with_timing(4, 2, PixelData::Rgb8(vec![0; 4 * 2 * 3]), Some(timing));

        let scaled = FrameScaler::new().scale_to_fit(frame, 2, 2).unwrap();

        assert_eq!(scaled.timing(), Some(timing));
    }
}
