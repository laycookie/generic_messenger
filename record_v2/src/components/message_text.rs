use iced::{
    Alignment, Color, Element, Font, Length, Padding,
    advanced::graphics::core::font,
    widget::{
        Button, Column, Row, Text, column, container, image, row,
        text::{Rich, Span},
    },
};
use iced_aw::Wrap;
use messenger_interface::types::{Emoji, Identifier, Message, Span as RichSpan, TextStyle};

/// IDs at or above this value are pending (optimistic send, not yet confirmed).
const PENDING_ID_THRESHOLD: u64 = u64::MAX - 1_000_000;

const LINK_COLOR: Color = Color::from_rgb(0.0, 0.0, 1.0);
const PENDING_COLOR: Color = Color::from_rgb(0.0, 0.8, 0.0);
const EDITED_COLOR: Color = Color::from_rgb(0.5, 0.5, 0.5);
/// Inline emoji image edge length, ~matching the text line height.
const EMOJI_SIZE: f32 = 20.0;
const STICKER_SIZE: f32 = 120.0;

fn font_for(style: TextStyle) -> Font {
    Font {
        weight: if style.bold {
            font::Weight::Bold
        } else {
            font::Weight::Normal
        },
        style: if style.italic {
            font::Style::Italic
        } else {
            font::Style::Normal
        },
        ..Default::default()
    }
}

/// One iced rich-text span for a styled run (the no-inline-image path).
fn styled_span<'a>(text: &'a str, style: TextStyle) -> Span<'a> {
    Span::new(text).font(font_for(style))
}

/// One word as a `Text` widget (the inline-image flow path).
fn word<'a>(text: &'a str, style: TextStyle, color: Option<Color>) -> Text<'a> {
    let text = Text::new(text).font(font_for(style));
    match color {
        Some(color) => text.color(color),
        None => text,
    }
}

pub fn message_text<'a, M: Clone + 'static>(
    msg: &'a Identifier<Message>,
    on_reaction: impl Fn(&'a Identifier<Message>, &'a str, bool) -> M + 'a,
) -> Element<'a, M> {
    let pending = *msg.id() >= PENDING_ID_THRESHOLD;
    // === Author ===
    let icon: std::path::PathBuf = msg
        .author
        .as_ref()
        .and_then(|a| a.icon.clone())
        .unwrap_or_else(|| "./public/imgs/placeholder.jpg".into());
    let image_height = Length::Fixed(36.0);
    let author_name: &str = msg
        .author
        .as_ref()
        .map(|a| a.name.as_str())
        .unwrap_or("Unknown");
    let author = Text::from(author_name);

    let spans = &msg.content.text.spans;
    let stickers: Vec<&'a std::path::PathBuf> = spans
        .iter()
        .filter_map(|span| match span {
            RichSpan::Sticker {
                image: Some(path), ..
            } => Some(path),
            _ => None,
        })
        .collect();
    // iced rich text can't embed images inline, so only switch to the flowing
    // word/image layout when an emoji actually resolved to an image; plain and
    // emoji-less messages keep the simpler, better-shaped `Rich` text.
    let has_inline_image = spans
        .iter()
        .any(|span| matches!(span, RichSpan::Emoji(Emoji { image: Some(_), .. })));

    let text_content: Element<'a, M> = if has_inline_image {
        let text_color = pending.then_some(PENDING_COLOR);
        let link_color = if pending { PENDING_COLOR } else { LINK_COLOR };
        let mut elements: Vec<Element<'a, M>> = Vec::new();
        for span in spans {
            match span {
                RichSpan::Text { text, style } => {
                    for w in text.split_whitespace() {
                        elements.push(word(w, *style, text_color).into());
                    }
                }
                RichSpan::Link { text, .. } => {
                    for w in text.split_whitespace() {
                        elements.push(word(w, TextStyle::default(), Some(link_color)).into());
                    }
                }
                RichSpan::Emoji(emoji) => match &emoji.image {
                    Some(path) => elements.push(
                        image(path)
                            .width(Length::Fixed(EMOJI_SIZE))
                            .height(Length::Fixed(EMOJI_SIZE))
                            .into(),
                    ),
                    None => {
                        let shortcode = Text::new(format!(":{}:", emoji.shortcode));
                        let shortcode = match text_color {
                            Some(color) => shortcode.color(color),
                            None => shortcode,
                        };
                        elements.push(shortcode.into());
                    }
                },
                RichSpan::Sticker { .. } => {}
            }
        }
        if msg.is_edited() {
            elements.push(Text::new("(edited)").size(12.0).color(EDITED_COLOR).into());
        }
        Wrap::with_elements(elements)
            .spacing(3.0)
            .line_spacing(3.0)
            .align_items(Alignment::Center)
            .width_items(Length::Fill)
            .into()
    } else {
        let mut rich_spans: Vec<Span<'_>> = Vec::new();
        for span in spans {
            match span {
                RichSpan::Text { text, style } => rich_spans.push(styled_span(text, *style)),
                RichSpan::Emoji(emoji) => {
                    rich_spans.push(Span::new(format!(":{}:", emoji.shortcode)))
                }
                RichSpan::Link { text, .. } => {
                    rich_spans.push(Span::new(text.as_str()).color(LINK_COLOR))
                }
                // Sticker images render below; a sticker with no image falls
                // back to its alt text here.
                RichSpan::Sticker { alt, image } => {
                    if image.is_none() {
                        rich_spans.push(Span::new(alt.as_str()));
                    }
                }
            }
        }
        if pending {
            for span in &mut rich_spans {
                *span = span.clone().color(PENDING_COLOR);
            }
        }
        if msg.is_edited() {
            rich_spans.push(Span::new(" (edited)").color(EDITED_COLOR).size(12.0));
        }
        Rich::from_iter(rich_spans).into()
    };

    // === Message body: text content, then any sticker images ===
    let mut body = Column::new().push(text_content);
    for sticker in stickers {
        body = body.push(image(sticker).height(Length::Fixed(STICKER_SIZE)));
    }

    // === Reactions ===
    let reactions = Row::from_iter(msg.reactions.iter().map(|reaction| {
        Button::new(row![
            Rich::with_spans([Span::<M>::new(reaction.emoji.shortcode.clone())]),
            Text::new(reaction.count)
        ])
        .on_press(on_reaction(
            msg,
            &reaction.emoji.shortcode,
            reaction.reacted,
        ))
        .into()
    }));

    row![
        image(&icon).height(image_height),
        container(column![author, body, reactions])
            .width(Length::Fill)
            .padding(Padding::new(0.0).left(5.0))
    ]
    .into()
}
