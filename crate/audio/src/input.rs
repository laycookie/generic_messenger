use std::{
    error::Error,
    fmt::Debug,
    ops::{Deref, DerefMut},
};

use cpal::{
    ChannelCount, SampleFormat, SampleRate,
    traits::{DeviceTrait, StreamTrait as _},
};
use ringbuf::{
    CachingCons, CachingProd, StaticRb,
    traits::{Consumer as _, Observer, Producer, Split as _},
};
use tracing::{error, info};

use crate::{
    AudioMixer, AudioSampleType, Channel, ChannelType, Notify, SampleConsumer, SampleProducer,
    SampleRb,
};

pub enum InputRxEvent {
    AddInputChannel(SampleProducer<5120>),
}
impl Debug for InputRxEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddInputChannel(_) => f.debug_tuple("AddInputChannel").finish(),
        }
    }
}

// TODO: Replace with a configurable noise gate/AGC once mic pipeline is finalized.
const NOISE_FLOOR: AudioSampleType = 0.002;

#[repr(transparent)]
pub struct Input<const N: usize>(SampleConsumer<N>);
impl<const N: usize> ChannelType<N> for Input<N> {
    fn from(rb: SampleRb<N>) -> Self {
        Self(CachingCons::new(rb.clone()))
    }
}
impl<const N: usize> Deref for Input<N> {
    type Target = SampleConsumer<N>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<const N: usize> DerefMut for Input<N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AudioMixer {
    pub fn create_input_channel(
        &mut self,
        channel_mode: ChannelCount,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> Result<Input<5120>, Box<dyn Error>> {
        let (channel, consumer) =
            Channel::<_, Input<5120>>::new(channel_mode, sample_format, sample_rate);

        if let Some(master) = &mut self.input
            && let Some(stream) = &mut master.stream
        {
            let sample_producer = CachingProd::new(channel.rb.clone());
            stream
                .to_audio_thread
                .try_push(InputRxEvent::AddInputChannel(sample_producer))
                .map_err(|err| format!("Could not exec: {:?}", err))?;
        }

        self.input_channels.push(channel);

        Ok(consumer)
    }

    pub fn start_stream_input(&mut self) -> Option<Notify> {
        if let Some(input) = &mut self.input {
            let config = input.device.default_input_config().unwrap();
            let mut stream_config = config.config();

            stream_config.sample_rate = 48_000; // TODO: Determine by device preference in future

            let (prod, mut cons) = StaticRb::default().split();
            let stream_close_notification = Notify::new();
            let send_stream_close_notification = stream_close_notification.clone();

            let mut sample_producers = self
                .input_channels
                .iter()
                .map(|channel| CachingProd::new(channel.rb.clone()))
                .collect::<Vec<SampleProducer<5120>>>();
            let stream = input
                .device
                .build_input_stream(
                    &stream_config,
                    move |data: &[AudioSampleType], &_| {
                        for event in cons.pop_iter() {
                            match event {
                                InputRxEvent::AddInputChannel(prod) => {
                                    sample_producers.push(prod);
                                }
                            };
                        }
                        sample_producers.retain(|producers| producers.read_is_held());
                        if sample_producers.is_empty() {
                            send_stream_close_notification.notify();
                        }

                        for sample_producer in sample_producers.iter_mut() {
                            // TODO: Noise floor should be filtered out by afx with a gate
                            // Compute the RMS (root mean square) of the frame to assess volume.
                            let frame_rms = {
                                let len = data.len() as f32;
                                if len == 0.0 {
                                    0.0
                                } else {
                                    let sum: f32 = data.iter().map(|sample| sample * sample).sum();
                                    (sum / len).sqrt()
                                }
                            };

                            // Only push if frame RMS exceeds NOISE_FLOOR
                            if frame_rms > NOISE_FLOOR {
                                info!("Playing");
                                sample_producer.push_iter(data.iter().copied());
                            }
                        }
                    },
                    move |err| {
                        error!("Audio stream error: {err:?}");
                    },
                    None,
                )
                .unwrap();

            stream.play().unwrap();
            input.stream = Some(crate::InputStream {
                stream,
                to_audio_thread: prod,
            });
            return Some(stream_close_notification);
        }

        None
    }

    pub fn stop_stream_input(&mut self) {
        if let Some(input) = &mut self.input {
            input.stream = None;
        }
    }
}
