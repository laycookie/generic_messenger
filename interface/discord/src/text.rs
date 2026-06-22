//! High-level text (messaging) layer.
//!
//! Implements the `Text` trait from `messenger_interface` on top of the raw
//! REST calls in `rest_api.rs`, reconciling REST results against gateway
//! events captured in a recording window (see
//! `crate/messenger_interface/docs/races.md`). The companion `ArcStream` impl
//! drains the buffered `TextEvent` queue, pumping the gateway when it's empty.
use std::{error::Error, sync::Arc};

use async_trait::async_trait;
use futures::future::join_all;
use messenger_interface::{
    interface::{Ordering, Text, TextEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{Identifier, Message, Place, Room, User},
};

use crate::{
    Discord, InnerDiscord, Owned, StreamPollGuard, TextDiscord, api_types, downloaders::CdnImage,
};

#[async_trait]
impl Text for InnerDiscord<Owned> {
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
        ordering: Ordering,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        let channel_location = *self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;

        let before = match load_messages_before {
            Some(msg) => Some(
                *self
                    .message_id_mappings
                    .get(msg.id())
                    .ok_or("No discord message id mapping for before-pagination")?,
            ),
            None => None,
        };

        // Open a recording window *before* firing REST so any
        // MessageUpdate / MessageDelete / Reaction events that arrive
        // during the fetch are captured for the post-response merge.
        // If the gateway is down, skip — REST is then the only source
        // of truth and there is nothing to merge against. See
        // `crate/messenger_interface/docs/races.md`.
        let gateway = self.gateway.load_full();
        let window = gateway.as_ref().map(|g| g.start_recording());

        let messages = self
            .rest_get_messages(channel_location.channel_id(), before)
            .await?;

        // Drain the recording window and fold captured deltas onto the
        // REST snapshot: deletes drop entries, updates replace them,
        // reaction events mutate the cached vec in place.
        let messages = match window {
            Some(w) => crate::gateways::general::recording::apply_to_messages(
                messages,
                w.take(),
                channel_location.channel_id(),
            ),
            None => messages,
        };

        // Discord returns messages newest-first. For `Ordering::Time` we
        // reverse so callers get oldest-first; `Unordered` keeps the
        // newest-first arrival order.
        let messages_iter: Box<dyn Iterator<Item = api_types::Message> + Send> = match ordering {
            Ordering::Time => Box::new(messages.into_iter().rev()),
            Ordering::Unordered => Box::new(messages.into_iter()),
        };
        let identifiers = join_all(messages_iter.map(async |message| {
            let (content, history) = message.revisions().await;
            let reactions = message.interface_reactions().await;

            let icon = match &message.author.avatar {
                Some(hash) => CdnImage::avatar(message.author.id, hash).fetch().await.ok(),
                None => None,
            };

            let author = messenger_interface::types::Identifier::new(
                message.author.id,
                messenger_interface::types::User {
                    name: message.author.username,
                    icon,
                },
            );
            let identifier = Discord::identifier_generator(
                message.id,
                Message {
                    content,
                    history,
                    reactions,
                    author: Some(author),
                },
            );

            (identifier, message.id)
        }))
        .await;

        Ok(identifiers
            .into_iter()
            .map(|(identifier, discord_msg_id)| {
                self.message_id_mappings
                    .insert(*identifier.id(), discord_msg_id);
                identifier
            })
            .collect())
    }

    async fn add_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let channel_location = *self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let msg_id = *self
            .message_id_mappings
            .get(message.id())
            .ok_or("No discord message id mapping")?;
        self.rest_add_reaction(channel_location.channel_id(), msg_id, emoji)
            .await
    }

    async fn remove_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let channel_location = *self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;
        let msg_id = *self
            .message_id_mappings
            .get(message.id())
            .ok_or("No discord message id mapping")?;
        self.rest_remove_reaction(channel_location.channel_id(), msg_id, emoji)
            .await
    }

    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
        let channel_location = *self
            .channel_id_mappings
            .get(location.id())
            .ok_or("No discord channel id mapping for this room")?;

        let msg = self
            .rest_send_message(
                channel_location.channel_id(),
                contents.content.text.to_plain(),
            )
            .await?;

        // Register the mapping immediately: reacting to (or paginating
        // before) a just-sent message must not depend on the gateway
        // MessageCreate echo arriving first.
        self.message_id_mappings.insert(msg.id, msg.id);

        let (content, history) = msg.revisions().await;
        let icon = match &msg.author.avatar {
            Some(hash) => CdnImage::avatar(msg.author.id, hash).fetch().await.ok(),
            None => None,
        };
        let author = Identifier::new(
            msg.author.id,
            User {
                name: msg.author.username,
                icon,
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
impl ArcStream for InnerDiscord<TextDiscord> {
    type Item = TextEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        let _guard = StreamPollGuard::new(&self.active_streams);
        loop {
            if self.killed.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            if self.owner_dropped() {
                self.kill();
                return None;
            }
            if let Some(event) = self.text_events.pop() {
                return Some(event);
            }
            self.poll_for_events().await?;
        }
    }
}
