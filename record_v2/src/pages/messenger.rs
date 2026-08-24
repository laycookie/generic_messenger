use std::{error::Error, path::PathBuf};

use crate::audio_controls::{AfxTarget, AudioControls, AudioOp};
use crate::components::divider::Divider;
use crate::pages::{RemoteVideo, SelectedVideoInput};
use messenger_unifier::{Call, MessengerData, MessengerId, MessengerRegistry, PendingSend};
use simple_audio_channels::{
    effects::AfxRegistration,
    effects_rack::{EffectsRack, SlotKey},
    params::{ParamKey, ParamSpec, ParamUnit},
};
use simple_video::{
    CaptureSource, PixelData, ScreenCaptureConfig, ScreenSource, has_screen_capture_permission,
    request_screen_capture_permission, screen_capture_supported, screen_sources, webcams,
};

use self::{
    chat::{Action as ChatAction, Chat, UpdateResult as ChatUpdateResult},
    contacts::{Action as ContactsAction, Contacts},
    navbar::{Action as NavbarAction, Navbar},
    sidebar::{Action as SidebarAction, Server, Sidebar},
};

use iced::{
    Alignment, Color, ContentFit, Element, Length, Padding, Task,
    widget::{
        Button, Column, Radio, Responsive, Row, Scrollable, Text, center, column, container, image,
        mouse_area, opaque, row, slider, stack,
    },
};
use messenger_interface::interface::{VideoBroadcastKind, VideoResolution, VideoStreamId};
use messenger_interface::types::{
    Details, House, ID, Identifier, Message as InterfaceMessage, Place, Revision, Room, User,
    VoiceState,
};
use tracing::{error, warn};

mod chat;
mod contacts;
mod navbar;
mod sidebar;

pub(super) const PLACEHOLDER_PFP: &str = "./public/imgs/placeholder.jpg";

fn closed_messenger_error() -> Box<dyn Error + Send + Sync> {
    std::io::Error::new(std::io::ErrorKind::NotConnected, "messenger is closed").into()
}

#[derive(Clone)]
pub(crate) enum Main {
    Contacts(Contacts),
    Chat(Box<Chat>),
}

pub struct Messenger {
    sidebar: Sidebar,
    main: Main,
    pending_counter: ID,
    /// Inspect sheet shown as a modal over the page, if any.
    inspect: Option<Inspect>,
    /// Afx sheet (a live channel's effect chain) shown as a modal, if any.
    afx: Option<AfxSheet>,
    /// Outgoing camera/screen source picker shown before broadcast starts.
    video_input_picker: Option<VideoInputPicker>,
}

impl Messenger {
    pub(crate) fn new() -> Self {
        Messenger {
            sidebar: Sidebar::new(168.0),
            main: Main::Contacts(Contacts::default()),
            pending_counter: ID::MAX,
            inspect: None,
            afx: None,
            video_input_picker: None,
        }
    }

    fn next_pending_id(&mut self) -> ID {
        let id = self.pending_counter;
        self.pending_counter = self.pending_counter.wrapping_sub(1);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_input_defaults_match_content_type() {
        let camera = default_video_settings(VideoBroadcastKind::Camera);
        assert_eq!(camera.frames_per_second, 30);
        assert_eq!(camera.resolution, VideoResolution::new(1280, 720));
        assert_eq!(camera.bitrate_bps, 1_200_000);

        let screen = default_video_settings(VideoBroadcastKind::Screen);
        assert_eq!(screen.frames_per_second, 15);
        assert_eq!(screen.resolution, VideoResolution::new(1280, 720));
        assert_eq!(screen.bitrate_bps, 1_500_000);
    }
}

fn video_capture_details() -> Details {
    let mut details = Details::default();

    match webcams() {
        Ok(webcams) => {
            details.push("Webcams", webcams.len().to_string());
            for (index, webcam) in webcams.iter().enumerate() {
                let mut value = format!("{} (ID: {})", webcam.name(), webcam.id());
                if !webcam.description().is_empty() {
                    value.push_str(&format!("\nDescription: {}", webcam.description()));
                }
                if !webcam.miscellaneous().is_empty() {
                    value.push_str(&format!("\nBackend data: {}", webcam.miscellaneous()));
                }
                details.push(format!("Webcam {}", index + 1), value);
            }
        }
        Err(err) => {
            details.push("Webcams", format!("Discovery failed: {err}"));
        }
    }

    details.push(
        "Screen support",
        if screen_capture_supported() {
            "supported"
        } else {
            "not supported"
        },
    );
    details.push(
        "Screen access",
        if has_screen_capture_permission() {
            "granted"
        } else {
            "not granted"
        },
    );

    let screens = screen_sources();
    details.push("Screen targets", screens.len().to_string());
    for (index, screen) in screens.iter().enumerate() {
        let id = screen
            .id()
            .map_or_else(|| "platform default".to_owned(), |id| id.to_string());
        details.push(
            format!("Target {}", index + 1),
            format!("{} ({:?}, ID: {id})", screen.title(), screen.kind()),
        );
    }

    if cfg!(target_os = "linux") {
        details.push(
            "Linux target",
            "Concrete screen/window is selected through desktop portal when capture starts.",
        );
    }

    details
}

fn screen_capture_test_details() -> Result<Details, String> {
    if !screen_capture_supported() {
        return Err("Screen capture is not supported on this system.".to_owned());
    }
    if !has_screen_capture_permission() && !request_screen_capture_permission() {
        return Err("Screen capture permission was not granted.".to_owned());
    }

    let source = ScreenSource::platform_default();
    let mut capture = source
        .open(ScreenCaptureConfig::default())
        .map_err(|error| error.to_string())?;
    let frame = capture.next_frame().map_err(|error| error.to_string())?;
    capture.stop();

    let (format, bytes) = match &frame.pixel_data {
        PixelData::Rgba8(data) => ("RGBA8", data.len().to_string()),
        PixelData::Rgb8(data) => ("RGB8", data.len().to_string()),
        PixelData::SharedRgba8(data) => ("RGBA8 (shared)", data.len().to_string()),
        PixelData::SharedRgb8(data) => ("RGB8 (shared)", data.len().to_string()),
        _ => ("unknown", "unknown".to_owned()),
    };
    let mut details = Details::default();
    details.push("Selection", "accepted");
    details.push("Resolution", format!("{}x{}", frame.width, frame.height));
    details.push("Pixel format", format);
    details.push("Frame bytes", bytes);
    if cfg!(target_os = "linux") {
        details.push(
            "Portal target",
            "Selected target metadata is not exposed by scap 0.0.8.",
        );
    }
    Ok(details)
}

fn discover_video_inputs(
    kind: VideoBroadcastKind,
) -> (Vec<crate::pages::VideoInputSource>, Option<String>) {
    let mut inputs = vec![crate::pages::VideoInputSource::CyanTestPattern];
    let warning = match kind {
        VideoBroadcastKind::Camera => match webcams() {
            Ok(webcams) => {
                inputs.extend(
                    webcams
                        .into_iter()
                        .map(CaptureSource::Webcam)
                        .map(crate::pages::VideoInputSource::Capture),
                );
                None
            }
            Err(error) => Some(format!("Webcam discovery failed: {error}")),
        },
        VideoBroadcastKind::Screen if !screen_capture_supported() => {
            Some("Screen capture is not supported on this system.".to_owned())
        }
        VideoBroadcastKind::Screen => {
            inputs.extend(
                screen_sources()
                    .into_iter()
                    .map(CaptureSource::Screen)
                    .map(crate::pages::VideoInputSource::Capture),
            );
            None
        }
    };
    (inputs, warning)
}

/// The inspect modal: an entity's name/icon plus backend detail fields.
#[derive(Clone)]
struct Inspect {
    title: String,
    icon: Option<PathBuf>,
    state: InspectState,
}

#[derive(Clone)]
enum InspectState {
    Loading,
    Loaded(Details),
    Failed(String),
}

impl Inspect {
    fn view(&self) -> Element<'_, Message> {
        let mut header = Row::new().spacing(10.0).align_y(Alignment::Center);
        if let Some(icon) = &self.icon {
            header = header.push(
                image(icon)
                    .height(Length::Fixed(48.0))
                    .width(Length::Fixed(48.0))
                    .content_fit(ContentFit::Cover),
            );
        }
        header = header.push(Text::new(self.title.as_str()).size(22));

        let body: Element<'_, Message> = match &self.state {
            InspectState::Loading => Text::new("Loading...").into(),
            InspectState::Failed(err) => Text::new(err.as_str())
                .color(Color::from_rgb(0.8, 0.2, 0.2))
                .into(),
            InspectState::Loaded(details) if details.fields.is_empty() => {
                Text::new("No details available").into()
            }
            InspectState::Loaded(details) => container(Scrollable::new(
                Column::from_iter(details.fields.iter().map(|(label, value)| {
                    row![
                        Text::new(label.as_str())
                            .size(13)
                            .width(Length::Fixed(110.0)),
                        Text::new(value.as_str()).width(Length::Fill),
                    ]
                    .spacing(8.0)
                    .into()
                }))
                .spacing(6.0)
                .width(Length::Fill),
            ))
            .max_height(320.0)
            .width(Length::Fill)
            .into(),
        };

        container(
            column![
                header,
                body,
                Button::new("Close").on_press(Message::CloseInspect)
            ]
            .spacing(14.0),
        )
        .padding(18.0)
        .width(Length::Fixed(380.0))
        .style(container::rounded_box)
        .into()
    }
}

struct VideoInputPicker {
    call: Call,
    kind: VideoBroadcastKind,
    state: VideoInputPickerState,
    selected_input: Option<usize>,
    settings: VideoInputSettings,
}

enum VideoInputPickerState {
    Loading,
    Loaded {
        inputs: Vec<crate::pages::VideoInputSource>,
        warning: Option<String>,
    },
}

#[derive(Clone, Copy)]
struct VideoInputSettings {
    frames_per_second: u32,
    resolution: VideoResolution,
    bitrate_bps: u32,
}

const VIDEO_FRAME_RATE_CHOICES: [u32; 6] = [5, 10, 15, 20, 24, 30];
const VIDEO_RESOLUTION_CHOICES: [VideoResolution; 3] = [
    VideoResolution::new(640, 360),
    VideoResolution::new(960, 540),
    VideoResolution::new(1280, 720),
];
const MIN_VIDEO_BITRATE_BPS: u32 = 250_000;
const MAX_VIDEO_BITRATE_BPS: u32 = 5_000_000;
const VIDEO_BITRATE_STEP_BPS: u32 = 50_000;

fn default_video_settings(kind: VideoBroadcastKind) -> VideoInputSettings {
    let (frames_per_second, bitrate_bps) = match kind {
        VideoBroadcastKind::Camera => (30, 1_200_000),
        VideoBroadcastKind::Screen => (15, 1_500_000),
    };
    VideoInputSettings {
        frames_per_second,
        resolution: VideoResolution::new(1280, 720),
        bitrate_bps,
    }
}

impl VideoInputPicker {
    fn view(&self) -> Element<'_, Message> {
        let title = match self.kind {
            VideoBroadcastKind::Camera => "Start camera",
            VideoBroadcastKind::Screen => "Start screen share",
        };
        let body: Element<'_, Message> = match &self.state {
            VideoInputPickerState::Loading => Text::new("Finding video inputs...").into(),
            VideoInputPickerState::Loaded { inputs, warning } => {
                let mut choices =
                    Column::from_iter(inputs.iter().enumerate().map(|(index, input)| {
                        Radio::new(
                            input.label(),
                            index,
                            self.selected_input,
                            Message::SelectVideoInput,
                        )
                        .into()
                    }))
                    .spacing(6.0)
                    .width(Length::Fill);
                if let Some(warning) = warning {
                    choices = choices.push(
                        Text::new(warning.as_str())
                            .size(12)
                            .color(Color::from_rgb(0.8, 0.4, 0.2)),
                    );
                }

                let frame_rates =
                    Column::from_iter(VIDEO_FRAME_RATE_CHOICES.chunks(3).map(|choices| {
                        Row::from_iter(choices.iter().copied().map(|frames_per_second| {
                            Radio::new(
                                frames_per_second.to_string(),
                                frames_per_second,
                                Some(self.settings.frames_per_second),
                                Message::SetVideoFrameRate,
                            )
                            .into()
                        }))
                        .spacing(10.0)
                        .into()
                    }))
                    .spacing(6.0);
                let resolutions =
                    Row::from_iter(VIDEO_RESOLUTION_CHOICES.into_iter().map(|resolution| {
                        Radio::new(
                            format!("{}×{}", resolution.width, resolution.height),
                            resolution,
                            Some(self.settings.resolution),
                            Message::SetVideoResolution,
                        )
                        .into()
                    }))
                    .spacing(10.0);
                let mut start = Button::new("Start broadcast").width(Length::Fill);
                if self.selected_input.is_some() {
                    start = start.on_press(Message::StartVideo);
                }

                column![
                    Text::new("Source").size(14),
                    container(Scrollable::new(choices))
                        .max_height(180.0)
                        .width(Length::Fill),
                    Text::new("Frames per second").size(14),
                    frame_rates,
                    Text::new("Resolution").size(14),
                    resolutions,
                    Text::new(format!(
                        "Bitrate: {} kbps",
                        self.settings.bitrate_bps / 1_000
                    ))
                    .size(14),
                    slider(
                        MIN_VIDEO_BITRATE_BPS..=MAX_VIDEO_BITRATE_BPS,
                        self.settings.bitrate_bps,
                        Message::SetVideoBitrate,
                    )
                    .step(VIDEO_BITRATE_STEP_BPS),
                    start,
                ]
                .spacing(8.0)
                .into()
            }
        };

        container(
            column![
                Text::new(title).size(20),
                body,
                Button::new("Cancel").on_press(Message::CloseVideoInputPicker),
            ]
            .spacing(12.0),
        )
        .padding(16.0)
        .width(Length::Fixed(360.0))
        .style(container::rounded_box)
        .into()
    }
}

/// The afx modal: a live channel's effect chain — listed with per-effect
/// remove and one generic control per parameter the effect declares
/// (rendered from its [`ParamSpec`] table), plus the effects that can be added.
struct AfxSheet {
    target: AfxTarget,
    /// Whose channel this is, for the header (a participant's name, or "Microphone").
    title: String,
}

/// Display a parameter value the way its spec says it should be read.
fn format_param(spec: &ParamSpec, value: f32) -> String {
    if spec.min_is_off && value <= spec.min {
        return "off".to_owned();
    }
    match spec.unit {
        ParamUnit::Amplitude => format!("{:.0}%", value * 100.0),
        ParamUnit::Ratio => format!("{value:.1}:1"),
        ParamUnit::Decibels => format!("{value:.1} dB"),
        ParamUnit::Milliseconds => format!("{value:.0} ms"),
        ParamUnit::Hertz => format!("{value:.0} Hz"),
    }
}

impl AfxSheet {
    fn view(&self, audio: &AudioControls) -> Element<'_, Message> {
        let header = Text::new(format!("Audio effects — {}", self.title)).size(20);

        let body: Element<'_, Message> = match audio.effects(self.target) {
            // Racks only exist while their channel does (during a call).
            None => Text::new("No live audio channel to edit.").into(),
            Some(effects) => {
                let listing: Element<'_, Message> = if effects.is_empty() {
                    Text::new("No effects").size(13).into()
                } else {
                    Column::from_iter(effects.iter().map(|afx| {
                        let effect = afx.id;
                        let title = row![
                            Text::new(afx.name).width(Length::Fill),
                            Button::new(Text::new("x").size(11))
                                .on_press(Message::AfxRemove(effect)),
                        ]
                        .align_y(Alignment::Center);

                        let knobs = Column::from_iter(afx.params().map(|state| {
                            let (spec, param) = (state.spec, state.spec.key);
                            row![
                                Text::new(spec.name).size(11).width(Length::Fixed(72.0)),
                                slider(spec.min..=spec.max, state.value, move |value| {
                                    Message::AfxSetParam {
                                        effect,
                                        param,
                                        value,
                                    }
                                })
                                .step((spec.max - spec.min) / 200.0)
                                .width(Length::Fill),
                                container(Text::new(format_param(spec, state.value)).size(11))
                                    .width(Length::Fixed(56.0))
                                    .padding(Padding::new(0.0).left(6.0)),
                            ]
                            .align_y(Alignment::Center)
                            .into()
                        }))
                        .spacing(2.0);

                        column![title, knobs].spacing(2.0).into()
                    }))
                    .spacing(10.0)
                    .into()
                };

                let add_row = Row::from_iter(
                    std::iter::once(Element::from(Text::new("Add:").size(13))).chain(
                        EffectsRack::available_effects().iter().map(|effect| {
                            Button::new(Text::new(format!("+ {}", effect.name())).size(13))
                                .on_press(Message::AfxAdd(*effect))
                                .into()
                        }),
                    ),
                )
                .spacing(8.0)
                .align_y(Alignment::Center);

                column![listing, add_row].spacing(12.0).into()
            }
        };

        container(
            column![
                header,
                body,
                Button::new("Close").on_press(Message::CloseAfx)
            ]
            .spacing(14.0),
        )
        .padding(18.0)
        .width(Length::Fixed(400.0))
        .style(container::rounded_box)
        .into()
    }
}

#[derive(Clone)]
pub(crate) enum Message {
    OpenSettings,
    Chat(ChatAction),
    Contacts(ContactsAction),
    Navbar(NavbarAction),
    Sidebar(SidebarAction),
    // ===
    ChangeMain(Main),
    SetSidebarServer(Option<Server>),
    DividerChange(f32),
    GuildRoomsLoaded {
        id: MessengerId,
        guild_id: ID,
        rooms: Vec<Identifier<Place<Room>>>,
    },
    UpdateChat {
        id: MessengerId,
        kv: (ID, Vec<Identifier<InterfaceMessage>>),
    },
    /// A message the user just sent, shown optimistically before server confirmation.
    #[allow(clippy::enum_variant_names)]
    PendingMessage {
        id: MessengerId,
        room_id: ID,
        message: Identifier<InterfaceMessage>,
    },
    /// A pending message was confirmed by the server — replace the temp entry.
    ConfirmPending {
        id: MessengerId,
        room_id: ID,
        pending_id: ID,
        confirmed: Identifier<InterfaceMessage>,
    },
    /// A pending message failed to send — remove it.
    RemovePending {
        id: MessengerId,
        room_id: ID,
        pending_id: ID,
    },
    /// The inspect sheet fetch resolved.
    InspectLoaded(Result<Details, String>),
    CloseInspect,
    /// Native capture-source discovery completed for outgoing video.
    VideoInputsLoaded {
        kind: VideoBroadcastKind,
        inputs: Vec<crate::pages::VideoInputSource>,
        warning: Option<String>,
    },
    SelectVideoInput(usize),
    SetVideoFrameRate(u32),
    SetVideoResolution(VideoResolution),
    SetVideoBitrate(u32),
    /// Start broadcast using every value selected in the single picker.
    StartVideo,
    CloseVideoInputPicker,
    /// Add the picked effect to the open afx sheet's channel.
    AfxAdd(AfxRegistration),
    /// Remove an effect from the open afx sheet's channel.
    AfxRemove(SlotKey),
    /// A parameter slider in the afx sheet moved.
    AfxSetParam {
        effect: SlotKey,
        param: ParamKey,
        value: f32,
    },
    CloseAfx,
    /// An edit was confirmed by the server; replace the stored message.
    ConfirmEdit {
        id: MessengerId,
        room_id: ID,
        updated: Identifier<InterfaceMessage>,
    },
    /// A delete was confirmed by the server; drop the stored message.
    ConfirmDelete {
        id: MessengerId,
        room_id: ID,
        message_id: ID,
    },
}

pub enum Action {
    None,
    OpenSettings,
    Run(Task<Message>),
    ModifyMessengerData {
        id: MessengerId,
        modify: Box<dyn FnOnce(&mut MessengerData) + Send>,
    },
    Call {
        messenger: MessengerId,
        channel: Identifier<Place<Room>>,
    },
    DisconnectFromCall(Call),
    /// Broadcast the client's own mute/deafen flags for an active call.
    SetCallState {
        call: Call,
        state: VoiceState,
    },
    /// Toggle the client's own video broadcast of `kind` (camera or screen
    /// share) for an active call.
    SetVideo {
        call: Call,
        kind: VideoBroadcastKind,
        enabled: bool,
        input: Option<SelectedVideoInput>,
    },
    /// Start receiving one remote source after an explicit UI action.
    SubscribeVideo {
        messenger: MessengerId,
        stream: VideoStreamId,
    },
    /// An audio edit for the app-owned `AudioControls` (volume, afx chain).
    Audio(AudioOp),
}

impl Messenger {
    /// Open the inspect modal for `user` and fetch their detail sheet.
    fn start_inspect_user(
        &mut self,
        messenger: MessengerId,
        user: Identifier<User>,
        messengers: &MessengerRegistry,
    ) -> Action {
        let title = user.name.clone();
        let icon = user.icon.clone();
        let Some(inspect) = messengers.with_interface_async(
            messenger,
            async move |interface| {
                let query = interface.query()?;
                query.inspect_user(&user).await
            },
        ) else {
            return Action::None;
        };
        self.inspect = Some(Inspect {
            title,
            icon,
            state: InspectState::Loading,
        });
        Action::Run(
            Task::future(async move {
                inspect
                    .await
                    .unwrap_or_else(|| Err(closed_messenger_error()))
            })
            .then(|result: Result<Details, Box<dyn Error + Send + Sync>>| {
                Task::done(Message::InspectLoaded(result.map_err(|e| e.to_string())))
            }),
        )
    }

    /// Open the inspect modal for `house` and fetch its detail sheet.
    fn start_inspect_house(
        &mut self,
        messenger: MessengerId,
        house: Identifier<Place<House>>,
        messengers: &MessengerRegistry,
    ) -> Action {
        let title = house.name.clone();
        let icon = house.icon.clone();
        let Some(inspect) = messengers.with_interface_async(
            messenger,
            async move |interface| {
                let query = interface.query()?;
                query.inspect_house(&house).await
            },
        ) else {
            return Action::None;
        };
        self.inspect = Some(Inspect {
            title,
            icon,
            state: InspectState::Loading,
        });
        Action::Run(
            Task::future(async move {
                inspect
                    .await
                    .unwrap_or_else(|| Err(closed_messenger_error()))
            })
            .then(|result: Result<Details, Box<dyn Error + Send + Sync>>| {
                Task::done(Message::InspectLoaded(result.map_err(|e| e.to_string())))
            }),
        )
    }

    /// Open capture-source debug sheet and discover native targets off UI path.
    fn start_inspect_video_capture(&mut self) -> Action {
        self.inspect = Some(Inspect {
            title: "Video capture sources".to_owned(),
            icon: None,
            state: InspectState::Loading,
        });
        Action::Run(
            Task::future(smol::unblock(video_capture_details))
                .then(|details| Task::done(Message::InspectLoaded(Ok(details)))),
        )
    }

    /// Open native screen selection and report one frame from selected target.
    fn start_pick_screen_capture(&mut self) -> Action {
        self.inspect = Some(Inspect {
            title: "Screen capture picker".to_owned(),
            icon: None,
            state: InspectState::Loading,
        });
        Action::Run(
            Task::future(smol::unblock(screen_capture_test_details))
                .then(|result| Task::done(Message::InspectLoaded(result))),
        )
    }

    fn start_video_input_picker(&mut self, call: Call, kind: VideoBroadcastKind) -> Action {
        self.video_input_picker = Some(VideoInputPicker {
            call,
            kind,
            state: VideoInputPickerState::Loading,
            selected_input: None,
            settings: default_video_settings(kind),
        });
        Action::Run(
            Task::future(smol::unblock(move || discover_video_inputs(kind))).then(
                move |(inputs, warning)| {
                    Task::done(Message::VideoInputsLoaded {
                        kind,
                        inputs,
                        warning,
                    })
                },
            ),
        )
    }

    pub(crate) fn update(&mut self, message: Message, messengers: &MessengerRegistry) -> Action {
        match message {
            Message::OpenSettings => Action::OpenSettings,
            Message::SetSidebarServer(server) => {
                self.sidebar.server_selected = server;
                Action::None
            }
            Message::ChangeMain(main) => {
                self.main = main;
                Action::None
            }
            Message::DividerChange(val) => {
                if (self.sidebar.width + val > 300.0) | (self.sidebar.width + val < 100.0) {
                    return Action::None;
                }
                self.sidebar.width += val;
                Action::None
            }
            Message::UpdateChat { id, kv } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    data.merge_message_snapshot(kv.0, kv.1);
                }),
            },
            Message::GuildRoomsLoaded {
                id,
                guild_id,
                rooms,
            } => {
                self.sidebar.server_selected = Some(Server::new(id, guild_id));
                Action::ModifyMessengerData {
                    id,
                    modify: Box::new(move |data| {
                        if let Some(guild) = data.guilds.iter_mut().find(|g| *g.id() == guild_id) {
                            guild.rooms = Some(rooms);
                            data.rebuild_room_index();
                        }
                    }),
                }
            }
            Message::PendingMessage {
                id,
                room_id,
                message,
            } => {
                let pending_id = *message.id();
                Action::ModifyMessengerData {
                    id,
                    modify: Box::new(move |data| {
                        data.pending_sends.push(PendingSend {
                            pending_id,
                            room_id,
                        });
                        if let Some(room) = data.room_mut(room_id) {
                            room.messages.get_or_insert_with(Vec::new).push(message);
                        }
                        data.move_conversation_to_front(room_id);
                    }),
                }
            }
            Message::ConfirmPending {
                id,
                room_id,
                pending_id,
                confirmed,
            } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    data.confirm_pending_message(room_id, pending_id, confirmed);
                }),
            },
            Message::RemovePending {
                id,
                room_id,
                pending_id,
            } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    data.pending_sends.retain(|p| p.pending_id != pending_id);
                    if let Some(room) = data.room_mut(room_id)
                        && let Some(msgs) = room.messages.as_mut()
                    {
                        msgs.retain(|m| *m.id() != pending_id);
                    }
                }),
            },
            Message::InspectLoaded(result) => {
                if let Some(inspect) = &mut self.inspect {
                    inspect.state = match result {
                        Ok(details) => InspectState::Loaded(details),
                        Err(err) => InspectState::Failed(err),
                    };
                }
                Action::None
            }
            Message::CloseInspect => {
                self.inspect = None;
                Action::None
            }
            Message::VideoInputsLoaded {
                kind,
                inputs,
                warning,
            } => {
                if let Some(picker) = &mut self.video_input_picker
                    && picker.kind == kind
                {
                    picker.state = VideoInputPickerState::Loaded { inputs, warning };
                }
                Action::None
            }
            Message::SelectVideoInput(index) => {
                let Some(picker) = &mut self.video_input_picker else {
                    return Action::None;
                };
                let VideoInputPickerState::Loaded { inputs, .. } = &picker.state else {
                    return Action::None;
                };
                if index >= inputs.len() {
                    return Action::None;
                }
                picker.selected_input = Some(index);
                Action::None
            }
            Message::SetVideoFrameRate(frames_per_second) => {
                let Some(picker) = &mut self.video_input_picker else {
                    return Action::None;
                };
                if VIDEO_FRAME_RATE_CHOICES.contains(&frames_per_second) {
                    picker.settings.frames_per_second = frames_per_second;
                }
                Action::None
            }
            Message::SetVideoResolution(resolution) => {
                let Some(picker) = &mut self.video_input_picker else {
                    return Action::None;
                };
                if VIDEO_RESOLUTION_CHOICES.contains(&resolution) {
                    picker.settings.resolution = resolution;
                }
                Action::None
            }
            Message::SetVideoBitrate(bitrate_bps) => {
                let Some(picker) = &mut self.video_input_picker else {
                    return Action::None;
                };
                picker.settings.bitrate_bps =
                    bitrate_bps.clamp(MIN_VIDEO_BITRATE_BPS, MAX_VIDEO_BITRATE_BPS);
                Action::None
            }
            Message::StartVideo => {
                let input = {
                    let Some(picker) = &mut self.video_input_picker else {
                        return Action::None;
                    };
                    let Some(index) = picker.selected_input else {
                        return Action::None;
                    };
                    let VideoInputPickerState::Loaded { inputs, .. } = &mut picker.state else {
                        return Action::None;
                    };
                    if index >= inputs.len() {
                        return Action::None;
                    }
                    inputs.swap_remove(index)
                };
                let Some(picker) = self.video_input_picker.take() else {
                    return Action::None;
                };
                Action::SetVideo {
                    call: picker.call,
                    kind: picker.kind,
                    enabled: true,
                    input: Some(SelectedVideoInput {
                        source: input,
                        frames_per_second: picker.settings.frames_per_second,
                        resolution: picker.settings.resolution,
                        bitrate_bps: picker.settings.bitrate_bps,
                        generation: 0,
                    }),
                }
            }
            Message::CloseVideoInputPicker => {
                self.video_input_picker = None;
                Action::None
            }
            Message::AfxAdd(effect) => match &self.afx {
                Some(sheet) => Action::Audio(AudioOp::AddEffect {
                    target: sheet.target,
                    effect,
                }),
                None => Action::None,
            },
            Message::AfxRemove(id) => match &self.afx {
                Some(sheet) => Action::Audio(AudioOp::RemoveEffect {
                    target: sheet.target,
                    id,
                }),
                None => Action::None,
            },
            Message::AfxSetParam {
                effect,
                param,
                value,
            } => match &self.afx {
                Some(sheet) => Action::Audio(AudioOp::SetEffectParam {
                    target: sheet.target,
                    effect,
                    param,
                    value,
                }),
                None => Action::None,
            },
            Message::CloseAfx => {
                self.afx = None;
                Action::None
            }
            Message::ConfirmEdit {
                id,
                room_id,
                updated,
            } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    data.update_message(room_id, updated);
                }),
            },
            Message::ConfirmDelete {
                id,
                room_id,
                message_id,
            } => Action::ModifyMessengerData {
                id,
                modify: Box::new(move |data| {
                    data.delete_message(room_id, message_id);
                }),
            },
            Message::Chat(msg) => {
                if let Main::Chat(chat) = &mut self.main {
                    return match msg {
                        ChatAction::Call { messenger, room } => Action::Call {
                            messenger,
                            channel: room,
                        },
                        ChatAction::Message(message) => match chat.update(message, messengers) {
                            ChatUpdateResult::Task(task) => {
                                Action::Run(task.map(|m| Message::Chat(ChatAction::Message(m))))
                            }
                            ChatUpdateResult::Send {
                                messenger,
                                room,
                                contents,
                            } => {
                                let id = messenger;
                                let room_id = *room.id();
                                let pending_id = self.next_pending_id();
                                let author =
                                    messengers.data(id).and_then(|data| data.profile.clone());

                                // TODO: Verify if we actually need to create an InterfaceMessage here,
                                // as we are already in the UI and all data in here is in the unified format.
                                let pending_msg = Identifier::new(
                                    pending_id,
                                    InterfaceMessage {
                                        content: Revision {
                                            at: None,
                                            text: contents.clone(),
                                        },
                                        history: Vec::new(),
                                        reactions: Vec::new(),
                                        author,
                                    },
                                );

                                let Some(send) = messengers.with_interface_async(
                                    id,
                                    async move |interface| {
                                        let text = interface.messenger().text().map_err(|e| {
                                            Box::new(e) as Box<dyn Error + Send + Sync>
                                        })?;
                                        let confirmed = text
                                            .send_message(
                                                &room,
                                                InterfaceMessage {
                                                    content: Revision {
                                                        at: None,
                                                        text: contents,
                                                    },
                                                    history: Vec::new(),
                                                    reactions: Vec::new(),
                                                    author: None,
                                                },
                                            )
                                            .await?;
                                        Ok(confirmed)
                                    },
                                ) else {
                                    return Action::None;
                                };

                                Action::Run(
                                    // Add pending message to the chat store immediately
                                    Task::done(Message::PendingMessage {
                                        id,
                                        room_id,
                                        message: pending_msg,
                                    })
                                    .chain(
                                        // Fire the async send and handle result
                                        Task::future(async move {
                                            send.await
                                                .unwrap_or_else(|| Err(closed_messenger_error()))
                                        })
                                        .then(
                                            move |result: Result<
                                                _,
                                                Box<dyn Error + Send + Sync>,
                                            >| {
                                                match result {
                                                    Ok(confirmed) => {
                                                        Task::done(Message::ConfirmPending {
                                                            id,
                                                            room_id,
                                                            pending_id,
                                                            confirmed,
                                                        })
                                                    }
                                                    Err(e) => {
                                                        error!("Failed to send message: {e:#?}");
                                                        Task::done(Message::RemovePending {
                                                            id,
                                                            room_id,
                                                            pending_id,
                                                        })
                                                    }
                                                }
                                            },
                                        ),
                                    ),
                                )
                            }
                            ChatUpdateResult::Edit {
                                messenger,
                                room,
                                message_id,
                                contents,
                            } => {
                                let id = messenger;
                                let Some(entry) = messengers.get(id) else {
                                    return Action::None;
                                };
                                if !entry.interface.edit {
                                    error!("Edit not supported by this messenger");
                                    return Action::None;
                                }
                                let room_id = *room.id();
                                let Some(edit) = messengers.with_interface_async(
                                    id,
                                    async move |interface| {
                                        let edit = interface.text_edit().map_err(|error| {
                                            Box::new(error) as Box<dyn Error + Send + Sync>
                                        })?;
                                        edit.edit_message(
                                            &room,
                                            &Identifier::new(
                                                message_id,
                                                InterfaceMessage::default(),
                                            ),
                                            InterfaceMessage {
                                                content: Revision {
                                                    at: None,
                                                    text: contents,
                                                },
                                                ..Default::default()
                                            },
                                        )
                                        .await
                                    },
                                ) else {
                                    return Action::None;
                                };
                                Action::Run(
                                    Task::future(async move {
                                        edit.await
                                            .unwrap_or_else(|| Err(closed_messenger_error()))
                                    })
                                    .then(
                                        move |result: Result<_, Box<dyn Error + Send + Sync>>| {
                                            match result {
                                                Ok(updated) => Task::done(Message::ConfirmEdit {
                                                    id,
                                                    room_id,
                                                    updated,
                                                }),
                                                Err(e) => {
                                                    error!("Failed to edit message: {e:#?}");
                                                    Task::none()
                                                }
                                            }
                                        },
                                    ),
                                )
                            }
                            ChatUpdateResult::Delete {
                                messenger,
                                room,
                                message_id,
                            } => {
                                let id = messenger;
                                let Some(entry) = messengers.get(id) else {
                                    return Action::None;
                                };
                                if !entry.interface.delete {
                                    error!("Delete not supported by this messenger");
                                    return Action::None;
                                }
                                let room_id = *room.id();
                                let Some(delete) = messengers.with_interface_async(
                                    id,
                                    async move |interface| {
                                        let delete = interface.text_delete().map_err(|error| {
                                            Box::new(error) as Box<dyn Error + Send + Sync>
                                        })?;
                                        delete
                                            .delete_message(
                                                &room,
                                                &Identifier::new(
                                                    message_id,
                                                    InterfaceMessage::default(),
                                                ),
                                            )
                                            .await
                                    },
                                ) else {
                                    return Action::None;
                                };
                                Action::Run(
                                    Task::future(async move {
                                        delete
                                            .await
                                            .unwrap_or_else(|| Err(closed_messenger_error()))
                                    })
                                    .then(
                                        move |result: Result<(), Box<dyn Error + Send + Sync>>| {
                                            match result {
                                                Ok(()) => Task::done(Message::ConfirmDelete {
                                                    id,
                                                    room_id,
                                                    message_id,
                                                }),
                                                Err(e) => {
                                                    error!("Failed to delete message: {e:#?}");
                                                    Task::none()
                                                }
                                            }
                                        },
                                    ),
                                )
                            }
                            ChatUpdateResult::InspectUser { messenger, user } => {
                                self.start_inspect_user(messenger, user, messengers)
                            }
                            ChatUpdateResult::ToggleReaction {
                                messenger,
                                room,
                                message_id,
                                emoji,
                                reacted,
                            } => {
                                let Some(entry) = messengers.get(messenger) else {
                                    return Action::None;
                                };
                                if !entry.interface.reactions {
                                    error!("Reactions not supported by this messenger");
                                    return Action::None;
                                }
                                let Some(update) = messengers.with_interface_async(
                                    messenger,
                                    async move |interface| {
                                        let reactions =
                                            interface.text_reactions().map_err(|error| {
                                                Box::new(error) as Box<dyn Error + Send + Sync>
                                            });
                                        let Ok(reactions) = reactions else {
                                            error!("Reactions not supported by this messenger");
                                            return;
                                        };
                                        let msg_ident = Identifier::new(
                                            message_id,
                                            InterfaceMessage::default(),
                                        );
                                        let result = if reacted {
                                            reactions
                                                .remove_reaction(&room, &msg_ident, &emoji)
                                                .await
                                        } else {
                                            reactions.add_reaction(&room, &msg_ident, &emoji).await
                                        };
                                        if let Err(e) = result {
                                            error!("Failed to toggle reaction: {e:#?}");
                                        }
                                    },
                                ) else {
                                    return Action::None;
                                };
                                Action::Run(
                                    Task::future(async move {
                                        if update.await.is_none() {
                                            error!("Messenger closed before reaction update");
                                        }
                                    })
                                    .then(|_| Task::none()),
                                )
                            }
                        },
                    };
                }
                Action::None
            }
            Message::Contacts(msg) => match msg {
                ContactsAction::OpenDm { id, user } => {
                    let conversation = messengers.data(id).and_then(|data| {
                        data.conversations.iter().find(|room| {
                            room.members
                                .as_deref()
                                .is_some_and(|members| matches!(members, [m] if m == &user))
                        })
                    });
                    let Some(conversation) = conversation else {
                        warn!("No existing DM conversation with {}", user.name);
                        return Action::None;
                    };

                    let conversation = conversation.clone();
                    self.update(
                        Message::Sidebar(SidebarAction::OpenChat { id, conversation }),
                        messengers,
                    )
                }
                msg => {
                    if let Main::Contacts(contacts) = &mut self.main {
                        return Action::Run(contacts.update(msg).map(Message::Contacts));
                    }
                    Action::None
                }
            },
            Message::Navbar(action) => match action {
                NavbarAction::GetDMs => Action::Run(Task::done(Message::SetSidebarServer(None))),
                NavbarAction::GetGuild { id, server } => {
                    if messengers.get(id).is_none() {
                        return Action::None;
                    }

                    let guild_id = *server.id();

                    // Check if rooms are already loaded in the registry
                    let rooms_loaded = messengers
                        .data(id)
                        .and_then(|d| d.guilds.iter().find(|g| *g.id() == guild_id))
                        .and_then(|g| g.rooms.as_ref())
                        .is_some();

                    if rooms_loaded {
                        return Action::Run(Task::done(Message::SetSidebarServer(Some(
                            Server::new(id, guild_id),
                        ))));
                    }

                    // Fetch rooms and store in registry
                    let Some(rooms) = messengers.with_interface_async(
                        id,
                        async move |interface| {
                            let query = interface.query()?;
                            let house = query.house_details(server).await?;
                            Ok((id, guild_id, house.rooms))
                        },
                    ) else {
                        return Action::None;
                    };
                    Action::Run(
                        Task::future(async move {
                            rooms
                                .await
                                .unwrap_or_else(|| Err(closed_messenger_error()))
                        })
                        .then(
                            |result: Result<_, Box<dyn Error + Send + Sync>>| match result {
                                Ok((id, guild_id, rooms)) => {
                                    Task::done(Message::GuildRoomsLoaded {
                                        id,
                                        guild_id,
                                        rooms: rooms.unwrap_or_default(),
                                    })
                                }
                                Err(e) => {
                                    error!("{e:#?}");
                                    Task::none()
                                }
                            },
                        ),
                    )
                }
            },
            Message::Sidebar(action) => match action {
                SidebarAction::Disconnect(call) => Action::DisconnectFromCall(call),
                SidebarAction::SetCallState(call, state) => Action::SetCallState { call, state },
                SidebarAction::SetVideo(call, kind, enabled) => Action::SetVideo {
                    call,
                    kind,
                    enabled,
                    input: None,
                },
                SidebarAction::PickVideoInput(call, kind) => {
                    self.start_video_input_picker(call, kind)
                }
                SidebarAction::InspectVideoCapture => self.start_inspect_video_capture(),
                SidebarAction::PickScreenCapture => self.start_pick_screen_capture(),
                SidebarAction::SubscribeVideo { messenger, stream } => {
                    Action::SubscribeVideo { messenger, stream }
                }
                SidebarAction::SetUserVolume {
                    messenger,
                    user,
                    volume,
                } => Action::Audio(AudioOp::SetUserVolume {
                    messenger,
                    user,
                    volume,
                }),
                SidebarAction::OpenAfx { target, title } => {
                    self.afx = Some(AfxSheet { target, title });
                    Action::None
                }
                SidebarAction::Call(channel) => {
                    let server = self.sidebar.server_selected.as_ref().unwrap();
                    if messengers.get(server.messenger_id).is_none() {
                        return Action::None;
                    }

                    Action::Call {
                        messenger: server.messenger_id,
                        channel,
                    }
                }
                SidebarAction::OpenContacts => {
                    self.main = Main::Contacts(Contacts::default());
                    Action::None
                }
                SidebarAction::InspectGuild { id, guild } => {
                    self.start_inspect_house(id, guild, messengers)
                }
                SidebarAction::OpenChat { id, conversation } => {
                    if messengers.get(id).is_none() {
                        return Action::None;
                    }

                    // Check cache
                    if let Some(data) = messengers.data(id)
                        && data
                            .room(*conversation.id())
                            .is_some_and(|room| room.messages.is_some())
                    {
                        return Action::Run(Task::done(Message::ChangeMain(Main::Chat(Box::new(
                            Chat::new(id, conversation),
                        )))));
                    }

                    // Otherwise fetch
                    let fetch_conversation = conversation.clone();
                    let Some(messages) = messengers.with_interface_async(
                        id,
                        async move |interface| {
                            let text = interface.messenger().text().map_err(|e| {
                                Box::new(e) as Box<dyn Error + Send + Sync>
                            })?;
                            let msgs = text
                                .get_messages(
                                    &fetch_conversation,
                                    None,
                                    messenger_interface::interface::Ordering::Time,
                                )
                                .await?;

                            Ok((id, fetch_conversation, msgs))
                        },
                    ) else {
                        return Action::None;
                    };
                    Action::Run(Task::batch([
                        Task::done(Message::ChangeMain(Main::Chat(Box::new(Chat::new(
                            id,
                            conversation,
                        ))))),
                        Task::future(async move {
                            messages
                                .await
                                .unwrap_or_else(|| Err(closed_messenger_error()))
                        })
                        .then(
                            |t: Result<_, Box<dyn Error + Send + Sync>>| match t {
                                Ok((id, conversation, msgs)) => Task::done(Message::UpdateChat {
                                    id,
                                    kv: (*conversation.id(), msgs),
                                }),
                                Err(err) => {
                                    error!("{err}");
                                    Task::none()
                                }
                            },
                        ),
                    ]))
                }
            },
        }
    }

    pub(crate) fn view<'a>(
        &'a self,
        messengers: &'a MessengerRegistry,
        audio: &'a AudioControls,
        remote_video: &'a RemoteVideo,
    ) -> iced::Element<'a, Message> {
        let profiles = row![
            Text::from(
                messengers
                    .iter()
                    .next()
                    .map(|(_, entry)| {
                        entry
                            .data
                            .profile
                            .as_ref()
                            .map(|p| p.name.as_str())
                            .unwrap_or("No connection made?")
                    })
                    .unwrap_or("No messengers connected")
            ),
            iced::widget::Space::new().width(Length::Fill),
            Button::new("Settings").on_press(Message::OpenSettings),
        ]
        .align_y(Alignment::Center);

        let navbar = Navbar::get_element(messengers).map(Message::Navbar);

        let window = Responsive::new(move |size| {
            let sidebar = self
                .sidebar
                .view(messengers, audio, remote_video)
                .map(Message::Sidebar);

            let main = match &self.main {
                Main::Contacts(contacts) => contacts.get_element(messengers).map(Message::Contacts),
                Main::Chat(chat) => chat.get_element(messengers).map(Message::Chat),
            };
            row![
                sidebar,
                Divider::new(10.0, size.height, Message::DividerChange),
                main
            ]
            .into()
        });

        let base = Element::from(column![profiles, row![navbar, window]]);

        // Modals: dimmed backdrop closes on click, the card itself swallows
        // clicks. Source selection wins if multiple sheets are somehow open.
        let (card, close): (Element<'a, Message>, Message) =
            if let Some(picker) = &self.video_input_picker {
                (picker.view(), Message::CloseVideoInputPicker)
            } else if let Some(inspect) = &self.inspect {
                (inspect.view(), Message::CloseInspect)
            } else if let Some(afx) = &self.afx {
                (afx.view(audio), Message::CloseAfx)
            } else {
                return base;
            };
        stack![
            base,
            opaque(
                mouse_area(center(opaque(card)).style(|_theme| {
                    container::Style {
                        background: Some(iced::Background::Color(Color {
                            a: 0.6,
                            ..Color::BLACK
                        })),
                        ..Default::default()
                    }
                }))
                .on_press(close)
            )
        ]
        .into()
    }
}
