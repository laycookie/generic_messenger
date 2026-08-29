use std::{fmt, time::Instant};

use crate::{PixelData, VideoFrame, VideoFrameTiming, timing::elapsed_micros};
use nokhwa::{
    Camera,
    pixel_format::RgbFormat,
    utils::{
        ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
        Resolution,
    },
};
#[cfg(target_os = "linux")]
use v4l::{
    capability::Flags as V4lCapabilities, device::Device as V4lDevice, format::FourCC,
    video::Capture as V4lCapture,
};

use super::{CaptureError, CaptureResult, FrameCapture};

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

impl WebcamFormat {
    /// Validate geometry and cadence before asking a native backend to use it.
    pub fn validate(self) -> CaptureResult<()> {
        if self.width == 0 || self.height == 0 || self.frames_per_second == 0 {
            return Err(CaptureError::InvalidConfiguration(
                "webcam format dimensions and frame rate must be greater than zero",
            ));
        }
        Ok(())
    }
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
    /// Validate an exact or closest format request before opening a backend.
    pub fn validate(self) -> CaptureResult<()> {
        match self.format {
            WebcamFormatPreference::Exact(format) | WebcamFormatPreference::Closest(format) => {
                format.validate()
            }
            WebcamFormatPreference::HighestResolution
            | WebcamFormatPreference::HighestFrameRate => Ok(()),
        }
    }

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
        config.validate()?;
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
        if max_width == 0 || max_height == 0 || max_frames_per_second == 0 {
            return Err(CaptureError::InvalidConfiguration(
                "webcam capture bounds and frame rate must be greater than zero",
            ));
        }
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

    let is_valid = |format: &&WebcamFormat| {
        format.width > 0 && format.height > 0 && format.frames_per_second > 0
    };

    formats
        .iter()
        .filter(is_valid)
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
                .filter(|format| {
                    format.width > 0 && format.height > 0 && format.frames_per_second > 0
                })
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

#[cfg(target_os = "linux")]
fn linux_webcam_is_usable(index: u32) -> Option<bool> {
    let Ok(device_index) = usize::try_from(index) else {
        return None;
    };
    let device = match V4lDevice::new(device_index) {
        Ok(device) => device,
        Err(error) => {
            tracing::debug!(
                camera.index = index,
                error = %error,
                "V4L webcam probe unavailable; retaining device conservatively"
            );
            return None;
        }
    };
    let capabilities = match device.query_caps() {
        Ok(capabilities) => capabilities.capabilities,
        Err(error) => {
            tracing::debug!(
                camera.index = index,
                error = %error,
                "V4L webcam capabilities unavailable; retaining device conservatively"
            );
            return None;
        }
    };
    // v4l 0.14 reads device_caps, which may be empty for legacy drivers.
    // A non-empty set without single-plane capture is definitive because
    // Nokhwa's Linux backend cannot stream multi-plane or metadata nodes.
    if !capabilities.is_empty() && !capabilities.contains(V4lCapabilities::VIDEO_CAPTURE) {
        return Some(false);
    }

    match device.enum_formats() {
        Ok(formats) => Some(
            formats
                .iter()
                .any(|format| nokhwa_supports_v4l_format(format.fourcc)),
        ),
        Err(error) => {
            tracing::debug!(
                camera.index = index,
                error = %error,
                "V4L webcam formats unavailable; retaining device conservatively"
            );
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn nokhwa_supports_v4l_format(format: FourCC) -> bool {
    matches!(
        &format.repr,
        b"YUYV" | b"MJPG" | b"GRAY" | b"RGB3" | b"BGR3" | b"NV12"
    )
}

#[cfg(target_os = "linux")]
fn normalize_linux_webcams_with(
    mut sources: Vec<WebcamSource>,
    mut is_usable: impl FnMut(u32) -> Option<bool>,
) -> Vec<WebcamSource> {
    sources.sort_by(|left, right| left.id.cmp(&right.id));
    sources
        .into_iter()
        .filter(|source| {
            let usable = match &source.id {
                WebcamId::Index(index) => is_usable(*index),
                WebcamId::String(_) => None,
            };
            if usable == Some(false) {
                tracing::debug!(
                    camera.id = %source.id,
                    camera.name = %source.name,
                    "unusable V4L webcam endpoint ignored"
                );
                false
            } else {
                true
            }
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn normalize_linux_webcams(sources: Vec<WebcamSource>) -> Vec<WebcamSource> {
    normalize_linux_webcams_with(sources, linux_webcam_is_usable)
}

/// Discover webcams which can produce frames supported by this crate.
///
/// Linux V4L metadata, multi-plane, and unsupported-format nodes are removed.
/// Nodes which cannot be probed are retained conservatively.
pub fn webcams() -> CaptureResult<Vec<WebcamSource>> {
    let sources: Vec<WebcamSource> = nokhwa::query(ApiBackend::Auto)?
        .into_iter()
        .map(|info| WebcamSource {
            id: WebcamId::from(info.index()),
            name: info.human_name(),
            description: info.description().to_owned(),
            miscellaneous: info.misc(),
        })
        .collect();
    #[cfg(target_os = "linux")]
    return Ok(normalize_linux_webcams(sources));
    #[cfg(not(target_os = "linux"))]
    Ok(sources)
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

    #[cfg(target_os = "linux")]
    fn webcam_source(index: u32, name: &str) -> WebcamSource {
        WebcamSource {
            id: WebcamId::Index(index),
            name: name.to_owned(),
            description: String::new(),
            miscellaneous: String::new(),
        }
    }

    #[test]
    fn exact_webcam_configuration_rejects_zero_geometry_or_cadence() {
        let config = WebcamCaptureConfig {
            format: WebcamFormatPreference::Exact(WebcamFormat {
                width: 0,
                height: 720,
                frames_per_second: 30,
                pixel_format: WebcamPixelFormat::Mjpeg,
            }),
        };
        assert!(matches!(
            config.validate(),
            Err(CaptureError::InvalidConfiguration(_))
        ));
    }

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
    fn bounded_format_ignores_invalid_backend_descriptors() {
        let invalid = WebcamFormat {
            width: 0,
            height: 720,
            frames_per_second: 30,
            pixel_format: WebcamPixelFormat::Mjpeg,
        };
        assert_eq!(bounded_format(&[invalid], 1280, 720, 30), None);
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

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_discovery_keeps_only_endpoints_that_can_produce_supported_frames() {
        let sources = vec![
            webcam_source(3, "USB Camera"),
            webcam_source(2, "USB Camera"),
            webcam_source(5, "Capture Card"),
            webcam_source(4, "USB Camera"),
            webcam_source(0, "USB Camera"),
            webcam_source(1, "USB Camera"),
        ];

        let normalized = normalize_linux_webcams_with(sources, |index| match index {
            0 | 4 => Some(true),
            1 | 2 | 3 | 5 => Some(false),
            _ => unreachable!("unexpected webcam index"),
        });

        assert_eq!(
            normalized
                .iter()
                .map(|source| source.id().clone())
                .collect::<Vec<_>>(),
            vec![WebcamId::Index(0), WebcamId::Index(4)]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_discovery_preserves_multiple_usable_endpoints() {
        let sources = vec![
            webcam_source(0, "USB Camera"),
            webcam_source(2, "USB Camera"),
        ];

        let normalized = normalize_linux_webcams_with(sources, |_| Some(true));

        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].id(), &WebcamId::Index(0));
        assert_eq!(normalized[1].id(), &WebcamId::Index(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_discovery_retains_unverified_endpoints() {
        let sources = vec![
            webcam_source(2, "USB Camera"),
            webcam_source(0, "USB Camera"),
        ];

        let normalized = normalize_linux_webcams_with(sources, |_| None);

        assert_eq!(
            normalized
                .iter()
                .map(|source| source.id().clone())
                .collect::<Vec<_>>(),
            vec![WebcamId::Index(0), WebcamId::Index(2)]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_format_filter_matches_nokhwa_decoder_support() {
        for supported in [b"YUYV", b"MJPG", b"GRAY", b"RGB3", b"BGR3", b"NV12"] {
            assert!(nokhwa_supports_v4l_format(FourCC::new(supported)));
        }
        assert!(!nokhwa_supports_v4l_format(FourCC::new(b"H264")));
    }
}
