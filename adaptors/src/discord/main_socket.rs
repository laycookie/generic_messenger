use std::time::Duration;

use async_tungstenite::{
    WebSocketStream,
    async_std::{ConnectStream, connect_async},
    tungstenite::Message,
};
use serde_json::json;
use serde_repr::Deserialize_repr;

use crate::{
    SocketEvent,
    discord::{
        Discord, DiscordSockets, GatewayPayload,
        vc_socket::VCOpcode,
        websocket::{AllData, HeartBeatingData, VCLoc, VCLocation},
    },
    types::{Identifier, Msg, Usr},
};

/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-opcodes>
#[repr(u8)]
#[derive(Debug, Deserialize_repr)]
pub(crate) enum Opcode {
    Dispatch = 0,
    Heartbeat = 1,
    Identify = 2,
    PresenceUpdate = 3,
    VoiceStateUpdate = 4,
    Hello = 10,
    HeartbeatAck = 11,
}
impl GatewayPayload<Opcode> {
    pub(super) async fn exec(
        self,
        discord: &Discord,
    ) -> Result<SocketEvent, Box<dyn std::error::Error>> {
        let mut socket = discord.socket.lock().await;

        if let Some(s) = self.s {
            println!("Updating seq: {s}");
            socket.last_sequence_number = Some(s);
        }

        match self.op {
            Opcode::Hello => {
                socket.heart_beating = Some(HeartBeatingData::new(
                    self.d
                        .get("heartbeat_interval")
                        .and_then(|v| v.as_u64())
                        .map(Duration::from_millis)
                        .unwrap(),
                    None,
                ));

                socket
                    .gateway_websocket
                    .as_mut()
                    .unwrap()
                    .send(Message::Text(
                        json!({
                            "op": Opcode::Identify as u8,
                            "d": {
                                "token": discord.token,
                                "intents": discord.intents,
                                "properties": {
                                    "$os": "Linux",
                                    "$browser": "Firefox",
                                    "$device": ""
                                }
                            }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .expect("Failed to send identify payload");
            }
            Opcode::Dispatch => {
                let event_name = self.t.as_ref().unwrap();
                println!("Dispatch event: {event_name:?}");
                // https://discord.com/developers/docs/events/gateway-events#receive-events
                match event_name.as_str() {
                    "READY" => {
                        println!("importing data");
                    }
                    // TODO: Duplicate of "VOICE_STATE_UPDATE"
                    "SESSIONS_REPLACE" => {
                        println!("{:#?}", self.d);
                    }
                    "VOICE_STATE_UPDATE" => {
                        let session_id = self
                            .d
                            .get("session_id")
                            .and_then(|session_id| session_id.as_str().map(|s| s.to_string()))
                            .unwrap();

                        let DiscordSockets {
                            vc_websocket,
                            vc_location,
                            ..
                        } = &mut *socket;
                        vc_location.insert_session(session_id);

                        if vc_websocket.is_some() {
                            return Ok(SocketEvent::Skip);
                        }

                        if let VCLoc::Ready(vc_location) = vc_location {
                            let profile = discord.profile.read().await;
                            let profile = profile.as_ref();
                            let user_id = profile.unwrap().id.as_str();
                            Discord::connect_vc_gateway(user_id, vc_websocket, vc_location).await;
                        }
                    }
                    "VOICE_SERVER_UPDATE" => {
                        let token = self
                            .d
                            .get("token")
                            .and_then(|token| token.as_str().map(|s| s.to_string()))
                            .unwrap();
                        let endpoint = self
                            .d
                            .get("endpoint")
                            .and_then(|endpoint| endpoint.as_str().map(|s| s.to_string()))
                            .unwrap();

                        socket.vc_location.insert_endpoint(endpoint, token);

                        let DiscordSockets {
                            vc_websocket,
                            vc_location,
                            ..
                        } = &mut *socket;
                        if let VCLoc::Ready(vc_location) = vc_location {
                            let profile = discord.profile.read().await;
                            let profile = profile.as_ref();
                            let user_id = profile.unwrap().id.as_str();

                            Discord::connect_vc_gateway(user_id, vc_websocket, vc_location).await;
                        }
                    }
                    "MESSAGE_CREATE" => {
                        let channel_id = self
                            .d
                            .get("channel_id")
                            .and_then(|id| id.as_str().map(|s| s.to_string()))
                            .unwrap();

                        let text = self
                            .d
                            .get("content")
                            .and_then(|id| id.as_str().map(|s| s.to_string()))
                            .unwrap();

                        let author = self.d.get("author").unwrap();
                        let author_id = author
                            .get("id")
                            .and_then(|id| id.as_str().map(|s| s.to_string()))
                            .unwrap();
                        let author_name = author
                            .get("username")
                            .and_then(|username| username.as_str().map(|s| s.to_string()))
                            .unwrap();

                        let channel_id_hash =
                            Discord::discord_id_to_internal_id(channel_id.as_str());
                        let msg_id_hash = Discord::discord_id_to_internal_id(channel_id.as_str());
                        let author_id_hash = Discord::discord_id_to_internal_id(author_id.as_str());

                        return Ok(SocketEvent::MessageCreated {
                            channel: Identifier {
                                neo_id: channel_id_hash,
                                data: (),
                            },
                            msg: Identifier {
                                neo_id: msg_id_hash,
                                data: Msg {
                                    author: Identifier {
                                        neo_id: author_id_hash,
                                        data: Usr {
                                            name: author_name,
                                            icon: None, // TODO:
                                        },
                                    },
                                    text,
                                },
                            },
                        });
                    }
                    "CALL_CREATE" => {
                        println!("{:#?}", self);
                    }
                    "CALL_UPDATE" => {
                        println!("{:#?}", self);
                    }
                    _ => eprintln!("Unknown event_name received: {event_name:?}",),
                }
            }
            Opcode::HeartbeatAck => {
                println!("HeartbeatAck");
            }
            Opcode::Heartbeat => todo!(),
            Opcode::Identify => todo!(),
            Opcode::PresenceUpdate => todo!(),
            Opcode::VoiceStateUpdate => todo!(),
        };
        Ok(SocketEvent::Skip)
    }
}

impl Discord {
    async fn connect_vc_gateway(
        user_id: &str,
        vc_gateway: &mut Option<WebSocketStream<ConnectStream>>,
        vc_location: &VCLocation<AllData>,
    ) {
        let (mut stream, _) = connect_async("wss://".to_string() + vc_location.get_endpoint())
            .await
            .unwrap();
        // <https://discord.com/developers/docs/topics/voice-connections#establishing-a-voice-websocket-connection>
        // TODO: I believe this payload should change with gateway v9
        let identify_payload = json!({
          "op": VCOpcode::Identify as u8,
          "d": {
            // The ID of the guild, private channel, stream, or lobby being connected to
            "server_id": vc_location.get_location_id(), // TODO
            "user_id": user_id,
            "session_id": vc_location.get_session(),
            "token": vc_location.get_token(),
          }
        });

        println!("{:#?}", identify_payload);
        stream
            .send(identify_payload.to_string().into())
            .await
            .unwrap();

        *vc_gateway = Some(stream);
    }
}
