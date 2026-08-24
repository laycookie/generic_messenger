use std::{collections::HashMap, future::Future, pin::Pin, sync::LazyLock};

use discord::Discord;
use iced::{
    Alignment,
    widget::{
        Button, ComboBox, Container, Radio, Row, TextInput, column, combo_box::State, qr_code, text,
    },
};
use messenger_interface::{
    auth::{AuthMethod, Credential, CredentialField, InteractiveAuthMethod, InteractiveAuthPrompt},
    interface::Messenger,
    owner::Owner,
};
#[cfg(feature = "slack")]
use slack::Slack;
#[cfg(feature = "steam")]
use steam::Steam;
#[cfg(feature = "telegram")]
use telegram::Telegram;

/// The future returned by a backend's async constructor, type-erased so the
/// registry can hold heterogeneous backends behind one fn pointer.
type CreateFuture = Pin<
    Box<
        dyn Future<Output = Result<Owner<dyn Messenger>, Box<dyn std::error::Error + Send + Sync>>>
            + Send,
    >,
>;

/// A login-capable backend, described declaratively so the login form and the
/// boot-time session restore are generated rather than hard-coded per platform.
/// Adding a messenger is a matter of adding an entry to [`BACKENDS`].
pub struct Backend {
    /// Display name, also the key persisted in `LoginInfo`.
    pub name: &'static str,
    pub auth_methods: fn() -> &'static [AuthMethod],
    pub create: fn(Credential) -> CreateFuture,
    interactive_auth_methods: &'static [InteractiveAuthMethod],
}

static BACKENDS: LazyLock<Vec<Backend>> = LazyLock::new(|| {
    vec![
        Backend {
            name: "Discord",
            auth_methods: Discord::auth_methods,
            create: |cred| Box::pin(Discord::create_messenger(cred)),
            interactive_auth_methods: Discord::interactive_auth_methods(),
        },
        #[cfg(feature = "steam")]
        Backend {
            name: "Steam",
            auth_methods: Steam::auth_methods,
            create: |cred| Box::pin(Steam::create_messenger(cred)),
            interactive_auth_methods: &[],
        },
        #[cfg(feature = "slack")]
        Backend {
            name: "Slack",
            auth_methods: Slack::auth_methods,
            create: |cred| Box::pin(Slack::create_messenger(cred)),
            interactive_auth_methods: &[],
        },
        #[cfg(feature = "telegram")]
        Backend {
            name: "Telegram",
            auth_methods: Telegram::auth_methods,
            create: |cred| Box::pin(Telegram::create_messenger(cred)),
            interactive_auth_methods: &[],
        },
    ]
});

/// All registered backends, in display order.
pub fn backends() -> &'static [Backend] {
    &BACKENDS
}

/// Look up a backend by its persisted/displayed name.
pub fn backend(name: &str) -> Option<&'static Backend> {
    BACKENDS.iter().find(|backend| backend.name == name)
}

/// Index of a backend's first UI-rendered (non-hidden) method, if it has one.
/// Used to seat the selection on a method the user can actually see.
fn first_visible_method(name: &str) -> Option<usize> {
    let backend = backend(name)?;
    let methods = (backend.auth_methods)();
    methods
        .iter()
        .position(|method| !method.hidden)
        .or_else(|| (!backend.interactive_auth_methods.is_empty()).then_some(methods.len()))
}

enum SelectedLogin {
    Credential(&'static AuthMethod),
    Interactive(InteractiveAuthMethod),
}

#[derive(Debug, Clone)]
pub enum Message {
    PlatformInput(&'static str),
    MethodInput(usize),
    /// A login field changed; carries the field's key and its new value.
    FieldInput(&'static str, String),
    Submit,
    Cancel,
}

pub enum Action {
    None,
    Cancel,
    /// The user submitted a login; the app resolves it into a live messenger
    /// (which may require async work, e.g. a Discord token fetch).
    Login {
        attempt: u64,
        backend: &'static str,
        credential: Credential,
    },
    InteractiveLogin {
        attempt: u64,
        backend: &'static str,
        method: InteractiveAuthMethod,
    },
}

#[derive(Debug)]
pub struct Login {
    /// Combo-box state over the backend names.
    platform: State<&'static str>,
    selected_platform: &'static str,
    /// Selection id: credential methods use their [`AuthMethod`] index;
    /// interactive methods follow the credential-method slice.
    selected_method: usize,
    /// Current value of each field the form has collected, keyed by the field's
    /// schema key. Values for fields not in the selected method are simply
    /// ignored on submit, so they survive switching methods.
    values: HashMap<&'static str, String>,
    next_attempt: u64,
    pending_attempt: Option<u64>,
    qr_code: Option<qr_code::Data>,
    status: Option<&'static str>,
    /// Last login error, shown under the form. Cleared on a new submit.
    error: Option<String>,
}
impl Default for Login {
    fn default() -> Self {
        let names: Vec<&'static str> = backends().iter().map(|backend| backend.name).collect();
        let selected_platform = names.first().copied().unwrap_or_default();
        Self {
            platform: State::new(names),
            selected_platform,
            selected_method: first_visible_method(selected_platform).unwrap_or(0),
            values: HashMap::new(),
            next_attempt: 0,
            pending_attempt: None,
            qr_code: None,
            status: None,
            error: None,
        }
    }
}

impl Login {
    pub(crate) fn update(&mut self, message: Message) -> Action {
        match message {
            Message::PlatformInput(name) => {
                if name == self.selected_platform {
                    return Action::None;
                }
                let canceled = self.cancel_attempt();
                self.selected_platform = name;
                // Method indices are per-backend; reset to the new backend's
                // first rendered method (0 if it has none).
                self.selected_method = first_visible_method(name).unwrap_or(0);
                if canceled {
                    return Action::Cancel;
                }
            }
            Message::MethodInput(index) => {
                if index == self.selected_method {
                    return Action::None;
                }
                let canceled = self.cancel_attempt();
                self.selected_method = index;
                self.error = None;
                if canceled {
                    return Action::Cancel;
                }
            }
            Message::FieldInput(key, value) => {
                self.values.insert(key, value);
            }
            Message::Cancel => {
                if self.cancel_attempt() {
                    return Action::Cancel;
                }
            }
            Message::Submit => {
                if self.pending_attempt.is_some() {
                    return Action::None;
                }
                self.error = None;
                self.qr_code = None;
                let Some(selected) = self.selected_login() else {
                    return Action::None;
                };
                let attempt = self.next_attempt;
                self.next_attempt = self.next_attempt.wrapping_add(1);
                self.pending_attempt = Some(attempt);
                match selected {
                    SelectedLogin::Interactive(method) => {
                        self.status = Some("Starting interactive login…");
                        return Action::InteractiveLogin {
                            attempt,
                            backend: self.selected_platform,
                            method,
                        };
                    }
                    SelectedLogin::Credential(method) => {
                        self.status = Some("Signing in…");
                        return Action::Login {
                            attempt,
                            backend: self.selected_platform,
                            credential: self.build_credential(method),
                        };
                    }
                }
            }
        }
        Action::None
    }

    fn selected_login(&self) -> Option<SelectedLogin> {
        let backend = backend(self.selected_platform)?;
        let methods = (backend.auth_methods)();
        if let Some(method) = methods
            .get(self.selected_method)
            .filter(|method| !method.hidden)
        {
            return Some(SelectedLogin::Credential(method));
        }
        let interactive_index = self.selected_method.checked_sub(methods.len())?;
        backend
            .interactive_auth_methods
            .get(interactive_index)
            .copied()
            .map(SelectedLogin::Interactive)
    }

    fn cancel_attempt(&mut self) -> bool {
        let canceled = self.pending_attempt.take().is_some();
        self.qr_code = None;
        self.status = None;
        canceled
    }

    pub(crate) fn is_pending(&self, attempt: u64) -> bool {
        self.pending_attempt == Some(attempt)
    }

    pub(crate) fn show_interactive_prompt(
        &mut self,
        attempt: u64,
        prompt: InteractiveAuthPrompt,
    ) -> Result<bool, String> {
        if !self.is_pending(attempt) {
            return Ok(false);
        }
        match prompt {
            InteractiveAuthPrompt::QrCode { payload } => {
                self.qr_code =
                    Some(qr_code::Data::new(payload.as_str()).map_err(|error| error.to_string())?);
                self.status = Some("Scan QR code, then approve login.");
            }
        }
        Ok(true)
    }

    pub(crate) fn fail(&mut self, attempt: u64, error: String) -> bool {
        if !self.is_pending(attempt) {
            return false;
        }
        self.pending_attempt = None;
        self.qr_code = None;
        self.status = None;
        self.error = Some(error);
        true
    }

    pub(crate) fn finish(&mut self, attempt: u64) -> bool {
        if !self.is_pending(attempt) {
            return false;
        }
        self.pending_attempt = None;
        self.qr_code = None;
        self.status = None;
        self.error = None;
        true
    }

    /// Assemble the [`Credential`] the selected method declares, pulling each
    /// declared field's value from the form. Blank optional fields are dropped so
    /// the backend sees only what the user actually supplied.
    fn build_credential(&self, method: &AuthMethod) -> Credential {
        let fields = method
            .fields
            .iter()
            .filter_map(|spec| {
                let value = self.values.get(spec.key).cloned().unwrap_or_default();
                if spec.optional && value.trim().is_empty() {
                    return None;
                }
                Some(CredentialField::new(spec.key, value))
            })
            .collect();
        Credential(fields)
    }

    pub(crate) fn view(&self) -> iced::Element<'_, Message> {
        let width = 360.0;

        let select_platform = ComboBox::new(
            &self.platform,
            "Platform",
            Some(&self.selected_platform),
            Message::PlatformInput,
        );

        let mut content = column!["Login", select_platform];

        let methods = backend(self.selected_platform)
            .map(|backend| (backend.auth_methods)())
            .unwrap_or(&[]);

        // The methods the UI actually renders, each paired with its real index so
        // a hidden method interleaved in the list doesn't shift the selection.
        let mut visible: Vec<(usize, &'static str)> = methods
            .iter()
            .enumerate()
            .filter(|(_, method)| !method.hidden)
            .map(|(index, method)| (index, method.label))
            .collect();
        if let Some(backend) = backend(self.selected_platform) {
            visible.extend(
                backend
                    .interactive_auth_methods
                    .iter()
                    .enumerate()
                    .map(|(index, method)| (methods.len() + index, method.label)),
            );
        }
        let selected_login = self.selected_login();
        let interactive_selected = matches!(&selected_login, Some(SelectedLogin::Interactive(_)));

        // Method picker, only when backend offers multiple rendered methods.
        if visible.len() > 1 {
            let picker = visible
                .iter()
                .fold(Row::new().spacing(10), |row, &(index, label)| {
                    row.push(Radio::new(
                        label,
                        index,
                        Some(self.selected_method),
                        Message::MethodInput,
                    ))
                });
            content = content.push(picker);
        }

        // Input fields, generated from the selected method's declared schema.
        if let Some(SelectedLogin::Credential(method)) = selected_login {
            for spec in method.fields {
                let value = self.values.get(spec.key).map(String::as_str).unwrap_or("");
                let key = spec.key;
                content = content.push(
                    TextInput::new(spec.label, value)
                        .on_input_maybe(
                            self.pending_attempt
                                .is_none()
                                .then_some(move |value| Message::FieldInput(key, value)),
                        )
                        .secure(spec.secret),
                );
            }
        }

        if let Some(data) = &self.qr_code {
            content = content.push(qr_code(data).total_size(220.0));
        }
        if let Some(status) = self.status {
            content = content.push(text(status));
        }

        let button = if self.pending_attempt.is_some() {
            Button::new("Cancel").on_press(Message::Cancel)
        } else {
            let label = if interactive_selected {
                "Continue"
            } else {
                "Submit"
            };
            Button::new(label).on_press(Message::Submit)
        };
        let mut content = content.push(button);
        if let Some(error) = &self.error {
            content = content.push(text(error.clone()).style(iced::widget::text::danger));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_login_can_be_started_shown_and_canceled() {
        let mut login = Login::default();
        let qr_method = Discord::auth_methods().len();

        assert!(matches!(
            login.update(Message::MethodInput(qr_method)),
            Action::None
        ));
        let attempt = match login.update(Message::Submit) {
            Action::InteractiveLogin {
                attempt, backend, ..
            } => {
                assert_eq!(backend, "Discord");
                attempt
            }
            _ => panic!("QR method did not start interactive login"),
        };

        assert!(login.is_pending(attempt));
        assert!(
            login
                .show_interactive_prompt(
                    attempt,
                    InteractiveAuthPrompt::QrCode {
                        payload: messenger_interface::auth::QrPayload::new(
                            "https://discord.com/ra/test-fingerprint",
                        ),
                    },
                )
                .unwrap()
        );
        assert!(matches!(login.update(Message::Cancel), Action::Cancel));
        assert!(!login.is_pending(attempt));
        assert!(
            !login
                .show_interactive_prompt(
                    attempt,
                    InteractiveAuthPrompt::QrCode {
                        payload: messenger_interface::auth::QrPayload::new(
                            "https://discord.com/ra/stale",
                        ),
                    },
                )
                .unwrap()
        );
    }
}
