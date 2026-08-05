//! High-level text (messaging) adapter.
//!
//! Implements the `Text` trait from `messenger_interface` on top of the raw
//! REST calls in `rest_api.rs`, reconciling REST results against gateway
//! events captured in a recording window (see
//! `crate/messenger_interface/docs/races.md`). The companion `ArcStream` impl
//! drains the buffered `TextEvent` queue, pumping the gateway when it's empty.
pub(crate) mod rich;

use std::{error::Error, pin::pin, sync::Arc};

use async_trait::async_trait;
use futures::{FutureExt as _, StreamExt as _, stream};
use messenger_interface::{
    interface::{Ordering, Text, TextDelete, TextEdit, TextEvent, TextReactions},
    stream::{ArcStream, WeakSocketStream},
    types::{Emoji, Identifier, Message, Place, Room, Sticker, User},
};

use crate::{
    Discord, InnerDiscord, Owned, api, downloaders::CdnImage, stream_poll::StreamPollGuard,
};

#[unitstruct::unit_struct]
pub(crate) struct TextDiscord;

const ASSET_FETCH_CONCURRENCY: usize = 16;

#[async_trait]
impl Text for InnerDiscord<Owned> {
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
        ordering: Ordering,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        self.cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();

        let before = load_messages_before.map(|msg| *msg.id());

        // Open a recording window *before* firing REST so any
        // MessageUpdate / MessageDelete / Reaction events that arrive
        // during the fetch are captured for the post-response merge.
        // If the gateway is down, skip — REST is then the only source
        // of truth and there is nothing to merge against. See
        // `crate/messenger_interface/docs/races.md`.
        let gateway = self.gateway.current.load_full();
        let window = gateway.as_ref().map(|g| g.recording.start());

        let messages = self.api.rest_get_messages(channel_id, before).await?;

        // Close the recording window and fold captured deltas onto the REST
        // snapshot: deletes drop entries, updates replace them, and reaction
        // events mutate the cached vec in place.
        let messages = match window {
            Some(w) => w.apply_to_messages(messages, channel_id, before.is_none()),
            None => messages,
        };

        // Discord returns messages newest-first. For `Ordering::Time` we
        // reverse so callers get oldest-first; `Unordered` keeps the
        // newest-first arrival order.
        let messages_iter: Box<dyn Iterator<Item = api::Message> + Send> = match ordering {
            Ordering::Time => Box::new(messages.into_iter().rev()),
            Ordering::Unordered => Box::new(messages.into_iter()),
        };
        let identifiers = stream::iter(messages_iter.map(async |message| {
            let (content, history) = message.revisions().await;
            let reactions = message.interface_reactions().await;

            let icon = match &message.author.avatar {
                Some(hash) => CdnImage::avatar(message.author.id, hash).fetch().await.ok(),
                None => None,
            };

            let author = messenger_interface::types::Identifier::new(
                message.author.id,
                messenger_interface::types::User {
                    name: message
                        .author
                        .global_name
                        .clone()
                        .unwrap_or_else(|| message.author.username.clone()),
                    tag: Some(message.author.username),
                    icon,
                    status: None,
                    voice: None,
                },
            );
            Discord::identifier_generator(
                message.id,
                Message {
                    content,
                    history,
                    reactions,
                    author: Some(author),
                },
            )
        }))
        .buffered(ASSET_FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

        Ok(identifiers)
    }

    /// The current guild's custom emoji (from the `Ready` snapshot). A DM has
    /// none — Unicode emoji, which the UI supplies itself, are all there is.
    async fn pickable_emojis(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<Vec<Emoji>, Box<dyn Error + Sync + Send>> {
        let route = self
            .cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let Some(guild_id) = route.guild_id else {
            return Ok(Vec::new());
        };
        let emojis = self
            .cache
            .guild_emojis
            .get(&guild_id)
            .map(|entry| entry.clone())
            .unwrap_or_default();

        Ok(stream::iter(emojis.into_iter().filter_map(|emoji| {
            if !emoji.available.unwrap_or(true) {
                return None;
            }
            let name = emoji.name?;
            let animated = emoji.animated.unwrap_or(false);
            Some(async move {
                Emoji {
                    reference: Some(rich::emoji_wire_tag(&name, emoji.id, animated)),
                    image: rich::resolve_emoji(emoji.id, animated).await,
                    shortcode: name,
                }
            })
        }))
        .buffered(ASSET_FETCH_CONCURRENCY)
        .collect()
        .await)
    }

    /// The current guild's stickers (from the `Ready` snapshot). Lottie
    /// stickers are omitted — there is no static image to show or render.
    async fn pickable_stickers(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<Vec<Sticker>, Box<dyn Error + Sync + Send>> {
        let route = self
            .cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let Some(guild_id) = route.guild_id else {
            return Ok(Vec::new());
        };
        let stickers = self
            .cache
            .guild_stickers
            .get(&guild_id)
            .map(|entry| entry.clone())
            .unwrap_or_default();

        Ok(stream::iter(stickers.into_iter().filter_map(|sticker| {
            if !sticker.available.unwrap_or(true) {
                return None;
            }
            let image = CdnImage::sticker(sticker.id, sticker.format_type)?;
            Some(async move {
                Sticker {
                    alt: sticker.name,
                    image: image.fetch().await.ok(),
                    reference: sticker.id.to_string(),
                }
            })
        }))
        .buffered(ASSET_FETCH_CONCURRENCY)
        .collect()
        .await)
    }

    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
        self.cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();

        let (content, sticker_ids) = rich::serialize_content(&contents.content.text);
        let msg = self
            .api
            .rest_send_message(channel_id, content, sticker_ids)
            .await?;

        // Interface message IDs are Discord snowflakes directly, so no
        // response-to-cache mapping or gateway echo is needed here.
        let (content, history) = msg.revisions().await;
        let icon = match &msg.author.avatar {
            Some(hash) => CdnImage::avatar(msg.author.id, hash).fetch().await.ok(),
            None => None,
        };
        let author = Identifier::new(
            msg.author.id,
            User {
                name: msg
                    .author
                    .global_name
                    .clone()
                    .unwrap_or_else(|| msg.author.username.clone()),
                tag: Some(msg.author.username),
                icon,
                status: None,
                voice: None,
            },
        );

        Ok(Identifier::new(
            msg.id,
            Message {
                content,
                history,
                reactions: Vec::new(),
                author: Some(author),
            },
        ))
    }
    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<TextEvent>, Box<dyn Error + Sync + Send>> {
        self.listen_as::<TextDiscord, _>().await
    }
}

#[async_trait]
impl TextEdit for InnerDiscord<Owned> {
    async fn edit_message(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
        self.cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();
        let msg_id = *message.id();

        // Stickers can't be edited into a message; only the content updates.
        let (content, _sticker_ids) = rich::serialize_content(&contents.content.text);
        let msg = self
            .api
            .rest_edit_message(channel_id, msg_id, content)
            .await?;

        let (content, history) = msg.revisions().await;
        let reactions = msg.interface_reactions().await;
        let icon = match &msg.author.avatar {
            Some(hash) => CdnImage::avatar(msg.author.id, hash).fetch().await.ok(),
            None => None,
        };
        let author = Identifier::new(
            msg.author.id,
            User {
                name: msg
                    .author
                    .global_name
                    .clone()
                    .unwrap_or_else(|| msg.author.username.clone()),
                tag: Some(msg.author.username),
                icon,
                status: None,
                voice: None,
            },
        );

        Ok(Identifier::new(
            msg.id,
            Message {
                content,
                history,
                reactions,
                author: Some(author),
            },
        ))
    }
}

#[async_trait]
impl TextDelete for InnerDiscord<Owned> {
    async fn delete_message(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();
        let msg_id = *message.id();
        self.api.rest_delete_message(channel_id, msg_id).await
    }
}

#[async_trait]
impl TextReactions for InnerDiscord<Owned> {
    async fn add_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let route = self
            .cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();
        let guild_id = route.guild_id;
        let msg_id = *message.id();
        let emoji = self.cache.reaction_api_emoji(guild_id, emoji);
        self.api.rest_add_reaction(channel_id, msg_id, &emoji).await
    }

    async fn remove_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let route = self
            .cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();
        let guild_id = route.guild_id;
        let msg_id = *message.id();
        let emoji = self.cache.reaction_api_emoji(guild_id, emoji);
        self.api
            .rest_remove_reaction(channel_id, msg_id, &emoji)
            .await
    }
}

#[async_trait]
impl ArcStream for InnerDiscord<TextDiscord> {
    type Item = TextEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        let _guard = StreamPollGuard::new(&self.gateway.active_streams);
        loop {
            if self
                .gateway
                .killed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return None;
            }
            if self.owner_dropped() {
                self.gateway.kill();
                return None;
            }
            let gateway = self.gateway.current.load_full()?;
            let mut event = pin!(gateway.events.next_text().fuse());
            let mut turn = pin!(self.wait_for_gateway_turn().fuse());
            futures::select! {
                event = event => return Some(event),
                turn = turn => {
                    let turn = turn?;
                    self.execute_gateway_turn(turn).await?;
                },
            }
        }
    }
}
