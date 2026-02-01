use std::{
    collections::{HashMap, VecDeque},
    error::Error,
    io, mem,
    time::Duration,
};

use async_tungstenite::{async_std::connect_async, tungstenite::Message};
use facet::Facet;
use futures::{StreamExt as _, channel::oneshot};
use simple_audio_channels::{CHANNEL_BUFFER_SIZE, input::Input, output::Output};
use smol::net::UdpSocket;
use surf::http::convert::json;
use tracing::{error, info, warn};

use crate::{
    Discord,
    gateaways::{
        Gateaway, GateawayStream, GatewayPayload, HeartBeatingData, deserialize_event,
        voice::connection::{Connection, EncryptionMode, SessionDescription, Ssrc},
    },
};

pub(super) mod connection;

/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#voice>
/// <https://docs.discord.food/topics/opcodes-and-status-codes#voice-opcodes>
#[repr(u8)]
#[non_exhaustive]
#[derive(Debug, Facet)]
#[facet(is_numeric)]
pub enum VoiceOpcode {
    Identify = 0,
    SelectProtocol = 1,
    Ready = 2,
    Heartbeat = 3,
    SessionDescription = 4,
    Speaking = 5,
    Hello = 8,
    ClientConnect = 11,
    Video = 12,
    ClientDisconnect = 13,
    ClientFlags = 18,
    ClientPlatform = 20,
}

/// <https://docs.discord.food/topics/voice-connections#hello-structure>
#[derive(Facet)]
struct HelloPayload {
    v: u8,
    heartbeat_interval: u64,
}

/// https://docs.discord.food/topics/voice-connections#ready-structure
#[derive(Facet)]
struct ReadyPayload {
    ssrc: Ssrc,
    ip: String,
    port: u16,
    modes: Vec<EncryptionMode>,
    experiments: Vec<String>,
    // streams:	Vec<stream object>
}

type SessionId = String;
pub struct Endpoint {
    wss: String,
    token: String,
}
impl Endpoint {
    pub fn new(wss: String, token: String) -> Self {
        Self { wss, token }
    }
}

#[derive(Default)]
pub enum VoiceGateawayState {
    #[default]
    Closed,
    AwaitingData,
    AwaitingEndpoint(SessionId),
    AwaitingSession(Endpoint),
    Ready {
        endpoint: Endpoint,
        session_id: SessionId,
    },
    Open {
        gateaway: Box<Gateaway<Voice>>,
        endpoint: Endpoint,
        session_id: SessionId,
    },
}
impl VoiceGateawayState {
    pub fn as_mut(&mut self) -> &mut VoiceGateawayState {
        self
    }
    pub fn mut_gateaway(&mut self) -> Option<&mut Gateaway<Voice>> {
        match self {
            VoiceGateawayState::Open { gateaway, .. } => Some(gateaway),
            _ => None,
        }
    }
    pub fn insert_endpoint(&mut self, endpoint: Endpoint) {
        *self = match mem::take(self) {
            VoiceGateawayState::Closed => VoiceGateawayState::Closed,
            VoiceGateawayState::AwaitingData => Self::AwaitingSession(endpoint),
            VoiceGateawayState::AwaitingEndpoint(session_id) => Self::Ready {
                endpoint,
                session_id,
            },
            VoiceGateawayState::AwaitingSession(_) => Self::AwaitingSession(endpoint),
            VoiceGateawayState::Ready {
                endpoint,
                session_id,
            } => Self::Ready {
                endpoint,
                session_id,
            },
            VoiceGateawayState::Open {
                gateaway,
                session_id,
                ..
            } => Self::Open {
                gateaway,
                endpoint,
                session_id,
            },
        };
    }
    pub fn insert_session_id(&mut self, session_id: SessionId) {
        *self = match mem::take(self) {
            VoiceGateawayState::Closed => VoiceGateawayState::Closed,
            VoiceGateawayState::AwaitingData => Self::AwaitingEndpoint(session_id),
            VoiceGateawayState::AwaitingEndpoint(_) => Self::AwaitingEndpoint(session_id),
            VoiceGateawayState::AwaitingSession(endpoint) => Self::Ready {
                endpoint,
                session_id,
            },
            VoiceGateawayState::Ready {
                endpoint,
                session_id,
            } => Self::Ready {
                endpoint,
                session_id,
            },
            VoiceGateawayState::Open {
                gateaway, endpoint, ..
            } => Self::Open {
                gateaway,
                endpoint,
                session_id,
            },
        }
    }
    pub async fn connect(
        self,
        user_id: &str,
        location_id: &str,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let (endpoint, session_id) = match self {
            VoiceGateawayState::Ready {
                endpoint,
                session_id,
            }
            | VoiceGateawayState::Open {
                endpoint,
                session_id,
                ..
            } => (endpoint, session_id),
            _ => {
                return Err("Does not have enough info about the peer to connect".into());
            }
        };

        let gateaway = Gateaway::<Voice>::new(&endpoint, &session_id, location_id, user_id).await?;

        Ok(Self::Open {
            gateaway: Box::new(gateaway),
            endpoint,
            session_id,
        })
    }
    pub fn close_gateway(&mut self) {
        *self = match mem::take(self) {
            VoiceGateawayState::Open {
                endpoint,
                session_id,
                ..
            } => Self::Ready {
                endpoint,
                session_id,
            },
            prev_state => {
                error!("Gateway already closed");
                prev_state
            }
        }
    }
}

pub enum AudioChannel {
    Initilizing(oneshot::Receiver<Output<CHANNEL_BUFFER_SIZE>>),
    Connected(Output<CHANNEL_BUFFER_SIZE>),
}

#[derive(Default)]
pub enum InputChannel {
    #[default]
    None,
    Initilizing(oneshot::Receiver<Input<CHANNEL_BUFFER_SIZE>>),
    Connected(Input<CHANNEL_BUFFER_SIZE>),
}

pub struct Voice {
    pub ssrc_to_audio_channel: HashMap<Ssrc, AudioChannel>,
    pub connection: Option<Connection>,
    pub input_channel: InputChannel,
    pub input_buffer: VecDeque<f32>,
    pub is_speaking: bool,
}
impl Gateaway<Voice> {
    pub async fn new(
        endpoint: &Endpoint,
        session_id: &SessionId,
        location_id: &str,
        user_id: &str,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let (mut websocket, _) = connect_async("wss://".to_string() + &endpoint.wss).await?;

        // <https://discord.com/developers/docs/topics/voice-connections#establishing-a-voice-websocket-connection>
        // TODO: I believe this payload should change with gateway v9
        let identify_payload = json!({
          "op": VoiceOpcode::Identify as u8,
          "d": {
            // The ID of the guild, private channel, stream, or lobby being connected to
            "server_id": location_id, // TODO
            "user_id": user_id,
            "session_id": session_id,
            "token": endpoint.token,
          }
        });
        websocket
            .send(identify_payload.to_string().into())
            .await
            .unwrap();

        let hello_event = websocket.next_gateaway_payload().await;

        let VoiceOpcode::Hello = hello_event.op else {
            return Err(Box::new(io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Expected to recive hello event as the first event",
            )));
        };

        let hello_d = facet_value::from_value::<HelloPayload>(hello_event.d).unwrap();
        let heart_beating_duration = Duration::from_millis(hello_d.heartbeat_interval);

        Ok(Self {
            websocket,
            heart_beating: HeartBeatingData::new(heart_beating_duration),
            last_sequence_number: None,
            type_specific_data: Voice {
                connection: None,
                ssrc_to_audio_channel: HashMap::new(),
                input_channel: Default::default(),
                input_buffer: VecDeque::new(),
                is_speaking: false,
            },
        })
    }
    pub async fn fetch_event(
        &mut self,
    ) -> Result<Option<GatewayPayload<VoiceOpcode>>, Box<dyn std::error::Error + Send + Sync>> {
        match self.websocket.next().await {
            Some(event) => Ok(Some(deserialize_event::<VoiceOpcode>(&event?)?)),
            None => Ok(None),
        }
    }
}

#[allow(non_camel_case_types)]
#[derive(Copy, Clone)]
#[repr(transparent)]
struct u16be(u16);
impl u16be {
    pub fn get(self) -> u16 {
        u16::from_be(self.0)
    }
}
impl From<u16> for u16be {
    fn from(value: u16) -> Self {
        Self(value.to_be())
    }
}

#[allow(non_camel_case_types)]
#[derive(Copy, Clone)]
#[repr(transparent)]
struct u32be(u32);
impl u32be {
    pub fn get(self) -> u32 {
        u32::from_be(self.0)
    }
}
impl From<u32> for u32be {
    fn from(value: u32) -> Self {
        Self(value.to_be())
    }
}

/// <https://discord.com/developers/docs/topics/voice-connections#ip-discovery>
#[repr(Rust, packed)]
struct IpDiscovery {
    _req_or_res: u16be,
    _length: u16be,
    ssrc: u32be,
    address_ascii: [u8; 64],
    port: u16be,
}

impl GatewayPayload<VoiceOpcode> {
    pub async fn exec(self, discord: &Discord) -> Result<(), Box<dyn Error>> {
        let mut voice_gateaway = discord.voice_gateaway.lock().await;
        if let Some(gateaway) = voice_gateaway.mut_gateaway()
            && let Some(s) = self.s
        {
            gateaway.last_sequence_number = Some(s);
        };
        drop(voice_gateaway);

        info!("{:?}", self.op);
        match self.op {
            VoiceOpcode::SessionDescription => {
                let session_description = facet_value::from_value::<SessionDescription>(self.d)?;
                if let Some(voice_gateaway) = discord.voice_gateaway.lock().await.mut_gateaway()
                    && let Some(connection) = &mut voice_gateaway.connection
                {
                    connection.set_description(session_description);
                };
            }
            VoiceOpcode::Ready => {
                let ready = facet_value::from_value::<ReadyPayload>(self.d).unwrap();

                // TODO: Not hard code it maybe?
                if !ready
                    .modes
                    .contains(&EncryptionMode::aead_xchacha20_poly1305_rtpsize)
                {
                    return Err("Encryption not supported".into());
                }

                let mut address_ascii = [0; 64];
                address_ascii[..ready.ip.len()].copy_from_slice(ready.ip.as_bytes());

                let send_ip_discovery = unsafe {
                    std::mem::transmute::<IpDiscovery, [u8; 74]>(IpDiscovery {
                        _req_or_res: 1.into(),
                        _length: 70.into(),
                        ssrc: ready.ssrc.into(),
                        address_ascii,
                        port: ready.port.into(),
                    })
                };
                let udp = UdpSocket::bind("0.0.0.0:0").await.unwrap();
                info!("Addr: {:?}", udp.local_addr());
                udp.connect((ready.ip.as_str(), ready.port)).await.unwrap();
                udp.send(&send_ip_discovery).await.unwrap();

                let mut buf = [0u8; 74];
                match udp.recv(&mut buf).await {
                    Ok(len) => println!("Got {len} bytes\n{buf:?}"),
                    Err(e) => eprintln!("No response: {e:?}"),
                }
                let recv_ip_discovery = unsafe { mem::transmute::<[u8; 74], IpDiscovery>(buf) };

                let mut ip_address = str::from_utf8(&recv_ip_discovery.address_ascii).unwrap();
                if let Some(null_position) =
                    recv_ip_discovery.address_ascii.iter().position(|c| *c == 0)
                {
                    ip_address = &ip_address[..null_position]
                };

                let mut voice_gateaway = discord.voice_gateaway.lock().await;
                let voice_gateaway = match voice_gateaway.mut_gateaway() {
                    Some(gateaway) => gateaway,
                    None => return Err("Gateaway has closed".into()),
                };

                voice_gateaway.connection =
                    Some(Connection::new(udp, recv_ip_discovery.ssrc.get()));
                let protocol_select = json!({
                    "op": VoiceOpcode::SelectProtocol as u8,
                    "d": {
                        "protocol": "udp",
                        "data": {
                            "address": ip_address,
                            "port": recv_ip_discovery.port.get(),
                            // TODO: We are hard coding it just for rn
                            "mode": EncryptionMode::aead_xchacha20_poly1305_rtpsize.as_str(),
                        },
                        "codecs": [
                            {
                                "name": "opus",
                                "type": "audio",
                                "priority": 1000,
                                "payload_type": 120
                            }
                        ]
                    },
                });
                voice_gateaway
                    .websocket
                    .send(Message::Text(protocol_select.to_string().into()))
                    .await?;
            }
            _ => {
                warn!("Unkown voice-opcode recived: {:?}", self.op);
            }
        }

        Ok(())
    }
}
