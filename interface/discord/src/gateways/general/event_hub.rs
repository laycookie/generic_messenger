use asyncs_sync::Notify;
use crossbeam::queue::SegQueue;
use messenger_interface::interface::{AudioEvent, QueryEvent, TextEvent, VideoEvent, VoiceEvent};
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::warn;

const EVENT_QUEUE_WARN_AT: usize = 4096;

/// Session-scoped interface-event routing.
///
/// Producers submit through typed methods instead of reaching into queue
/// storage. Each submission also wakes its matching `listen()` stream, which
/// matters for media events produced by a different stream's driver.
pub(crate) struct EventHub {
    query: EventQueue<QueryEvent>,
    text: EventQueue<TextEvent>,
    voice: EventQueue<VoiceEvent>,
    audio: EventQueue<AudioEvent>,
    video: EventQueue<VideoEvent>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self {
            query: EventQueue::new("query"),
            text: EventQueue::new("text"),
            voice: EventQueue::new("voice"),
            audio: EventQueue::new("audio"),
            video: EventQueue::new("video"),
        }
    }
}

macro_rules! event_routes {
    ($(($submit:ident, $next:ident, $field:ident, $event:ty)),+ $(,)?) => {
        impl EventHub {
            $(
                pub(crate) fn $submit(&self, event: $event) {
                    self.$field.submit(event);
                }

                pub(crate) async fn $next(&self) -> $event {
                    self.$field.next().await
                }
            )+
        }
    };
}

event_routes!(
    (submit_query, next_query, query, QueryEvent),
    (submit_text, next_text, text, TextEvent),
    (submit_voice, next_voice, voice, VoiceEvent),
    (submit_audio, next_audio, audio, AudioEvent),
    (submit_video, next_video, video, VideoEvent),
);

struct EventQueue<T> {
    events: SegQueue<T>,
    queued: AtomicUsize,
    ready: Notify,
    route: &'static str,
}

impl<T> EventQueue<T> {
    fn new(route: &'static str) -> Self {
        Self {
            events: SegQueue::new(),
            queued: AtomicUsize::new(0),
            ready: Notify::new(),
            route,
        }
    }

    fn submit(&self, event: T) {
        // Reserve the depth before publishing. This keeps `queued` from
        // underflowing if a waiting consumer pops immediately after `push`.
        let depth = self.queued.fetch_add(1, Ordering::Relaxed) + 1;
        self.events.push(event);
        if depth >= EVENT_QUEUE_WARN_AT && depth.is_power_of_two() {
            warn!(
                route = self.route,
                queued = depth,
                "Discord interface event consumer is falling behind"
            );
        }
        // `notify_one` retains a permit when the consumer has not registered
        // yet, closing the pop -> wait race. Interface contracts specify one
        // long-lived consumer per event type.
        self.ready.notify_one();
    }

    async fn next(&self) -> T {
        loop {
            if let Some(event) = self.events.pop() {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                return event;
            }
            let notified = self.ready.notified();
            // Register intent to wait, then check again. `notify_one` also
            // retains a permit, but this second pop keeps correctness explicit
            // if notification behavior changes later.
            if let Some(event) = self.events.pop() {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                return event;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, sync::atomic::Ordering, task::Poll};

    use futures::{Future as _, future::poll_fn};

    use super::{EVENT_QUEUE_WARN_AT, EventQueue};

    #[test]
    fn queue_preserves_events_past_previous_capacity() {
        let queue = EventQueue::new("test");
        for event in 0..=EVENT_QUEUE_WARN_AT {
            queue.submit(event);
        }

        futures::executor::block_on(async {
            for event in 0..=EVENT_QUEUE_WARN_AT {
                assert_eq!(queue.next().await, event);
            }
        });
        assert_eq!(queue.queued.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn queue_is_fifo() {
        let queue = EventQueue::new("test");
        queue.submit(1);
        queue.submit(2);
        queue.submit(3);

        futures::executor::block_on(async {
            assert_eq!(queue.next().await, 1);
            assert_eq!(queue.next().await, 2);
            assert_eq!(queue.next().await, 3);
        });
    }

    #[test]
    fn submit_before_wait_registration_is_not_missed() {
        futures::executor::block_on(async {
            let queue = EventQueue::new("test");
            let next = queue.next();
            queue.submit(7);

            assert_eq!(next.await, 7);
        });
    }

    #[test]
    fn submit_wakes_registered_waiter() {
        futures::executor::block_on(async {
            let queue = EventQueue::new("test");
            let mut next = pin!(queue.next());
            poll_fn(|cx| {
                assert!(matches!(next.as_mut().poll(cx), Poll::Pending));
                Poll::Ready(())
            })
            .await;

            queue.submit(9);
            assert_eq!(next.await, 9);
        });
    }
}
