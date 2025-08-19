use async_trait::async_trait;
use async_tungstenite::async_std::ConnectStream;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::{WebSocketStream, async_std::connect_async};
use futures::{FutureExt, Stream, StreamExt, pending, poll};
use futures_timer::Delay;
use serde::Deserialize;
use serde_json::json;
use serde_repr::Deserialize_repr;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use crate::VCLocation;
use crate::types::{Identifier, Msg, Usr};
use crate::{Socket, SocketEvent, VC};

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
    PresenceUpdate = 3,
    VoiceStateUpdate = 4,
    Hello = 10,
    HeartbeatAck = 11,
}

pub(super) struct DiscordSocket {
    pub websocket: WebSocketStream<ConnectStream>,
    last_sequance_number: Option<usize>,
    // VC
    vc_websocket: Option<WebSocketStream<ConnectStream>>,
    vc_location: Option<(String, String)>, // (token, endpoint)
    vc_session_id: Option<String>,
}
impl DiscordSocket {
    pub fn new(websocket: WebSocketStream<ConnectStream>) -> Self {
        DiscordSocket {
            websocket,
            last_sequance_number: None,
            vc_websocket: None,
            vc_location: None,
            vc_session_id: None,
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
        let event = loop {
            // Checks heartbeats
            {
                let mut heart_beat_future = self.heart_beat_future.lock().await;
                if heart_beat_future.is_none() {
                    *heart_beat_future = Some(Box::pin(self.clone().heart_beat()));
                };
                if poll!(heart_beat_future.as_mut().unwrap()).is_ready() {
                    *heart_beat_future = None
                }
            }

            let mut socket = self.socket.lock().await;
            // Pull vc event
            {
                if let Some(a) = socket.as_mut()?.vc_websocket.as_mut() {
                    if let Poll::Ready(event) = poll!(a.next()) {
                        println!("VC event: {event:#?}")
                    };
                };
            }

            // Pull next event
            {
                let next_event = poll!(socket.as_mut()?.websocket.next());
                if let Poll::Ready(event) = next_event {
                    break event;
                }
            }
            pending!()
        };
        let mut socket = self.socket.lock().await;
        let discord_stream = socket.as_mut()?;

        let json = match event? {
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
                let event_name = json.t.as_ref().unwrap();
                println!("{event_name:?}");
                match event_name.as_str() {
                    "READY" => {
                        println!("importing data");
                    }
                    "SESSIONS_REPLACE" => {
                        println!("something something");
                    }
                    "VOICE_STATE_UPDATE" => {
                        let session_id = json
                            .d
                            .get("session_id")
                            .and_then(|session_id| session_id.as_str().map(|s| s.to_string()))
                            .unwrap();
                        discord_stream.vc_session_id = Some(session_id);
                        if discord_stream.vc_websocket.is_none() {
                            let profile = self.profile.read().await;
                            let profile = profile.as_ref();
                            let user_id = profile.unwrap().id.as_str();
                            connect_vc(user_id, discord_stream).await;
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
                        discord_stream.vc_location = Some((token, endpoint));
                        if discord_stream.vc_websocket.is_none() {
                            let profile = self.profile.read().await;
                            let profile = profile.as_ref();
                            let user_id = profile.unwrap().id.as_str();
                            connect_vc(user_id, discord_stream).await;
                        }
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

                        let channel_id_hash = Discord::discord_id_to_u32_id(channel_id.as_str());
                        let msg_id_hash = Discord::discord_id_to_u32_id(channel_id.as_str());
                        let author_id_hash = Discord::discord_id_to_u32_id(author_id.as_str());

                        return Some(SocketEvent::MessageCreated {
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
                    _ => eprintln!("Unkown event_name recived: {event_name:?}",),
                }
            }
            _ => {}
        };
        Some(SocketEvent::Skip)
    }
}

#[async_trait]
impl VC for Discord {
    async fn connect<'a>(&'a self, location: VCLocation<'a>) {
        let mut socket = self.socket.lock().await;
        let socket = socket.as_mut().unwrap();
        let websocket = &mut socket.websocket;

        // let t = self.guild_data.read().await;
        let t1 = self.channel_data.read().await;
        let (guild_id, channel_id) = {
            match location {
                VCLocation::Direct(identifier) => {
                    let a = t1.get(identifier.get_id()).unwrap();
                    (&a.id, &a.id)
                }
                VCLocation::Server => todo!(),
            }
        };

        let connection_payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": guild_id,
                "channel_id": channel_id,
                "self_mute": false,
                "self_deaf": false
              }
        });
        websocket
            .send(connection_payload.to_string().into())
            .await
            .unwrap();
    }
}

async fn connect_vc(user_id: &str, socket: &mut DiscordSocket) {
    if let Some((token, endpoint)) = socket.vc_location.as_ref()
        && let Some(session_id) = socket.vc_session_id.as_ref()
    {
        println!("{endpoint:?}");
        let (mut stream, response) = connect_async("wss://".to_string() + endpoint)
            .await
            .unwrap();
        println!("Response HTTP code: {}", response.status());

        let handshake_payload = json!({
          "op": 0,
          "d": {
            "server_id": "743307525524422666",
            "user_id": user_id,
            "session_id": session_id,
            "token": token,
          }
        });

        stream
            .send(handshake_payload.to_string().into())
            .await
            .unwrap();

        socket.vc_websocket = Some(stream);
    }
}
