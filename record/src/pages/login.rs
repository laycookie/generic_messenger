use audio::AudioMixer;
use discord::Discord;
use iced::{
    Alignment,
    widget::{Button, Column, ComboBox, Container, TextInput, column, combo_box::State},
};
use messaging_interface::interface::Messanger as NeoMessanger;
use std::{
    fmt::Display,
    sync::{Arc, Mutex},
};
use strum::EnumString;

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
    pub fn to_messanger(
        &self,
        auth: &str,
        audio_mixer: &Arc<Mutex<AudioMixer>>,
    ) -> Arc<dyn NeoMessanger> {
        Arc::new(match self {
            // Self::Discord => Discord::new(&auth, audio_mixer.clone()),
            Self::Discord => Discord::new(&auth, audio_mixer.clone()),
            Self::Test => {
                todo!()
            }
        })
    }
    fn get_login_methods(&self) -> Vec<LoginMethods> {
        match self {
            Platform::Discord => vec![LoginMethods::Token],
            Platform::Test => vec![LoginMethods::Unknown],
        }
    }
}
// ===

enum LoginMethods {
    Token,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum Message {
    ToggleButtonState,
    PlatformInput(Platform),
    TokenInput(String),
    SubmitToken,
}

pub enum Action {
    None,
    Login(Arc<dyn NeoMessanger>),
}

#[derive(Debug, Clone)]
pub struct Login {
    platform: State<Platform>,
    selected_platform: Platform,
    token: String,
    // TODO: Display error
    _error: Option<String>,
    button_state: bool,
}
impl Default for Login {
    fn default() -> Self {
        // TODO: Automate addition of new enum variants in here
        let service = State::new(vec![Platform::Discord, Platform::Test]);
        Self {
            platform: service,
            selected_platform: Platform::Test,
            token: String::new(),
            _error: None,
            button_state: true,
        }
    }
}

impl Login {
    pub(crate) fn update(
        &mut self,
        message: Message,
        audio_mixer: &Arc<Mutex<AudioMixer>>,
    ) -> Action {
        match message {
            Message::ToggleButtonState => {
                self.button_state = true;
            }
            Message::PlatformInput(platform) => {
                self.selected_platform = platform;
            }
            Message::TokenInput(change) => {
                self.token = change;
            }
            Message::SubmitToken => {
                // TODO: Disable submit button until the operation ether
                self.button_state = false;
                let platform = self.selected_platform.clone();
                let token = self.token.clone();

                let messanger = platform.to_messanger(&token, audio_mixer);
                return Action::Login(messanger);
            }
        }

        Action::None
    }

    pub(crate) fn view<'a>(&'a self) -> iced::Element<'a, Message> {
        let width = 360.0;

        let select_platform = ComboBox::new(
            &self.platform,
            "Platform",
            Some(&self.selected_platform),
            |platform| Message::PlatformInput(platform),
        );

        let auth_input = self
            .selected_platform
            .get_login_methods()
            .iter()
            .filter_map(|method| match method {
                LoginMethods::Token => Some(
                    TextInput::new("Token", self.token.as_str())
                        .on_input(|text| Message::TokenInput(text)),
                ),
                LoginMethods::Unknown => None,
            })
            .fold(Column::new(), |column, widget| column.push(widget));

        let content = column![
            "Login",
            select_platform,
            auth_input,
            Button::new("Submit").on_press_maybe(self.button_state.then_some(Message::SubmitToken))
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
