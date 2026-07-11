use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, LazyLock},
};

use discord::Discord;
use iced::{
    Alignment,
    widget::{Button, ComboBox, Container, Radio, Row, TextInput, column, combo_box::State, text},
};
use messenger_interface::{
    auth::{AuthMethod, Credential, CredentialField},
    interface::Messenger as NeoMessenger,
};
use steam::Steam;

/// The future returned by a backend's async constructor, type-erased so the
/// registry can hold heterogeneous backends behind one fn pointer.
type CreateFuture = Pin<
    Box<
        dyn Future<Output = Result<Arc<dyn NeoMessenger>, Box<dyn std::error::Error + Send + Sync>>>
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
}

static BACKENDS: LazyLock<Vec<Backend>> = LazyLock::new(|| {
    vec![
        Backend {
            name: "Discord",
            auth_methods: Discord::auth_methods,
            create: |cred| Box::pin(Discord::create_messenger(cred)),
        },
        Backend {
            name: "Steam",
            auth_methods: Steam::auth_methods,
            create: |cred| Box::pin(Steam::create_messenger(cred)),
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
    let methods = (backend(name)?.auth_methods)();
    methods.iter().position(|method| !method.hidden)
}

#[derive(Debug, Clone)]
pub enum Message {
    PlatformInput(&'static str),
    MethodInput(usize),
    /// A login field changed; carries the field's key and its new value.
    FieldInput(&'static str, String),
    Submit,
    /// Credential verification failed; show the error and re-enable submit so
    /// the user can correct and retry. The app does not save or navigate.
    LoginFailed(String),
}

pub enum Action {
    None,
    /// The user submitted a login; the app resolves it into a live messenger
    /// (which may require async work, e.g. a Discord token fetch).
    Login {
        backend: &'static str,
        credential: Credential,
    },
}

#[derive(Debug, Clone)]
pub struct Login {
    /// Combo-box state over the backend names.
    platform: State<&'static str>,
    selected_platform: &'static str,
    /// Index into the selected backend's [`AuthMethod`]s.
    selected_method: usize,
    /// Current value of each field the form has collected, keyed by the field's
    /// schema key. Values for fields not in the selected method are simply
    /// ignored on submit, so they survive switching methods.
    values: HashMap<&'static str, String>,
    button_state: bool,
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
            button_state: true,
            error: None,
        }
    }
}

impl Login {
    pub(crate) fn update(&mut self, message: Message) -> Action {
        match message {
            Message::PlatformInput(name) => {
                self.selected_platform = name;
                // Method indices are per-backend; reset to the new backend's
                // first rendered method (0 if it has none).
                self.selected_method = first_visible_method(name).unwrap_or(0);
            }
            Message::MethodInput(index) => self.selected_method = index,
            Message::FieldInput(key, value) => {
                self.values.insert(key, value);
            }
            Message::LoginFailed(error) => {
                self.error = Some(error);
                self.button_state = true;
            }
            Message::Submit => {
                self.button_state = false;
                self.error = None;
                let Some(method) = self.selected_method() else {
                    return Action::None;
                };
                return Action::Login {
                    backend: self.selected_platform,
                    credential: self.build_credential(method),
                };
            }
        }
        Action::None
    }

    /// The currently selected auth method, or `None` when the index doesn't point
    /// at a rendered method (a stale index, or a backend with no visible methods).
    /// Callers render nothing in that case rather than falling back.
    fn selected_method(&self) -> Option<&'static AuthMethod> {
        let methods = (backend(self.selected_platform)?.auth_methods)();
        methods
            .get(self.selected_method)
            .filter(|method| !method.hidden)
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
        let visible: Vec<(usize, &AuthMethod)> = methods
            .iter()
            .enumerate()
            .filter(|(_, method)| !method.hidden)
            .collect();

        // Method picker, only when the backend offers more than one rendered
        // method (e.g. Discord token vs. email & password).
        if visible.len() > 1 {
            let picker = visible
                .iter()
                .fold(Row::new().spacing(10), |row, &(index, method)| {
                    row.push(Radio::new(
                        method.label,
                        index,
                        Some(self.selected_method),
                        Message::MethodInput,
                    ))
                });
            content = content.push(picker);
        }

        // Input fields, generated from the selected method's declared schema.
        if let Some(method) = self.selected_method() {
            for spec in method.fields {
                let value = self.values.get(spec.key).map(String::as_str).unwrap_or("");
                let key = spec.key;
                content = content.push(
                    TextInput::new(spec.label, value)
                        .on_input(move |value| Message::FieldInput(key, value))
                        .secure(spec.secret),
                );
            }
        }

        let mut content = content.push(
            Button::new("Submit").on_press_maybe(self.button_state.then_some(Message::Submit)),
        );
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
