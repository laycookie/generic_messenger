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
use steam_vent_proto::protobuf::Message as _;
use steam_vent_proto::steamnetworkingsockets_messages::cmsg_steam_networking_p2prendezvous::{
    ConnectOK, ConnectRequest, ConnectionClosed,
};
use steam_vent_proto::steamnetworkingsockets_messages::{
    CMsgICECandidate as IceCandidate, CMsgSteamNetworkingP2PRendezvous as Rendezvous,
};

use super::{GnsError, VoiceSession};

/// Try to decode a raw CM message as a P2P media rendezvous and, if it plausibly
/// is one, classify how Steam is wiring the media. **This is the decisive
/// diagnostic** for the whole voice-media question:
///
/// - `ice_enabled` / `reliable_messages` (trickled ICE candidates) ⇒ a
///   direct/ICE path our open-GNS transport (`crate::gns`) *can* connect to.
/// - `sdr_routes` ⇒ Steam Datagram Relay — the closed-source relay, unreachable
///   from a third-party client.
///
/// Returns `None` when `data` isn't a rendezvous (protobuf decodes permissively,
/// so we require a signature field to avoid false positives).
pub(crate) fn classify_rendezvous(data: &[u8]) -> Option<String> {
    let msg = Rendezvous::parse_from_bytes(data).ok()?;
    let is_rendezvous = msg.connect_request.is_some()
        || msg.connect_ok.is_some()
        || msg.connection_closed.is_some()
        || msg.sdr_routes.is_some()
        || (msg.ice_enabled.is_some()
            && (msg.from_identity.is_some() || msg.to_identity.is_some()));
    if !is_rendezvous {
        return None;
    }

    let sdr_len = msg.sdr_routes.as_ref().map_or(0, |routes| routes.len());
    let path = if sdr_len > 0 {
        "SDR (closed relay — unreachable)"
    } else if msg.ice_enabled.unwrap_or(false) || !msg.reliable_messages.is_empty() {
        "ICE (direct — reachable by our transport)"
    } else {
        "unknown"
    };
    Some(format!(
        "P2P RENDEZVOUS [{path}] ice_enabled={:?} sdr_routes={sdr_len}B hosted_ticket={:?}B \
         reliable_msgs={} connect_request={} connect_ok={} closed={} from={:?} to={:?}",
        msg.ice_enabled,
        msg.hosted_server_ticket.as_ref().map(|ticket| ticket.len()),
        msg.reliable_messages.len(),
        msg.connect_request.is_some(),
        msg.connect_ok.is_some(),
        msg.connection_closed.is_some(),
        msg.from_identity,
        msg.to_identity,
    ))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use steam_vent_proto::protobuf::MessageField;

    #[test]
    fn classifies_ice_rendezvous_as_reachable() {
        let mut rendezvous = Rendezvous::new();
        rendezvous.from_identity = Some("steamid:1".into());
        rendezvous.ice_enabled = Some(true);
        rendezvous.connect_request = MessageField::some(ConnectRequest::new());
        let bytes = rendezvous.write_to_bytes().unwrap();

        let classification = classify_rendezvous(&bytes).expect("recognized as a rendezvous");
        assert!(classification.contains("ICE"), "{classification}");
        assert!(!classification.contains("SDR"), "{classification}");
    }

    #[test]
    fn classifies_sdr_rendezvous_as_closed() {
        let mut rendezvous = Rendezvous::new();
        rendezvous.from_identity = Some("steamid:1".into());
        rendezvous.sdr_routes = Some(vec![1, 2, 3, 4]);
        let bytes = rendezvous.write_to_bytes().unwrap();

        let classification = classify_rendezvous(&bytes).expect("recognized as a rendezvous");
        assert!(classification.contains("SDR"), "{classification}");
    }

    #[test]
    fn ignores_non_rendezvous_bytes() {
        // An empty rendezvous (no signature fields) is not treated as one.
        let empty = Rendezvous::new().write_to_bytes().unwrap();
        assert!(classify_rendezvous(&empty).is_none());
    }
}
