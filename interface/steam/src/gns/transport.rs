//! Phases 3–5 — ICE connectivity, the SNP data channel, and Opus media.
//!
//! Once signaling (phase 2) yields a candidate UDP path, the remaining work is:
//!
//! - **ICE connectivity** — STUN-based candidate gathering and connectivity
//!   checks to pin a working path (direct only; the SDR relay fallback is
//!   proprietary, so symmetric-NAT calls are out of reach).
//! - **SNP data channel** — Valve's Steam Networking Protocol: the Curve25519
//!   ECDH + AES-GCM handshake and reliable/unreliable message framing, ported
//!   from the C++ [ValveSoftware/GameNetworkingSockets].
//! - **Opus media** — encode the microphone and decode peers, bridged to the
//!   messenger-agnostic audio graph via `AddAudioInput`/`AddAudioSource`.
//!
//! **Blocked:** all three are downstream of phases 1–2 and the SNP port has not
//! been started. Kept as one entry point until those foundations exist, since
//! none of it is independently exercisable.
//!
//! [ValveSoftware/GameNetworkingSockets]: https://github.com/ValveSoftware/GameNetworkingSockets

use steam_vent::Connection;

use super::{GnsError, VoiceSession};

/// Run the connected media path: ICE → SNP → Opus, bridged to the audio graph.
pub(crate) async fn run(conn: &Connection, session: &VoiceSession) -> Result<(), GnsError> {
    let _ = (conn, session);
    Err(GnsError::TransportUnimplemented)
}
