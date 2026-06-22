//! `Query` (profile/friends) and `Text` (history/send) implementations.
//!
//! Each method resolves the live [`Connected`](crate::Connected) session, then
//! either reads a background-populated cache (friends) or runs a Steam RPC
//! through [`SteamMessenger::run`](crate::SteamMessenger::run).

use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::DateTime;
use dashmap::DashMap;
use futures::future::{Either, join_all, select};
use futures::lock::Mutex as AsyncMutex;
use futures_timer::Delay;

use messenger_interface::interface::{
    ArcStream, Ordering, Query, Text, TextEvent, WeakSocketStream,
};
use messenger_interface::types::{
    Emoji, House, ID, Identifier, Message, Place, Reaction, Revision, Room, RoomCapabilities, User,
};
use steam_vent::{Connection, ConnectionTrait};
use steam_vent_proto::steammessages_chat_steamclient::{
    CChatRoom_GetChatRoomGroupState_Request, CChatRoom_GetChatRoomGroupSummary_Response,
    CChatRoom_GetMessageHistory_Request, CChatRoom_GetMyChatRoomGroups_Request,
    CChatRoom_SendChatMessage_Request, CChatRoom_SetSessionActiveChatRoomGroups_Request,
    CChatRoomGroupState, CChatRoomState, EChatRoomJoinState,
    cchat_room_get_message_history_response,
};
use steam_vent_proto::steammessages_friendmessages_steamclient::{
    CFriendMessages_GetRecentMessages_Request, CFriendMessages_SendMessage_Request,
    CFriendsMessages_GetActiveMessageSessions_Request,
};
use steam_vent_proto::steammessages_player_steamclient::CPlayer_GetPlayerLinkDetails_Request;
use tracing::{debug, error, warn};

use crate::SteamMessenger;
use crate::api_types::{
    CHAT_ENTRY_TYPE_CHAT_MSG, ChatGroupEntry, ChatRoomEntry, ChatRoomLocation,
    EFRIENDRELATIONSHIP_FRIEND, FriendEntry, account_id_to_steam_id, hex_avatar, message_id,
    steam_group_room_id,
};
use crate::downloaders::{cache_avatar, steam_user_identifier};
use crate::session::{Connected, TextPoll};

/// Recent-history page size for [`Text::get_messages`].
const RECENT_MESSAGE_COUNT: u32 = 50;
/// `Player.GetPlayerLinkDetails` accepts a repeated ID list; keep chunks
/// moderate so startup profile hydration is predictable.
const PROFILE_DETAILS_CHUNK_SIZE: usize = 100;
const PROFILE_DETAILS_TIMEOUT: Duration = Duration::from_secs(3);
const ACTIVE_SESSIONS_TIMEOUT: Duration = Duration::from_secs(1);
const CHAT_GROUPS_TIMEOUT: Duration = Duration::from_secs(3);
const CHAT_GROUP_STATE_TIMEOUT: Duration = Duration::from_secs(3);
const CHAT_MESSAGE_HISTORY_TIMEOUT: Duration = Duration::from_secs(3);
const TEXT_EVENT_IDLE_CHECK: Duration = Duration::from_millis(250);
async fn timeout_after<F>(duration: Duration, future: F) -> Option<F::Output>
where
    F: Future,
{
    futures::pin_mut!(future);
    let delay = Delay::new(duration);
    futures::pin_mut!(delay);
    match select(future, delay).await {
        Either::Left((output, _)) => Some(output),
        Either::Right((_, _)) => None,
    }
}

/// The friends list and the per-friend display names arrive in separate Steam
/// pushes, so the cache fills in two stages. Startup only needs the friends
/// list before it can use the profile-details fallback below; waiting for every
/// persona broadcast made the UI sit on the loading screen until timeout.
async fn wait_for_friend_list(connected: &Connected) {
    let mut idle_polls = 0usize;
    for _ in 0..100 {
        // Scope the iterator so its DashMap shard locks are released before the
        // await below — otherwise Steam update processing could block on a
        // shard while we wait holding it.
        let has_friends = {
            connected
                .friends
                .iter()
                .any(|entry| entry.relationship == EFRIENDRELATIONSHIP_FRIEND)
        };
        if has_friends {
            break;
        }

        if connected
            .drive_update_for_cache(Duration::from_millis(100))
            .await
        {
            idle_polls = 0;
        } else {
            idle_polls += 1;
            if idle_polls >= 10 {
                break;
            }
        }
    }
}

fn resolved_name(entry: &FriendEntry) -> Option<String> {
    entry
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn unresolved_friend_ids(friends: &DashMap<ID, FriendEntry>) -> Vec<ID> {
    friends
        .iter()
        .filter(|entry| entry.relationship == EFRIENDRELATIONSHIP_FRIEND)
        .filter(|entry| resolved_name(entry).is_none())
        .map(|entry| *entry.key())
        .collect()
}

fn count_unresolved_friends(friends: &DashMap<ID, FriendEntry>) -> usize {
    friends
        .iter()
        .filter(|entry| entry.relationship == EFRIENDRELATIONSHIP_FRIEND)
        .filter(|entry| resolved_name(entry).is_none())
        .count()
}

async fn hydrate_unresolved_friends(
    conn: Connection,
    friends: Arc<DashMap<ID, FriendEntry>>,
    profile_details_loaded: Arc<AsyncMutex<bool>>,
) {
    let mut loaded = profile_details_loaded.lock().await;
    if *loaded {
        return;
    }

    let ids = unresolved_friend_ids(&friends);
    if ids.is_empty() {
        *loaded = true;
        return;
    }

    hydrate_profile_details(conn, friends.clone(), ids, "friend").await;

    let unresolved = count_unresolved_friends(&friends);
    if unresolved > 0 {
        warn!(
            "Steam: {unresolved} friend(s) still lack profile names after lookup; omitting them instead of showing SteamIDs"
        );
    }
    *loaded = true;
}

async fn hydrate_profile_details(
    conn: Connection,
    profiles: Arc<DashMap<ID, FriendEntry>>,
    ids: Vec<ID>,
    label: &str,
) {
    let ids = ids
        .into_iter()
        .filter(|id| {
            profiles
                .get(id)
                .is_none_or(|entry| resolved_name(&entry).is_none())
        })
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return;
    }

    debug!(
        "Steam: resolving {} {label} profile(s) via Player.GetPlayerLinkDetails",
        ids.len()
    );

    for chunk in ids.chunks(PROFILE_DETAILS_CHUNK_SIZE) {
        let request = CPlayer_GetPlayerLinkDetails_Request {
            steamids: chunk.to_vec(),
            ..Default::default()
        };

        let response =
            match timeout_after(PROFILE_DETAILS_TIMEOUT, conn.service_method(request)).await {
                Some(Ok(response)) => response,
                Some(Err(err)) => {
                    warn!("Steam: failed to fetch friend profile details: {err}");
                    continue;
                }
                None => {
                    warn!(
                        "Steam: timed out fetching friend profile details after {}s",
                        PROFILE_DETAILS_TIMEOUT.as_secs()
                    );
                    continue;
                }
            };

        for account in response.accounts {
            let Some(public_data) = account.public_data.as_ref() else {
                continue;
            };
            let Some(id) = public_data.steamid else {
                continue;
            };

            let mut entry = profiles.entry(id).or_default();
            if let Some(name) = public_data
                .persona_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                entry.name = Some(name.to_owned());
            }
            if let Some(hash) = public_data
                .sha_digest_avatar
                .as_deref()
                .and_then(hex_avatar)
            {
                entry.avatar_hash = Some(hash);
            }
        }
    }
}

/// Snapshot the friend cache into owned `(id, display name, avatar hash)`
/// tuples, so avatar downloads can run without holding DashMap shard locks
/// across `.await`. Unresolved friends are omitted instead of being surfaced as
/// bare SteamIDs; the UI stores this startup snapshot without a later contact
/// refresh.
fn snapshot_friends(friends: &DashMap<ID, FriendEntry>) -> Vec<(ID, String, Option<String>)> {
    let mut unresolved = 0usize;
    let snapshot = friends
        .iter()
        .filter(|entry| entry.relationship == EFRIENDRELATIONSHIP_FRIEND)
        .filter_map(|entry| {
            let id = *entry.key();
            let Some(name) = resolved_name(&entry) else {
                unresolved += 1;
                return None;
            };
            Some((id, name, entry.avatar_hash.clone()))
        })
        .collect();

    if unresolved > 0 {
        debug!("Steam: omitted {unresolved} unresolved friend(s) from snapshot");
    } else {
        debug!("Steam: all friends in snapshot have display names");
    }
    snapshot
}

async fn recent_message_times(
    conn: Connection,
    friend_snapshot: &[(ID, String, Option<String>)],
) -> HashMap<ID, u32> {
    let account_to_steamid: HashMap<u32, ID> = friend_snapshot
        .iter()
        .map(|(id, _, _)| (*id as u32, *id))
        .collect();

    let response = match timeout_after(
        ACTIVE_SESSIONS_TIMEOUT,
        conn.service_method(CFriendsMessages_GetActiveMessageSessions_Request {
            lastmessage_since: Some(0),
            only_sessions_with_messages: Some(true),
            ..Default::default()
        }),
    )
    .await
    {
        Some(Ok(response)) => response,
        Some(Err(err)) => {
            warn!("Steam: failed to fetch active message sessions: {err}");
            return HashMap::new();
        }
        None => {
            warn!(
                "Steam: timed out fetching active message sessions after {}s",
                ACTIVE_SESSIONS_TIMEOUT.as_secs()
            );
            return HashMap::new();
        }
    };

    response
        .message_sessions
        .into_iter()
        .filter_map(|session| {
            let account_id = session.accountid_friend?;
            let steam_id = account_to_steamid.get(&account_id)?;
            Some((*steam_id, session.last_message.unwrap_or(0)))
        })
        .collect()
}

fn display_name(name: Option<&String>, fallback: impl FnOnce() -> String) -> String {
    name.map(String::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(fallback)
}

fn chat_room_entry(
    chat_group_id: ID,
    room: &CChatRoomState,
    default_chat_id: Option<ID>,
) -> Option<ChatRoomEntry> {
    let chat_id = room.chat_id?;
    let name = display_name(room.chat_name.as_ref(), || {
        if Some(chat_id) == default_chat_id {
            "General".to_string()
        } else {
            format!("Chat {chat_id}")
        }
    });

    Some(ChatRoomEntry {
        chat_group_id,
        chat_id,
        name,
        voice_allowed: room.voice_allowed.unwrap_or(false),
        last_message_at: room.time_last_message.unwrap_or(0),
        sort_order: room.sort_order.unwrap_or(u32::MAX),
    })
}

fn chat_group_from_summary(
    summary: &CChatRoom_GetChatRoomGroupSummary_Response,
) -> Option<ChatGroupEntry> {
    let id = summary.chat_group_id?;
    let name = display_name(summary.chat_group_name.as_ref(), || {
        format!("Steam group {id}")
    });
    let avatar_hash = summary
        .chat_group_avatar_sha
        .as_deref()
        .and_then(hex_avatar);
    let default_chat_id = summary.default_chat_id;
    let mut rooms = summary
        .chat_rooms
        .iter()
        .filter_map(|room| chat_room_entry(id, room, default_chat_id))
        .collect::<Vec<_>>();
    sort_chat_rooms(&mut rooms);

    Some(ChatGroupEntry {
        id,
        name,
        avatar_hash,
        rooms,
    })
}

fn chat_group_from_state(chat_group_id: ID, state: &CChatRoomGroupState) -> ChatGroupEntry {
    let header = state.header_state.as_ref();
    let id = header
        .and_then(|header| header.chat_group_id)
        .unwrap_or(chat_group_id);
    let name = display_name(header.and_then(|header| header.chat_name.as_ref()), || {
        format!("Steam group {id}")
    });
    let avatar_hash = header
        .and_then(|header| header.avatar_sha.as_deref())
        .and_then(hex_avatar);
    let default_chat_id = state.default_chat_id;
    let mut rooms = state
        .chat_rooms
        .iter()
        .filter_map(|room| chat_room_entry(id, room, default_chat_id))
        .collect::<Vec<_>>();
    sort_chat_rooms(&mut rooms);

    ChatGroupEntry {
        id,
        name,
        avatar_hash,
        rooms,
    }
}

fn sort_chat_rooms(rooms: &mut [ChatRoomEntry]) {
    rooms.sort_by(|a, b| {
        a.sort_order
            .cmp(&b.sort_order)
            .then_with(|| b.last_message_at.cmp(&a.last_message_at))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

async fn steam_group_icon(group: &ChatGroupEntry) -> Option<PathBuf> {
    match &group.avatar_hash {
        Some(hash) => cache_avatar(group.id, hash).await,
        None => None,
    }
}

fn room_capabilities(room: &ChatRoomEntry) -> RoomCapabilities {
    let mut capabilities = RoomCapabilities::Text;
    if room.voice_allowed {
        capabilities |= RoomCapabilities::Voice;
    }
    capabilities
}

fn chat_room_identifier(
    room: &ChatRoomEntry,
    participants: Option<Vec<Identifier<User>>>,
) -> Identifier<Place<Room>> {
    Identifier::new(
        steam_group_room_id(room.chat_group_id, room.chat_id),
        Place::new(
            room.name.clone(),
            None,
            Room::new(room_capabilities(room), participants, None),
        ),
    )
}

async fn known_user(
    steamid: ID,
    friends: &DashMap<ID, FriendEntry>,
    client_steamid: ID,
    username: &str,
) -> Identifier<User> {
    steam_user_identifier(steamid, friends, client_steamid, username).await
}

fn chat_message_reactions(
    reactions: Vec<cchat_room_get_message_history_response::chat_message::MessageReaction>,
) -> Vec<Reaction> {
    reactions
        .into_iter()
        .filter_map(|reaction| {
            let emoji = reaction.reaction?;
            Some(Reaction {
                // Reaction images aren't resolved here (the shortcode is shown
                // either way); carry the name so the model stays uniform.
                emoji: Emoji::shortcode(emoji),
                count: reaction.num_reactors.unwrap_or(0),
                reacted: reaction.has_user_reacted.unwrap_or(false),
            })
        })
        .collect()
}

impl SteamMessenger {
    /// Sleep up to `duration`, waking early (returning `false`) if the app
    /// dropped its last messenger handle. Lets the live stream terminate
    /// promptly during a reconnect backoff instead of lingering for the full
    /// delay. A strong count of 1 means only the in-flight `next` future holds
    /// the messenger (see [`ArcStream::next`] for `SteamMessenger`).
    async fn sleep_unless_dropped(self: &Arc<Self>, duration: Duration) -> bool {
        let mut remaining = duration;
        while remaining > Duration::ZERO {
            if Arc::strong_count(self) == 1 {
                return false;
            }
            let step = remaining.min(TEXT_EVENT_IDLE_CHECK);
            Delay::new(step).await;
            remaining = remaining.saturating_sub(step);
        }
        Arc::strong_count(self) != 1
    }

    pub(crate) async fn load_chat_groups(
        &self,
        connected: &Connected,
    ) -> Result<Vec<ChatGroupEntry>, Box<dyn Error + Sync + Send>> {
        let conn = connected.conn.clone();
        let chat_groups = connected.chat_groups.clone();
        let room_locations = connected.chat_room_locations.clone();

        let groups = self
            .run(async move {
                let response = match timeout_after(
                    CHAT_GROUPS_TIMEOUT,
                    conn.service_method(CChatRoom_GetMyChatRoomGroups_Request::default()),
                )
                .await
                {
                    Some(Ok(response)) => response,
                    Some(Err(err)) => {
                        warn!("Steam: failed to fetch chat groups: {err}");
                        return Vec::new();
                    }
                    None => {
                        warn!(
                            "Steam: timed out fetching chat groups after {}s",
                            CHAT_GROUPS_TIMEOUT.as_secs()
                        );
                        return Vec::new();
                    }
                };

                let groups = response
                    .chat_room_groups
                    .iter()
                    .filter_map(|pair| pair.group_summary.as_ref())
                    .filter_map(chat_group_from_summary)
                    .collect::<Vec<_>>();

                let group_ids = groups.iter().map(|group| group.id).collect::<Vec<_>>();
                if !group_ids.is_empty() {
                    match timeout_after(
                        CHAT_GROUPS_TIMEOUT,
                        conn.service_method(CChatRoom_SetSessionActiveChatRoomGroups_Request {
                            chat_group_ids: group_ids.clone(),
                            chat_groups_data_requested: group_ids,
                            virtualize_members_threshold: Some(5000),
                            ..Default::default()
                        }),
                    )
                    .await
                    {
                        Some(Ok(_)) => {}
                        Some(Err(err)) => warn!("Steam: failed to activate chat groups: {err}"),
                        None => warn!(
                            "Steam: timed out activating chat groups after {}s",
                            CHAT_GROUPS_TIMEOUT.as_secs()
                        ),
                    }
                }

                for group in &groups {
                    chat_groups.insert(group.id, group.clone());
                    for room in &group.rooms {
                        room_locations.insert(
                            steam_group_room_id(room.chat_group_id, room.chat_id),
                            ChatRoomLocation::Group {
                                chat_group_id: room.chat_group_id,
                                chat_id: room.chat_id,
                            },
                        );
                    }
                }

                groups
            })
            .await?;

        Ok(groups)
    }

    pub(crate) async fn load_chat_group_details(
        &self,
        connected: &Connected,
        chat_group_id: ID,
    ) -> Result<(ChatGroupEntry, Vec<ID>), Box<dyn Error + Sync + Send>> {
        let conn = connected.conn.clone();
        let friends = connected.friends.clone();
        let chat_groups = connected.chat_groups.clone();
        let room_locations = connected.chat_room_locations.clone();
        let client_steamid = connected.client_steamid;

        let details = self
            .run(async move {
                let response = match timeout_after(
                    CHAT_GROUP_STATE_TIMEOUT,
                    conn.service_method(CChatRoom_GetChatRoomGroupState_Request {
                        chat_group_id: Some(chat_group_id),
                        ..Default::default()
                    }),
                )
                .await
                {
                    Some(Ok(response)) => response,
                    Some(Err(err)) => {
                        warn!("Steam: failed to fetch chat group {chat_group_id}: {err}");
                        return None;
                    }
                    None => {
                        warn!(
                            "Steam: timed out fetching chat group {chat_group_id} after {}s",
                            CHAT_GROUP_STATE_TIMEOUT.as_secs()
                        );
                        return None;
                    }
                };

                let state = response.state.as_ref()?;
                let group = chat_group_from_state(chat_group_id, state);
                let member_ids = state
                    .members
                    .iter()
                    .filter(|member| {
                        member.state.is_none()
                            || member.state() == EChatRoomJoinState::k_EChatRoomJoinState_Joined
                    })
                    .filter_map(|member| member.accountid)
                    .map(|account_id| account_id_to_steam_id(client_steamid, account_id))
                    .collect::<Vec<_>>();

                hydrate_profile_details(
                    conn.clone(),
                    friends.clone(),
                    member_ids.clone(),
                    "chat member",
                )
                .await;

                chat_groups.insert(group.id, group.clone());
                for room in &group.rooms {
                    room_locations.insert(
                        steam_group_room_id(room.chat_group_id, room.chat_id),
                        ChatRoomLocation::Group {
                            chat_group_id: room.chat_group_id,
                            chat_id: room.chat_id,
                        },
                    );
                }

                Some((group, member_ids))
            })
            .await?;

        if let Some(details) = details {
            return Ok(details);
        }

        if let Some(group) = connected.chat_groups.get(&chat_group_id) {
            return Ok((group.value().clone(), Vec::new()));
        }

        Ok((
            ChatGroupEntry {
                id: chat_group_id,
                name: format!("Steam group {chat_group_id}"),
                avatar_hash: None,
                rooms: Vec::new(),
            },
            Vec::new(),
        ))
    }
}

#[async_trait]
impl Query for SteamMessenger {
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        let connected = self.connected().await?;
        let conn = connected.conn.clone();
        let friends = connected.friends.clone();
        let client_steamid = connected.client_steamid;
        self.run(async move {
            hydrate_profile_details(conn, friends, vec![client_steamid], "client").await;
        })
        .await?;
        Ok(steam_user_identifier(
            connected.client_steamid,
            &connected.friends,
            connected.client_steamid,
            &connected.username,
        )
        .await)
    }

    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        let connected = self.connected().await?;
        wait_for_friend_list(&connected).await;
        let conn = connected.conn.clone();
        let friends = connected.friends.clone();
        let profile_details_loaded = connected.profile_details_loaded.clone();
        let friend_snapshot = self
            .run(async move {
                hydrate_unresolved_friends(conn, friends.clone(), profile_details_loaded).await;
                snapshot_friends(&friends)
            })
            .await?;
        let downloads = friend_snapshot
            .into_iter()
            .map(|(id, name, avatar_hash)| async move {
                let icon = match avatar_hash {
                    Some(hash) => cache_avatar(id, &hash).await,
                    None => None,
                };
                Identifier::new(id, User { name, icon })
            });
        let users = join_all(downloads).await;
        Ok(users)
    }

    /// Each friend's direct-message conversation is surfaced as a text-only
    /// room whose ID is the friend's SteamID — the same ID `get_messages` and
    /// `send_message` expect as `location`.
    async fn rooms(&self) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        let connected = self.connected().await?;
        wait_for_friend_list(&connected).await;
        let conn = connected.conn.clone();
        let friends = connected.friends.clone();
        let profile_details_loaded = connected.profile_details_loaded.clone();
        let (friend_snapshot, last_messages) = self
            .run(async move {
                hydrate_unresolved_friends(conn.clone(), friends.clone(), profile_details_loaded)
                    .await;
                let friend_snapshot = snapshot_friends(&friends);
                let last_messages = recent_message_times(conn, &friend_snapshot).await;
                (friend_snapshot, last_messages)
            })
            .await?;
        let downloads = friend_snapshot.into_iter().map(|(id, name, avatar_hash)| {
            let last_message_at = last_messages.get(&id).copied().unwrap_or(0);
            async move {
                let icon = match avatar_hash {
                    Some(hash) => cache_avatar(id, &hash).await,
                    None => None,
                };
                let sort_name = name.to_lowercase();
                let room = Room::new(RoomCapabilities::Text, None, None);
                (
                    last_message_at,
                    sort_name,
                    Identifier::new(id, Place::new(name, icon, room)),
                )
            }
        });
        let mut rooms = join_all(downloads).await;
        rooms.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let rooms = rooms.into_iter().map(|(_, _, room)| room).collect();
        Ok(rooms)
    }

    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        let connected = self.connected().await?;
        let groups = self.load_chat_groups(&connected).await?;
        let houses = join_all(groups.into_iter().map(|group| async move {
            let icon = steam_group_icon(&group).await;
            Identifier::new(group.id, Place::new(group.name, icon, House::new(None)))
        }))
        .await;
        Ok(houses)
    }

    async fn house_details(
        &self,
        house: Identifier<Place<House>>,
    ) -> Result<House, Box<dyn Error + Sync + Send>> {
        let connected = self.connected().await?;
        let (group, member_ids) = self
            .load_chat_group_details(&connected, *house.id())
            .await?;
        let participants = if member_ids.is_empty() {
            None
        } else {
            Some(
                join_all(member_ids.into_iter().map(|steamid| {
                    known_user(
                        steamid,
                        &connected.friends,
                        connected.client_steamid,
                        &connected.username,
                    )
                }))
                .await,
            )
        };

        let rooms = group
            .rooms
            .iter()
            .map(|room| chat_room_identifier(room, participants.clone()))
            .collect();
        Ok(House::new(Some(rooms)))
    }
}

#[async_trait]
impl Text for SteamMessenger {
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
        ordering: Ordering,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        let connected = self.connected().await?;
        let conn = connected.conn.clone();
        let client = connected.client_steamid;
        let username = connected.username.clone();
        let room_location = connected
            .chat_room_locations
            .get(location.id())
            .map(|location| *location)
            .unwrap_or(ChatRoomLocation::Direct {
                steamid: *location.id(),
            });

        let mut messages = match room_location {
            ChatRoomLocation::Direct { steamid: friend } => {
                let response = self
                    .run(async move {
                        conn.service_method(CFriendMessages_GetRecentMessages_Request {
                            steamid1: Some(client),
                            steamid2: Some(friend),
                            count: Some(RECENT_MESSAGE_COUNT),
                            // Request the BBCode form so emoticons/stickers come
                            // through for `rich::build_content` to parse.
                            bbcode_format: Some(true),
                            ..Default::default()
                        })
                        .await
                    })
                    .await??;

                // The sender is identified by 32-bit account id; the low 32
                // bits of our SteamID are our account id, which is how we tell
                // our own messages apart from the friend's.
                let client_account = client as u32;
                join_all(response.messages.into_iter().map(|msg| {
                    let friends = connected.friends.clone();
                    let username = username.clone();
                    async move {
                        let timestamp = msg.timestamp.unwrap_or(0);
                        let ordinal = msg.ordinal.unwrap_or(0);
                        let from_me = msg.accountid == Some(client_account);
                        let author_id = if from_me { client } else { friend };
                        let author =
                            steam_user_identifier(author_id, &friends, client, &username).await;
                        let revision = Revision {
                            at: DateTime::from_timestamp(timestamp as i64, 0),
                            text: crate::rich::build_content(&msg.message.unwrap_or_default())
                                .await,
                        };
                        Identifier::new(
                            message_id(timestamp, ordinal),
                            Message {
                                content: revision,
                                author: Some(author),
                                ..Default::default()
                            },
                        )
                    }
                }))
                .await
            }
            ChatRoomLocation::Group {
                chat_group_id,
                chat_id,
            } => {
                let friends = connected.friends.clone();
                let before = load_messages_before.as_ref().map(|message| *message.id());
                let response = self
                    .run(async move {
                        let mut request = CChatRoom_GetMessageHistory_Request {
                            chat_group_id: Some(chat_group_id),
                            chat_id: Some(chat_id),
                            max_count: Some(RECENT_MESSAGE_COUNT),
                            ..Default::default()
                        };
                        if let Some(before) = before {
                            request.last_time = Some((before >> 32) as u32);
                            request.last_ordinal = Some(before as u32);
                        }

                        let response = match timeout_after(
                            CHAT_MESSAGE_HISTORY_TIMEOUT,
                            conn.service_method(request),
                        )
                        .await
                        {
                            Some(Ok(response)) => response,
                            Some(Err(err)) => {
                                return Err::<_, Box<dyn Error + Sync + Send>>(
                                    format!("Steam: failed to fetch chat history: {err}").into(),
                                );
                            }
                            None => {
                                return Err::<_, Box<dyn Error + Sync + Send>>(
                                    format!(
                                        "Steam: timed out fetching chat history after {}s",
                                        CHAT_MESSAGE_HISTORY_TIMEOUT.as_secs()
                                    )
                                    .into(),
                                );
                            }
                        };

                        let sender_ids = response
                            .messages
                            .iter()
                            .filter_map(|message| message.sender)
                            .map(|account_id| account_id_to_steam_id(client, account_id))
                            .collect::<Vec<_>>();
                        hydrate_profile_details(
                            conn.clone(),
                            friends.clone(),
                            sender_ids,
                            "chat message author",
                        )
                        .await;

                        Ok(response)
                    })
                    .await??;

                let message_futures = response
                    .messages
                    .into_iter()
                    .filter(|msg| !msg.deleted.unwrap_or(false))
                    .filter_map(|msg| {
                        let raw = msg.message?.trim_end_matches('\0').to_owned();
                        if raw.trim().is_empty() {
                            return None;
                        }
                        let timestamp = msg.server_timestamp.unwrap_or(0);
                        let ordinal = msg.ordinal.unwrap_or(0);
                        let author_id = msg
                            .sender
                            .map(|account_id| account_id_to_steam_id(client, account_id));
                        let reactions = chat_message_reactions(msg.reactions);
                        let friends = connected.friends.clone();
                        let username = connected.username.clone();
                        Some(async move {
                            let author = match author_id {
                                Some(author_id) => Some(
                                    steam_user_identifier(author_id, &friends, client, &username)
                                        .await,
                                ),
                                None => None,
                            };
                            Identifier::new(
                                message_id(timestamp, ordinal),
                                Message {
                                    content: Revision {
                                        at: DateTime::from_timestamp(timestamp as i64, 0),
                                        text: crate::rich::build_content(&raw).await,
                                    },
                                    reactions,
                                    author,
                                    ..Default::default()
                                },
                            )
                        })
                    });

                join_all(message_futures).await
            }
        };

        if ordering == Ordering::Time {
            messages.sort_by_key(|msg| *msg.id());
        }
        Ok(messages)
    }

    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
        let connected = self.connected().await?;
        let conn = connected.conn.clone();
        // The user composes plain text; flatten to a string to send.
        let outgoing = contents.content.text.to_plain();
        let room_location = connected
            .chat_room_locations
            .get(location.id())
            .map(|location| *location)
            .unwrap_or(ChatRoomLocation::Direct {
                steamid: *location.id(),
            });

        let (timestamp, ordinal, text) = match room_location {
            ChatRoomLocation::Direct { steamid: friend } => {
                let response = self
                    .run(async move {
                        conn.service_method(CFriendMessages_SendMessage_Request {
                            steamid: Some(friend),
                            message: Some(outgoing),
                            chat_entry_type: Some(CHAT_ENTRY_TYPE_CHAT_MSG),
                            ..Default::default()
                        })
                        .await
                    })
                    .await??;

                (
                    response.server_timestamp.unwrap_or(0),
                    response.ordinal.unwrap_or(0),
                    contents.content.text,
                )
            }
            ChatRoomLocation::Group {
                chat_group_id,
                chat_id,
            } => {
                let response = self
                    .run(async move {
                        conn.service_method(CChatRoom_SendChatMessage_Request {
                            chat_group_id: Some(chat_group_id),
                            chat_id: Some(chat_id),
                            message: Some(outgoing),
                            echo_to_sender: Some(false),
                            ..Default::default()
                        })
                        .await
                    })
                    .await??;

                // Reflect the server's canonical (BBCode) form if it modified
                // the message, else echo what we composed.
                let text = match response.modified_message.filter(|m| !m.is_empty()) {
                    Some(bbcode) => crate::rich::build_content(&bbcode).await,
                    None => contents.content.text,
                };
                (
                    response.server_timestamp.unwrap_or(0),
                    response.ordinal.unwrap_or(0),
                    text,
                )
            }
        };

        let author = steam_user_identifier(
            connected.client_steamid,
            &connected.friends,
            connected.client_steamid,
            &connected.username,
        )
        .await;
        let revision = Revision {
            at: DateTime::from_timestamp(timestamp as i64, 0),
            text,
        };
        Ok(Identifier::new(
            message_id(timestamp, ordinal),
            Message {
                content: revision,
                author: Some(author),
                ..Default::default()
            },
        ))
    }

    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<TextEvent>, Box<dyn Error + Sync + Send>> {
        // Establish once so a hard failure (bad credentials) surfaces here.
        // The stream re-acquires the session via `connected()` on every poll,
        // so it transparently rides through reconnects after a Steam logoff.
        self.connected().await?;
        Ok(WeakSocketStream::from_arc(self))
    }
}

/// Capped exponential backoff for reconnect attempt `attempt` (1-based):
/// 1s, 2s, 4s, 8s, 16s, then capped at 30s.
fn reconnect_backoff(attempt: u32) -> Duration {
    const CAP: Duration = Duration::from_secs(30);
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(5)).min(CAP)
}

/// Record a reconnect attempt and report whether the retry budget is spent.
/// A failure more than `stable` after the previous one resets the counter, so
/// only *rapid* repeated failures (a re-kick loop) count toward `max`; a
/// long-lived session that drops once does not inch toward giving up.
fn note_reconnect(
    attempts: &mut u32,
    last_at: &mut Option<Instant>,
    max: u32,
    stable: Duration,
) -> bool {
    let now = Instant::now();
    if last_at.is_some_and(|t| now.duration_since(t) > stable) {
        *attempts = 0;
    }
    *attempts += 1;
    *last_at = Some(now);
    *attempts > max
}

/// The live text-event stream is driven by the *messenger*, not a specific
/// [`Connected`], so it survives a dropped Steam session: when the current
/// session dies (Steam logoff), `connected()` re-establishes from the saved
/// refresh token and the loop resumes on the fresh connection. Queries share
/// the same `connected()`, so they recover too. Terminates only when the app
/// drops its last `Arc<dyn Messenger>` handle.
///
/// Caveat: a raw TCP drop with no `CMsgClientLoggedOff` is *not* detected —
/// steam-vent exposes no socket-close signal and its broadcast streams go
/// silent rather than ending, so that case still needs an app restart.
#[async_trait]
impl ArcStream for SteamMessenger {
    type Item = TextEvent;

    async fn next(self: Arc<Self>) -> Option<Self::Item> {
        const MAX_RECONNECT_ATTEMPTS: u32 = 6;
        const STABLE_SESSION: Duration = Duration::from_secs(60);

        let mut attempts: u32 = 0;
        let mut last_reconnect: Option<Instant> = None;

        loop {
            // Only this in-flight `next` future still holds the messenger →
            // the app dropped its handle; let `WeakSocketStream` end.
            if Arc::strong_count(&self) == 1 {
                return None;
            }

            let connected = match self.connected().await {
                Ok(connected) => connected,
                Err(err) => {
                    if note_reconnect(
                        &mut attempts,
                        &mut last_reconnect,
                        MAX_RECONNECT_ATTEMPTS,
                        STABLE_SESSION,
                    ) {
                        error!(
                            "Steam: giving up after {MAX_RECONNECT_ATTEMPTS} failed reconnects ({err}); restart to retry"
                        );
                        return None;
                    }
                    let backoff = reconnect_backoff(attempts);
                    warn!(
                        "Steam: reconnect failed ({err}); retrying in {}s (attempt {attempts}/{MAX_RECONNECT_ATTEMPTS})",
                        backoff.as_secs()
                    );
                    if !self.sleep_unless_dropped(backoff).await {
                        return None;
                    }
                    continue;
                }
            };

            match connected.next_text_event(TEXT_EVENT_IDLE_CHECK).await {
                TextPoll::Text(event) => return Some(event),
                TextPoll::Idle => continue,
                TextPoll::Disconnected => {
                    // `next_text_event` already latched the session dead, so
                    // the next `connected()` rebuilds it. Bound a re-kick loop.
                    if note_reconnect(
                        &mut attempts,
                        &mut last_reconnect,
                        MAX_RECONNECT_ATTEMPTS,
                        STABLE_SESSION,
                    ) {
                        error!(
                            "Steam: re-kicked {MAX_RECONNECT_ATTEMPTS} times in quick succession; giving up (restart to retry)"
                        );
                        return None;
                    }
                    let backoff = reconnect_backoff(attempts);
                    warn!(
                        "Steam: session disconnected; re-establishing in {}s (attempt {attempts}/{MAX_RECONNECT_ATTEMPTS})",
                        backoff.as_secs()
                    );
                    if !self.sleep_unless_dropped(backoff).await {
                        return None;
                    }
                    continue;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_avatar_cache_category_uses_misc_for_non_users() {
        // 76561197969249708 is an individual Steam account.
        assert_eq!(
            crate::api_types::steam_avatar_cache_category(76561197969249708).as_str(),
            "users"
        );
        // 103582791432294076 is a clan/group SteamID.
        assert_eq!(
            crate::api_types::steam_avatar_cache_category(103582791432294076).as_str(),
            "misc"
        );
    }

    #[test]
    fn steam_group_room_ids_are_namespaced_and_stable() {
        let room_id = steam_group_room_id(123, 456);
        assert_eq!(room_id, steam_group_room_id(123, 456));
        assert_ne!(room_id, steam_group_room_id(123, 457));
        assert_ne!(room_id, 76561197969249708);
        assert_ne!(room_id >> 63, 0);
    }

    #[test]
    fn account_id_to_steam_id_preserves_client_prefix() {
        let client = 76561197969249708;
        let account_id = 12345;
        assert_eq!(
            account_id_to_steam_id(client, account_id),
            (client & !(u32::MAX as u64)) | account_id as u64
        );
    }

    #[test]
    fn reconnect_backoff_grows_then_caps() {
        assert_eq!(reconnect_backoff(1), Duration::from_secs(1));
        assert_eq!(reconnect_backoff(2), Duration::from_secs(2));
        assert_eq!(reconnect_backoff(3), Duration::from_secs(4));
        assert_eq!(reconnect_backoff(5), Duration::from_secs(16));
        // 1 << 5 == 32, capped to 30; and never grows past the cap.
        assert_eq!(reconnect_backoff(6), Duration::from_secs(30));
        assert_eq!(reconnect_backoff(100), Duration::from_secs(30));
    }

    #[test]
    fn note_reconnect_bounds_a_rapid_re_kick_loop() {
        let max = 6;
        let stable = Duration::from_secs(60);
        let mut attempts = 0;
        let mut last = None;
        // Rapid failures (all within the stable window) accumulate and stay
        // under budget up to `max`...
        for _ in 0..max {
            assert!(!note_reconnect(&mut attempts, &mut last, max, stable));
        }
        // ...then the next one exhausts it, so the stream gives up.
        assert!(note_reconnect(&mut attempts, &mut last, max, stable));
    }

    #[test]
    fn note_reconnect_resets_after_a_stable_session() {
        // A zero stable threshold means any elapsed time counts as "stable",
        // so a fresh failure resets the accumulated count instead of giving up.
        let mut attempts = 5;
        let mut last = Some(Instant::now());
        std::thread::sleep(Duration::from_millis(2));
        assert!(!note_reconnect(&mut attempts, &mut last, 6, Duration::ZERO));
        assert_eq!(attempts, 1, "counter resets to 0 then records this attempt");
    }
}
