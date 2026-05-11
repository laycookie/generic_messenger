use futures::StreamExt;
use iced::Task;
use messenger_interface::interface::{AudioEvent, CallStatus, QueryEvent, TextEvent, VoiceEvent};
use simple_audio_channels::{AudioMixer, SampleFormat};
use tracing::{debug, error, warn};

use crate::pages::{AppMessage, StreamDirection};
use crate::state::{MessengerId, MessengerRegistry};

pub fn process_query_event(
    id: MessengerId,
    event: QueryEvent,
    messengers: &mut MessengerRegistry,
) -> Task<AppMessage> {
    match event {
        QueryEvent::ChannelCreated { r#where, room } => {
            if let Some(data) = messengers.data_mut(id) {
                match r#where {
                    None => {
                        if !data.conversations.iter().any(|r| r.id() == room.id()) {
                            data.conversations.push(room);
                        }
                    }
                    Some(server_id) => {
                        if let Some(server) =
                            data.guilds.iter_mut().find(|g| g.id() == server_id.id())
                            && let Some(rooms) = server.rooms.as_mut()
                            && !rooms.iter().any(|r| r.id() == room.id())
                        {
                            rooms.push(room);
                        } else {
                            warn!("Couldn't find server for channel creation");
                        }
                    }
                }
            }
        }
    }
    Task::none()
}

pub fn process_text_event(
    id: MessengerId,
    event: TextEvent,
    messengers: &mut MessengerRegistry,
) -> Task<AppMessage> {
    match event {
        TextEvent::MessageCreated { room, message } => {
            if let Some(data) = messengers.data_mut(id) {
                let room_id = *room.id();
                let msgs = data.chats.entry(room_id).or_default();

                // Check if there's a pending message for this room we should replace
                if let Some(pending_pos) =
                    data.pending_sends.iter().position(|p| p.room_id == room_id)
                {
                    let pending_id = data.pending_sends.remove(pending_pos).pending_id;
                    if let Some(pending_msg) = msgs.iter_mut().find(|m| *m.id() == pending_id) {
                        *pending_msg = message;
                    } else {
                        msgs.push(message);
                    }
                } else if !msgs.iter().any(|m| m.id() == message.id()) {
                    // No pending send — append if not a duplicate
                    msgs.push(message);
                }

                // Move this conversation to the front of the DM list (most recent first)
                if let Some(pos) = data.conversations.iter().position(|c| c.id() == room.id())
                    && pos != 0
                {
                    let conv = data.conversations.remove(pos);
                    data.conversations.insert(0, conv);
                }
            }
        }
        TextEvent::MessageUpdated { room, message } => {
            if let Some(data) = messengers.data_mut(id)
                && let Some(msgs) = data.chats.get_mut(room.id())
                && let Some(existing) = msgs.iter_mut().find(|m| m.id() == message.id())
            {
                *existing = message;
            }
        }
        TextEvent::MessageDeleted { room, message_id } => {
            if let Some(data) = messengers.data_mut(id)
                && let Some(msgs) = data.chats.get_mut(room.id())
            {
                msgs.retain(|m| *m.id() != message_id);
            }
        }
        TextEvent::ReactionAdded {
            room,
            message_id,
            user_id,
            emoji,
        } => {
            if let Some(data) = messengers.data_mut(id) {
                let is_self = data.profile.as_ref().is_some_and(|p| *p.id() == user_id);

                if let Some(msgs) = data.chats.get_mut(room.id())
                    && let Some(msg) = msgs.iter_mut().find(|m| *m.id() == message_id)
                {
                    if let Some(reaction) = msg.reactions.iter_mut().find(|r| r.emoji == emoji) {
                        reaction.count += 1;
                        if is_self {
                            reaction.reacted = true;
                        }
                    } else {
                        msg.reactions.push(messenger_interface::types::Reaction {
                            emoji,
                            count: 1,
                            reacted: is_self,
                        });
                    }
                }
            }
        }
        TextEvent::ReactionRemoved {
            room,
            message_id,
            user_id,
            emoji,
        } => {
            if let Some(data) = messengers.data_mut(id) {
                let is_self = data.profile.as_ref().is_some_and(|p| *p.id() == user_id);

                if let Some(msgs) = data.chats.get_mut(room.id())
                    && let Some(msg) = msgs.iter_mut().find(|m| *m.id() == message_id)
                    && let Some(reaction) = msg.reactions.iter_mut().find(|r| r.emoji == emoji)
                {
                    reaction.count = reaction.count.saturating_sub(1);
                    if is_self {
                        reaction.reacted = false;
                    }
                    if reaction.count == 0 {
                        msg.reactions.retain(|r| r.emoji != emoji);
                    }
                }
            }
        }
    }
    Task::none()
}

pub fn process_voice_event(id: MessengerId, event: VoiceEvent) -> Task<AppMessage> {
    match event {
        VoiceEvent::CallStatusUpdate(status) => match status {
            CallStatus::Connected(stream) => {
                return Task::stream(stream.map(move |event| AppMessage::AudioEvent((id, event))));
            }
            CallStatus::Connecting(msg) => debug!("{msg}"),
            CallStatus::Failed => error!("Call connection failed"),
        },
    }
    Task::none()
}

pub fn process_audio_event(
    _id: MessengerId,
    event: AudioEvent,
    audio: &mut AudioMixer,
) -> Task<AppMessage> {
    match event {
        AudioEvent::AddAudioSource(sender) => {
            let producer = audio
                .create_output_channel(2, SampleFormat::I16, 48_000)
                .unwrap();

            if sender.send(producer).is_err() {
                warn!("Couldn't send audio channel to the adapter");
            }

            if !audio.is_streaming_output() {
                return Task::done(AppMessage::StartStream(StreamDirection::Output));
            }
        }
        AudioEvent::AddAudioInput(sender) => {
            let input = audio
                .create_input_channel(2, SampleFormat::I16, 48_000)
                .unwrap();

            if sender.send(input).is_err() {
                warn!("Couldn't send audio input channel to the adapter");
            }

            if !audio.is_streaming_input() {
                return Task::done(AppMessage::StartStream(StreamDirection::Input));
            }
        }
    }
    Task::none()
}
