//! Steam protocol constants and the small value/cache types the adapter keeps.
//!
//! These are deliberately interface-local: the cache structs ([`FriendEntry`],
//! [`ChatGroupEntry`], ...) hold just the profile fields the UI needs, and the
//! ID helpers translate between Steam's wire identifiers and the app's local
//! [`ID`] namespace.

use messenger_interface::types::{CacheCategory, ID};

/// `k_EFriendRelationshipFriend` — the relationship value for an actual,
/// mutually-accepted friend (as opposed to a pending/blocked entry).
pub(crate) const EFRIENDRELATIONSHIP_FRIEND: u32 = 3;
/// `k_EChatEntryTypeChatMsg` — a normal chat message.
pub(crate) const CHAT_ENTRY_TYPE_CHAT_MSG: i32 = 1;

/// Cached per-friend data, merged from two Steam pushes: the friends list
/// (relationship) and persona state (display name + avatar hash).
#[derive(Default, Clone)]
pub(crate) struct FriendEntry {
    pub(crate) name: Option<String>,
    pub(crate) relationship: u32,
    /// Hex-encoded avatar SHA1, used to build the CDN URL. `None` means no
    /// custom avatar.
    pub(crate) avatar_hash: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ChatRoomLocation {
    Direct { steamid: ID },
    Group { chat_group_id: ID, chat_id: ID },
}

#[derive(Clone, Debug)]
pub(crate) struct ChatRoomEntry {
    pub(crate) chat_group_id: ID,
    pub(crate) chat_id: ID,
    pub(crate) name: String,
    pub(crate) voice_allowed: bool,
    pub(crate) last_message_at: u32,
    pub(crate) sort_order: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatGroupEntry {
    pub(crate) id: ID,
    pub(crate) name: String,
    pub(crate) avatar_hash: Option<String>,
    pub(crate) rooms: Vec<ChatRoomEntry>,
}

pub(crate) fn account_id_to_steam_id(client_steamid: ID, account_id: u32) -> ID {
    (client_steamid & !(u32::MAX as u64)) | account_id as u64
}

pub(crate) fn steam_group_room_id(chat_group_id: ID, chat_id: ID) -> ID {
    // Interface IDs are local, so use a fixed FNV-1a hash and reserve the top
    // bit for Steam group rooms. Friend DM IDs are real SteamIDs and stay below
    // this namespace in normal use.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in chat_group_id
        .to_le_bytes()
        .into_iter()
        .chain(chat_id.to_le_bytes())
    {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash | (1 << 63)
}

pub(crate) fn steam_avatar_cache_category(steamid: ID) -> CacheCategory {
    const STEAM_ACCOUNT_TYPE_SHIFT: u64 = 52;
    const STEAM_ACCOUNT_TYPE_MASK: u64 = 0xF;
    const STEAM_ACCOUNT_TYPE_INDIVIDUAL: u64 = 1;

    let account_type = (steamid >> STEAM_ACCOUNT_TYPE_SHIFT) & STEAM_ACCOUNT_TYPE_MASK;
    if account_type == STEAM_ACCOUNT_TYPE_INDIVIDUAL {
        CacheCategory::Users
    } else {
        CacheCategory::Misc
    }
}

/// Build an `Identifier<Message>` ID that encodes `(timestamp, ordinal)` so
/// that sorting by ID matches chronological order. Steam messages have no
/// single opaque ID; this pair uniquely orders messages within a conversation.
pub(crate) fn message_id(timestamp: u32, ordinal: u32) -> ID {
    ((timestamp as u64) << 32) | ordinal as u64
}

/// Hex-encode a Steam avatar hash (20-byte SHA1). Returns `None` for the empty
/// or all-zero "no custom avatar" hash, so we fall back to no icon.
pub(crate) fn hex_avatar(hash: &[u8]) -> Option<String> {
    use std::fmt::Write;
    if hash.is_empty() || hash.iter().all(|&byte| byte == 0) {
        return None;
    }
    Some(
        hash.iter()
            .fold(String::with_capacity(hash.len() * 2), |mut s, byte| {
                let _ = write!(s, "{byte:02x}");
                s
            }),
    )
}
