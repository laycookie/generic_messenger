use std::{marker::PhantomData, sync::Arc};

use cpal::{
    ChannelCount,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
pub use cpal::{SampleFormat, SampleRate};
use ringbuf::{
    CachingCons, CachingProd,
    traits::{Consumer as _, Observer},
    wrap::caching::Caching,
};
use tracing::{error, info};

pub use ringbuf::traits::Producer;

pub type AudioSampleType = f32;

// === Effects ===
pub trait Afx {
    fn apply_to(&mut self, audio: &mut [AudioSampleType]);
}

struct Gain {
    multiplier: f32,
}
impl Afx for Gain {
    fn apply_to(&mut self, audio: &mut [AudioSampleType]) {
        for sample in audio.iter_mut() {
            *sample *= self.multiplier;
        }
    }
}

// ===============================
type SampleRb<const N: usize> =
    Arc<ringbuf::SharedRb<ringbuf::storage::Owning<[std::mem::MaybeUninit<AudioSampleType>; N]>>>;
pub type SampleProducer<const N: usize> = Caching<SampleRb<N>, true, false>;
pub type SampleConsumer<const N: usize> = Caching<SampleRb<N>, false, true>;

struct Output;
struct Input;
struct Channel<const N: usize, T> {
    // Channel metadata
    channel_count: ChannelCount, // Mono(1), Setro(2), etc.
    sample_format: SampleFormat,
    sample_rate: SampleRate,
    effects: Vec<Box<dyn Afx + Send + Sync>>,
    rb: SampleRb<N>,
    _marker: PhantomData<T>,
}
impl<const N: usize, T> Channel<N, T> {
    fn add_effect(&mut self, new_effect: Box<dyn Afx + Send + Sync>) {
        self.effects.push(new_effect);
    }
}
impl<const N: usize> Channel<N, Output> {
    fn new(
        channel_mode: ChannelCount,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> (Self, SampleProducer<N>) {
        let rb: SampleRb<N> = ringbuf::SharedRb::default().into();
        let producer = CachingProd::new(rb.clone());

        let channel = Channel {
            channel_count: channel_mode,
            sample_format,
            sample_rate,
            effects: Vec::new(),
            rb,
            _marker: PhantomData,
        };

        (channel, producer)
    }
}
impl<const N: usize> Channel<N, Input> {
    fn new(
        channel_mode: ChannelCount,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> (Self, SampleConsumer<N>) {
        let rb: SampleRb<N> = ringbuf::SharedRb::default().into();
        let consumer = CachingCons::new(rb.clone());

        let channel = Channel {
            channel_count: channel_mode,
            sample_format,
            sample_rate,
            effects: Vec::new(),
            rb,
            _marker: PhantomData,
        };

        (channel, consumer)
    }
}

// ==================================
struct Master {
    device: cpal::Device,
    stream: Option<cpal::Stream>,
}

pub struct AudioMixer {
    output: Option<Master>,
    input: Option<Master>,
    output_channels: Vec<Channel<5120, Output>>,
}
impl Default for AudioMixer {
    fn default() -> Self {
        let mut audio_mixer = AudioMixer {
            output: None,
            input: None,
            output_channels: Vec::new(),
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

impl AudioMixer {
    /// Creates a new output channel and returns a producer for writing audio data.
    ///
    /// Note: Channels must be created before the audio stream starts. Once the stream
    /// is running, newly created channels will not be included in the output mix.
    /// The stream starts automatically when the first channel is created.
    pub fn create_output_channel(
        &mut self,
        channel_mode: ChannelCount,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> SampleProducer<5120> {
        let (channel, producer) =
            Channel::<_, Output>::new(channel_mode, sample_format, sample_rate);

        self.output_channels.push(channel);
        if let Some(master) = &self.output
        // && master.stream.is_none()
        {
            self.stop_stream_output();
            self.start_stream_output();
        };

        producer
    }

    pub fn add_effect_to_channel(
        &mut self,
        channel_index: usize,
        effect: Box<dyn Afx + Send + Sync>,
    ) {
        if let Some(channel) = self.output_channels.get_mut(channel_index) {
            channel.add_effect(effect);
        }
    }

    fn start_stream_output(&mut self) {
        if let Some(output) = &self.output
            && output.stream.is_none()
            && !self.output_channels.is_empty()
        {
            let config = output.device.default_output_config().unwrap();
            let mut stream_config = config.config();

            stream_config.sample_rate = 48_000; // TODO: Determene by device preference in future

            info!("Starting output stream with config: {:#?}", stream_config);
            let mut sample_consumers = self
                .output_channels
                .iter()
                .map(|channel| CachingCons::new(channel.rb.clone()))
                .collect::<Vec<SampleConsumer<5120>>>();
            let stream = output
                .device
                .build_output_stream(
                    &stream_config,
                    move |data: &mut [AudioSampleType], _| {
                        sample_consumers.retain(|consumer| consumer.write_is_held());
                        info!("{}", sample_consumers.len());
                        if sample_consumers.is_empty() {
                            info!("TODO: Close stream");
                        }
                        // Mix audio from all channels
                        for stream_sample in data {
                            *stream_sample = sample_consumers
                                .iter_mut()
                                .map(|consumer| consumer.try_pop().unwrap_or(0.0))
                                .sum();
                        }
                    },
                    move |err| {
                        error!("Audio stream error: {err:?}");
                    },
                    None,
                )
                .unwrap();

            stream.play().unwrap();
            if let Some(output) = self.output.as_mut() {
                output.stream = Some(stream);
            }
        }
    }

    fn stop_stream_output(&mut self) {
        if let Some(output) = &mut self.output {
            output.stream = None;
        }
    }
}
