use adaptors::types::{Identifier, Server};
use iced::advanced::{self, renderer};
use iced::widget::scrollable::{self, Direction, Scrollbar};
use iced::widget::{Button, Column, Scrollable, button, image};
use iced::{ContentFit, Element, Length, Task};

use crate::messanger_unifier::{MessangerHandle, Messangers};
use crate::pages::messenger::PLACEHOLDER_PFP;

#[derive(Debug)]
pub struct Navbar;

#[derive(Debug, Clone)]
pub enum Action {
    GetGuild {
        handle: MessangerHandle,
        server: Identifier<Server>,
    },
}

impl Navbar {
    pub fn get_element<'a, 'b, Theme, Renderer>(
        messengers: &'a Messangers,
    ) -> Element<'a, Action, Theme, Renderer>
    where
        Renderer: 'a + renderer::Renderer + advanced::image::Renderer,
        <Renderer as advanced::image::Renderer>::Handle:
            for<'c> From<&'c std::path::PathBuf> + From<&'static str>,
        Action: 'a + Clone,
        Theme: 'a + scrollable::Catalog + button::Catalog,
    {
        let servers = messengers
            .data_iter()
            .zip(messengers.interface_iter())
            .flat_map(|(data, (m_handle, _))| {
                data.guilds.iter().map(|server| {
                    let image = match &server.icon {
                        Some(icon) => image(icon),
                        None => image(PLACEHOLDER_PFP),
                    };
                    Element::from(
                        Button::new(
                            image
                                .height(Length::Fixed(48.0))
                                .width(Length::Fixed(48.0))
                                .content_fit(ContentFit::Cover),
                        )
                        .on_press(Action::GetGuild {
                            handle: *m_handle,
                            server: server.to_owned(),
                        }),
                    )
                })
            });

        Scrollable::new(Column::with_children(servers))
            .direction(Direction::Vertical(
                Scrollbar::default().width(0).scroller_width(0),
            ))
            .into()
    }
}
