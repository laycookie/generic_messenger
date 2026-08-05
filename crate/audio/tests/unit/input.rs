use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use ringbuf::traits::{Observer as _, Producer as _};

use super::*;
use crate::{SampleFormat, effects::Afx, effects_rack::EffectsRack, params::AfxParams};

#[derive(AfxParams)]
struct DropAll {
    _private: (),
}

impl Afx for DropAll {
    fn name(&self) -> &'static str {
        "DropAll"
    }

    fn apply_to(&mut self, _format: StreamFormat, _audio: &mut [AudioSampleType]) -> bool {
        false
    }
}

fn voice_format() -> StreamFormat {
    StreamFormat {
        channels: 2,
        sample_format: SampleFormat::F32,
        sample_rate: 48_000,
    }
}

fn gated_consumer() -> (SampleConsumer, SampleProd<CHANNEL_BUFFER_SIZE>) {
    let format = voice_format();
    let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
    let notify = Arc::new(Notify::new());
    let (mut rack, mut effects) = EffectsRack::new();
    rack.insert(Box::new(DropAll { _private: () }));
    effects.prepare(format);

    (
        SampleConsumer {
            cons: CachingCons::new(rb.clone()),
            notify,
            resampler: Resampler::new(
                format.channels as usize,
                format.sample_rate,
                format.channels as usize,
                format.sample_rate,
            ),
            effects,
            scratch: vec![0.0; CONVERT_SCRATCH_LEN].into_boxed_slice(),
            format,
            backlog_high_water: CHANNEL_BUFFER_SIZE,
            backlog_target: CHANNEL_BUFFER_SIZE,
            writer_started: Arc::new(AtomicBool::new(true)),
        },
        CachingProd::new(rb),
    )
}

#[test]
fn stale_backlog_is_trimmed_to_recent_whole_frames() {
    let format = voice_format();
    let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
    let notify = Arc::new(Notify::new());
    let (rack, mut effects) = EffectsRack::new();
    drop(rack);
    effects.prepare(format);
    let mut consumer = SampleConsumer {
        cons: CachingCons::new(rb.clone()),
        notify,
        resampler: Resampler::new(2, 48_000, 2, 48_000),
        effects,
        scratch: vec![0.0; CONVERT_SCRATCH_LEN].into_boxed_slice(),
        format,
        backlog_high_water: 8,
        backlog_target: 4,
        writer_started: Arc::new(AtomicBool::new(true)),
    };
    let mut producer = CachingProd::new(rb);
    let samples = (0..12).map(|sample| sample as f32).collect::<Vec<_>>();
    assert_eq!(producer.push_slice(&samples), samples.len());

    let mut out = [0.0_f32; 4];
    assert_eq!(consumer.pop_now(&mut out), 4);
    assert_eq!(out, [8.0, 9.0, 10.0, 11.0]);
}

#[test]
fn pop_now_batches_wrapped_ring_storage() {
    let format = voice_format();
    let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
    let (rack, mut effects) = EffectsRack::new();
    drop(rack);
    effects.prepare(format);
    let mut consumer = SampleConsumer {
        cons: CachingCons::new(rb.clone()),
        notify: Arc::new(Notify::new()),
        resampler: Resampler::new(2, 48_000, 2, 48_000),
        effects,
        scratch: vec![0.0; CONVERT_SCRATCH_LEN].into_boxed_slice(),
        format,
        backlog_high_water: CHANNEL_BUFFER_SIZE,
        backlog_target: CHANNEL_BUFFER_SIZE,
        writer_started: Arc::new(AtomicBool::new(true)),
    };
    let mut producer = CachingProd::new(rb);

    let initial = (0..9_000).map(|sample| sample as f32).collect::<Vec<_>>();
    assert_eq!(producer.push_slice(&initial), initial.len());
    let mut discarded = vec![0.0_f32; 8_996];
    assert_eq!(consumer.pop_now(&mut discarded), discarded.len());

    let wrapped = (10_000..11_000)
        .map(|sample| sample as f32)
        .collect::<Vec<_>>();
    assert_eq!(producer.push_slice(&wrapped), wrapped.len());
    let mut out = vec![0.0_f32; 1_004];
    assert_eq!(consumer.pop_now(&mut out), out.len());
    assert_eq!(&out[..4], &[8_996.0, 8_997.0, 8_998.0, 8_999.0]);
    assert_eq!(&out[4..], wrapped.as_slice());
}

#[test]
fn async_pop_reports_closed_writer() {
    let (mut consumer, producer) = gated_consumer();
    drop(producer);

    let mut out = [0.0; 2];
    let mut pop = std::pin::pin!(consumer.pop_or_closed::<f32>(&mut out));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert_eq!(
        std::future::Future::poll(pop.as_mut(), &mut cx),
        Poll::Ready(None)
    );
}

#[test]
fn writer_dropped_before_first_poll_reports_closed() {
    let format = voice_format();
    let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
    let notify = Arc::new(Notify::new());
    let writer_started = Arc::new(AtomicBool::new(false));
    let (rack, mut effects) = EffectsRack::new();
    drop(rack);
    effects.prepare(format);
    let mut consumer = SampleConsumer {
        cons: CachingCons::new(rb.clone()),
        notify: notify.clone(),
        resampler: Resampler::new(2, 48_000, 2, 48_000),
        effects,
        scratch: vec![0.0; CONVERT_SCRATCH_LEN].into_boxed_slice(),
        format,
        backlog_high_water: CHANNEL_BUFFER_SIZE,
        backlog_target: CHANNEL_BUFFER_SIZE,
        writer_started: writer_started.clone(),
    };

    let writer = InputWriter::new(CachingProd::new(rb), notify, &writer_started);
    assert!(writer_started.load(Ordering::Acquire));
    drop(writer);

    let mut out = [0.0; 2];
    let mut pop = std::pin::pin!(consumer.pop_or_closed::<f32>(&mut out));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert_eq!(
        std::future::Future::poll(pop.as_mut(), &mut cx),
        Poll::Ready(None)
    );
}

#[test]
fn dropping_parked_writer_wakes_then_reports_closed() {
    struct CountWake(AtomicUsize);
    impl std::task::Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let format = voice_format();
    let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
    let notify = Arc::new(Notify::new());
    let writer_started = Arc::new(AtomicBool::new(false));
    let (rack, mut effects) = EffectsRack::new();
    drop(rack);
    effects.prepare(format);
    let mut consumer = SampleConsumer {
        cons: CachingCons::new(rb.clone()),
        notify: notify.clone(),
        resampler: Resampler::new(2, 48_000, 2, 48_000),
        effects,
        scratch: vec![0.0; CONVERT_SCRATCH_LEN].into_boxed_slice(),
        format,
        backlog_high_water: CHANNEL_BUFFER_SIZE,
        backlog_target: CHANNEL_BUFFER_SIZE,
        writer_started: writer_started.clone(),
    };
    let writer = InputWriter::new(CachingProd::new(rb), notify, &writer_started);
    let mut out = [0.0; 2];
    let mut pop = std::pin::pin!(consumer.pop_or_closed::<f32>(&mut out));
    let wakes = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wakes.clone());
    let mut cx = std::task::Context::from_waker(&waker);

    assert_eq!(
        std::future::Future::poll(pop.as_mut(), &mut cx),
        Poll::Pending
    );
    // Finish first cooperative yield, consume InputWriter::new's retained
    // Notify permit, then enter next loop's cooperative yield.
    assert_eq!(
        std::future::Future::poll(pop.as_mut(), &mut cx),
        Poll::Pending
    );
    // Complete next yield so this future is parked on Notify.
    assert_eq!(
        std::future::Future::poll(pop.as_mut(), &mut cx),
        Poll::Pending
    );
    let before_drop = wakes.0.load(Ordering::Relaxed);

    drop(writer);

    assert!(wakes.0.load(Ordering::Relaxed) > before_drop);
    assert_eq!(
        std::future::Future::poll(pop.as_mut(), &mut cx),
        Poll::Ready(None)
    );
}

#[test]
fn dropped_buffers_do_not_drain_the_whole_ring() {
    let (mut consumer, mut producer) = gated_consumer();
    let samples = vec![0.0; 1024];
    assert_eq!(producer.push_slice(&samples), samples.len());

    let before = consumer.cons.occupied_len();
    let mut out = vec![0.0; 128];
    assert_eq!(consumer.pop_now::<f32>(&mut out), 0);

    let drained = before - consumer.cons.occupied_len();
    assert_eq!(
        drained,
        out.len(),
        "one discarded read may consume one output-sized chunk, not the whole queued ring"
    );
}
