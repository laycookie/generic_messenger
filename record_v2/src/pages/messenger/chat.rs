use iced::{
    Element, Length, Task,
    widget::{Button, Column, Scrollable, Text, TextInput, column, image, row, text::LineHeight},
};
use iced_aw::Wrap;
use messenger_interface::types::{
    Emoji, ID, Identifier, Place, RichText, Room, RoomCapabilities, Span, Sticker, TextStyle, User,
};
use tracing::debug;

use crate::components::message_text::message_text;
use messenger_unifier::{MessengerId, MessengerRegistry};

/// Cap on emoji shown at once in the picker grid; the full Unicode set is
/// large enough to make layout expensive. Search narrows the result set.
const MAX_PICKER_RESULTS: usize = 200;
/// Height of the picker panel between the chat and the compose box.
const PICKER_HEIGHT: f32 = 220.0;
const PICKER_EMOJI_SIZE: f32 = 22.0;
const PICKER_STICKER_SIZE: f32 = 80.0;

/// The Unicode emoji groups, labelled by a representative emoji.
const UNICODE_GROUPS: &[(&str, emojis::Group)] = &[
    ("😀", emojis::Group::SmileysAndEmotion),
    ("👋", emojis::Group::PeopleAndBody),
    ("🐻", emojis::Group::AnimalsAndNature),
    ("🍔", emojis::Group::FoodAndDrink),
    ("✈", emojis::Group::TravelAndPlaces),
    ("⚽", emojis::Group::Activities),
    ("💡", emojis::Group::Objects),
    ("🔣", emojis::Group::Symbols),
    ("🚩", emojis::Group::Flags),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerTab {
    /// Unicode emoji from the static [`emojis`] database.
    #[default]
    Unicode,
    /// The messenger's custom emoji for this room (`pickable_emojis`).
    Custom,
    /// The messenger's stickers for this room (`pickable_stickers`).
    Stickers,
}

/// Where the picked emoji lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PickerTarget {
    /// Insert into the compose box; stickers send immediately.
    #[default]
    Compose,
    /// React to this message. Stickers cannot react, so that tab is hidden.
    React(ID),
}

/// Open picker panel state. Closed = `Chat::picker` is `None`.
#[derive(Debug, Clone, Default)]
struct Picker {
    tab: PickerTab,
    search: String,
    /// Selected [`UNICODE_GROUPS`] index for Unicode browsing.
    group: usize,
    target: PickerTarget,
}

impl Picker {
    fn reacting_to(message_id: ID) -> Self {
        Picker {
            target: PickerTarget::React(message_id),
            ..Picker::default()
        }
    }
}

#[derive(Clone)]
pub struct Chat {
    pub messenger: MessengerId,
    pub room: Identifier<Place<Room>>,
    msg_box: String,
    /// When set, the compose box is editing this message.
    editing: Option<ID>,
    /// Emoji/sticker picker panel; `None` while closed.
    picker: Option<Picker>,
    /// Custom emoji sendable in this room. `None` until the first picker open.
    custom_emojis: Option<Vec<Emoji>>,
    /// Stickers sendable in this room; loaded alongside `custom_emojis`.
    stickers: Option<Vec<Sticker>>,
    /// Bound widget construction for long-lived rooms. Users can expand the
    /// window in chunks without paying to rebuild every loaded message on
    /// every keystroke or media frame.
    visible_message_limit: usize,
}

const INITIAL_VISIBLE_MESSAGES: usize = 200;
const VISIBLE_MESSAGE_CHUNK: usize = 200;

#[derive(Debug, Clone)]
pub enum Message {
    MsgInput(String),
    MsgSend,
    ToggleReaction {
        message_id: ID,
        emoji: String,
        reacted: bool,
    },
    /// Begin editing a message: load its current text into the compose box.
    StartEdit {
        message_id: ID,
        current: String,
    },
    CancelEdit,
    Delete {
        message_id: ID,
    },
    /// Open the inspect sheet for a user.
    InspectUser(Identifier<User>),
    /// Open/close the emoji and sticker picker panel.
    TogglePicker,
    /// Open the emoji picker to react to a message.
    StartReact {
        message_id: ID,
    },
    PickerTab(PickerTab),
    PickerSearch(String),
    /// Select a Unicode group (index into [`UNICODE_GROUPS`]).
    PickerGroup(usize),
    /// An emoji was picked.
    PickEmoji(String),
    /// Send a sticker immediately as its own message.
    SendSticker(Sticker),
    /// The async palette fetch finished.
    PaletteLoaded {
        emojis: Vec<Emoji>,
        stickers: Vec<Sticker>,
    },
    ShowOlder,
    OpenLink(String),
}

#[derive(Clone)]
pub enum Action {
    Call {
        messenger: MessengerId,
        room: Identifier<Place<Room>>,
    },
    Message(Message),
}

pub enum UpdateResult {
    Task(Task<Message>),
    /// User wants to send a message. The parent handles optimistic insert + async send.
    Send {
        messenger: MessengerId,
        room: Identifier<Place<Room>>,
        contents: RichText,
    },
    /// User confirmed an edit; the parent fires the async edit.
    Edit {
        messenger: MessengerId,
        room: Identifier<Place<Room>>,
        message_id: ID,
        contents: RichText,
    },
    /// User wants a message gone; the parent fires the async delete.
    Delete {
        messenger: MessengerId,
        room: Identifier<Place<Room>>,
        message_id: ID,
    },
    /// User clicked a reaction button; toggle it on the server.
    ToggleReaction {
        messenger: MessengerId,
        room: Identifier<Place<Room>>,
        message_id: ID,
        emoji: String,
        reacted: bool,
    },
    /// User clicked a message author; open the inspect sheet.
    InspectUser {
        messenger: MessengerId,
        user: Identifier<User>,
    },
}

/// Resolve the compose box's plain string into rich spans. Every
/// `:shortcode:` token matching the room's custom-emoji palette becomes an
/// emoji span; everything else stays text.
fn resolve_compose(text: &str, palette: &[Emoji]) -> RichText {
    if palette.is_empty() {
        return RichText::plain(text);
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        let after = &rest[start + 1..];
        let matched = after.find(':').and_then(|end| {
            let token = &after[..end];
            let valid =
                !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !valid {
                return None;
            }
            palette
                .iter()
                .find(|emoji| emoji.shortcode == token)
                .map(|emoji| (end, emoji.clone()))
        });
        match matched {
            Some((end, emoji)) => {
                plain.push_str(&rest[..start]);
                if !plain.is_empty() {
                    spans.push(Span::Text {
                        text: std::mem::take(&mut plain),
                        style: TextStyle::default(),
                    });
                }
                spans.push(Span::Emoji(emoji));
                rest = &rest[start + 1 + end + 1..];
            }
            None => {
                // Not a known shortcode: keep the colon as text and continue
                // right after it.
                plain.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    plain.push_str(rest);
    if !plain.is_empty() {
        spans.push(Span::Text {
            text: plain,
            style: TextStyle::default(),
        });
    }
    RichText { spans }
}

impl Chat {
    pub fn new(messenger: MessengerId, room: Identifier<Place<Room>>) -> Self {
        Self {
            messenger,
            room,
            msg_box: String::new(),
            editing: None,
            picker: None,
            custom_emojis: None,
            stickers: None,
            visible_message_limit: INITIAL_VISIBLE_MESSAGES,
        }
    }

    /// Fetch this room's custom emoji and stickers from the backend.
    fn load_palette(&self, messengers: &MessengerRegistry) -> Task<Message> {
        let room = self.room.clone();
        let Some(palette) = messengers.with_interface_async(
            self.messenger,
            async move |interface| match interface.messenger().text() {
                Ok(text) => {
                    let (emojis, stickers) = futures::join!(
                        text.pickable_emojis(&room),
                        text.pickable_stickers(&room)
                    );
                    (
                        emojis.unwrap_or_else(|e| {
                            debug!("No custom emoji palette: {e}");
                            Vec::new()
                        }),
                        stickers.unwrap_or_else(|e| {
                            debug!("No sticker palette: {e}");
                            Vec::new()
                        }),
                    )
                }
                Err(_) => (Vec::new(), Vec::new()),
            },
        ) else {
            return Task::none();
        };
        Task::future(async move {
            let (emojis, stickers) = palette.await.unwrap_or_default();
            Message::PaletteLoaded { emojis, stickers }
        })
    }

    /// The Unicode-emoji grid: the selected group, or a search across the
    /// whole database.
    fn unicode_grid<'a>(&'a self, picker: &'a Picker) -> Element<'a, Action> {
        let query = picker.search.trim().to_lowercase();
        let matches: Box<dyn Iterator<Item = &'static emojis::Emoji>> = if query.is_empty() {
            Box::new(UNICODE_GROUPS[picker.group].1.emojis())
        } else {
            Box::new(emojis::iter().filter(move |emoji| {
                emoji.name().contains(&query)
                    || emoji.shortcode().is_some_and(|s| s.contains(&query))
            }))
        };
        let buttons = matches
            .take(MAX_PICKER_RESULTS)
            .map(|emoji| {
                Button::new(Text::new(emoji.as_str()).size(PICKER_EMOJI_SIZE))
                    .on_press(Action::Message(Message::PickEmoji(
                        emoji.as_str().to_owned(),
                    )))
                    .into()
            })
            .collect();
        Wrap::with_elements(buttons)
            .spacing(2.0)
            .line_spacing(2.0)
            .into()
    }

    /// The custom-emoji grid for this room's messenger palette.
    fn custom_grid<'a>(&'a self, picker: &'a Picker) -> Element<'a, Action> {
        let Some(palette) = &self.custom_emojis else {
            return Text::new("Loading...").into();
        };
        if palette.is_empty() {
            return Text::new("No custom emoji here.").into();
        }
        let query = picker.search.trim().to_lowercase();
        let buttons = palette
            .iter()
            .filter(|emoji| query.is_empty() || emoji.shortcode.to_lowercase().contains(&query))
            .take(MAX_PICKER_RESULTS)
            .map(|emoji| {
                let face: Element<'a, Action> = match &emoji.image {
                    Some(path) => image(path)
                        .width(Length::Fixed(PICKER_EMOJI_SIZE))
                        .height(Length::Fixed(PICKER_EMOJI_SIZE))
                        .into(),
                    None => Text::new(format!(":{}:", emoji.shortcode)).into(),
                };
                // Composing inserts the token the compose resolver understands;
                // reacting wants the bare shortcode stored in reactions.
                let picked = match picker.target {
                    PickerTarget::Compose => format!(":{}:", emoji.shortcode),
                    PickerTarget::React(_) => emoji.shortcode.clone(),
                };
                Button::new(face)
                    .on_press(Action::Message(Message::PickEmoji(picked)))
                    .into()
            })
            .collect();
        Wrap::with_elements(buttons)
            .spacing(2.0)
            .line_spacing(2.0)
            .into()
    }

    /// The sticker grid; clicking one sends it immediately.
    fn sticker_grid<'a>(&'a self, picker: &'a Picker) -> Element<'a, Action> {
        let Some(stickers) = &self.stickers else {
            return Text::new("Loading...").into();
        };
        if stickers.is_empty() {
            return Text::new("No stickers here.").into();
        }
        let query = picker.search.trim().to_lowercase();
        let buttons = stickers
            .iter()
            .filter(|sticker| query.is_empty() || sticker.alt.to_lowercase().contains(&query))
            .take(MAX_PICKER_RESULTS)
            .map(|sticker| {
                let face: Element<'a, Action> = match &sticker.image {
                    Some(path) => image(path)
                        .width(Length::Fixed(PICKER_STICKER_SIZE))
                        .height(Length::Fixed(PICKER_STICKER_SIZE))
                        .into(),
                    None => Text::new(sticker.alt.as_str()).into(),
                };
                Button::new(face)
                    .on_press(Action::Message(Message::SendSticker(sticker.clone())))
                    .into()
            })
            .collect();
        Wrap::with_elements(buttons)
            .spacing(4.0)
            .line_spacing(4.0)
            .into()
    }

    fn picker_panel<'a>(&'a self, picker: &'a Picker) -> Element<'a, Action> {
        let tab_button = |label: &'a str, tab: PickerTab| {
            let button = Button::new(Text::new(label).size(12));
            if picker.tab == tab {
                button
            } else {
                button.on_press(Action::Message(Message::PickerTab(tab)))
            }
        };
        let reacting = matches!(picker.target, PickerTarget::React(_));
        let mut header = row![
            tab_button("Emoji", PickerTab::Unicode),
            tab_button("Custom", PickerTab::Custom),
        ]
        .spacing(4.0);
        if !reacting {
            header = header.push(tab_button("Stickers", PickerTab::Stickers));
        }
        if reacting {
            header = header.push(Text::new("React with...").size(12));
        }
        header = header.push(
            TextInput::new("Search...", &picker.search)
                .on_input(|s| Action::Message(Message::PickerSearch(s)))
                .size(12),
        );

        if picker.tab == PickerTab::Unicode && picker.search.trim().is_empty() {
            header = UNICODE_GROUPS.iter().enumerate().fold(
                header,
                |header, (index, (label, _group))| {
                    header.push(
                        Button::new(Text::new(*label).size(12))
                            .on_press(Action::Message(Message::PickerGroup(index))),
                    )
                },
            );
        }

        let grid = match picker.tab {
            PickerTab::Unicode => self.unicode_grid(picker),
            PickerTab::Custom => self.custom_grid(picker),
            PickerTab::Stickers => self.sticker_grid(picker),
        };

        column![
            header,
            Scrollable::new(grid)
                .width(Length::Fill)
                .height(Length::Fixed(PICKER_HEIGHT)),
        ]
        .spacing(4.0)
        .into()
    }

    pub fn get_element<'a>(&'a self, messengers: &'a MessengerRegistry) -> Element<'a, Action> {
        let mut channel_info = row![Text::new(self.room.name.clone())];
        if self
            .room
            .room_capabilities
            .contains(RoomCapabilities::Voice)
        {
            channel_info = channel_info.push(Button::new("CALL").on_press(Action::Call {
                messenger: self.messenger,
                room: self.room.clone(),
            }));
        }
        if let Some([partner]) = self.room.members.as_deref() {
            channel_info = channel_info.push(
                Button::new("Info")
                    .on_press(Action::Message(Message::InspectUser(partner.clone()))),
            );
        }

        let entry = messengers.get(self.messenger);
        let data = entry.map(|entry| &entry.data);
        let room_id = *self.room.id();
        let messages = data
            .and_then(|d| d.room(*self.room.id()))
            .and_then(|room| room.messages.as_ref())
            .or_else(|| self.room.messages.as_ref());

        let client_id = data.and_then(|d| d.profile.as_ref()).map(|p| *p.id());
        let pending_sends = data.map(|data| data.pending_sends.as_slice());

        let can_edit = entry.is_some_and(|entry| entry.interface.edit);
        let can_delete = entry.is_some_and(|entry| entry.interface.delete);
        let can_react = entry.is_some_and(|entry| entry.interface.reactions);

        let chat = Scrollable::new(match messages {
            Some(messages) => {
                let hidden = messages.len().saturating_sub(self.visible_message_limit);
                let messages = messages
                    .iter()
                    .skip(hidden)
                    .map(|msg| {
                        let pending = pending_sends.is_some_and(|pending_sends| {
                            pending_sends.iter().any(|pending_send| {
                                pending_send.room_id == room_id
                                    && pending_send.pending_id == *msg.id()
                            })
                        });
                        let rendered = message_text(
                            msg,
                            pending,
                            |msg, emoji, reacted| {
                                Action::Message(Message::ToggleReaction {
                                    message_id: *msg.id(),
                                    emoji: emoji.to_owned(),
                                    reacted,
                                })
                            },
                            |author| Action::Message(Message::InspectUser(author.clone())),
                            |href| Action::Message(Message::OpenLink(href.to_owned())),
                        );

                        let own = client_id.is_some_and(|client_id| {
                            msg.author.as_ref().is_some_and(|a| *a.id() == client_id)
                        });
                        let can_edit = own && can_edit;
                        let can_delete = own && can_delete;
                        if pending || !(can_react || can_edit || can_delete) {
                            return rendered;
                        }

                        let message_id = *msg.id();
                        let mut actions = row![rendered].spacing(4.0);
                        if can_react {
                            actions = actions.push(
                                Button::new(Text::new("React").size(12))
                                    .on_press(Action::Message(Message::StartReact { message_id })),
                            );
                        }
                        if can_edit {
                            actions =
                                actions.push(Button::new(Text::new("Edit").size(12)).on_press(
                                    Action::Message(Message::StartEdit {
                                        message_id,
                                        current: msg.content.text.to_plain(),
                                    }),
                                ));
                        }
                        if can_delete {
                            actions = actions.push(
                                Button::new(Text::new("Delete").size(12))
                                    .on_press(Action::Message(Message::Delete { message_id })),
                            );
                        }
                        actions.into()
                    })
                    .fold(Column::new().spacing(15.0), |column, widget| {
                        column.push(widget)
                    });
                if hidden == 0 {
                    messages
                } else {
                    Column::new()
                        .spacing(15.0)
                        .push(
                            Button::new(Text::new(format!("Show {hidden} older loaded messages")))
                                .on_press(Action::Message(Message::ShowOlder)),
                        )
                        .push(messages)
                }
            }
            None => Column::new(),
        })
        .anchor_bottom()
        .width(Length::Fill)
        .height(Length::Fill);

        let message_box = TextInput::new(
            if self.editing.is_some() {
                "Edit msg..."
            } else {
                "New msg..."
            },
            &self.msg_box,
        )
        .on_input(|s| Action::Message(Message::MsgInput(s)))
        .on_submit(Action::Message(Message::MsgSend))
        .line_height(LineHeight::Absolute(20.into()));

        let compose_row = row![
            message_box,
            Button::new(Text::new("😀")).on_press(Action::Message(Message::TogglePicker)),
        ]
        .spacing(4.0);

        let mut composer = Column::new().spacing(4.0);
        if self.editing.is_some() {
            composer = composer.push(
                row![
                    Text::new("Editing message").size(12),
                    Button::new(Text::new("Cancel").size(12))
                        .on_press(Action::Message(Message::CancelEdit)),
                ]
                .spacing(8.0),
            );
        }
        if let Some(picker) = &self.picker {
            composer = composer.push(self.picker_panel(picker));
        }
        let composer = composer.push(compose_row);

        column![channel_info, chat, composer].into()
    }

    pub fn update(
        &mut self,
        message: Message,
        messengers: &MessengerRegistry,
    ) -> UpdateResult {
        match message {
            Message::MsgInput(change) => {
                self.msg_box = change;
                UpdateResult::Task(Task::none())
            }
            Message::MsgSend => {
                let contents = std::mem::take(&mut self.msg_box);
                if contents.is_empty() {
                    return UpdateResult::Task(Task::none());
                }
                let contents =
                    resolve_compose(&contents, self.custom_emojis.as_deref().unwrap_or(&[]));
                match self.editing.take() {
                    Some(message_id) => UpdateResult::Edit {
                        messenger: self.messenger,
                        room: self.room.clone(),
                        message_id,
                        contents,
                    },
                    None => UpdateResult::Send {
                        messenger: self.messenger,
                        room: self.room.clone(),
                        contents,
                    },
                }
            }
            Message::StartEdit {
                message_id,
                current,
            } => {
                self.editing = Some(message_id);
                self.msg_box = current;
                UpdateResult::Task(Task::none())
            }
            Message::CancelEdit => {
                self.editing = None;
                self.msg_box.clear();
                UpdateResult::Task(Task::none())
            }
            Message::Delete { message_id } => UpdateResult::Delete {
                messenger: self.messenger,
                room: self.room.clone(),
                message_id,
            },
            Message::InspectUser(user) => UpdateResult::InspectUser {
                messenger: self.messenger,
                user,
            },
            Message::ToggleReaction {
                message_id,
                emoji,
                reacted,
            } => UpdateResult::ToggleReaction {
                messenger: self.messenger,
                room: self.room.clone(),
                message_id,
                emoji,
                reacted,
            },
            Message::TogglePicker => {
                let compose_open = self
                    .picker
                    .as_ref()
                    .is_some_and(|picker| picker.target == PickerTarget::Compose);
                if compose_open {
                    self.picker = None;
                } else {
                    self.picker = Some(Picker::default());
                    if self.custom_emojis.is_none() {
                        return UpdateResult::Task(self.load_palette(messengers));
                    }
                }
                UpdateResult::Task(Task::none())
            }
            Message::StartReact { message_id } => {
                let same_target = self
                    .picker
                    .as_ref()
                    .is_some_and(|picker| picker.target == PickerTarget::React(message_id));
                if same_target {
                    self.picker = None;
                } else {
                    self.picker = Some(Picker::reacting_to(message_id));
                    if self.custom_emojis.is_none() {
                        return UpdateResult::Task(self.load_palette(messengers));
                    }
                }
                UpdateResult::Task(Task::none())
            }
            Message::PickerTab(tab) => {
                if let Some(picker) = &mut self.picker {
                    picker.tab = tab;
                }
                UpdateResult::Task(Task::none())
            }
            Message::PickerSearch(search) => {
                if let Some(picker) = &mut self.picker {
                    picker.search = search;
                }
                UpdateResult::Task(Task::none())
            }
            Message::PickerGroup(group) => {
                if let Some(picker) = &mut self.picker {
                    picker.group = group.min(UNICODE_GROUPS.len() - 1);
                }
                UpdateResult::Task(Task::none())
            }
            Message::PickEmoji(emoji) => {
                let target = self
                    .picker
                    .as_ref()
                    .map(|picker| picker.target)
                    .unwrap_or_default();
                match target {
                    PickerTarget::Compose => {
                        self.msg_box.push_str(&emoji);
                        UpdateResult::Task(Task::none())
                    }
                    PickerTarget::React(message_id) => {
                        self.picker = None;
                        UpdateResult::ToggleReaction {
                            messenger: self.messenger,
                            room: self.room.clone(),
                            message_id,
                            emoji,
                            reacted: false,
                        }
                    }
                }
            }
            Message::SendSticker(sticker) => {
                self.picker = None;
                UpdateResult::Send {
                    messenger: self.messenger,
                    room: self.room.clone(),
                    contents: RichText {
                        spans: vec![Span::Sticker(sticker)],
                    },
                }
            }
            Message::PaletteLoaded { emojis, stickers } => {
                self.custom_emojis = Some(emojis);
                self.stickers = Some(stickers);
                UpdateResult::Task(Task::none())
            }
            Message::ShowOlder => {
                self.visible_message_limit = self
                    .visible_message_limit
                    .saturating_add(VISIBLE_MESSAGE_CHUNK);
                UpdateResult::Task(Task::none())
            }
            Message::OpenLink(href) => {
                if let Err(error) = open_external_link(&href) {
                    debug!("Could not open message link: {error}");
                }
                UpdateResult::Task(Task::none())
            }
        }
    }
}

/// Launch an untrusted message link without a shell. Restrict schemes so
/// backend text cannot invoke local file/application handlers.
fn open_external_link(href: &str) -> Result<(), String> {
    let lower = href.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err("only http/https links are allowed".to_owned());
    }

    #[cfg(target_os = "windows")]
    let child = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", href])
        .spawn();
    #[cfg(target_os = "macos")]
    let child = std::process::Command::new("open").arg(href).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let child = std::process::Command::new("xdg-open").arg(href).spawn();
    #[cfg(not(any(unix, target_os = "windows")))]
    let child: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "link opening is unsupported on this platform",
    ));

    child.map(|_| ()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Vec<Emoji> {
        vec![Emoji {
            shortcode: "steamhappy".to_owned(),
            image: None,
            reference: Some("<:steamhappy:123>".to_owned()),
        }]
    }

    #[test]
    fn empty_palette_stays_plain() {
        let rich = resolve_compose("hi :steamhappy: yo", &[]);
        assert!(matches!(
            rich.spans.as_slice(),
            [Span::Text { text, .. }] if text == "hi :steamhappy: yo"
        ));
    }

    #[test]
    fn known_shortcode_becomes_emoji_span() {
        let rich = resolve_compose("hi :steamhappy: yo", &palette());
        assert!(matches!(
            rich.spans.as_slice(),
            [
                Span::Text { text: before, .. },
                Span::Emoji(emoji),
                Span::Text { text: after, .. },
            ] if before == "hi " && emoji.shortcode == "steamhappy" && after == " yo"
        ));
    }

    #[test]
    fn unknown_shortcode_and_stray_colons_stay_text() {
        let rich = resolve_compose("ratio 3:2 :nope: end", &palette());
        assert!(matches!(
            rich.spans.as_slice(),
            [Span::Text { text, .. }] if text == "ratio 3:2 :nope: end"
        ));
    }

    #[test]
    fn colon_run_recovers_following_match() {
        let rich = resolve_compose("x :a b:steamhappy:", &palette());
        assert!(matches!(
            rich.spans.as_slice(),
            [Span::Text { text, .. }, Span::Emoji(emoji)]
                if text == "x :a b" && emoji.shortcode == "steamhappy"
        ));
    }

    #[test]
    fn emoji_only_message() {
        let rich = resolve_compose(":steamhappy:", &palette());
        assert!(matches!(
            rich.spans.as_slice(),
            [Span::Emoji(emoji)] if emoji.shortcode == "steamhappy"
        ));
        assert!(!rich.is_empty());
    }
}
