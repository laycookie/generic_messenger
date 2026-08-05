#![feature(portable_simd)]

use std::sync::{Arc, atomic::AtomicBool};

pub use cpal::{ChannelCount, SampleFormat, SampleRate};
use cpal::{
    SupportedStreamConfig,
    traits::{DeviceTrait as _, HostTrait as _},
};
use ringbuf::{StaticRb, traits::Observer as _, wrap::caching::Caching};

pub use asyncs_sync::Notify;

use crate::{
    input::InputRxEvent,
    output::OutputRxEvent,
    stream::{Master, open_master},
};

pub mod effects;
pub mod effects_rack;
mod graph;
pub mod input;
mod limiter;
pub mod output;
pub mod params;
mod resample;
mod simd;
mod stream;

/// Scalar and SIMD kernels exposed only for differential benchmarks.
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_internals {
    pub fn add_assign_scalar(output: &mut [f32], input: &[f32]) {
        crate::simd::add_assign_scalar(output, input);
    }

    pub fn add_assign_simd(output: &mut [f32], input: &[f32]) {
        crate::simd::add_assign(output, input);
    }

    #[must_use]
    pub fn max_abs_scalar(samples: &[f32]) -> f32 {
        crate::simd::max_abs_scalar(samples)
    }

    #[must_use]
    pub fn max_abs_simd(samples: &[f32]) -> f32 {
        crate::simd::max_abs(samples)
    }

    pub fn multiply_scalar(samples: &mut [f32], multiplier: f32) {
        crate::simd::multiply_scalar(samples, multiplier);
    }

    pub fn multiply_simd(samples: &mut [f32], multiplier: f32) {
        crate::simd::multiply(samples, multiplier);
    }

    #[must_use]
    pub fn all_magnitude_le_scalar(samples: &[f32], limit: f32) -> bool {
        crate::simd::all_magnitude_le_scalar(samples, limit)
    }

    #[must_use]
    pub fn all_magnitude_le_simd(samples: &[f32], limit: f32) -> bool {
        crate::simd::all_magnitude_le(samples, limit)
    }
}

pub type AudioSampleType = f32;

pub(crate) type SampleRb<const N: usize> = Arc<StaticRb<AudioSampleType, N>>;
pub(crate) type SampleProd<const N: usize> = Caching<SampleRb<N>, true, false>;
pub(crate) type SampleConsum<const N: usize> = Caching<SampleRb<N>, false, true>;

/// Number of samples in audio channel ring buffers (~100ms at 48kHz stereo).
pub const CHANNEL_BUFFER_SIZE: usize = 9600;

/// Samples per fixed scratch buffer used inside the real-time audio callbacks.
pub(crate) const MIX_SCRATCH_LEN: usize = 1024;
/// Samples per staging buffer used for format conversion on the control side.
pub(crate) const CONVERT_SCRATCH_LEN: usize = 4096;
/// Hard per-direction channel cap. Callback vectors reserve this exact bound,
/// so adopting a channel never reallocates on the real-time audio thread.
pub(crate) const MAX_AUDIO_CHANNELS: usize = 32;

/// The format an application streams audio in: what it pushes into an output
/// channel, or wants to receive from an input channel. The mixer converts
/// between this and the device's preferred format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamFormat {
    pub channels: ChannelCount,
    pub sample_format: SampleFormat,
    pub sample_rate: SampleRate,
}
impl StreamFormat {
    pub(crate) fn of_device(config: &SupportedStreamConfig) -> Self {
        StreamFormat {
            channels: config.channels(),
            sample_format: config.sample_format(),
            sample_rate: config.sample_rate(),
        }
    }
}

pub struct AudioMixer {
    output: Option<Master<OutputRxEvent>>,
    input: Option<Master<InputRxEvent>>,
    /// Rings of all live channels. Holding an `Arc` here guarantees the audio
    /// thread never drops the last reference to a ring; deallocation happens
    /// on the control thread when these lists are pruned.
    output_channels: Vec<SampleRb<CHANNEL_BUFFER_SIZE>>,
    input_channels: Vec<(SampleRb<CHANNEL_BUFFER_SIZE>, Arc<Notify>, Arc<AtomicBool>)>,
}

impl Default for AudioMixer {
    fn default() -> Self {
        let host = cpal::default_host();
        AudioMixer {
            output: open_master(
                host.default_output_device(),
                |device| device.default_output_config(),
                "Output",
            ),
            input: open_master(
                host.default_input_device(),
                |device| device.default_input_config(),
                "Input",
            ),
            output_channels: Vec::new(),
            input_channels: Vec::new(),
        }
    }
}

impl AudioMixer {
    pub fn is_streaming_output(&self) -> bool {
        self.output
            .as_ref()
            .is_some_and(|output| output.is_streaming())
    }

    pub fn is_streaming_input(&self) -> bool {
        self.input
            .as_ref()
            .is_some_and(|input| input.is_streaming())
    }

    /// Whether any playback producer is still alive after pruning stale rings.
    pub fn has_live_output_channels(&mut self) -> bool {
        self.prune_output_channels();
        !self.output_channels.is_empty()
    }

    /// Whether any capture consumer is still alive after pruning stale rings.
    pub fn has_live_input_channels(&mut self) -> bool {
        self.prune_input_channels();
        !self.input_channels.is_empty()
    }

    /// Drop bookkeeping for channels that are dead on both ends. Pruning only
    /// when no end is held makes this control-thread drop the deallocating
    /// one, so ring memory is never freed on the audio thread.
    pub(crate) fn prune_output_channels(&mut self) {
        self.output_channels
            .retain(|rb| rb.write_is_held() || rb.read_is_held());
    }

    pub(crate) fn prune_input_channels(&mut self) {
        self.input_channels
            .retain(|(rb, _, _)| rb.write_is_held() || rb.read_is_held());
    }
}
