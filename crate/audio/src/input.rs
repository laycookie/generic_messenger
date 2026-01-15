use std::ops::{Deref, DerefMut};

use cpal::{ChannelCount, SampleFormat, SampleRate};
use ringbuf::CachingCons;

use crate::{AudioMixer, Channel, ChannelType, SampleConsumer, SampleRb};

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
    ) -> Input<5120> {
        let (channel, consumer) =
            Channel::<_, Input<5120>>::new(channel_mode, sample_format, sample_rate);

        self.input_channels.push(channel);
        if let Some(master) = &self.output
        // && master.stream.is_none()
        {
            // self.stop_stream_output();
            // self.start_stream_output();
        };

        consumer
    }
}
