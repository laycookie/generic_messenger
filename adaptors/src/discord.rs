use std::{
    collections::HashMap,
    fmt::Debug,
    future::poll_fn,
    hash::{DefaultHasher, Hash, Hasher},
    pin::pin,
    sync::{Arc, Weak, mpsc::Sender},
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use async_tungstenite::{
    WebSocketStream,
    async_std::{ConnectStream, connect_async},
    tungstenite::Message as WebSocketMessage,
};
use discortp::{Packet, rtp::RtpPacket};
use futures::{FutureExt, Stream, StreamExt, lock::Mutex, pending, poll};
use futures_locks::RwLock as RwLockAwait;
use libsodium_rs::crypto_aead;
use serde::Deserialize;
use serde_json::json;

use crate::{
    Messanger, MessangerQuery, ParameterizedMessangerQuery, Socket,
    discord::{
        main_socket::Opcode,
        vc_socket::{EncryptionMode, VCConnection, VCOpcode},
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

    audio_sender: Sender<i16>,
}
impl DiscordSockets {
    fn new(audio_sender: Sender<i16>) -> Self {
        Self {
            gateway_websocket: Default::default(),
            heart_beating: Default::default(),
            last_sequence_number: Default::default(),
            vc_websocket: Default::default(),
            vc_heart_beating: Default::default(),
            vc_location: Default::default(),
            vc_connection: Default::default(),
            vc_last_sequence_number: Default::default(),
            audio_sender,
        }
    }
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
    pub fn new(token: &str, sender: Sender<i16>) -> Self {
        Discord {
            token: token.into(),
            intents: 161789,
            socket: DiscordSockets::new(sender).into(),
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
        let mut rtp_packet_buff = [0; 1024];
        let mut decoded_audio = [0; 8048];
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
            if let Some(vc_connection) = socket.vc_connection.as_mut()
                && let Some(description) = vc_connection.description()
            {
                let n_bytes_in_packet = {
                    let udp = vc_connection.udp();
                    match poll!(pin!(udp.recv_from(&mut rtp_packet_buff))) {
                        Poll::Ready(Ok((bytes_recived, _))) => bytes_recived,
                        Poll::Ready(Err(err)) => {
                            panic!("{err}")
                        }
                        Poll::Pending => {
                            drop(socket); // Otherwise it blocks socket for other things on the runtime
                            pending!();
                            continue;
                        }
                    }
                };

                #[derive(Debug, PartialEq)]
                enum PacketType {
                    Voice,
                    Unkown1,
                    Unkown,
                }

                let packet_type = match rtp_packet_buff[1] {
                    0x78 => PacketType::Voice,
                    0xc9 => PacketType::Unkown1,
                    _ => PacketType::Unkown,
                };

                if packet_type != PacketType::Voice {
                    eprintln!("Unkown packet type on udp: {:?}", rtp_packet_buff[1]);
                    continue;
                };

                let rtp_packet = RtpPacket::new(&rtp_packet_buff[..n_bytes_in_packet]).unwrap();
                let is_rtp_extended = rtp_packet.get_extension() != 0;

                let rtp_header_len = if is_rtp_extended {
                    rtp_packet.packet().len() - rtp_packet.payload().len() + 4
                } else {
                    println!("None-extended");
                    rtp_packet.packet().len() - rtp_packet.payload().len()
                };

                let (rtp_header, rtp_body) = rtp_packet.packet().split_at(rtp_header_len);

                let mode = description.mode().unwrap();
                let decrypted_payload = match mode {
                    EncryptionMode::aead_aes256_gcm_rtpsize => todo!(),
                    EncryptionMode::aead_xchacha20_poly1305_rtpsize => {
                        let (voice_payload, nonce_u32) =
                            rtp_body.split_at(rtp_body.len() - mode.nonce_size());

                        let mut nonce = [0; 24];
                        nonce[..mode.nonce_size()].copy_from_slice(nonce_u32);
                        let nonce = crypto_aead::xchacha20poly1305::Nonce::from_bytes(nonce);

                        let key = crypto_aead::xchacha20poly1305::Key::from_bytes(
                            &description.secret_key().unwrap()[..],
                        )
                        .expect("Invalid key length");

                        crypto_aead::xchacha20poly1305::decrypt(
                            voice_payload,
                            Some(rtp_header),
                            &nonce,
                            &key,
                        )
                        .unwrap()
                    }
                    EncryptionMode::aead_aes256_gcm => todo!("Depricated"),
                    EncryptionMode::xsalsa20_poly1305 => todo!("Depricated"),
                    EncryptionMode::xsalsa20_poly1305_suffix => todo!("Depricated"),
                    EncryptionMode::xsalsa20_poly1305_lite => todo!("Depricated"),
                    EncryptionMode::xsalsa20_poly1305_lite_rtpsize => todo!("Depricated"),
                };
                println!();

                // <https://datatracker.ietf.org/doc/html/rfc6464>
                let (potentially, voice_data) = decrypted_payload.split_at(8);
                let unkown_const = &potentially[..1]; // CONST 55
                let timecode_unkown = &potentially[1..4]; // Timecode
                let unkown_const_2 = &potentially[4..5]; // CONST 16
                let avrage_volume = &potentially[5..6]; // Avrage volume of the frame?
                let unkown_const_3 = &potentially[6..7]; // CONST 144
                let channels = &potentially[7..]; // Channels?
                if unkown_const != [50] {
                    eprintln!("ANOMOLY const1");
                }
                if unkown_const_2 != [16] {
                    eprintln!("ANOMOLY const2");
                }
                if unkown_const_3 != [144] {
                    eprintln!("ANOMOLY const2");
                }
                // println!("timecode: {:?}", timecode_unkown);
                // println!("Avrage volume: {:?}", avrage_volume[0] as i8);
                // println!("Channels: {:?}", channels);
                // println!("{:?}", potentially);
                // println!();

                // let out = Vec::new();

                println!("{:?}", voice_data);
                println!("{:?}", voice_data.len());
                let n_decoded_samples =
                    match vc_connection
                        .decoder()
                        .decode(voice_data, &mut decoded_audio, false)
                    {
                        Ok(n_samples) => n_samples,
                        Err(err) => {
                            eprintln!("{:?}", err);
                            continue;
                        }
                    };

                // if channels[0] != 0 {
                decoded_audio[..(2 * n_decoded_samples)]
                    .iter()
                    .for_each(|byte| socket.audio_sender.send(*byte).unwrap());
                // }
                println!("{:?}", &decoded_audio[..(2 * n_decoded_samples)].len());
            } else {
                drop(socket); // Otherwise it blocks socket for other things on the runtime
                pending!();
                continue;
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
