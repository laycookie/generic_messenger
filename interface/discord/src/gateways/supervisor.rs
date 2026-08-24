//! Main gateway ownership and connection initialization.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use futures_locks::Mutex;

use super::{Gateway, Handshake as _, Handshaking, general::General};
use crate::{Capabilities, Intents, rest_api::DiscordApi};

/// Sole owner of main-gateway configuration and connection state.
pub(crate) struct GatewaySupervisor {
    intents: Intents,
    capabilities: Capabilities,
    pub(crate) current: ArcSwapOption<Gateway<General>>,
    /// Serializes gateway turns; owned guard follows committed work.
    pub(crate) turn: Mutex<()>,
}

impl GatewaySupervisor {
    pub(crate) fn new(intents: Intents, capabilities: Capabilities) -> Self {
        Self {
            intents,
            capabilities,
            current: ArcSwapOption::empty(),
            turn: Mutex::new(()),
        }
    }

    pub(crate) async fn ensure(
        &self,
        api: &DiscordApi,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Teardown unpublishes while holding `turn`.
        if self.current.load().is_some() {
            return Ok(());
        }

        // Recheck after shared turn/initialization permit.
        let _permit = self.turn.lock().await;
        if self.current.load().is_some() {
            return Ok(());
        }

        let gateway = Gateway::<General, Handshaking>::new(
            api.token(),
            self.intents.bits(),
            self.capabilities.bits(),
        )
        .await?
        .handshake()
        .await?;
        self.current.store(Some(Arc::new(gateway)));
        Ok(())
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
