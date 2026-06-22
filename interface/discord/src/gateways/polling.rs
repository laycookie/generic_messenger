use std::{
    pin::pin,
    sync::{Arc, atomic::Ordering},
};

use asyncs_sync::Notify;
use futures::{FutureExt as _, StreamExt, future::Either, select, stream};
use smol::future::yield_now;
use tracing::{debug, error, trace, warn};

use super::{general::Opcode, voice::VoiceOpcode};
use crate::{InnerDiscord, UnitStruct, gateways::GatewayStreamReciver};

/// Wakes everyone parked on `pulled_notification` when dropped. The holder
/// of the receiver lock can exit `poll_for_events` early (gateway closed)
/// or be cancelled at any await point (its consumer stream got dropped);
/// without this guard the other event streams would stay parked in
/// `notified()` forever even though the lock is free again.
struct NotifyOnDrop<'a>(&'a Notify);
impl Drop for NotifyOnDrop<'_> {
    fn drop(&mut self) {
        self.0.notify_all();
    }
}

impl<T: UnitStruct> InnerDiscord<T> {
    /// Pump one round of gateway events. Returns `None` when the gateway is
    /// gone for good (never started, killed, or torn down after the
    /// connection closed) — the calling stream should then end. Re-calling
    /// `listen()` re-establishes a fresh gateway connection.
    // TODO: Depricate in favore of just using poll_for_events
    pub async fn poll_gateway_cache_event(&self) -> Option<()> {
        if self.killed.load(Ordering::Acquire) {
            return None;
        }
        let gateway = self.gateway.load();
        let Some(ref_gateway) = gateway.as_ref() else {
            debug!("Gateway not connected (never started, disconnected, or killed)");
            return None;
        };

        // If someone else is already pulling we just wait until they finish by looking at
        // the lock state. We also need to yield here, as try_lock isn't a future which means
        // that a stream polling at the moment might relock before ever yielding to us.
        yield_now().await;
        let Some(mut gateway_receiver) = ref_gateway.websocket.receiver.try_lock() else {
            self.pulled_notification.notified().await;
            return Some(());
        };
        let _notify_guard = NotifyOnDrop(&self.pulled_notification);
        let mut gateway_receiver = pin!(gateway_receiver.filter_payload::<Opcode>());

        select! {
        event = gateway_receiver.next() => {
            let Some(event) = event else {
                error!("Gateway connection closed; tearing it down");
                self.gateway.store(None);
                return None;
            };
            if let Err(err) = event.exec(self).await {
                warn!("Failed to execute gateway event: {err}");
            }
            trace!("poll_gateway_cache_event: main gateway event handled");
        }
        result = ref_gateway.heartbeat().fuse() => {
            if let Err(err) = result {
                error!("Gateway heartbeat failed: {err}; tearing down the connection");
                self.gateway.store(None);
                return None;
            }
        }
        };
        Some(())
    }

    pub async fn poll_for_events(self: &Arc<Self>) -> Option<()> {
        if self.killed.load(Ordering::Acquire) {
            return None;
        }
        // === Main Gateway ===
        let gateway = self.gateway.load();
        let Some(ref_gateway) = gateway.as_ref() else {
            debug!("Gateway not connected (never started, disconnected, or killed)");
            return None;
        };
        // If someone else is already pulling we just wait until they finish by looking at
        // the lock state. We also need to yield here, as try_lock isn't a future which means
        // that a stream polling at the moment might relock before ever yielding to us.
        yield_now().await;
        let Some(mut gateway_receiver) = ref_gateway.websocket.receiver.try_lock() else {
            self.pulled_notification.notified().await;
            return Some(());
        };
        // Wake the waiters above no matter how this pass ends — including
        // early returns and cancellation (the consumer stream can be
        // dropped at any await point while we hold the receiver lock).
        let _notify_guard = NotifyOnDrop(&self.pulled_notification);
        let mut gateway_receiver = pin!(gateway_receiver.filter_payload::<Opcode>());

        // === Voice Gateway ===
        let voice_gateway = ref_gateway.voice.full_load_gateway();
        let voice_gateway_clone = voice_gateway.clone();

        let mut websocket_reciver_guard;
        let mut voice_gateway_reciver = pin!(match voice_gateway.as_ref() {
            Some(voice_gateway) => {
                // Freeze suspect: taken while already holding the main
                // receiver lock; a wedged holder stalls every event pass.
                trace!("poll_for_events: waiting for voice receiver lock");
                websocket_reciver_guard = voice_gateway.websocket.receiver.lock().await;
                trace!("poll_for_events: voice receiver lock acquired");
                Either::Right(websocket_reciver_guard.filter_payload::<VoiceOpcode>())
            }
            // Eternally hang this
            None => Either::Left(stream::empty()),
        });

        // TODO: Investigate using Websocket::next_payload() diractly
        select! {
        // Main gateway
        event = gateway_receiver.next() => {
            let Some(event) = event else {
                error!("Gateway connection closed; tearing it down");
                self.gateway.store(None);
                return None;
            };
            if let Err(err) = event.exec(self).await {
                warn!("Failed to execute gateway event: {err}");
            }
            // Pairs with the "Dispatch event:"/opcode logs inside exec; if
            // this never prints after one of those, exec wedged while
            // holding the receiver lock(s).
            trace!("poll_for_events: main gateway event handled");
        }
        // voice gateway
        event = voice_gateway_reciver.next() => {
            let Some(event) = event else {
                warn!("Voice gateway connection closed; tearing down the voice session");
                ref_gateway.voice.disconnect().await;
                return Some(());
            };
            if  let Err(err) = event.exec(self).await
            {
                warn!("Failed to execute voice gateway event: {err}")
            }
            // Pairs with "VoiceOpcode: ..." inside exec; the last opcode
            // without this completion line is the handler that wedged.
            trace!("poll_for_events: voice gateway event handled");
        }
        // heartbeat over main gateway
        result = ref_gateway.heartbeat().fuse() => {
            if let Err(err) = result {
                error!("Gateway heartbeat failed: {err}; tearing down the connection");
                self.gateway.store(None);
                return None;
            }
        }
        // voice heartbeat over main gateway
        result = async {
                match voice_gateway_clone {
                    Some(voice_gateway) => voice_gateway.heartbeat().await,
                    None => futures::future::pending().await,
                }
            }.fuse() => {
            if let Err(err) = result {
                error!("Voice gateway heartbeat failed: {err}; tearing down the voice session");
                ref_gateway.voice.disconnect().await;
            }
        }
        };
        Some(())
    }
}
