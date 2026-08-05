//! Text chat: the guaranteed core ([`Text`]) plus the optional per-operation
//! capabilities ([`TextEdit`], [`TextDelete`], [`TextCancelSend`],
//! [`TextReactions`]), their casters, and the realtime [`TextEvent`]s.

use std::time::Duration;
use std::{error::Error, sync::Arc};

use async_trait::async_trait;

use super::MessengerError;
use super::caster::capability_caster;
use crate::stream::WeakSocketStream;
use crate::types::{Emoji, ID, Identifier, Message, Place, Room, Sticker};

capability_caster!(
    /// Capability: editing previously sent messages.
    TextCasterEdit: Text => TextEdit { edit, arc_edit }
    /// Capability: deleting sent messages.
    TextCasterDelete: Text => TextDelete { delete, arc_delete }
    /// Capability: retracting a message while it is still within its
    /// send window (e.g. email "undo send").
    TextCasterCancel: Text => TextCancelSend { cancel, arc_cancel }
    /// Capability: reacting to messages with emoji.
    TextCasterReactions: Text => TextReactions { reactions, arc_reactions }
);

/// Ordering requested by a caller of [`Text::get_messages`].
///
/// Adapters that do not care about the distinction may treat both variants
/// the same. The intent is to let callers that *do* care opt into a stronger
/// guarantee without forcing every adapter to sort eagerly when the cost is
/// non-trivial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordering {
    /// No ordering guarantee. The adapter returns messages in whatever order is
    /// cheapest (typically the order the backend returned them).
    Unordered,
    /// Messages are returned sorted oldest-first by their creation time.
    /// Adapters whose IDs encode a timestamp (e.g. Discord snowflakes) should
    /// sort by ID; others should sort by an equivalent timestamp field.
    Time,
}

/// Text chat API for reading/sending messages in a room/channel.
///
/// This is the guaranteed core every text-capable backend provides. Optional
/// mutations (editing, deleting, cancelling a send) live in their own
/// capability traits ([`TextEdit`], [`TextDelete`], [`TextCancelSend`]);
/// whether a backend supports them is a property of its *type*, surfaced to
/// the UI through the `TextCaster*` supertraits the same way
/// [`Audio`](super::Audio) support is surfaced through
/// [`MessengerCasterAudio`](super::MessengerCasterAudio).
#[async_trait]
pub trait Text:
    Send + Sync + TextCasterEdit + TextCasterDelete + TextCasterCancel + TextCasterReactions
{
    /// Load messages in `location`, optionally paginating older messages by passing
    /// `load_messages_before`. `ordering` selects how the returned vector is sorted.
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
        ordering: Ordering,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>>;

    /// The custom emoji the client may compose into messages in `location`
    /// (platform emoticons / server emoji), for an emoji-picker UI. Unicode
    /// emoji are universal and not included. Room-scoped because availability
    /// can depend on where the message goes (e.g. a Discord guild's emoji).
    async fn pickable_emojis(
        &self,
        _location: &Identifier<Place<Room>>,
    ) -> Result<Vec<Emoji>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// The stickers the client may send in `location`, for a sticker-picker
    /// UI. Each carries the platform `reference` needed to send it back
    /// through [`Text::send_message`] as a [`crate::types::Span::Sticker`].
    async fn pickable_stickers(
        &self,
        _location: &Identifier<Place<Room>>,
    ) -> Result<Vec<Sticker>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Send a new message into `location`.
    ///
    /// Returns the confirmed message (with server-assigned ID) on success.
    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>>;

    /// Subscribe to realtime message and reaction changes.
    ///
    /// Intended for one long-lived consumer per backend. The returned weak
    /// stream does not keep the backend alive. Events are deltas; callers still
    /// use [`Self::get_messages`] for initial state and history. The default
    /// reports [`MessengerError::NotImplemented`].
    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<TextEvent>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
}

/// Capability: edit previously sent messages.
///
/// Deliberately a separate trait rather than a defaulted [`Text`] method: a
/// backend that doesn't implement it has no edit path at all, and the UI
/// learns this from the type (via [`TextCasterEdit`]) instead of by calling
/// and failing. Receiving *others'* edits is unrelated — that stays on
/// [`Text::listen`] as [`TextEvent::MessageUpdated`].
#[async_trait]
pub trait TextEdit: Text {
    /// Edit a previously sent message, replacing its content.
    ///
    /// Returns the updated message as confirmed by the server.
    async fn edit_message(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>>;
}

/// Capability: delete sent messages, with no time limit beyond the
/// platform's own permissions. A retract-within-a-window platform (email
/// "undo send") is *not* this — that is [`TextCancelSend`].
#[async_trait]
pub trait TextDelete: Text {
    /// Delete a message.
    async fn delete_message(
        &self,
        location: &Identifier<Place<Room>>,
        message: &Identifier<Message>,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

/// Capability: react to messages with emoji.
///
/// `emoji` is the reaction's identity exactly as the unified message store
/// carries it (`Reaction.emoji.shortcode`): a literal Unicode emoji, or a
/// custom emoji's bare shortcode. Backends resolve their own platform
/// references from that (e.g. Discord looks the shortcode up in the guild's
/// emoji to build `name:id`). Receiving *others'* reactions is unrelated —
/// that stays on [`Text::listen`] as [`TextEvent::ReactionAdded`].
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

/// Capability: retract a message while it is still within its send window.
///
/// Distinct from [`TextDelete`]: the affordance ("Cancel send", shown only
/// briefly) and the semantics (the message may never have left the server)
/// differ, which is exactly what makes support decidable per backend type.
#[async_trait]
pub trait TextCancelSend: Text {
    /// How long after sending a message stays cancellable. Static per
    /// backend; per-message eligibility is `now - sent_at < cancel_window()`,
    /// computed client-side from timestamps the UI already has.
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

    /// Implements only the required [`Text`] core.
    struct CoreText;
    /// Implements the core plus the edit/delete capabilities.
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

    /// The `TextCaster*` specialization resolves per concrete type —
    /// implementors answer `Ok`, everyone else `NotImplemented` — and the
    /// resolution survives erasure to `dyn Text` / `Arc<dyn Text>`.
    #[test]
    fn text_casters_resolve_by_concrete_type() {
        let core: Arc<dyn Text> = Arc::new(CoreText);
        assert!(core.edit().is_err());
        assert!(core.delete().is_err());
        assert!(core.cancel().is_err());
        assert!(core.reactions().is_err());
        assert!(core.clone().arc_edit().is_err());
        assert!(core.clone().arc_delete().is_err());
        assert!(core.clone().arc_reactions().is_err());

        let mutable: Arc<dyn Text> = Arc::new(MutableText);
        assert!(mutable.edit().is_ok());
        assert!(mutable.delete().is_ok());
        assert!(mutable.cancel().is_err());
        assert!(mutable.reactions().is_ok());
        assert!(mutable.clone().arc_edit().is_ok());
        assert!(mutable.clone().arc_delete().is_ok());
        assert!(mutable.clone().arc_reactions().is_ok());
    }
}
