use futures::StreamExt;
use iced::Task;
use messenger_interface::interface::{AudioEvent, CallState, QueryEvent, TextEvent, VoiceEvent};
use messenger_interface::types::{Emoji, ID, Identifier, Place, Room, User, UserStatus};
use messenger_unifier::{MessengerData, MessengerId, MessengerRegistry};
use simple_audio_channels::{
    AudioMixer, SampleFormat, StreamFormat,
    effects::{Gate, GateSettings},
};
use tracing::{debug, trace, warn};

use crate::pages::{AppMessage, StreamDirection};

fn apply_user_status_to_user(user: &mut Identifier<User>, user_id: ID, status: UserStatus) {
    if *user.id() == user_id {
        user.status = Some(status);
    }
}

fn apply_user_status_to_room(room: &mut Identifier<Place<Room>>, user_id: ID, status: UserStatus) {
    if let Some(members) = room.members.as_mut() {
        for member in members {
            apply_user_status_to_user(member, user_id, status);
        }
    }

    if let Some(participants) = room.participants.as_mut() {
        for participant in participants {
            apply_user_status_to_user(participant, user_id, status);
        }
    }

    if let Some(messages) = room.messages.as_mut() {
        for message in messages {
            if let Some(author) = message.author.as_mut() {
                apply_user_status_to_user(author, user_id, status);
            }
        }
    }
}

pub fn apply_user_status(data: &mut MessengerData, user_id: ID, status: UserStatus) {
    if let Some(profile) = data.profile.as_mut() {
        apply_user_status_to_user(profile, user_id, status);
    }

    for contact in &mut data.contacts {
        apply_user_status_to_user(contact, user_id, status);
    }

    for room in &mut data.conversations {
        apply_user_status_to_room(room, user_id, status);
    }

    for guild in &mut data.guilds {
        if let Some(rooms) = guild.rooms.as_mut() {
            for room in rooms {
                apply_user_status_to_room(room, user_id, status);
            }
        }
    }

    for call in &mut data.calls {
        apply_user_status_to_room(call.source_mut(), user_id, status);
    }
}

/// Drop a user from every room's voice roster (`Room.participants`).
/// Room membership (`Room.members`) is deliberately untouched: leaving a
/// call does not remove someone from the room itself.
fn remove_voice_participant(data: &mut MessengerData, user_id: ID) -> Vec<ID> {
    let mut removed_from = Vec::new();

    for room in &mut data.conversations {
        if let Some(participants) = room.participants.as_mut() {
            let participant_count = participants.len();
            participants.retain(|participant| *participant.id() != user_id);
            if participants.len() != participant_count && !removed_from.contains(room.id()) {
                removed_from.push(*room.id());
            }
        }
    }

    for guild in &mut data.guilds {
        let Some(rooms) = guild.rooms.as_mut() else {
            continue;
        };

        for room in rooms {
            if let Some(participants) = room.participants.as_mut() {
                let participant_count = participants.len();
                participants.retain(|participant| *participant.id() != user_id);
                if participants.len() != participant_count && !removed_from.contains(room.id()) {
                    removed_from.push(*room.id());
                }
            }
        }
    }

    for call in &mut data.calls {
        if let Some(participants) = call.source_mut().participants.as_mut() {
            let participant_count = participants.len();
            participants.retain(|participant| *participant.id() != user_id);
            if participants.len() != participant_count && !removed_from.contains(&call.id()) {
                removed_from.push(call.id());
            }
        }
    }

    removed_from
}

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
        QueryEvent::UserStatusChanged { user_id, status } => {
            if let Some(data) = messengers.data_mut(id) {
                apply_user_status(data, user_id, status);
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
                let pending_id = data
                    .pending_sends
                    .iter()
                    .position(|p| p.room_id == room_id)
                    .map(|pending_pos| data.pending_sends.remove(pending_pos).pending_id);

                if let Some(room) = data.room_mut(room_id) {
                    let msgs = room.messages.get_or_insert_with(Vec::new);

                    // Check if there's a pending message for this room we should replace
                    if let Some(pending_id) = pending_id {
                        if let Some(pending_msg) = msgs.iter_mut().find(|m| *m.id() == pending_id) {
                            *pending_msg = message;
                        } else {
                            msgs.push(message);
                        }
                    } else if !msgs.iter().any(|m| m.id() == message.id()) {
                        // No pending send — append if not a duplicate
                        msgs.push(message);
                    }
                }

                data.move_conversation_to_front(room_id);
            }
        }
        TextEvent::MessageUpdated { room, message } => {
            if let Some(data) = messengers.data_mut(id)
                && let Some(room) = data.room_mut(*room.id())
                && let Some(msgs) = room.messages.as_mut()
                && let Some(existing) = msgs.iter_mut().find(|m| m.id() == message.id())
            {
                *existing = message;
            }
        }
        TextEvent::MessageDeleted { room, message_id } => {
            if let Some(data) = messengers.data_mut(id)
                && let Some(room) = data.room_mut(*room.id())
                && let Some(msgs) = room.messages.as_mut()
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

                if let Some(room) = data.room_mut(*room.id())
                    && let Some(msgs) = room.messages.as_mut()
                    && let Some(msg) = msgs.iter_mut().find(|m| *m.id() == message_id)
                {
                    if let Some(reaction) = msg
                        .reactions
                        .iter_mut()
                        .find(|r| r.emoji.shortcode == emoji)
                    {
                        reaction.count += 1;
                        if is_self {
                            reaction.reacted = true;
                        }
                    } else {
                        msg.reactions.push(messenger_interface::types::Reaction {
                            emoji: Emoji::shortcode(emoji),
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

                if let Some(room) = data.room_mut(*room.id())
                    && let Some(msgs) = room.messages.as_mut()
                    && let Some(msg) = msgs.iter_mut().find(|m| *m.id() == message_id)
                    && let Some(reaction) = msg
                        .reactions
                        .iter_mut()
                        .find(|r| r.emoji.shortcode == emoji)
                {
                    reaction.count = reaction.count.saturating_sub(1);
                    if is_self {
                        reaction.reacted = false;
                    }
                    if reaction.count == 0 {
                        msg.reactions.retain(|r| r.emoji.shortcode != emoji);
                    }
                }
            }
        }
    }
    Task::none()
}

pub fn process_voice_event(
    id: MessengerId,
    event: VoiceEvent,
    messengers: &mut MessengerRegistry,
) -> Task<AppMessage> {
    match event {
        VoiceEvent::CallStatusUpdate(status) => {
            debug!("{}", status.as_str());
            if let Some(data) = messengers.data_mut(id) {
                for call in &mut data.calls {
                    call.set_state(CallState::Pending(status));
                }
            }
        }
        VoiceEvent::CallStreamReady(stream) => {
            if let Some(data) = messengers.data_mut(id) {
                for call in &mut data.calls {
                    call.set_state(CallState::Connected);
                }
            }
            return Task::stream(stream.map(move |event| AppMessage::AudioEvent((id, event))));
        }
        VoiceEvent::ParticipantJoined { room, user } => {
            trace!("{:?} joined {room:?}", user.id());
            if let Some(data) = messengers.data_mut(id) {
                remove_voice_participant(data, *user.id());

                let add = |participants: &mut Option<Vec<Identifier<User>>>| {
                    let participants = participants.get_or_insert_with(Vec::new);
                    participants.retain(|participant| participant.id() != user.id());
                    participants.push(user.clone());
                };

                if let Some(room) = data.room_mut(*room.id()) {
                    add(&mut room.participants);
                }
                // Otherwise the containing guild hasn't been opened in the
                // UI yet — the discord crate owns the authoritative voice
                // roster and will fold it into `Room.participants` when
                // `house_details` is fetched, so nothing is lost here.

                for call in &mut data.calls {
                    if call.id() == *room.id() {
                        add(&mut call.source_mut().participants);
                    }
                }
            }
        }
        VoiceEvent::ParticipantLeft { user_id } => {
            trace!("{user_id:?} left vc");
            if let Some(data) = messengers.data_mut(id) {
                let left_rooms = remove_voice_participant(data, user_id);

                if data
                    .profile
                    .as_ref()
                    .is_some_and(|profile| *profile.id() == user_id)
                {
                    if left_rooms.is_empty() && data.calls.len() == 1 {
                        data.calls.clear();
                    } else {
                        data.calls
                            .retain(|call| !left_rooms.iter().any(|room_id| call.id() == *room_id));
                    }
                }
            }
        }
    }
    Task::none()
}

pub fn process_audio_event(
    _id: MessengerId,
    event: AudioEvent,
    audio: &mut AudioMixer,
) -> Task<AppMessage> {
    match event {
        AudioEvent::AddAudioSource { channel, .. } => {
            // Discord voice pushes opus-decoded i16 stereo at 48kHz.
            let producer = audio
                .create_output_channel(
                    StreamFormat {
                        channels: 2,
                        sample_format: SampleFormat::I16,
                        sample_rate: 48_000,
                    },
                    Vec::new(),
                )
                .unwrap();

            if channel.send(producer).is_err() {
                warn!("Couldn't send audio channel to the adapter");
            }

            if !audio.is_streaming_output() {
                return Task::done(AppMessage::StartStream(StreamDirection::Output));
            }
        }
        AudioEvent::AddAudioInput(sender) => {
            // Discord voice encodes f32 stereo frames at 48kHz.
            let input = audio
                .create_input_channel(
                    StreamFormat {
                        channels: 2,
                        sample_format: SampleFormat::F32,
                        sample_rate: 48_000,
                    },
                    // Noise-gate the microphone near the device noise floor,
                    // holding across the gaps between words so transmission is
                    // not chopped, then closing fully so silence parks.
                    vec![Box::new(Gate::new(GateSettings {
                        threshold_db: -54.0,
                        ..Default::default()
                    }))],
                )
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
