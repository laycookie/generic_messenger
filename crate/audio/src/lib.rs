use std::{marker::PhantomData, ops::DerefMut, sync::Arc};

use cpal::{ChannelCount, traits::HostTrait};
pub use cpal::{SampleFormat, SampleRate};
use ringbuf::wrap::caching::Caching;

pub use ringbuf::traits::Producer;

use crate::{effects::Afx, input::Input, output::Output};

pub mod effects;
pub mod input;
pub mod output;

type SampleRb<const N: usize> =
    Arc<ringbuf::SharedRb<ringbuf::storage::Owning<[std::mem::MaybeUninit<AudioSampleType>; N]>>>;
type SampleProducer<const N: usize> = Caching<SampleRb<N>, true, false>;
type SampleConsumer<const N: usize> = Caching<SampleRb<N>, false, true>;

pub type AudioSampleType = f32;

struct Channel<const N: usize, C: ChannelType<N>> {
    channel_count: ChannelCount, // Mono(1), Setro(2), etc.
    sample_format: SampleFormat,
    sample_rate: SampleRate,
    effects: Vec<Box<dyn Afx + Send + Sync>>,
    rb: SampleRb<N>,
    _marker: PhantomData<C>,
}

trait ChannelType<const N: usize>: DerefMut {
    fn from(rb: SampleRb<N>) -> Self;
}

impl<const N: usize, C: ChannelType<N>> Channel<N, C> {
    fn new(
        channel_mode: ChannelCount,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> (Channel<N, C>, C) {
        let rb: SampleRb<N> = ringbuf::SharedRb::default().into();
        let c = C::from(rb.clone());

        let channel = Channel {
            channel_count: channel_mode,
            sample_format,
            sample_rate,
            effects: Vec::new(),
            rb,
            _marker: PhantomData,
        };
        (channel, c)
    }
}

struct Master {
    device: cpal::Device,
    stream: Option<cpal::Stream>,
}

pub struct AudioMixer {
    output: Option<Master>,
    input: Option<Master>,
    output_channels: Vec<Channel<5120, Output<5120>>>,
    input_channels: Vec<Channel<5120, Input<5120>>>,
}
impl Default for AudioMixer {
    fn default() -> Self {
        let mut audio_mixer = AudioMixer {
            output: None,
            input: None,
            output_channels: Vec::new(),
            input_channels: Vec::new(),
        };

        let host = cpal::default_host();

        if let Some(output) = host.default_output_device() {
            let main = Master {
                device: output,
                stream: None,
            };
            audio_mixer.output = Some(main);
        }
        if let Some(input) = host.default_input_device() {
            let main = Master {
                device: input,
                stream: None,
            };
            audio_mixer.input = Some(main);
        }

        audio_mixer
    }
}
