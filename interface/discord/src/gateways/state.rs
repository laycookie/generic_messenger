use std::{
    error::Error,
    io,
    ops::Deref,
    pin::{Pin, pin},
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_tungstenite::tungstenite::Message as WebsocketMessage;
use facet::Facet;
use futures::{
    StreamExt,
    future::{Either, select},
    lock::Mutex as AsyncMutex,
};
use futures_timer::Delay;
use tracing::warn;

use crate::random::SystemRandom;

use super::{
    Gateway, GatewayFrame, GatewayFrameConversionError, GatewayPayload, GatewayStreamExt, Websocket,
};

pub(crate) struct Handshaking;
pub(crate) struct Connected;

pub(crate) trait GatewayState<T> {
    type Data;
}

pub(crate) struct ConnectedData<T> {
    pub(super) heartbeat_timer: AsyncMutex<HeartbeatTimer>,
    pub(super) heartbeat_acknowledged: HeartbeatAcknowledgement,
    heartbeat_in_flight: AtomicBool,
    /// Last sequence observed by the transport driver, used for heartbeat
    /// payloads. Resume uses the separately tracked last *executed* sequence so
    /// queued-but-unapplied dispatches are replayed rather than skipped.
    pub(super) last_sequence_number: OnceLock<AtomicU64>,
    pub(super) type_specific_data: T,
}

pub(super) struct HeartbeatAcknowledgement(AtomicU8);

impl Default for HeartbeatAcknowledgement {
    fn default() -> Self {
        Self(AtomicU8::new(0))
    }
}

impl HeartbeatAcknowledgement {
    pub(super) async fn allow_dispatch_grace(&self) {
        if self.0.load(Ordering::Acquire) != 0 {
            // The websocket ACK and heartbeat deadline are selected by separate
            // driver branches. Give an already-ready ACK one short scheduling
            // window to update this atomic, not an entire heartbeat interval.
            Delay::new(Duration::from_millis(50)).await;
        }
    }

    pub(super) fn check_before_heartbeat(&self) -> io::Result<()> {
        if self.0.load(Ordering::Acquire) == 0 {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Discord gateway missed its heartbeat acknowledgement",
            ))
        }
    }

    pub(super) fn mark_sent(&self) {
        self.0.store(1, Ordering::Release);
    }

    pub(super) fn acknowledge(&self) {
        self.0.store(0, Ordering::Release);
    }
}

impl<T> GatewayState<T> for Connected {
    type Data = ConnectedData<T>;
}

impl<T, S> Gateway<T, S>
where
    S: GatewayState<T>,
{
    pub(crate) async fn send(
        &self,
        msg: WebsocketMessage,
    ) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.websocket.send(msg).await
    }
}

impl Websocket {
    pub(super) fn into_gateway<T>(self, heartbeat_interval: Duration, data: T) -> Gateway<T> {
        Gateway {
            websocket: self,
            state: ConnectedData {
                heartbeat_timer: HeartbeatTimer::new(heartbeat_interval).into(),
                heartbeat_acknowledged: HeartbeatAcknowledgement::default(),
                heartbeat_in_flight: AtomicBool::new(false),
                last_sequence_number: OnceLock::new(),
                type_specific_data: data,
            },
            _marker: std::marker::PhantomData,
        }
    }
}

pub(crate) async fn with_timeout<F, T, E>(
    future: F,
    timeout: Duration,
    operation: &str,
) -> Result<T, Box<dyn Error + Send + Sync>>
where
    F: Future<Output = Result<T, E>>,
    E: Into<Box<dyn Error + Send + Sync>>,
{
    match select(Box::pin(future), Box::pin(Delay::new(timeout))).await {
        Either::Left((result, _)) => result.map_err(Into::into),
        Either::Right(((), _)) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{operation} exceeded {} second deadline", timeout.as_secs()),
        )
        .into()),
    }
}

pub(crate) async fn expect_hello<Op, H>(
    websocket: &Websocket,
    is_hello: impl FnOnce(&Op) -> bool,
    gateway_name: &str,
) -> Result<H, Box<dyn Error + Send + Sync>>
where
    Op: for<'facet> Facet<'facet> + TryFrom<u8>,
    H: Facet<'static>,
    GatewayFrame<Op>: TryFrom<WebsocketMessage, Error = GatewayFrameConversionError>,
{
    let mut receiver = websocket.receiver.lock().await;
    let mut frames = pin!(receiver.gateway_frames::<Op>());

    loop {
        let frame = frames.next().await.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{gateway_name} closed before receiving hello"),
            )
        })?;
        let payload: GatewayPayload<'_, Op> = match frame.payload() {
            Ok(payload) => payload,
            Err(err) => {
                warn!("Failed to parse {gateway_name} payload while waiting for Hello: {err}");
                continue;
            }
        };

        if !is_hello(payload.opcode()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{gateway_name} sent a non-Hello first event"),
            )
            .into());
        }

        return Ok(payload.decode()?);
    }
}

#[allow(async_fn_in_trait)]
pub(crate) trait Handshake: Sized {
    type Output;

    async fn handshake(self) -> Result<Self::Output, Box<dyn Error + Send + Sync>>;
}

/// Protocol-specific scheduling and sending of Discord heartbeats.
#[allow(async_fn_in_trait)]
pub(crate) trait Heartbeat {
    async fn heartbeat(&self) -> Result<(), async_tungstenite::tungstenite::Error>;
    async fn heartbeat_now(&self) -> Result<(), async_tungstenite::tungstenite::Error>;
}

impl<T> Gateway<T> {
    /// Wait only for the heartbeat deadline. The transport driver selects this
    /// cancellation-safe phase, then runs [`Heartbeat::heartbeat`] outside the
    /// receiver select so an in-progress websocket write is never discarded by
    /// an unrelated event or topology wakeup.
    pub(crate) async fn wait_for_heartbeat(&self) {
        self.state.heartbeat_timer.lock().await.wait().await;
    }

    pub(crate) fn heartbeat_in_flight(&self) -> bool {
        self.state.heartbeat_in_flight.load(Ordering::Acquire)
    }

    pub(crate) fn try_begin_heartbeat(&self) -> bool {
        self.state
            .heartbeat_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn finish_heartbeat(&self) {
        self.state
            .heartbeat_in_flight
            .store(false, Ordering::Release);
    }
}

impl<T> Deref for Gateway<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.state.type_specific_data
    }
}

pub(super) struct HeartbeatTimer {
    duration: Duration,
    // Keep the deadline across canceled wait branches created by transport
    // driver topology/event select rounds.
    future: Pin<Box<dyn Future<Output = ()> + Send + Sync>>,
}

fn randomized_initial_heartbeat_delay(duration: Duration) -> Duration {
    duration.mul_f64(SystemRandom::default().unit_interval())
}

impl HeartbeatTimer {
    fn new(duration: Duration) -> Self {
        // Discord requires the first heartbeat at a random point in
        // [0, heartbeat_interval); subsequent heartbeats use the full interval.
        let initial_delay = randomized_initial_heartbeat_delay(duration);
        Self {
            duration,
            future: Box::pin(Delay::new(initial_delay)),
        }
    }

    pub(super) async fn wait(&mut self) {
        (&mut self.future).await;
    }

    /// Advance only after a heartbeat was actually written. Canceling a due
    /// wait branch for an event/topology refresh leaves the completed deadline
    /// ready, so the driver retries immediately on its next select round.
    pub(super) fn mark_sent(&mut self) {
        self.future = Box::pin(Delay::new(self.duration));
    }
}

#[cfg(test)]
mod tests {
    use futures::future;

    use super::*;

    #[test]
    fn heartbeat_rejects_an_unacknowledged_previous_send() {
        let acknowledgement = HeartbeatAcknowledgement::default();
        acknowledgement.check_before_heartbeat().unwrap();
        acknowledgement.mark_sent();
        assert_eq!(
            acknowledgement.check_before_heartbeat().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        acknowledgement.acknowledge();
        acknowledgement.check_before_heartbeat().unwrap();
    }

    #[test]
    fn initial_heartbeat_jitter_stays_within_the_interval() {
        let interval = Duration::from_secs(45);
        for _ in 0..128 {
            assert!(randomized_initial_heartbeat_delay(interval) <= interval);
        }
    }

    #[test]
    fn timeout_returns_ready_result() {
        let result = futures::executor::block_on(with_timeout(
            future::ready::<io::Result<_>>(Ok(7)),
            Duration::from_secs(1),
            "test operation",
        ));

        assert_eq!(result.unwrap(), 7);
    }

    #[test]
    fn timeout_reports_operation_and_io_kind() {
        let error = futures::executor::block_on(with_timeout(
            future::pending::<io::Result<()>>(),
            Duration::from_millis(1),
            "test operation",
        ))
        .unwrap_err();

        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::TimedOut)
        );
        assert!(error.to_string().contains("test operation"));
    }
}
