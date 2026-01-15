use std::ops::{Deref, DerefMut};

use cpal::{
    ChannelCount, SampleFormat, SampleRate,
    traits::{DeviceTrait as _, StreamTrait},
};
use ringbuf::{
    CachingCons, CachingProd,
    traits::{Consumer as _, Observer as _},
};
use tracing::{error, info};

use crate::{
    AudioMixer, AudioSampleType, Channel, ChannelType, SampleConsumer, SampleProducer, SampleRb,
};

#[repr(transparent)]
pub struct Output<const N: usize>(SampleProducer<N>);
impl<const N: usize> ChannelType<N> for Output<N> {
    fn from(rb: SampleRb<N>) -> Self {
        Self(CachingProd::new(rb.clone()))
    }
}
impl<const N: usize> Deref for Output<N> {
    type Target = SampleProducer<N>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<const N: usize> DerefMut for Output<N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
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
    ) -> Output<5120> {
        let (channel, producer) =
            Channel::<_, Output<5120>>::new(channel_mode, sample_format, sample_rate);

        self.output_channels.push(channel);
        if let Some(master) = &self.output
        // && master.stream.is_none()
        {
            self.stop_stream_output();
            self.start_stream_output();
        };

        producer
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
