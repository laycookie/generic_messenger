//! The live, logged-in Steam session and the update streams it drives.
//!
//! [`Connected`] owns the `steam_vent` [`Connection`] plus the friend/persona
//! caches that its [`SteamStreams`] keep populated. Queries and the live
//! [`Text`](messenger_interface::interface::Text) stream both poll the same
//! session; [`Connected::alive`] latches death so the next
//! [`SteamMessenger::connected`](crate::SteamMessenger::connected) re-establishes.

use std::{
    collections::VecDeque,
    error::Error,
    fmt::Write as _,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_compat::CompatExt;
use bitflags::bitflags;
use chrono::DateTime;
use dashmap::DashMap;
use futures::{
    StreamExt,
    future::{Either, select},
    io::{Cursor, sink},
    lock::Mutex as AsyncMutex,
    stream::{BoxStream, SelectAll},
};
use futures_timer::Delay;
use tracing::{debug, info, warn};

use messenger_interface::{
    interface::TextEvent,
    types::{ID, Identifier, Message, Revision},
};

use steam_vent::auth::{
    DeviceConfirmationHandler, FileGuardDataStore, UserProvidedAuthConfirmationHandler,
};
use steam_vent::{
    Connection, ConnectionError, ConnectionTrait, NetMessage, NetworkError, ServerList,
};
use steam_vent_proto::{
    MsgKind,
    steammessages_chat_steamclient::CChatRoom_IncomingChatMessage_Notification,
    steammessages_clientserver_friends::{
        CMsgClientFriendsList, CMsgClientPersonaState, CMsgClientRequestFriendData,
    },
    steammessages_clientserver_login::CMsgClientLoggedOff,
    steammessages_friendmessages_steamclient::CFriendMessages_IncomingMessage_Notification,
};

use crate::api_types::{
    CHAT_ENTRY_TYPE_CHAT_MSG, ChatGroupEntry, ChatRoomLocation, FriendEntry, hex_avatar,
    message_id, steam_group_room_id,
};
use crate::downloaders::steam_user_identifier;

// `EClientPersonaStateFlag` bits requested when asking Steam for friend
// persona data. The proto has no avatar-specific request bit, so ask for the
// complete known persona-state mask and cache whatever profile fields Steam
// includes in the resulting `CMsgClientPersonaState` packets. `0x20` is not a
// defined flag in steam-vent-proto 0.5.2, so it is intentionally left undeclared
// below; `PersonaStateFlags::all()` is then exactly the intended `0x7FFF & !0x20`
// mask (the union of the declared flags).
bitflags! {
    struct PersonaStateFlags: u32 {
        const STATUS = 1;
        const PLAYER_NAME = 2;
        const QUERY_PORT = 4;
        const SOURCE_ID = 8;
        const PRESENCE = 16;
        const LAST_SEEN = 64;
        const USER_CLAN_RANK = 128;
        const GAME_EXTRA_INFO = 256;
        const GAME_DATA_BLOB = 512;
        const CLAN_DATA = 1024;
        const FACEBOOK = 2048;
        const RICH_PRESENCE = 4096;
        const BROADCAST = 8192;
        const WATCHING = 16384;
    }
}
const PERSONA_STATE_FLAGS: u32 = PersonaStateFlags::all().bits();
// Machine-check the mask the comment promises: adding or removing a flag above
// must not silently change what we request, and `0x20` must stay excluded.
const _: () = assert!(PERSONA_STATE_FLAGS == 0x7FFF & !0x20);

/// Cheap heuristic: a Steam refresh token is a JWT — three dot-separated
/// base64url segments, never containing `:` — whereas a password might contain
/// neither. Distinguishes a reusable saved session from a password.
fn looks_like_jwt(secret: &str) -> bool {
    !secret.contains(':') && secret.split('.').count() == 3
}

/// Voice **signaling capture** toggle (`STEAM_VOICE_CAPTURE=1`). Off by default
/// so it has zero effect on normal runs. When on, [`drain_voice_capture`] dumps
/// every CM message `steam-vent` couldn't route — the hunt for the proprietary
/// GNS P2P rendezvous carrier (see [`crate::gns`] and the capture notes). Read
/// once and cached.
fn voice_capture_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("STEAM_VOICE_CAPTURE")
            .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
    })
}

/// Log (at `info`, target `steam::session`) every unrouted CM message with its
/// EMsg + protobuf flag + hex body. The rendezvous carrier lands here when it's
/// a classic EMsg; service-method-notification carriers are dropped by
/// `steam-vent` before this point and instead show by name under
/// `RUST_LOG=steam_vent=debug`. Draining is otherwise a no-op (nothing reads the
/// unprocessed ring after login), so this is safe to call every poll.
fn drain_voice_capture(conn: &Connection) {
    for raw in conn.take_unprocessed() {
        let mut hex = String::with_capacity(raw.data.len() * 2);
        for byte in raw.data.iter() {
            let _ = write!(hex, "{byte:02x}");
        }
        info!(
            emsg = raw.kind.value(),
            proto = raw.is_protobuf,
            len = raw.data.len(),
            hex = %hex,
            "voice-capture: unrouted CM message",
        );
    }
}

/// Fold a friends-list payload into `friends` (relationship per friend) and ask
/// Steam for persona data (display names) for everyone on it. Shared by the
/// logon-buffer drain and the live-update listener in [`Connected::establish`].
async fn process_friends_list(
    conn: &Connection,
    friends: &DashMap<ID, FriendEntry>,
    list: &CMsgClientFriendsList,
) {
    let mut ids = Vec::new();
    for friend in &list.friends {
        if let Some(id) = friend.ulfriendid {
            friends.entry(id).or_default().relationship = friend.efriendrelationship.unwrap_or(0);
            ids.push(id);
        }
    }
    if !ids.is_empty()
        && let Err(err) = conn
            .send(CMsgClientRequestFriendData {
                friends: ids,
                persona_state_requested: Some(PERSONA_STATE_FLAGS),
                ..Default::default()
            })
            .await
    {
        warn!("Steam: failed to request friend persona data: {err}");
    }
}

/// Fold a persona-state payload into `friends`: display names and avatar
/// hashes. Shared by the logon-buffer drain and the live-update listener.
/// Returns how many friends had a name resolved for the first time.
fn process_persona_state(
    friends: &DashMap<ID, FriendEntry>,
    state: &CMsgClientPersonaState,
) -> usize {
    let mut newly_named = 0;
    for friend in &state.friends {
        if let Some(id) = friend.friendid {
            let mut entry = friends.entry(id).or_default();
            if let Some(name) = friend
                .player_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                if entry.name.is_none() {
                    newly_named += 1;
                }
                entry.name = Some(name.to_owned());
            }
            if let Some(hash) = friend.avatar_hash.as_deref().and_then(hex_avatar) {
                debug!(steam_id = id, "Steam: resolved friend avatar hash");
                entry.avatar_hash = Some(hash);
            }
        }
    }
    newly_named
}

async fn incoming_notification_text_event(
    incoming: CFriendMessages_IncomingMessage_Notification,
    friends: &DashMap<ID, FriendEntry>,
    client_steamid: ID,
    username: &str,
    fallback_ordinal: &mut u32,
) -> Option<TextEvent> {
    if incoming
        .chat_entry_type
        .is_some_and(|kind| kind != CHAT_ENTRY_TYPE_CHAT_MSG)
    {
        return None;
    }
    let friend = incoming.steamid_friend?;

    // Parse the BBCode form so emoticons/stickers survive (the no-bbcode
    // variant strips them); fall back to the no-bbcode text only if absent.
    let raw = incoming
        .message
        .as_deref()
        .or(incoming.message_no_bbcode.as_deref())
        .map(|message| message.trim_end_matches('\0'))
        .unwrap_or_default();
    let body = crate::rich::build_content(raw).await;
    if body.is_empty() {
        return None;
    }

    let timestamp = incoming.rtime32_server_timestamp.unwrap_or(0);
    let ordinal = match incoming.ordinal {
        Some(ordinal) => ordinal,
        None => {
            *fallback_ordinal = fallback_ordinal.wrapping_add(1);
            *fallback_ordinal
        }
    };
    let local_echo = incoming.local_echo.unwrap_or(false);
    let author_id = if local_echo { client_steamid } else { friend };

    let author = steam_user_identifier(author_id, friends, client_steamid, username).await;
    let message = Identifier::new(
        message_id(timestamp, ordinal),
        Message {
            content: Revision {
                at: DateTime::from_timestamp(timestamp as i64, 0),
                text: body,
            },
            author: Some(author),
            ..Default::default()
        },
    );
    let room = Identifier::new(friend, ());

    Some(TextEvent::MessageCreated { room, message })
}

async fn incoming_group_text_event(
    incoming: CChatRoom_IncomingChatMessage_Notification,
    friends: &DashMap<ID, FriendEntry>,
    room_locations: &DashMap<ID, ChatRoomLocation>,
    client_steamid: ID,
    username: &str,
) -> Option<TextEvent> {
    let chat_group_id = incoming.chat_group_id?;
    let chat_id = incoming.chat_id?;
    let sender = incoming.steamid_sender?;

    // Parse the BBCode form so emoticons/stickers survive (the no-bbcode
    // variant strips them); fall back to the no-bbcode text only if absent.
    let raw = incoming
        .message
        .as_deref()
        .or(incoming.message_no_bbcode.as_deref())
        .map(|message| message.trim_end_matches('\0'))
        .unwrap_or_default();
    let body = crate::rich::build_content(raw).await;
    if body.is_empty() {
        return None;
    }

    let room_id = steam_group_room_id(chat_group_id, chat_id);
    room_locations.insert(
        room_id,
        ChatRoomLocation::Group {
            chat_group_id,
            chat_id,
        },
    );

    let timestamp = incoming.timestamp.unwrap_or(0);
    let ordinal = incoming.ordinal.unwrap_or(0);
    let author = steam_user_identifier(sender, friends, client_steamid, username).await;
    let message = Identifier::new(
        message_id(timestamp, ordinal),
        Message {
            content: Revision {
                at: DateTime::from_timestamp(timestamp as i64, 0),
                text: body,
            },
            author: Some(author),
            ..Default::default()
        },
    );
    let room = Identifier::new(room_id, ());

    Some(TextEvent::MessageCreated { room, message })
}

enum SteamUpdate {
    Friends(std::result::Result<CMsgClientFriendsList, NetworkError>),
    Persona(std::result::Result<CMsgClientPersonaState, NetworkError>),
    Notification(std::result::Result<CFriendMessages_IncomingMessage_Notification, NetworkError>),
    GroupNotification(
        std::result::Result<CChatRoom_IncomingChatMessage_Notification, NetworkError>,
    ),
    /// Steam told us the session ended (logged in from another location, or
    /// the session expired). Distinct from a transient broadcast-stream lag.
    LoggedOff(std::result::Result<CMsgClientLoggedOff, NetworkError>),
}

enum UpdatePoll {
    Text(TextEvent),
    Cache,
    Timeout,
    /// The session is no longer usable (Steam logged us off, or every event
    /// stream closed). The caller should re-establish.
    Disconnected,
}

pub(crate) enum TextPoll {
    Text(TextEvent),
    Idle,
    /// The session died; the messenger-level stream re-establishes and resumes.
    Disconnected,
}

struct SteamStreams {
    updates: SelectAll<BoxStream<'static, SteamUpdate>>,
    resolved_persona_names: usize,
    notification_fallback_ordinal: u32,
}

impl SteamStreams {
    fn new(conn: &Connection) -> Self {
        let mut updates = SelectAll::new();
        updates.push(
            conn.on::<CMsgClientFriendsList>()
                .map(SteamUpdate::Friends)
                .boxed(),
        );
        updates.push(
            conn.on::<CMsgClientPersonaState>()
                .map(SteamUpdate::Persona)
                .boxed(),
        );
        // Friend DMs arrive *only* via this service notification — the sole
        // path the modern Steam protocol uses (see steam-vent's `chat.rs`).
        // Do NOT also subscribe to the legacy `CMsgClientFriendMsgIncoming`:
        // it carries no `ordinal`, forcing a synthetic local counter whose
        // message IDs can never match the real server ordinals returned by
        // `GetRecentMessages` — so the same message gets two different IDs
        // live vs. on history reload, defeating the UI's by-ID dedup.
        updates.push(
            conn.on_notification::<CFriendMessages_IncomingMessage_Notification>()
                .map(SteamUpdate::Notification)
                .boxed(),
        );
        updates.push(
            conn.on_notification::<CChatRoom_IncomingChatMessage_Notification>()
                .map(SteamUpdate::GroupNotification)
                .boxed(),
        );
        // Steam pushes this when it ends our session (logged in elsewhere,
        // session expired, ...). It is our one reliable disconnect signal:
        // steam-vent surfaces no socket-close event, and the `on::<_>()`
        // broadcast streams go silent rather than ending when the socket dies.
        updates.push(
            conn.on::<CMsgClientLoggedOff>()
                .map(SteamUpdate::LoggedOff)
                .boxed(),
        );

        Self {
            updates,
            resolved_persona_names: 0,
            notification_fallback_ordinal: 0,
        }
    }

    async fn next_update(
        &mut self,
        conn: &Connection,
        friends: &DashMap<ID, FriendEntry>,
        room_locations: &DashMap<ID, ChatRoomLocation>,
        client_steamid: ID,
        username: &str,
    ) -> UpdatePoll {
        match self.updates.next().await {
            Some(SteamUpdate::Friends(Ok(list))) => {
                info!(
                    "Steam: friends list update ({} entries)",
                    list.friends.len()
                );
                process_friends_list(conn, friends, &list).await;
                UpdatePoll::Cache
            }
            Some(SteamUpdate::Friends(Err(err))) => {
                // Broadcast-channel lag (steam-vent renders it "Unexpected end
                // of stream"): a friends-list push was dropped under load — not
                // a disconnect. A later push or the profile fallback recovers.
                warn!("Steam: skipped a friends-list update under load ({err})");
                UpdatePoll::Cache
            }
            Some(SteamUpdate::Persona(Ok(state))) => {
                let newly_named = process_persona_state(friends, &state);
                self.resolved_persona_names += newly_named;
                // Steam sends a persona-state push per friend *and* for every
                // presence/avatar refresh, so most resolve no new name. Only
                // log when the total advances — otherwise the same line repeats
                // verbatim on every push.
                if newly_named > 0 {
                    info!(
                        "Steam: resolved {newly_named} new friend name(s) ({} total)",
                        self.resolved_persona_names
                    );
                }
                UpdatePoll::Cache
            }
            Some(SteamUpdate::Persona(Err(err))) => {
                // Same benign broadcast lag as above; persona updates were
                // skipped. `hydrate_unresolved_friends` backfills any missing
                // names/avatars via Player.GetPlayerLinkDetails.
                debug!("Steam: skipped some persona updates under load ({err})");
                UpdatePoll::Cache
            }
            Some(SteamUpdate::Notification(Ok(incoming))) => {
                match incoming_notification_text_event(
                    incoming,
                    friends,
                    client_steamid,
                    username,
                    &mut self.notification_fallback_ordinal,
                )
                .await
                {
                    Some(event) => UpdatePoll::Text(event),
                    None => UpdatePoll::Cache,
                }
            }
            Some(SteamUpdate::Notification(Err(err))) => {
                warn!("Steam: incoming message notification decode failed: {err}");
                UpdatePoll::Cache
            }
            Some(SteamUpdate::GroupNotification(Ok(incoming))) => {
                match incoming_group_text_event(
                    incoming,
                    friends,
                    room_locations,
                    client_steamid,
                    username,
                )
                .await
                {
                    Some(event) => UpdatePoll::Text(event),
                    None => UpdatePoll::Cache,
                }
            }
            Some(SteamUpdate::GroupNotification(Err(err))) => {
                warn!("Steam: incoming group chat notification decode failed: {err}");
                UpdatePoll::Cache
            }
            Some(SteamUpdate::LoggedOff(Ok(logoff))) => {
                warn!(
                    "Steam: server logged us off (eresult={}); will re-establish",
                    logoff.eresult.unwrap_or(0)
                );
                UpdatePoll::Disconnected
            }
            Some(SteamUpdate::LoggedOff(Err(err))) => {
                warn!("Steam: logged-off stream lagged: {err}");
                UpdatePoll::Cache
            }
            None => UpdatePoll::Disconnected,
        }
    }
}

/// A live, logged-in Steam session plus the caches its Steam update stream
/// keeps populated when polled.
pub(crate) struct Connected {
    pub(crate) conn: Connection,
    pub(crate) client_steamid: ID,
    pub(crate) username: String,
    /// Friend SteamID -> cached info. Filled from Steam friend/persona updates.
    pub(crate) friends: Arc<DashMap<ID, FriendEntry>>,
    pub(crate) chat_groups: Arc<DashMap<ID, ChatGroupEntry>>,
    pub(crate) chat_room_locations: Arc<DashMap<ID, ChatRoomLocation>>,
    /// Guards the one-shot profile-details fallback used when persona
    /// broadcasts do not include every friend before startup queries run.
    pub(crate) profile_details_loaded: Arc<AsyncMutex<bool>>,
    streams: AsyncMutex<SteamStreams>,
    pending_text_events: AsyncMutex<VecDeque<TextEvent>>,
    /// Cleared once the session is observed dead (Steam logged us off, or all
    /// event streams closed). [`SteamMessenger::connected`](crate::SteamMessenger::connected)
    /// rebuilds while this is `false`, so queries and the live stream
    /// transparently reconnect.
    pub(crate) alive: AtomicBool,
}

impl Connected {
    /// Discover servers, establish a session, and register the friends/persona
    /// update streams.
    ///
    /// If `secret` looks like a saved refresh token it is reused directly;
    /// otherwise it is treated as a password and a full Steam Guard login runs.
    pub(crate) async fn establish(
        username: String,
        secret: String,
        guard_code: Option<String>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        info!(account = %username, "Steam: discovering servers");
        let server_list = ServerList::discover().await?;

        let conn = if looks_like_jwt(&secret) {
            info!("Steam: reusing saved session");
            Connection::access(&server_list, &username, &secret)
                .await
                .map_err(|err| {
                    warn!("Steam: saved session rejected (expired or revoked): {err}");
                    format!("Steam session expired — please log in again ({err})")
                })?
        } else {
            Self::password_login(&server_list, &username, &secret, guard_code).await?
        };

        let client_steamid: ID = u64::from(conn.steam_id());
        info!(steam_id = client_steamid, "Steam: logged in");
        let friends: Arc<DashMap<ID, FriendEntry>> = Arc::new(DashMap::new());
        let chat_groups: Arc<DashMap<ID, ChatGroupEntry>> = Arc::new(DashMap::new());
        let chat_room_locations: Arc<DashMap<ID, ChatRoomLocation>> = Arc::new(DashMap::new());

        // Register the broadcast subscriptions *before* returning so the
        // friends-list push Steam sends right after login isn't dropped for
        // lack of a subscriber. The streams are polled explicitly by queries
        // and by `ArcStream::next`; no detached tasks are created here.
        let streams = SteamStreams::new(&conn);
        let pending_text_events = VecDeque::new();

        // Steam pushes the initial friends list *and* per-friend persona state
        // (names + avatars) during logon — before the subscriptions above
        // existed — so they land in the connection's "unprocessed" ring buffer
        // instead of our streams. Drain both here so first-load contacts have
        // names and icons; explicit stream polling handles later updates and
        // the response to the persona request `process_friends_list` sends.
        let friends_kind = MsgKind::from(CMsgClientFriendsList::KIND);
        let persona_kind = MsgKind::from(CMsgClientPersonaState::KIND);
        for raw in conn.take_unprocessed() {
            let kind = raw.kind;
            if kind == friends_kind {
                if let Ok(list) = raw.into_message::<CMsgClientFriendsList>() {
                    info!("Steam: loaded {} friends from logon", list.friends.len());
                    process_friends_list(&conn, &friends, &list).await;
                }
            } else if kind == persona_kind
                && let Ok(state) = raw.into_message::<CMsgClientPersonaState>()
            {
                process_persona_state(&friends, &state);
            }
        }

        Ok(Self {
            conn,
            client_steamid,
            username,
            friends,
            chat_groups,
            chat_room_locations,
            profile_details_loaded: Arc::new(AsyncMutex::new(false)),
            streams: AsyncMutex::new(streams),
            pending_text_events: AsyncMutex::new(pending_text_events),
            alive: AtomicBool::new(true),
        })
    }

    /// Full credential login. Submits a typed Steam Guard code via an in-memory
    /// reader, otherwise waits for mobile-app approval. The two handlers are
    /// different types, so each branch calls `login` itself.
    async fn password_login(
        server_list: &ServerList,
        username: &str,
        password: &str,
        guard_code: Option<String>,
    ) -> Result<Connection, ConnectionError> {
        match guard_code
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            Some(code) => {
                info!("Steam: submitting Steam Guard code");
                let reader = Cursor::new(format!("{code}\n").into_bytes());
                Connection::login(
                    server_list,
                    username,
                    password,
                    FileGuardDataStore::user_cache(),
                    UserProvidedAuthConfirmationHandler::new(reader.compat(), sink().compat()),
                )
                .await
            }
            None => {
                info!(
                    "Steam: logging in — if prompted, approve the login in your Steam mobile app"
                );
                Connection::login(
                    server_list,
                    username,
                    password,
                    FileGuardDataStore::user_cache(),
                    DeviceConfirmationHandler,
                )
                .await
            }
        }
    }

    async fn poll_update_once(&self, timeout: Duration) -> UpdatePoll {
        // Voice signaling capture (STEAM_VOICE_CAPTURE=1): drain + log every
        // unrouted CM message each ~250ms tick, before the unprocessed ring
        // (cap 32) can evict the rendezvous carrier we're hunting for.
        if voice_capture_enabled() {
            drain_voice_capture(&self.conn);
        }
        let conn = self.conn.clone();
        let friends = self.friends.clone();
        let room_locations = self.chat_room_locations.clone();
        let client_steamid = self.client_steamid;
        let username = self.username.clone();
        let mut streams = self.streams.lock().await;
        let update = streams
            .next_update(&conn, &friends, &room_locations, client_steamid, &username)
            .compat();
        futures::pin_mut!(update);
        let delay = Delay::new(timeout);
        futures::pin_mut!(delay);

        let outcome = match select(update, delay).await {
            Either::Left((update, _)) => update,
            Either::Right((_, _)) => UpdatePoll::Timeout,
        };
        // Latch death here so the next `connected()` rebuilds, from any caller
        // (queries included) — not only the stream loop that observed it.
        if matches!(outcome, UpdatePoll::Disconnected) {
            self.alive.store(false, Ordering::Release);
        }
        outcome
    }

    pub(crate) async fn drive_update_for_cache(&self, timeout: Duration) -> bool {
        match self.poll_update_once(timeout).await {
            UpdatePoll::Text(event) => {
                self.pending_text_events.lock().await.push_back(event);
                true
            }
            UpdatePoll::Cache => true,
            UpdatePoll::Timeout | UpdatePoll::Disconnected => false,
        }
    }

    pub(crate) async fn next_text_event(&self, timeout: Duration) -> TextPoll {
        if let Some(event) = self.pending_text_events.lock().await.pop_front() {
            return TextPoll::Text(event);
        }

        match self.poll_update_once(timeout).await {
            UpdatePoll::Text(event) => TextPoll::Text(event),
            UpdatePoll::Cache | UpdatePoll::Timeout => TextPoll::Idle,
            UpdatePoll::Disconnected => TextPoll::Disconnected,
        }
    }
}
