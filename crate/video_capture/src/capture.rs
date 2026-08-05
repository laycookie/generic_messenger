use std::time::Instant;

use simple_video::{FrameProducer, FrameScaler, VideoFrame, VideoFrameTiming};

use crate::{
    CaptureError, CaptureResult, ScreenCapture, ScreenCaptureConfig, ScreenResolution,
    ScreenSource, WebcamCapture, WebcamCaptureConfig, WebcamSource, screen_sources, webcams,
};

pub(crate) fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Upper bounds applied to realtime capture before frames reach an encoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_frames_per_second: u32,
}

impl CaptureLimits {
    fn validate(self) -> CaptureResult<()> {
        if self.max_width == 0 || self.max_height == 0 || self.max_frames_per_second == 0 {
            return Err(CaptureError::InvalidConfiguration(
                "capture limits must be greater than zero",
            ));
        }
        Ok(())
    }
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_width: 1280,
            max_height: 720,
            max_frames_per_second: 30,
        }
    }
}

/// Blocking source of decoded video frames.
pub trait FrameCapture {
    fn next_frame(&mut self) -> CaptureResult<VideoFrame>;

    /// Capture next frame while bounding dimensions.
    ///
    /// Default implementation captures then scales. Backends may override this
    /// to fuse native pixel conversion and scaling without processing pixels
    /// which will be discarded.
    fn next_frame_bounded(&mut self, max_width: u32, max_height: u32) -> CaptureResult<VideoFrame> {
        self.next_frame_bounded_with(&mut FrameScaler::new(), max_width, max_height)
    }

    /// Stateful form for capture loops. Default software path reuses scaling
    /// allocations; native backends may override to fuse conversion/scaling.
    fn next_frame_bounded_with(
        &mut self,
        scaler: &mut FrameScaler,
        max_width: u32,
        max_height: u32,
    ) -> CaptureResult<VideoFrame> {
        let fallback_timing = tracing::enabled!(target: "video_timing", tracing::Level::TRACE)
            .then(VideoFrameTiming::start);
        let frame = self.next_frame()?;
        let timing = frame.timing().or(fallback_timing);
        Ok(scaler.scale_to_fit(frame.with_pipeline_timing(timing), max_width, max_height)?)
    }
}

/// Capture frames until consuming side of `output` closes or capture fails.
///
/// Function blocks. Run it on a dedicated thread when UI or async tasks consume
/// receiving half of [`simple_video::frame_channel`]. Some native capture
/// handles are not [`Send`], so open capture inside that thread.
pub fn capture_to_channel(
    mut capture: impl FrameCapture,
    mut output: FrameProducer,
) -> CaptureResult<()> {
    while !output.is_closed() {
        let fallback_timing = tracing::enabled!(target: "video_timing", tracing::Level::TRACE)
            .then(VideoFrameTiming::start);
        let frame = capture.next_frame()?;
        let timing = frame.timing().or(fallback_timing);
        let frame = frame.with_pipeline_timing(timing);
        if !output.push(frame) {
            break;
        }
    }
    Ok(())
}

/// Capture frames with software dimension bounds until `output` closes.
///
/// Software scaling is required as a fallback because some native backends,
/// notably PipeWire through `scap`, can return native resolution despite an
/// output-resolution request.
pub fn capture_to_channel_bounded(
    mut capture: impl FrameCapture,
    mut output: FrameProducer,
    limits: CaptureLimits,
) -> CaptureResult<()> {
    limits.validate()?;
    let mut scaler = FrameScaler::new();
    while !output.is_closed() {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let total_started = timing_enabled.then(Instant::now);
        let capture_started = timing_enabled.then(Instant::now);
        let frame =
            capture.next_frame_bounded_with(&mut scaler, limits.max_width, limits.max_height)?;
        let capture_us = capture_started.map(elapsed_micros).unwrap_or_default();
        let output_width = frame.width;
        let output_height = frame.height;
        let output_bytes = frame.pixel_data.as_bytes().len();
        let frame_timing = frame.timing();
        let publish_started = timing_enabled.then(Instant::now);
        let published = output.push(frame);
        let publish_us = publish_started.map(elapsed_micros).unwrap_or_default();
        if let Some(total_started) = total_started {
            tracing::trace!(
                target: "video_timing",
                stage = "capture.bounded",
                frame_id = frame_timing.map_or(0, |timing| timing.id()),
                pipeline_elapsed_us = frame_timing.map_or(0, |timing| timing.elapsed_micros()),
                elapsed_us = elapsed_micros(total_started),
                capture_us,
                publish_us,
                output_width,
                output_height,
                output_bytes,
                max_width = limits.max_width,
                max_height = limits.max_height,
                max_frames_per_second = limits.max_frames_per_second,
                published,
                "video stage complete"
            );
        }
        if !published {
            break;
        }
    }
    Ok(())
}

/// Any discoverable video input source.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CaptureSource {
    Webcam(WebcamSource),
    Screen(ScreenSource),
}

impl CaptureSource {
    /// Human-readable source name suitable for a picker.
    pub fn name(&self) -> &str {
        match self {
            Self::Webcam(source) => source.name(),
            Self::Screen(source) => source.title(),
        }
    }

    /// Open source with backend's default capture configuration.
    pub fn open(&self) -> CaptureResult<Capture> {
        match self {
            Self::Webcam(source) => source
                .open(WebcamCaptureConfig::default())
                .map(Capture::Webcam),
            Self::Screen(source) => source
                .open(ScreenCaptureConfig::default())
                .map(|capture| Capture::Screen(Box::new(capture))),
        }
    }

    /// Open source with device-side realtime bounds where supported.
    ///
    /// Use [`capture_to_channel_bounded`] as well: Linux screen capture can
    /// negotiate native PipeWire resolution despite this request.
    pub fn open_bounded(&self, limits: CaptureLimits) -> CaptureResult<Capture> {
        limits.validate()?;
        match self {
            Self::Webcam(source) => source
                .open_bounded(
                    limits.max_width,
                    limits.max_height,
                    limits.max_frames_per_second,
                )
                .map(Capture::Webcam),
            Self::Screen(source) => source
                .open(ScreenCaptureConfig {
                    frames_per_second: limits.max_frames_per_second,
                    resolution: screen_resolution(limits.max_height),
                    ..ScreenCaptureConfig::default()
                })
                .map(|capture| Capture::Screen(Box::new(capture))),
        }
    }
}

fn screen_resolution(max_height: u32) -> ScreenResolution {
    match max_height {
        0..=480 => ScreenResolution::P480,
        481..=720 => ScreenResolution::P720,
        721..=1080 => ScreenResolution::P1080,
        1081..=1440 => ScreenResolution::P1440,
        1441..=2160 => ScreenResolution::P2160,
        _ => ScreenResolution::P4320,
    }
}

/// Discover webcams followed by displays and windows.
///
/// Webcam discovery errors are returned. Screen source discovery itself is
/// infallible; permission errors occur when a screen source is opened.
pub fn sources() -> CaptureResult<Vec<CaptureSource>> {
    let mut sources = webcams()?
        .into_iter()
        .map(CaptureSource::Webcam)
        .collect::<Vec<_>>();
    sources.extend(screen_sources().into_iter().map(CaptureSource::Screen));
    Ok(sources)
}

/// Any active webcam or screen capture stream.
#[non_exhaustive]
pub enum Capture {
    Webcam(WebcamCapture),
    /// Screen backends hold substantially more native state than webcams.
    /// Indirection keeps this sum type cheap to move through capture tasks.
    Screen(Box<ScreenCapture>),
}

impl Capture {
    /// Capture next decoded frame. Blocks until frame arrives.
    pub fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
        match self {
            Self::Webcam(capture) => capture.next_frame(),
            Self::Screen(capture) => capture.next_frame(),
        }
    }
}

impl FrameCapture for Capture {
    fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
        Self::next_frame(self)
    }

    fn next_frame_bounded(&mut self, max_width: u32, max_height: u32) -> CaptureResult<VideoFrame> {
        match self {
            Self::Webcam(capture) => capture.next_frame_bounded(max_width, max_height),
            Self::Screen(capture) => capture.next_frame_bounded(max_width, max_height),
        }
    }

    fn next_frame_bounded_with(
        &mut self,
        scaler: &mut FrameScaler,
        max_width: u32,
        max_height: u32,
    ) -> CaptureResult<VideoFrame> {
        match self {
            Self::Webcam(capture) => {
                Ok(scaler.scale_to_fit(capture.next_frame()?, max_width, max_height)?)
            }
            // ScreenCapture owns a fused converter/scaler with equivalent
            // reusable state; a second software pass would waste work.
            Self::Screen(capture) => capture.next_frame_bounded(max_width, max_height),
        }
    }
}
