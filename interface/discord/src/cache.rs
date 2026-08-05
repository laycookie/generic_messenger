//! Backend-local Discord state populated by gateway dispatches and consumed by
//! adapter queries. HTTP transport stays cache-blind; orchestration code owns
//! any REST/cache reconciliation policy.

use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use std::sync::Arc;

use crate::{
    api::{self, SNOWFLAKE},
    gateways::general::VoiceRoster,
};

/// Minimal information needed to route interface room operations back to
/// Discord. Interface room IDs already are channel snowflakes, so retaining a
/// second full `api::Channel` here duplicated payloads and strings needlessly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChannelRoute {
    pub(crate) guild_id: Option<SNOWFLAKE>,
}

impl ChannelRoute {
    pub(crate) fn new(guild_id: Option<SNOWFLAKE>) -> Self {
        Self { guild_id }
    }
}

#[derive(Default)]
pub(crate) struct DiscordCache {
    pub(crate) profile: ArcSwapOption<api::Profile>,
    pub(crate) voice: VoiceRoster,
    pub(crate) relationships: ArcSwapOption<Box<[api::Friend]>>,
    pub(crate) dm_channels: ArcSwapOption<Box<[api::Channel]>>,
    pub(crate) guilds: ArcSwapOption<Box<[api::Guild]>>,
    pub(crate) channel_routes: DashMap<SNOWFLAKE, ChannelRoute>,
    pub(crate) guild_channels: DashMap<SNOWFLAKE, Vec<api::Channel>>,
    /// Gateway message-create overlay. Discord does not send channel updates
    /// for this field; keeping it separate makes the hot event path O(1)
    /// instead of cloning/scanning channel payload collections.
    pub(crate) last_message_ids: DashMap<SNOWFLAKE, SNOWFLAKE>,
    pub(crate) guild_emojis: DashMap<SNOWFLAKE, Vec<api::GuildEmoji>>,
    pub(crate) guild_stickers: DashMap<SNOWFLAKE, Vec<api::GuildSticker>>,
}

impl DiscordCache {
    /// Clear session-scoped state before rebuilding it from a fresh Ready.
    /// Gateway dispatch is the sole writer, so readers see either old or new
    /// immutable slices while each replacement is published atomically.
    pub(crate) fn clear_for_ready(&self) {
        self.profile.store(None);
        self.relationships.store(None);
        self.dm_channels.store(None);
        self.guilds.store(None);
        self.channel_routes.clear();
        self.guild_channels.clear();
        self.last_message_ids.clear();
        self.guild_emojis.clear();
        self.guild_stickers.clear();
        self.voice.clear();
    }

    pub(crate) fn replace_relationships(&self, relationships: Vec<api::Friend>) {
        self.relationships
            .store(Some(Arc::new(relationships.into_boxed_slice())));
    }

    pub(crate) fn upsert_relationship(&self, relationship: api::Friend) {
        let mut relationships = self
            .relationships
            .load_full()
            .map(|cached| cached.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        match relationships
            .iter_mut()
            .find(|cached| cached.id == relationship.id)
        {
            Some(cached) => *cached = relationship,
            None => relationships.push(relationship),
        }
        self.replace_relationships(relationships);
    }

    pub(crate) fn remove_relationship(&self, user_id: SNOWFLAKE) {
        let mut relationships = self
            .relationships
            .load_full()
            .map(|cached| cached.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        relationships.retain(|relationship| relationship.id != user_id);
        self.replace_relationships(relationships);
    }

    pub(crate) fn replace_dm_channels(&self, channels: Vec<api::Channel>) {
        self.channel_routes
            .retain(|_, route| route.guild_id.is_some());
        for channel in &channels {
            self.channel_routes
                .insert(channel.id, ChannelRoute::new(None));
        }
        self.dm_channels
            .store(Some(Arc::new(channels.into_boxed_slice())));
    }

    pub(crate) fn replace_guilds(&self, guilds: Vec<api::Guild>) {
        self.guilds.store(Some(Arc::new(guilds.into_boxed_slice())));
    }

    pub(crate) fn upsert_guild(&self, guild: api::Guild) {
        let mut guilds = self
            .guilds
            .load_full()
            .map(|cached| cached.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        match guilds.iter_mut().find(|cached| cached.id == guild.id) {
            Some(cached) => *cached = guild,
            None => guilds.push(guild),
        }
        self.replace_guilds(guilds);
    }

    pub(crate) fn remove_guild(&self, guild_id: SNOWFLAKE) {
        let mut guilds = self
            .guilds
            .load_full()
            .map(|cached| cached.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        guilds.retain(|guild| guild.id != guild_id);
        self.replace_guilds(guilds);

        if let Some((_, channels)) = self.guild_channels.remove(&guild_id) {
            for channel in channels {
                self.last_message_ids.remove(&channel.id);
            }
        }
        self.guild_emojis.remove(&guild_id);
        self.guild_stickers.remove(&guild_id);
        self.channel_routes
            .retain(|_, route| route.guild_id != Some(guild_id));
        self.voice.remove_guild(guild_id);
    }

    pub(crate) fn replace_guild_channels(&self, guild_id: SNOWFLAKE, channels: Vec<api::Channel>) {
        self.channel_routes
            .retain(|_, route| route.guild_id != Some(guild_id));
        for channel in &channels {
            self.channel_routes
                .insert(channel.id, ChannelRoute::new(Some(guild_id)));
        }
        self.guild_channels.insert(guild_id, channels);
    }

    pub(crate) fn upsert_channel(&self, channel: api::Channel) {
        let channel_id = channel.id;
        let guild_id = channel.guild_id;
        self.channel_routes
            .insert(channel_id, ChannelRoute::new(guild_id));

        if let Some(guild_id) = guild_id {
            let mut channels = self.guild_channels.entry(guild_id).or_default();
            match channels.iter_mut().find(|cached| cached.id == channel_id) {
                Some(cached) => *cached = channel,
                None => channels.push(channel),
            }
            return;
        }

        let mut channels = self
            .dm_channels
            .load_full()
            .map(|cached| cached.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        match channels.iter_mut().find(|cached| cached.id == channel_id) {
            Some(cached) => *cached = channel,
            None => channels.push(channel),
        }
        self.replace_dm_channels(channels);
    }

    pub(crate) fn remove_channel(&self, channel_id: SNOWFLAKE, guild_id: Option<SNOWFLAKE>) {
        let route = self
            .channel_routes
            .remove(&channel_id)
            .map(|(_, route)| route);
        self.voice.remove_channel(channel_id);
        self.last_message_ids.remove(&channel_id);
        match guild_id.or_else(|| route.and_then(|route| route.guild_id)) {
            Some(guild_id) => {
                if let Some(mut channels) = self.guild_channels.get_mut(&guild_id) {
                    channels.retain(|channel| channel.id != channel_id);
                }
            }
            None => {
                let mut channels = self
                    .dm_channels
                    .load_full()
                    .map(|cached| cached.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                channels.retain(|channel| channel.id != channel_id);
                self.replace_dm_channels(channels);
            }
        }
    }

    /// `CHANNEL_UPDATE` is not sent when Discord advances
    /// `last_message_id`, so Message Create must keep cached room ordering
    /// metadata current itself.
    pub(crate) fn note_message_created(&self, channel_id: SNOWFLAKE, message_id: SNOWFLAKE) {
        self.last_message_ids.insert(channel_id, message_id);
    }

    pub(crate) fn last_message_id(&self, channel: &api::Channel) -> Option<SNOWFLAKE> {
        self.last_message_ids
            .get(&channel.id)
            .map(|message_id| *message_id)
            .or(channel.last_message_id)
    }

    pub(crate) fn channel_route(&self, channel_id: SNOWFLAKE) -> Option<ChannelRoute> {
        self.channel_routes.get(&channel_id).map(|route| *route)
    }

    pub(crate) fn reaction_api_emoji(&self, guild_id: Option<SNOWFLAKE>, emoji: &str) -> String {
        guild_id
            .and_then(|guild_id| {
                let guild_emojis = self.guild_emojis.get(&guild_id)?;
                let custom = guild_emojis
                    .iter()
                    .find(|guild_emoji| guild_emoji.name.as_deref() == Some(emoji))?;
                Some(format!("{emoji}:{}", custom.id))
            })
            .unwrap_or_else(|| emoji.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: SNOWFLAKE, guild_id: Option<SNOWFLAKE>, name: &str) -> api::Channel {
        api::Channel {
            id,
            guild_id,
            channel_type: api::ChannelTypes::GuildText,
            position: None,
            parent_id: None,
            icon: None,
            last_message_id: None,
            name: Some(name.to_owned()),
            recipients: None,
            permission_overwrites: None,
        }
    }

    #[test]
    fn route_index_is_smaller_than_wire_channel() {
        assert!(std::mem::size_of::<ChannelRoute>() < std::mem::size_of::<api::Channel>());
    }

    #[test]
    fn channel_updates_replace_without_duplicates() {
        let cache = DiscordCache::default();
        cache.upsert_channel(channel(7, Some(1), "before"));
        cache.upsert_channel(channel(7, Some(1), "after"));

        let channels = cache.guild_channels.get(&1).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].name.as_deref(), Some("after"));
        assert_eq!(cache.channel_route(7).unwrap().guild_id, Some(1));
    }

    #[test]
    fn guild_removal_clears_dependent_indexes() {
        let cache = DiscordCache::default();
        cache.replace_guild_channels(1, vec![channel(7, Some(1), "room")]);
        cache.guild_emojis.insert(1, Vec::new());
        cache.guild_stickers.insert(1, Vec::new());

        cache.remove_guild(1);

        assert!(cache.guild_channels.get(&1).is_none());
        assert!(cache.guild_emojis.get(&1).is_none());
        assert!(cache.guild_stickers.get(&1).is_none());
        assert!(cache.channel_route(7).is_none());
    }

    #[test]
    fn dm_removal_does_not_touch_guild_channels() {
        let cache = DiscordCache::default();
        cache.replace_dm_channels(vec![channel(7, None, "dm")]);
        cache.replace_guild_channels(1, vec![channel(8, Some(1), "guild")]);

        cache.remove_channel(7, None);

        assert!(cache.dm_channels.load_full().unwrap().is_empty());
        assert_eq!(cache.guild_channels.get(&1).unwrap().len(), 1);
    }
}
