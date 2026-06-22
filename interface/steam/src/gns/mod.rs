//! Steam voice media over GameNetworkingSockets (GNS) P2P — **integration skeleton**.
//!
//! This is the not-yet-functional media half of Steam voice. The signaling half
//! (`ChatRoom.JoinVoiceChat`) lives in [`crate::voice`]; once a session is
//! joined, *this* module is where the audio path would be brought up and handed
//! back as the `CallStreamReady` audio stream.
//!
//! It is deliberately a **shell**: the protocol is modelled with the real
//! `steam_vent_proto` types and the work is split into the ordered [`Phase`]s
//! below, but each phase's integration point returns the precise [`GnsError`]
//! explaining why it cannot run yet. No phase fabricates behaviour. As each
//! becomes unblocked it gets implemented in place; the call sequence in
//! [`establish_voice_media`] already wires them together.
//!
//! # Phases
//!
//! 1. [`Phase::NetworkingCert`] ([`cert`]) — obtain the Steam-signed networking
//!    certificate that identifies us in the GNS crypto handshake.
//! 2. [`Phase::Signaling`] ([`signaling`]) — exchange the GNS rendezvous
//!    (`ConnectRequest`/`ConnectOK` + ICE candidates) with the peer through
//!    Steam's signaling backend.
//! 3. [`Phase::IceConnectivity`] ([`transport`]) — STUN-based candidate
//!    gathering and connectivity checks to establish a UDP path.
//! 4. [`Phase::SnpTransport`] ([`transport`]) — the SNP data channel: Curve25519
//!    ECDH + AES-GCM and reliable/unreliable framing, ported from Valve's C++
//!    GameNetworkingSockets.
//! 5. [`Phase::OpusMedia`] ([`transport`]) — Opus encode/decode bridged to the
//!    messenger-agnostic audio graph (`AddAudioSource`/`AddAudioInput`).
//!
//! # Why each foundation phase is blocked
//!
//! - **Signaling carrier (phase 2)** is the gating unknown. Per Valve's P2P
//!   docs, on Steam the rendezvous "goes through the steam backend": the
//!   envelope (`CMsgSteamNetworkingP2PRendezvous`) is in the protos, but the CM
//!   EMsg that *delivers* it to the peer is proprietary and absent from
//!   `steam_vent_proto`. Recovering it needs a packet capture of a live call.
//! - **Networking cert (phase 1)** is now implemented ([`cert`]): the reply has
//!   no `RpcMessageWithKind` binding in `steam_vent_proto`, but a local newtype
//!   may impl the foreign trait (orphan rule permitting), so `Connection::job`
//!   round-trips it with no proto fork.
//! - **SNP transport (phases 3–5)** is a from-scratch port of the GNS data
//!   channel; large, and only worth starting once phases 1–2 are unblocked.
//!
//! See the `project_steam_voice_no_media_transport` memory note for the full
//! findings.

// Forward-declaration shell: most items model phases that are blocked on
// protocol recovery and are not yet called. Suppress dead-code noise until the
// phases are implemented (the `establish_voice_media` sequence already
// references the per-phase entry points).
#![allow(dead_code)]

use std::error::Error;
use std::fmt;

use steam_vent::Connection;

use messenger_interface::types::ID;

pub(crate) mod cert;
pub(crate) mod signaling;
pub(crate) mod transport;

/// The ordered phases of bringing up a GNS voice media path. See the module
/// docs for what each entails and why the foundation phases are blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    NetworkingCert,
    Signaling,
    IceConnectivity,
    SnpTransport,
    OpusMedia,
}

/// A joined Steam voice session — the handle `ChatRoom.JoinVoiceChat` yields
/// (see [`crate::voice`]), from which the media path is established.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VoiceSession {
    pub(crate) client_steamid: ID,
    /// The `voice_chatid` returned by `JoinVoiceChat`.
    pub(crate) voice_chatid: u64,
}

/// The precise reason a GNS phase cannot run yet. Each variant names the
/// concrete protocol gap rather than a generic "unimplemented", so the gating
/// work is unambiguous.
#[derive(Debug)]
pub(crate) enum GnsError {
    /// Phase 2: the CM carrier EMsg that delivers a rendezvous to the peer is
    /// proprietary and absent from `steam_vent_proto`; recover via packet
    /// capture.
    SignalingCarrierUnknown,
    /// Phase 1: the networking cert request RPC failed at runtime (network error
    /// or a rejected request).
    CertRequest(String),
    /// Phases 3–5: the SNP data channel (crypto + framing + Opus) is not yet
    /// ported from Valve's C++ GameNetworkingSockets.
    TransportUnimplemented,
}

impl GnsError {
    /// The phase this error gates.
    pub(crate) fn phase(&self) -> Phase {
        match self {
            GnsError::CertRequest(_) => Phase::NetworkingCert,
            GnsError::SignalingCarrierUnknown => Phase::Signaling,
            GnsError::TransportUnimplemented => Phase::SnpTransport,
        }
    }
}

impl fmt::Display for GnsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GnsError::SignalingCarrierUnknown => f.write_str(
                "GNS media not negotiated: as the newcomer we must initiate the P2P connection \
                 (send the first rendezvous), which needs the GNS transport — not built yet",
            ),
            GnsError::CertRequest(err) => write!(f, "networking cert request failed: {err}"),
            GnsError::TransportUnimplemented => {
                f.write_str("GNS SNP data channel is not yet ported")
            }
        }
    }
}

impl Error for GnsError {}

/// Bring up the GNS media path for a joined voice session, ultimately yielding
/// the audio stream that [`crate::voice`] would surface as `CallStreamReady`.
///
/// Sequences the phases in order, so the returned error is the *first* blocking
/// phase — today [`GnsError::SignalingCarrierUnknown`], since phase 1 (the
/// networking cert) is now implemented and runs for real.
pub(crate) async fn establish_voice_media(
    conn: &Connection,
    session: VoiceSession,
) -> Result<(), GnsError> {
    let _identity = cert::request_networking_cert(conn).await?; // Phase 1 (implemented)
    signaling::open_rendezvous(conn, &session).await?; // Phase 2
    transport::run(conn, &session).await // Phases 3–5
}
