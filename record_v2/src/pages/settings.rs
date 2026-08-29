use iced::{
    Alignment, Background, Color, Element, Length,
    widget::{Button, Column, Radio, Scrollable, Space, button, column, container, row, text},
};
use login_store::{StorageMode, StorageStatus};
use messenger_interface::interface::VideoCodec;
use messenger_unifier::MessengerId;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Category {
    #[default]
    Messengers,
    PrivacySecurity,
    AudioVideo,
}

impl Category {
    fn label(self) -> &'static str {
        match self {
            Self::Messengers => "Messengers",
            Self::PrivacySecurity => "Privacy & Security",
            Self::AudioVideo => "Audio & Video",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Message {
    Back,
    SelectCategory(Category),
    SignOut(MessengerId),
    RemoveSavedAccount(u64),
    RestartInput,
    RestartOutput,
    RedetectAudio,
    SetVideoCodec(VideoCodec),
    ResetVideo,
    RetryMessengerData,
    SetCredentialStorage(StorageMode),
    ClearError,
    ClearCredentialError,
}

pub struct LoggedInAccount {
    pub id: MessengerId,
    pub backend: String,
    pub display_name: Option<String>,
    pub tag: Option<String>,
}

pub struct UnavailableAccount {
    pub id: u64,
    pub backend: String,
    pub error_code: &'static str,
    pub error: String,
}

pub struct Status<'a> {
    pub selected_category: Category,
    pub logged_in_accounts: Vec<LoggedInAccount>,
    pub unavailable_accounts: Vec<UnavailableAccount>,
    pub credential_store_writable: bool,
    pub credential_storage: Option<&'a StorageStatus>,
    pub plaintext_storage_supported: bool,
    pub credential_error: Option<&'a str>,
    pub input_streaming: bool,
    pub output_streaming: bool,
    pub active_remote_video: usize,
    pub configured_video_inputs: usize,
    pub preferred_video_codec: VideoCodec,
    pub media_error: Option<&'a str>,
}

fn logged_in_account(
    account: LoggedInAccount,
    credential_store_writable: bool,
) -> Element<'static, Message> {
    let identity = match (account.display_name, account.tag) {
        (Some(name), Some(tag)) if name != tag => format!("{name} (@{tag})"),
        (Some(name), _) => name,
        (None, Some(tag)) => format!("@{tag}"),
        (None, None) => "Profile not loaded".to_owned(),
    };
    let mut sign_out = Button::new("Sign out");
    if credential_store_writable {
        sign_out = sign_out.on_press(Message::SignOut(account.id));
    }

    container(
        row![
            column![
                text(account.backend).size(16),
                text(identity)
                    .size(13)
                    .color(Color::from_rgb(0.55, 0.55, 0.6)),
            ]
            .spacing(4)
            .width(Length::Fill),
            sign_out,
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(12)
    .width(Length::Fill)
    .style(container::rounded_box)
    .into()
}

fn unavailable_account(
    account: UnavailableAccount,
    credential_store_writable: bool,
) -> Element<'static, Message> {
    let mut remove = Button::new("Remove saved account");
    if credential_store_writable {
        remove = remove.on_press(Message::RemoveSavedAccount(account.id));
    }

    container(
        row![
            column![
                text(account.backend).size(16),
                text(format!("Error code: {}", account.error_code))
                    .size(12)
                    .color(Color::from_rgb(0.8, 0.25, 0.25)),
                text(account.error).size(13),
            ]
            .spacing(4)
            .width(Length::Fill),
            remove,
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(12)
    .width(Length::Fill)
    .style(container::rounded_box)
    .into()
}

fn category_button(category: Category, selected_category: Category) -> Button<'static, Message> {
    let control = Button::new(text(category.label()).size(15))
        .padding([10, 12])
        .width(Length::Fill)
        .on_press(Message::SelectCategory(category));

    if category == selected_category {
        control.style(button::primary)
    } else {
        control.style(button::secondary)
    }
}

fn credential_security_warning<'a>(
    storage: Option<&'a StorageStatus>,
) -> Option<Element<'a, Message>> {
    let storage = storage.filter(|status| status.mode() == StorageMode::Plaintext)?;
    let (heading, summary) = if storage.is_automatic_fallback() {
        (
            "Security warning: OS secret storage failed",
            "Saved login secrets use an owner-only plaintext file because the OS secrets manager could not be used.",
        )
    } else {
        (
            "Security warning: plaintext credential storage enabled",
            "Saved login secrets are stored directly in an owner-only plaintext file.",
        )
    };
    let mut content = column![text(heading).size(17), text(summary).size(13)].spacing(6);
    if let Some(error) = storage.vault_error()
        && storage.is_automatic_fallback()
    {
        content = content.push(text(format!("OS secrets manager error: {error}")).size(12));
    }
    if let Some(warning) = storage.warning() {
        content = content.push(text(format!("Credential storage warning: {warning}")).size(12));
    }
    content = content.push(
        Button::new("Review Privacy & Security")
            .on_press(Message::SelectCategory(Category::PrivacySecurity)),
    );

    Some(
        container(content)
            .padding(14)
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Background::Color(Color::from_rgb(1.0, 0.82, 0.45))),
                text_color: Some(Color::from_rgb(0.18, 0.11, 0.02)),
                ..Default::default()
            })
            .into(),
    )
}

fn privacy_security_category<'a>(
    storage: Option<&'a StorageStatus>,
    credential_store_writable: bool,
    plaintext_storage_supported: bool,
    credential_error: Option<&'a str>,
) -> Element<'a, Message> {
    let mode = storage.map(StorageStatus::mode);
    let current = match storage {
        Some(status) if status.mode() == StorageMode::SystemVault => {
            if status.vault_error().is_some() {
                "OS secrets manager (currently unavailable)"
            } else {
                "OS secrets manager"
            }
        }
        Some(status) if status.is_automatic_fallback() => "Plaintext fallback",
        Some(_) => "Plaintext file (selected by you)",
        None => "Unknown — credential store could not be read safely",
    };

    let mut use_system = Button::new(if mode == Some(StorageMode::SystemVault) {
        "Retry OS secrets manager"
    } else {
        "Use OS secrets manager"
    });
    if credential_store_writable
        && (mode != Some(StorageMode::SystemVault)
            || storage.is_some_and(|status| status.vault_error().is_some())
            || credential_error.is_some())
    {
        use_system = use_system.on_press(Message::SetCredentialStorage(StorageMode::SystemVault));
    }

    let plaintext_label = if storage.is_some_and(StorageStatus::is_automatic_fallback) {
        "Keep plaintext by choice"
    } else {
        "Use plaintext file"
    };
    let mut use_plaintext = Button::new(plaintext_label);
    if credential_store_writable
        && plaintext_storage_supported
        && (mode != Some(StorageMode::Plaintext)
            || storage.is_some_and(StorageStatus::is_automatic_fallback))
    {
        use_plaintext =
            use_plaintext.on_press(Message::SetCredentialStorage(StorageMode::Plaintext));
    }

    let system = container(
        column![
            text("OS secrets manager (recommended)").size(17),
            text("Secrets are kept by Secret Service, Keychain, or Credential Manager. LoginInfo stores only opaque references.")
                .size(13),
            use_system,
        ]
        .spacing(8),
    )
    .padding(12)
    .width(Length::Fill)
    .style(container::rounded_box);

    let plaintext_description = if plaintext_storage_supported {
        "Secrets are written directly to LoginInfo with owner-only (0600) permissions. Other programs running as your user may still read them."
    } else {
        "Plaintext storage is disabled on this platform because owner-only file protection cannot be enforced safely."
    };
    let plaintext = container(
        column![
            text("Plaintext file").size(17),
            text(plaintext_description).size(13),
            use_plaintext,
        ]
        .spacing(8),
    )
    .padding(12)
    .width(Length::Fill)
    .style(container::rounded_box);

    let mut page = column![
        text("Privacy & Security").size(22),
        text(format!("Current credential storage: {current}")),
        text("Changing storage rewrites all saved account credentials transactionally."),
        system,
        plaintext,
    ]
    .spacing(12)
    .width(Length::Fill);

    if let Some(error) = storage.and_then(StorageStatus::vault_error) {
        page = page.push(
            container(
                column![
                    text("OS secrets manager details").size(16),
                    text(error).size(13),
                ]
                .spacing(6),
            )
            .padding(12)
            .width(Length::Fill),
        );
    }
    if let Some(warning) = storage.and_then(StorageStatus::warning) {
        page = page.push(
            container(
                column![
                    text("Credential storage warning").size(16),
                    text(warning).size(13),
                ]
                .spacing(6),
            )
            .padding(12)
            .width(Length::Fill),
        );
    }
    if let Some(error) = credential_error {
        page = page.push(
            container(
                column![
                    text("Last credential-store error").size(16),
                    text(error),
                    Button::new("Dismiss error").on_press(Message::ClearCredentialError),
                ]
                .spacing(6),
            )
            .padding(12)
            .width(Length::Fill),
        );
    }
    if !credential_store_writable {
        page = page.push(
            text("Storage controls are disabled because LoginInfo was not read completely or changed in another app process. Restore OS secrets-manager access or close the other process, then restart to retry safely.")
                .size(13)
                .color(Color::from_rgb(0.8, 0.4, 0.2)),
        );
    }

    page.into()
}

fn messengers_category<'a>(
    logged_in_accounts: Vec<LoggedInAccount>,
    unavailable_accounts: Vec<UnavailableAccount>,
    credential_store_writable: bool,
    credential_error: Option<&'a str>,
) -> Element<'a, Message> {
    let mut logged_in = Column::new().spacing(8).width(Length::Fill);
    if logged_in_accounts.is_empty() {
        logged_in = logged_in.push(text("No active accounts."));
    } else {
        for account in logged_in_accounts {
            logged_in = logged_in.push(logged_in_account(account, credential_store_writable));
        }
    }

    let mut accounts = column![
        text("Accounts").size(22),
        text("Active sessions and saved accounts that could not be loaded."),
        text("Logged in").size(16),
        logged_in,
    ]
    .spacing(10);

    if !credential_store_writable {
        accounts = accounts.push(
            text("Account changes are disabled because LoginInfo could not be read safely.")
                .size(13)
                .color(Color::from_rgb(0.8, 0.4, 0.2)),
        );
    }

    if !unavailable_accounts.is_empty() {
        let unavailable = Column::from_iter(
            unavailable_accounts
                .into_iter()
                .map(|account| unavailable_account(account, credential_store_writable)),
        )
        .spacing(8)
        .width(Length::Fill);
        accounts = accounts
            .push(text("Unavailable saved accounts").size(16))
            .push(
                text("Preserved entries that failed credential parsing or session restoration.")
                    .size(13),
            )
            .push(unavailable);
    }

    if let Some(error) = credential_error {
        accounts = accounts.push(
            container(column![
                text("Last credential-store error").size(16),
                text(error),
                Button::new("Dismiss error").on_press(Message::ClearCredentialError),
            ])
            .padding(12),
        );
    }

    let data = column![
        text("Messenger data").size(22),
        text("Re-fetch profiles, contacts, rooms, and servers; live events remain buffered."),
        Button::new("Retry messenger data").on_press(Message::RetryMessengerData),
    ]
    .spacing(10);

    column![accounts, data]
        .spacing(28)
        .width(Length::Fill)
        .into()
}

fn audio_video_category<'a>(
    input_streaming: bool,
    output_streaming: bool,
    active_remote_video: usize,
    configured_video_inputs: usize,
    preferred_video_codec: VideoCodec,
    media_error: Option<&'a str>,
) -> Element<'a, Message> {
    let input_state = if input_streaming {
        "running"
    } else {
        "stopped"
    };
    let output_state = if output_streaming {
        "running"
    } else {
        "stopped"
    };
    let mut audio = column![
        text("Audio").size(22),
        text(format!(
            "Microphone: {}    Playback: {}",
            input_state, output_state
        )),
        row![
            Button::new("Restart microphone").on_press(Message::RestartInput),
            Button::new("Restart playback").on_press(Message::RestartOutput),
            Button::new("Re-detect audio devices").on_press(Message::RedetectAudio),
        ]
        .spacing(8),
    ]
    .spacing(10);

    if let Some(error) = media_error {
        audio = audio.push(
            container(column![
                text("Last recoverable error").size(16),
                text(error),
                Button::new("Dismiss error").on_press(Message::ClearError),
            ])
            .padding(12),
        );
    }

    let video = column![
        text("Video").size(22),
        text(format!(
            "Remote streams: {}    Configured local inputs: {}",
            active_remote_video, configured_video_inputs
        )),
        text("Outgoing codec"),
        row![
            Radio::new(
                "H.264",
                VideoCodec::H264,
                Some(preferred_video_codec),
                Message::SetVideoCodec,
            ),
            Radio::new(
                "AV1",
                VideoCodec::Av1,
                Some(preferred_video_codec),
                Message::SetVideoCodec,
            ),
        ]
        .spacing(12),
        text("H.264 favors compatibility; AV1 saves bandwidth but uses more CPU.").size(12),
        text("Reset stops outgoing camera/screen broadcasts and drops stale previews."),
        Button::new("Reset video state").on_press(Message::ResetVideo),
    ]
    .spacing(10);

    column![audio, video].spacing(28).width(Length::Fill).into()
}

pub fn view<'a>(status: Status<'a>) -> Element<'a, Message> {
    let Status {
        selected_category,
        logged_in_accounts,
        unavailable_accounts,
        credential_store_writable,
        credential_storage,
        plaintext_storage_supported,
        credential_error,
        input_streaming,
        output_streaming,
        active_remote_video,
        configured_video_inputs,
        preferred_video_codec,
        media_error,
    } = status;

    let title = row![
        text("Settings").size(28),
        Space::new().width(Length::Fill),
        Button::new("Back").on_press(Message::Back),
    ]
    .align_y(Alignment::Center);

    let navigation = container(
        column![
            text("Categories").size(13),
            category_button(Category::Messengers, selected_category),
            category_button(Category::PrivacySecurity, selected_category),
            category_button(Category::AudioVideo, selected_category),
        ]
        .spacing(6)
        .width(Length::Fill),
    )
    .padding(8)
    .width(Length::Fixed(200.0))
    .height(Length::Fill)
    .style(container::rounded_box);

    let page = match selected_category {
        Category::Messengers => messengers_category(
            logged_in_accounts,
            unavailable_accounts,
            credential_store_writable,
            credential_error,
        ),
        Category::PrivacySecurity => privacy_security_category(
            credential_storage,
            credential_store_writable,
            plaintext_storage_supported,
            credential_error,
        ),
        Category::AudioVideo => audio_video_category(
            input_streaming,
            output_streaming,
            active_remote_video,
            configured_video_inputs,
            preferred_video_codec,
            media_error,
        ),
    };

    let body = row![
        navigation,
        Scrollable::new(page)
            .width(Length::Fill)
            .height(Length::Fill),
    ]
    .spacing(24)
    .width(Length::Fill)
    .height(Length::Fill);

    let mut content = column![title].spacing(18).width(Length::Fill);
    if let Some(warning) = credential_security_warning(credential_storage) {
        content = content.push(warning);
    }
    content = content.push(body).height(Length::Fill);

    container(content)
        .padding(24)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
