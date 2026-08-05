use iced::{
    Alignment, Element, Length,
    widget::{Button, Radio, Space, column, container, row, text},
};
use messenger_interface::interface::VideoCodec;

#[derive(Debug, Clone, Copy)]
pub enum Message {
    Back,
    RestartInput,
    RestartOutput,
    RedetectAudio,
    SetVideoCodec(VideoCodec),
    ResetVideo,
    RetryMessengerData,
    ClearError,
}

pub struct Status<'a> {
    pub input_streaming: bool,
    pub output_streaming: bool,
    pub active_remote_video: usize,
    pub configured_video_inputs: usize,
    pub preferred_video_codec: VideoCodec,
    pub media_error: Option<&'a str>,
}

pub fn view<'a>(status: Status<'a>) -> Element<'a, Message> {
    let title = row![
        text("Settings").size(28),
        Space::new().width(Length::Fill),
        Button::new("Back").on_press(Message::Back),
    ]
    .align_y(Alignment::Center);

    let input_state = if status.input_streaming {
        "running"
    } else {
        "stopped"
    };
    let output_state = if status.output_streaming {
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

    if let Some(error) = status.media_error {
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
            status.active_remote_video, status.configured_video_inputs
        )),
        text("Outgoing codec"),
        row![
            Radio::new(
                "H.264",
                VideoCodec::H264,
                Some(status.preferred_video_codec),
                Message::SetVideoCodec,
            ),
            Radio::new(
                "AV1",
                VideoCodec::Av1,
                Some(status.preferred_video_codec),
                Message::SetVideoCodec,
            ),
        ]
        .spacing(12),
        text("H.264 favors compatibility; AV1 saves bandwidth but uses more CPU.").size(12),
        text("Reset stops outgoing camera/screen broadcasts and drops stale previews."),
        Button::new("Reset video state").on_press(Message::ResetVideo),
    ]
    .spacing(10);

    let data = column![
        text("Messenger data").size(22),
        text("Re-fetch profiles, contacts, rooms, and servers; live events remain buffered."),
        Button::new("Retry messenger data").on_press(Message::RetryMessengerData),
    ]
    .spacing(10);

    container(column![title, audio, video, data].spacing(28))
        .padding(24)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
