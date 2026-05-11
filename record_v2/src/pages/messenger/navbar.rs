use std::iter;

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::{Button, Column, Scrollable, image};
use iced::{ContentFit, Element, Length};
use messenger_interface::types::{House, Identifier, Place};

use crate::pages::messenger::PLACEHOLDER_PFP;
use crate::state::{MessengerId, MessengerRegistry};

#[derive(Debug)]
pub struct Navbar;

#[derive(Debug, Clone)]
pub enum Action {
    GetDMs,
    GetGuild {
        id: MessengerId,
        server: Identifier<Place<House>>,
    },
}

impl Navbar {
    pub fn get_element<'a>(messengers: &'a MessengerRegistry) -> Element<'a, Action> {
        let dm_switch = Element::from(Button::new("test").on_press(Action::GetDMs));

        let servers = messengers.iter().flat_map(|(_, entry)| {
            let id = entry.interface.id;
            entry.data.guilds.iter().map(move |server| {
                let icon = &server.icon;
                let image = match icon {
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
                        id,
                        server: server.to_owned(),
                    }),
                )
            })
        });

        Scrollable::new(Column::with_children(iter::once(dm_switch).chain(servers)))
            .direction(Direction::Vertical(
                Scrollbar::default().width(0).scroller_width(0),
            ))
            .into()
    }
}
