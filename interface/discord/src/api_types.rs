use chrono::{DateTime, Utc};
use facet::Facet;
use futures::future::join_all;
use messenger_interface::types::{Identifier, Place, Revision, RichText, Room, RoomCapabilities};
use tracing::error;

use crate::downloaders::CdnImage;

pub type SNOWFLAKE = u64;

// === Users ===
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
    pub username: String,
    // verified: bool,
}

#[derive(Facet, Clone)]
pub struct User {
    pub avatar: Option<String>,
    // avatar_decoration_data: Option<String>,
    // clan: Option<String>,
    // discriminator: String,
    pub id: SNOWFLAKE,
    pub username: String,
}

#[derive(Facet)]
pub struct Friend {
    pub id: SNOWFLAKE,
    // is_spam_request: bool,
    // nickname: Option<String>,
    // since: String,
    // type: i32,
    pub user: User,
}

#[derive(Facet)]
pub struct Recipient {
    pub(crate) avatar: Option<String>,
    // avatar_decoration_data: Option<String>,
    // clan: Option<String>,
    // discriminator: String,
    // global_name: Option<String>,
    pub(crate) id: SNOWFLAKE,
    // public_flags: i32,
    pub(crate) username: String,
}

// === Chennels ===

#[derive(Facet)]
pub(crate) struct OverwriteObject {
    // pub(crate) id: String,
    // pub(crate) allow: String,
    pub(crate) deny: String,
}

/// <https://discord.com/developers/docs/resources/channel#channel-object-channel-types>
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

#[derive(Facet)]
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
                        name: recipient.username.clone(),
                        icon: recipient_icon,
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
            Some(recipients),
            None,
        );

        Place {
            name,
            icon,
            place_data: room,
        }
    }
}

#[derive(Facet)]
pub struct CountDetails {
    // burst: u32,
    // normal: u32,
}

#[derive(Facet, Clone)]
pub struct Emoji {
    pub id: Option<SNOWFLAKE>,
    pub name: String,
}

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
#[derive(Facet, Clone)]
pub struct StickerItem {
    pub id: SNOWFLAKE,
    pub name: String,
    /// 1 = PNG, 2 = APNG, 3 = Lottie (JSON), 4 = GIF.
    pub format_type: u8,
}

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
        let mut reactions = Vec::new();
        for reaction in self.reactions.as_deref().unwrap_or(&[]) {
            // A custom emoji carries an id (→ resolve its image); a Unicode
            // emoji has no id and renders from its name alone.
            let image = match reaction.emoji.id {
                Some(id) => crate::rich::resolve_emoji(id, false).await,
                None => None,
            };
            reactions.push(messenger_interface::types::Reaction {
                emoji: messenger_interface::types::Emoji {
                    shortcode: reaction.emoji.name.clone(),
                    image,
                },
                count: reaction.count,
                reacted: reaction.me,
            });
        }
        reactions
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
        let content = crate::rich::build_content(&self.content, stickers).await;
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

// https://discord.com/developers/docs/resources/message#create-message-jsonform-params
#[derive(Facet)]
pub struct CreateMessage {
    pub nonce: Option<String>, // Can be used to verify a message was sent (up to 25 characters). Value will appear in the Message Create event.
    pub enforce_nonce: Option<bool>, // If true, checks nonce uniqueness
    pub tts: Option<bool>,     // True if this is a TTS message
    pub content: Option<String>, // Up to 2000 characters
    //
    // embeds: Option<Vec<Embed>>,                // Up to 10 rich embeds (max 6000 chars total)
    // allowed_mentions: Option<AllowedMentions>, // Who can be mentioned
    // message_reference: Option<MessageReference>, // Reply or forward
    // components: Option<Vec<Component>>,        // Components to include with the message
    // sticker_ids: Option<Vec<Snowflake>>,        // IDs of up to 3 stickers
    // files: Option<Vec<FileContent>>,            // Files being sent
    // payload_json: Option<String>,               // JSON-encoded body for multipart/form-data
    // attachments: Option<Vec<Attachment>>,       // Attachments (filename, description)
    //
    pub flags: Option<u32>, // Bitfield (only certain flags allowed)
                            // poll: Option<Poll>, // Poll object
}

// === Auth / Login ===
// Username + password login, as used by the official client (not the bot API).
// Field/endpoint shapes verified against: https://docs.discord.food/authentication

/// Body for `POST /auth/login`. Per the docs `login` and `password` are
/// required (`password` is 8-72 chars); `undelete` is optional. The remaining
/// optional fields (`login_source`, `gift_code_sku_id`) are omitted.
/// <https://docs.discord.food/authentication#login-account>
#[derive(Facet)]
pub struct LoginRequest {
    /// Email or E.164-formatted phone number of the account.
    pub login: String,
    pub password: String,
    /// Whether to reactivate a disabled/deleted account on login.
    pub undelete: bool,
}

/// Response to `POST /auth/login`. On a plain login `token` is populated; when
/// the account has two-factor enabled Discord instead returns `mfa: true` plus
/// a `ticket` (and `login_instance_id`) to be redeemed via one of the
/// `/auth/mfa/{type}` endpoints. Unknown fields (`user_settings`,
/// `required_actions`, captcha challenge, ...) are ignored by facet.
/// <https://docs.discord.food/authentication#login-account>
#[derive(Facet)]
pub struct LoginResponse {
    pub token: Option<String>,
    pub mfa: Option<bool>,
    pub ticket: Option<String>,
    /// Whether a TOTP (authenticator app) code is an accepted MFA method.
    pub totp: Option<bool>,
    /// Opaque instance id for the MFA flow, echoed back in the MFA request.
    pub login_instance_id: Option<String>,
}

/// Body for `POST /auth/mfa/totp` — redeems the login `ticket` with a TOTP code.
/// Per the docs the fields are `ticket` (required), `code` (required), and the
/// optional `login_instance_id`/`login_source`/`gift_code_sku_id`; there is no
/// `login_type` field.
/// <https://docs.discord.food/authentication#verify-mfa>
#[derive(Facet)]
pub struct MfaTotpRequest {
    /// The MFA ticket received from the login response.
    pub ticket: String,
    /// The TOTP (authenticator app) code.
    pub code: String,
    /// Echoed from `LoginResponse::login_instance_id` when present.
    pub login_instance_id: Option<String>,
}

/// Response to `POST /auth/mfa/totp`.
/// <https://docs.discord.food/authentication#verify-mfa>
#[derive(Facet)]
pub struct MfaResponse {
    pub token: Option<String>,
}

// https://discord.com/developers/docs/events/gateway-events#message-delete
#[derive(Facet)]
pub struct MessageDelete {
    pub id: SNOWFLAKE,
    pub channel_id: SNOWFLAKE,
    // guild_id: Option<SNOWFLAKE>,
}

// https://discord.com/developers/docs/events/gateway-events#message-reaction-add
#[derive(Facet)]
pub struct MessageReactionChange {
    pub user_id: SNOWFLAKE,
    pub channel_id: SNOWFLAKE,
    pub message_id: SNOWFLAKE,
    // guild_id: Option<SNOWFLAKE>,
    pub emoji: Emoji,
}

// https://discord.com/developers/docs/resources/guild#guild-object
#[derive(Facet)]
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
