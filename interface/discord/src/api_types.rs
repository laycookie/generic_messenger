use facet::Facet;
use messenger_interface::types::{Identifier, Room, RoomCapabilities};
use tracing::error;

use crate::{Discord, downloaders::cache_download};

// === Users ===
#[derive(Facet)]
pub struct Profile {
    // accent_color: Option<String>,
    // authenticator_types: Vec<String>,
    // avatar: Option<String>,
    // avatar_decoration_data: Option<String>,
    // banner: Option<String>,
    // banner_color: Option<String>,
    // bio: String,
    // clan: Option<String>,
    // discriminator: String,
    // email: String,
    // flags: i32,
    // global_name: String,
    pub id: String,
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

#[derive(Facet)]
pub struct User {
    pub avatar: Option<String>,
    // avatar_decoration_data: Option<String>,
    // clan: Option<String>,
    // discriminator: String,
    pub id: String,
    pub username: String,
}

#[derive(Facet)]
pub struct Friend {
    pub id: String,
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
    pub(crate) id: String,
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

#[derive(Facet, Clone, Copy)]
#[facet(is_numeric)]
#[repr(u8)]
pub enum ChannelTypes {
    GuildText,
    DM,
    GuildVoice,
    GroupDM,
    GuildCategory,
    GuildAnnouncement,
    AnnouncementThread,
    PublicThread,
    PrivateThread,
    GuildStageVoice,
    GuildDirectory,
    GuildForum,
    GuildMedia,
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
    pub id: String,
    pub guild_id: Option<String>,
    #[facet(rename = "type")]
    pub channel_type: ChannelTypes,
    // flags: i32,
    pub position: Option<i32>,
    pub parent_id: Option<String>,
    pub icon: Option<String>,
    // pub(crate) last_message_id: Option<String>,
    pub name: Option<String>,
    pub recipients: Option<Vec<Recipient>>,
    pub permission_overwrites: Option<Vec<OverwriteObject>>,
}
impl Channel {
    // TODO: This method is deprecated - use to_room_data() instead
    // Kept for backward compatibility during migration
    pub async fn to_room(&self) -> (String, Option<std::path::PathBuf>, Room) {
        let (name, icon, room) = self.to_room_data().await;
        (name, icon, room)
    }

    /// Extract room data, name, and icon from a channel.
    /// Returns (name, icon, room_data).
    pub async fn to_room_data(&self) -> (String, Option<std::path::PathBuf>, Room) {
        let name = self.name.to_owned().unwrap_or_else(|| {
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

        let mut icon = None;

        // If channel has icon, use that
        if let Some(hash) = &self.icon {
            let downloaded_icon = cache_download(
                format!(
                    "https://cdn.discordapp.com/channel-icons/{}/{}.webp?size=80&quality=lossless",
                    self.id, hash
                ),
                format!("./cache/imgs/channels/discord/{}", self.id).into(),
                format!("{hash}.webp"),
            )
            .await;
            match downloaded_icon {
                Ok(path) => icon = Some(path),
                Err(e) => {
                    error!("Failed to download icon for channel: {}\n{}", name, e);
                }
            };
        } else if let Some(recipients) = self.recipients.as_ref() {
            // If any recipient has an avatar, use that as room icon
            for recipient in recipients {
                if let Some(hash) = &recipient.avatar {
                    let downloaded_icon = cache_download(
                        format!(
                            "https://cdn.discordapp.com/avatars/{}/{}.webp?size=80&quality=lossless",
                            recipient.id, hash
                        ),
                        format!("./cache/imgs/users/discord/{}", recipient.id).into(),
                        format!("{hash}.webp"),
                    )
                    .await;
                    match downloaded_icon {
                        Ok(path) => {
                            icon = Some(path);
                            break;
                        }
                        Err(e) => {
                            error!("Failed to download icon for channel: {}\n{}", name, e);
                        }
                    };
                }
            }
        }

        let room = Room::new(
            // NOTE: DMs can have voice calls; treat as both for now.
            RoomCapabilities::from(self.channel_type),
            Some(Vec::new()),
            None,
        );

        (name, icon, room)
    }
}

#[derive(Facet)]
pub struct CountDetails {
    // burst: u32,
    // normal: u32,
}

#[derive(Facet)]
pub struct Emoji {
    pub id: Option<String>,
    pub name: String,
}

#[derive(Facet)]
pub struct Reaction {
    // burst_colors: Vec<String>,
    // burst_count: u32,
    // burst_me: bool,
    pub count: u32,
    // count_details: CountDetails,
    pub emoji: Emoji,
    // me: bool,
    // me_burst: bool,
}

#[derive(Facet)]
pub struct Message {
    // attachments: Vec<String>,
    pub author: User,
    pub channel_id: String,
    // components: Vec<String>,
    pub content: String,
    // edited_timestamp: Option<String>,
    // embeds: Vec<u32>,
    // flags: u32,
    pub id: String,
    // mention_everyone: bool,
    // mention_roles: Vec<String>,
    // mentions: Vec<String>,
    // pinned: bool,
    pub reactions: Option<Vec<Reaction>>,
    // timestamp: String,
    // tts: bool,
    // type: u32,
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

// https://discord.com/developers/docs/resources/guild#guild-object
#[derive(Facet)]
pub struct Guild {
    pub id: String,
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
