use iced::{
    Alignment, ContentFit, Element, Length, Padding, Task,
    widget::{Button, Column, Row, Scrollable, Text, TextInput, column, container, image},
};

use crate::components::user_status::status_indicator;
use messenger_interface::types::{Identifier, User};
use messenger_unifier::{MessengerId, MessengerRegistry};

use super::PLACEHOLDER_PFP;

#[derive(Debug, Clone, Default)]
pub struct Contacts {
    search_input: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    MsgInput(String),
    /// A contact was clicked; open the DM conversation with them.
    OpenDm {
        id: MessengerId,
        user: Identifier<User>,
    },
}

impl Contacts {
    fn matches_search(contact: &User, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || contact.name.to_lowercase().contains(&query)
            || contact
                .tag
                .as_ref()
                .is_some_and(|tag| tag.to_lowercase().contains(&query))
    }

    pub fn get_element<'a>(&self, messengers: &'a MessengerRegistry) -> Element<'a, Action> {
        let widget = Column::new();
        let widget =
            widget.push(TextInput::new("Search", &self.search_input).on_input(Action::MsgInput));
        let contacts = messengers
            .iter()
            .flat_map(|(_, entry)| {
                let id = entry.interface.id;
                entry.data.contacts.iter().filter_map(move |contact| {
                    if Self::matches_search(contact, &self.search_input) {
                        let avatar = match contact.icon.as_ref() {
                            Some(icon) => image(icon),
                            None => image(PLACEHOLDER_PFP),
                        };

                        let mut label = Row::new().spacing(6.0).align_y(Alignment::Center);
                        label = label.push(
                            container(
                                avatar
                                    .height(Length::Fixed(28.0))
                                    .width(Length::Fixed(28.0))
                                    .content_fit(ContentFit::Cover),
                            )
                            .padding(Padding::new(0.0).right(4.0)),
                        );
                        if let Some(status) = status_indicator(contact.status) {
                            label = label.push(status);
                        }
                        let mut identity = column![Text::from(contact.name.as_str())];
                        if let Some(tag) = contact.tag.as_ref() {
                            identity = identity.push(Text::new(tag.as_str()).size(12));
                        }
                        label = label.push(identity);

                        return Some(Button::new(label).width(Length::Fill).on_press(
                            Action::OpenDm {
                                id,
                                user: contact.clone(),
                            },
                        ));
                    }
                    None
                })
            })
            .fold(Column::new(), |column, widget| column.push(widget));

        widget
            .push(Scrollable::new(contacts).height(Length::Fill))
            .into()
    }

    pub fn update(&mut self, msg: Action) -> Task<Action> {
        match msg {
            Action::MsgInput(change) => {
                self.search_input = change;
                Task::none()
            }
            Action::OpenDm { .. } => Task::none(),
        }
    }
}
