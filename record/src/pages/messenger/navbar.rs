use std::iter;

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::{Button, Column, Scrollable, image};
use iced::{ContentFit, Element, Length};
use messenger_interface::types::{House, Identifier, Place};

use crate::messanger_unifier::{MessangerHandle, Messangers};
use crate::pages::messenger::PLACEHOLDER_PFP;

#[derive(Debug)]
pub struct Navbar;

#[derive(Debug, Clone)]
pub enum Action {
    GetDMs,
    GetGuild {
        handle: MessangerHandle,
        server: Identifier<Place<House>>,
    },
}

impl Navbar {
    pub fn get_element<'a>(messengers: &'a Messangers) -> Element<'a, Action> {
        let dm_switch = Element::from(Button::new("test").on_press(Action::GetDMs));

        let servers = messengers.data_iter().flat_map(|data| {
            data.guilds.iter().map(|server| {
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
                        handle: data.handle(),
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
