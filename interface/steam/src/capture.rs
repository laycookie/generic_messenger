//! Manual diagnostic: capture Steam voice **P2P signaling** off a live session.
//!
//! This is the data-gathering step that unblocks the GNS media path (see
//! [`crate::gns`]). The one missing piece is the Connection-Manager message that
//! Steam's backend uses to relay a `CMsgSteamNetworkingP2PRendezvous` between
//! peers — it's proprietary and absent from `steam_vent_proto`, so it has to be
//! observed on a real call.
//!
//! We don't need Wireshark or to break Steam's encryption: `steam-vent` is a CM
//! client, so it has already negotiated the session key and **decrypts every
//! message for us**. This test logs in, joins a group voice chat (making us a
//! real P2P participant), and then dumps every message `steam-vent` couldn't
//! route — payloads and all — so the rendezvous carrier can't hide.
//!
//! # Topology (why group voice, not 1:1)
//!
//! Account **A** is this capture client (steam-vent). Account **B** is a normal
//! Steam client on a second account/device. Both must be in a **shared chat
//! group that has a voice channel**. A joins the voice via `JoinVoiceChat` (which
//! steam-vent *can* signal); B then joins the same channel from the official
//! client and tries to mesh-connect to A, firing the rendezvous at A. A 1:1 call
//! won't work — it needs an "accept" steam-vent can't drive, and the rendezvous
//! only flows after that.
//!
//! # Running it
//!
//! ```text
//! # 1) List your voice-capable rooms (no target set):
//! RUST_LOG=steam=debug,steam_vent=debug \
//! STEAM_USERNAME=accountA STEAM_SECRET='<password-or-saved-token>' \
//!   nix develop --command cargo test -p steam --release capture_voice_signaling -- --ignored --nocapture
//!
//! # 2) Re-run with a target room, then have account B join that voice & talk:
//! RUST_LOG=steam=debug,steam_vent=debug \
//! STEAM_USERNAME=accountA STEAM_SECRET='<...>' \
//! STEAM_VOICE_GROUP=<chat_group_id> STEAM_VOICE_CHAT=<chat_id> \
//!   nix develop --command cargo test -p steam --release capture_voice_signaling -- --ignored --nocapture
//! ```
//!
//! `STEAM_GUARD=<code>` may be needed on the first password login. Paste the
//! resulting log back: we want the unknown `emsg=`/`UNHANDLED` lines (classic
//! carrier, with payload) and any unfamiliar notification `job_name=` from
//! steam-vent's debug logs (service-method carrier — payload via a follow-up).

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use steam_vent::ConnectionTrait;
use steam_vent_proto::steammessages_chat_steamclient::CChatRoom_JoinVoiceChat_Request;
use tracing::{error, info, warn};

use crate::SteamMessenger;

/// How long to sit in the call draining messages while B connects.
const CAPTURE_WINDOW: Duration = Duration::from_secs(120);

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual: needs live STEAM_USERNAME/STEAM_SECRET + a 2nd account in a group voice call"]
async fn capture_voice_signaling() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let Some(username) = env("STEAM_USERNAME") else {
        panic!("set STEAM_USERNAME");
    };
    let Some(secret) = env("STEAM_SECRET") else {
        panic!("set STEAM_SECRET (password or saved refresh token)");
    };
    let guard = env("STEAM_GUARD");

    let messenger = SteamMessenger::for_capture(username, secret, guard);
    let connected = messenger.connected().await.expect("Steam login failed");
    info!(steam_id = connected.client_steamid, "capture: logged in");

    // List voice-capable rooms so the operator can pick a target to join.
    match messenger.load_chat_groups(&connected).await {
        Ok(groups) => {
            info!("capture: scanning {} chat group(s) for voice rooms", groups.len());
            for group in &groups {
                match messenger.load_chat_group_details(&connected, group.id).await {
                    Ok((detail, _members)) => {
                        for room in detail.rooms.iter().filter(|room| room.voice_allowed) {
                            info!(
                                group = %group.name,
                                room = %room.name,
                                "capture: VOICE ROOM -> STEAM_VOICE_GROUP={} STEAM_VOICE_CHAT={}",
                                room.chat_group_id,
                                room.chat_id,
                            );
                        }
                    }
                    Err(err) => warn!(group = %group.name, "capture: group details failed: {err}"),
                }
            }
        }
        Err(err) => warn!("capture: could not list chat groups: {err}"),
    }

    let target = env("STEAM_VOICE_GROUP")
        .and_then(|value| value.parse::<u64>().ok())
        .zip(env("STEAM_VOICE_CHAT").and_then(|value| value.parse::<u64>().ok()));
    let Some((chat_group_id, chat_id)) = target else {
        info!("capture: no STEAM_VOICE_GROUP/STEAM_VOICE_CHAT set — listed rooms above; exiting");
        return;
    };

    let conn = connected.conn.clone();

    // Drain anything already buffered so the capture window starts clean.
    let _ = conn.take_unprocessed();

    match conn
        .service_method(CChatRoom_JoinVoiceChat_Request {
            chat_group_id: Some(chat_group_id),
            chat_id: Some(chat_id),
            ..Default::default()
        })
        .await
    {
        Ok(response) => info!(voice_chatid = ?response.voice_chatid, "capture: joined group voice"),
        Err(err) => {
            error!("capture: JoinVoiceChat failed: {err}");
            return;
        }
    }

    info!(
        "capture: now have account B join this voice channel and talk. \
         Watching for {}s — looking for the rendezvous carrier...",
        CAPTURE_WINDOW.as_secs()
    );

    let deadline = Instant::now() + CAPTURE_WINDOW;
    let mut seen = 0usize;
    while Instant::now() < deadline {
        for raw in conn.take_unprocessed() {
            seen += 1;
            info!(
                emsg = raw.kind.value(),
                proto = raw.is_protobuf,
                len = raw.data.len(),
                bytes = %hex(&raw.data),
                "capture: UNHANDLED message",
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    info!("capture: window closed; {seen} unhandled message(s) logged");
}
