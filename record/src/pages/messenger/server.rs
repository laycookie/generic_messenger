use adaptors::types::{Chan, Identifier};
use iced::{
    Element, Task,
    widget::{Button, Column},
};

use crate::messanger_unifier::Messangers;

#[derive(Debug, Clone)]
pub struct Server {
    channels: Vec<Identifier<Chan>>,
}

#[derive(Debug, Clone)]
pub enum Message {}

impl Server {
    pub fn new(channels: Vec<Identifier<Chan>>) -> Self {
        Self { channels }
    }

    pub fn get_element<'a, Theme, Renderer>(
        &'a self,
        messengers: &'a Messangers,
    ) -> Element<'a, Message, Theme, Renderer>
    where
        Renderer: iced::advanced::Renderer + iced::advanced::text::Renderer + 'a,
        Theme: iced::widget::button::Catalog + iced::widget::text::Catalog + 'a,
    {
        let channels = self
            .channels
            .iter()
            .map(|chan| Element::from(Button::new(chan.name.as_str())));
        Column::from_iter(channels).into()
    }

    pub fn update(&self, msg: Message) -> Task<Message> {
        println!("Triggered");
        // match msg {};
        Task::none()
    }
}
