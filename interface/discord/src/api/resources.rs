//! Discord REST and Gateway resource objects.
//!
//! Resource index: <https://docs.discord.food/resources>

use chrono::{DateTime, Utc};
use facet::Facet;
use futures::{StreamExt, future::join_all, stream};
use messenger_interface::types::{
    Identifier, Place, Revision, RichText, Room, RoomCapabilities, UserStatus,
};
use tracing::error;

use crate::downloaders::CdnImage;

// Discord documentation names this scalar SNOWFLAKE. Keep protocol spelling
// at adapter boundary; unified/public IDs remain ordinary `u64`.
#[allow(clippy::upper_case_acronyms)]
pub type SNOWFLAKE = u64;

// === Users ===
#[derive(Facet)]
#[facet(rename_all = "snake_case")]
#[repr(u8)]
pub(crate) enum DiscordPresenceStatus {
    Online,
    Idle,
    #[facet(rename = "dnd")]
    DoNotDisturb,
    Offline,
    Invisible,
}

impl From<DiscordPresenceStatus> for UserStatus {
    fn from(status: DiscordPresenceStatus) -> Self {
        match status {
            DiscordPresenceStatus::Online => Self::Online,
            DiscordPresenceStatus::Idle => Self::Idle,
            DiscordPresenceStatus::DoNotDisturb => Self::DoNotDisturb,
            DiscordPresenceStatus::Offline | DiscordPresenceStatus::Invisible => Self::Offline,
        }
    }
}

/// Current-user object returned by `GET /users/@me`.
/// <https://docs.discord.food/resources/user#user-object>
#[derive(Facet)]
pub struct Profile {
    // accent_color: Option<String>,
    // authenticator_types: Vec<String>,
    pub avatar: Option<String>,
    // avatar_decoration_data: Option<String>,
    // banner: Option<String>,
    // banner_color: Option<String>,
    // bio: String,
    // clan: Option<String>,
    // discriminator: String,
    // email: String,
    // flags: i32,
    // global_name: String,
    pub id: SNOWFLAKE,
    // linked_users: Vec<String>,
    // locale: String,
    // mfa_enabled: bool,
    // nsfw_allowed: bool,
    // phone: Option<String>,
    // premium_type: i32,
    // public_flags: i32,
    pub global_name: Option<String>,
    pub username: String,
    // verified: bool,
}

/// Reusable partial Discord user object.
/// <https://docs.discord.food/resources/user#user-object>
#[derive(Facet, Clone)]
pub struct User {
    pub avatar: Option<String>,
    // avatar_decoration_data: Option<String>,
    // clan: Option<String>,
    // discriminator: String,
    pub id: SNOWFLAKE,
    pub global_name: Option<String>,
    pub username: String,
}

/// Relationship object returned by `GET /users/@me/relationships`.
/// <https://docs.discord.food/resources/relationships#relationship-object>
#[derive(Clone, Facet)]
pub struct Friend {
    pub id: SNOWFLAKE,
    // is_spam_request: bool,
    // nickname: Option<String>,
    // since: String,
    // type: i32,
    pub user: User,
}

/// Partial user embedded in a private channel's `recipients`.
/// <https://docs.discord.food/resources/channel#channel-object>
#[derive(Clone, Facet)]
pub struct Recipient {
    pub(crate) avatar: Option<String>,
    // avatar_decoration_data: Option<String>,
    // clan: Option<String>,
    // discriminator: String,
    pub(crate) global_name: Option<String>,
    pub(crate) id: SNOWFLAKE,
    // public_flags: i32,
    pub(crate) username: String,
}

// === Channels ===

/// Permission overwrite embedded in a guild channel.
/// <https://docs.discord.food/resources/channel#permission-overwrite-object>
#[derive(Clone, Facet)]
pub(crate) struct OverwriteObject {
    // pub(crate) id: String,
    // pub(crate) allow: String,
    pub(crate) deny: String,
}

/// <https://docs.discord.food/resources/channel#channel-type>
#[derive(Facet, Clone, Copy)]
#[facet(is_numeric)]
#[repr(u8)]
pub enum ChannelTypes {
    GuildText = 0,
    DM = 1,
    GuildVoice = 2,
    GroupDM = 3,
    GuildCategory = 4,
    GuildAnnouncement = 5,
    AnnouncementThread = 10,
    PublicThread = 11,
    PrivateThread = 12,
    GuildStageVoice = 13,
    GuildDirectory = 14,
    GuildForum = 15,
    GuildMedia = 16,
}
impl From<ChannelTypes> for RoomCapabilities {
    fn from(val: ChannelTypes) -> Self {
        match val {
            ChannelTypes::DM | ChannelTypes::GroupDM => {
                RoomCapabilities::Text | RoomCapabilities::Voice
            }
            ChannelTypes::GuildText | ChannelTypes::GuildAnnouncement => RoomCapabilities::Text,
            ChannelTypes::GuildVoice | ChannelTypes::GuildStageVoice => RoomCapabilities::Voice,
            ChannelTypes::GuildCategory => RoomCapabilities::empty(),
            _ => RoomCapabilities::empty(),
        }
    }
}

/// Discord channel object used for guild channels and private channels.
/// <https://docs.discord.food/resources/channel#channel-object>
#[derive(Clone, Facet)]
pub struct Channel {
    pub id: SNOWFLAKE,
    pub guild_id: Option<SNOWFLAKE>,
    #[facet(rename = "type")]
    pub channel_type: ChannelTypes,
    // flags: i32,
    pub position: Option<i32>,
    pub parent_id: Option<String>,
    pub icon: Option<String>,
    pub last_message_id: Option<SNOWFLAKE>,
    pub name: Option<String>,
    pub recipients: Option<Vec<Recipient>>,
    pub permission_overwrites: Option<Vec<OverwriteObject>>,
}
impl Channel {
    /// Extract room data, name, and icon from a channel.
    /// Returns (name, icon, room_data).
    pub async fn to_room_data(&self) -> Place<Room> {
        let name = self.name.clone().unwrap_or_else(|| {
            self.recipients
                .as_ref()
                .map(|recipients| {
                    recipients
                        .iter()
                        .map(|recipient| recipient.username.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| "Unknown channel".to_string())
        });

        let recipients = join_all(self.recipients.as_deref().unwrap_or(&[]).iter().map(
            async |recipient| {
                let recipient_icon = match &recipient.avatar {
                    Some(hash) => match CdnImage::avatar(recipient.id, hash).fetch().await {
                        Ok(path) => Some(path),
                        Err(e) => {
                            error!("Failed to download icon for channel: {}\n{}", name, e);
                            None
                        }
                    },
                    None => None,
                };

                Identifier::new(
                    recipient.id,
                    messenger_interface::types::User {
                        name: recipient
                            .global_name
                            .clone()
                            .unwrap_or_else(|| recipient.username.clone()),
                        tag: Some(recipient.username.clone()),
                        icon: recipient_icon,
                        status: None,
                        voice: None,
                    },
                )
            },
        ))
        .await;

        // Deterministic fallback icon: the first recipient (in roster
        // order) that has an avatar — not whichever download finished
        // first.
        let mut icon = recipients
            .iter()
            .find_map(|recipient| recipient.icon.clone());

        // If channel has icon, use that
        if let Some(hash) = &self.icon {
            match CdnImage::channel_icon(self.id, hash).fetch().await {
                Ok(path) => icon = Some(path),
                Err(e) => {
                    error!("Failed to download icon for channel: {}\n{}", name, e);
                }
            };
        }

        let room = Room::new(
            // NOTE: DMs can have voice calls; treat as both for now.
            RoomCapabilities::from(self.channel_type),
            // Recipients are the room's members; who is in a call is
            // tracked separately in `participants` via `VoiceEvent`s.
            Some(recipients),
            None,
            None,
        );

        Place {
            name,
            icon,
            place_data: room,
        }
    }
}

/// Normal/burst reaction counts.
/// <https://docs.discord.food/resources/message#reaction-count-details-structure>
#[derive(Facet)]
pub struct CountDetails {
    // burst: u32,
    // normal: u32,
}

/// Emoji embedded in message reactions.
/// <https://docs.discord.food/resources/emoji#emoji-object>
#[derive(Facet, Clone, PartialEq, Eq)]
pub struct Emoji {
    pub id: Option<SNOWFLAKE>,
    pub name: String,
}

/// Reaction summary embedded in a message.
/// <https://docs.discord.food/resources/message#reaction-object>
#[derive(Facet, Clone)]
pub struct Reaction {
    // burst_colors: Vec<String>,
    // burst_count: u32,
    // burst_me: bool,
    pub count: u32,
    // count_details: CountDetails,
    pub emoji: Emoji,
    pub me: bool,
    // me_burst: bool,
}

/// One sticker attached to a message (Discord `sticker_items`).
/// <https://docs.discord.food/resources/sticker#sticker-item-object>
#[derive(Facet, Clone)]
pub struct StickerItem {
    pub id: SNOWFLAKE,
    pub name: String,
    /// 1 = PNG, 2 = APNG, 3 = Lottie (JSON), 4 = GIF.
    pub format_type: u8,
}

/// A guild's custom emoji (Ready/GuildCreate `emojis` entry subset).
/// <https://docs.discord.food/resources/emoji#emoji-object>
#[derive(Facet, Clone)]
pub struct GuildEmoji {
    pub id: SNOWFLAKE,
    /// `None` only for emoji Discord no longer resolves; those are unusable.
    pub name: Option<String>,
    pub animated: Option<bool>,
    /// False when the emoji is unusable (e.g. lost through a Boost downgrade).
    pub available: Option<bool>,
}

/// A guild's sticker (Ready/GuildCreate `stickers` entry subset).
/// <https://docs.discord.food/resources/sticker#sticker-object>
#[derive(Facet, Clone)]
pub struct GuildSticker {
    pub id: SNOWFLAKE,
    pub name: String,
    /// 1 = PNG, 2 = APNG, 3 = Lottie (JSON), 4 = GIF.
    pub format_type: u8,
    pub available: Option<bool>,
}

/// Discord message object used by REST and Gateway events.
/// <https://docs.discord.food/resources/message#message-object>
#[derive(Facet, Clone)]
pub struct Message {
    // attachments: Vec<String>,
    pub author: User,
    pub channel_id: SNOWFLAKE,
    // components: Vec<String>,
    pub content: String,
    pub edited_timestamp: Option<String>,
    // embeds: Vec<u32>,
    // flags: u32,
    pub id: SNOWFLAKE,
    // mention_everyone: bool,
    // mention_roles: Vec<String>,
    // mentions: Vec<String>,
    // pinned: bool,
    pub reactions: Option<Vec<Reaction>>,
    pub sticker_items: Option<Vec<StickerItem>>,
    pub timestamp: String,
    // tts: bool,
    // type: u32,
}

impl Message {
    /// Map the Discord reaction objects onto interface reactions. An absent
    /// `reactions` field maps to an empty list.
    pub async fn interface_reactions(&self) -> Vec<messenger_interface::types::Reaction> {
        const MAX_REACTION_FETCH_CONCURRENCY: usize = 8;

        stream::iter(self.reactions.as_deref().unwrap_or(&[]).iter().cloned())
            .map(|reaction| async move {
                // A custom emoji carries an id (→ resolve its image); a Unicode
                // emoji has no id and renders from its name alone.
                let image = match reaction.emoji.id {
                    Some(id) => crate::adapter::text::rich::resolve_emoji(id, false).await,
                    None => None,
                };
                messenger_interface::types::Reaction {
                    emoji: messenger_interface::types::Emoji {
                        shortcode: reaction.emoji.name,
                        image,
                        // Reactions are sent by name:id, not the content wire
                        // tag; the compose reference doesn't apply here.
                        reference: None,
                    },
                    count: reaction.count,
                    reacted: reaction.me,
                }
            })
            .buffered(MAX_REACTION_FETCH_CONCURRENCY)
            .collect()
            .await
    }

    /// Split this Discord message into `(content, history)` for the
    /// `messenger_interface::types::Message` fields.
    ///
    /// Discord's REST and gateway payloads expose `timestamp` (creation
    /// time) and `edited_timestamp` (last-edit time, present only if the
    /// message has ever been edited), but they don't surface the original
    /// *content* of an edited message. So when `edited_timestamp` is
    /// present, we populate `history` with a single placeholder revision
    /// whose `text` is empty — enough to drive the UI's "edited"
    /// indicator while being honest that we don't know what the message
    /// used to say.
    pub async fn revisions(&self) -> (Revision, Vec<Revision>) {
        let parse = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        };
        let created_at = parse(&self.timestamp);
        let stickers = self.sticker_items.as_deref().unwrap_or(&[]);
        let content = crate::adapter::text::rich::build_content(&self.content, stickers).await;
        match self.edited_timestamp.as_deref().and_then(parse) {
            Some(edit_at) => (
                Revision {
                    at: Some(edit_at),
                    text: content,
                },
                vec![Revision {
                    at: created_at,
                    text: RichText::default(),
                }],
            ),
            None => (
                Revision {
                    at: created_at,
                    text: content,
                },
                Vec::new(),
            ),
        }
    }
}

/// `GET /users/{user_id}` — public user object subset.
/// <https://docs.discord.food/resources/user#get-user>
#[derive(Facet)]
pub struct UserDetails {
    pub id: SNOWFLAKE,
    pub username: String,
    pub global_name: Option<String>,
    pub bot: Option<bool>,
}

/// The `user_profile` sub-object of the profile endpoint response.
#[derive(Facet)]
pub struct UserProfileMeta {
    pub bio: Option<String>,
    pub pronouns: Option<String>,
}

#[derive(Facet)]
pub struct MutualGuild {
    pub id: SNOWFLAKE,
}

/// `GET /users/{user_id}/profile` — rich profile, user-token only endpoint.
/// Requires a mutual friend or guild with the target; 403s otherwise, in
/// which case callers fall back to the public user object.
/// <https://docs.discord.food/resources/user#get-user-profile>
#[derive(Facet)]
pub struct UserProfile {
    pub user: UserDetails,
    pub user_profile: Option<UserProfileMeta>,
    pub premium_since: Option<String>,
    pub mutual_guilds: Option<Vec<MutualGuild>>,
}

/// `GET /guilds/{guild_id}?with_counts=true` response subset. Kept separate
/// from [`Guild`] (the list/Ready shape) so extending it can never disturb
/// parsing of those payloads.
/// <https://docs.discord.food/resources/guild#get-guild>
#[derive(Facet)]
pub struct GuildDetails {
    pub id: SNOWFLAKE,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: Option<SNOWFLAKE>,
    pub approximate_member_count: Option<u32>,
    pub approximate_presence_count: Option<u32>,
}

/// Guild object used by guild-list and Gateway payloads.
/// <https://docs.discord.food/resources/guild#guild-object>
#[derive(Clone, Facet)]
pub struct Guild {
    pub id: SNOWFLAKE,
    pub name: String,
    pub icon: Option<String>,
    // pub icon_hash: Option<String>,
    // pub splash: Option<String>,
    // pub discovery_splash: Option<String>,
    // pub owner: Option<bool>,
    // pub owner_id: String,  // Snowflake
    // pub permissions: Option<String>,
    // pub region: Option<String>,  // Deprecated
    // pub afk_channel_id: Option<String>,  // Snowflake
    // pub afk_timeout: u32,
    // pub widget_enabled: Option<bool>,
    // pub widget_channel_id: Option<String>,  // Snowflake
    // pub verification_level: u8,
    // pub default_message_notifications: u8,
    // pub explicit_content_filter: u8,
    // pub roles: Vec<Role>,
    // pub emojis: Vec<Emoji>,
    // pub features: Vec<String>,
    // pub mfa_level: u8,
    // pub application_id: Option<String>,  // Snowflake
    // pub system_channel_id: Option<String>,  // Snowflake
    // pub system_channel_flags: u32,
    // pub rules_channel_id: Option<String>,  // Snowflake
    // pub max_presences: Option<u32>,
    // pub max_members: Option<u32>,
    // pub vanity_url_code: Option<String>,
    // pub description: Option<String>,
    // pub banner: Option<String>,
    // pub premium_tier: u8,
    // pub premium_subscription_count: Option<u32>,
    // pub preferred_locale: String,
    // pub public_updates_channel_id: Option<String>,  // Snowflake
    // pub max_video_channel_users: Option<u32>,
    // pub max_stage_video_channel_users: Option<u32>,
    // pub approximate_member_count: Option<u32>,
    // pub approximate_presence_count: Option<u32>,
    // pub welcome_screen: Option<WelcomeScreen>,
    // pub nsfw_level: u8,
    // pub stickers: Option<Vec<Sticker>>,
    // pub premium_progress_bar_enabled: bool,
    // pub safety_alerts_channel_id: Option<String>,  // Snowflake
    // pub incidents_data: Option<IncidentsData>,
}
