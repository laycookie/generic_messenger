use std::sync::Arc;

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
pub mod input;
pub mod output;
mod resample;
mod stream;

pub type AudioSampleType = f32;

pub(crate) type SampleRb<const N: usize> = Arc<StaticRb<AudioSampleType, N>>;
pub(crate) type SampleProd<const N: usize> = Caching<SampleRb<N>, true, false>;
pub(crate) type SampleConsum<const N: usize> = Caching<SampleRb<N>, false, true>;

/// Number of samples in audio channel ring buffers (~129ms at 48kHz stereo).
pub const CHANNEL_BUFFER_SIZE: usize = 12400;

/// Samples per fixed scratch buffer used inside the real-time audio callbacks.
pub(crate) const MIX_SCRATCH_LEN: usize = 1024;
/// Samples per staging buffer used for format conversion on the control side.
pub(crate) const CONVERT_SCRATCH_LEN: usize = 4096;
/// Extra capacity reserved in the callbacks' channel lists so that adding
/// channels normally does not allocate on the audio thread (exceeding the
/// headroom falls back to a reallocation).
pub(crate) const CHANNEL_HEADROOM: usize = 32;

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
    input_channels: Vec<(SampleRb<CHANNEL_BUFFER_SIZE>, Arc<Notify>)>,
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

    /// Drop bookkeeping for channels that are dead on both ends. Pruning only
    /// when no end is held makes this control-thread drop the deallocating
    /// one, so ring memory is never freed on the audio thread.
    pub(crate) fn prune_output_channels(&mut self) {
        self.output_channels
            .retain(|rb| rb.write_is_held() || rb.read_is_held());
    }

    pub(crate) fn prune_input_channels(&mut self) {
        self.input_channels
            .retain(|(rb, _)| rb.write_is_held() || rb.read_is_held());
    }
}
