use std::time::Duration;
use async_trait::async_trait;
use async_tungstenite::async_std::ConnectStream;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::WebSocketStream;
use futures::lock::Mutex;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use serde_repr::Deserialize_repr;

use crate::{Socket, SocketUpdate};

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
    pub heart_beat_interval: Option<Duration>
}
impl DiscordSocket {
    pub fn new() -> Self {
        DiscordSocket {
            websocket: None.into(),
            heart_beat_interval: None
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
        // let stream = discord_stream.websocket.as_mut()?;
        
        
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
                println!("{:#?}", json);
                discord_stream.heart_beat_interval = json.d.get("heartbeat_interval")
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
                discord_stream.websocket.as_mut()?
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
                        println!("{:#?}", json.d);
                        return Some(SocketUpdate::MessageCreated);
                    }
                    _ => eprintln!("Unkown event_name recived: {:?}", event_name),
                }
            }
            _=> {}
        };

        Some(SocketUpdate::Skip)
    }
}
