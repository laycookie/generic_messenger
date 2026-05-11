use iced::{
    Element, Task,
    widget::{Column, Text, TextInput},
};

use crate::state::MessengerRegistry;

#[derive(Debug, Clone, Default)]
pub struct Contacts {
    search_input: String,
}

#[derive(Debug, Clone)]
pub enum Action {
    MsgInput(String),
}

impl Contacts {
    pub fn get_element<'a>(&self, messengers: &'a MessengerRegistry) -> Element<'a, Action> {
        let widget = Column::new();
        let widget =
            widget.push(TextInput::new("Search", &self.search_input).on_input(Action::MsgInput));
        widget
            .push(
                messengers
                    .iter()
                    .flat_map(|(_, entry)| {
                        entry.data.contacts.iter().filter_map(|i| {
                            if self.search_input.is_empty()
                                || i.name
                                    .to_lowercase()
                                    .contains(self.search_input.to_lowercase().as_str())
                            {
                                return Some(Text::from(i.name.as_str()));
                            }
                            None
                        })
                    })
                    .fold(Column::new(), |column, widget| column.push(widget)),
            )
            .into()
    }
    pub fn update(&mut self, msg: Action) -> Task<Action> {
        match msg {
            Action::MsgInput(change) => {
                self.search_input = change;
                Task::none()
            }
        }
    }
}
