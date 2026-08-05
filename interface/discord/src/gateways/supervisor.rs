//! Main gateway ownership and backend stream lifecycle.

use std::{
    pin::pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Poll,
};

use asyncs_sync::Notify;
use futures::{future::poll_fn, lock::Mutex};

use super::{Gateway, Handshake as _, Handshaking, general::General};
use crate::{Capabilities, Intents, lazy_arc::LazyArc, rest_api::DiscordApi};

/// Sole owner of main-gateway configuration and backend stream lifecycle.
pub(crate) struct GatewaySupervisor {
    intents: Intents,
    capabilities: Capabilities,
    pub(crate) current: LazyArc<Gateway<General>>,
    /// Serializes selecting and fully executing one gateway turn. Event
    /// listeners may cancel while waiting for this lock or for a websocket
    /// frame, but retain the guard after a frame is consumed so dispatch can
    /// never overlap or be abandoned halfway through.
    pub(crate) turn: Mutex<()>,
    pub(crate) killed: AtomicBool,
    pub(crate) kill_notify: Notify,
    pub(crate) active_streams: AtomicUsize,
}

impl GatewaySupervisor {
    pub(crate) fn new(intents: Intents, capabilities: Capabilities) -> Self {
        Self {
            intents,
            capabilities,
            current: Default::default(),
            turn: Mutex::new(()),
            killed: AtomicBool::new(false),
            kill_notify: Default::default(),
            active_streams: AtomicUsize::new(0),
        }
    }

    pub(crate) async fn ensure(
        &self,
        api: &DiscordApi,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.killed.load(Ordering::Acquire) {
            return Err("messenger was dropped".into());
        }
        self.current
            .get_or_try_init(async || {
                Gateway::<General, Handshaking>::new(
                    api.token(),
                    self.intents.bits(),
                    self.capabilities.bits(),
                )
                .await?
                .handshake()
                .await
            })
            .await?;
        Ok(())
    }

    pub(crate) fn kill(&self) {
        self.killed.store(true, Ordering::Release);
        self.current.store(None);
        self.kill_notify.notify_all();
    }

    pub(crate) async fn killed_signal(&self) {
        if self.killed.load(Ordering::Acquire) {
            return;
        }
        let mut notified = pin!(self.kill_notify.notified());
        poll_fn(|cx| {
            if self.killed.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            if notified.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
            if self.killed.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
            Poll::Pending
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::pin, task::Poll};

    use futures::{FutureExt as _, poll};

    use super::*;

    #[test]
    fn selected_gateway_turn_remains_exclusive_until_execution_finishes() {
        smol::block_on(async {
            let supervisor = GatewaySupervisor::new(Intents::empty(), Capabilities::empty());
            let selected_turn = supervisor.turn.lock().await;
            let mut competing_listener = pin!(supervisor.turn.lock().fuse());

            assert!(matches!(poll!(competing_listener.as_mut()), Poll::Pending));

            drop(selected_turn);

            assert!(matches!(poll!(competing_listener.as_mut()), Poll::Ready(_)));
        });
    }
}
