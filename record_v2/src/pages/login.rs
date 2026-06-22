use discord::Discord;
use iced::{
    Alignment,
    widget::{Button, Checkbox, Column, ComboBox, Container, TextInput, column, combo_box::State},
};
use messenger_interface::interface::Messenger as NeoMessenger;
use std::{fmt::Display, sync::Arc};
use steam::Steam;
use strum::EnumString;

// TODO: Make adapters handle the functionality of this enum
#[derive(Debug, Clone, EnumString)]
pub enum Platform {
    Discord,
    Steam,
    Test,
}
impl Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Platform::Discord => f.write_str("Discord"),
            Platform::Steam => f.write_str("Steam"),
            Platform::Test => f.write_str("Test"),
        }
    }
}
impl Platform {
    pub fn to_messenger(&self, auth: &str) -> Arc<dyn NeoMessenger> {
        match self {
            Self::Discord => Discord::new_messenger(auth),
            // Steam expects `auth` as "username:password" (see steam::Steam).
            Self::Steam => Steam::new_messenger(auth),
            Self::Test => {
                todo!()
            }
        }
    }
    /// Which input fields to show for this platform. Discord additionally
    /// depends on `use_credentials`: it can authenticate with a single token,
    /// or with a username (email/phone) + password (+ optional MFA code).
    fn get_login_methods(&self, use_credentials: bool) -> Vec<LoginMethods> {
        match self {
            Platform::Discord if use_credentials => vec![
                LoginMethods::Username,
                LoginMethods::Password,
                LoginMethods::GuardCode,
            ],
            Platform::Discord => vec![LoginMethods::Token],
            Platform::Steam => vec![
                LoginMethods::Username,
                LoginMethods::Password,
                LoginMethods::GuardCode,
            ],
            Platform::Test => vec![LoginMethods::Unknown],
        }
    }
}

enum LoginMethods {
    Token,
    Username,
    Password,
    GuardCode,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum Message {
    ToggleButtonState,
    PlatformInput(Platform),
    DiscordUseCredentials(bool),
    TokenInput(String),
    UsernameInput(String),
    PasswordInput(String),
    GuardCodeInput(String),
    SubmitToken,
    /// Credential verification failed; show the error and re-enable submit so
    /// the user can correct and retry. The app does not save or navigate.
    LoginFailed(String),
}

pub enum Action {
    None,
    Login(Arc<dyn NeoMessenger>),
}

#[derive(Debug, Clone)]
pub struct Login {
    platform: State<Platform>,
    selected_platform: Platform,
    token: String,
    username: String,
    password: String,
    guard_code: String,
    /// When set, Discord logs in with username + password instead of a token.
    discord_use_credentials: bool,
    button_state: bool,
    /// Last login error, shown under the form. Cleared on a new submit.
    error: Option<String>,
}
impl Default for Login {
    fn default() -> Self {
        // TODO: Automate addition of new enum variants in here
        let service = State::new(vec![Platform::Discord, Platform::Steam, Platform::Test]);
        Self {
            platform: service,
            selected_platform: Platform::Test,
            token: String::new(),
            username: String::new(),
            password: String::new(),
            guard_code: String::new(),
            discord_use_credentials: false,
            button_state: true,
            error: None,
        }
    }
}

impl Login {
    pub(crate) fn update(&mut self, message: Message) -> Action {
        match message {
            Message::ToggleButtonState => {
                self.button_state = true;
            }
            Message::PlatformInput(platform) => {
                self.selected_platform = platform;
            }
            Message::DiscordUseCredentials(enabled) => {
                self.discord_use_credentials = enabled;
            }
            Message::TokenInput(change) => {
                self.token = change;
            }
            Message::UsernameInput(change) => {
                self.username = change;
            }
            Message::PasswordInput(change) => {
                self.password = change;
            }
            Message::GuardCodeInput(change) => {
                self.guard_code = change;
            }
            Message::LoginFailed(error) => {
                self.error = Some(error);
                self.button_state = true;
            }
            Message::SubmitToken => {
                self.button_state = false;
                self.error = None;
                // Steam always authenticates with username + password plus an
                // optional Steam Guard code. Discord can do either: a username +
                // password (+ optional MFA code) login, or a single token. Every
                // other platform uses a single token.
                let guard_code =
                    (!self.guard_code.trim().is_empty()).then(|| self.guard_code.clone());
                let messenger = match self.selected_platform {
                    Platform::Steam => {
                        Steam::login(&self.username, &self.password, guard_code)
                    }
                    Platform::Discord if self.discord_use_credentials => {
                        Discord::login(&self.username, &self.password, guard_code)
                    }
                    _ => self.selected_platform.to_messenger(&self.token),
                };
                return Action::Login(messenger);
            }
        }

        Action::None
    }

    pub(crate) fn view(&self) -> iced::Element<'_, Message> {
        let width = 360.0;

        let select_platform = ComboBox::new(
            &self.platform,
            "Platform",
            Some(&self.selected_platform),
            Message::PlatformInput,
        );

        let is_discord = matches!(self.selected_platform, Platform::Discord);

        // Field labels differ per platform: Discord's "username" is an email or
        // phone, and its extra code is an MFA (TOTP) code rather than Steam Guard.
        let (username_label, code_label): (&str, &str) = if is_discord {
            ("Email or phone", "Two-factor code (if enabled)")
        } else {
            ("Username", "Steam Guard code (blank = approve on mobile)")
        };

        let auth_input = self
            .selected_platform
            .get_login_methods(self.discord_use_credentials)
            .iter()
            .filter_map(|method| match method {
                LoginMethods::Token => {
                    Some(TextInput::new("Token", self.token.as_str()).on_input(Message::TokenInput))
                }
                LoginMethods::Username => Some(
                    TextInput::new(username_label, self.username.as_str())
                        .on_input(Message::UsernameInput),
                ),
                LoginMethods::Password => Some(
                    TextInput::new("Password", self.password.as_str())
                        .on_input(Message::PasswordInput)
                        .secure(true),
                ),
                LoginMethods::GuardCode => Some(
                    TextInput::new(code_label, self.guard_code.as_str())
                        .on_input(Message::GuardCodeInput),
                ),
                LoginMethods::Unknown => None,
            })
            .fold(Column::new(), |column, widget| column.push(widget));

        // Discord supports both a token login and a username/password login;
        // let the user pick. Other platforms have a single login method.
        let mut content = column!["Login", select_platform];
        if is_discord {
            content = content.push(
                Checkbox::new(self.discord_use_credentials)
                    .label("Log in with username & password")
                    .on_toggle(Message::DiscordUseCredentials),
            );
        }
        let mut content = content.push(auth_input).push(
            Button::new("Submit").on_press_maybe(self.button_state.then_some(Message::SubmitToken)),
        );
        if let Some(error) = &self.error {
            content = content.push(iced::widget::text(error.clone()).style(iced::widget::text::danger));
        }
        let content = content
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
