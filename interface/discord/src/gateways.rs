use std::marker::PhantomData;

mod frame;
mod state;
mod supervisor;
mod transport;

pub mod general;
pub mod media;
mod polling;

use state::Connected;

pub(crate) struct Gateway<T, S = Connected>
where
    S: GatewayState<T>,
{
    websocket: Websocket,
    state: S::Data,
    _marker: PhantomData<fn() -> (T, S)>,
}

pub(crate) use frame::{GatewayFrame, GatewayFrameConversionError, GatewayPayload};
pub(crate) use polling::{GatewayTurn, finish_committed_gateway_turn};
pub(crate) use state::{
    GatewayState, Handshake, Handshaking, Heartbeat, expect_hello, with_timeout,
};
pub(crate) use supervisor::{GatewaySupervisor, ReconnectMode};
pub(crate) use transport::{GatewayStreamExt, Websocket};
