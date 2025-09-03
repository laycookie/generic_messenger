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
        Discord, DiscordSockets,
        vc_socket::VCOpcode,
        websocket::{AllData, GateawayPayload, HeartBeatingData, VCLoc, VCLocation},
    },
    types::{Identifier, Msg, Usr},
};

/// https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-opcodes
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

impl Discord {
    async fn connect_vc_socket(
        user_id: &str,
        websocket: &mut Option<WebSocketStream<ConnectStream>>,
        vc_location: &VCLocation<AllData>,
    ) {
        let (mut stream, _) = connect_async("wss://".to_string() + vc_location.get_endpoint())
            .await
            .unwrap();
        // https://discord.com/developers/docs/topics/voice-connections#establishing-a-voice-websocket-connection
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

        stream
            .send(identify_payload.to_string().into())
            .await
            .unwrap();

        *websocket = Some(stream);
    }

    pub(super) async fn event_exec(
        &self,
        json: GateawayPayload<Opcode>,
        socket: &mut DiscordSockets,
    ) -> Result<SocketEvent, Box<dyn std::error::Error>> {
        println!("Received: {:#?}", json.op);
        match json.op {
            Opcode::Hello => {
                socket.heart_beating = Some(HeartBeatingData::new(
                    json.d
                        .get("heartbeat_interval")
                        .and_then(|v| v.as_u64())
                        .map(Duration::from_millis)
                        .unwrap(),
                ));

                socket
                    .websocket
                    .send(Message::Text(
                        json!({
                            "op": Opcode::Identify as u8,
                            "d": {
                                "token": self.token,
                                "intents": self.intents,
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
                let event_name = json.t.as_ref().unwrap();
                println!("{event_name:?}");
                // https://discord.com/developers/docs/events/gateway-events#receive-events
                match event_name.as_str() {
                    "READY" => {
                        println!("importing data");
                    }
                    "SESSIONS_REPLACE" => {
                        println!("{json:#?}");
                    }
                    "VOICE_STATE_UPDATE" => {
                        let session_id = json
                            .d
                            .get("session_id")
                            .and_then(|session_id| session_id.as_str().map(|s| s.to_string()))
                            .unwrap();

                        socket.vc_location.insert_session(session_id);
                        if let VCLoc::Ready(vc_location) = &socket.vc_location {
                            let profile = self.profile.read().await;
                            let profile = profile.as_ref();
                            let user_id = profile.unwrap().id.as_str();
                            Discord::connect_vc_socket(
                                user_id,
                                &mut socket.vc_websocket,
                                vc_location,
                            )
                            .await;
                        }
                    }
                    "VOICE_SERVER_UPDATE" => {
                        let token = json
                            .d
                            .get("token")
                            .and_then(|token| token.as_str().map(|s| s.to_string()))
                            .unwrap();
                        let endpoint = json
                            .d
                            .get("endpoint")
                            .and_then(|endpoint| endpoint.as_str().map(|s| s.to_string()))
                            .unwrap();

                        socket.vc_location.insert_endpoint(endpoint, token);

                        if let VCLoc::Ready(vc_location) = &socket.vc_location {
                            let profile = self.profile.read().await;
                            let profile = profile.as_ref();
                            let user_id = profile.unwrap().id.as_str();
                            Discord::connect_vc_socket(
                                user_id,
                                &mut socket.vc_websocket,
                                vc_location,
                            )
                            .await;
                        }
                    }
                    "MESSAGE_CREATE" => {
                        let channel_id = json
                            .d
                            .get("channel_id")
                            .and_then(|id| id.as_str().map(|s| s.to_string()))
                            .unwrap();

                        let text = json
                            .d
                            .get("content")
                            .and_then(|id| id.as_str().map(|s| s.to_string()))
                            .unwrap();

                        let author = json.d.get("author").unwrap();
                        let author_id = author
                            .get("id")
                            .and_then(|id| id.as_str().map(|s| s.to_string()))
                            .unwrap();
                        let author_name = author
                            .get("username")
                            .and_then(|username| username.as_str().map(|s| s.to_string()))
                            .unwrap();

                        let channel_id_hash = Discord::discord_id_to_u32_id(channel_id.as_str());
                        let msg_id_hash = Discord::discord_id_to_u32_id(channel_id.as_str());
                        let author_id_hash = Discord::discord_id_to_u32_id(author_id.as_str());

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
                        println!("{:#?}", json);
                    }
                    "CALL_UPDATE" => {
                        println!("{:#?}", json);
                    }
                    _ => eprintln!("Unkown event_name recived: {event_name:?}",),
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
