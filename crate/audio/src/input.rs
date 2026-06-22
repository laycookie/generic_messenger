use std::{error::Error, fmt::Debug, io, sync::Arc};

use cpal::{FromSample, Sample, SizedSample, traits::DeviceTrait as _};
use ringbuf::{
    CachingCons, CachingProd, StaticRb,
    traits::{Consumer as _, Observer as _, Producer as _},
};
use tracing::error;

use crate::{
    AudioMixer, AudioSampleType, CHANNEL_BUFFER_SIZE, CHANNEL_HEADROOM, CONVERT_SCRATCH_LEN,
    MIX_SCRATCH_LEN, Notify, SampleConsum, SampleProd, SampleRb, StreamFormat,
    resample::Resampler,
    effects::EffectChain,
    stream::{for_each_sample_format, update_presence},
};

pub(crate) enum InputRxEvent {
    AddInputChannel(SampleProd<CHANNEL_BUFFER_SIZE>, Arc<Notify>),
}
impl Debug for InputRxEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddInputChannel(_, _) => f.debug_tuple("AddInputChannel").finish(),
        }
    }
}

/// Application handle for pulling captured audio out of an input channel.
///
/// The audio thread buffers device-format samples; they are converted to the
/// format declared at channel creation (and run through the channel's
/// effects) here, on the caller's thread, as they are popped.
pub struct SampleConsumer {
    cons: SampleConsum<CHANNEL_BUFFER_SIZE>,
    notify: Arc<Notify>,
    resampler: Resampler,
    effects: EffectChain,
    /// Declared-format staging between conversion/effects and the caller.
    scratch: Box<[AudioSampleType]>,
    format: StreamFormat,
}

impl SampleConsumer {
    /// Non-blocking pop of converted samples into `out`. Returns the number
    /// of samples written (always whole frames).
    pub fn pop_now<T>(&mut self, out: &mut [T]) -> usize
    where
        T: SizedSample + FromSample<AudioSampleType>,
    {
        debug_assert_eq!(
            T::FORMAT,
            self.format.sample_format,
            "popped sample type does not match the channel's declared format",
        );
        let dst_ch = self.format.channels as usize;
        let usable = out.len() - out.len() % dst_ch;
        let mut written = 0;
        while written < usable {
            let budget = (usable - written).min(self.scratch.len());
            let cons = &mut self.cons;
            let converted = self.resampler.pull(
                |frame: &mut [AudioSampleType]| {
                    if cons.occupied_len() < frame.len() {
                        return false;
                    }
                    // Whole frames only; the producer side never splits one.
                    let popped = cons.pop_slice(frame);
                    debug_assert_eq!(popped, frame.len());
                    true
                },
                &mut self.scratch[..budget],
            );
            if converted == 0 {
                break;
            }
            // A dropped buffer (a gate on silence) is consumed from the ring
            // but never handed back: skip it and keep pulling, so a silent
            // channel returns nothing and the caller's `pop` parks.
            let keep = self
                .effects
                .iter_mut()
                .all(|effect| effect.apply_to(&mut self.scratch[..converted]));
            if !keep {
                continue;
            }
            for (slot, sample) in out[written..written + converted]
                .iter_mut()
                .zip(&self.scratch[..converted])
            {
                *slot = T::from_sample(*sample);
            }
            written += converted;
        }
        written
    }

    /// Pop converted samples, waiting until at least one frame is available.
    /// Returns 0 only if `out` cannot hold a single frame.
    pub async fn pop<T>(&mut self, out: &mut [T]) -> usize
    where
        T: SizedSample + FromSample<AudioSampleType>,
    {
        if out.len() < self.format.channels as usize {
            return 0;
        }
        loop {
            let written = self.pop_now(out);
            if written > 0 {
                return written;
            }
            self.notify.notified().await;
        }
    }
}

impl AudioMixer {
    /// Create an input (capture) channel. `format` declares what the
    /// application wants to receive; captured audio is converted from the
    /// device's preferred format, with `effects` applied (in the declared
    /// format) at pop time.
    pub fn create_input_channel(
        &mut self,
        format: StreamFormat,
        mut effects: EffectChain,
    ) -> Result<SampleConsumer, Box<dyn Error>> {
        if format.channels == 0 || format.sample_rate == 0 {
            return Err("invalid stream format: zero channels or sample rate".into());
        }
        if format.channels as usize > CONVERT_SCRATCH_LEN {
            return Err(format!("unsupported channel count: {}", format.channels).into());
        }
        self.prune_input_channels();
        let Some(master) = &mut self.input else {
            return Err("no input device available".into());
        };
        let device_format = StreamFormat::of_device(&master.config);

        // Input effects run in the channel's declared format (post-conversion);
        // give each the format it will see before any audio flows.
        for effect in effects.iter_mut() {
            effect.prepare(format);
        }

        let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
        let notify = Arc::new(Notify::new());
        let consumer = SampleConsumer {
            cons: CachingCons::new(rb.clone()),
            notify: notify.clone(),
            resampler: Resampler::new(
                device_format.channels as usize,
                device_format.sample_rate,
                format.channels as usize,
                format.sample_rate,
            ),
            effects,
            scratch: vec![0.0; CONVERT_SCRATCH_LEN].into_boxed_slice(),
            format,
        };

        if let Some(event_tx) = master.event_tx() {
            let sample_producer = CachingProd::new(rb.clone());
            event_tx
                .try_push(InputRxEvent::AddInputChannel(
                    sample_producer,
                    notify.clone(),
                ))
                .map_err(|err| {
                    io::Error::other(format!("failed to push input channel event: {err:?}"))
                })?;
        }
        self.input_channels.push((rb, notify));

        Ok(consumer)
    }

    /// Start the capture stream using the device's preferred configuration.
    ///
    /// Returns a notification that fires when the last channel closes, or
    /// `None` when there is no input device. Calling this while the stream is
    /// already running returns the running stream's notification.
    pub fn start_stream_input(&mut self) -> Result<Option<Arc<Notify>>, Box<dyn Error>> {
        self.prune_input_channels();
        let Some(master) = &mut self.input else {
            return Ok(None);
        };
        if let Some(notify) = master.running_notify() {
            return Ok(Some(notify));
        }

        let producers = self
            .input_channels
            .iter()
            .map(|(rb, notify)| (CachingProd::new(rb.clone()), notify.clone()))
            .collect::<Vec<_>>();

        master.start(
            "input",
            producers,
            |device, config, sample_format, producers, events, close_notify| {
                for_each_sample_format!(
                    sample_format,
                    build_input_stream_typed,
                    device,
                    config,
                    producers,
                    events,
                    close_notify
                )
            },
        )
    }

    /// Stop the capture stream. Channels survive; a later
    /// `start_stream_input` picks them up again.
    pub fn stop_stream_input(&mut self) {
        if let Some(input) = &mut self.input {
            input.stop();
        }
        self.prune_input_channels();
    }
}

/// Build the real-time capture callback. It must stay free of locks,
/// allocation and blocking: it drains the channel-event ring and copies
/// device frames into per-channel rings. The per-channel
/// `notify_one` is lock-free unless that channel's consumer task is parked —
/// waking it is the point — and the close notification fires once, on the
/// transition to zero channels.
fn build_input_stream_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut sample_producers: Vec<(SampleProd<CHANNEL_BUFFER_SIZE>, Arc<Notify>)>,
    mut events: CachingCons<Arc<StaticRb<InputRxEvent, 8>>>,
    close_notify: Arc<Notify>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    AudioSampleType: FromSample<T>,
{
    let device_channels = config.channels as usize;
    // Frame-aligned chunking so a partial push can never shift interleaving.
    let chunk_len = (MIX_SCRATCH_LEN / device_channels) * device_channels;
    sample_producers.reserve(CHANNEL_HEADROOM);
    let mut had_channels = !sample_producers.is_empty();
    let mut in_buf = [0.0 as AudioSampleType; MIX_SCRATCH_LEN];

    device.build_input_stream(
        config,
        move |data: &[T], _| {
            for event in events.pop_iter() {
                match event {
                    InputRxEvent::AddInputChannel(prod, notify) => {
                        sample_producers.push((prod, notify))
                    }
                }
            }
            // Dropped here when the consumer is gone, but the mixer still
            // holds each ring's `Arc`, so the deallocating drop happens on
            // the control thread (`AudioMixer::prune_input_channels`).
            sample_producers.retain(|(prod, _)| prod.read_is_held());
            update_presence(
                !sample_producers.is_empty(),
                &mut had_channels,
                &close_notify,
            );
            if sample_producers.is_empty() {
                return;
            }

            for chunk in data.chunks(chunk_len) {
                let len = chunk.len();
                for (slot, sample) in in_buf[..len].iter_mut().zip(chunk) {
                    *slot = AudioSampleType::from_sample(*sample);
                }

                for (prod, notify) in sample_producers.iter_mut() {
                    let vacant = prod.vacant_len();
                    // Whole frames only: an overrun must not flip parity.
                    let take = (vacant - vacant % device_channels).min(len);
                    if prod.push_slice(&in_buf[..take]) > 0 {
                        notify.notify_one();
                    }
                }
            }
        },
        move |err| {
            error!("Audio input stream error: {err:?}");
        },
        None,
    )
}
