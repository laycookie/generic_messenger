use iced::{
    Color, Element, Font, Length, Padding,
    advanced::graphics::core::font,
    widget::{
        Button, Row, Text, column, container, image, row,
        text::{Rich, Span},
    },
};
use messenger_interface::types::{Identifier, Message};
use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{tag, tag_no_case, take_until},
    character::complete::{alphanumeric1, one_of},
    combinator::recognize,
    multi::many1,
    sequence::delimited,
};

/// Excludes regular text type
#[derive(Debug)]
enum Markdown<'a> {
    Bold(&'a str),
    Italicized(&'a str),
    Link(&'a str),
}

// === Parsers ===
fn bold_parser(input: &str) -> IResult<&str, Markdown<'_>> {
    let (left, parsed) = delimited(tag("**"), take_until("**"), tag("**")).parse(input)?;
    Ok((left, Markdown::Bold(parsed)))
}
fn italicized_parser(input: &str) -> IResult<&str, Markdown<'_>> {
    let (left, parsed) = delimited(tag("*"), take_until("*"), tag("*")).parse(input)?;
    Ok((left, Markdown::Italicized(parsed)))
}

// TODO: Very loose rn, solidify it
fn url_parser(input: &str) -> IResult<&str, Markdown<'_>> {
    let protocol_scheme = alt((tag_no_case("https://"), tag_no_case("http://")));
    let valid_url_char = alt((alphanumeric1, recognize(one_of("-._~:/?#[]@!$&'()*+,;=%"))));

    let (left, parsed) = recognize((protocol_scheme, many1(valid_url_char))).parse(input)?;
    Ok((left, Markdown::Link(parsed)))
}

// === Special parsers ===
fn until_parser<'a, F>(
    mut parser: F,
) -> impl FnMut(&'a str) -> IResult<&'a str, (&'a str, Markdown<'a>)>
where
    F: Parser<&'a str, Output = Markdown<'a>, Error = nom::error::Error<&'a str>>,
{
    move |input: &str| {
        let err = match parser.parse(input) {
            Ok(val) => return Ok(("", val)),
            Err(err) => err,
        };

        for (i, _) in input.char_indices().skip(1) {
            if let Ok((left, parsed)) = parser.parse(&input[i..]) {
                return Ok((&input[..i], (left, parsed)));
            }
        }

        Err(err)
    }
}
// ======================

pub fn message_text<'a, M: Clone + 'static>(msg: &'a Identifier<Message>) -> Element<'a, M> {
    // === Author ===
    // TODO(record-migration): `messenger_interface::types::Message` no longer includes an author.
    // If the UI should display author names/avatars, we should reintroduce it in the interface types
    // (and have adapters populate it).
    let icon: std::path::PathBuf = "./public/imgs/placeholder.jpg".into();
    let image_height = Length::Fixed(36.0);
    let author = Text::from("Unknown");

    // === Create Message text box ===
    let mut spans: std::vec::Vec<Span<'_>> = Vec::new();

    let mut text_left = msg.text.as_str();
    while let Ok((regular_text_parsed, (unparssed, parsed_markdown))) =
        until_parser(alt((url_parser, italicized_parser, bold_parser))).parse(text_left)
    {
        if !regular_text_parsed.is_empty() {
            spans.push(Span::new(regular_text_parsed));
        }
        spans.push(match parsed_markdown {
            Markdown::Italicized(text) => Span::new(text).font(Font {
                style: font::Style::Italic,
                ..Default::default()
            }),
            Markdown::Bold(text) => Span::new(text).font(Font {
                weight: font::Weight::Bold,
                ..Default::default()
            }),
            Markdown::Link(link) => Span::new(link).color(Color::from_rgb(0.0, 0.0, 1.0)),
        });
        text_left = unparssed;
    }
    spans.push(Span::new(text_left));

    let message = Rich::from_iter(spans);

    // === Reactions ===
    let reactions = Row::from_iter(msg.reactions.iter().map(|reaction| {
        Button::new(row![
            Rich::with_spans([Span::<M>::new(reaction.emoji)]),
            Text::new(reaction.count)
        ])
        .into()
    }));

    row![
        image(&icon).height(image_height),
        container(column![author, message, reactions]).padding(Padding::new(0.0).left(5.0))
    ]
    .into()
}
