use std::{
    error::Error,
    fmt::Debug,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use cpal::{
    ChannelCount, SampleFormat, SampleRate,
    traits::{DeviceTrait as _, StreamTrait},
};
use ringbuf::{
    CachingCons, CachingProd, StaticRb,
    traits::{Consumer as _, Observer as _, Producer, Split},
};
use tracing::{error, info};

use crate::{
    AudioMixer, AudioSampleType, CHANNEL_BUFFER_SIZE, Channel, ChannelType, Notify, SampleConsumer,
    SampleProducer, SampleRb,
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

pub enum OutputRxEvent {
    AddOutputChannel(SampleConsumer<CHANNEL_BUFFER_SIZE>),
}
impl Debug for OutputRxEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddOutputChannel(_) => f.debug_tuple("AddOutputChannel").finish(),
        }
    }
}
pub enum TxEvent {
    Close,
}

impl AudioMixer {
    pub fn create_output_channel(
        &mut self,
        channel_mode: ChannelCount,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> Result<Output<CHANNEL_BUFFER_SIZE>, Box<dyn Error>> {
        let (channel, producer) = Channel::<_, Output<CHANNEL_BUFFER_SIZE>>::new(
            channel_mode,
            sample_format,
            sample_rate,
        );

        if let Some(master) = &mut self.output
            && let Some(stream) = &mut master.stream
        {
            let sample_consumer = CachingCons::new(channel.rb.clone());
            stream
                .to_audio_thread
                .try_push(OutputRxEvent::AddOutputChannel(sample_consumer))
                .map_err(|err| format!("Could not exec: {:?}", err))?;
        }
        self.output_channels.push(channel);

        Ok(producer)
    }

    pub fn start_stream_output(&mut self) -> Option<Notify> {
        if let Some(output) = &mut self.output {
            let config = output.device.default_output_config().unwrap();
            let mut stream_config = config.config();

            stream_config.sample_rate = 48_000; // TODO: Determene by device preference in future

            info!("Starting output stream with config: {:#?}", stream_config);

            let mut sample_consumers = self
                .output_channels
                .iter()
                .map(|channel| CachingCons::new(channel.rb.clone()))
                .collect::<Vec<SampleConsumer<CHANNEL_BUFFER_SIZE>>>();

            let (prod, mut cons) = StaticRb::default().split();
            let stream_close_notification = Notify::new();
            let send_stream_close_notification = stream_close_notification.clone();
            let stream = output
                .device
                .build_output_stream(
                    &stream_config,
                    move |data: &mut [AudioSampleType], _| {
                        for event in cons.pop_iter() {
                            match event {
                                OutputRxEvent::AddOutputChannel(cons) => {
                                    sample_consumers.push(cons);
                                }
                            };
                        }
                        sample_consumers.retain(|consumer| consumer.write_is_held());
                        if sample_consumers.is_empty() {
                            send_stream_close_notification.notify();
                        }
                        // Mix audio from all channels
                        for stream_sample in data.iter_mut() {
                            *stream_sample = sample_consumers
                                .iter_mut()
                                .map(|consumer| consumer.try_pop().unwrap_or(0.0))
                                .sum();
                        }
                        println!("{:?}", data);
                    },
                    move |err| {
                        error!("Audio stream error: {err:?}");
                    },
                    None,
                )
                .unwrap();

            stream.play().unwrap();
            output.stream = Some(crate::OutputStream {
                stream,
                to_audio_thread: prod,
            });
            return Some(stream_close_notification);
        }
        None
    }

    pub fn stop_stream_output(&mut self) {
        if let Some(output) = &mut self.output {
            output.stream = None;
        }
    }
}
