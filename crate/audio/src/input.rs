use std::{
    error::Error,
    fmt::Debug,
    future, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};

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
    resample::Resampler,
    stream::{EventCons, for_each_sample_format, update_presence},
};

const INPUT_LATENCY_HIGH_WATER_MS: u64 = 80;
const INPUT_LATENCY_TARGET_MS: u64 = 40;

pub(crate) enum InputRxEvent {
    AddInputChannel(InputWriter),
}
impl Debug for InputRxEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddInputChannel(_) => f.debug_tuple("AddInputChannel").finish(),
        }
    }
}

/// Producer plus close wakeup. Dropping an input stream drops these writers;
/// parked consumers wake and can observe `write_is_held() == false`.
pub(crate) struct InputWriter {
    producer: Option<SampleProd<CHANNEL_BUFFER_SIZE>>,
    notify: Arc<Notify>,
}

impl InputWriter {
    fn new(
        producer: SampleProd<CHANNEL_BUFFER_SIZE>,
        notify: Arc<Notify>,
        writer_started: &Arc<AtomicBool>,
    ) -> Self {
        // Wake consumer created before device stream started. Producer is now
        // held even if callback has not adopted its event yet.
        writer_started.store(true, Ordering::Release);
        notify.notify_one();
        Self {
            producer: Some(producer),
            notify,
        }
    }
}

impl Drop for InputWriter {
    fn drop(&mut self) {
        // Release the ring's writer before waking the consumer. Otherwise the
        // consumer can observe a live writer, park, then miss its final wake.
        drop(self.producer.take());
        self.notify.notify_one();
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
    effects: RackBackend,
    /// Declared-format staging between conversion/effects and the caller.
    scratch: Box<[AudioSampleType]>,
    format: StreamFormat,
    /// Device-format sample counts. Crossing high water discards oldest whole
    /// frames down to target, recovering live latency after scheduler stalls.
    backlog_high_water: usize,
    backlog_target: usize,
    /// A newly created channel may precede device stream startup. Only report
    /// closure after at least one writer has existed.
    writer_started: Arc<AtomicBool>,
}

impl SampleConsumer {
    fn trim_stale_backlog(&mut self) {
        let occupied = self.cons.occupied_len();
        if occupied <= self.backlog_high_water {
            return;
        }
        let source_channels = self.resampler.src_channels();
        let discard = occupied
            .saturating_sub(self.backlog_target)
            .saturating_sub(occupied.saturating_sub(self.backlog_target) % source_channels);
        if discard > 0 {
            self.cons.skip(discard);
            // Interpolation window referenced discarded history.
            self.resampler.reset();
        }
    }

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
        self.trim_stale_backlog();
        let dst_ch = self.format.channels as usize;
        let usable = out.len() - out.len() % dst_ch;
        let mut written = 0;
        while written < usable {
            let budget = (usable - written).min(self.scratch.len());
            let source_channels = self.resampler.src_channels();
            // Snapshot occupied storage once and publish the total consumed
            // count when this iterator drops. Popping one frame at a time would
            // otherwise synchronize the shared read index for every frame.
            let samples = self.cons.pop_iter();
            let available = samples.len();
            let frame_aligned = available - available % source_channels;
            let mut samples = samples.take(frame_aligned);
            let converted = self.resampler.pull(
                |frame: &mut [AudioSampleType]| {
                    if samples.len() < frame.len() {
                        return false;
                    }
                    for slot in frame {
                        *slot = samples
                            .next()
                            .expect("frame-aligned ring snapshot contains a complete frame");
                    }
                    true
                },
                &mut self.scratch[..budget],
            );
            if converted == 0 {
                break;
            }
            // A dropped buffer (a gate on silence) is consumed from the ring
            // but never handed back. Stop after one discarded chunk so a
            // silent hot input cannot drain the whole ring in one poll.
            if !self.effects.apply(&mut self.scratch[..converted]) {
                break;
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
    /// Returns `None` after capture writer closes and buffered samples drain.
    pub async fn pop_or_closed<T>(&mut self, out: &mut [T]) -> Option<usize>
    where
        T: SizedSample + FromSample<AudioSampleType>,
    {
        if out.len() < self.format.channels as usize {
            return Some(0);
        }
        loop {
            let written = self.pop_now(out);
            if written > 0 {
                return Some(written);
            }
            if self.writer_started.load(Ordering::Acquire) && !self.cons.write_is_held() {
                return None;
            }
            yield_to_executor().await;
            if self.cons.occupied_len() >= self.format.channels as usize {
                continue;
            }
            if self.writer_started.load(Ordering::Acquire) && !self.cons.write_is_held() {
                return None;
            }
            self.notify.notified().await;
        }
    }

    /// Compatibility wrapper. Returns zero after capture closes.
    /// New long-lived loops should use [`Self::pop_or_closed`] to avoid
    /// interpreting closure as temporary silence.
    pub async fn pop<T>(&mut self, out: &mut [T]) -> usize
    where
        T: SizedSample + FromSample<AudioSampleType>,
    {
        self.pop_or_closed(out).await.unwrap_or(0)
    }
}

fn latency_samples(format: StreamFormat, milliseconds: u64) -> usize {
    let frames = u64::from(format.sample_rate)
        .saturating_mul(milliseconds)
        .checked_div(1000)
        .unwrap_or(0);
    usize::try_from(frames)
        .unwrap_or(usize::MAX)
        .saturating_mul(format.channels as usize)
        .max(format.channels as usize)
        .min(CHANNEL_BUFFER_SIZE)
}

async fn yield_to_executor() {
    let mut yielded = false;
    future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

impl AudioMixer {
    /// Create an input (capture) channel. `format` declares what the
    /// application wants to receive; captured audio is converted from the
    /// device's preferred format, with `effects` applied (in the declared
    /// format) at pop time. Keep the rack's frontend to edit the chain while
    /// the channel is live.
    pub fn create_input_channel(
        &mut self,
        format: StreamFormat,
        mut effects: RackBackend,
    ) -> Result<SampleConsumer, Box<dyn Error>> {
        if format.channels == 0 || format.sample_rate == 0 {
            return Err("invalid stream format: zero channels or sample rate".into());
        }
        if format.channels as usize > CONVERT_SCRATCH_LEN {
            return Err(format!("unsupported channel count: {}", format.channels).into());
        }
        self.prune_input_channels();
        if self.input_channels.len() >= MAX_AUDIO_CHANNELS {
            return Err(format!("input channel limit reached ({MAX_AUDIO_CHANNELS})").into());
        }
        let Some(master) = &mut self.input else {
            return Err("no input device available".into());
        };
        let device_format = StreamFormat::of_device(&master.config);

        // Input effects run in the channel's declared format (post-conversion);
        // give the rack the format it will see before any audio flows.
        effects.prepare(format);

        let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
        let notify = Arc::new(Notify::new());
        let writer_started = Arc::new(AtomicBool::new(false));
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
            backlog_high_water: latency_samples(device_format, INPUT_LATENCY_HIGH_WATER_MS),
            backlog_target: latency_samples(device_format, INPUT_LATENCY_TARGET_MS),
            writer_started: writer_started.clone(),
        };

        if let Some(event_tx) = master.event_tx() {
            let writer = InputWriter::new(
                CachingProd::new(rb.clone()),
                notify.clone(),
                &writer_started,
            );
            event_tx
                .try_push(InputRxEvent::AddInputChannel(writer))
                .map_err(|err| {
                    io::Error::other(format!("failed to push input channel event: {err:?}"))
                })?;
        }
        self.input_channels.push((rb, notify, writer_started));

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
            .map(|(rb, notify, writer_started)| {
                InputWriter::new(CachingProd::new(rb.clone()), notify.clone(), writer_started)
            })
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

#[cfg(test)]
#[path = "../tests/unit/input.rs"]
mod tests;

/// Build the real-time capture callback. Audio movement stays allocation-free
/// and nonblocking: it drains the channel-event ring and copies device frames
/// into per-channel rings. Per-channel `notify_one` may briefly take its waiter
/// lock when a consumer task is parked; notifications are coalesced to once per
/// device callback. Close notification fires once, on transition to zero
/// channels.
fn build_input_stream_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut sample_producers: Vec<InputWriter>,
    mut events: EventCons<InputRxEvent>,
    close_notify: Arc<Notify>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    AudioSampleType: FromSample<T>,
{
    let device_channels = config.channels as usize;
    // Frame-aligned chunking so a partial push can never shift interleaving.
    let chunk_len = (MIX_SCRATCH_LEN / device_channels) * device_channels;
    let missing_capacity = MAX_AUDIO_CHANNELS.saturating_sub(sample_producers.len());
    sample_producers.reserve(missing_capacity);
    let mut had_channels = !sample_producers.is_empty();
    let mut in_buf = [0.0 as AudioSampleType; MIX_SCRATCH_LEN];

    device.build_input_stream(
        config,
        move |data: &[T], _| {
            for event in events.pop_iter() {
                match event {
                    InputRxEvent::AddInputChannel(writer) => {
                        if sample_producers.len() < MAX_AUDIO_CHANNELS {
                            sample_producers.push(writer);
                        }
                    }
                }
            }
            // Dropped here when the consumer is gone, but the mixer still
            // holds each ring's `Arc`, so the deallocating drop happens on
            // the control thread (`AudioMixer::prune_input_channels`).
            sample_producers.retain(|writer| {
                writer
                    .producer
                    .as_ref()
                    .is_some_and(|producer| producer.read_is_held())
            });
            update_presence(
                !sample_producers.is_empty(),
                &mut had_channels,
                &close_notify,
            );
            if sample_producers.is_empty() {
                return;
            }

            let mut wrote_to = [false; MAX_AUDIO_CHANNELS];
            for chunk in data.chunks(chunk_len) {
                let len = chunk.len();
                for (slot, sample) in in_buf[..len].iter_mut().zip(chunk) {
                    *slot = AudioSampleType::from_sample(*sample);
                }

                for (index, writer) in sample_producers.iter_mut().enumerate() {
                    let Some(prod) = &mut writer.producer else {
                        continue;
                    };
                    let vacant = prod.vacant_len();
                    // Whole frames only: an overrun must not flip parity.
                    let take = (vacant - vacant % device_channels).min(len);
                    wrote_to[index] |= prod.push_slice(&in_buf[..take]) > 0;
                }
            }
            // One wake per writer and device callback, even when a large
            // callback was split into multiple scratch-sized chunks.
            for (writer, wrote) in sample_producers.iter().zip(wrote_to) {
                if wrote {
                    writer.notify.notify_one();
                }
            }
        },
        move |err| {
            error!("Audio input stream error: {err:?}");
        },
        None,
    )
}
