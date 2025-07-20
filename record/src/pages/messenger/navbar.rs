use adaptors::types::{Identifier, Server as ServerType};
use iced::advanced::{self, renderer};
use iced::widget::scrollable::{self, Direction, Scrollbar};
use iced::widget::{Button, Column, Scrollable, button, image};
use iced::{ContentFit, Element, Length};

use crate::pages::messenger::PLACEHOLDER_PFP;

#[derive(Debug)]
pub struct Navbar;

impl Navbar {
    pub fn get_element<'a, 'b, Message, Theme, Renderer>(
        servers: impl Iterator<Item = &'b Identifier<ServerType>>,
    ) -> Element<'a, Message, Theme, Renderer>
    where
        Renderer: 'a + renderer::Renderer + advanced::image::Renderer,
        <Renderer as advanced::image::Renderer>::Handle:
            for<'c> From<&'c std::path::PathBuf> + From<&'static str>,
        Message: 'a + Clone,
        Theme: 'a + scrollable::Catalog + button::Catalog,
    {
        let servers = servers.into_iter().map(|server| {
            let image = match &server.icon {
                Some(icon) => image(icon),
                None => image(PLACEHOLDER_PFP),
            };
            Button::new(
                image
                    .height(Length::Fixed(48.0))
                    .width(Length::Fixed(48.0))
                    .content_fit(ContentFit::Cover),
            )
            .into()
        });

        Scrollable::new(Column::with_children(servers))
            .direction(Direction::Vertical(
                Scrollbar::default().width(0).scroller_width(0),
            ))
            .into()
    }
}
