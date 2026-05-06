use std::{error::Error, fmt::Debug, io};

use cpal::{
    ChannelCount, SampleFormat, SampleRate,
    traits::{DeviceTrait as _, StreamTrait},
};
use ringbuf::{
    CachingCons, CachingProd, StaticRb,
    traits::{Consumer as _, Observer as _, Producer, Split},
};
use tracing::{debug, error};

use crate::{
    AudioMixer, AudioSampleType, CHANNEL_BUFFER_SIZE, Channel, ChannelType, Notify, SampleConsum,
    SampleProd, SampleRb,
};

pub struct SampleProducer(SampleProd<CHANNEL_BUFFER_SIZE>);
impl SampleProducer {
    pub fn new(caching: SampleProd<CHANNEL_BUFFER_SIZE>) -> Self {
        Self(caching)
    }
    pub fn push_iter<I: Iterator<Item = AudioSampleType>>(&mut self, iter: I) -> usize {
        self.0.push_iter(iter)
    }
}

pub struct Output(SampleRb<CHANNEL_BUFFER_SIZE>);
impl ChannelType for Output {
    fn new() -> Self {
        Self(SampleRb::default())
    }
}
impl Output {
    pub fn push_iter<I: Iterator<Item = AudioSampleType>>(&self, iter: I) -> usize {
        CachingProd::new(self.0.clone()).push_iter(iter)
    }
}

pub enum OutputRxEvent {
    AddOutputChannel(SampleConsum<CHANNEL_BUFFER_SIZE>),
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
    ) -> Result<SampleProducer, Box<dyn Error>> {
        let channel = Channel::<Output>::new(channel_mode, sample_format, sample_rate);
        let producer = SampleProducer(CachingProd::new(channel.interface.0.clone()));

        if let Some(master) = &mut self.output
            && let Some(stream) = &mut master.stream
        {
            let sample_consumer = CachingCons::new(channel.interface.0.clone());
            stream
                .to_audio_thread
                .try_push(OutputRxEvent::AddOutputChannel(sample_consumer))
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("failed to push output channel event: {err:?}")))?;
        }
        self.output_channels.push(channel);

        Ok(producer)
    }

    pub fn start_stream_output(&mut self) -> Result<Option<Notify>, Box<dyn Error>> {
        let Some(output) = &mut self.output else {
            return Ok(None);
        };

        let config = output.device.default_output_config()?;
        let mut stream_config = config.config();

        stream_config.sample_rate = 48_000; // TODO: Determine by device preference in future

        debug!("Starting output stream with config: {:#?}", stream_config);

        let mut sample_consumers = self
            .output_channels
            .iter()
            .map(|channel| CachingCons::new(channel.interface.0.clone()))
            .collect::<Vec<SampleConsum<CHANNEL_BUFFER_SIZE>>>();

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
                            .sum::<AudioSampleType>()
                            .clamp(-1.0, 1.0);
                    }
                },
                move |err| {
                    error!("Audio stream error: {err:?}");
                },
                None,
            )?;

        stream.play()?;
        output.stream = Some(crate::OutputStream {
            stream,
            to_audio_thread: prod,
        });
        Ok(Some(stream_close_notification))
    }

    pub fn stop_stream_output(&mut self) {
        if let Some(output) = &mut self.output {
            output.stream = None;
        }
    }
}
