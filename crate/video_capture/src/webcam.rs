use std::{fmt, time::Instant};

use nokhwa::{
    Camera,
    pixel_format::RgbFormat,
    utils::{
        ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
        Resolution,
    },
};
use simple_video::{PixelData, VideoFrame, VideoFrameTiming};

use crate::{CaptureResult, FrameCapture, capture::elapsed_micros};

/// Stable, owned identifier used to reopen a webcam.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum WebcamId {
    Index(u32),
    String(String),
}

impl fmt::Display for WebcamId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index(index) => index.fmt(formatter),
            Self::String(id) => id.fmt(formatter),
        }
    }
}

impl From<&CameraIndex> for WebcamId {
    fn from(index: &CameraIndex) -> Self {
        match index {
            CameraIndex::Index(index) => Self::Index(*index),
            CameraIndex::String(id) => Self::String(id.clone()),
        }
    }
}

impl From<&WebcamId> for CameraIndex {
    fn from(id: &WebcamId) -> Self {
        match id {
            WebcamId::Index(index) => Self::Index(*index),
            WebcamId::String(id) => Self::String(id.clone()),
        }
    }
}

/// Device format delivered by a webcam before `nokhwa` decodes it to RGB8.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum WebcamPixelFormat {
    Mjpeg,
    Yuyv,
    Nv12,
    Gray,
    RawRgb,
    RawBgr,
}

impl From<FrameFormat> for WebcamPixelFormat {
    fn from(format: FrameFormat) -> Self {
        match format {
            FrameFormat::MJPEG => Self::Mjpeg,
            FrameFormat::YUYV => Self::Yuyv,
            FrameFormat::NV12 => Self::Nv12,
            FrameFormat::GRAY => Self::Gray,
            FrameFormat::RAWRGB => Self::RawRgb,
            FrameFormat::RAWBGR => Self::RawBgr,
        }
    }
}

impl From<WebcamPixelFormat> for FrameFormat {
    fn from(format: WebcamPixelFormat) -> Self {
        match format {
            WebcamPixelFormat::Mjpeg => Self::MJPEG,
            WebcamPixelFormat::Yuyv => Self::YUYV,
            WebcamPixelFormat::Nv12 => Self::NV12,
            WebcamPixelFormat::Gray => Self::GRAY,
            WebcamPixelFormat::RawRgb => Self::RAWRGB,
            WebcamPixelFormat::RawBgr => Self::RAWBGR,
        }
    }
}

/// Resolution, frame rate, and device-side format supported by a webcam.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WebcamFormat {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
    pub pixel_format: WebcamPixelFormat,
}

impl From<CameraFormat> for WebcamFormat {
    fn from(format: CameraFormat) -> Self {
        Self {
            width: format.width(),
            height: format.height(),
            frames_per_second: format.frame_rate(),
            pixel_format: format.format().into(),
        }
    }
}

impl From<WebcamFormat> for CameraFormat {
    fn from(format: WebcamFormat) -> Self {
        Self::new(
            Resolution::new(format.width, format.height),
            format.pixel_format.into(),
            format.frames_per_second,
        )
    }
}

/// Rule used by `nokhwa` to select a supported webcam format.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum WebcamFormatPreference {
    HighestResolution,
    #[default]
    HighestFrameRate,
    Exact(WebcamFormat),
    Closest(WebcamFormat),
}

/// Webcam capture options.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WebcamCaptureConfig {
    pub format: WebcamFormatPreference,
}

impl WebcamCaptureConfig {
    fn requested_format(self) -> RequestedFormat<'static> {
        let requested = match self.format {
            WebcamFormatPreference::HighestResolution => {
                RequestedFormatType::AbsoluteHighestResolution
            }
            WebcamFormatPreference::HighestFrameRate => {
                RequestedFormatType::AbsoluteHighestFrameRate
            }
            WebcamFormatPreference::Exact(format) => RequestedFormatType::Exact(format.into()),
            WebcamFormatPreference::Closest(format) => RequestedFormatType::Closest(format.into()),
        };
        RequestedFormat::new::<RgbFormat>(requested)
    }
}

/// Discoverable webcam and its metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebcamSource {
    id: WebcamId,
    name: String,
    description: String,
    miscellaneous: String,
}

impl WebcamSource {
    /// Identifier accepted by the native webcam backend.
    pub fn id(&self) -> &WebcamId {
        &self.id
    }

    /// Human-readable device name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Backend-provided device description.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Backend-specific metadata, such as a symbolic link or AVFoundation ID.
    pub fn miscellaneous(&self) -> &str {
        &self.miscellaneous
    }

    /// Query every device format reported by this webcam.
    pub fn formats(&self) -> CaptureResult<Vec<WebcamFormat>> {
        let request = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
        let mut camera = Camera::new(CameraIndex::from(&self.id), request)?;
        Ok(camera
            .compatible_camera_formats()?
            .into_iter()
            .map(WebcamFormat::from)
            .collect())
    }

    /// Open and start this webcam using `config`.
    pub fn open(&self, config: WebcamCaptureConfig) -> CaptureResult<WebcamCapture> {
        let mut camera = Camera::new(CameraIndex::from(&self.id), config.requested_format())?;
        camera.open_stream()?;
        Ok(WebcamCapture { camera })
    }

    /// Open this webcam using best reported realtime format within given
    /// dimensions and frame rate.
    pub fn open_bounded(
        &self,
        max_width: u32,
        max_height: u32,
        max_frames_per_second: u32,
    ) -> CaptureResult<WebcamCapture> {
        let format = bounded_format(
            &self.formats()?,
            max_width,
            max_height,
            max_frames_per_second,
        )
        .ok_or(crate::CaptureError::NoUsableWebcamFormat)?;
        self.open(WebcamCaptureConfig {
            format: WebcamFormatPreference::Exact(format),
        })
    }
}

fn bounded_format(
    formats: &[WebcamFormat],
    max_width: u32,
    max_height: u32,
    max_frames_per_second: u32,
) -> Option<WebcamFormat> {
    let fits_dimensions =
        |format: &&WebcamFormat| format.width <= max_width && format.height <= max_height;
    let area = |format: &WebcamFormat| u64::from(format.width) * u64::from(format.height);

    formats
        .iter()
        .filter(fits_dimensions)
        // Prefer closest device cadence. A slightly faster mode is better
        // than a much slower one because latest-frame pacing drops excess.
        .min_by_key(|format| {
            (
                format.frames_per_second.abs_diff(max_frames_per_second),
                std::cmp::Reverse(area(format)),
            )
        })
        .copied()
        .or_else(|| {
            // Unusual device exposing only larger modes: choose smallest one,
            // then software downscaling enforces dimensions.
            formats
                .iter()
                .min_by_key(|format| {
                    (
                        area(format),
                        format.frames_per_second.abs_diff(max_frames_per_second),
                    )
                })
                .copied()
        })
}

/// Ask `nokhwa` to initialize webcam access.
///
/// Call before webcam discovery on macOS. Callback receives permission result.
/// Other native platforms invoke callback with `true` immediately.
pub fn initialize_webcams(on_complete: impl Fn(bool) + Send + Sync + 'static) {
    nokhwa::nokhwa_initialize(on_complete);
}

/// Whether webcam permission initialization has completed successfully.
pub fn has_webcam_permission() -> bool {
    nokhwa::nokhwa_check()
}

/// Discover webcams using current platform's native backend.
pub fn webcams() -> CaptureResult<Vec<WebcamSource>> {
    Ok(nokhwa::query(ApiBackend::Auto)?
        .into_iter()
        .map(|info| WebcamSource {
            id: WebcamId::from(info.index()),
            name: info.human_name(),
            description: info.description().to_owned(),
            miscellaneous: info.misc(),
        })
        .collect())
}

/// Open webcam stream producing decoded RGB8 frames.
pub struct WebcamCapture {
    camera: Camera,
}

impl WebcamCapture {
    /// Device format selected by `nokhwa`.
    pub fn format(&self) -> WebcamFormat {
        self.camera.camera_format().into()
    }

    /// Capture and decode next webcam frame. Blocks until frame arrives.
    pub fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let frame_timing = timing_enabled.then(VideoFrameTiming::start);
        let total_started = timing_enabled.then(Instant::now);
        let wait_started = timing_enabled.then(Instant::now);
        let buffer = self.camera.frame()?;
        let wait_us = wait_started.map(elapsed_micros).unwrap_or_default();
        let resolution = buffer.resolution();
        let source_bytes = buffer.buffer().len();
        let source_format = buffer.source_frame_format();
        let decode_started = timing_enabled.then(Instant::now);
        let data = buffer.decode_image::<RgbFormat>()?.into_raw();
        let frame = VideoFrame::with_timing(
            resolution.width(),
            resolution.height(),
            PixelData::Rgb8(data),
            frame_timing,
        );
        frame.validate()?;
        if let Some(total_started) = total_started {
            tracing::trace!(
                target: "video_timing",
                stage = "webcam.capture",
                frame_id = frame_timing.map_or(0, VideoFrameTiming::id),
                pipeline_elapsed_us = frame_timing
                    .map_or(0, VideoFrameTiming::elapsed_micros),
                elapsed_us = elapsed_micros(total_started),
                wait_us,
                decode_us = decode_started.map(elapsed_micros).unwrap_or_default(),
                source_width = resolution.width(),
                source_height = resolution.height(),
                source_bytes,
                source_format = ?source_format,
                output_width = frame.width,
                output_height = frame.height,
                output_bytes = frame.pixel_data.as_bytes().len(),
                "video stage complete"
            );
        }
        Ok(frame)
    }
}

impl FrameCapture for WebcamCapture {
    fn next_frame(&mut self) -> CaptureResult<VideoFrame> {
        Self::next_frame(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webcam_format_round_trips() {
        let format = WebcamFormat {
            width: 1920,
            height: 1080,
            frames_per_second: 60,
            pixel_format: WebcamPixelFormat::Mjpeg,
        };

        assert_eq!(WebcamFormat::from(CameraFormat::from(format)), format);
    }

    #[test]
    fn bounded_format_prefers_realtime_rate_then_resolution() {
        let format = |width, height, frames_per_second| WebcamFormat {
            width,
            height,
            frames_per_second,
            pixel_format: WebcamPixelFormat::Mjpeg,
        };
        let formats = [
            format(1920, 1080, 60),
            format(1280, 720, 30),
            format(640, 480, 60),
            format(640, 480, 30),
        ];

        assert_eq!(
            bounded_format(&formats, 1280, 720, 30),
            Some(format(1280, 720, 30))
        );
    }

    #[test]
    fn bounded_format_falls_back_to_smallest_oversized_mode() {
        let format = |width, height| WebcamFormat {
            width,
            height,
            frames_per_second: 30,
            pixel_format: WebcamPixelFormat::Yuyv,
        };
        let formats = [format(1920, 1080), format(1280, 720)];

        assert_eq!(
            bounded_format(&formats, 640, 360, 30),
            Some(format(1280, 720))
        );
    }

    #[test]
    fn bounded_format_uses_closest_device_rate() {
        let format = |frames_per_second| WebcamFormat {
            width: 640,
            height: 480,
            frames_per_second,
            pixel_format: WebcamPixelFormat::Mjpeg,
        };
        let formats = [format(15), format(30)];

        assert_eq!(bounded_format(&formats, 960, 540, 24), Some(format(30)));
    }
}
