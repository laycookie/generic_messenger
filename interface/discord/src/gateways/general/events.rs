use std::{collections::HashMap, io, sync::atomic::Ordering};

use facet_pretty::FacetPretty;
use messenger_interface::{
    interface::{QueryEvent, TextEvent, VoiceEvent},
    types::{CacheCategory, Identifier, Message as GlobalMessage, User as GlobalUser},
};
use tracing::{debug, error, trace, warn};

use super::{
    GatewayEvent, Opcode,
    payloads::{ReadyPayload, SessionObjectPayload, VoiceServerUpdatePayload, VoiceStatePayload},
};
use crate::{
    ChannelID, Discord, InnerDiscord, UnitStruct,
    api_types::{self, Message},
    downloaders::cache_cdn_image,
    gateways::{GatewayPayload, voice::Endpoint},
};

async fn emit_voice_state_participant<T: UnitStruct>(
    discord: &InnerDiscord<T>,
    user_id: api_types::SNOWFLAKE,
    voice_state: VoiceStatePayload,
    member_user: Option<api_types::User>,
) {
    match voice_state.channel_id {
        Some(channel_id) => {
            let Some(user) = voice_state.member.map(|member| member.user).or(member_user) else {
                warn!("Voice state for user {user_id} is missing member data");
                return;
            };

            let icon = match &user.avatar {
                Some(hash) => cache_cdn_image("avatars", CacheCategory::Users, user.id, hash)
                    .await
                    .ok(),
                None => None,
            };

            discord.voice_events.push(VoiceEvent::ParticipantJoined {
                room: Identifier::new(channel_id, ()),
                user: Identifier::new(
                    user.id,
                    GlobalUser {
                        name: user.username,
                        icon,
                    },
                ),
            });
        }
        None => {
            discord
                .voice_events
                .push(VoiceEvent::ParticipantLeft { user_id });
        }
    }
}

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
                        let ready = facet_value::from_value::<ReadyPayload>(self.d)?;
                        let mut merged_members =
                            ready.merged_members.unwrap_or_default().into_iter();

                        for guild in ready.guilds.unwrap_or_default() {
                            let mut guild_members = guild.members.unwrap_or_default();
                            if let Some(members) = merged_members.next() {
                                guild_members.extend(members);
                            }

                            let mut members = guild_members
                                .into_iter()
                                .map(|member| (member.user.id, member.user))
                                .collect::<HashMap<_, _>>();

                            for voice_state in guild.voice_states.unwrap_or_default() {
                                let member_user = members.remove(&voice_state.user_id);
                                emit_voice_state_participant(
                                    discord,
                                    voice_state.user_id,
                                    voice_state,
                                    member_user,
                                )
                                .await;
                            }
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

                        if current_user_id == Some(voice_state.user_id) {
                            gateway
                                .voice
                                .insert_session_id(voice_state.session_id.clone())
                                .await;
                        }

                        emit_voice_state_participant(
                            discord,
                            voice_state.user_id,
                            voice_state,
                            None,
                        )
                        .await;
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

                        match gateway.voice.connect(user_id).await {
                            Ok(_) => (),
                            Err(err) => {
                                error!("{err:?}");
                            }
                        };
                    }
                    GatewayEvent::MessageCreate => {
                        let message = facet_value::from_value::<Message>(self.d)?;

                        let channel_id_hash = message.channel_id;
                        let msg_id_hash = message.id;

                        trace!("{}", message.pretty());
                        let icon = match &message.author.avatar {
                            Some(hash) => cache_cdn_image(
                                "avatars",
                                CacheCategory::Users,
                                message.author.id,
                                hash,
                            )
                            .await
                            .ok(),
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
                                text: message.content,
                                reactions: Vec::new(),
                                author: Some(author),
                            },
                        );

                        discord
                            .message_id_mappings
                            .insert(*msg_identifier.id(), msg_id_hash);

                        discord.text_events.push(TextEvent::MessageCreated {
                            room: Identifier::new(channel_id_hash, ()),
                            message: msg_identifier,
                        });
                    }
                    GatewayEvent::MessageUpdate => {
                        let message = facet_value::from_value::<Message>(self.d)?;

                        let channel_id_hash = message.channel_id;
                        let msg_id_hash = message.id;

                        trace!("{}", message.pretty());
                        let icon = match &message.author.avatar {
                            Some(hash) => cache_cdn_image(
                                "avatars",
                                CacheCategory::Users,
                                message.author.id,
                                hash,
                            )
                            .await
                            .ok(),
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
                                text: message.content,
                                reactions: Vec::new(),
                                author: Some(author),
                            },
                        );

                        discord.text_events.push(TextEvent::MessageUpdated {
                            room: Identifier::new(channel_id_hash, ()),
                            message: msg_identifier,
                        });
                    }
                    GatewayEvent::MessageDelete => {
                        let payload = facet_value::from_value::<api_types::MessageDelete>(self.d)?;

                        trace!("{}", payload.pretty());

                        discord.text_events.push(TextEvent::MessageDeleted {
                            room: Identifier::new(payload.channel_id, ()),
                            message_id: payload.id,
                        });
                    }
                    GatewayEvent::MessageReactionAdd => {
                        let payload =
                            facet_value::from_value::<api_types::MessageReactionChange>(self.d)?;

                        trace!("{}", payload.pretty());

                        discord.text_events.push(TextEvent::ReactionAdded {
                            room: Identifier::new(payload.channel_id, ()),
                            message_id: payload.message_id,
                            user_id: payload.user_id,
                            emoji: payload.emoji.name,
                        });
                    }
                    GatewayEvent::MessageReactionRemove => {
                        let payload =
                            facet_value::from_value::<api_types::MessageReactionChange>(self.d)?;

                        trace!("{}", payload.pretty());

                        discord.text_events.push(TextEvent::ReactionRemoved {
                            room: Identifier::new(payload.channel_id, ()),
                            message_id: payload.message_id,
                            user_id: payload.user_id,
                            emoji: payload.emoji.name,
                        });
                    }
                    GatewayEvent::ChannelCreate => {
                        let channel = facet_value::from_value::<api_types::Channel>(self.d)?;

                        let place_room = channel.to_room_data().await;
                        let room = Discord::identifier_generator(channel.id, place_room);

                        discord.channel_id_mappings.insert(
                            *room.id(),
                            ChannelID {
                                guild_id: channel.guild_id,
                                id: channel.id,
                            },
                        );

                        discord.query_events.push(QueryEvent::ChannelCreated {
                            r#where: channel
                                .guild_id
                                .map(|guild_id| Discord::identifier_generator(guild_id, ())),
                            room,
                        });
                    }
                    _ => warn!("Unknown event_name received: {}", event_name.pretty()),
                }
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
