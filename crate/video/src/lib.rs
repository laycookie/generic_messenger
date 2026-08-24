#![feature(portable_simd)]
#![cfg_attr(
    all(
        feature = "capture",
        any(target_os = "linux", target_os = "macos", target_os = "windows")
    ),
    doc = include_str!("../README.md")
)]

//! Cross-platform capture, latest-frame streams, raw frames, and codec wrappers.

#[cfg(feature = "av1")]
mod av1;
#[cfg(all(
    feature = "capture",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
mod capture;
mod channel;
mod error;
mod frame;
#[cfg(feature = "h264")]
mod h264;

#[cfg(feature = "av1")]
pub use av1::{Av1Encoder, Av1EncoderUsage, MAX_AV1_TEMPORAL_UNIT_BYTES};
#[cfg(all(
    feature = "capture",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
pub use capture::{
    Capture, CaptureCompletion, CaptureError, CaptureLimits, CaptureResult, CaptureSource,
    CaptureSourceKind, CaptureStream, CropArea, FrameCapture, ScreenCapture, ScreenCaptureConfig,
    ScreenResolution, ScreenSource, ScreenSourceKind, WebcamCapture, WebcamCaptureConfig,
    WebcamFormat, WebcamFormatPreference, WebcamId, WebcamPixelFormat, WebcamSource,
    capture_sources, capture_to_channel, capture_to_channel_bounded, has_screen_capture_permission,
    has_webcam_permission, initialize_webcams, request_screen_capture_permission,
    screen_capture_supported, screen_sources, sources, try_capture_sources, webcams,
};
pub use channel::{FrameConsumer, FrameProducer, frame_channel};
pub use error::VideoError;
pub use frame::{
    EncodedVideoFrame, FrameScaler, PixelData, VideoCodec, VideoFrame, VideoFrameTiming,
};
#[cfg(feature = "h264")]
pub use h264::{H264Decoder, H264Encoder, H264EncoderUsage, MAX_H264_ACCESS_UNIT_BYTES};

/// Scalar and SIMD kernels exposed only for differential benchmarks.
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_internals {
    pub fn rgb_to_rgba_scalar(source: &[u8], output: &mut Vec<u8>) {
        crate::frame::rgb_to_rgba_scalar(source, output);
    }

    pub fn rgb_to_rgba_simd(source: &[u8], output: &mut Vec<u8>) {
        crate::frame::rgb_to_rgba_simd(source, output);
    }

    #[cfg(feature = "av1")]
    pub fn rgb_to_yuv420_scalar(
        source: &[u8],
        width: usize,
        height: usize,
        bytes_per_pixel: usize,
        y: &mut [u8],
        u: &mut [u8],
        v: &mut [u8],
    ) {
        crate::av1::convert_contiguous_scalar(source, width, height, bytes_per_pixel, y, u, v);
    }

    #[cfg(feature = "av1")]
    pub fn rgb_to_yuv420_simd(
        source: &[u8],
        width: usize,
        height: usize,
        bytes_per_pixel: usize,
        y: &mut [u8],
        u: &mut [u8],
        v: &mut [u8],
    ) {
        crate::av1::convert_contiguous_simd(source, width, height, bytes_per_pixel, y, u, v);
    }

    #[cfg(all(
        feature = "capture",
        any(target_os = "linux", target_os = "macos", target_os = "windows")
    ))]
    pub fn packed_4_to_rgb_scalar(source: &[u8], output: &mut Vec<u8>, channels: [usize; 3]) {
        crate::capture::packed_4_to_rgb_scalar(source, output, channels);
    }

    #[cfg(all(
        feature = "capture",
        any(target_os = "linux", target_os = "macos", target_os = "windows")
    ))]
    pub fn packed_4_to_rgb_simd(source: &[u8], output: &mut Vec<u8>, channels: [usize; 3]) {
        crate::capture::packed_4_to_rgb_simd(source, output, channels);
    }

    #[cfg(all(
        feature = "capture",
        any(target_os = "linux", target_os = "macos", target_os = "windows")
    ))]
    pub fn bgr_to_rgb_scalar(pixels: &mut [u8]) {
        crate::capture::bgr_to_rgb_scalar(pixels);
    }

    #[cfg(all(
        feature = "capture",
        any(target_os = "linux", target_os = "macos", target_os = "windows")
    ))]
    pub fn bgr_to_rgb_simd(pixels: &mut [u8]) {
        crate::capture::bgr_to_rgb_simd(pixels);
    }
}
