use scap::{
    Target,
    capturer::{Area, Capturer, Options, Point, Resolution as ScapResolution, Size},
    frame::{Frame, FrameType},
};
use simple_video::{FrameScaler, PixelData, VideoError, VideoFrame, VideoFrameTiming};
use std::simd::{simd_swizzle, u8x8, u8x16};
use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind},
    time::Instant,
};

use crate::{CaptureError, CaptureResult, FrameCapture, capture::elapsed_micros};

/// Kind of display-system target represented by [`ScreenSource`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ScreenSourceKind {
    /// Platform default. Linux opens desktop portal's interactive picker.
    Default,
    Display,
    Window,
}

/// Discoverable screen, display, or window capture source.
#[derive(Clone, Debug)]
pub struct ScreenSource {
    kind: ScreenSourceKind,
    id: Option<u32>,
    title: String,
    target: Option<Target>,
}

impl ScreenSource {
    /// Platform default target.
    ///
    /// On Linux, opening this source presents desktop portal's screen/window
    /// picker. On macOS and Windows, it captures primary display.
    pub fn platform_default() -> Self {
        Self {
            kind: ScreenSourceKind::Default,
            id: None,
            title: if cfg!(target_os = "linux") {
                "Select screen or window".to_owned()
            } else {
                "Primary display".to_owned()
            },
            target: None,
        }
    }

    pub fn kind(&self) -> ScreenSourceKind {
        self.kind
    }

    pub fn id(&self) -> Option<u32> {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Open and start this screen source using `config`.
    pub fn open(&self, config: ScreenCaptureConfig) -> CaptureResult<ScreenCapture> {
        config.validate()?;
        let options = Options {
            fps: config.frames_per_second,
            show_cursor: config.show_cursor,
            show_highlight: config.show_highlight,
            target: self.target.clone(),
            crop_area: config.crop_area.map(CropArea::into_scap),
            output_type: FrameType::BGRAFrame,
            output_resolution: config.resolution.into_scap(),
            excluded_targets: None,
        };
        // scap 0.0.8 uses `expect` when Linux portal selection is cancelled.
        // Keep that backend detail from terminating applications using this
        // crate's fallible API.
        let capturer = catch_unwind(AssertUnwindSafe(|| {
            let mut capturer = Capturer::build(options)?;
            capturer.start_capture();
            Ok::<Capturer, scap::capturer::CapturerBuildError>(capturer)
        }))
        .map_err(|payload| CaptureError::ScreenBackendPanicked(panic_message(payload)))??;
        Ok(ScreenCapture {
            capturer,
            running: true,
            normalizer: RgbFrameNormalizer::default(),
        })
    }
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "unknown backend panic".to_owned()
    }
}

/// Region cropped from selected screen target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropArea {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl CropArea {
    fn into_scap(self) -> Area {
        Area {
            origin: Point {
                x: self.x,
                y: self.y,
            },
            size: Size {
                width: self.width,
                height: self.height,
            },
        }
    }
}

/// Maximum output resolution requested from `scap`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum ScreenResolution {
    P480,
    P720,
    P1080,
    P1440,
    P2160,
    P4320,
    #[default]
    Captured,
}

impl ScreenResolution {
    fn into_scap(self) -> ScapResolution {
        match self {
            Self::P480 => ScapResolution::_480p,
            Self::P720 => ScapResolution::_720p,
            Self::P1080 => ScapResolution::_1080p,
            Self::P1440 => ScapResolution::_1440p,
            Self::P2160 => ScapResolution::_2160p,
            Self::P4320 => ScapResolution::_4320p,
            Self::Captured => ScapResolution::Captured,
        }
    }
}

/// Screen capture options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenCaptureConfig {
    pub frames_per_second: u32,
    pub show_cursor: bool,
    pub show_highlight: bool,
    pub crop_area: Option<CropArea>,
    pub resolution: ScreenResolution,
}

impl ScreenCaptureConfig {
    fn validate(self) -> CaptureResult<()> {
        if self.frames_per_second == 0 {
            return Err(CaptureError::InvalidConfiguration(
                "screen frame rate must be greater than zero",
            ));
        }
        if let Some(area) = self.crop_area
            && (!area.x.is_finite()
                || !area.y.is_finite()
                || !area.width.is_finite()
                || !area.height.is_finite()
                || area.x < 0.0
                || area.y < 0.0
                || area.width <= 0.0
                || area.height <= 0.0)
        {
            return Err(CaptureError::InvalidConfiguration(
                "screen crop must contain finite, non-negative coordinates and positive dimensions",
            ));
        }
        Ok(())
    }
}

impl Default for ScreenCaptureConfig {
    fn default() -> Self {
        Self {
            frames_per_second: 30,
            show_cursor: true,
            show_highlight: false,
            crop_area: None,
            resolution: ScreenResolution::Captured,
        }
    }
}

/// Whether `scap` supports screen capture on current system.
pub fn screen_capture_supported() -> bool {
    scap::is_supported()
}

/// Whether current process has screen-capture permission.
pub fn has_screen_capture_permission() -> bool {
    scap::has_permission()
}

/// Request screen-capture permission. May present native system UI.
pub fn request_screen_capture_permission() -> bool {
    scap::request_permission()
}

/// Discover screen targets.
///
/// Returned list always starts with platform default. Linux target selection
/// happens through desktop portal when default source is opened, so no other
/// targets are listed there.
pub fn screen_sources() -> Vec<ScreenSource> {
    let mut sources = vec![ScreenSource::platform_default()];
    sources.extend(scap::get_all_targets().into_iter().map(|target| {
        let (kind, id, title) = match &target {
            Target::Display(display) => {
                (ScreenSourceKind::Display, display.id, display.title.clone())
            }
            Target::Window(window) => (ScreenSourceKind::Window, window.id, window.title.clone()),
        };
        ScreenSource {
            kind,
            id: Some(id),
            title,
            target: Some(target),
        }
    }));
    sources
}

/// Active `scap` screen stream producing decoded RGB8 frames.
pub struct ScreenCapture {
    capturer: Capturer,
    running: bool,
    normalizer: RgbFrameNormalizer,
}

impl ScreenCapture {
    /// Capture next screen frame. Blocks until frame arrives.
    pub fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
        self.next_frame_bounded(u32::MAX, u32::MAX)
    }

    /// Capture and normalize next frame, fusing native format conversion with
    /// downscaling so high-resolution source pixels are never converted only
    /// to be discarded.
    pub fn next_frame_bounded(
        &mut self,
        max_width: u32,
        max_height: u32,
    ) -> CaptureResult<VideoFrame> {
        if max_width == 0 || max_height == 0 {
            return Err(CaptureError::InvalidConfiguration(
                "screen capture bounds must be greater than zero",
            ));
        }
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let frame_timing = timing_enabled.then(VideoFrameTiming::start);
        let total_started = timing_enabled.then(Instant::now);
        let wait_started = timing_enabled.then(Instant::now);
        loop {
            let frame = self.capturer.get_next_frame()?;
            let (source_width, source_height, source_bytes, source_format) = frame_metadata(&frame);
            // macOS uses an empty frame to report an idle stream.
            if (source_width, source_height) == (0, 0) {
                continue;
            }
            // Linux negotiates PipeWire's native layout instead of honoring
            // requested BGRA output, so normalize every format it can return.
            let wait_us = wait_started.map(elapsed_micros).unwrap_or_default();
            let normalize_started = timing_enabled.then(Instant::now);
            let normalized = match frame {
                Frame::BGRA(frame) => self.normalizer.normalize(
                    frame.width,
                    frame.height,
                    frame.data,
                    RgbLayout::new(4, [2, 1, 0]),
                    max_width,
                    max_height,
                ),
                Frame::RGB(frame) => self.normalizer.normalize(
                    frame.width,
                    frame.height,
                    frame.data,
                    RgbLayout::new(3, [0, 1, 2]),
                    max_width,
                    max_height,
                ),
                Frame::RGBx(frame) => self.normalizer.normalize(
                    frame.width,
                    frame.height,
                    frame.data,
                    RgbLayout::new(4, [0, 1, 2]),
                    max_width,
                    max_height,
                ),
                Frame::XBGR(frame) => self.normalizer.normalize(
                    frame.width,
                    frame.height,
                    frame.data,
                    RgbLayout::new(4, [3, 2, 1]),
                    max_width,
                    max_height,
                ),
                Frame::BGRx(frame) => self.normalizer.normalize(
                    frame.width,
                    frame.height,
                    frame.data,
                    RgbLayout::new(4, [2, 1, 0]),
                    max_width,
                    max_height,
                ),
                Frame::BGR0(frame) => self.normalizer.normalize(
                    frame.width,
                    frame.height,
                    frame.data,
                    RgbLayout::new(3, [2, 1, 0]),
                    max_width,
                    max_height,
                ),
                Frame::YUVFrame(_) => Err(CaptureError::UnexpectedScreenFormat("YUV")),
            }?
            .with_pipeline_timing(frame_timing);
            if let Some(total_started) = total_started {
                tracing::trace!(
                    target: "video_timing",
                    stage = "screen.capture",
                    frame_id = frame_timing.map_or(0, VideoFrameTiming::id),
                    pipeline_elapsed_us = frame_timing
                        .map_or(0, VideoFrameTiming::elapsed_micros),
                    elapsed_us = elapsed_micros(total_started),
                    wait_us,
                    normalize_us = normalize_started.map(elapsed_micros).unwrap_or_default(),
                    source_width,
                    source_height,
                    source_bytes,
                    source_format,
                    output_width = normalized.width,
                    output_height = normalized.height,
                    output_bytes = normalized.pixel_data.as_bytes().len(),
                    "video stage complete"
                );
            }
            return Ok(normalized);
        }
    }

    /// Stop native screen stream. Safe to call more than once.
    pub fn stop(&mut self) {
        if self.running {
            self.capturer.stop_capture();
            self.running = false;
        }
    }
}

impl FrameCapture for ScreenCapture {
    fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
        Self::next_frame(self)
    }

    fn next_frame_bounded(&mut self, max_width: u32, max_height: u32) -> CaptureResult<VideoFrame> {
        Self::next_frame_bounded(self, max_width, max_height)
    }

    fn next_frame_bounded_with(
        &mut self,
        _scaler: &mut FrameScaler,
        max_width: u32,
        max_height: u32,
    ) -> CaptureResult<VideoFrame> {
        Self::next_frame_bounded(self, max_width, max_height)
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn frame_metadata(frame: &Frame) -> (i32, i32, usize, &'static str) {
    match frame {
        Frame::YUVFrame(frame) => (
            frame.width,
            frame.height,
            frame
                .luminance_bytes
                .len()
                .saturating_add(frame.chrominance_bytes.len()),
            "YUV",
        ),
        Frame::RGB(frame) => (frame.width, frame.height, frame.data.len(), "RGB"),
        Frame::RGBx(frame) => (frame.width, frame.height, frame.data.len(), "RGBx"),
        Frame::XBGR(frame) => (frame.width, frame.height, frame.data.len(), "XBGR"),
        Frame::BGRx(frame) => (frame.width, frame.height, frame.data.len(), "BGRx"),
        Frame::BGR0(frame) => (frame.width, frame.height, frame.data.len(), "BGR0"),
        Frame::BGRA(frame) => (frame.width, frame.height, frame.data.len(), "BGRA"),
    }
}

fn dimensions(width: i32, height: i32) -> CaptureResult<(u32, u32)> {
    let original_width = width;
    let original_height = height;
    let width = u32::try_from(width).ok().filter(|width| *width > 0).ok_or(
        CaptureError::InvalidDimensions {
            width: original_width,
            height: original_height,
        },
    )?;
    let height = u32::try_from(height)
        .ok()
        .filter(|height| *height > 0)
        .ok_or(CaptureError::InvalidDimensions {
            width: original_width,
            height: original_height,
        })?;
    Ok((width, height))
}

#[derive(Default)]
struct RgbFrameNormalizer {
    output: Vec<u8>,
    source_x: Vec<usize>,
    map_key: Option<(u32, u32, usize)>,
}

#[derive(Clone, Copy)]
struct RgbLayout {
    bytes_per_pixel: usize,
    channels: [usize; 3],
}

impl RgbLayout {
    const fn new(bytes_per_pixel: usize, channels: [usize; 3]) -> Self {
        Self {
            bytes_per_pixel,
            channels,
        }
    }
}

impl RgbFrameNormalizer {
    fn normalize(
        &mut self,
        width: i32,
        height: i32,
        mut data: Vec<u8>,
        layout: RgbLayout,
        max_width: u32,
        max_height: u32,
    ) -> CaptureResult<VideoFrame> {
        let RgbLayout {
            bytes_per_pixel: source_bytes_per_pixel,
            channels,
        } = layout;
        let (width, height) = dimensions(width, height)?;
        let source_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(source_bytes_per_pixel))
            .ok_or(VideoError::FrameSizeOverflow)?;
        if data.len() != source_len {
            return Err(VideoError::FrameSizeMismatch {
                expected: source_len,
                actual: data.len(),
            }
            .into());
        }

        let (output_width, output_height) = fitted_dimensions(width, height, max_width, max_height);
        if (output_width, output_height) == (width, height)
            && source_bytes_per_pixel == 3
            && channels == [0, 1, 2]
        {
            return Ok(VideoFrame::new(width, height, PixelData::Rgb8(data)));
        }
        if (output_width, output_height) == (width, height)
            && source_bytes_per_pixel == 3
            && channels == [2, 1, 0]
        {
            bgr_to_rgb_simd(&mut data);
            return Ok(VideoFrame::new(width, height, PixelData::Rgb8(data)));
        }
        if (output_width, output_height) == (width, height) && source_bytes_per_pixel == 4 {
            packed_4_to_rgb_simd(&data, &mut self.output, channels);
            return Ok(VideoFrame::new(
                width,
                height,
                PixelData::Rgb8(std::mem::replace(&mut self.output, data)),
            ));
        }

        let output_len = (output_width as usize)
            .checked_mul(output_height as usize)
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or(VideoError::FrameSizeOverflow)?;
        let map_key = (width, output_width, source_bytes_per_pixel);
        if self.map_key != Some(map_key) {
            self.source_x.clear();
            self.source_x.extend((0..output_width).map(|x| {
                (u64::from(x) * u64::from(width) / u64::from(output_width)) as usize
                    * source_bytes_per_pixel
            }));
            self.map_key = Some(map_key);
        }
        self.output.resize(output_len, 0);
        let output_row_len = output_width as usize * 3;
        for (y, output_row) in self.output.chunks_exact_mut(output_row_len).enumerate() {
            let source_y = (y as u64 * u64::from(height) / u64::from(output_height)) as usize;
            let source_row = &data[source_y * width as usize * source_bytes_per_pixel..];
            for (pixel, source_x) in output_row.chunks_exact_mut(3).zip(&self.source_x) {
                pixel[0] = source_row[*source_x + channels[0]];
                pixel[1] = source_row[*source_x + channels[1]];
                pixel[2] = source_row[*source_x + channels[2]];
            }
        }
        debug_assert_eq!(self.output.len(), output_len);

        let frame = VideoFrame::new(
            output_width,
            output_height,
            PixelData::Rgb8(std::mem::replace(&mut self.output, data)),
        );
        frame.validate()?;
        Ok(frame)
    }
}

pub(crate) fn packed_4_to_rgb_scalar(source: &[u8], output: &mut Vec<u8>, channels: [usize; 3]) {
    output.clear();
    output.reserve(source.len() / 4 * 3);
    for pixel in source.chunks_exact(4) {
        output.extend_from_slice(&[pixel[channels[0]], pixel[channels[1]], pixel[channels[2]]]);
    }
}

pub(crate) fn packed_4_to_rgb_simd(source: &[u8], output: &mut Vec<u8>, channels: [usize; 3]) {
    if !matches!(channels, [0, 1, 2] | [2, 1, 0] | [3, 2, 1]) {
        packed_4_to_rgb_scalar(source, output, channels);
        return;
    }
    output.clear();
    output.reserve(source.len() / 4 * 3);
    let mut chunks = source.chunks_exact(16);
    for chunk in &mut chunks {
        let source = u8x16::from_slice(chunk);
        let rgb: u8x16 = match channels {
            [0, 1, 2] => {
                simd_swizzle!(source, [0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, 0, 0, 0, 0])
            }
            [2, 1, 0] => {
                simd_swizzle!(source, [2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, 0, 0, 0, 0])
            }
            [3, 2, 1] => simd_swizzle!(
                source,
                [3, 2, 1, 7, 6, 5, 11, 10, 9, 15, 14, 13, 0, 0, 0, 0]
            ),
            _ => unreachable!("unsupported layout handled above"),
        };
        output.extend_from_slice(&rgb.to_array()[..12]);
    }
    for pixel in chunks.remainder().chunks_exact(4) {
        output.extend_from_slice(&[pixel[channels[0]], pixel[channels[1]], pixel[channels[2]]]);
    }
}

pub(crate) fn bgr_to_rgb_scalar(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(3) {
        pixel.swap(0, 2);
    }
}

pub(crate) fn bgr_to_rgb_simd(pixels: &mut [u8]) {
    let mut chunks = pixels.chunks_exact_mut(24);
    for chunk in &mut chunks {
        let first = u8x16::from_slice(chunk);
        let mut second = [0_u8; 16];
        second[..8].copy_from_slice(&chunk[16..24]);
        let second = u8x16::from_array(second);
        let output_first: u8x16 = simd_swizzle!(
            first,
            second,
            [2, 1, 0, 5, 4, 3, 8, 7, 6, 11, 10, 9, 14, 13, 12, 17]
        );
        let output_second: u8x8 = simd_swizzle!(first, second, [16, 15, 20, 19, 18, 23, 22, 21]);
        output_first.copy_to_slice(&mut chunk[..16]);
        output_second.copy_to_slice(&mut chunk[16..24]);
    }
    bgr_to_rgb_scalar(chunks.into_remainder());
}

fn fitted_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }

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
    if width > 1 {
        width &= !1;
    }
    if height > 1 {
        height &= !1;
    }
    (width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize_rgb_frame(
        width: i32,
        height: i32,
        data: Vec<u8>,
        source_bytes_per_pixel: usize,
        channels: [usize; 3],
        max_width: u32,
        max_height: u32,
    ) -> CaptureResult<VideoFrame> {
        RgbFrameNormalizer::default().normalize(
            width,
            height,
            data,
            RgbLayout::new(source_bytes_per_pixel, channels),
            max_width,
            max_height,
        )
    }

    #[test]
    fn converts_bgra_to_rgb() {
        let frame =
            normalize_rgb_frame(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8], 4, [2, 1, 0], 2, 1).unwrap();
        assert_eq!(frame.pixel_data, PixelData::Rgb8(vec![3, 2, 1, 7, 6, 5]));
    }

    #[test]
    fn packed_simd_candidates_match_scalar_for_layouts_tails_and_alignments() {
        let mut state = 0xfeed_face_u64;
        for channels in [[0, 1, 2], [2, 1, 0], [3, 2, 1]] {
            for offset in 0..16 {
                for pixels in 0..=65 {
                    let len = pixels * 4;
                    let mut storage = vec![0_u8; offset + len + 16];
                    for byte in &mut storage {
                        state = state
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1);
                        *byte = (state >> 56) as u8;
                    }
                    let source = &storage[offset..offset + len];
                    let mut scalar = Vec::new();
                    let mut simd = Vec::new();
                    packed_4_to_rgb_scalar(source, &mut scalar, channels);
                    packed_4_to_rgb_simd(source, &mut simd, channels);
                    assert_eq!(simd, scalar);
                }
            }
        }
    }

    #[test]
    fn bgr_simd_candidate_matches_scalar_for_tails_and_alignments() {
        let mut state = 0xdeca_fbad_u64;
        for offset in 0..16 {
            for pixels in 0..=65 {
                let len = pixels * 3;
                let mut scalar = vec![0_u8; offset + len + 16];
                for byte in &mut scalar {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    *byte = (state >> 56) as u8;
                }
                let mut simd = scalar.clone();
                bgr_to_rgb_scalar(&mut scalar[offset..offset + len]);
                bgr_to_rgb_simd(&mut simd[offset..offset + len]);
                assert_eq!(simd, scalar);
            }
        }
    }

    #[test]
    fn normalizes_pipewire_formats() {
        assert_eq!(
            normalize_rgb_frame(1, 1, vec![1, 2, 3, 0], 4, [0, 1, 2], 1, 1)
                .unwrap()
                .pixel_data,
            PixelData::Rgb8(vec![1, 2, 3])
        );
        assert_eq!(
            normalize_rgb_frame(1, 1, vec![0, 3, 2, 1], 4, [3, 2, 1], 1, 1)
                .unwrap()
                .pixel_data,
            PixelData::Rgb8(vec![1, 2, 3])
        );
        assert_eq!(
            normalize_rgb_frame(1, 1, vec![3, 2, 1, 0], 4, [2, 1, 0], 1, 1)
                .unwrap()
                .pixel_data,
            PixelData::Rgb8(vec![1, 2, 3])
        );
        assert_eq!(
            normalize_rgb_frame(1, 1, vec![3, 2, 1], 3, [2, 1, 0], 1, 1)
                .unwrap()
                .pixel_data,
            PixelData::Rgb8(vec![1, 2, 3])
        );
    }

    #[test]
    fn fuses_bgra_conversion_and_downscale() {
        let data = [
            [1, 2, 3, 0],
            [4, 5, 6, 0],
            [7, 8, 9, 0],
            [10, 11, 12, 0],
            [13, 14, 15, 0],
            [16, 17, 18, 0],
            [19, 20, 21, 0],
            [22, 23, 24, 0],
        ]
        .concat();
        let frame = normalize_rgb_frame(4, 2, data, 4, [2, 1, 0], 2, 2).unwrap();

        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.pixel_data, PixelData::Rgb8(vec![3, 2, 1, 9, 8, 7]));
    }

    #[test]
    fn normalizer_reuses_source_allocation_and_x_map() {
        let mut normalizer = RgbFrameNormalizer::default();
        let first_source = vec![3; 8 * 4 * 4];
        let first_source_ptr = first_source.as_ptr();
        let _first = normalizer
            .normalize(8, 4, first_source, RgbLayout::new(4, [2, 1, 0]), 4, 2)
            .unwrap();
        let map_ptr = normalizer.source_x.as_ptr();

        let second = normalizer
            .normalize(8, 4, vec![9; 8 * 4 * 4], RgbLayout::new(4, [2, 1, 0]), 4, 2)
            .unwrap();
        let PixelData::Rgb8(pixels) = second.pixel_data else {
            panic!("screen normalizer must output RGB8");
        };
        assert_eq!(pixels.as_ptr(), first_source_ptr);
        assert_eq!(normalizer.source_x.as_ptr(), map_ptr);
    }

    #[test]
    fn rejects_zero_frame_rate() {
        let config = ScreenCaptureConfig {
            frames_per_second: 0,
            ..ScreenCaptureConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(CaptureError::InvalidConfiguration(_))
        ));
    }
}
