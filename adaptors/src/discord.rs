use std::{
    collections::HashMap,
    fmt::Debug,
    future::poll_fn,
    hash::{DefaultHasher, Hash, Hasher},
    pin::pin,
    sync::{Arc, Weak},
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use async_tungstenite::{
    WebSocketStream,
    async_std::{ConnectStream, connect_async},
    tungstenite::Message as WebSocketMessage,
};
use futures::{FutureExt, Stream, StreamExt, lock::Mutex, pending, poll};
use futures_locks::RwLock as RwLockAwait;
use serde::Deserialize;
use serde_json::json;

use crate::{
    Messanger, MessangerQuery, ParameterizedMessangerQuery, Socket,
    discord::{
        main_socket::Opcode,
        vc_socket::{VCConnection, VCOpcode},
        websocket::{HeartBeatingData, VCLoc},
    },
    types::{ID, Identifier},
};
use crate::{SocketEvent, VC};

pub mod json_structs;
pub mod main_socket;
pub mod rest_api;
pub mod vc_socket;
pub mod websocket;

/// <https://discord.com/developers/docs/events/gateway-events#payload-structure>
#[derive(Debug, Deserialize)]
struct GatewayPayload<Op> {
    // Opcode
    op: Op,
    // Event type
    t: Option<String>,
    // Sequence numbers
    s: Option<usize>,
    // data
    d: serde_json::Value,
}

#[derive(Default)]
struct DiscordSockets {
    // Main
    gateway_websocket: Option<WebSocketStream<ConnectStream>>,
    heart_beating: Option<HeartBeatingData>,
    last_sequence_number: Option<usize>,

    // VC
    vc_websocket: Option<WebSocketStream<ConnectStream>>,
    vc_heart_beating: Option<HeartBeatingData>,

    vc_location: VCLoc,
    vc_connection: Option<VCConnection>,
    vc_last_sequence_number: Option<usize>,
}

type Events = (
    Option<GatewayPayload<Opcode>>,
    Option<GatewayPayload<VCOpcode>>,
);
impl DiscordSockets {
    fn nuke_main_gateway(&mut self) {
        println!("Erasing gateway related information");
        self.gateway_websocket = None;
        self.last_sequence_number = None;
        self.heart_beating = None;
    }
    fn nuke_vc_gateway(&mut self) {
        println!("Erasing VC related information");
        self.vc_websocket = None;
        self.vc_heart_beating = None;
        // self.vc_location.clear();
        self.vc_connection = None;
    }

    fn fetch_events(&mut self, cx: &mut Context<'_>) -> Poll<Events> {
        let mut events: Events = (None, None);

        if let Some(socket) = self.gateway_websocket.as_mut()
            && let Poll::Ready(event) = socket.select_next_some().poll_unpin(cx)
        {
            let deserialized_event = match &event {
                Ok(event) => deserialize_event::<Opcode>(event),
                Err(err) => Err(Box::new(err) as Box<dyn std::error::Error>),
            };

            match deserialized_event {
                Ok(unwraped_event) => events.0 = Some(unwraped_event),
                Err(err) => {
                    eprintln!("gateway_event deserialization failed: {err:#?}");
                    self.nuke_main_gateway();
                }
            }
        };

        if let Some(vc_socket) = self.vc_websocket.as_mut()
            && let Poll::Ready(vc_event) = vc_socket.select_next_some().poll_unpin(cx)
        {
            let deserialized_event = match &vc_event {
                Ok(event) => deserialize_event::<VCOpcode>(event),
                Err(err) => Err(Box::new(err) as Box<dyn std::error::Error>),
            };

            match deserialized_event {
                Ok(unwraped_event) => events.1 = Some(unwraped_event),
                Err(err) => {
                    eprintln!(
                        "vc_event failed to deserialize: {vc_event:#?}\n With error: {err:#?}"
                    );
                    self.nuke_vc_gateway();
                }
            }
        }

        if events.0.is_none() && events.1.is_none() {
            return Poll::Pending;
        }

        Poll::Ready(events)
    }
}

struct ChannelID {
    guild_id: Option<String>,
    id: String,
}

type GuildID = String;
type MessageID = String;
pub struct Discord {
    // Metadata
    token: String, // TODO: Make it secure
    intents: u32,
    // Owned data
    socket: Mutex<DiscordSockets>,
    // Cache (External IDs, to internal)
    profile: RwLockAwait<Option<json_structs::Profile>>,
    channel_id_mappings: RwLockAwait<HashMap<ID, ChannelID>>,
    guild_id_mappings: RwLockAwait<HashMap<ID, GuildID>>,
    msg_data: RwLockAwait<HashMap<ID, MessageID>>,
}

impl Discord {
    pub fn new(token: &str) -> Self {
        Discord {
            token: token.into(),
            intents: 161789, // 32767,
            socket: DiscordSockets::default().into(),
            profile: RwLockAwait::new(None),
            guild_id_mappings: RwLockAwait::new(HashMap::new()),
            channel_id_mappings: RwLockAwait::new(HashMap::new()),
            msg_data: RwLockAwait::new(HashMap::new()),
        }
    }
    fn id(&self) -> String {
        self.name().to_owned() + &self.token
    }
    fn name(&self) -> &'static str {
        "Discord"
    }
    fn discord_id_to_internal_id(id: &str) -> u32 {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        hasher.finish() as u32
    }
    fn identifier_generator<D>(id: &str, data: D) -> Identifier<D> {
        Identifier {
            neo_id: Discord::discord_id_to_internal_id(id),
            data,
        }
    }
}

impl Debug for Discord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discord").finish()
    }
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

#[async_trait]
impl Messanger for Discord {
    fn id(&self) -> String {
        self.id()
    }
    // === Unify a bit ===
    fn name(&self) -> &'static str {
        self.name()
    }
    fn auth(&self) -> String {
        self.token.clone()
    }
    fn query(&self) -> Option<&dyn MessangerQuery> {
        Some(self)
    }
    fn param_query(&self) -> Option<&dyn ParameterizedMessangerQuery> {
        Some(self)
    }
    async fn socket(self: Arc<Self>) -> Option<Weak<dyn Socket + Send + Sync>> {
        let mut socket = self.socket.lock().await;

        if socket.gateway_websocket.is_none() {
            let gateway_url = "wss://gateway.discord.gg/?encoding=json&v=9";
            let (stream, response) = connect_async(gateway_url)
                .await
                .expect("Failed to connect to Discord gateway");

            println!("Response HTTP code: {}", response.status());

            socket.gateway_websocket = Some(stream);
        };
        Some(Arc::<Discord>::downgrade(&self) as Weak<dyn Socket + Send + Sync>)
    }
    fn vc(&self) -> Option<&dyn VC> {
        Some(self)
    }
}

#[async_trait]
impl Socket for Discord {
    async fn next(self: Arc<Self>) -> Option<SocketEvent> {
        let mut buff = [0; 1024];
        let (event, vc_event) = loop {
            let mut socket = self.socket.lock().await;
            let DiscordSockets {
                gateway_websocket,
                heart_beating,
                last_sequence_number,
                //
                vc_websocket,
                vc_heart_beating,
                vc_last_sequence_number,
                ..
            } = &mut *socket;

            // Main socket heartbeat
            if let Some(websocket) = gateway_websocket
                && let Some(heart_beating_data) = heart_beating
                && heart_beating_data.is_beat_time().await
            {
                websocket
                    .send(
                        json!({
                                "op": Opcode::Heartbeat as u8,
                                "d": last_sequence_number,
                        })
                        .to_string()
                        .into(),
                    )
                    .await
                    .unwrap();
            }

            // VC socket heartbeat
            if let Some(vc_websocket) = vc_websocket
                && let Some(heart_beating_data) = vc_heart_beating
                && heart_beating_data.is_beat_time().await
            {
                let time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                // https://discord.com/developers/docs/topics/voice-connections#heartbeating-example-hello-payload
                let v = heart_beating_data.version().unwrap();
                let heartbeat = if v < 8 {
                    json!({
                        "op": VCOpcode::Heartbeat as u8,
                        "d": time,
                    })
                } else {
                    json!({
                        "op": VCOpcode::Heartbeat as u8,
                        "d": {
                            "t": time,
                            "seq_ack": vc_last_sequence_number,
                        }
                    })
                };

                vc_websocket
                    .send(heartbeat.to_string().into())
                    .await
                    .unwrap();
            }

            // Pull socket event
            if let Poll::Ready(events) = poll!(poll_fn(|cx| socket.fetch_events(cx))) {
                break events;
            };

            // Pull UDP VC socket
            let udp = match socket.vc_connection.as_ref() {
                Some(vc_connection) => {
                    let udp = vc_connection.udp();
                    Some(udp)
                }
                None => None,
            };

            drop(socket); // Otherwise it blocks socket for other things on the runtime

            if let Some(udp) = udp {
                let udp = udp.lock().await;
                let fut = pin!(udp.recv_from(&mut buff));

                match poll!(fut) {
                    Poll::Ready(data) => {
                        println!("Data: {:#?}", data)
                    }
                    Poll::Pending => {
                        pending!()
                    }
                }
            } else {
                pending!()
            };
        };
        if let Some(vc_event) = vc_event {
            println!("Executing VC event.");
            match vc_event.exec(&self).await {
                Ok(_) => {}
                Err(err) => {
                    eprintln!("Failed to execute vc event:\n{err:#?}");
                    return None;
                }
            }
        }

        // Run, and return SocketEvent
        if let Some(event) = event {
            println!("Executing gateway event.");
            match event.exec(&self).await {
                Ok(event) => return Some(event),
                Err(err) => {
                    eprintln!("Failed to execute gateway event:\n{err:#?}");
                    return None;
                }
            };
        };
        Some(SocketEvent::Skip)
    }
}

fn deserialize_event<Op: for<'a> Deserialize<'a>>(
    event: &WebSocketMessage,
) -> Result<GatewayPayload<Op>, Box<dyn std::error::Error>> {
    let json = match event {
        WebSocketMessage::Text(text) => serde_json::from_str::<GatewayPayload<Op>>(text).unwrap(),
        WebSocketMessage::Binary(_) => todo!(),
        WebSocketMessage::Frame(frame) => {
            return Err(format!("Frame: {frame:?}").into());
        }
        WebSocketMessage::Close(frame) => {
            return Err(format!("Close frame: {frame:?}").into());
        }
        WebSocketMessage::Ping(_) => todo!(),
        WebSocketMessage::Pong(_) => todo!(),
    };
    Ok(json)
}
