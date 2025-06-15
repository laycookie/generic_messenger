use async_trait::async_trait;
use async_tungstenite::tungstenite::Message;
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
        let mut stream = self.socket.lock().await;
        let stream = stream.as_mut()?;

        let json = match stream.next().await? {
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
        // println!("Received: {:#?}", json.op);

        match json.op {
            Opcode::Hello => {
                println!("Identify self");
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
                stream
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
            _ => {}
        };

        Some(SocketUpdate::Skip)
    }
}
