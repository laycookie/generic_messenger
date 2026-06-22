//! Messages and their parts: [`Reaction`]s and the [`Revision`] history.

use std::mem;

use chrono::{DateTime, Utc};

use super::identifier::Identifier;
use super::rich_text::{Emoji, RichText};
use super::user::User;

/// Represents a reaction (emoji) on a message.
#[derive(Debug, Clone)]
pub struct Reaction {
    /// The emoji this reaction was made with (Unicode or a custom emoji).
    pub emoji: Emoji,
    /// Number of users who added this reaction.
    pub count: u32,
    /// Whether the current (client) user has reacted with this emoji.
    pub reacted: bool,
}

/// A single version of a message's content.
///
/// The timestamp is optional so that backends without a sense of time
/// (e.g. a shell-style messenger) can still construct messages.
#[derive(Debug, Clone, Default)]
pub struct Revision {
    /// When this version became the message's content.
    pub at: Option<DateTime<Utc>>,
    /// The content as a sequence of rich-text spans.
    pub text: RichText,
}

/// Represents a message in a chat room/channel.
///
/// `content` is the live revision; `history` holds the *previous*
/// revisions (oldest first) and is empty when the message has never been
/// edited.
#[derive(Debug, Clone, Default)]
pub struct Message {
    /// The current text content of the message.
    pub content: Revision,
    /// Previous revisions of the message text, oldest first.
    /// Empty when the message has never been edited.
    pub history: Vec<Revision>,
    /// List of reactions on this message.
    pub reactions: Vec<Reaction>,
    /// The author of this message, if known.
    pub author: Option<Identifier<User>>,
}

impl Message {
    pub fn is_edited(&self) -> bool {
        !self.history.is_empty()
    }

    /// Move `content` into `history` (becoming the most recent past
    /// revision) and replace `content` with `new`. Backends call this
    /// when an edit event arrives.
    pub fn edit(&mut self, new: Revision) {
        let prev = mem::replace(&mut self.content, new);
        self.history.push(prev);
    }
}
