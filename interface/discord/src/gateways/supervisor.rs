//! Main gateway ownership and connection initialization.

use std::{
    io,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use arc_swap::ArcSwapOption;
use async_lock::Mutex;
use futures_timer::Delay;
use tracing::warn;

use super::{
    Gateway, Handshake as _, Handshaking,
    general::{GatewayContinuity, General},
    polling::GatewaySession,
};
use crate::{Capabilities, Intents, rest_api::DiscordApi};

const MAX_RESUME_SETUP_RETRIES: u8 = 3;
const MAX_RESUME_PROTOCOL_ATTEMPTS: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReconnectMode {
    Resume,
    ResumeAfter(Duration),
    Reidentify,
    ReidentifyAfter(Duration),
}

impl ReconnectMode {
    pub(crate) fn wants_resume(self) -> bool {
        matches!(self, Self::Resume | Self::ResumeAfter(_))
    }

    pub(crate) fn delay(self) -> Option<Duration> {
        match self {
            Self::ResumeAfter(delay) | Self::ReidentifyAfter(delay) => Some(delay),
            Self::Resume | Self::Reidentify => None,
        }
    }
}

#[derive(Clone)]
struct ResumeState {
    session_id: String,
    gateway_url: String,
    sequence: u64,
    continuity: GatewayContinuity,
    setup_retries: u8,
    protocol_attempts: u8,
}

impl ResumeState {
    fn advance(&mut self, sequence: u64) {
        self.sequence = self.sequence.max(sequence);
    }

    fn next_setup_retry(&mut self) -> Option<Duration> {
        if self.setup_retries >= MAX_RESUME_SETUP_RETRIES {
            return None;
        }
        let delay = Duration::from_secs(1_u64 << self.setup_retries);
        self.setup_retries += 1;
        Some(delay)
    }

    fn begin_protocol_attempt(&mut self) {
        self.setup_retries = 0;
        self.protocol_attempts = self.protocol_attempts.saturating_add(1);
    }

    fn may_retry_protocol(&self) -> bool {
        self.protocol_attempts < MAX_RESUME_PROTOCOL_ATTEMPTS
    }

    fn confirm(&mut self) {
        self.setup_retries = 0;
        self.protocol_attempts = 0;
    }
}

fn resume_setup_is_retryable(error: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    !error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::InvalidData
                | io::ErrorKind::InvalidInput
                | io::ErrorKind::PermissionDenied
        )
    })
}

/// Sole owner of main-gateway configuration, transport generations, and the
/// protocol session data needed to replay a disconnected generation safely.
pub(crate) struct GatewaySupervisor {
    intents: Intents,
    capabilities: Capabilities,
    pub(crate) current: ArcSwapOption<Gateway<General>>,
    /// Transport driver and ordered queue for the currently published root.
    pub(super) session: ArcSwapOption<GatewaySession>,
    /// Serializes gateway turns; owned guard follows committed work.
    pub(crate) turn: Arc<Mutex<()>>,
    resume: StdMutex<Option<ResumeState>>,
    reconnect_not_before: StdMutex<Option<Instant>>,
    continuity_lost: AtomicBool,
}

impl GatewaySupervisor {
    pub(crate) fn new(intents: Intents, capabilities: Capabilities) -> Self {
        Self {
            intents,
            capabilities,
            current: ArcSwapOption::empty(),
            session: ArcSwapOption::empty(),
            turn: Arc::new(Mutex::new(())),
            resume: StdMutex::new(None),
            reconnect_not_before: StdMutex::new(None),
            continuity_lost: AtomicBool::new(false),
        }
    }

    async fn wait_for_reconnect_window(&self) {
        loop {
            let deadline = *self
                .reconnect_not_before
                .lock()
                .expect("gateway reconnect deadline mutex poisoned");
            if let Some(remaining) =
                deadline.and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            {
                // Leave the deadline stored while awaiting so cancellation
                // cannot bypass Discord's Invalid Session backoff.
                Delay::new(remaining).await;
                continue;
            }
            self.reconnect_not_before
                .lock()
                .expect("gateway reconnect deadline mutex poisoned")
                .take();
            return;
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
        self.wait_for_reconnect_window().await;

        let resume = self
            .resume
            .lock()
            .expect("gateway Resume state mutex poisoned")
            .clone();
        let gateway = if let Some(resume) = resume {
            let attempt = match Gateway::<General, Handshaking>::resume(
                api.token(),
                &resume.session_id,
                &resume.gateway_url,
                resume.sequence,
                resume.continuity,
            )
            .await
            {
                Ok(gateway) => gateway.handshake().await,
                Err(error) => Err(error),
            };
            match attempt {
                Ok(gateway) => {
                    if let Some(resume) = self
                        .resume
                        .lock()
                        .expect("gateway Resume state mutex poisoned")
                        .as_mut()
                    {
                        resume.begin_protocol_attempt();
                    }
                    gateway
                }
                Err(error) => {
                    let retry_delay = resume_setup_is_retryable(error.as_ref())
                        .then(|| {
                            self.resume
                                .lock()
                                .expect("gateway Resume state mutex poisoned")
                                .as_mut()
                                .and_then(ResumeState::next_setup_retry)
                        })
                        .flatten();
                    if let Some(delay) = retry_delay {
                        self.defer_reconnect(delay);
                        warn!(
                            error = %error,
                            sequence = resume.sequence,
                            ?delay,
                            "Discord Gateway Resume setup failed transiently; retaining the session"
                        );
                        return Err(error);
                    }

                    warn!(
                        error = %error,
                        sequence = resume.sequence,
                        "Discord Gateway Resume setup failed permanently or exhausted retries; falling back to a fresh Identify"
                    );
                    self.resume
                        .lock()
                        .expect("gateway Resume state mutex poisoned")
                        .take();
                    self.continuity_lost.store(true, Ordering::Release);
                    Gateway::<General, Handshaking>::new(
                        api.token(),
                        self.intents.bits(),
                        self.capabilities.bits(),
                    )
                    .await?
                    .handshake()
                    .await?
                }
            }
        } else {
            Gateway::<General, Handshaking>::new(
                api.token(),
                self.intents.bits(),
                self.capabilities.bits(),
            )
            .await?
            .handshake()
            .await?
        };
        let gateway = Arc::new(gateway);
        let session = GatewaySession::start(Arc::clone(&gateway));
        // Publish the queue first. Any listener that observes it waits on this
        // still-held turn permit until the matching root pointer is visible.
        self.session.store(Some(session));
        self.current.store(Some(gateway));
        Ok(())
    }

    pub(crate) fn register_resume_session(
        &self,
        gateway: &Gateway<General>,
        session_id: String,
        gateway_url: String,
        sequence: u64,
    ) {
        let current = self.current.load_full();
        if session_id.is_empty()
            || gateway_url.is_empty()
            || !current
                .as_ref()
                .is_some_and(|current| std::ptr::eq(current.as_ref(), gateway))
        {
            return;
        }
        *self
            .resume
            .lock()
            .expect("gateway Resume state mutex poisoned") = Some(ResumeState {
            session_id,
            gateway_url,
            sequence,
            continuity: gateway.continuity(),
            setup_retries: 0,
            protocol_attempts: 0,
        });
    }

    pub(crate) fn commit_sequence(&self, gateway: &Gateway<General>, sequence: u64) {
        let current = self.current.load_full();
        if !current
            .as_ref()
            .is_some_and(|current| std::ptr::eq(current.as_ref(), gateway))
        {
            return;
        }
        if let Some(resume) = self
            .resume
            .lock()
            .expect("gateway Resume state mutex poisoned")
            .as_mut()
            && resume.continuity.belongs_to(gateway)
        {
            resume.advance(sequence);
        }
    }

    pub(crate) fn confirm_resume_session(&self, gateway: &Gateway<General>) {
        let current = self.current.load_full();
        if !current
            .as_ref()
            .is_some_and(|current| std::ptr::eq(current.as_ref(), gateway))
        {
            return;
        }
        if let Some(resume) = self
            .resume
            .lock()
            .expect("gateway Resume state mutex poisoned")
            .as_mut()
            && resume.continuity.belongs_to(gateway)
        {
            resume.confirm();
        }
    }

    pub(crate) fn can_resume(&self, gateway: &Gateway<General>) -> bool {
        !gateway.events.is_invalidated()
            && self
                .resume
                .lock()
                .expect("gateway Resume state mutex poisoned")
                .as_ref()
                .is_some_and(|resume| {
                    resume.continuity.belongs_to(gateway)
                        && (!gateway.awaiting_resumed() || resume.may_retry_protocol())
                })
    }

    pub(crate) fn discard_resume_session(&self, gateway: &Gateway<General>) {
        let mut resume = self
            .resume
            .lock()
            .expect("gateway Resume state mutex poisoned");
        if resume
            .as_ref()
            .is_some_and(|resume| resume.continuity.belongs_to(gateway))
        {
            resume.take();
        }
    }

    pub(crate) fn defer_reconnect(&self, delay: Duration) {
        let deadline = Instant::now() + delay;
        let mut pending = self
            .reconnect_not_before
            .lock()
            .expect("gateway reconnect deadline mutex poisoned");
        *pending = Some(pending.map_or(deadline, |current| current.max(deadline)));
    }

    pub(crate) fn take_continuity_loss(&self) -> bool {
        self.continuity_lost.swap(false, Ordering::AcqRel)
    }

    /// Atomically unpublish the session matching `gateway`. Callers hold the
    /// turn permit, so initialization cannot interleave with these two stores.
    pub(super) fn unpublish(&self, gateway: &Gateway<General>) -> Option<Arc<GatewaySession>> {
        let current = self.current.load_full()?;
        if !std::ptr::eq(current.as_ref(), gateway) {
            return None;
        }
        // Hide the root first so lock-free control-plane readers never observe
        // a live gateway after its driver queue has disappeared.
        self.current.store(None);
        self.session.swap(None)
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

    #[test]
    fn committed_resume_sequence_never_regresses() {
        let mut resume = ResumeState {
            session_id: "session".into(),
            gateway_url: "wss://gateway.discord.gg".into(),
            sequence: 41,
            continuity: GatewayContinuity::default(),
            setup_retries: 0,
            protocol_attempts: 0,
        };

        resume.advance(43);
        resume.advance(42);

        assert_eq!(resume.sequence, 43);
    }

    #[test]
    fn resume_setup_retries_are_bounded_and_reset_after_confirmation() {
        let mut resume = ResumeState {
            session_id: "session".into(),
            gateway_url: "wss://gateway.discord.gg".into(),
            sequence: 41,
            continuity: GatewayContinuity::default(),
            setup_retries: 0,
            protocol_attempts: 0,
        };

        assert_eq!(resume.next_setup_retry(), Some(Duration::from_secs(1)));
        assert_eq!(resume.next_setup_retry(), Some(Duration::from_secs(2)));
        assert_eq!(resume.next_setup_retry(), Some(Duration::from_secs(4)));
        assert_eq!(resume.next_setup_retry(), None);
        resume.confirm();
        assert_eq!(resume.next_setup_retry(), Some(Duration::from_secs(1)));
    }

    #[test]
    fn pre_resumed_protocol_retries_are_bounded() {
        let mut resume = ResumeState {
            session_id: "session".into(),
            gateway_url: "wss://gateway.discord.gg".into(),
            sequence: 41,
            continuity: GatewayContinuity::default(),
            setup_retries: 2,
            protocol_attempts: 0,
        };

        resume.begin_protocol_attempt();
        assert_eq!(resume.setup_retries, 0);
        assert!(resume.may_retry_protocol());
        resume.begin_protocol_attempt();
        assert!(resume.may_retry_protocol());
        resume.begin_protocol_attempt();
        assert!(!resume.may_retry_protocol());
        resume.confirm();
        assert!(resume.may_retry_protocol());
    }

    #[test]
    fn reconnect_modes_preserve_protocol_intent_and_delay() {
        let delay = Duration::from_secs(3);
        assert!(ReconnectMode::Resume.wants_resume());
        assert!(ReconnectMode::ResumeAfter(delay).wants_resume());
        assert!(!ReconnectMode::Reidentify.wants_resume());
        assert_eq!(ReconnectMode::ResumeAfter(delay).delay(), Some(delay));
        assert_eq!(ReconnectMode::ReidentifyAfter(delay).delay(), Some(delay));
    }
}
