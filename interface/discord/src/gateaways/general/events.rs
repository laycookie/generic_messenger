use std::sync::atomic::Ordering;

use facet_pretty::FacetPretty;
use messenger_interface::{
    interface::{QueryEvent, TextEvent},
    types::{Identifier, Message as GlobalMessage, User as GlobalUser},
};
use tracing::{error, info, warn};

use super::{
    GatewayEvent, Opcode,
    payloads::{SessionObjectPayload, VoiceServerUpdatePayload, VoiceStatePayload},
};
use crate::{
    Discord, InnerDiscord, UnitStruct,
    api_types::{self, Message},
    gateaways::{GatewayPayload, voice::Endpoint},
};

impl GatewayPayload<Opcode> {
    pub(in crate::gateaways) async fn exec<T: UnitStruct>(
        self,
        discord: &InnerDiscord<T>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let gateaway = discord.gateaway.load();
        let Some(gateaway) = gateaway.as_ref() else {
            return Err("TODO".into());
        };

        if let Some(s) = self.s {
            gateaway
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
                info!("Dispatch event: {event_name:?}");
                // https://discord.com/developers/docs/events/gateway-events#receive-events
                match event_name {
                    GatewayEvent::Ready => {
                        info!("importing data");
                    }
                    GatewayEvent::SessionsReplace => {
                        info!("Session replace");
                        let session = facet_value::from_value::<Vec<SessionObjectPayload>>(self.d)?;
                        info!("{}", session.pretty());
                    }
                    GatewayEvent::VoiceStateUpdate => {
                        let voice_state = facet_value::from_value::<VoiceStatePayload>(self.d)?;

                        gateaway
                            .voice
                            .insert_session_id(voice_state.session_id)
                            .await;
                    }
                    GatewayEvent::VoiceServerUpdate => {
                        let server_update =
                            facet_value::from_value::<VoiceServerUpdatePayload>(self.d)?;

                        gateaway
                            .voice
                            .insert_endpoint(Endpoint::new(
                                server_update.endpoint.unwrap(),
                                server_update.token,
                            ))
                            .await;

                        let profile = discord.profile.read().await;
                        let profile = profile.as_ref();
                        let user_id = profile.unwrap().id;

                        match gateaway.voice.connect(user_id).await {
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

                        info!(
                            "MessageCreate: channel={} msg={} text={:?}",
                            channel_id_hash, msg_id_hash, &message.content
                        );
                        let author = Identifier::new(
                            message.author.id,
                            GlobalUser {
                                name: message.author.username,
                                icon: None,
                            },
                        );
                        discord.text_events.push(TextEvent::MessageCreated {
                            room: Identifier::new(channel_id_hash, ()),
                            message: Identifier::new(
                                msg_id_hash,
                                GlobalMessage {
                                    text: message.content,
                                    reactions: Vec::new(),
                                    author: Some(author),
                                },
                            ),
                        });
                        info!(
                            "text_events queue length after push: {}",
                            discord.text_events.len()
                        );
                    }
                    GatewayEvent::ChannelCreate => {
                        let channel = facet_value::from_value::<api_types::Channel>(self.d)?;

                        let place_room = channel.to_room_data().await;
                        discord.query_events.push(QueryEvent::ChannelCreated {
                            r#where: channel
                                .guild_id
                                .map(|guild_id| Discord::identifier_generator(guild_id, ())),
                            room: Discord::identifier_generator(channel.id, place_room),
                        });
                    }
                    _ => warn!("Unknown event_name received: {event_name:?}"),
                }
            }
            Opcode::HeartbeatAck => {
                info!("HeartbeatAck");
            }
            _ => {
                warn!("Unkown opcode recived: {:?}", self.op)
            }
        };
        Ok(())
    }
}
