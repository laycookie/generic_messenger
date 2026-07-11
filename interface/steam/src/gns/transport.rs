//! Phases 3–5 — the voice media transport, and the seam the Opus bridge runs on.
//!
//! The media path is split at the [`VoiceTransport`] trait so the reusable,
//! testable audio work ([`super::audio`]) is decoupled from the still-blocked GNS
//! transport:
//!
//! - [`GnsTransport`] — the real GNS P2P transport. `connect` runs the cert
//!   (phase 1, done) → rendezvous (phase 2) → ICE/SNP (phases 3–4). It is
//!   **blocked** on the `gns-sys` FFI (custom signaling + `SetCertificate`, which
//!   the safe wrapper doesn't expose) and the live rendezvous carrier, so it
//!   returns the precise gating [`GnsError`] today. This is the *only* genuinely
//!   blocked joint in the media plane.
//! - [`LoopbackTransport`] (test-only) — an in-memory echo used to exercise the
//!   whole audio bridge without a network. See `super::audio`'s tests.
//!
//! Once ICE/SNP is ported, `establish_voice_media` drives [`super::audio::run`]
//! over the connected transport and surfaces the stream as `CallStreamReady`.

use std::error::Error;
use std::fmt;

use async_trait::async_trait;
use steam_vent::Connection;
use tracing::debug;

use super::cert::NetworkingIdentity;
use super::{GnsError, VoiceSession, signaling};

/// One Opus-encoded audio frame (48 kHz stereo, 20 ms) crossing the transport.
pub(crate) struct VoiceFrame {
    /// SteamID of the speaker, on incoming frames. Ignored on outgoing frames
    /// (the transport knows its own peers).
    pub(crate) peer: u64,
    /// The Opus payload.
    pub(crate) opus: Vec<u8>,
}

/// Why a transport send/recv failed.
#[derive(Debug)]
pub(crate) enum TransportError {
    /// The transport is closed, or the peer went away.
    Closed,
    /// A lower-level I/O or codec failure, carrying its message.
    Io(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Closed => f.write_str("voice transport closed"),
            TransportError::Io(err) => write!(f, "voice transport error: {err}"),
        }
    }
}

impl Error for TransportError {}

/// The media transport seam: send/recv Opus frames to the call's peers.
///
/// The Opus bridge ([`super::audio`]) is written against this trait so it can be
/// exercised over an in-memory [`LoopbackTransport`] with no live GNS connection.
///
/// `recv` must be **cancel-safe** — the bridge races it against the microphone in
/// a `select!`, so a dropped (unfinished) `recv` must not lose a frame.
#[async_trait]
pub(crate) trait VoiceTransport: Send + Sync {
    /// Send one Opus frame to the peer(s).
    async fn send(&self, opus_frame: &[u8]) -> Result<(), TransportError>;
    /// Receive the next Opus frame from a peer.
    async fn recv(&self) -> Result<VoiceFrame, TransportError>;
}

/// The real GNS P2P media transport.
///
/// **Not yet functional.** Bringing up the P2P connection needs the `gns-sys`
/// FFI (`ConnectP2PCustomSignaling` + `SetCertificate`; the safe `gns` wrapper is
/// IP-socket-only) plus the CM service method that delivers our rendezvous to the
/// peer — discoverable only by sending the first one live. `connect` runs the
/// real phase-1 cert, then hits the phase-2 wall and returns the gating error;
/// `send`/`recv` are unreachable until then. See `VOICE_PROTOCOL.md`.
pub(crate) struct GnsTransport {
    // Filled in once `connect` can succeed: the GNS instance/connection handle
    // and the identity below. Kept minimal (unit-like) until then rather than
    // holding fields that are never populated.
}

impl GnsTransport {
    /// Bring up the GNS P2P media path for a joined voice session, presenting
    /// `identity` (the phase-1 Steam-signed cert) in the crypto handshake.
    ///
    /// Blocked at phase 2 today — see the type docs.
    pub(crate) async fn connect(
        conn: &Connection,
        session: &VoiceSession,
        identity: NetworkingIdentity,
    ) -> Result<Self, GnsError> {
        debug!(
            cert_len = identity.cert.len(),
            ca_key_id = identity.ca_key_id,
            "Steam: GNS transport bring-up (phase-1 cert ready)"
        );
        // Phase 2: as the newcomer we must initiate the P2P connection (send the
        // first rendezvous). That needs the GNS instance to generate it and the
        // CM carrier to deliver it; both unbuilt. Returns the precise gating error.
        signaling::open_rendezvous(conn, session).await?;
        // Phases 3–5 (ICE / SNP crypto / framing) would follow once phase 2 is
        // unblocked, ending with `Ok(GnsTransport { .. })`.
        Err(GnsError::TransportUnimplemented)
    }
}

#[async_trait]
impl VoiceTransport for GnsTransport {
    async fn send(&self, _opus_frame: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::Closed)
    }
    async fn recv(&self) -> Result<VoiceFrame, TransportError> {
        Err(TransportError::Closed)
    }
}

/// In-memory echo transport: every frame you `send` comes back from `recv`,
/// tagged as spoken by `peer`. Two uses:
///
/// - the **loopback voice demo** (`STEAM_VOICE_LOOPBACK=1`) — joining a Steam
///   voice channel echoes your own microphone back through the full real
///   pipeline (mic → Opus → transport → Opus → playback), so the voice subsystem
///   is exercisable end-to-end before the GNS transport exists;
/// - driving the Opus bridge in tests without a network (see `super::audio`).
pub(crate) struct LoopbackTransport {
    peer: u64,
    tx: futures::channel::mpsc::UnboundedSender<VoiceFrame>,
    rx: futures::lock::Mutex<futures::channel::mpsc::UnboundedReceiver<VoiceFrame>>,
}

impl LoopbackTransport {
    pub(crate) fn new(peer: u64) -> Self {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        Self {
            peer,
            tx,
            rx: futures::lock::Mutex::new(rx),
        }
    }
}

#[async_trait]
impl VoiceTransport for LoopbackTransport {
    async fn send(&self, opus_frame: &[u8]) -> Result<(), TransportError> {
        self.tx
            .unbounded_send(VoiceFrame {
                peer: self.peer,
                opus: opus_frame.to_vec(),
            })
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Result<VoiceFrame, TransportError> {
        use futures::StreamExt as _;
        let mut rx = self.rx.lock().await;
        rx.next().await.ok_or(TransportError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loopback_roundtrips_frames_in_order() {
        let transport = LoopbackTransport::new(42);
        transport.send(&[1, 2, 3]).await.unwrap();
        transport.send(&[4, 5]).await.unwrap();

        let first = transport.recv().await.unwrap();
        let second = transport.recv().await.unwrap();

        assert_eq!(first.peer, 42);
        assert_eq!(first.opus, vec![1, 2, 3]);
        assert_eq!(second.opus, vec![4, 5]);
    }
}
