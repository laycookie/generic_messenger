use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{
    StreamExt,
    future::{Either, select},
};
use futures_timer::Delay;
use iced::Task;
use messenger_interface::interface::{
    AudioEvent, CallState, FrameScaler, PixelData, QueryEvent, TextEvent, VideoEvent, VideoFrame,
    VideoInput as InterfaceVideoInput, VideoResolution, VoiceEvent, frame_channel,
};
use messenger_interface::types::{Emoji, ID, Identifier, Place, Room, User, UserStatus};
use messenger_unifier::{MessengerData, MessengerId, MessengerRegistry};
use simple_audio_channels::{
    AudioMixer, SampleFormat, StreamFormat,
    effects::{Gain, Gate, Mute},
    effects_rack::EffectsRack,
};
use simple_video::CaptureLimits;
use tracing::{debug, trace, warn};

use crate::audio_controls::AudioControls;
use crate::pages::{
    AppMessage, RemoteVideo, SelectedVideoInput, StreamDirection, VideoInputKey, VideoInputSource,
};

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
                            data.rebuild_room_index();
                        }
                    }
                    Some(server_id) => {
                        if let Some(server) =
                            data.guilds.iter_mut().find(|g| g.id() == server_id.id())
                            && let Some(rooms) = server.rooms.as_mut()
                            && !rooms.iter().any(|r| r.id() == room.id())
                        {
                            rooms.push(room);
                            data.rebuild_room_index();
                        } else {
                            warn!("Couldn't find server for channel creation");
                        }
                    }
                }
            }
        }
        QueryEvent::ChannelDeleted {
            r#where: _,
            room_id,
        } => {
            if let Some(data) = messengers.data_mut(id) {
                data.conversations.retain(|room| *room.id() != room_id);
                for guild in &mut data.guilds {
                    if let Some(rooms) = guild.rooms.as_mut() {
                        rooms.retain(|room| *room.id() != room_id);
                    }
                }
                data.calls.retain(|call| call.id() != room_id);
                data.rebuild_room_index();
            }
        }
        QueryEvent::UserStatusChanged { user_id, status } => {
            if let Some(data) = messengers.data_mut(id) {
                apply_user_status(data, user_id, status);
            }
        }
        // The application intercepts this control event before reducer
        // dispatch and rebuilds the whole messenger snapshot.
        QueryEvent::StateInvalidated => {}
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
                if !data.message_is_deleted(room_id, *message.id())
                    && let Some(room) = data.room_mut(room_id)
                {
                    let msgs = room.messages.get_or_insert_with(Vec::new);

                    // A room may contain several pending sends, and this event
                    // may belong to another user. Without a backend-provided
                    // client nonce there is no sound way to correlate it with
                    // one pending row. Keep pending rows until the matching
                    // HTTP send future returns; that result carries the exact
                    // server ID. `ConfirmPending` removes an early socket echo
                    // with the same server ID.
                    if !msgs.iter().any(|m| m.id() == message.id()) {
                        msgs.push(message);
                    }
                }

                data.move_conversation_to_front(room_id);
            }
        }
        TextEvent::MessageUpdated { room, message } => {
            if let Some(data) = messengers.data_mut(id) {
                data.update_message(*room.id(), message);
            }
        }
        TextEvent::MessageDeleted { room, message_id } => {
            if let Some(data) = messengers.data_mut(id) {
                data.delete_message(*room.id(), message_id);
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
    controls: &mut AudioControls,
) -> Task<AppMessage> {
    match event {
        VoiceEvent::CallStatusUpdate { room, status } => {
            debug!("{}", status.as_str());
            if let Some(data) = messengers.data_mut(id)
                && let Some(call) = data.calls.iter_mut().find(|call| call.id() == *room.id())
                && !call.is_connected()
            {
                call.set_state(CallState::Pending(status));
            }
        }
        VoiceEvent::CallStreamReady { room, source } => {
            let Some(stream) = messengers
                .get(id)
                .map(|entry| entry.interface.bind_stream(source))
            else {
                return Task::none();
            };
            if let Some(data) = messengers.data_mut(id)
                && let Some(call) = data.calls.iter_mut().find(|call| call.id() == *room.id())
            {
                call.set_state(CallState::Connected);
                return Task::stream(stream.map(move |event| AppMessage::AudioEvent((id, event))));
            }
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

                // Reconcile our own echo into the (optimistically set) local
                // state: normally a confirmation of the last press, but this
                // is also how an admin's server-mute silences the mic, and how
                // a rejoin re-applies whatever the server reports.
                if data
                    .profile
                    .as_ref()
                    .is_some_and(|profile| profile.id() == user.id())
                {
                    let state = user.voice.unwrap_or_default();
                    debug!(
                        "Applying own voice state: muted={} deafened={}",
                        state.muted, state.deafened
                    );
                    controls.set_mic_muted(state.muted);
                    controls.set_deafened(state.deafened);
                }
            }
        }
        VoiceEvent::ParticipantLeft { user_id } => {
            trace!("{user_id:?} left vc");
            // Their playback channel dies with them; keep only the stored volume.
            controls.detach_user(id, user_id);
            if let Some(data) = messengers.data_mut(id) {
                let left_rooms = remove_voice_participant(data, user_id);

                if data
                    .profile
                    .as_ref()
                    .is_some_and(|profile| *profile.id() == user_id)
                {
                    // Out of the call: audible and un-muted for the next one.
                    controls.set_mic_muted(false);
                    controls.set_deafened(false);
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

/// Cap UI texture churn while preserving realtime motion. Incoming channels
/// remain latest-wins, so a slow render drops stale frames instead of lagging.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(33);
const PREVIEW_MAX_WIDTH: u32 = 320;
const PREVIEW_MAX_HEIGHT: u32 = 180;
const MAX_TEST_PATTERN_PIXELS: usize = 4096 * 2160;

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn capture_limits(resolution: VideoResolution, frames_per_second: u32) -> CaptureLimits {
    CaptureLimits {
        max_width: resolution.width,
        max_height: resolution.height,
        max_frames_per_second: frames_per_second.clamp(1, 30),
    }
}

/// Preserve discovery frame before sampling latest channel frames. Without this
/// separate first item, Discord's frame that creates `AddVideoStream` is lost
/// while application returns producer half of frame channel.
fn preview_frames(
    initial_frame: VideoFrame,
    consumer: messenger_interface::interface::FrameConsumer,
    cancelled: futures::channel::oneshot::Receiver<()>,
) -> impl futures::Stream<Item = VideoFrame> + Send + 'static {
    futures::stream::unfold(
        (
            consumer,
            Box::pin(cancelled),
            Some(initial_frame),
            None::<Instant>,
            FrameScaler::new(),
        ),
        |(mut consumer, cancelled, mut initial_frame, next_refresh, mut scaler)| async move {
            let cancelled = if let Some(deadline) = next_refresh {
                match deadline.checked_duration_since(Instant::now()) {
                    Some(wait) => match select(Box::pin(Delay::new(wait)), cancelled).await {
                        Either::Left(((), cancelled)) => cancelled,
                        Either::Right(_) => return None,
                    },
                    None => cancelled,
                }
            } else {
                cancelled
            };
            let (mut frame, cancelled, is_initial) = match initial_frame.take() {
                Some(frame) => (frame, cancelled, true),
                None => match select(Box::pin(consumer.pop()), cancelled).await {
                    Either::Left((Some(frame), cancelled)) => (frame, cancelled, false),
                    Either::Left((None, _)) | Either::Right(_) => return None,
                },
            };
            let mut newer_frames = 0_u32;
            if !is_initial {
                while let Some(newer) = consumer.try_pop() {
                    frame = newer;
                    newer_frames = newer_frames.saturating_add(1);
                }
            }
            let source_width = frame.width;
            let source_height = frame.height;
            let scale_started = Instant::now();
            let frame = match frame.downscale_to_fit_with(
                &mut scaler,
                PREVIEW_MAX_WIDTH,
                PREVIEW_MAX_HEIGHT,
            ) {
                Ok(frame) => frame,
                Err(err) => {
                    warn!(error = %err, "Couldn't scale incoming video preview");
                    return None;
                }
            };
            trace!(
                target: "video_timing",
                stage = "ui.preview_select",
                source_width,
                source_height,
                output_width = frame.width,
                output_height = frame.height,
                newer_frames,
                scale_us = elapsed_micros(scale_started),
                "video stage complete"
            );
            Some((
                frame,
                (
                    consumer,
                    cancelled,
                    initial_frame,
                    Some(Instant::now() + PREVIEW_INTERVAL),
                    scaler,
                ),
            ))
        },
    )
}

pub fn process_video_event(
    id: MessengerId,
    event: VideoEvent,
    video: &mut RemoteVideo,
    video_inputs: &HashMap<VideoInputKey, SelectedVideoInput>,
) -> Task<AppMessage> {
    match event {
        VideoEvent::VideoStreamAvailable {
            id: stream,
            metadata,
        } => {
            debug!("Video stream available on request: {stream:?}");
            video.available((id, stream), metadata);
            Task::none()
        }
        VideoEvent::AddVideoStream {
            id: stream,
            metadata,
            initial_frame,
            channel,
        } => {
            debug!("Video stream started: {stream:?}");
            let key = (id, stream);
            let (generation, cancelled) = video.start(key, metadata);
            let (producer, consumer) = frame_channel();
            if channel.send(producer).is_err() {
                warn!("Couldn't send video channel to the adapter");
                if video.is_current(&key, generation) {
                    video.remove(&key);
                }
                return Task::none();
            }
            // Sidebar renders only 160 logical pixels wide; helper preserves
            // discovery frame and downsizes before iced uploads it.
            let frames = preview_frames(initial_frame, consumer, cancelled);
            Task::stream(frames.map(move |frame| AppMessage::VideoPreview {
                messenger: id,
                stream,
                generation,
                frame,
            }))
            .chain(Task::done(AppMessage::VideoPreviewEnded {
                messenger: id,
                stream,
                generation,
            }))
        }
        VideoEvent::RemoveVideoStream { id: stream } => {
            debug!("Video stream ended: {stream:?}");
            video.remove(&(id, stream));
            Task::none()
        }
        VideoEvent::AddVideoInput { kind, channel } => {
            let Some(input) = video_inputs.get(&(id, kind)).cloned() else {
                warn!("Video input requested without selected source: kind={kind:?}");
                return Task::none();
            };
            let frames_per_second = input.frames_per_second.clamp(1, 30);
            let resolution = input.resolution;
            let bitrate_bps = input.bitrate_bps;
            let generation = input.generation;
            let input_key = (id, kind);

            match input.source {
                VideoInputSource::CyanTestPattern => {
                    debug!("Feeding cyan test pattern: kind={kind:?}");
                    let pixels = usize::try_from(resolution.width)
                        .ok()
                        .and_then(|width| {
                            usize::try_from(resolution.height)
                                .ok()
                                .and_then(|height| width.checked_mul(height))
                        })
                        .filter(|pixels| *pixels > 0 && *pixels <= MAX_TEST_PATTERN_PIXELS);
                    let Some(pixels) = pixels else {
                        return Task::done(AppMessage::VideoCaptureResult {
                            key: input_key,
                            generation,
                            result: Err(format!(
                                "Invalid test-pattern resolution: {}x{}",
                                resolution.width, resolution.height
                            )),
                        });
                    };
                    let (producer, consumer) = frame_channel();
                    if channel
                        .send(InterfaceVideoInput::new(
                            consumer,
                            frames_per_second,
                            resolution,
                            bitrate_bps,
                        ))
                        .is_err()
                    {
                        warn!("Couldn't send video input channel to the adapter");
                        return Task::none();
                    }
                    Task::future(async move {
                        let mut producer = producer;
                        let (width, height) = (resolution.width, resolution.height);
                        let cyan: Arc<[u8]> = [0, 255, 255, 255].repeat(pixels).into();
                        while !producer.is_closed() {
                            producer.push(VideoFrame::new(
                                width,
                                height,
                                // Static frame: clone Arc header, not several
                                // megabytes of pixels every capture interval.
                                PixelData::SharedRgba8(cyan.clone()),
                            ));
                            Delay::new(Duration::from_secs_f64(1.0 / f64::from(frames_per_second)))
                                .await;
                        }
                        debug!("Cyan video input ended (consumer dropped)");
                        AppMessage::VideoCaptureResult {
                            key: input_key,
                            generation,
                            result: Ok(()),
                        }
                    })
                }
                VideoInputSource::Capture(source) => {
                    let source_name = source.name().to_owned();
                    let limits = capture_limits(resolution, frames_per_second);
                    debug!("Opening {kind:?} video input: {source_name}");
                    Task::future(async move {
                        let open_started = Instant::now();
                        let stream = match source.stream_with_limits(limits).await {
                            Ok(stream) => stream,
                            Err(error) => {
                                return AppMessage::VideoCaptureResult {
                                    key: input_key,
                                    generation,
                                    result: Err(format!(
                                        "Couldn't open video input {source_name}: {error}"
                                    )),
                                };
                            }
                        };
                        trace!(
                            target: "video_timing",
                            stage = "capture.open",
                            ?kind,
                            source = %source_name,
                            elapsed_us = elapsed_micros(open_started),
                            "video stage complete"
                        );
                        let (consumer, completion) = stream.into_parts();
                        if channel
                            .send(InterfaceVideoInput::new(
                                consumer,
                                frames_per_second,
                                resolution,
                                bitrate_bps,
                            ))
                            .is_err()
                        {
                            debug!(
                                "Video input request was cancelled while opening: {source_name}"
                            );
                            return AppMessage::VideoCaptureResult {
                                key: input_key,
                                generation,
                                result: Ok(()),
                            };
                        }
                        let result = completion.wait().await.map_err(|error| {
                            format!("Video capture failed for {source_name}: {error}")
                        });
                        AppMessage::VideoCaptureResult {
                            key: input_key,
                            generation,
                            result,
                        }
                    })
                }
            }
        }
    }
}

pub fn process_audio_event(
    id: MessengerId,
    event: AudioEvent,
    audio: &mut AudioMixer,
    controls: &mut AudioControls,
) -> Task<AppMessage> {
    match event {
        AudioEvent::AddAudioSource { user, channel } => {
            // Voice backends push opus-decoded i16 stereo at 48kHz.
            let (mut rack, backend) = EffectsRack::new();
            let deafen_mute = Mute::new();
            let deafen_switch = deafen_mute.switch();
            let deafen_id = rack.insert(Box::new(deafen_mute));
            let volume = user
                .map(|user| controls.user_volume(id, user))
                .unwrap_or(1.0);
            let gain = rack.insert(Box::new(Gain::with_multiplier(volume)));
            let producer = match audio.create_output_channel(
                StreamFormat {
                    channels: 2,
                    sample_format: SampleFormat::I16,
                    sample_rate: 48_000,
                },
                backend,
            ) {
                Ok(producer) => producer,
                Err(error) => {
                    return Task::done(AppMessage::MediaError(format!(
                        "Couldn't create voice playback channel: {error}"
                    )));
                }
            };

            if channel.send(producer).is_err() {
                warn!("Couldn't send audio channel to the adapter");
                return Task::none();
            }
            controls.attach_output_mute(id, user, deafen_switch);

            // Known speaker: keep the rack frontend in their controls, so the
            // participant volume slider / "+ afx" sheet reach the backend. The
            // Gain is seeded with any volume set before (or between) channels.
            if let Some(user) = user {
                controls.attach_user(id, user, rack, gain, vec![deafen_id]);
            }

            if !audio.is_streaming_output() {
                return Task::done(AppMessage::StartStream(StreamDirection::Output));
            }
        }
        AudioEvent::AddAudioInput(sender) => {
            // Noise-gate the microphone near the device noise floor, holding
            // across the gaps between words so transmission is not chopped,
            // then closing fully so silence parks.
            let (mut rack, backend) = EffectsRack::new();
            let mic_mute = Mute::new();
            let mic_mute_switch = mic_mute.switch();
            let mic_mute_id = rack.insert(Box::new(mic_mute));
            rack.insert(Box::new(
                Gate::new().tuned(|gate| gate.threshold_db = -54.0),
            ));

            // Voice backends encode f32 stereo frames at 48kHz.
            let input = match audio.create_input_channel(
                StreamFormat {
                    channels: 2,
                    sample_format: SampleFormat::F32,
                    sample_rate: 48_000,
                },
                backend,
            ) {
                Ok(input) => input,
                Err(error) => {
                    return Task::done(AppMessage::MediaError(format!(
                        "Couldn't create microphone channel: {error}"
                    )));
                }
            };

            if sender.send(input).is_err() {
                warn!("Couldn't send audio input channel to the adapter");
                return Task::none();
            }
            controls.attach_mic(id, rack, mic_mute_switch, vec![mic_mute_id]);

            if !audio.is_streaming_input() {
                return Task::done(AppMessage::StartStream(StreamDirection::Input));
            }
        }
    }
    Task::none()
}

#[cfg(test)]
mod video_tests {
    use futures::channel::oneshot;
    use messenger_interface::interface::{VideoBroadcastKind, VideoStreamId, VideoStreamMetadata};

    use super::*;
    use crate::pages::RemoteVideoStatus;

    fn stream(kind: VideoBroadcastKind) -> (VideoStreamId, VideoStreamMetadata) {
        (
            VideoStreamId::new(7),
            VideoStreamMetadata::new(Some(7), Some(11), kind),
        )
    }

    #[test]
    fn advertised_source_waits_for_ui_request() {
        let messenger = MessengerId::default();
        let (stream, metadata) = stream(VideoBroadcastKind::Screen);
        let mut video = RemoteVideo::default();
        let video_inputs = HashMap::new();

        let _task = process_video_event(
            messenger,
            VideoEvent::VideoStreamAvailable {
                id: stream,
                metadata,
            },
            &mut video,
            &video_inputs,
        );

        assert_eq!(
            video
                .streams
                .get(&(messenger, stream))
                .map(|stream| stream.status),
            Some(RemoteVideoStatus::Available)
        );
        assert!(
            video
                .streams
                .values()
                .all(|stream| stream.preview.is_none())
        );
    }

    #[test]
    fn automatic_source_attaches_immediately() {
        let messenger = MessengerId::default();
        let (stream, metadata) = stream(VideoBroadcastKind::Camera);
        let mut video = RemoteVideo::default();
        let video_inputs = HashMap::new();
        let (channel, mut reply) = oneshot::channel();

        let task = process_video_event(
            messenger,
            VideoEvent::AddVideoStream {
                id: stream,
                metadata,
                initial_frame: VideoFrame::new(1, 1, PixelData::Rgba8(vec![1, 2, 3, 255])),
                channel,
            },
            &mut video,
            &video_inputs,
        );

        assert_eq!(
            video
                .streams
                .get(&(messenger, stream))
                .map(|stream| stream.status),
            Some(RemoteVideoStatus::Active)
        );
        assert!(matches!(reply.try_recv(), Ok(Some(_))));
        drop(task);
    }

    #[test]
    fn preview_emits_discovery_frame_without_channel_activity() {
        futures::executor::block_on(async {
            let pixels = vec![1, 2, 3, 255, 5, 6, 7, 255];
            let initial = VideoFrame::new(2, 1, PixelData::Rgba8(pixels.clone()));
            let (_producer, consumer) = frame_channel();
            let (_cancel, cancelled) = oneshot::channel();
            let mut frames = Box::pin(preview_frames(initial, consumer, cancelled));

            let first = frames.next().await.expect("discovery frame should render");

            assert_eq!(first.width, 2);
            assert_eq!(first.height, 1);
            assert_eq!(first.pixel_data, PixelData::Rgba8(pixels));
        });
    }

    #[test]
    fn replaced_stream_rejects_old_channel_generation() {
        let messenger = MessengerId::default();
        let (stream, metadata) = stream(VideoBroadcastKind::Camera);
        let key = (messenger, stream);
        let mut video = RemoteVideo::default();

        let (old, _old_cancelled) = video.start(key, metadata);
        let (current, _current_cancelled) = video.start(key, metadata);

        assert!(!video.is_current(&key, old));
        assert!(video.is_current(&key, current));
    }

    #[test]
    fn replacement_preserves_explicit_subscription_ownership() {
        let messenger = MessengerId::default();
        let (stream, metadata) = stream(VideoBroadcastKind::Camera);
        let key = (messenger, stream);
        let mut video = RemoteVideo::default();
        video.available(key, metadata);
        let request = video.begin_subscription(&key).unwrap();

        let (_first, _cancelled) = video.start(key, metadata);
        assert!(video.is_current_subscription(&key, request));
        let (_replacement, _cancelled) = video.start(key, metadata);

        assert_eq!(video.owned_subscriptions().collect::<Vec<_>>(), vec![key]);
    }

    #[test]
    fn stale_subscription_failure_cannot_roll_back_recreated_stream() {
        let messenger = MessengerId::default();
        let (stream, metadata) = stream(VideoBroadcastKind::Camera);
        let key = (messenger, stream);
        let mut video = RemoteVideo::default();
        video.available(key, metadata);
        let old = video.begin_subscription(&key).unwrap();
        video.remove(&key);
        video.available(key, metadata);
        let current = video.begin_subscription(&key).unwrap();

        video.subscription_failed(&key, old);

        assert!(!video.is_current_subscription(&key, old));
        assert!(video.is_current_subscription(&key, current));
        assert_eq!(
            video.streams.get(&key).map(|stream| stream.status),
            Some(RemoteVideoStatus::Requested)
        );
    }

    #[test]
    fn clear_cancels_active_preview_consumer() {
        let messenger = MessengerId::default();
        let (stream, metadata) = stream(VideoBroadcastKind::Camera);
        let key = (messenger, stream);
        let mut video = RemoteVideo::default();
        let (_generation, cancelled) = video.start(key, metadata);

        video.clear();

        assert!(futures::executor::block_on(cancelled).is_ok());
    }

    #[test]
    fn selected_cyan_input_is_attached_to_backend_request() {
        let messenger = MessengerId::default();
        let kind = VideoBroadcastKind::Camera;
        let mut video = RemoteVideo::default();
        let mut video_inputs = HashMap::new();
        video_inputs.insert(
            (messenger, kind),
            SelectedVideoInput {
                source: VideoInputSource::CyanTestPattern,
                frames_per_second: 15,
                resolution: VideoResolution::new(640, 360),
                bitrate_bps: 2_500_000,
                generation: 0,
            },
        );
        let (channel, mut reply) = oneshot::channel();

        let task = process_video_event(
            messenger,
            VideoEvent::AddVideoInput { kind, channel },
            &mut video,
            &video_inputs,
        );

        let input = reply.try_recv().unwrap().unwrap();
        assert_eq!(input.frames_per_second, 15);
        assert_eq!(input.resolution, VideoResolution::new(640, 360));
        assert_eq!(input.bitrate_bps, 2_500_000);
        drop(task);
    }
}
