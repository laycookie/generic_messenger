//! Parse Discord message content into the interface [`RichText`] model.
//!
//! Discord sends content as a flat string with markdown (`**bold**`,
//! `*italic*`/`_italic_`), bare URLs, and custom-emoji tokens
//! (`<:name:id>` / animated `<a:name:id>`); stickers ride alongside as a
//! separate `sticker_items` array. We tokenize all of that here — *in the
//! adapter* — so the UI renders one uniform span model and never has to know
//! Discord's syntax. Image resolution (emoji/sticker → cached file) happens in
//! the async pass; the tokenizer itself is pure and unit-tested.

use std::path::PathBuf;

use messenger_interface::types::{Emoji, RichText, Span, TextStyle};

use crate::{
    api_types::{self, SNOWFLAKE},
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

/// Download+cache a Discord sticker image. Lottie (format 3) has no static
/// image, so it falls back to alt text (`None`).
async fn resolve_sticker(sticker: &api_types::StickerItem) -> Option<PathBuf> {
    CdnImage::sticker(sticker.id, sticker.format_type)?
        .fetch()
        .await
        .ok()
}

/// Build the [`RichText`] for a message body plus its stickers, resolving
/// emoji/sticker images into the cache as it goes.
pub async fn build_content(content: &str, stickers: &[api_types::StickerItem]) -> RichText {
    let mut spans = Vec::new();
    for piece in tokenize(content) {
        match piece {
            Piece::Text { text, style } => spans.push(Span::Text { text, style }),
            Piece::Link { text, href } => spans.push(Span::Link { text, href }),
            Piece::Emoji { name, id, animated } => {
                let image = resolve_emoji(id, animated).await;
                spans.push(Span::Emoji(Emoji {
                    shortcode: name,
                    image,
                }));
            }
        }
    }
    for sticker in stickers {
        let image = resolve_sticker(sticker).await;
        spans.push(Span::Sticker {
            alt: sticker.name.clone(),
            image,
        });
    }
    RichText { spans }
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
}
