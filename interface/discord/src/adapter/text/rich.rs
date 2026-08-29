//! Parse Discord message content into the interface [`RichText`] model.
//!
//! Discord sends content as a flat string with markdown (`**bold**`,
//! `*italic*`/`_italic_`), bare URLs, and custom-emoji tokens
//! (`<:name:id>` / animated `<a:name:id>`); stickers ride alongside as a
//! separate `sticker_items` array. We tokenize all of that here — *in the
//! adapter* — so the UI renders one uniform span model and never has to know
//! Discord's syntax. Image resolution (emoji/sticker → cached file) happens in
//! the async pass; the tokenizer itself is pure and unit-tested.
//!
//! <https://docs.discord.food/reference#message-formatting>

use std::path::PathBuf;

use futures::{StreamExt, stream};
use messenger_interface::types::{Emoji, RichText, Span, Sticker, TextStyle};

const MAX_ASSET_FETCH_CONCURRENCY: usize = 8;

use crate::{
    api::{self, SNOWFLAKE},
    downloaders::CdnImage,
};

/// A token produced by [`tokenize`], before image resolution. Custom emoji
/// carry their id so the CDN image can be fetched in the async pass.
#[derive(Debug, PartialEq)]
enum Piece {
    Text {
        text: String,
        style: TextStyle,
    },
    Emoji {
        name: String,
        id: SNOWFLAKE,
        animated: bool,
    },
    Link {
        text: String,
        href: String,
    },
}

fn bytes_ci_prefix(s: &str, prefix: &str) -> bool {
    let (sb, pb) = (s.as_bytes(), prefix.as_bytes());
    sb.len() >= pb.len() && sb[..pb.len()].eq_ignore_ascii_case(pb)
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
}

/// `<:name:id>` or `<a:name:id>`. Returns (bytes consumed, piece).
fn match_emoji(rest: &str) -> Option<(usize, Piece)> {
    let (animated, after_prefix) = if let Some(r) = rest.strip_prefix("<a:") {
        (true, r)
    } else if let Some(r) = rest.strip_prefix("<:") {
        (false, r)
    } else {
        return None;
    };
    let colon = after_prefix.find(':')?;
    let name = &after_prefix[..colon];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let after_name = &after_prefix[colon + 1..];
    let gt = after_name.find('>')?;
    let id_str = &after_name[..gt];
    if id_str.is_empty() || !id_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id: SNOWFLAKE = id_str.parse().ok()?;
    let consumed = rest.len() - (after_name.len() - (gt + 1));
    Some((
        consumed,
        Piece::Emoji {
            name: name.to_owned(),
            id,
            animated,
        },
    ))
}

/// `**bold**`. Empty `****` is treated as literal text (returns `None`).
fn match_bold(rest: &str) -> Option<(usize, Piece)> {
    let inner = rest.strip_prefix("**")?;
    let end = inner.find("**")?;
    if end == 0 {
        return None;
    }
    Some((
        2 + end + 2,
        Piece::Text {
            text: inner[..end].to_owned(),
            style: TextStyle {
                bold: true,
                ..Default::default()
            },
        },
    ))
}

/// `*italic*` or `_italic_`. A leading `**` is left for [`match_bold`].
fn match_italic(rest: &str) -> Option<(usize, Piece)> {
    let delim = match rest.as_bytes().first()? {
        b'*' if !rest.starts_with("**") => '*',
        b'_' => '_',
        _ => return None,
    };
    let inner = &rest[1..];
    let end = inner.find(delim)?;
    if end == 0 {
        return None;
    }
    Some((
        1 + end + 1,
        Piece::Text {
            text: inner[..end].to_owned(),
            style: TextStyle {
                italic: true,
                ..Default::default()
            },
        },
    ))
}

/// A bare `http(s)://…` URL.
fn match_url(rest: &str) -> Option<(usize, Piece)> {
    if !bytes_ci_prefix(rest, "http://") && !bytes_ci_prefix(rest, "https://") {
        return None;
    }
    let end = rest.find(|c: char| !is_url_char(c)).unwrap_or(rest.len());
    // Need at least one char past the shortest scheme, else it's not a URL.
    if end <= "http://".len() {
        return None;
    }
    let url = rest[..end].to_owned();
    Some((
        end,
        Piece::Link {
            text: url.clone(),
            href: url,
        },
    ))
}

fn match_special(rest: &str) -> Option<(usize, Piece)> {
    match_emoji(rest)
        .or_else(|| match_bold(rest))
        .or_else(|| match_italic(rest))
        .or_else(|| match_url(rest))
}

/// Split `content` into [`Piece`]s. Plain runs (including Unicode emoji, which
/// need no special handling) accumulate into `Text` pieces; everything between
/// is a recognized markdown/emoji/link token.
fn tokenize(content: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut rest = content;

    while !rest.is_empty() {
        if let Some((consumed, piece)) = match_special(rest) {
            if !text.is_empty() {
                pieces.push(Piece::Text {
                    text: std::mem::take(&mut text),
                    style: TextStyle::default(),
                });
            }
            pieces.push(piece);
            rest = &rest[consumed..];
        } else {
            let ch = rest.chars().next().expect("rest is non-empty");
            text.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text {
            text,
            style: TextStyle::default(),
        });
    }
    pieces
}

/// Download+cache a Discord custom-emoji image, returning the local path.
pub(crate) async fn resolve_emoji(id: SNOWFLAKE, animated: bool) -> Option<PathBuf> {
    CdnImage::emoji(id, animated).fetch().await.ok()
}

/// The wire tag that sends a custom emoji inside message content. Stored as
/// the [`Emoji::reference`] so outgoing serialization is a straight copy.
pub(crate) fn emoji_wire_tag(name: &str, id: SNOWFLAKE, animated: bool) -> String {
    if animated {
        format!("<a:{name}:{id}>")
    } else {
        format!("<:{name}:{id}>")
    }
}

/// Download+cache a Discord sticker image. Lottie (format 3) has no static
/// image, so it falls back to alt text (`None`).
async fn resolve_sticker(sticker: &api::StickerItem) -> Option<PathBuf> {
    CdnImage::sticker(sticker.id, sticker.format_type)?
        .fetch()
        .await
        .ok()
}

/// Build the [`RichText`] for a message body plus its stickers, resolving
/// emoji/sticker images into the cache as it goes.
pub async fn build_content(content: &str, stickers: &[api::StickerItem]) -> RichText {
    // Asset fetches are independent and may each wait on the network. Resolve a
    // bounded number concurrently so one emoji-heavy message cannot serialize
    // dozens of request timeouts on the gateway dispatcher.
    let content_spans = stream::iter(tokenize(content))
        .map(|piece| async move {
            match piece {
                Piece::Text { text, style } => Span::Text { text, style },
                Piece::Link { text, href } => Span::Link { text, href },
                Piece::Emoji { name, id, animated } => {
                    let image = resolve_emoji(id, animated).await;
                    let reference = Some(emoji_wire_tag(&name, id, animated));
                    Span::Emoji(Emoji {
                        shortcode: name,
                        image,
                        reference,
                    })
                }
            }
        })
        .buffered(MAX_ASSET_FETCH_CONCURRENCY)
        .collect::<Vec<_>>();

    let sticker_spans = stream::iter(stickers.iter().cloned())
        .map(|sticker| async move {
            let image = resolve_sticker(&sticker).await;
            Span::Sticker(Sticker {
                alt: sticker.name,
                image,
                reference: sticker.id.to_string(),
            })
        })
        .buffered(MAX_ASSET_FETCH_CONCURRENCY)
        .collect::<Vec<_>>();

    let (mut spans, sticker_spans) = futures::join!(content_spans, sticker_spans);
    spans.extend(sticker_spans);
    RichText { spans }
}

/// Serialize outgoing [`RichText`] into Discord wire form: the `content`
/// string plus the sticker ids that ride in the separate `sticker_ids`
/// field of the create-message payload. An emoji span's [`Emoji::reference`]
/// already is its wire tag; one without a reference (composed for another
/// platform) degrades to `:shortcode:` text. Links use their target so the
/// URL stays functional.
pub(crate) fn serialize_content(text: &RichText) -> (String, Vec<SNOWFLAKE>) {
    let mut content = String::new();
    let mut sticker_ids = Vec::new();
    for span in &text.spans {
        match span {
            Span::Text { text, .. } => content.push_str(text),
            Span::Link { href, .. } => content.push_str(href),
            Span::Emoji(emoji) => match &emoji.reference {
                Some(tag) => content.push_str(tag),
                None => {
                    content.push(':');
                    content.push_str(&emoji.shortcode);
                    content.push(':');
                }
            },
            Span::Sticker(sticker) => {
                // A sticker we can't reference (foreign platform or
                // unresolved incoming) has no sendable form; drop it rather
                // than leak its alt text into the message.
                if let Ok(id) = sticker.reference.parse::<SNOWFLAKE>() {
                    sticker_ids.push(id);
                }
            }
        }
    }
    (content, sticker_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(pieces: &[Piece]) -> Vec<String> {
        pieces.iter().map(|p| format!("{p:?}")).collect()
    }

    #[test]
    fn plain_text_is_one_piece() {
        assert_eq!(
            tokenize("hello world"),
            vec![Piece::Text {
                text: "hello world".to_owned(),
                style: TextStyle::default()
            }]
        );
    }

    #[test]
    fn bold_italic_and_url() {
        let pieces = tokenize("a **b** c *i* http://x.com/y end");
        // a , bold(b), " c ", italic(i), " ", link, " end"
        assert!(matches!(
            pieces[1],
            Piece::Text {
                style: TextStyle { bold: true, .. },
                ..
            }
        ));
        assert!(matches!(
            pieces[3],
            Piece::Text {
                style: TextStyle { italic: true, .. },
                ..
            }
        ));
        assert!(matches!(&pieces[5], Piece::Link { href, .. } if href == "http://x.com/y"));
    }

    #[test]
    fn custom_emoji_static_and_animated() {
        let pieces = tokenize("hi <:steamhappy:12345> and <a:wave:67890>!");
        assert!(
            matches!(&pieces[1], Piece::Emoji { name, id: 12345, animated: false } if name == "steamhappy")
        );
        assert!(
            matches!(&pieces[3], Piece::Emoji { name, id: 67890, animated: true } if name == "wave")
        );
    }

    #[test]
    fn malformed_emoji_stays_text() {
        // No id digits / no closing > → treated as literal text, not dropped.
        let pieces = tokenize("<:broken> <:nope:bad>");
        assert_eq!(texts(&pieces).len(), 1);
        assert!(matches!(&pieces[0], Piece::Text { text, .. } if text == "<:broken> <:nope:bad>"));
    }

    #[test]
    fn unicode_emoji_passes_through_as_text() {
        let pieces = tokenize("hi 😀");
        assert_eq!(
            pieces,
            vec![Piece::Text {
                text: "hi 😀".to_owned(),
                style: TextStyle::default()
            }]
        );
    }

    #[test]
    fn serialize_emits_emoji_wire_tags() {
        let rich = RichText {
            spans: vec![
                Span::Text {
                    text: "hi ".to_owned(),
                    style: TextStyle::default(),
                },
                Span::Emoji(Emoji {
                    shortcode: "wave".to_owned(),
                    image: None,
                    reference: Some(emoji_wire_tag("wave", 67890, true)),
                }),
            ],
        };
        let (content, sticker_ids) = serialize_content(&rich);
        assert_eq!(content, "hi <a:wave:67890>");
        assert!(sticker_ids.is_empty());
    }

    #[test]
    fn serialize_splits_stickers_out_of_content() {
        let rich = RichText {
            spans: vec![Span::Sticker(Sticker {
                alt: "cat".to_owned(),
                image: None,
                reference: "42".to_owned(),
            })],
        };
        let (content, sticker_ids) = serialize_content(&rich);
        assert!(content.is_empty());
        assert_eq!(sticker_ids, vec![42]);
    }

    #[test]
    fn serialize_referenceless_emoji_degrades_to_shortcode() {
        let rich = RichText {
            spans: vec![Span::Emoji(Emoji::shortcode("steamhappy"))],
        };
        let (content, _) = serialize_content(&rich);
        assert_eq!(content, ":steamhappy:");
    }
}
