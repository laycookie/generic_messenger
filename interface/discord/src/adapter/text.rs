//! Discord REST text adapter with gateway-race reconciliation.
//!
//! History fetches record concurrent gateway deltas and fold them onto REST.
//! First merged snapshot seeds cache; later fetches do not replace it. See
//! `crate/messenger_interface/docs/races.md`.
pub(crate) mod rich;

use std::{error::Error, pin::pin};

use async_trait::async_trait;
use futures::{FutureExt as _, StreamExt as _, future::Either, stream};
use messenger_interface::{
    interface::{Ordering, Text, TextDelete, TextEdit, TextEvent, TextReactions},
    owner::Retained,
    stream::EventFuture,
    types::{Emoji, Identifier, Message, Place, Room, Sticker, User},
};

use crate::{InnerDiscord, api, downloaders::CdnImage};

const ASSET_FETCH_CONCURRENCY: usize = 16;

#[async_trait]
impl Text for InnerDiscord {
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
        ordering: Ordering,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        let runtime = self.runtime();
        runtime
            .cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();

        let before = load_messages_before.map(|msg| *msg.id());

        let gateway = runtime.gateway.current.load_full();
        let window = gateway.as_ref().map(|g| g.recording.start());

        let messages = self.api.rest_get_messages(channel_id, before).await?;

        let messages = match window {
            Some(w) => w.apply_to_messages(messages, channel_id, before.is_none()),
            None => messages,
        };

        runtime
            .cache
            .populate_messages_if_absent(channel_id, &messages);

        // Discord returns newest-first; `Time` requests reversal.
        let messages_iter: Box<dyn Iterator<Item = api::Message> + Send> = match ordering {
            Ordering::Time => Box::new(messages.into_iter().rev()),
            Ordering::Unordered => Box::new(messages.into_iter()),
        };
        let identifiers = stream::iter(messages_iter.map(async |message| {
            let avatar = async {
                match &message.author.avatar {
                    Some(hash) => CdnImage::avatar(message.author.id, hash).fetch().await.ok(),
                    None => None,
                }
            };
            let ((content, history), reactions, icon) =
                futures::join!(message.revisions(), message.interface_reactions(), avatar);

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
            Identifier::new(
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

    /// Current guild custom emoji; DMs return none.
    async fn pickable_emojis(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<Vec<Emoji>, Box<dyn Error + Sync + Send>> {
        let cache = &self.runtime().cache;
        let route = cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let Some(guild_id) = route.guild_id else {
            return Ok(Vec::new());
        };
        let emojis = cache
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

    /// Current guild static-image stickers; excludes Lottie.
    async fn pickable_stickers(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<Vec<Sticker>, Box<dyn Error + Sync + Send>> {
        let cache = &self.runtime().cache;
        let route = cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let Some(guild_id) = route.guild_id else {
            return Ok(Vec::new());
        };
        let stickers = cache
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
        self.runtime()
            .cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();

        let (content, sticker_ids) = rich::serialize_content(&contents.content.text);
        let msg = self
            .api
            .rest_send_message(channel_id, content, sticker_ids)
            .await?;

        // Interface message IDs are raw Discord snowflakes.
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

    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.ensure_gateway().await
    }

    fn next_event(self: Retained<Self>) -> EventFuture<TextEvent> {
        Box::pin(async move {
            let mut retained = self;
            loop {
                if !retained.is_active() {
                    return None;
                }
                if retained
                    .get()
                    .runtime()
                    .gateway
                    .current
                    .load_full()
                    .is_none()
                {
                    retained.get().retry_gateway_after_loss().await;
                    continue;
                }
                let selected = {
                    let runtime = retained.get().runtime();
                    let Some(gateway) = runtime.gateway.current.load_full() else {
                        continue;
                    };
                    let mut event = pin!(gateway.events.next_text().fuse());
                    let mut turn = pin!(runtime.wait_for_gateway_turn().fuse());
                    futures::select! {
                        event = event => Either::Left(event),
                        turn = turn => Either::Right(turn),
                    }
                };
                match selected {
                    Either::Left(event) => return event,
                    Either::Right(Some(turn)) => {
                        retained = InnerDiscord::finish_gateway_turn(retained, turn).await?;
                    }
                    Either::Right(None) => continue,
                }
            }
        })
    }
}

#[async_trait]
impl TextEdit for InnerDiscord {
    async fn edit_message(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
        self.runtime()
            .cache
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

        let avatar = async {
            match &msg.author.avatar {
                Some(hash) => CdnImage::avatar(msg.author.id, hash).fetch().await.ok(),
                None => None,
            }
        };
        let ((content, history), reactions, icon) =
            futures::join!(msg.revisions(), msg.interface_reactions(), avatar);
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
impl TextDelete for InnerDiscord {
    async fn delete_message(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.runtime()
            .cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();
        let msg_id = *message.id();
        self.api.rest_delete_message(channel_id, msg_id).await
    }
}

#[async_trait]
impl TextReactions for InnerDiscord {
    async fn add_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let cache = &self.runtime().cache;
        let route = cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();
        let guild_id = route.guild_id;
        let msg_id = *message.id();
        let emoji = cache.reaction_api_emoji(guild_id, emoji);
        self.api.rest_add_reaction(channel_id, msg_id, &emoji).await
    }

    async fn remove_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let cache = &self.runtime().cache;
        let route = cache
            .channel_route(*location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let channel_id = *location.id();
        let guild_id = route.guild_id;
        let msg_id = *message.id();
        let emoji = cache.reaction_api_emoji(guild_id, emoji);
        self.api
            .rest_remove_reaction(channel_id, msg_id, &emoji)
            .await
    }
}
