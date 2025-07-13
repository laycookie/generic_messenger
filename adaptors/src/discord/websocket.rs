use async_trait::async_trait;
use async_tungstenite::WebSocketStream;
use async_tungstenite::async_std::ConnectStream;
use async_tungstenite::tungstenite::Message;
use futures::{FutureExt, Stream, StreamExt, poll};
use futures_timer::Delay;
use serde::Deserialize;
use serde_json::json;
use serde_repr::Deserialize_repr;
use std::sync::Arc;
use std::time::Duration;

use crate::{
    Socket, SocketEvent,
    types::{Identifier, Msg, Usr},
};

use super::Discord;

// Implementation of:
// https://discord.com/developers/docs/events/gateway

// https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-opcodes
#[repr(u8)]
#[derive(Debug, Deserialize_repr)]
enum Opcode {
    Dispatch = 0,
    Heartbeat = 1,
    Identify = 2,
    Hello = 10,
    HeartbeatAck = 11,
}

pub(super) struct DiscordSocket {
    pub websocket: WebSocketStream<ConnectStream>,
    last_sequance_number: Option<usize>,
}
impl DiscordSocket {
    pub fn new(websocket: WebSocketStream<ConnectStream>) -> Self {
        DiscordSocket {
            websocket,
            last_sequance_number: None,
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
    s: Option<usize>,
    // data
    d: serde_json::Value,
}

// TODO: Think hard about this
impl Stream for Discord {
    type Item = SocketEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.next().poll_unpin(cx)
    }
}

impl Discord {
    async fn heart_beat(self: Arc<Self>) -> Option<()> {
        if let Some(interval) = *self.heart_beat_interval.read().await {
            Delay::new(interval).await;

            let mut socket = self.socket.lock().await;
            let discord_stream = socket.as_mut()?;

            discord_stream
                .websocket
                .send(
                    json!({
                            "op": Opcode::Heartbeat as u8,
                            "d": discord_stream.last_sequance_number,

                    })
                    .to_string()
                    .into(),
                )
                .await
                .unwrap();
        }
        Some(())
    }
}

#[async_trait]
impl Socket for Discord {
    async fn next(self: Arc<Self>) -> Option<SocketEvent> {
        let mut heart_beat_future = self.heart_beat_future.lock().await;
        {
            if heart_beat_future.is_none() {
                *heart_beat_future = Some(Box::pin(self.clone().heart_beat()));
            };
            match poll!(heart_beat_future.as_mut().unwrap()) {
                std::task::Poll::Ready(_) => *heart_beat_future = None,
                std::task::Poll::Pending => {}
            };
        }

        let mut socket = self.socket.lock().await;
        let discord_stream = socket.as_mut()?;

        let json = match discord_stream.websocket.next().await? {
            Ok(Message::Text(text)) => serde_json::from_str::<GateawayPayload>(&text).unwrap(),
            Ok(Message::Close(frame)) => {
                println!("Disconnected: {frame:?}");
                *socket = None;
                return None;
            }
            Ok(_) => todo!(),
            Err(e) => {
                eprintln!("Error: {e}");
                *socket = None;
                return None;
            }
        };
        if let Some(sequance_number) = json.s {
            discord_stream.last_sequance_number = Some(sequance_number);
        };
        println!("Received: {:#?}", json.op);

        match json.op {
            Opcode::Hello => {
                let mut heart_beat_interval = self.heart_beat_interval.write().await;
                *heart_beat_interval = json
                    .d
                    .get("heartbeat_interval")
                    .and_then(|v| v.as_u64())
                    .map(Duration::from_millis);
                println!("{:?}", *heart_beat_interval);

                // Send Identify payload
                let identify_payload = json!({
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
                });
                discord_stream
                    .websocket
                    .send(Message::Text(identify_payload.to_string().into()))
                    .await
                    .expect("Failed to send identify payload");
            }

            Opcode::Dispatch => {
                let event_name = json.t.unwrap();
                println!("{event_name:?}");
                match event_name.as_str() {
                    "READY" => {
                        println!("importing data");
                    }
                    "SESSIONS_REPLACE" => {
                        println!("something something");
                    }
                    "MESSAGE_CREATE" => {
                        let channel_id = json
                            .d
                            .get("channel_id")
                            .and_then(|id| id.as_str().map(|s| s.to_string()))
                            .unwrap();
                        let msg_id = json
                            .d
                            .get("id")
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

                        return Some(SocketEvent::MessageCreated {
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
                    _ => eprintln!("Unkown event_name recived: {event_name:?}",),
                }
            }
            _ => {}
        };
        Some(SocketEvent::Skip)
    }
}
