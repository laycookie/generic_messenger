use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use asyncs_sync::Notify;
use messenger_interface::interface::{QueryEvent, TextEvent, VideoEvent, VoiceEvent};
use tracing::warn;

const EVENT_QUEUE_CAPACITY: usize = 4096;

/// Session-scoped interface-event routing.
///
/// Producers submit through typed methods instead of reaching into queue
/// storage. Each submission also wakes its matching listener stream, which
/// matters for media events produced by a different stream's driver. If any
/// route reaches its memory bound, the whole session is invalidated instead of
/// silently losing a realtime delta.
pub(crate) struct EventHub {
    query: EventQueue<QueryEvent>,
    text: EventQueue<TextEvent>,
    voice: EventQueue<VoiceEvent>,
    video: EventQueue<VideoEvent>,
    invalidated: AtomicBool,
    invalidation_reported: AtomicBool,
}

impl Default for EventHub {
    fn default() -> Self {
        Self {
            query: EventQueue::new(),
            text: EventQueue::new(),
            voice: EventQueue::new(),
            video: EventQueue::new(),
            invalidated: AtomicBool::new(false),
            invalidation_reported: AtomicBool::new(false),
        }
    }
}

macro_rules! event_submitters {
    ($(($submit:ident, $field:ident, $event:ty, $route:literal)),+ $(,)?) => {
        impl EventHub {
            $(
                pub(crate) fn $submit(&self, event: $event) {
                    if self.$field.submit(event, &self.invalidated) == SubmitOutcome::Full {
                        self.invalidate($route);
                    }
                }
            )+
        }
    };
}

event_submitters!(
    (submit_query, query, QueryEvent, "query"),
    (submit_text, text, TextEvent, "text"),
    (submit_voice, voice, VoiceEvent, "voice"),
    (submit_video, video, VideoEvent, "video"),
);

impl EventHub {
    pub(crate) fn is_invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Acquire)
    }

    fn invalidate(&self, route: &'static str) {
        if self
            .invalidated
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        warn!(
            route,
            capacity = EVENT_QUEUE_CAPACITY,
            "Discord interface event queue overflowed; invalidating session state"
        );
        self.query.clear_and_wake();
        self.text.clear_and_wake();
        self.voice.clear_and_wake();
        self.video.clear_and_wake();
    }

    /// Query is the session-control route. Exactly one invalidation event asks
    /// the application to rebuild the messenger and take a fresh snapshot.
    pub(crate) async fn next_query(&self) -> Option<QueryEvent> {
        match self.query.next(&self.invalidated).await {
            Some(event) => Some(event),
            None if self
                .invalidation_reported
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok() =>
            {
                Some(QueryEvent::StateInvalidated)
            }
            None => None,
        }
    }

    pub(crate) async fn next_text(&self) -> Option<TextEvent> {
        self.text.next(&self.invalidated).await
    }

    pub(crate) async fn next_voice(&self) -> Option<VoiceEvent> {
        self.voice.next(&self.invalidated).await
    }

    pub(crate) async fn next_video(&self) -> Option<VideoEvent> {
        self.video.next(&self.invalidated).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubmitOutcome {
    Accepted,
    Full,
    Invalidated,
}

struct EventQueue<T> {
    events: Mutex<VecDeque<T>>,
    ready: Notify,
}

impl<T> EventQueue<T> {
    fn new() -> Self {
        Self {
            events: Mutex::new(VecDeque::with_capacity(EVENT_QUEUE_CAPACITY)),
            ready: Notify::new(),
        }
    }

    fn submit(&self, event: T, invalidated: &AtomicBool) -> SubmitOutcome {
        {
            let mut events = self.events.lock().unwrap();
            // Read the session flag while holding this route's lock. An
            // invalidator that raced an earlier read must then wait and clear
            // anything inserted here before publishing the reset signal.
            if invalidated.load(Ordering::Acquire) {
                return SubmitOutcome::Invalidated;
            }
            if events.len() >= EVENT_QUEUE_CAPACITY {
                return SubmitOutcome::Full;
            }
            events.push_back(event);
        }

        // `notify_one` retains a permit when the consumer has not registered
        // yet, closing the pop -> wait race. Interface contracts specify one
        // long-lived consumer per event type.
        self.ready.notify_one();
        SubmitOutcome::Accepted
    }

    fn pop(&self) -> Option<T> {
        self.events.lock().unwrap().pop_front()
    }

    fn clear_and_wake(&self) {
        self.events.lock().unwrap().clear();
        self.ready.notify_all();
    }

    async fn next(&self, invalidated: &AtomicBool) -> Option<T> {
        loop {
            if invalidated.load(Ordering::Acquire) {
                return None;
            }
            if let Some(event) = self.pop() {
                return Some(event);
            }
            let notified = self.ready.notified();
            // Register intent to wait, then check again. `notify_one` also
            // retains a permit, but this second check keeps correctness explicit
            // if notification behavior changes later.
            if invalidated.load(Ordering::Acquire) {
                return None;
            }
            if let Some(event) = self.pop() {
                return Some(event);
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::pin,
        sync::{Arc, Barrier, atomic::AtomicBool},
        task::Poll,
        thread,
    };

    use futures::{Future as _, future::poll_fn};
    use messenger_interface::{interface::QueryEvent, types::UserStatus};

    use super::{EVENT_QUEUE_CAPACITY, EventHub, EventQueue, SubmitOutcome};

    #[test]
    fn queue_rejects_overflow_without_discarding_retained_events() {
        let queue = EventQueue::new();
        let invalidated = AtomicBool::new(false);
        for event in 0..EVENT_QUEUE_CAPACITY {
            assert_eq!(queue.submit(event, &invalidated), SubmitOutcome::Accepted);
        }
        assert_eq!(
            queue.submit(EVENT_QUEUE_CAPACITY, &invalidated),
            SubmitOutcome::Full
        );

        assert_eq!(queue.events.lock().unwrap().len(), EVENT_QUEUE_CAPACITY);
        futures::executor::block_on(async {
            for event in 0..EVENT_QUEUE_CAPACITY {
                assert_eq!(queue.next(&invalidated).await, Some(event));
            }
        });
        assert!(queue.events.lock().unwrap().is_empty());
    }

    #[test]
    fn concurrent_producers_keep_queue_bounded_without_over_eviction() {
        const PRODUCERS: usize = 8;
        const EVENTS_PER_PRODUCER: usize = 1024;

        let queue = Arc::new(EventQueue::new());
        let invalidated = Arc::new(AtomicBool::new(false));
        let start = Arc::new(Barrier::new(PRODUCERS));
        let workers = (0..PRODUCERS)
            .map(|producer| {
                let queue = queue.clone();
                let invalidated = invalidated.clone();
                let start = start.clone();
                thread::spawn(move || {
                    start.wait();
                    let mut full = 0;
                    for event in 0..EVENTS_PER_PRODUCER {
                        if queue.submit(producer * EVENTS_PER_PRODUCER + event, &invalidated)
                            == SubmitOutcome::Full
                        {
                            full += 1;
                        }
                    }
                    full
                })
            })
            .collect::<Vec<_>>();

        let rejected = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .sum::<usize>();
        let submitted = PRODUCERS * EVENTS_PER_PRODUCER;
        assert_eq!(queue.events.lock().unwrap().len(), EVENT_QUEUE_CAPACITY);
        assert_eq!(rejected, submitted - EVENT_QUEUE_CAPACITY);
    }

    #[test]
    fn overflow_invalidates_all_routes_and_reports_one_reset() {
        let hub = EventHub::default();
        for user_id in 0..EVENT_QUEUE_CAPACITY as u64 {
            hub.submit_query(QueryEvent::UserStatusChanged {
                user_id,
                status: UserStatus::Online,
            });
        }
        hub.submit_query(QueryEvent::UserStatusChanged {
            user_id: EVENT_QUEUE_CAPACITY as u64,
            status: UserStatus::Online,
        });

        futures::executor::block_on(async {
            assert!(matches!(
                hub.next_query().await,
                Some(QueryEvent::StateInvalidated)
            ));
            assert!(hub.next_query().await.is_none());
            assert!(hub.next_text().await.is_none());
            assert!(hub.next_voice().await.is_none());
            assert!(hub.next_video().await.is_none());
        });
    }

    #[test]
    fn queue_is_fifo() {
        let queue = EventQueue::new();
        let invalidated = AtomicBool::new(false);
        queue.submit(1, &invalidated);
        queue.submit(2, &invalidated);
        queue.submit(3, &invalidated);

        futures::executor::block_on(async {
            assert_eq!(queue.next(&invalidated).await, Some(1));
            assert_eq!(queue.next(&invalidated).await, Some(2));
            assert_eq!(queue.next(&invalidated).await, Some(3));
        });
    }

    #[test]
    fn submit_before_wait_registration_is_not_missed() {
        futures::executor::block_on(async {
            let queue = EventQueue::new();
            let invalidated = AtomicBool::new(false);
            let next = queue.next(&invalidated);
            queue.submit(7, &invalidated);

            assert_eq!(next.await, Some(7));
        });
    }

    #[test]
    fn submit_wakes_registered_waiter() {
        futures::executor::block_on(async {
            let queue = EventQueue::new();
            let invalidated = AtomicBool::new(false);
            let mut next = pin!(queue.next(&invalidated));
            poll_fn(|cx| {
                assert!(matches!(next.as_mut().poll(cx), Poll::Pending));
                Poll::Ready(())
            })
            .await;

            queue.submit(9, &invalidated);
            assert_eq!(next.await, Some(9));
        });
    }
}
