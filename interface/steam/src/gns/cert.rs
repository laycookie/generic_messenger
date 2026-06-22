//! Phase 1 — the Steam-signed networking certificate.
//!
//! GNS peers authenticate each other in the crypto handshake with a certificate
//! that Steam signs over their networking identity key. We request one with
//! [`CertRequest`] (our public key in `key_data`) and receive [`CertReply`]
//! (`cert` + `ca_key_id` + `ca_signature`). The real Steam client does this
//! right after `JoinVoiceChat` to become reachable in Valve's networking layer.
//!
//! `CMsgClientNetworkingCertReply` carries no `RpcMessageWithKind` binding in
//! `steam_vent_proto`, so `conn.job::<CertRequest, CertReply>` won't compile
//! directly. The orphan rule forbids impl'ing the foreign trait on the foreign
//! reply type — but it permits impl'ing it on a **local newtype**
//! ([`NetworkingCertReply`]), and steam-vent's blanket
//! `impl<T: RpcMessageWithKind + Send> NetMessage for T` then makes the wrapper
//! a valid job response. So no proto fork is needed.

use ed25519_dalek::SigningKey;
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto::enums_clientserver::EMsg;
use steam_vent_proto::steammessages_clientserver::{
    CMsgClientNetworkingCertReply as CertReply, CMsgClientNetworkingCertRequest as CertRequest,
};
use steam_vent_proto::{RpcMessage, RpcMessageWithKind};
use tracing::{debug, info};

use super::GnsError;

/// Local wrapper that binds the otherwise-unbound `CMsgClientNetworkingCertReply`
/// to its EMsg so `conn.job` can decode it (see the module docs).
#[derive(Debug)]
struct NetworkingCertReply(CertReply);

impl RpcMessage for NetworkingCertReply {
    fn parse(reader: &mut dyn std::io::Read) -> steam_vent_proto::protobuf::Result<Self> {
        CertReply::parse(reader).map(NetworkingCertReply)
    }

    fn write(&self, writer: &mut dyn std::io::Write) -> steam_vent_proto::protobuf::Result<()> {
        self.0.write(writer)
    }

    fn encode_size(&self) -> usize {
        self.0.encode_size()
    }
}

impl RpcMessageWithKind for NetworkingCertReply {
    type KindEnum = EMsg;
    const KIND: Self::KindEnum = EMsg::k_EMsgClientNetworkingCertRequestResponse;
}

/// Our networking identity: the Ed25519 keypair plus the Steam-signed cert over
/// its public half. The keypair is retained because the GNS handshake will need
/// it to sign; the cert + `ca_key_id` identify us to peers.
pub(crate) struct NetworkingIdentity {
    pub(crate) signing_key: SigningKey,
    pub(crate) cert: Vec<u8>,
    pub(crate) ca_key_id: u64,
}

/// Generate an identity keypair and obtain a Steam-signed networking cert over
/// its public key.
pub(crate) async fn request_networking_cert(
    conn: &Connection,
) -> Result<NetworkingIdentity, GnsError> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|err| GnsError::CertRequest(err.to_string()))?;
    let signing_key = SigningKey::from_bytes(&seed);
    let public_key = signing_key.verifying_key().to_bytes();
    let steamid = u64::from(conn.steam_id());

    // `key_data` is a serialized unsigned `CMsgSteamDatagramCertificate` carrying
    // key_type + the raw public key + our identity (canonical "steamid:N" string
    // and the legacy fixed64); the server fills time/app_ids, signs it, and the
    // reply's `cert` is the signed form of this same message. (Per Valve's GNS
    // `GetCertificateRequest`; its `CMsgSteamDatagramCertificateRequest` wrapper
    // is the app-side blob — the CM message carries the bare cert.)
    // steam_vent_proto lacks these GNS types, so we encode the protobuf by hand.
    let mut cert = Vec::new();
    put_varint_field(&mut cert, 1, 1); // key_type = ED25519
    put_bytes_field(&mut cert, 2, &public_key); // key_data = raw 32-byte pubkey
    put_fixed64_field(&mut cert, 4, steamid); // legacy_steam_id
    put_bytes_field(&mut cert, 12, format!("steamid:{steamid}").as_bytes()); // identity_string
    debug!("Steam: networking cert request key_data = {cert:02x?}");

    let request = CertRequest {
        key_data: Some(cert),
        // app_id is REQUIRED at the CM layer: absent → InvalidParam, even though
        // Valve's open-GNS GetCertificateRequest sets none (the closed Steam
        // client supplies one). 480 (Spacewar) issues a cert successfully.
        // TODO(app_id): circle back. 480 likely scopes the cert to the wrong app
        // — try `app_id: 0` (unscoped cert) and/or a voice-specific id, and
        // confirm the issued cert is accepted for voice P2P. Parked for now.
        app_id: Some(480),
        ..Default::default()
    };

    let NetworkingCertReply(reply) = conn
        .job::<CertRequest, NetworkingCertReply>(request)
        .await
        .map_err(|err| GnsError::CertRequest(err.to_string()))?;

    let cert = reply.cert.unwrap_or_default();
    let ca_key_id = reply.ca_key_id.unwrap_or(0);
    info!(
        cert_len = cert.len(),
        ca_key_id, "Steam: obtained networking cert"
    );

    Ok(NetworkingIdentity {
        signing_key,
        cert,
        ca_key_id,
    })
}

// Minimal protobuf wire encoders for the hand-built GNS cert request (the GNS
// `CMsgSteamDatagramCertificate*` types aren't in `steam_vent_proto`). Field
// numbers are all < 16, so each tag is a single byte.
fn put_varint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            buf.push(byte);
            return;
        }
        buf.push(byte | 0x80);
    }
}

fn put_varint_field(buf: &mut Vec<u8>, field: u8, value: u64) {
    buf.push(field << 3); // wire type 0 (varint)
    put_varint(buf, value);
}

fn put_bytes_field(buf: &mut Vec<u8>, field: u8, data: &[u8]) {
    buf.push((field << 3) | 2); // wire type 2 (length-delimited)
    put_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn put_fixed64_field(buf: &mut Vec<u8>, field: u8, value: u64) {
    buf.push((field << 3) | 1); // wire type 1 (64-bit)
    buf.extend_from_slice(&value.to_le_bytes());
}
