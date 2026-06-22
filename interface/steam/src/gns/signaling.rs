//! Phase 2 — GNS rendezvous signaling.
//!
//! To open a P2P connection, the two peers exchange small [`Rendezvous`]
//! envelopes carrying a [`ConnectRequest`]/[`ConnectOK`] handshake and trickled
//! [`IceCandidate`]s, until they agree on a UDP path. The envelope format is
//! known (these proto types); the delivery is not.
//!
//! **Status:** the rendezvous "on Steam goes through the steam backend" (Valve's
//! P2P docs). We now know the per-call signaling is standard `k_EMsgServiceMethod`
//! `VoiceChatClient.*` notifications (see `VOICE_PROTOCOL.md`), and that **no
//! rendezvous arrives passively** — as the newcomer we must *initiate* the GNS
//! connection. So this isn't waiting on a packet capture; it's waiting on the
//! GNS transport that will generate the first rendezvous and the service method
//! that delivers it to the peer (discovered once we initiate).

use steam_vent::Connection;
use steam_vent_proto::steamnetworkingsockets_messages::cmsg_steam_networking_p2prendezvous::{
    ConnectOK, ConnectRequest, ConnectionClosed,
};
use steam_vent_proto::steamnetworkingsockets_messages::{
    CMsgICECandidate as IceCandidate, CMsgSteamNetworkingP2PRendezvous as Rendezvous,
};

use super::{GnsError, VoiceSession};

/// One rendezvous signal that must be routed to (or arrives from) the peer. The
/// typed model the carrier — once recovered — would wrap and unwrap.
pub(crate) enum Signal {
    /// Initiate the P2P connection.
    Connect(ConnectRequest),
    /// Accept an incoming connection.
    Accept(ConnectOK),
    /// Tear the connection down.
    Close(ConnectionClosed),
    /// A trickled ICE candidate for NAT traversal.
    Candidate(IceCandidate),
}

/// Route a rendezvous envelope to the peer through Steam's signaling backend.
pub(crate) async fn send_rendezvous(
    conn: &Connection,
    envelope: Rendezvous,
) -> Result<(), GnsError> {
    let _ = (conn, envelope);
    Err(GnsError::SignalingCarrierUnknown)
}

/// Open the rendezvous exchange for `session`: send our `ConnectRequest`, gather
/// and trickle candidates, and drive it to a connected UDP path.
pub(crate) async fn open_rendezvous(
    conn: &Connection,
    session: &VoiceSession,
) -> Result<(), GnsError> {
    let _ = (conn, session);
    Err(GnsError::SignalingCarrierUnknown)
}
