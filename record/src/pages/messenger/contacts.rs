use iced::{
    Element, Task,
    widget::{Column, Text, TextInput},
};

use crate::messanger_unifier::Messangers;

#[derive(Debug, Clone, Default)]
pub struct Contacts {
    search_input: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    MsgInput(String),
}

impl Contacts {
    pub fn get_element<'a>(&self, messengers: &'a Messangers) -> Element<'a, Message> {
        let widget = Column::new();
        let widget =
            widget.push(TextInput::new("Search", &self.search_input).on_input(Message::MsgInput));
        widget
            .push(
                messengers
                    .data_iter()
                    .flat_map(|messanger| {
                        messanger.contacts.iter().filter_map(|i| {
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
    pub fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::MsgInput(change) => {
                self.search_input = change;
                Task::none()
            }
        }
    }
}
