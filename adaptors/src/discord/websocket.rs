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
    async fn next(&self) -> usize {
        let mut unlock = self.stream.lock().await;
        let test = unlock.next().await;
        if let Some(val) = test {
            println!("{:#?}", val);
        }

        1
    }
}

#[async_trait]
impl Socket for Discord {
    async fn get_stream(&self) -> Arc<dyn TestStream + Send + Sync> {
        // TODO: Upgrade to use TLS
        let gateway_url = "wss://gateway.discord.gg/?encoding=json&v=9";
        let (mut socket, response) = connect_async(gateway_url)
            .await
            .expect("Failed to connect to Discord gateway");

        println!("Connected to Discord gateway");
        println!("Response HTTP code: {}", response.status());

        // Read hello message
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

        // Example loop to read messages
        // while let Some(msg) = socket.next().await {
        //     match msg {
        //         Ok(Message::Text(text)) => {
        //             println!("Text: {}", text)
        //         }
        //         Ok(Message::Close(frame)) => {
        //             println!("Disconnected: {:?}", frame);
        //             break;
        //         }
        //         Ok(_) => {}
        //         Err(e) => {
        //             eprintln!("Error: {}", e);
        //             break;
        //         }
        //     }
        // }

        let stream = DiscordStream {
            stream: socket.into(),
        };

        Arc::new(stream)
    }
}
