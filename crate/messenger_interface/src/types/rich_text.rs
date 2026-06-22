//! The platform-agnostic inline content model: [`RichText`] and its [`Span`]s.
//!
//! Backends parse their own wire format (Discord markdown, Steam BBCode, ...)
//! into this uniform model so the UI never has to know a platform's syntax.

use std::path::PathBuf;

/// A custom emoji / emoticon: a small image identified by a shortcode.
///
/// `shortcode` is the platform name (e.g. `steamhappy`, a Discord emoji name,
/// or a Unicode emoji itself) and doubles as the textual fallback rendered as
/// `:shortcode:` when there is no image. `image` is a cached local path, or
/// `None` when the emoji has no image (Unicode) or it could not be resolved.
#[derive(Debug, Clone, Default)]
pub struct Emoji {
    pub shortcode: String,
    pub image: Option<PathBuf>,
}
impl Emoji {
    /// A shortcode-only emoji with no resolved image (Unicode, or pending).
    pub fn shortcode(shortcode: impl Into<String>) -> Self {
        Self {
            shortcode: shortcode.into(),
            image: None,
        }
    }
}

/// Inline text styling for a run of text. Extend as backends/UI grow
/// (strikethrough, code, color, ...).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
}

/// One inline piece of a message's content. A message's text is a flat
/// sequence of these (see [`RichText`]); backends parse their own wire format
/// (Discord markdown, Steam BBCode, ...) into spans so the UI renders one
/// uniform model and never has to know a platform's syntax.
#[derive(Debug, Clone)]
pub enum Span {
    /// A run of plain text with optional styling.
    Text { text: String, style: TextStyle },
    /// A custom emoji rendered inline (currently shown as its shortcode).
    Emoji(Emoji),
    /// A sticker: a large standalone image. `alt` is the textual fallback.
    Sticker { alt: String, image: Option<PathBuf> },
    /// A hyperlink: display text plus its target URL.
    Link { text: String, href: String },
}
impl Span {
    /// Plain-text projection, for flattening / search / logging. Emoji become
    /// `:shortcode:`, stickers their `alt`, links their display text.
    pub fn to_plain(&self) -> String {
        match self {
            Span::Text { text, .. } => text.clone(),
            Span::Emoji(emoji) => format!(":{}:", emoji.shortcode),
            Span::Sticker { alt, .. } => alt.clone(),
            Span::Link { text, .. } => text.clone(),
        }
    }
}

/// A message's rendered content: a flat sequence of inline [`Span`]s.
///
/// Backends build this from their wire format; the compose path uses
/// [`RichText::plain`] since the user types plain text.
#[derive(Debug, Clone, Default)]
pub struct RichText {
    pub spans: Vec<Span>,
}
impl RichText {
    /// A single unstyled text span (empty input yields no spans). Used by the
    /// compose path and by backends with no rich formatting.
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        if text.is_empty() {
            return Self::default();
        }
        Self {
            spans: vec![Span::Text {
                text,
                style: TextStyle::default(),
            }],
        }
    }

    /// Flatten to plain text (see [`Span::to_plain`]).
    pub fn to_plain(&self) -> String {
        self.spans.iter().map(Span::to_plain).collect()
    }

    /// True when there is no renderable content (no spans, or only empty text).
    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.to_plain().is_empty())
    }
}
impl std::fmt::Display for RichText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_plain())
    }
}
