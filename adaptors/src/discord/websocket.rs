use std::sync::Arc;

use async_trait::async_trait;
use async_tungstenite::{
    WebSocketStream,
    async_std::{ConnectStream, connect_async},
    tungstenite::Message,
};
use futures::{StreamExt, lock::Mutex};
use serde_json::json;

use crate::{Socket, TestStream};

use super::Discord;

struct DiscordStream {
    stream: Mutex<WebSocketStream<ConnectStream>>,
}

#[async_trait]
impl TestStream for DiscordStream {
    async fn next(&self) -> Option<usize> {
        let mut unlock = self.stream.lock().await;
        let msg = unlock.next().await?;

        match msg {
            Ok(Message::Text(text)) => {
                println!("Text: {}", text);
                return Some(2);
            }
            Ok(Message::Close(frame)) => {
                println!("Disconnected: {:?}", frame);
                return None;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error: {}", e);
                return None;
            }
        }

        None
    }
}

#[async_trait]
impl Socket for Discord {
    async fn get_stream(&self) -> Arc<dyn TestStream + Send + Sync> {
        let gateway_url = "wss://gateway.discord.gg/?encoding=json&v=9";
        let (mut socket, response) = connect_async(gateway_url)
            .await
            .expect("Failed to connect to Discord gateway");

        println!("Response HTTP code: {}", response.status());

        if let Some(Ok(msg)) = socket.next().await {
            println!("Received: {}", msg);
        }

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

        socket
            .send(Message::Text(identify_payload.to_string().into()))
            .await
            .expect("Failed to send identify payload");

        let stream = DiscordStream {
            stream: socket.into(),
        };

        Arc::new(stream)
    }
}
