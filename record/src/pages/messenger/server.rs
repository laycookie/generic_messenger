use adaptors::types::{Chan, Identifier};
use iced::{Element, Task, widget::Column};

use crate::messanger_unifier::{MessangerHandle, Messangers};

#[derive(Debug, Clone)]
pub struct Server {
    pub handle: MessangerHandle,
    pub channels: Vec<Identifier<Chan>>, // TODO: Move this in to cache
}

#[derive(Debug, Clone)]
pub enum Message {
    A,
}

impl Server {
    pub fn new(handle: MessangerHandle, channels: Vec<Identifier<Chan>>) -> Self {
        Self { handle, channels }
    }

    pub fn get_element<'a, Theme, Renderer>(
        &'a self,
        messengers: &'a Messangers,
    ) -> Element<'a, Message, Theme, Renderer>
    where
        Renderer: iced::advanced::Renderer + iced::advanced::text::Renderer + 'a,
        Theme: iced::widget::button::Catalog + iced::widget::text::Catalog + 'a,
    {
        // For testing might be worth nuking later as chat can pretty much sub this
        Column::new().into()
    }

    pub fn update(&self, msg: Message) -> Task<Message> {
        println!("Triggered");
        // match msg {};
        Task::none()
    }
}
