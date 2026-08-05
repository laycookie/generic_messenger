use std::{
    collections::HashMap,
    io,
    sync::{Arc, atomic::Ordering},
};

use facet_pretty::FacetPretty;
use futures::future::join_all;
use messenger_interface::{
    interface::{QueryEvent, TextEvent, VideoBroadcastKind},
    types::{Identifier, Message as GlobalMessage, User as GlobalUser},
};
use tracing::{debug, error, trace, warn};

use super::{
    GatewayEvent, Opcode,
    events::{EventContext, StreamRole},
    payloads::{
        ChannelDeletePayload, GuildDeletePayload, GuildEmojisUpdatePayload,
        GuildStickersUpdatePayload, MessageDelete, MessageDeleteBulk, MessageReactionChange,
        PresencePayload, ReadyGuildPayload, ReadyPayload, RelationshipPayload,
        SessionObjectPayload, StreamCreatePayload, StreamDeletePayload, StreamServerUpdatePayload,
        VoiceServerUpdatePayload, VoiceStatePayload,
    },
    recording::RecordedEvent,
};
use crate::{
    Discord, InnerDiscord, UnitStruct,
    api::{self, Message},
    downloaders::CdnImage,
    gateways::{
        GatewayFrame,
        media::{ConnectProgress, Endpoint, StreamKey},
    },
};

impl GatewayFrame<Opcode> {
    /// Dispatch Discord Gateway events into interface caches and event queues.
    ///
    /// <https://docs.discord.food/gateway/gateway-events#receive-events>
    pub(in crate::gateways) async fn exec<T: UnitStruct>(
        self,
        discord: &InnerDiscord<T>,
        gateway: &crate::gateways::Gateway<super::General>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let payload = self.payload()?;
        let context = EventContext::new(&discord.cache, &discord.media, gateway);

        if let Some(s) = payload.sequence() {
            gateway
                .state
                .last_sequence_number
                .get_or_init(|| s.into())
                .store(s, Ordering::Relaxed);
        };

        match payload.opcode() {
            Opcode::Hello => {}
            Opcode::Dispatch => {
                let Some(event_name) = payload.event_type() else {
                    warn!("Dispatch opcode received without an event type (t)");
                    return Ok(());
                };
                debug!("Dispatch event: {}", event_name.pretty());
                match event_name {
                    GatewayEvent::Ready => {
                        let ready = payload.decode::<ReadyPayload>()?;
                        // Reconnects reuse `InnerDiscord`. Clear every
                        // session-scoped cache before publishing this Ready,
                        // including empty collections, so deleted entities do
                        // not survive reconnect.
                        discord.cache.clear_for_ready();

                        if let Some(user) = ready.user {
                            discord.cache.profile.store(Some(Arc::new(api::Profile {
                                id: user.id,
                                username: user.username,
                                global_name: user.global_name,
                                avatar: user.avatar,
                            })));
                        }

                        discord
                            .cache
                            .replace_dm_channels(ready.private_channels.unwrap_or_default());

                        let mut cached_relationships = Vec::new();
                        for relationship in ready.relationships.unwrap_or_default() {
                            match facet_value::from_value::<api::Friend>(relationship) {
                                Ok(relationship) => cached_relationships.push(relationship),
                                Err(err) => warn!("Skipping unparseable relationship: {err}"),
                            }
                        }
                        discord.cache.replace_relationships(cached_relationships);

                        if let Some(presences) = ready.presences {
                            context.emit_presence_values(presences);
                        }

                        let mut merged_members =
                            ready.merged_members.unwrap_or_default().into_iter();
                        let mut cached_guilds = Vec::new();
                        // Collect one future per voice participant and resolve
                        // them concurrently below instead of awaiting each in
                        // turn. Every future awaits an avatar download; doing
                        // them sequentially blocks the dispatch task — and with
                        // it the heartbeat (see the `select!` in `polling.rs`) —
                        // for the *sum* of every download, which on a cold image
                        // cache can exceed the heartbeat interval and get the
                        // gateway dropped. Voice state was just cleared and a
                        // user appears in at most one voice state, so these are
                        // independent (no cross-eviction) and order-free.
                        let mut voice_participant_futures = Vec::new();

                        for guild_payload in ready.guilds.unwrap_or_default() {
                            if let (Some(guild_id), Some(channels)) =
                                (guild_payload.id, guild_payload.channels)
                            {
                                discord.cache.replace_guild_channels(guild_id, channels);
                            }

                            // Custom emoji / stickers for the picker. Parsed
                            // leniently per entry so one odd object doesn't
                            // cost the whole guild its palette.
                            if let Some(guild_id) = guild_payload.id {
                                if let Some(emojis) = guild_payload.emojis {
                                    let emojis = emojis
                                        .into_iter()
                                        .filter_map(|emoji| {
                                            facet_value::from_value::<api::GuildEmoji>(emoji)
                                                .inspect_err(|err| {
                                                    warn!("Skipping unparseable guild emoji: {err}")
                                                })
                                                .ok()
                                        })
                                        .collect();
                                    discord.cache.guild_emojis.insert(guild_id, emojis);
                                }
                                if let Some(stickers) = guild_payload.stickers {
                                    let stickers = stickers
                                        .into_iter()
                                        .filter_map(|sticker| {
                                            facet_value::from_value::<api::GuildSticker>(sticker)
                                                .inspect_err(|err| {
                                                    warn!(
                                                        "Skipping unparseable guild sticker: {err}"
                                                    )
                                                })
                                                .ok()
                                        })
                                        .collect();
                                    discord.cache.guild_stickers.insert(guild_id, stickers);
                                }
                            }

                            if let Some(presences) = guild_payload.presences {
                                context.emit_presence_values(presences);
                            }

                            if let Some(properties) = guild_payload.properties {
                                cached_guilds.push(properties);
                            }

                            let mut guild_members = guild_payload.members.unwrap_or_default();
                            if let Some(members) = merged_members.next() {
                                guild_members.extend(members);
                            }

                            let mut members = guild_members
                                .into_iter()
                                .map(|member| (member.user.id, member.user))
                                .collect::<HashMap<_, _>>();

                            for voice_state in guild_payload.voice_states.unwrap_or_default() {
                                let member_user = members.remove(&voice_state.user_id);
                                let voice_context = &context;
                                voice_participant_futures.push(async move {
                                    let _ = voice_context
                                        .apply_voice_state_update(voice_state, member_user)
                                        .await;
                                });
                            }
                        }

                        discord.cache.replace_guilds(cached_guilds);
                        join_all(voice_participant_futures).await;
                    }
                    GatewayEvent::SessionsReplace => {
                        debug!("Session replace");
                        let session = payload.decode::<Vec<SessionObjectPayload>>()?;
                        debug!("{}", session.pretty());
                    }
                    GatewayEvent::GuildCreate => {
                        let guild = payload.decode::<ReadyGuildPayload>()?;
                        let Some(guild_id) = guild.id else {
                            warn!("GUILD_CREATE missing guild id");
                            return Ok(());
                        };

                        if let Some(channels) = guild.channels {
                            discord.cache.replace_guild_channels(guild_id, channels);
                        }
                        if let Some(emojis) = guild.emojis {
                            discord.cache.guild_emojis.insert(
                                guild_id,
                                emojis
                                    .into_iter()
                                    .filter_map(|emoji| {
                                        facet_value::from_value::<api::GuildEmoji>(emoji)
                                            .inspect_err(|err| {
                                                warn!("Skipping unparseable guild emoji: {err}")
                                            })
                                            .ok()
                                    })
                                    .collect(),
                            );
                        }
                        if let Some(stickers) = guild.stickers {
                            discord.cache.guild_stickers.insert(
                                guild_id,
                                stickers
                                    .into_iter()
                                    .filter_map(|sticker| {
                                        facet_value::from_value::<api::GuildSticker>(sticker)
                                            .inspect_err(|err| {
                                                warn!("Skipping unparseable guild sticker: {err}")
                                            })
                                            .ok()
                                    })
                                    .collect(),
                            );
                        }
                        if let Some(presences) = guild.presences {
                            context.emit_presence_values(presences);
                        }
                        if let Some(properties) = guild.properties {
                            discord.cache.upsert_guild(properties);
                        }

                        let mut members = guild
                            .members
                            .unwrap_or_default()
                            .into_iter()
                            .map(|member| (member.user.id, member.user))
                            .collect::<HashMap<_, _>>();
                        let voice_updates = guild.voice_states.unwrap_or_default();
                        join_all(voice_updates.into_iter().map(|voice_state| {
                            let member_user = members.remove(&voice_state.user_id);
                            let context = &context;
                            async move {
                                let _ = context
                                    .apply_voice_state_update(voice_state, member_user)
                                    .await;
                            }
                        }))
                        .await;
                    }
                    GatewayEvent::GuildUpdate => {
                        discord.cache.upsert_guild(payload.decode::<api::Guild>()?);
                    }
                    GatewayEvent::GuildDelete => {
                        let deleted = payload.decode::<GuildDeletePayload>()?;
                        if deleted.unavailable == Some(true) {
                            debug!(guild_id = deleted.id, "Guild temporarily unavailable");
                        } else {
                            discord.cache.remove_guild(deleted.id);
                        }
                    }
                    GatewayEvent::GuildEmojisUpdate => {
                        let update = payload.decode::<GuildEmojisUpdatePayload>()?;
                        discord
                            .cache
                            .guild_emojis
                            .insert(update.guild_id, update.emojis);
                    }
                    GatewayEvent::GuildStickersUpdate => {
                        let update = payload.decode::<GuildStickersUpdatePayload>()?;
                        discord
                            .cache
                            .guild_stickers
                            .insert(update.guild_id, update.stickers);
                    }
                    GatewayEvent::RelationshipAdd => {
                        discord
                            .cache
                            .upsert_relationship(payload.decode::<api::Friend>()?);
                    }
                    GatewayEvent::RelationshipUpdate => {
                        let relationship = payload.decode::<RelationshipPayload>()?;
                        if let Some(user) = relationship.user {
                            discord.cache.upsert_relationship(api::Friend {
                                id: relationship.id,
                                user,
                            });
                        }
                    }
                    GatewayEvent::RelationshipRemove => {
                        let relationship = payload.decode::<RelationshipPayload>()?;
                        discord.cache.remove_relationship(relationship.id);
                    }
                    GatewayEvent::VoiceStateUpdate => {
                        let mut voice_state = payload.decode::<VoiceStatePayload>()?;

                        let current_user_id = discord
                            .cache
                            .profile
                            .load()
                            .as_ref()
                            .map(|profile| profile.id);

                        let event_user_id = voice_state.user_id;
                        let is_own_state = current_user_id == Some(event_user_id);
                        if is_own_state {
                            gateway
                                .voice
                                .insert_session_id(std::mem::take(&mut voice_state.session_id))
                                .await;
                        }

                        let transition = context.apply_voice_state_update(voice_state, None).await;
                        let previous_channel = transition.previous.map(|state| state.channel_id);
                        let source_channel = transition.current.map(|state| state.channel_id);
                        let camera_was_on =
                            transition.previous.is_some_and(|state| state.self_video);
                        let screen_was_on =
                            transition.previous.is_some_and(|state| state.self_stream);
                        let camera_on = transition.current.is_some_and(|state| state.self_video);
                        let screen_on = transition.current.is_some_and(|state| state.self_stream);
                        let our_channel = if is_own_state {
                            source_channel
                        } else {
                            current_user_id
                                .and_then(|id| discord.cache.voice.state(id))
                                .map(|state| state.channel_id)
                        };

                        if is_own_state {
                            // Joining/moving must surface streams that were
                            // already live before our own voice-state update.
                            if let Some(channel_id) = our_channel {
                                for (user_id, state) in discord
                                    .cache
                                    .voice
                                    .states_in_channel(channel_id)
                                    .filter(|(user_id, _)| *user_id != event_user_id)
                                {
                                    if state.self_stream {
                                        context.advertise_remote_video(
                                            user_id,
                                            channel_id,
                                            VideoBroadcastKind::Screen,
                                        );
                                    }
                                }
                            }

                            // Leaving or moving makes old-call advertisements
                            // stale even when broadcasters remain live there.
                            if previous_channel != our_channel
                                && let Some(old_channel) = previous_channel
                            {
                                for slot in gateway.voice.remove_watched_channel(old_channel).await
                                {
                                    slot.disconnect().await;
                                }
                                for (user_id, state) in discord
                                    .cache
                                    .voice
                                    .states_in_channel(old_channel)
                                    .filter(|(user_id, _)| *user_id != event_user_id)
                                {
                                    if state.self_video {
                                        context.remove_participant_video(
                                            user_id,
                                            old_channel,
                                            VideoBroadcastKind::Camera,
                                        );
                                    }
                                    if state.self_stream {
                                        context.remove_participant_video(
                                            user_id,
                                            old_channel,
                                            VideoBroadcastKind::Screen,
                                        );
                                    }
                                }
                            }
                        } else {
                            let was_in_call = previous_channel
                                .is_some_and(|channel| Some(channel) == our_channel);
                            let is_in_call =
                                source_channel.is_some_and(|channel| Some(channel) == our_channel);

                            for (kind, was_on, is_on) in [
                                (VideoBroadcastKind::Camera, camera_was_on, camera_on),
                                (VideoBroadcastKind::Screen, screen_was_on, screen_on),
                            ] {
                                if was_in_call
                                    && was_on
                                    && (!is_in_call || !is_on)
                                    && let Some(channel_id) = previous_channel
                                {
                                    if kind == VideoBroadcastKind::Screen {
                                        for slot in gateway
                                            .voice
                                            .remove_watched_source(event_user_id, channel_id)
                                            .await
                                        {
                                            slot.disconnect().await;
                                        }
                                    }
                                    context.remove_participant_video(
                                        event_user_id,
                                        channel_id,
                                        kind,
                                    );
                                }
                                // Go Live requires explicit Stream Watch.
                                // Camera media arrives automatically and is
                                // surfaced only when its first frame decodes.
                                if kind == VideoBroadcastKind::Screen
                                    && is_in_call
                                    && is_on
                                    && let Some(channel_id) = source_channel
                                {
                                    context.advertise_remote_video(event_user_id, channel_id, kind);
                                }
                            }
                        }

                        // VOICE_STATE_UPDATE and VOICE_SERVER_UPDATE can
                        // arrive in either order. If the endpoint got here
                        // first, the connect attempt in the VoiceServerUpdate
                        // handler bailed with "not ready" — retry now that
                        // the session id completed the handshake data.
                        if is_own_state && gateway.voice.load_gateway().is_none() {
                            trace!("VoiceStateUpdate: attempting voice gateway connect");
                            match gateway.voice.connect(event_user_id).await {
                                Ok(ConnectProgress::Connected) => {
                                    debug!("Voice connected via VoiceStateUpdate path")
                                }
                                Ok(
                                    ConnectProgress::WaitingForData
                                    | ConnectProgress::AlreadyConnected
                                    | ConnectProgress::Superseded,
                                ) => {}
                                Err(err) => error!("{err:?}"),
                            }
                            trace!("VoiceStateUpdate: voice connect attempt finished");
                        }
                    }
                    GatewayEvent::PresenceUpdate => {
                        let presence = payload.decode::<PresencePayload>()?;
                        context.emit_presence(presence);
                    }
                    GatewayEvent::VoiceServerUpdate => {
                        let server_update = payload.decode::<VoiceServerUpdatePayload>()?;

                        gateway
                            .voice
                            .insert_endpoint(Endpoint::new(
                                server_update.endpoint.ok_or_else(|| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "missing voice server endpoint",
                                    )
                                })?,
                                server_update.token,
                            ))
                            .await;

                        let user_id = discord
                            .cache
                            .profile
                            .load()
                            .as_ref()
                            .ok_or_else(|| {
                                io::Error::new(io::ErrorKind::NotFound, "user profile not loaded")
                            })?
                            .id;

                        trace!("VoiceServerUpdate: attempting voice gateway connect");
                        match gateway.voice.connect(user_id).await {
                            Ok(ConnectProgress::Connected | ConnectProgress::AlreadyConnected) => {}
                            // Session id hasn't arrived yet; the
                            // VoiceStateUpdate handler retries when it does.
                            Ok(ConnectProgress::WaitingForData) => {
                                debug!("Voice handshake incomplete (awaiting session id)")
                            }
                            Ok(ConnectProgress::Superseded) => {}
                            Err(err) => {
                                error!("{err:?}");
                            }
                        };
                        trace!("VoiceServerUpdate: voice connect attempt finished");
                    }
                    GatewayEvent::MessageCreate => {
                        let message = payload.decode::<Message>()?;

                        gateway
                            .recording
                            .maybe_record(|| RecordedEvent::MessageCreated {
                                channel_id: message.channel_id,
                                message: message.clone(),
                            });
                        discord
                            .cache
                            .note_message_created(message.channel_id, message.id);

                        let channel_id_hash = message.channel_id;
                        let msg_id_hash = message.id;

                        trace!("{}", message.pretty());
                        let (content, history) = message.revisions().await;
                        let icon = match &message.author.avatar {
                            Some(hash) => {
                                CdnImage::avatar(message.author.id, hash).fetch().await.ok()
                            }
                            None => None,
                        };
                        let author = Identifier::new(
                            message.author.id,
                            GlobalUser {
                                name: message
                                    .author
                                    .global_name
                                    .clone()
                                    .unwrap_or_else(|| message.author.username.clone()),
                                tag: Some(message.author.username),
                                icon,
                                status: None,
                                voice: None,
                            },
                        );
                        let msg_identifier = Identifier::new(
                            msg_id_hash,
                            GlobalMessage {
                                content,
                                history,
                                reactions: Vec::new(),
                                author: Some(author),
                            },
                        );

                        gateway.events.submit_text(TextEvent::MessageCreated {
                            room: Identifier::new(channel_id_hash, ()),
                            message: msg_identifier,
                        });
                    }
                    GatewayEvent::MessageUpdate => {
                        // MESSAGE_UPDATE payloads can be partial: embed
                        // unfurls (someone posts a link) arrive without
                        // author/content/timestamp. We track none of the
                        // fields such updates carry, so skip them instead
                        // of failing the whole dispatch.
                        let message = match payload.decode::<Message>() {
                            Ok(message) => message,
                            Err(err) => {
                                debug!("Skipping partial MESSAGE_UPDATE: {err}");
                                return Ok(());
                            }
                        };

                        trace!("{}", message.pretty());

                        // Record before emitting: if a concurrent
                        // `rest_get_messages` is in flight, its merge step
                        // must observe this update — emitting first would
                        // race the UI consuming the event before the
                        // recording window captures it. See
                        // `crate/messenger_interface/docs/races.md`.
                        gateway
                            .recording
                            .maybe_record(|| RecordedEvent::MessageUpdated {
                                channel_id: message.channel_id,
                                message: message.clone(),
                            });

                        let (content, history) = message.revisions().await;
                        // Edit payloads often omit `reactions`; map what we
                        // got so reactions survive when they are included.
                        let reactions = message.interface_reactions().await;
                        let icon = match &message.author.avatar {
                            Some(hash) => {
                                CdnImage::avatar(message.author.id, hash).fetch().await.ok()
                            }
                            None => None,
                        };
                        let author = Identifier::new(
                            message.author.id,
                            GlobalUser {
                                name: message
                                    .author
                                    .global_name
                                    .clone()
                                    .unwrap_or_else(|| message.author.username.clone()),
                                tag: Some(message.author.username),
                                icon,
                                status: None,
                                voice: None,
                            },
                        );
                        let msg_identifier = Identifier::new(
                            message.id,
                            GlobalMessage {
                                content,
                                history,
                                reactions,
                                author: Some(author),
                            },
                        );

                        gateway.events.submit_text(TextEvent::MessageUpdated {
                            room: Identifier::new(message.channel_id, ()),
                            message: msg_identifier,
                        });
                    }
                    GatewayEvent::MessageDelete => {
                        let payload = payload.decode::<MessageDelete>()?;

                        trace!("{}", payload.pretty());

                        // Record BEFORE emitting: a concurrent
                        // rest_get_messages whose window is open must
                        // observe this delete so the UI doesn't see a
                        // resurrected message after consuming the
                        // TextEvent::MessageDeleted. See
                        // `crate/messenger_interface/docs/races.md`.
                        gateway
                            .recording
                            .maybe_record(|| RecordedEvent::MessageDeleted {
                                channel_id: payload.channel_id,
                                message_id: payload.id,
                            });

                        gateway.events.submit_text(TextEvent::MessageDeleted {
                            room: Identifier::new(payload.channel_id, ()),
                            message_id: payload.id,
                        });
                    }
                    GatewayEvent::MessageDeleteBulk => {
                        let deleted = payload.decode::<MessageDeleteBulk>()?;
                        for &message_id in &deleted.ids {
                            gateway
                                .recording
                                .maybe_record(|| RecordedEvent::MessageDeleted {
                                    channel_id: deleted.channel_id,
                                    message_id,
                                });
                        }
                        for message_id in deleted.ids {
                            gateway.events.submit_text(TextEvent::MessageDeleted {
                                room: Identifier::new(deleted.channel_id, ()),
                                message_id,
                            });
                        }
                    }
                    GatewayEvent::MessageReactionAdd => {
                        let payload = payload.decode::<MessageReactionChange>()?;

                        trace!("{}", payload.pretty());

                        let is_self = discord
                            .cache
                            .profile
                            .load()
                            .as_ref()
                            .map(|p| p.id == payload.user_id)
                            .unwrap_or(false);
                        let channel_id = payload.channel_id;
                        let message_id = payload.message_id;
                        let user_id = payload.user_id;
                        let emoji_name = payload.emoji.name.clone();

                        gateway
                            .recording
                            .maybe_record(|| RecordedEvent::ReactionAdded {
                                channel_id,
                                message_id,
                                emoji: payload.emoji,
                                is_self,
                            });

                        gateway.events.submit_text(TextEvent::ReactionAdded {
                            room: Identifier::new(channel_id, ()),
                            message_id,
                            user_id,
                            emoji: emoji_name,
                        });
                    }
                    GatewayEvent::MessageReactionRemove => {
                        let payload = payload.decode::<MessageReactionChange>()?;

                        trace!("{}", payload.pretty());

                        let is_self = discord
                            .cache
                            .profile
                            .load()
                            .as_ref()
                            .map(|p| p.id == payload.user_id)
                            .unwrap_or(false);
                        let channel_id = payload.channel_id;
                        let message_id = payload.message_id;
                        let user_id = payload.user_id;
                        let emoji_name = payload.emoji.name.clone();

                        gateway
                            .recording
                            .maybe_record(|| RecordedEvent::ReactionRemoved {
                                channel_id,
                                message_id,
                                emoji: payload.emoji,
                                is_self,
                            });

                        gateway.events.submit_text(TextEvent::ReactionRemoved {
                            room: Identifier::new(channel_id, ()),
                            message_id,
                            user_id,
                            emoji: emoji_name,
                        });
                    }
                    GatewayEvent::ChannelCreate => {
                        let channel = payload.decode::<api::Channel>()?;

                        let place_room = channel.to_room_data().await;
                        let room = Discord::identifier_generator(channel.id, place_room);
                        let guild_id = channel.guild_id;

                        discord.cache.upsert_channel(channel);

                        gateway.events.submit_query(QueryEvent::ChannelCreated {
                            r#where: guild_id
                                .map(|guild_id| Discord::identifier_generator(guild_id, ())),
                            room,
                        });
                    }
                    GatewayEvent::ChannelUpdate => {
                        discord
                            .cache
                            .upsert_channel(payload.decode::<api::Channel>()?);
                    }
                    GatewayEvent::ChannelDelete => {
                        let deleted = payload.decode::<ChannelDeletePayload>()?;
                        discord.cache.remove_channel(deleted.id, deleted.guild_id);
                        gateway.events.submit_query(QueryEvent::ChannelDeleted {
                            r#where: deleted
                                .guild_id
                                .map(|guild_id| Discord::identifier_generator(guild_id, ())),
                            room_id: deleted.id,
                        });
                    }
                    GatewayEvent::CallCreate
                    | GatewayEvent::CallUpdate
                    | GatewayEvent::CallDelete => {
                        // Private (DM) call lifecycle events; we don't track
                        // private call state, so just acknowledge them.
                        trace!("Call lifecycle event: {}", event_name.pretty());
                    }
                    GatewayEvent::StreamCreate | GatewayEvent::StreamUpdate => {
                        // Answers our op-20 Stream Watch: carries the RTC
                        // server hosting the stream's media. Paired with
                        // STREAM_SERVER_UPDATE (endpoint + token); either can
                        // arrive first, so both retry the connect.
                        let payload = payload.decode::<StreamCreatePayload>()?;
                        trace!("{}", payload.pretty());
                        if let Some(rtc_server_id) = payload.rtc_server_id {
                            let stream_key = StreamKey::from(payload.stream_key);
                            if gateway.voice.broadcast.watching().await.as_ref()
                                == Some(&stream_key)
                            {
                                let slot = &gateway.voice.broadcast;
                                slot.set_rtc_server_id(&stream_key, rtc_server_id).await;
                                context
                                    .try_open_stream(
                                        &gateway.voice.broadcast,
                                        StreamRole::Broadcast,
                                    )
                                    .await;
                            }
                            if let Some(slot) = gateway.voice.watched_stream(&stream_key) {
                                slot.set_rtc_server_id(&stream_key, rtc_server_id).await;
                                context.try_open_stream(&slot, StreamRole::Watched).await;
                            }
                        }
                    }
                    GatewayEvent::StreamServerUpdate => {
                        let payload = payload.decode::<StreamServerUpdatePayload>()?;
                        trace!("{}", payload.pretty());
                        let stream_key = StreamKey::from(payload.stream_key);
                        let Some(endpoint) = payload.endpoint else {
                            warn!("STREAM_SERVER_UPDATE without an endpoint");
                            return Ok(());
                        };
                        if gateway.voice.broadcast.watching().await.as_ref() == Some(&stream_key) {
                            let slot = &gateway.voice.broadcast;
                            slot.set_server(
                                &stream_key,
                                Endpoint::new(endpoint.clone(), payload.token.clone()),
                            )
                            .await;
                            context
                                .try_open_stream(&gateway.voice.broadcast, StreamRole::Broadcast)
                                .await;
                        }
                        if let Some(slot) = gateway.voice.watched_stream(&stream_key) {
                            slot.set_server(&stream_key, Endpoint::new(endpoint, payload.token))
                                .await;
                            context.try_open_stream(&slot, StreamRole::Watched).await;
                        }
                    }
                    GatewayEvent::StreamDelete => {
                        let payload = payload.decode::<StreamDeletePayload>()?;
                        let stream_key = StreamKey::from(payload.stream_key);
                        debug!(
                            "Go Live stream ended: key={} reason={:?}",
                            stream_key, payload.reason
                        );
                        // Route by key: our own broadcast slot resets its send
                        // state on disconnect; otherwise it is watched stream.
                        if gateway.voice.broadcast.watching().await.as_ref() == Some(&stream_key) {
                            gateway.voice.broadcast.disconnect().await;
                        } else if let Some(slot) = gateway.voice.remove_watched_stream(&stream_key)
                        {
                            let source = slot.watched_source().await;
                            slot.disconnect().await;
                            if let Some((user_id, channel_id)) = source {
                                context.remove_participant_video(
                                    user_id,
                                    channel_id,
                                    VideoBroadcastKind::Screen,
                                );
                            }
                        } else {
                            trace!("Ignoring STREAM_DELETE for untracked key {}", stream_key);
                        }
                    }
                    _ => warn!("Unknown event_name received: {}", event_name.pretty()),
                }
            }
            Opcode::Reconnect | Opcode::InvalidSession => {
                // The server wants this connection gone (and we don't
                // implement RESUME). Tear the gateway down: the event
                // streams end, and a later `listen()` establishes a
                // fresh connection with a fresh `Ready` snapshot.
                warn!(
                    "Server requested reconnect / invalidated the session; tearing down the gateway"
                );
                discord.end_gateway_session(gateway).await;
            }
            Opcode::HeartbeatAck => {
                trace!("HeartbeatAck");
            }
            _ => {
                warn!("Unknown opcode received: {}", payload.opcode().pretty())
            }
        };
        Ok(())
    }
}
