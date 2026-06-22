//! Parse Steam chat content into the interface [`RichText`] model.
//!
//! Steam carries everything in one string: plain text, BBCode (`[b]`, `[i]`,
//! `[u]`, `[url]`), inline emoticons as `[emoticon]name[/emoticon]` tags, and stickers as a
//! `[sticker …]` tag. (The proto has no structured emoji/sticker field — see
//! [[project_steam_vent_no_disconnect_signal]] siblings.) We parse it here so
//! the UI renders one uniform span model. The tokenizer is pure and unit
//! tested; emoticon/sticker image resolution happens in the async pass.
//!
//! Sticker note: Steam sends stickers as `[sticker type="name" limit="0"][/sticker]`
//! (confirmed from live data). [`resolve_sticker`] reads the name from `type` and
//! downloads from the economy sticker CDN, falling back to a placeholder when that
//! can't resolve. The exact CDN URL shape may still need tuning per sticker.

use std::path::PathBuf;

use messenger_interface::types::{CacheCategory, Emoji, RichText, Span, TextStyle, cache_img_dir};
use tracing::debug;

use crate::downloaders::cache_remote_image;

/// Steam emoticon image CDN; the path segment is the bare emoticon name.
const EMOTICON_CDN: &str = "https://community.steamstatic.com/economy/emoticon";

/// Steam sticker image CDN. Stickers are economy items served alongside
/// emoticons; the path segment is the sticker's name/path (see [`resolve_sticker`]).
const STICKER_CDN: &str = "https://community.steamstatic.com/economy/sticker";

#[derive(Debug, PartialEq)]
enum Piece {
    Text { text: String, style: TextStyle },
    Emoticon { name: String },
    Sticker { raw: String },
    Link { text: String, href: String },
}

/// `[sticker …]` — Steam's sticker tag, e.g. `[sticker type="name" limit="0"]`.
/// Attributes live on the opening tag; Steam follows it with an empty `[/sticker]`
/// close which we consume here so it does not leak into the rendered text.
/// Guarded so `[stickerfoo]` (a different tag sharing the prefix) does not match.
fn match_sticker(rest: &str) -> Option<(usize, Piece)> {
    const CLOSE: &str = "[/sticker]";
    let after = rest.strip_prefix("[sticker")?;
    match after.bytes().next() {
        Some(b' ' | b'=' | b']') => {}
        _ => return None,
    }
    let end = after.find(']')?;
    let mut consumed = "[sticker".len() + end + 1;
    if rest[consumed..].starts_with(CLOSE) {
        consumed += CLOSE.len();
    }
    Some((
        consumed,
        Piece::Sticker {
            raw: after[..end].trim().to_owned(),
        },
    ))
}

/// `[url=HREF]TEXT[/url]` or `[url]HREF[/url]`.
fn match_url(rest: &str) -> Option<(usize, Piece)> {
    const CLOSE: &str = "[/url]";
    if let Some(after) = rest.strip_prefix("[url=") {
        let rb = after.find(']')?;
        let href = after[..rb].to_owned();
        let body = &after[rb + 1..];
        let end = body.find(CLOSE)?;
        let text = &body[..end];
        let text = if text.is_empty() {
            href.clone()
        } else {
            text.to_owned()
        };
        return Some((
            "[url=".len() + rb + 1 + end + CLOSE.len(),
            Piece::Link { text, href },
        ));
    }
    let after = rest.strip_prefix("[url]")?;
    let end = after.find(CLOSE)?;
    let href = after[..end].to_owned();
    Some((
        "[url]".len() + end + CLOSE.len(),
        Piece::Link {
            text: href.clone(),
            href,
        },
    ))
}

/// `[tag]inner[/tag]` for a simple styling tag. `[u]` maps to no style (we have
/// no underline yet) but its tags are still stripped.
fn match_simple_tag(rest: &str, tag: &str, style: TextStyle) -> Option<(usize, Piece)> {
    let open = ["[", tag, "]"].concat();
    let close = ["[/", tag, "]"].concat();
    let inner = rest.strip_prefix(&open)?;
    let end = inner.find(&close)?;
    Some((
        open.len() + end + close.len(),
        Piece::Text {
            text: inner[..end].to_owned(),
            style,
        },
    ))
}

/// `[emoticon]name[/emoticon]` — Steam's BBCode form for an inline emoticon
/// (what `bbcode_format` returns). A literal `:name:` is *not* an emoticon and
/// stays plain text.
fn match_emoticon(rest: &str) -> Option<(usize, Piece)> {
    const OPEN: &str = "[emoticon]";
    const CLOSE: &str = "[/emoticon]";
    let inner = rest.strip_prefix(OPEN)?;
    let end = inner.find(CLOSE)?;
    let name = inner[..end].trim();
    if name.is_empty() {
        return None;
    }
    Some((
        OPEN.len() + end + CLOSE.len(),
        Piece::Emoticon {
            name: name.to_owned(),
        },
    ))
}

fn match_special(rest: &str) -> Option<(usize, Piece)> {
    let bold = TextStyle {
        bold: true,
        ..Default::default()
    };
    let italic = TextStyle {
        italic: true,
        ..Default::default()
    };
    match_sticker(rest)
        .or_else(|| match_url(rest))
        .or_else(|| match_simple_tag(rest, "b", bold))
        .or_else(|| match_simple_tag(rest, "i", italic))
        .or_else(|| match_simple_tag(rest, "u", TextStyle::default()))
        .or_else(|| match_emoticon(rest))
}

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

/// Download+cache a Steam emoticon image, returning the local path.
async fn resolve_emoticon(name: &str) -> Option<PathBuf> {
    let url = format!("{EMOTICON_CDN}/{name}");
    let dir = cache_img_dir(CacheCategory::Emoji, "steam", name);
    cache_remote_image(&url, dir, &format!("{name}.png")).await
}

/// Parse the inside of a `[sticker …]` tag into its `key=value` attributes.
///
/// The exact attribute set is still unverified against live data, so this stays
/// deliberately permissive: whitespace-separated `key=value` pairs, with the
/// value optionally double-quoted (`id="123"`, which may then contain spaces).
/// Anything that isn't a `key=value` pair is skipped. Returned in source order.
fn parse_sticker_attrs(raw: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let mut rest = raw.trim_start();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                None => (quoted, ""),
            }
        } else {
            match after.find(char::is_whitespace) {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        if !key.is_empty() {
            attrs.push((key.to_owned(), value.to_owned()));
        }
        rest = next.trim_start();
    }
    attrs
}

/// Pull the sticker name out of a sticker tag's attributes.
///
/// Confirmed from live data: Steam carries the sticker name in the `type`
/// attribute (e.g. `[sticker type="aliya_anoxic" limit="0"]`). A few other keys
/// are tried as defensive fallbacks. A value that already looks like an absolute
/// URL is used directly by the caller; otherwise it is a name under the economy
/// sticker CDN. Returns `None` when nothing usable is present.
fn sticker_image_ref(attrs: &[(String, String)]) -> Option<&str> {
    const KEYS: [&str; 5] = ["type", "url", "name", "value", "id"];
    KEYS.iter().find_map(|wanted| {
        attrs
            .iter()
            .find(|(key, val)| key == wanted && !val.is_empty())
            .map(|(_, val)| val.as_str())
    })
}

/// Reduce a sticker reference (a name, path, or URL) to a safe single-segment
/// cache key for the directory and filename.
fn sticker_cache_key(reference: &str) -> String {
    reference
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Sticker resolution. Reads the sticker name from the tag's attributes
/// ([`sticker_image_ref`]) and downloads it from the economy sticker CDN. On any
/// failure (`cache_remote_image` returns `None`) we fall back to a `[sticker]`
/// placeholder with no image, which still keeps sticker-only messages from
/// vanishing entirely. A download warning echoes the attempted URL, so a 404
/// reveals if the CDN path shape needs adjusting for a given sticker.
async fn resolve_sticker(raw: &str) -> (String, Option<PathBuf>) {
    let attrs = parse_sticker_attrs(raw);
    debug!("Steam: sticker tag attrs={attrs:?} raw=[sticker {raw}]");

    let image = match sticker_image_ref(&attrs) {
        Some(reference) => {
            let url = if reference.starts_with("http") {
                reference.to_owned()
            } else {
                format!("{STICKER_CDN}/{reference}")
            };
            let key = sticker_cache_key(reference);
            let dir = cache_img_dir(CacheCategory::Stickers, "steam", &key);
            cache_remote_image(&url, dir, &format!("{key}.png")).await
        }
        None => {
            debug!("Steam: sticker tag had no recognizable image attribute (confirm format)");
            None
        }
    };
    ("[sticker]".to_owned(), image)
}

/// Build [`RichText`] from a raw Steam chat string (BBCode form), resolving
/// emoticon/sticker images into the cache as it goes.
pub async fn build_content(raw: &str) -> RichText {
    let mut spans = Vec::new();
    for piece in tokenize(raw) {
        match piece {
            Piece::Text { text, style } => spans.push(Span::Text { text, style }),
            Piece::Link { text, href } => spans.push(Span::Link { text, href }),
            Piece::Emoticon { name } => {
                let image = resolve_emoticon(&name).await;
                spans.push(Span::Emoji(Emoji {
                    shortcode: name,
                    image,
                }));
            }
            Piece::Sticker { raw } => {
                let (alt, image) = resolve_sticker(&raw).await;
                spans.push(Span::Sticker { alt, image });
            }
        }
    }
    RichText { spans }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn emoticon_between_text() {
        let pieces = tokenize("hi [emoticon]steamhappy[/emoticon] yo");
        assert_eq!(
            pieces,
            vec![
                Piece::Text {
                    text: "hi ".to_owned(),
                    style: TextStyle::default()
                },
                Piece::Emoticon {
                    name: "steamhappy".to_owned()
                },
                Piece::Text {
                    text: " yo".to_owned(),
                    style: TextStyle::default()
                },
            ]
        );
    }

    #[test]
    fn bbcode_bold_italic() {
        let pieces = tokenize("[b]big[/b] and [i]slanted[/i]");
        assert!(matches!(&pieces[0], Piece::Text { text, style } if text == "big" && style.bold));
        assert!(
            matches!(&pieces[2], Piece::Text { text, style } if text == "slanted" && style.italic)
        );
    }

    #[test]
    fn bbcode_url_both_forms() {
        let labeled = tokenize("[url=https://x.com]click[/url]");
        assert!(
            matches!(&labeled[0], Piece::Link { text, href } if text == "click" && href == "https://x.com")
        );
        let bare = tokenize("[url]https://y.com[/url]");
        assert!(
            matches!(&bare[0], Piece::Link { text, href } if text == "https://y.com" && href == "https://y.com")
        );
    }

    #[test]
    fn sticker_tag_is_captured_not_dropped() {
        let pieces = tokenize("[sticker type=foo id=42]");
        assert_eq!(
            pieces,
            vec![Piece::Sticker {
                raw: "type=foo id=42".to_owned()
            }]
        );
        // A look-alike tag must not be swallowed as a sticker.
        assert!(matches!(&tokenize("[stickerish]")[0], Piece::Text { .. }));
    }

    #[test]
    fn sticker_closing_tag_is_consumed() {
        // Real Steam form: a trailing [/sticker] must not leak into the text.
        let pieces = tokenize(r#"[sticker type="aliya_anoxic" limit="0"][/sticker]"#);
        assert_eq!(
            pieces,
            vec![Piece::Sticker {
                raw: r#"type="aliya_anoxic" limit="0""#.to_owned()
            }]
        );
        // The name is read from `type`.
        let attrs = parse_sticker_attrs(r#"type="aliya_anoxic" limit="0""#);
        assert_eq!(sticker_image_ref(&attrs), Some("aliya_anoxic"));
    }

    #[test]
    fn sticker_attrs_parsed_unquoted_and_quoted() {
        assert_eq!(
            parse_sticker_attrs("type=foo id=42"),
            vec![
                ("type".to_owned(), "foo".to_owned()),
                ("id".to_owned(), "42".to_owned()),
            ]
        );
        // Quoted values may contain spaces; bare flags are skipped.
        assert_eq!(
            parse_sticker_attrs(r#"id="ab cd" appid=730"#),
            vec![
                ("id".to_owned(), "ab cd".to_owned()),
                ("appid".to_owned(), "730".to_owned()),
            ]
        );
    }

    #[test]
    fn sticker_image_ref_priority_and_cache_key() {
        // `type` (the confirmed sticker-name key) wins over other candidates.
        let attrs = parse_sticker_attrs(r#"type=aliya_anoxic url=http://x/y.png"#);
        assert_eq!(sticker_image_ref(&attrs), Some("aliya_anoxic"));
        // Falls through to `id` when no higher-priority key is present.
        let attrs = parse_sticker_attrs("limit=0 id=42");
        assert_eq!(sticker_image_ref(&attrs), Some("42"));
        // Nothing usable -> placeholder path.
        assert_eq!(sticker_image_ref(&parse_sticker_attrs("limit=0")), None);
        // Cache key is a safe single segment.
        assert_eq!(sticker_cache_key("a/b c.png"), "a_b_c_png");
    }

    #[test]
    fn colon_shortcodes_are_literal_text() {
        // Steam sends emoticons as [emoticon] tags, so a literal :name: (or any
        // stray colon) is ordinary text, never an emoticon.
        assert_eq!(
            tokenize(":steamhappy: ratio 3:2"),
            vec![Piece::Text {
                text: ":steamhappy: ratio 3:2".to_owned(),
                style: TextStyle::default()
            }]
        );
    }

    #[test]
    fn emoticon_tag_only_message() {
        assert_eq!(
            tokenize("[emoticon]cleanhourglass[/emoticon]"),
            vec![Piece::Emoticon {
                name: "cleanhourglass".to_owned()
            }]
        );
    }
}
