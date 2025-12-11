use std::sync::{Arc, Mutex};

use cpal::{
    Sample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
pub use cpal::{SampleFormat, SampleRate};
use crossbeam::atomic::AtomicCell;
use ringbuf::{
    StaticRb,
    traits::{Consumer as _, Observer, Producer as _, Split},
    wrap::caching::Caching,
};

// ===============================
type Prod<const N: usize> = Caching<
    ringbuf::Arc<
        ringbuf::SharedRb<ringbuf::storage::Owning<[std::mem::MaybeUninit<AudioSampleType>; N]>>,
    >,
    true,
    false,
>;
type Consum<const N: usize> = Caching<
    ringbuf::Arc<
        ringbuf::SharedRb<ringbuf::storage::Owning<[std::mem::MaybeUninit<AudioSampleType>; N]>>,
    >,
    false,
    true,
>;

pub struct Producer<const N: usize> {
    pub producer: Prod<N>,
    active: Arc<AtomicCell<bool>>,
}
impl<const N: usize> Producer<N> {
    pub fn push_iter<S: Sample, I: Iterator<Item = S>>(&mut self, iter: I) -> usize
    where
        f32: cpal::FromSample<S>,
    {
        self.producer
            .push_iter(iter.map(|sample| sample.to_sample()))
    }
}

impl<const N: usize> Drop for Producer<N> {
    fn drop(&mut self) {
        self.active.store(false);
    }
}
struct Consumer<const N: usize> {
    active: Arc<AtomicCell<bool>>,
    consumer: Consum<N>,
}

struct Channel<const N: usize> {
    channel_mode: u8,
    sample_format: SampleFormat,
    sample_rate: SampleRate,
    effects: f32,
    sample_consumer: Consumer<N>,
}
impl<const N: usize> Channel<N> {
    fn new(
        channel_mode: u8,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> (Channel<N>, Producer<N>) {
        let buffer = StaticRb::default();
        let (producer, consumer) = buffer.split();

        let active = Arc::new(AtomicCell::new(true));

        (
            Self {
                channel_mode,
                sample_format,
                sample_rate,
                effects: 1.0,
                sample_consumer: Consumer {
                    active: active.clone(),
                    consumer,
                },
            },
            Producer { producer, active },
        )
    }
}

// ==================================

pub type AudioSampleType = f32;
#[derive(Default)]
pub struct AudioMixer {
    // Devices Selected
    output_device: Option<cpal::Device>,
    // Streams
    output_stream: Option<cpal::Stream>,
    // Mixer
    channels: Arc<Mutex<Vec<Channel<5120>>>>,
}

impl AudioMixer {
    pub fn new() -> Self {
        let mut audio_mixer = AudioMixer::default();

        let host = cpal::default_host();
        if let Some(output) = host.default_output_device() {
            audio_mixer.output_device = Some(output);
        }

        audio_mixer
    }
    pub fn create_channel(
        &mut self,
        channel_mode: u8,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> Producer<5120> {
        let (channel, producer) = Channel::new(channel_mode, sample_format, sample_rate);
        self.channels.lock().unwrap().push(channel);

        self.start_stream_output();

        producer
    }

    fn start_stream_output(&mut self) {
        if let None = self.output_stream
            && let Some(output) = &self.output_device
            && !self.channels.lock().unwrap().is_empty()
        {
            let config = output.default_output_config().unwrap();
            let mut config = config.config();
            config.sample_rate = SampleRate(48_000); // TODO
            println!("{:#?}", config);

            let channels = self.channels.clone();
            let stream = output
                .build_output_stream(
                    &config,
                    move |data: &mut [AudioSampleType], _| {
                        let mut channels = channels.lock().unwrap();

                        for stream_sample in data {
                            *stream_sample = channels
                                .iter_mut()
                                .map(|channel| {
                                    channel.sample_consumer.consumer.try_pop().unwrap_or(0.0)
                                })
                                .sum::<AudioSampleType>();
                        }
                        println!("{:?}", channels.len());
                    },
                    move |err| {
                        eprintln!("{err:?}");
                    },
                    None,
                )
                .unwrap();

            stream.play().unwrap();
            self.output_stream = Some(stream);
        };
    }
}
