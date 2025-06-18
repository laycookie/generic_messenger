use async_trait::async_trait;
use async_tungstenite::WebSocketStream;
use async_tungstenite::async_std::ConnectStream;
use async_tungstenite::tungstenite::Message;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use serde_repr::Deserialize_repr;
use std::{sync::Arc, time::Duration};

use crate::{
    Socket, SocketUpdate,
    types::{Identifier, Msg, Usr},
};

use super::Discord;

/// Implementation of:
/// https://discord.com/developers/docs/events/gateway

// https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-opcodes
#[repr(u8)]
#[derive(Debug, Deserialize_repr)]
enum Opcode {
    Dispatch = 0,
    Heartbeat = 1,
    Identify = 2,
    Hello = 10,
}

pub(super) struct DiscordSocket {
    pub websocket: Option<WebSocketStream<ConnectStream>>,
    pub heart_beat_interval: Option<Duration>,
}
impl DiscordSocket {
    pub fn new() -> Self {
        DiscordSocket {
            websocket: None.into(),
            heart_beat_interval: None,
        }
    }
}
// https://discord.com/developers/docs/events/gateway-events#payload-structure
#[derive(Debug, Deserialize)]
struct GateawayPayload {
    op: Opcode,
    // Event type
    t: Option<String>,
    // Sequence numbers
    s: Option<u32>,
    // data
    d: serde_json::Value,
}

#[async_trait]
impl Socket for Discord {
    async fn next(&self) -> Option<SocketUpdate> {
        let mut discord_stream = self.socket.lock().await;

        let json = match discord_stream.websocket.as_mut()?.next().await? {
            Ok(Message::Text(text)) => serde_json::from_str::<GateawayPayload>(&text).unwrap(),
            Ok(Message::Close(frame)) => {
                println!("Disconnected: {:?}", frame);
                return None;
            }
            Ok(_) => todo!(),
            Err(e) => {
                eprintln!("Error: {}", e);
                return None;
            }
        };
        // println!("Received: {:#?}", json);
        println!("Received: {:#?}", json.op);

        match json.op {
            Opcode::Hello => {
                discord_stream.heart_beat_interval = json
                    .d
                    .get("heartbeat_interval")
                    .and_then(|v| v.as_u64())
                    .map(Duration::from_millis);

                // Send Identify payload
                let identify_payload = json!({
                    "op": 2,
                    "d": {
                        "token": self.token,
                        "intents": self.intents,
                        "properties": {
                            "$os": "Linux",
                            "$browser": "Firefox",
                            "$device": ""
                        }
                    }
                });
                discord_stream
                    .websocket
                    .as_mut()?
                    .send(Message::Text(identify_payload.to_string().into()))
                    .await
                    .expect("Failed to send identify payload");
            }

            Opcode::Dispatch => {
                let event_name = json.t.unwrap();
                println!("{:?}", event_name);
                match event_name.as_str() {
                    "READY" => {
                        println!("importing data")
                    }
                    "SESSIONS_REPLACE" => {
                        println!("something something")
                    }
                    "MESSAGE_CREATE" => {
                        let channel_id = json
                            .d
                            .get("channel_id")
                            .and_then(|id| Some(id.to_string()))
                            .unwrap();
                        let msg_id = json
                            .d
                            .get("id")
                            .and_then(|id| Some(id.to_string()))
                            .unwrap();

                        let text = json
                            .d
                            .get("content")
                            .and_then(|id| Some(id.to_string()))
                            .unwrap();

                        let author = json.d.get("author").unwrap();
                        let author_id = author
                            .get("id")
                            .and_then(|id| Some(id.to_string()))
                            .unwrap();
                        let author_name = author
                            .get("username")
                            .and_then(|username| Some(username.to_string()))
                            .unwrap();

                        return Some(SocketUpdate::MessageCreated {
                            channel: Identifier {
                                id: channel_id.to_owned(),
                                hash: None,
                                data: (),
                            },
                            msg: Identifier {
                                id: msg_id,
                                hash: None,
                                data: Msg {
                                    author: Identifier {
                                        id: author_id,
                                        hash: None,
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
                    _ => eprintln!("Unkown event_name recived: {:?}", event_name),
                }
            }
            _ => {}
        };

        Some(SocketUpdate::Skip)
    }
}
