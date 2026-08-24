//! Text chat core, optional operation capabilities, and realtime events.

use std::error::Error;
use std::time::Duration;

use async_trait::async_trait;

use super::MessengerError;
use super::caster::capability_caster;
use crate::types::{Emoji, ID, Identifier, Message, Place, Room, Sticker};

capability_caster!(
    /// Capability: editing previously sent messages.
    TextCasterEdit: Text => TextEdit { edit }
    /// Capability: deleting sent messages.
    TextCasterDelete: Text => TextDelete { delete }
    /// Capability: retracting a message within its send window.
    TextCasterCancel: Text => TextCancelSend { cancel }
    /// Capability: reacting to messages with emoji.
    TextCasterReactions: Text => TextReactions { reactions }
);

/// Ordering requested from [`Text::get_messages`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordering {
    /// Cheapest backend order; no guarantee.
    Unordered,
    /// Oldest-first by creation time or equivalent ordered ID.
    Time,
}

/// Required text history and send API.
/// Optional mutations use separate, specialization-backed capability traits.
#[async_trait]
pub trait Text:
    Send + Sync + TextCasterEdit + TextCasterDelete + TextCasterCancel + TextCasterReactions
{
    /// Loads messages, optionally before cursor and with requested ordering.
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
        ordering: Ordering,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>>;

    /// Returns room-available custom emoji; excludes Unicode emoji.
    async fn pickable_emojis(
        &self,
        _location: &Identifier<Place<Room>>,
    ) -> Result<Vec<Emoji>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Returns sendable stickers with backend references.
    async fn pickable_stickers(
        &self,
        _location: &Identifier<Place<Room>>,
    ) -> Result<Vec<Sticker>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Sends and returns server-confirmed message.
    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>>;

    /// Prepare this backend's text event source.
    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Polls next prepared event.
    fn next_event(self: crate::owner::Retained<Self>) -> crate::stream::EventFuture<TextEvent> {
        Box::pin(async { None })
    }
}

/// Capability for editing sent messages.
/// Incoming edits remain [`TextEvent::MessageUpdated`].
#[async_trait]
pub trait TextEdit: Text {
    /// Replaces content and returns server-confirmed message.
    async fn edit_message(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>>;
}

/// Capability for normal deletion; timed retraction uses [`TextCancelSend`].
#[async_trait]
pub trait TextDelete: Text {
    /// Delete a message.
    async fn delete_message(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

/// Capability for emoji reactions.
/// `emoji` is Unicode or bare custom shortcode; backend resolves wire identity.
#[async_trait]
pub trait TextReactions: Text {
    /// Add the client's reaction to a message.
    async fn add_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;

    /// Remove the client's own reaction from a message.
    async fn remove_reaction(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

/// Capability for timed retraction before send becomes final.
#[async_trait]
pub trait TextCancelSend: Text {
    /// Static duration in which sent messages remain cancellable.
    fn cancel_window(&self) -> Duration;
    /// Retract a message still inside [`cancel_window`](TextCancelSend::cancel_window).
    async fn cancel_send(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

pub enum TextEvent {
    /// A message was created in a channel.
    MessageCreated {
        room: Identifier<()>,
        message: Identifier<Message>,
    },
    /// A message was edited/updated in a channel.
    MessageUpdated {
        room: Identifier<()>,
        message: Identifier<Message>,
    },
    /// A message was deleted in a channel.
    MessageDeleted {
        room: Identifier<()>,
        message_id: ID,
    },
    /// A reaction was added to a message.
    ReactionAdded {
        room: Identifier<()>,
        message_id: ID,
        user_id: ID,
        emoji: String,
    },
    /// A reaction was removed from a message.
    ReactionRemoved {
        room: Identifier<()>,
        message_id: ID,
        user_id: ID,
        emoji: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CoreText;
    struct MutableText;

    macro_rules! impl_text_core {
        ($ty:ty) => {
            #[async_trait]
            impl Text for $ty {
                async fn get_messages(
                    &self,
                    _: &Identifier<Place<Room>>,
                    _: Option<Identifier<Message>>,
                    _: Ordering,
                ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
                    unimplemented!()
                }
                async fn send_message(
                    &self,
                    _: &Identifier<Place<Room>>,
                    _: Message,
                ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
                    unimplemented!()
                }
            }
        };
    }
    impl_text_core!(CoreText);
    impl_text_core!(MutableText);

    #[async_trait]
    impl TextEdit for MutableText {
        async fn edit_message(
            &self,
            _: &Identifier<Place<Room>>,
            _: &Identifier<Message>,
            _: Message,
        ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
            unimplemented!()
        }
    }
    #[async_trait]
    impl TextDelete for MutableText {
        async fn delete_message(
            &self,
            _: &Identifier<Place<Room>>,
            _: &Identifier<Message>,
        ) -> Result<(), Box<dyn Error + Sync + Send>> {
            unimplemented!()
        }
    }
    #[async_trait]
    impl TextReactions for MutableText {
        async fn add_reaction(
            &self,
            _: &Identifier<Place<Room>>,
            _: &Identifier<Message>,
            _: &str,
        ) -> Result<(), Box<dyn Error + Sync + Send>> {
            unimplemented!()
        }
        async fn remove_reaction(
            &self,
            _: &Identifier<Place<Room>>,
            _: &Identifier<Message>,
            _: &str,
        ) -> Result<(), Box<dyn Error + Sync + Send>> {
            unimplemented!()
        }
    }

    #[test]
    fn text_casters_resolve_by_concrete_type() {
        let core: &dyn Text = &CoreText;
        assert!(core.edit().is_err());
        assert!(core.delete().is_err());
        assert!(core.cancel().is_err());
        assert!(core.reactions().is_err());

        let mutable: &dyn Text = &MutableText;
        assert!(mutable.edit().is_ok());
        assert!(mutable.delete().is_ok());
        assert!(mutable.cancel().is_err());
        assert!(mutable.reactions().is_ok());
    }
}
