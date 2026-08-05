#![feature(portable_simd)]

//! Cross-platform webcam and screen capture producing [`simple_video`] frames.
//!
//! [`nokhwa`] handles webcam discovery and capture. [`scap`] handles display
//! and window capture. Both backends are normalized to decoded [`VideoFrame`]
//! values, keeping device formats and platform APIs out of application code.

mod capture;
mod error;
mod screen;
mod webcam;

pub use capture::{
    Capture, CaptureLimits, CaptureSource, FrameCapture, capture_to_channel,
    capture_to_channel_bounded, sources,
};
pub use error::{CaptureError, CaptureResult};
pub use screen::{
    CropArea, ScreenCapture, ScreenCaptureConfig, ScreenResolution, ScreenSource, ScreenSourceKind,
    has_screen_capture_permission, request_screen_capture_permission, screen_capture_supported,
    screen_sources,
};
pub use simple_video::{
    FrameConsumer, FrameProducer, PixelData, VideoFrame, VideoFrameTiming, frame_channel,
};
pub use webcam::{
    WebcamCapture, WebcamCaptureConfig, WebcamFormat, WebcamFormatPreference, WebcamId,
    WebcamPixelFormat, WebcamSource, has_webcam_permission, initialize_webcams, webcams,
};

/// Scalar and SIMD candidates exposed only for differential benchmarks.
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_internals {
    pub fn packed_4_to_rgb_scalar(source: &[u8], output: &mut Vec<u8>, channels: [usize; 3]) {
        crate::screen::packed_4_to_rgb_scalar(source, output, channels);
    }

    pub fn packed_4_to_rgb_simd(source: &[u8], output: &mut Vec<u8>, channels: [usize; 3]) {
        crate::screen::packed_4_to_rgb_simd(source, output, channels);
    }

    pub fn bgr_to_rgb_scalar(pixels: &mut [u8]) {
        crate::screen::bgr_to_rgb_scalar(pixels);
    }

    pub fn bgr_to_rgb_simd(pixels: &mut [u8]) {
        crate::screen::bgr_to_rgb_simd(pixels);
    }
}
