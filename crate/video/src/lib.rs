#![feature(portable_simd)]

//! Raw video frames, latest-frame channels, and reusable codec wrappers.

#[cfg(feature = "av1")]
mod av1;
mod channel;
mod error;
mod frame;
#[cfg(feature = "h264")]
mod h264;

#[cfg(feature = "av1")]
pub use av1::{Av1Encoder, Av1EncoderUsage, MAX_AV1_TEMPORAL_UNIT_BYTES};
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
}
