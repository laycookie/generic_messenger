use iced::{
    Alignment, Color, Element, Font, Length, Padding,
    advanced::graphics::core::font,
    widget::{
        Button, Column, Row, Text, column, container, image, mouse_area, row,
        text::{Rich, Span},
    },
};
use iced_aw::Wrap;
use messenger_interface::types::{
    Emoji, Identifier, Message, Span as RichSpan, Sticker, TextStyle, User,
};

use crate::components::user_status::status_indicator;

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
    pending: bool,
    on_reaction: impl Fn(&'a Identifier<Message>, &'a str, bool) -> M + 'a,
    on_author: impl Fn(&'a Identifier<User>) -> M + 'a,
    on_link: impl Fn(&str) -> M + 'a,
) -> Element<'a, M> {
    // === Author ===
    let icon: std::path::PathBuf = msg
        .author
        .as_ref()
        .and_then(|a| a.icon.clone())
        .unwrap_or_else(|| "./public/imgs/placeholder.jpg".into());
    let image_height = Length::Fixed(36.0);
    // Clicking the author name inspects their profile.
    let author: Element<'a, M> = match msg.author.as_ref() {
        Some(author) => {
            let mut row = Row::new().spacing(4.0).align_y(Alignment::Center);
            if let Some(status) = status_indicator(author.status) {
                row = row.push(status);
            }
            row.push(mouse_area(Text::from(author.name.as_str())).on_press(on_author(author)))
                .into()
        }
        None => Text::from("Unknown").into(),
    };

    let spans = &msg.content.text.spans;
    let stickers: Vec<&'a std::path::PathBuf> = spans
        .iter()
        .filter_map(|span| match span {
            RichSpan::Sticker(Sticker {
                image: Some(path), ..
            }) => Some(path),
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
                    // Keep delimiters inside each token. `split_whitespace`
                    // silently collapsed repeated spaces/newlines whenever an
                    // inline emoji forced this flow layout.
                    for run in text.split_inclusive(char::is_whitespace) {
                        elements.push(word(run, *style, text_color).into());
                    }
                }
                RichSpan::Link { text, href } => {
                    for run in text.split_inclusive(char::is_whitespace) {
                        elements.push(
                            mouse_area(word(run, TextStyle::default(), Some(link_color)))
                                .on_press(on_link(href))
                                .into(),
                        );
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
                RichSpan::Sticker(_) => {}
            }
        }
        if msg.is_edited() {
            elements.push(Text::new("(edited)").size(12.0).color(EDITED_COLOR).into());
        }
        Wrap::with_elements(elements)
            .spacing(0.0)
            .line_spacing(3.0)
            .align_items(Alignment::Center)
            .width_items(Length::Fill)
            .into()
    } else {
        let mut rich_spans: Vec<Span<'_, String>> = Vec::new();
        for span in spans {
            match span {
                RichSpan::Text { text, style } => {
                    rich_spans.push(Span::new(text.as_str()).font(font_for(*style)));
                }
                RichSpan::Emoji(emoji) => {
                    rich_spans.push(Span::new(format!(":{}:", emoji.shortcode)))
                }
                RichSpan::Link { text, href } => rich_spans.push(
                    Span::new(text.as_str())
                        .color(LINK_COLOR)
                        .underline(true)
                        .link(href.clone()),
                ),
                // Sticker images render below; a sticker with no image falls
                // back to its alt text here.
                RichSpan::Sticker(sticker) => {
                    if sticker.image.is_none() {
                        rich_spans.push(Span::new(sticker.alt.as_str()));
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
        Rich::from_iter(rich_spans)
            .on_link_click(move |href| on_link(&href))
            .into()
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
