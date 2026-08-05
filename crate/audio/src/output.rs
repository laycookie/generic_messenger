use std::{error::Error, fmt::Debug, io, sync::Arc};

use cpal::{FromSample, Sample, SizedSample, traits::DeviceTrait as _};
use ringbuf::{
    CachingCons, CachingProd,
    traits::{Consumer as _, Observer as _, Producer as _},
};
use tracing::error;

use crate::{
    AudioMixer, AudioSampleType, CHANNEL_BUFFER_SIZE, CONVERT_SCRATCH_LEN, MAX_AUDIO_CHANNELS,
    MIX_SCRATCH_LEN, Notify, SampleConsum, SampleProd, SampleRb, StreamFormat,
    effects_rack::RackBackend,
    limiter::PeakLimiter,
    resample::Resampler,
    simd,
    stream::{EventCons, for_each_sample_format, update_presence},
};

/// Application handle for pushing audio into an output channel.
///
/// Samples are converted from the format declared at channel creation to the
/// device's preferred format and run through the channel's effects before
/// being buffered for the audio thread. All of that happens here, on the
/// caller's thread — the audio callback only mixes.
pub struct SampleProducer {
    producer: SampleProd<CHANNEL_BUFFER_SIZE>,
    resampler: Resampler,
    effects: RackBackend,
    /// Device-format staging between conversion/effects and the ring.
    scratch: Box<[AudioSampleType]>,
    format: StreamFormat,
}

impl SampleProducer {
    /// Whether mixer still owns receiving end of this channel.
    pub fn is_connected(&self) -> bool {
        self.producer.read_is_held()
    }

    /// Push interleaved samples in the channel's declared format.
    ///
    /// Returns how many samples were consumed (always whole frames). A short
    /// count means the channel's buffer is full; retry the remainder later.
    pub fn push_iter<T>(&mut self, samples: &[T]) -> usize
    where
        T: SizedSample,
        AudioSampleType: FromSample<T>,
    {
        debug_assert_eq!(
            T::FORMAT,
            self.format.sample_format,
            "pushed sample type does not match the channel's declared format",
        );
        let src_ch = self.format.channels as usize;
        let dst_ch = self.resampler.dst_channels();
        let usable = samples.len() - samples.len() % src_ch;
        let mut consumed = 0;
        while consumed < usable {
            let vacant = self.producer.vacant_len();
            let budget = (vacant - vacant % dst_ch).min(self.scratch.len());
            if budget < dst_ch {
                break;
            }
            let before = consumed;
            let written = self.resampler.pull(
                |frame: &mut [AudioSampleType]| {
                    if consumed + src_ch > usable {
                        return false;
                    }
                    for (slot, sample) in
                        frame.iter_mut().zip(&samples[consumed..consumed + src_ch])
                    {
                        *slot = AudioSampleType::from_sample(*sample);
                    }
                    consumed += src_ch;
                    true
                },
                &mut self.scratch[..budget],
            );
            // A dropped buffer (a gate on silence) is consumed from the source
            // but never queued for playback.
            if self.effects.apply(&mut self.scratch[..written]) {
                let pushed = self.producer.push_slice(&self.scratch[..written]);
                debug_assert_eq!(pushed, written);
            }
            if written == 0 && consumed == before {
                // Source exhausted while (re)priming the resampler window.
                break;
            }
        }
        consumed
    }
}

pub(crate) enum OutputRxEvent {
    AddOutputChannel(SampleConsum<CHANNEL_BUFFER_SIZE>),
}
impl Debug for OutputRxEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddOutputChannel(_) => f.debug_tuple("AddOutputChannel").finish(),
        }
    }
}

impl AudioMixer {
    /// Create an output (playback) channel. `format` declares what the
    /// application will push; audio is converted to the device's preferred
    /// format, with `effects` applied (in device format) at push time. Keep
    /// the rack's frontend to edit the chain while the channel is live.
    pub fn create_output_channel(
        &mut self,
        format: StreamFormat,
        mut effects: RackBackend,
    ) -> Result<SampleProducer, Box<dyn Error>> {
        if format.channels == 0 || format.sample_rate == 0 {
            return Err("invalid stream format: zero channels or sample rate".into());
        }
        self.prune_output_channels();
        if self.output_channels.len() >= MAX_AUDIO_CHANNELS {
            return Err(format!("output channel limit reached ({MAX_AUDIO_CHANNELS})").into());
        }
        let Some(master) = &mut self.output else {
            return Err("no output device available".into());
        };
        let device_format = StreamFormat::of_device(&master.config);
        if device_format.channels as usize > CONVERT_SCRATCH_LEN {
            return Err(format!(
                "unsupported device channel count: {}",
                device_format.channels
            )
            .into());
        }

        // Output effects run in the device format (post-conversion).
        effects.prepare(device_format);

        let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
        let producer = SampleProducer {
            producer: CachingProd::new(rb.clone()),
            resampler: Resampler::new(
                format.channels as usize,
                format.sample_rate,
                device_format.channels as usize,
                device_format.sample_rate,
            ),
            effects,
            scratch: vec![0.0; CONVERT_SCRATCH_LEN].into_boxed_slice(),
            format,
        };

        if let Some(event_tx) = master.event_tx() {
            let sample_consumer = CachingCons::new(rb.clone());
            event_tx
                .try_push(OutputRxEvent::AddOutputChannel(sample_consumer))
                .map_err(|err| {
                    io::Error::other(format!("failed to push output channel event: {err:?}"))
                })?;
        }
        self.output_channels.push(rb);

        Ok(producer)
    }

    /// Start the playback stream using the device's preferred configuration.
    ///
    /// Returns a notification that fires when the last channel closes, or
    /// `None` when there is no output device. Calling this while the stream
    /// is already running returns the running stream's notification.
    pub fn start_stream_output(&mut self) -> Result<Option<Arc<Notify>>, Box<dyn Error>> {
        self.prune_output_channels();
        let Some(master) = &mut self.output else {
            return Ok(None);
        };
        if let Some(notify) = master.running_notify() {
            return Ok(Some(notify));
        }

        let consumers = self
            .output_channels
            .iter()
            .map(|rb| CachingCons::new(rb.clone()))
            .collect::<Vec<_>>();

        master.start(
            "output",
            consumers,
            |device, config, sample_format, consumers, events, close_notify| {
                for_each_sample_format!(
                    sample_format,
                    build_output_stream_typed,
                    device,
                    config,
                    consumers,
                    events,
                    close_notify
                )
            },
        )
    }

    /// Stop the playback stream. Channels survive; a later
    /// `start_stream_output` picks them up again.
    pub fn stop_stream_output(&mut self) {
        if let Some(output) = &mut self.output {
            output.stop();
        }
        self.prune_output_channels();
    }
}

#[cfg(test)]
#[path = "../tests/unit/output.rs"]
mod tests;

/// Add every available whole frame, up to `mix.len()`, directly from a channel
/// ring into the mix buffer. Ring storage may wrap into two physical slices;
/// both are consumed under one frame-aligned count.
fn mix_channel<const N: usize>(
    consumer: &mut SampleConsum<N>,
    mix: &mut [AudioSampleType],
    channels: usize,
) -> usize {
    debug_assert!(channels > 0);
    debug_assert_eq!(mix.len() % channels, 0);
    let take = {
        let (first, second) = consumer.as_slices();
        let occupied = first.len() + second.len();
        let take = (occupied - occupied % channels).min(mix.len());
        let first_len = first.len().min(take);

        simd::add_assign(&mut mix[..first_len], &first[..first_len]);

        let second_len = take - first_len;
        simd::add_assign(&mut mix[first_len..take], &second[..second_len]);
        take
    };

    if take > 0 {
        consumer.skip(take);
    }
    take
}

/// Build the real-time playback callback. Audio movement stays allocation-free
/// and nonblocking: it drains the channel-event ring and mixes channel rings
/// into the device buffer in frame-aligned chunks. `close_notify` may briefly
/// take its waiter lock; it fires only when the last channel disappears, keeping
/// that cost off the steady-state path.
fn build_output_stream_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut sample_consumers: Vec<SampleConsum<CHANNEL_BUFFER_SIZE>>,
    mut events: EventCons<OutputRxEvent>,
    close_notify: Arc<Notify>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<AudioSampleType>,
{
    let device_channels = config.channels as usize;
    // Frame-aligned chunking so a partial pop can never shift interleaving.
    let chunk_len = (MIX_SCRATCH_LEN / device_channels) * device_channels;
    let missing_capacity = MAX_AUDIO_CHANNELS.saturating_sub(sample_consumers.len());
    sample_consumers.reserve(missing_capacity);
    let mut had_channels = !sample_consumers.is_empty();
    let mut mix_buf = [0.0 as AudioSampleType; MIX_SCRATCH_LEN];
    let samples_per_second = config
        .sample_rate
        .saturating_mul(u32::from(config.channels));
    let mut limiter = PeakLimiter::new(samples_per_second);

    device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            for event in events.pop_iter() {
                match event {
                    OutputRxEvent::AddOutputChannel(cons) => {
                        if sample_consumers.len() < MAX_AUDIO_CHANNELS {
                            sample_consumers.push(cons);
                        }
                    }
                }
            }
            // Channels whose producer is gone are dropped here, but the mixer
            // still holds each ring's `Arc`, so the deallocating drop happens
            // on the control thread (`AudioMixer::prune_output_channels`).
            sample_consumers.retain(|consumer| consumer.write_is_held());
            update_presence(
                !sample_consumers.is_empty(),
                &mut had_channels,
                &close_notify,
            );

            for chunk in data.chunks_mut(chunk_len) {
                let len = chunk.len();
                mix_buf[..len].fill(0.0);
                for consumer in sample_consumers.iter_mut() {
                    mix_channel(consumer, &mut mix_buf[..len], device_channels);
                }
                limiter.process_into(&mix_buf[..len], chunk);
            }
        },
        move |err| {
            error!("Audio output stream error: {err:?}");
        },
        None,
    )
}
