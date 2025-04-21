use adaptors::{discord::Discord, Messanger};
use iced::{
    widget::{column, combo_box::State, Button, Column, ComboBox, Container, TextInput},
    Alignment, Element, Task,
};
use std::{error, fmt::Display, sync::Arc};
use strum::EnumString;

use super::{chat::MessangerWindow, MyAppMessage, Page, UpdateResult};

// TODO: Make adapters handle the functionality of this enum
#[derive(Debug, Clone, EnumString)]
pub enum Platform {
    Discord,
    Test,
}
impl Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Discord => f.write_str("Discord"),
            Platform::Test => f.write_str("Test"),
        }
    }
}
impl Platform {
    pub fn to_messanger(&self, auth: String) -> Arc<dyn Messanger> {
        match self {
            Self::Discord => Arc::new(Discord::new(&auth)),
            Self::Test => todo!(),
        }
    }
    fn get_login_methods(&self) -> Vec<LoginMethods> {
        match self {
            Platform::Discord => vec![LoginMethods::Token],
            Platform::Test => vec![LoginMethods::Unkown],
        }
    }
}
// ===

enum LoginMethods {
    Token,
    Unkown,
}

#[derive(Debug, Clone)]
pub enum Message {
    PlatformInput(Platform),
    TokenInput(String),
    SubmitToken,
}
// TODO: Automate
impl Into<MyAppMessage> for Message {
    fn into(self) -> MyAppMessage {
        MyAppMessage::Login(self)
    }
}
//

pub struct Login {
    platform: State<Platform>,
    selected_platform: Platform,
    token: String,
}
impl Login {
    pub fn new() -> Self {
        // TODO: Automate addition of new enum varients in here
        let service = State::new(vec![Platform::Discord, Platform::Test]);
        Self {
            platform: service,
            selected_platform: Platform::Test,
            token: String::new(),
        }
    }
}
impl Page for Login {
    fn update(&mut self, message: MyAppMessage) -> Task<MyAppMessage> {
        if let MyAppMessage::Login(message) = message {
            match message {
                Message::PlatformInput(platform) => self.selected_platform = platform,
                Message::TokenInput(change) => self.token = change,
                Message::SubmitToken => {
                    // TODO: Disable submit button until the operation ether
                    let auth = self.selected_platform.to_messanger(self.token.clone());
                    return Task::perform(async { auth }, |auth| MyAppMessage::AddAuth(auth));
                }
            }
        }

        Task::none()
    }

    fn view(&self) -> Element<MyAppMessage> {
        let width = 360.0;

        let select_platform = ComboBox::new(
            &self.platform,
            "Platform",
            Some(&self.selected_platform),
            |platform| MyAppMessage::Login(Message::PlatformInput(platform)),
        );

        let auth_input = self
            .selected_platform
            .get_login_methods()
            .iter()
            .filter_map(|method| match method {
                LoginMethods::Token => Some(Element::from(
                    TextInput::new("Token", self.token.as_str())
                        .on_input(|text| MyAppMessage::Login(Message::TokenInput(text))),
                )),
                LoginMethods::Unkown => None,
            })
            .fold(Column::new(), |column, widget| column.push(widget));

        let content = column![
            "Login",
            select_platform,
            auth_input,
            Button::new("Submit").on_press(MyAppMessage::Login(Message::SubmitToken))
        ]
        .width(iced::Length::Fixed(width))
        .align_x(Alignment::Center)
        .spacing(20);

        Container::new(content)
            .height(iced::Length::Fill)
            .width(iced::Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center)
            .into()
    }
}
