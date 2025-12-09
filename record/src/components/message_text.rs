use adaptors::types::{Identifier, Msg};
use iced::{
    Color, Element, Font,
    advanced::graphics::core::font,
    widget::{
        Button, Row, Text, column, row,
        text::{Rich, Span},
    },
};
use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{tag, tag_no_case, take_until},
    character::complete::{alphanumeric1, one_of},
    combinator::recognize,
    multi::many1,
    sequence::delimited,
};

#[derive(Debug)]
enum MarkdownText<'a> {
    Link(&'a str),
    Bold(&'a str),
}

fn bold_parser(input: &str) -> IResult<&str, MarkdownText<'_>> {
    let (left, parsed) = delimited(tag("**"), take_until("**"), tag("**")).parse(input)?;
    Ok((left, MarkdownText::Bold(parsed)))
}

// TODO: Very loose rn, solidify it
fn url_parser(input: &str) -> IResult<&str, MarkdownText<'_>> {
    let protocol_scheme = alt((tag_no_case("https://"), tag_no_case("http://")));
    let valid_url_char = alt((alphanumeric1, recognize(one_of("-._~:/?#[]@!$&'()*+,;=%"))));

    let (left, parsed) = recognize((protocol_scheme, many1(valid_url_char))).parse(input)?;
    Ok((left, MarkdownText::Link(parsed)))
}

fn until_parser<'a, F>(
    mut parser: F,
) -> impl FnMut(&'a str) -> IResult<&'a str, (&'a str, MarkdownText<'a>)>
where
    F: Parser<&'a str, Output = MarkdownText<'a>, Error = nom::error::Error<&'a str>>,
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

#[derive(Clone)]
enum Link {}

pub fn message_text<'a, M: Clone + 'static>(msg: &'a Identifier<Msg>) -> Element<'a, M> {
    // === Author ===
    let author = Text::from(msg.author.name.as_str());

    // === Create Message text box ===
    let mut spans: std::vec::Vec<iced::advanced::text::Span<'_, Link>> = Vec::new();

    let mut text_left = msg.text.as_str();
    while let Ok((text_span, (left, special_markdown))) =
        until_parser(alt((url_parser, bold_parser))).parse(text_left)
    {
        if !text_span.is_empty() {
            spans.push(Span::new(text_span));
        }
        spans.push(match special_markdown {
            MarkdownText::Link(link) => Span::new(link).color(Color::from_rgb(0.0, 0.0, 1.0)),
            MarkdownText::Bold(text) => Span::new(text).font(Font {
                weight: font::Weight::Bold,
                ..Default::default()
            }),
        });
        text_left = left;
    }
    spans.push(Span::new(text_left));

    let message = Rich::from_iter(spans);

    // === Reactions ===
    let reactions = Row::from_iter(msg.reactions.iter().map(|reaction| {
        Button::new(row![
            Rich::with_spans([Span::<Link>::new(reaction.emoji)]),
            Text::new(reaction.count)
        ])
        .into()
    }));

    column![author, message, reactions].into()
}
