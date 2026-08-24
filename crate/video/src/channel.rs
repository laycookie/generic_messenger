use std::{
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Instant,
};

use futures::{future::poll_fn, task::AtomicWaker};

use crate::VideoFrame;

static NEXT_CHANNEL_ID: AtomicU64 = AtomicU64::new(1);

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// Create two halves of a latest-frame channel.
///
/// Producer never blocks. A new frame replaces any unseen older frame, so a
/// slow consumer observes current video instead of accumulating latency.
pub fn frame_channel() -> (FrameProducer, FrameConsumer) {
    let channel = Arc::new(FrameChannel {
        id: NEXT_CHANNEL_ID.fetch_add(1, Ordering::Relaxed),
        state: Mutex::new(FrameChannelState {
            frame: None,
            next_sequence: 0,
            producer_alive: true,
            consumer_alive: true,
        }),
        consumer_waker: AtomicWaker::new(),
    });
    (FrameProducer(channel.clone()), FrameConsumer(channel))
}

struct FrameChannel {
    id: u64,
    state: Mutex<FrameChannelState>,
    consumer_waker: AtomicWaker,
}

impl FrameChannel {
    fn state(&self) -> MutexGuard<'_, FrameChannelState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct FrameChannelState {
    frame: Option<QueuedFrame>,
    next_sequence: u64,
    producer_alive: bool,
    consumer_alive: bool,
}

struct QueuedFrame {
    frame: VideoFrame,
    sequence: u64,
    published_at: Option<Instant>,
}

/// Sending half of a [`frame_channel`].
pub struct FrameProducer(Arc<FrameChannel>);

impl FrameProducer {
    /// Publish newest frame, replacing any unseen older frame.
    ///
    /// Returns `false` only after consumer has gone away.
    pub fn push(&mut self, mut frame: VideoFrame) -> bool {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let stage_started = timing_enabled.then(Instant::now);
        let frame_timing = timing_enabled.then(|| frame.ensure_timing());
        let lock_started = timing_enabled.then(Instant::now);
        let mut state = self.0.state();
        let lock_wait_us = lock_started.map(elapsed_micros).unwrap_or_default();
        if !state.consumer_alive {
            return false;
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        let width = frame.width;
        let height = frame.height;
        let bytes = frame.pixel_data.as_bytes().len();
        // Move stale frame out first. Dropping a multi-megabyte pixel buffer
        // while holding channel mutex lengthens producer/consumer contention.
        let stale = state.frame.replace(QueuedFrame {
            frame,
            sequence,
            published_at: timing_enabled.then(Instant::now),
        });
        let stale_replaced = stale.is_some();
        drop(state);
        drop(stale);
        self.0.consumer_waker.wake();
        if timing_enabled {
            tracing::trace!(
                target: "video_timing",
                stage = "channel.publish",
                channel_id = self.0.id,
                frame_id = frame_timing.map_or(0, |timing| timing.id()),
                channel_sequence = sequence,
                pipeline_elapsed_us = frame_timing.map_or(0, |timing| timing.elapsed_micros()),
                width,
                height,
                bytes,
                stale_replaced,
                lock_wait_us,
                elapsed_us = stage_started.map(elapsed_micros).unwrap_or_default(),
                "video stage complete"
            );
        }
        true
    }

    /// True once consuming side was dropped.
    pub fn is_closed(&self) -> bool {
        !self.0.state().consumer_alive
    }
}

impl Drop for FrameProducer {
    fn drop(&mut self) {
        self.0.state().producer_alive = false;
        self.0.consumer_waker.wake();
    }
}

/// Receiving half of a [`frame_channel`].
pub struct FrameConsumer(Arc<FrameChannel>);

impl FrameConsumer {
    /// Await next frame; `None` once producer is gone.
    pub async fn pop(&mut self) -> Option<VideoFrame> {
        poll_fn(|cx| self.poll_pop(cx)).await
    }

    pub(crate) fn poll_pop(&mut self, cx: &mut Context<'_>) -> Poll<Option<VideoFrame>> {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let stage_started = timing_enabled.then(Instant::now);
        let lock_started = timing_enabled.then(Instant::now);
        let mut state = self.0.state();
        let lock_wait_us = lock_started.map(elapsed_micros).unwrap_or_default();
        if let Some(queued) = state.frame.take() {
            drop(state);
            if timing_enabled {
                let frame_timing = queued.frame.timing();
                tracing::trace!(
                    target: "video_timing",
                    stage = "channel.consume",
                    channel_id = self.0.id,
                    frame_id = frame_timing.map_or(0, |timing| timing.id()),
                    channel_sequence = queued.sequence,
                    pipeline_elapsed_us = frame_timing
                        .map_or(0, |timing| timing.elapsed_micros()),
                    queue_age_us = queued
                        .published_at
                        .map(elapsed_micros)
                        .unwrap_or_default(),
                    lock_wait_us,
                    elapsed_us = stage_started.map(elapsed_micros).unwrap_or_default(),
                    "video stage complete"
                );
            }
            return Poll::Ready(Some(queued.frame));
        }
        if !state.producer_alive {
            return Poll::Ready(None);
        }
        self.0.consumer_waker.register(cx.waker());
        Poll::Pending
    }

    /// Take ready frame; `None` when empty or ended.
    ///
    /// Callers needing to distinguish those states should use [`Self::pop`].
    pub fn try_pop(&mut self) -> Option<VideoFrame> {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let stage_started = timing_enabled.then(Instant::now);
        let queued = self.0.state().frame.take()?;
        let frame_timing = queued.frame.timing();
        tracing::trace!(
            target: "video_timing",
            stage = "channel.try_consume",
            channel_id = self.0.id,
            frame_id = frame_timing.map_or(0, |timing| timing.id()),
            channel_sequence = queued.sequence,
            pipeline_elapsed_us = frame_timing.map_or(0, |timing| timing.elapsed_micros()),
            queue_age_us = queued
                .published_at
                .map(elapsed_micros)
                .unwrap_or_default(),
            elapsed_us = stage_started.map(elapsed_micros).unwrap_or_default(),
            "video stage complete"
        );
        Some(queued.frame)
    }
}

impl Drop for FrameConsumer {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.consumer_alive = false;
        let stale = state.frame.take();
        drop(state);
        drop(stale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PixelData;

    #[test]
    fn replaces_stale_frame_and_reports_closure() {
        let frame = |value| VideoFrame::new(1, 1, PixelData::Rgba8(vec![value; 4]));
        let (mut producer, mut consumer) = frame_channel();
        assert!(producer.push(frame(1)));
        assert!(producer.push(frame(2)));
        assert_eq!(
            consumer.try_pop().unwrap().pixel_data,
            PixelData::Rgba8(vec![2; 4])
        );
        assert!(consumer.try_pop().is_none());

        assert!(!producer.is_closed());
        drop(consumer);
        assert!(producer.is_closed());
        assert!(!producer.push(frame(3)));
    }

    #[test]
    fn drains_last_frame_before_ending() {
        futures::executor::block_on(async {
            let (mut producer, mut consumer) = frame_channel();
            assert!(producer.push(VideoFrame::new(1, 1, PixelData::Rgb8(vec![1, 2, 3]),)));
            drop(producer);

            assert_eq!(
                consumer.pop().await.unwrap().pixel_data,
                PixelData::Rgb8(vec![1, 2, 3])
            );
            assert!(consumer.pop().await.is_none());
        });
    }
}
