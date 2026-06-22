use std::{
    collections::HashMap,
    io,
    sync::{Arc, atomic::Ordering},
};

use facet_pretty::FacetPretty;
use futures::future::join_all;
use messenger_interface::{
    interface::{QueryEvent, TextEvent},
    types::{Identifier, Message as GlobalMessage, User as GlobalUser},
};
use tracing::{debug, error, trace, warn};

use super::{
    GatewayEvent, Opcode,
    payloads::{ReadyPayload, SessionObjectPayload, VoiceServerUpdatePayload, VoiceStatePayload},
    recording::RecordedEvent,
};
use crate::{
    ChannelLocation, Discord, InnerDiscord, UnitStruct,
    api_types::{self, Message},
    downloaders::CdnImage,
    gateways::{GatewayPayload, voice::Endpoint},
};

impl GatewayPayload<Opcode> {
    pub(in crate::gateways) async fn exec<T: UnitStruct>(
        self,
        discord: &InnerDiscord<T>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let gateway = discord.gateway.load();
        let Some(gateway) = gateway.as_ref() else {
            return Err(
                io::Error::new(io::ErrorKind::NotConnected, "gateway not connected").into(),
            );
        };

        if let Some(s) = self.s {
            gateway
                .last_sequence_number
                .get_or_init(|| s.into())
                .store(s, Ordering::Relaxed);
        };

        match self.op {
            Opcode::Hello => {}
            Opcode::Dispatch => {
                let Some(event_name) = self.t.as_ref() else {
                    warn!("Dispatch opcode received without an event type (t)");
                    return Ok(());
                };
                debug!("Dispatch event: {}", event_name.pretty());
                // https://discord.com/developers/docs/events/gateway-events#receive-events
                match event_name {
                    GatewayEvent::Ready => {
                        // Reconnects reuse `InnerDiscord`, so wipe per-channel
                        // voice rosters before rebuilding from this Ready's
                        // voice_states — otherwise stale entries linger.
                        discord.voice_states.clear();
                        discord.voice_participants.clear();

                        let ready = facet_value::from_value::<ReadyPayload>(self.d)?;

                        if let Some(user) = ready.user {
                            discord.profile.store(Some(Arc::new(api_types::Profile {
                                id: user.id,
                                username: user.username,
                                avatar: user.avatar,
                            })));
                        }

                        if let Some(private_channels) = ready.private_channels {
                            discord.dm_channels.store(Some(Arc::new(private_channels)));
                        }

                        if let Some(relationships) = ready.relationships {
                            let mut cached_relationships = Vec::new();
                            for relationship in relationships {
                                match facet_value::from_value::<api_types::Friend>(relationship) {
                                    Ok(relationship) => cached_relationships.push(relationship),
                                    Err(err) => warn!("Skipping unparseable relationship: {err}"),
                                }
                            }
                            discord
                                .relationships
                                .store(Some(Arc::new(cached_relationships)));
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
                        // gateway dropped. `voice_states` was just cleared and a
                        // user appears in at most one voice state, so these are
                        // independent (no cross-eviction) and order-free.
                        let mut voice_participant_futures = Vec::new();

                        for guild_payload in ready.guilds.unwrap_or_default() {
                            if let (Some(guild_id), Some(channels)) =
                                (guild_payload.id, guild_payload.channels)
                            {
                                discord.guild_channels.insert(guild_id, channels);
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
                                voice_participant_futures.push(
                                    discord.emit_voice_state_participant(
                                        voice_state.user_id,
                                        voice_state,
                                        member_user,
                                    ),
                                );
                            }
                        }

                        join_all(voice_participant_futures).await;

                        if !cached_guilds.is_empty() {
                            discord.guilds.store(Some(Arc::new(cached_guilds)));
                        }
                    }
                    GatewayEvent::SessionsReplace => {
                        debug!("Session replace");
                        let session = facet_value::from_value::<Vec<SessionObjectPayload>>(self.d)?;
                        debug!("{}", session.pretty());
                    }
                    GatewayEvent::VoiceStateUpdate => {
                        let voice_state = facet_value::from_value::<VoiceStatePayload>(self.d)?;

                        let current_user_id =
                            discord.profile.load().as_ref().map(|profile| profile.id);

                        let event_user_id = voice_state.user_id;
                        let is_own_state = current_user_id == Some(event_user_id);
                        if is_own_state {
                            gateway
                                .voice
                                .insert_session_id(voice_state.session_id.clone())
                                .await;
                        }

                        discord
                            .emit_voice_state_participant(voice_state.user_id, voice_state, None)
                            .await;

                        // VOICE_STATE_UPDATE and VOICE_SERVER_UPDATE can
                        // arrive in either order. If the endpoint got here
                        // first, the connect attempt in the VoiceServerUpdate
                        // handler bailed with "not ready" — retry now that
                        // the session id completed the handshake data.
                        if is_own_state && gateway.voice.full_load_gateway().is_none() {
                            trace!("VoiceStateUpdate: attempting voice gateway connect");
                            match gateway.voice.connect(event_user_id).await {
                                Ok(true) => debug!("Voice connected via VoiceStateUpdate path"),
                                Ok(false) => (), // endpoint not received yet — the normal order
                                Err(err) => error!("{err:?}"),
                            }
                            trace!("VoiceStateUpdate: voice connect attempt finished");
                        }
                    }
                    GatewayEvent::VoiceServerUpdate => {
                        let server_update =
                            facet_value::from_value::<VoiceServerUpdatePayload>(self.d)?;

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
                            .profile
                            .load()
                            .as_ref()
                            .ok_or_else(|| {
                                io::Error::new(io::ErrorKind::NotFound, "user profile not loaded")
                            })?
                            .id;

                        trace!("VoiceServerUpdate: attempting voice gateway connect");
                        match gateway.voice.connect(user_id).await {
                            Ok(true) => (),
                            // Session id hasn't arrived yet; the
                            // VoiceStateUpdate handler retries when it does.
                            Ok(false) => {
                                debug!("Voice handshake incomplete (awaiting session id)")
                            }
                            Err(err) => {
                                error!("{err:?}");
                            }
                        };
                        trace!("VoiceServerUpdate: voice connect attempt finished");
                    }
                    GatewayEvent::MessageCreate => {
                        let message = facet_value::from_value::<Message>(self.d)?;

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
                                name: message.author.username,
                                icon,
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

                        discord
                            .message_id_mappings
                            .insert(*msg_identifier.id(), msg_id_hash);

                        discord.text_events.force_push(TextEvent::MessageCreated {
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
                        let message = match facet_value::from_value::<Message>(self.d) {
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
                        gateway.maybe_record(|| RecordedEvent::MessageUpdated {
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
                                name: message.author.username,
                                icon,
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

                        discord.text_events.force_push(TextEvent::MessageUpdated {
                            room: Identifier::new(message.channel_id, ()),
                            message: msg_identifier,
                        });
                    }
                    GatewayEvent::MessageDelete => {
                        let payload = facet_value::from_value::<api_types::MessageDelete>(self.d)?;

                        trace!("{}", payload.pretty());

                        // Record BEFORE emitting: a concurrent
                        // rest_get_messages whose window is open must
                        // observe this delete so the UI doesn't see a
                        // resurrected message after consuming the
                        // TextEvent::MessageDeleted. See
                        // `crate/messenger_interface/docs/races.md`.
                        gateway.maybe_record(|| RecordedEvent::MessageDeleted {
                            channel_id: payload.channel_id,
                            message_id: payload.id,
                        });

                        discord.text_events.force_push(TextEvent::MessageDeleted {
                            room: Identifier::new(payload.channel_id, ()),
                            message_id: payload.id,
                        });
                    }
                    GatewayEvent::MessageDeleteBulk => {
                        // TODO: implement once messenger_interface unifies
                        // single + bulk delete events (TextEvent::MessageDeleted
                        // extended to carry multiple IDs, with the singular
                        // case becoming "bulk of one"). At that point this
                        // handler should:
                        //   1. Iterate the bulk payload's `ids` field and
                        //      call `gateway.maybe_record(|| RecordedEvent::
                        //      MessageDeleted { ... })` for each, BEFORE
                        //      emitting any TextEvent, to preserve the
                        //      record-before-emit invariant documented in
                        //      `crate/messenger_interface/docs/races.md`.
                        //   2. Emit one unified TextEvent carrying all IDs.
                        warn!("MessageDeleteBulk received but not yet handled");
                    }
                    GatewayEvent::MessageReactionAdd => {
                        let payload =
                            facet_value::from_value::<api_types::MessageReactionChange>(self.d)?;

                        trace!("{}", payload.pretty());

                        let is_self = discord
                            .profile
                            .load()
                            .as_ref()
                            .map(|p| p.id == payload.user_id)
                            .unwrap_or(false);
                        let channel_id = payload.channel_id;
                        let message_id = payload.message_id;
                        let user_id = payload.user_id;
                        let emoji_name = payload.emoji.name.clone();

                        gateway.maybe_record(|| RecordedEvent::ReactionAdded {
                            channel_id,
                            message_id,
                            emoji: payload.emoji,
                            is_self,
                        });

                        discord.text_events.force_push(TextEvent::ReactionAdded {
                            room: Identifier::new(channel_id, ()),
                            message_id,
                            user_id,
                            emoji: emoji_name,
                        });
                    }
                    GatewayEvent::MessageReactionRemove => {
                        let payload =
                            facet_value::from_value::<api_types::MessageReactionChange>(self.d)?;

                        trace!("{}", payload.pretty());

                        let is_self = discord
                            .profile
                            .load()
                            .as_ref()
                            .map(|p| p.id == payload.user_id)
                            .unwrap_or(false);
                        let channel_id = payload.channel_id;
                        let message_id = payload.message_id;
                        let user_id = payload.user_id;
                        let emoji_name = payload.emoji.name.clone();

                        gateway.maybe_record(|| RecordedEvent::ReactionRemoved {
                            channel_id,
                            message_id,
                            emoji: payload.emoji,
                            is_self,
                        });

                        discord.text_events.force_push(TextEvent::ReactionRemoved {
                            room: Identifier::new(channel_id, ()),
                            message_id,
                            user_id,
                            emoji: emoji_name,
                        });
                    }
                    GatewayEvent::ChannelCreate => {
                        let channel = facet_value::from_value::<api_types::Channel>(self.d)?;

                        let place_room = channel.to_room_data().await;
                        let room = Discord::identifier_generator(channel.id, place_room);
                        let guild_id = channel.guild_id;

                        // Top-level ChannelCreate carries guild_id directly,
                        // so no parent context is needed.
                        if let Some(location) = ChannelLocation::from_api(&channel, None) {
                            discord.channel_id_mappings.insert(*room.id(), location);
                        } else {
                            warn!(
                                "ChannelCreate for {} produced no ChannelLocation",
                                channel.id
                            );
                        }

                        // Append to the per-guild channel cache so a later
                        // `house_details` fetch for this guild sees the new
                        // channel — without this, the cache reflects only
                        // the Ready snapshot and freshly-created channels
                        // vanish until the gateway reconnects. DM/group-DM
                        // creates (no guild_id) aren't cached here yet.
                        if let Some(guild_id) = guild_id {
                            discord
                                .guild_channels
                                .entry(guild_id)
                                .or_default()
                                .push(channel);
                        }

                        discord.query_events.force_push(QueryEvent::ChannelCreated {
                            r#where: guild_id
                                .map(|guild_id| Discord::identifier_generator(guild_id, ())),
                            room,
                        });
                    }
                    GatewayEvent::CallCreate
                    | GatewayEvent::CallUpdate
                    | GatewayEvent::CallDelete => {
                        // Private (DM) call lifecycle events; we don't track
                        // private call state, so just acknowledge them.
                        trace!("Call lifecycle event: {}", event_name.pretty());
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
                discord.gateway.store(None);
            }
            Opcode::HeartbeatAck => {
                trace!("HeartbeatAck");
            }
            _ => {
                warn!("Unknown opcode received: {}", self.op.pretty())
            }
        };
        Ok(())
    }
}
