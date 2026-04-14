use std::{error::Error, fmt::Debug};

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
    AudioMixer, AudioSampleType, CHANNEL_BUFFER_SIZE, Channel, ChannelType, Notify, SampleConsum,
    SampleProd, SampleRb,
};

pub enum InputRxEvent {
    AddInputChannel(SampleProd<CHANNEL_BUFFER_SIZE>, Notify),
}
impl Debug for InputRxEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddInputChannel(_, _) => f.debug_tuple("AddInputChannel").finish(),
        }
    }
}

const NOISE_FLOOR: AudioSampleType = 0.002;

pub struct SampleConsumer(SampleConsum<CHANNEL_BUFFER_SIZE>, Notify);
impl SampleConsumer {
    pub async fn pop_iter(
        &mut self,
    ) -> ringbuf::consumer::PopIter<'_, SampleConsum<CHANNEL_BUFFER_SIZE>> {
        self.1.notified().await;
        self.0.pop_iter()
    }
    pub fn try_pop(&mut self) -> Option<AudioSampleType> {
        self.0.try_pop()
    }
}

pub(super) struct Input(SampleRb<CHANNEL_BUFFER_SIZE>, Notify);
impl ChannelType for Input {
    fn new() -> Self {
        Self(SampleRb::default(), Notify::default())
    }
}

impl AudioMixer {
    pub fn create_input_channel(
        &mut self,
        channel_mode: ChannelCount,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> Result<SampleConsumer, Box<dyn Error>> {
        let channel = Channel::<Input>::new(channel_mode, sample_format, sample_rate);
        let Input(rb, notify) = &channel.interface;
        let consumer = SampleConsumer(CachingCons::new(rb.clone()), notify.clone());

        if let Some(master) = &mut self.input
            && let Some(stream) = &mut master.stream
        {
            let sample_producer = CachingProd::new(rb.clone());
            stream
                .to_audio_thread
                .try_push(InputRxEvent::AddInputChannel(
                    sample_producer,
                    notify.clone(),
                ))
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

            let (event_prod, mut event_cons) = StaticRb::default().split();
            let stream_close_notification = Notify::new();
            let send_stream_close_notification = stream_close_notification.clone();

            let mut sample_producers = self
                .input_channels
                .iter()
                .map(|channel| {
                    let Input(rb, notify) = &channel.interface;
                    (CachingProd::new(rb.clone()), notify.clone())
                })
                .collect::<Vec<_>>();
            let stream = input
                .device
                .build_input_stream(
                    &stream_config,
                    move |data: &[AudioSampleType], &_| {
                        for event in event_cons.pop_iter() {
                            match event {
                                InputRxEvent::AddInputChannel(prod, notify) => {
                                    sample_producers.push((prod, notify));
                                }
                            };
                        }

                        sample_producers.retain(|producers| producers.0.read_is_held());
                        if sample_producers.is_empty() {
                            info!("Closing input audio stream");
                            send_stream_close_notification.notify();
                            return;
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

                            if frame_rms > NOISE_FLOOR {
                                sample_producer.0.push_iter(data.iter().copied());
                                sample_producer.1.notify();
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
                to_audio_thread: event_prod,
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
