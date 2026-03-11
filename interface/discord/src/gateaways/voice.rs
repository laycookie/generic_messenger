use std::{
    collections::VecDeque,
    error::Error,
    io, mem,
    num::NonZeroU16,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use arc_swap::ArcSwapOption;
use async_tungstenite::{async_std::connect_async, tungstenite::Message};
use dashmap::DashMap;
use davey::{DAVE_PROTOCOL_VERSION, DaveSession};
use facet::Facet;
use futures::channel::oneshot;
use futures::lock::Mutex as AsyncMutex;
use messenger_interface::{
    interface::{CallStatus, VoiceEvent},
    stream::WeakSocketStream,
};
use num_enum::TryFromPrimitive;
use simple_audio_channels::{CHANNEL_BUFFER_SIZE, input::Input, output::Output};
use smol::net::UdpSocket;
use surf::http::convert::json;
use tracing::{error, info, warn};

use crate::{
    ChannelID, InnerDiscord,
    api_types::SNOWFLAKE,
    gateaways::{
        Gateaway, GateawayStream, GatewayPayload, HeartBeatingData, Websocket,
        voice::connection::{Connection, EncryptionMode, SessionDescription, Ssrc},
    },
};

pub(super) mod connection;

/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#voice>
/// <https://docs.discord.food/topics/opcodes-and-status-codes#voice-opcodes>
#[repr(u8)]
#[non_exhaustive]
#[derive(Debug, Facet, TryFromPrimitive)]
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
    DAVEProtocolPrepareTransition = 21,
    DAVEProtocolExecuteTransition = 22,
    DAVEProtocolTransitionReady = 23,
    DAVEProtocolPrepareEpoch = 24,
    MLSExternalSenderPackage = 25,
    MLSKeyPackage = 26,
    MLSProposals = 27,
    MLSCommitWelcome = 28,
    MLSAnnounceCommitTransition = 29,
    MLSWelcome = 30,
    MLSInvalidCommitWelcome = 31,
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

/// <https://docs.discord.food/topics/voice-connections#speaking-structure>
#[derive(Facet)]
struct SpeakingPayload {
    speaking: bool, // Should be u8
    ssrc: Ssrc,
    user_id: SNOWFLAKE, // Only sent by the voice server.
    delay: Option<u32>, // Not sent by the voice server.
}

#[derive(Facet)]
struct DAVEPrepareEpoch {
    transition_id: u16,
    epoch: u64,
    protocol_version: u16,
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
pub enum VoiceGateawayStatus {
    #[default]
    Closed,
    AwaitingData {
        channel_id: ChannelID,
    },
    AwaitingEndpoint {
        channel_id: ChannelID,
        session_id: SessionId,
    },
    AwaitingSession {
        channel_id: ChannelID,
        endpoint: Endpoint,
    },
    Ready {
        channel_id: ChannelID,
        endpoint: Endpoint,
        session_id: SessionId,
    },
}
impl VoiceGateawayStatus {
    pub fn insert_endpoint(&mut self, endpoint: Endpoint) {
        *self = match mem::take(self) {
            Self::Closed => Self::Closed,
            Self::AwaitingData { channel_id } => Self::AwaitingSession {
                endpoint,
                channel_id,
            },
            Self::AwaitingEndpoint {
                channel_id,
                session_id,
            } => Self::Ready {
                endpoint,
                session_id,
                channel_id,
            },
            Self::AwaitingSession { channel_id, .. } => Self::AwaitingSession {
                endpoint,
                channel_id,
            },
            Self::Ready {
                endpoint,
                session_id,
                channel_id,
            } => Self::Ready {
                endpoint,
                session_id,
                channel_id,
            },
        }
    }
    pub fn insert_session_id(&mut self, session_id: SessionId) {
        *self = match mem::take(self) {
            Self::Closed => Self::Closed,
            Self::AwaitingData { channel_id } => Self::AwaitingEndpoint {
                session_id,
                channel_id,
            },
            Self::AwaitingEndpoint {
                channel_id,
                session_id,
            } => Self::AwaitingEndpoint {
                channel_id,
                session_id,
            },
            Self::AwaitingSession {
                channel_id,
                endpoint,
            } => Self::Ready {
                endpoint,
                session_id,
                channel_id,
            },
            Self::Ready {
                endpoint,
                session_id,
                channel_id,
            } => Self::Ready {
                endpoint,
                session_id,
                channel_id,
            },
        }
    }
}

#[derive(Default)]
pub struct VoiceGateaway {
    status: AsyncMutex<VoiceGateawayStatus>,
    voice_gateaway: ArcSwapOption<Gateaway<Voice>>,
}
impl VoiceGateaway {
    pub async fn initiate_connection(&self, channel_id: ChannelID) {
        let mut status = self.status.lock().await;
        *status = VoiceGateawayStatus::AwaitingData { channel_id };
    }
    pub async fn replace_status(&self, new_status: VoiceGateawayStatus) {
        let mut status = self.status.lock().await;
        *status = new_status;
    }
    pub async fn insert_endpoint(&self, endpoint: Endpoint) {
        let mut status = self.status.lock().await;
        status.insert_endpoint(endpoint);
    }
    pub async fn insert_session_id(&self, session_id: SessionId) {
        let mut status = self.status.lock().await;
        status.insert_session_id(session_id);
    }
    pub fn full_load_gateaway(&self) -> Option<Arc<Gateaway<Voice>>> {
        self.voice_gateaway.load_full()
    }
    pub async fn connect(&self, user_id: SNOWFLAKE) -> Result<(), Box<dyn Error + Send + Sync>> {
        let status = self.status.lock().await;

        let (endpoint, session_id, channel_id) = match &*status {
            VoiceGateawayStatus::Ready {
                endpoint,
                session_id,
                channel_id,
            } => (endpoint, session_id, channel_id),
            _ => {
                return Err("Does not have enough info about the peer to connect".into());
            }
        };

        let gateaway = Gateaway::<Voice>::new(
            endpoint,
            session_id,
            channel_id.guild_id,
            channel_id.id,
            user_id,
        )
        .await?;
        self.voice_gateaway.store(Some(Arc::new(gateaway)));

        Ok(())
    }

    pub async fn disconnect(&self) {
        let mut status = self.status.lock().await;
        *status = VoiceGateawayStatus::default();
        self.voice_gateaway.store(None);
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
    pub channel_id: SNOWFLAKE,
    pub guild_id: Option<SNOWFLAKE>,
    pub dave_pending_transitions: DashMap<u16, NonZeroU16>, // transition_id, dave_protocol_version
    pub dave_session: AsyncMutex<Option<DaveSession>>,
    pub connection: AsyncMutex<Option<Connection>>,
    pub ssrc_to_audio_channel: DashMap<Ssrc, AsyncMutex<AudioChannel>>,
    pub ssrc_to_user_id: DashMap<Ssrc, SNOWFLAKE>,
    pub input_channel: AsyncMutex<InputChannel>,
    pub input_buffer: AsyncMutex<VecDeque<f32>>,
    pub is_speaking: AtomicBool,
}
impl Gateaway<Voice> {
    pub async fn new(
        endpoint: &Endpoint,
        session_id: &SessionId,
        guild_id: Option<SNOWFLAKE>,
        channel_id: SNOWFLAKE,
        user_id: u64,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let (mut voice_websocket, _) = connect_async("wss://".to_string() + &endpoint.wss).await?;

        // <https://discord.com/developers/docs/topics/voice-connections#establishing-a-voice-websocket-connection>
        let identify_payload = json!({
          "op": VoiceOpcode::Identify as u8,
          "d": {
            // The ID of the guild, private channel, stream, or lobby being connected to
            "server_id": guild_id, // TODO
            "channel_id": channel_id, // TODO
            "user_id": user_id,
            "session_id": session_id,
            "token": endpoint.token,
            "max_dave_protocol_version": DAVE_PROTOCOL_VERSION,
          }
        });
        info!("{identify_payload:#?}");
        voice_websocket
            .send(identify_payload.to_string().into())
            .await
            .unwrap();

        let hello_event = voice_websocket.next_gateaway_payload().await;

        let VoiceOpcode::Hello = hello_event.op else {
            return Err(io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Expected to recive hello event as the first event",
            )
            .into());
        };

        let hello_d = facet_value::from_value::<HelloPayload>(hello_event.d).unwrap();
        let heart_beating_duration = Duration::from_millis(hello_d.heartbeat_interval);

        Ok(Self {
            websocket: crate::gateaways::Websocket::new(voice_websocket),
            heart_beating: HeartBeatingData::new(heart_beating_duration).into(),
            last_sequence_number: OnceLock::new(),
            type_specific_data: Voice {
                channel_id,
                guild_id,
                dave_pending_transitions: DashMap::new(),
                dave_session: AsyncMutex::new(None),
                connection: AsyncMutex::new(None),
                ssrc_to_audio_channel: DashMap::new(),
                ssrc_to_user_id: Default::default(),
                input_channel: Default::default(),
                input_buffer: Default::default(),
                is_speaking: false.into(),
            },
        })
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
    pub async fn exec<T>(self, discord: &Arc<InnerDiscord<T>>) -> Result<(), Box<dyn Error>> {
        let gateaway = discord.gateaway.load();
        let Some(gateaway) = gateaway.as_ref() else {
            return Err("TODO".into());
        };
        let Some(voice_gateaway) = gateaway.voice.full_load_gateaway() else {
            return Err("TODO".into());
        };

        if let Some(s) = self.s {
            voice_gateaway
                .last_sequence_number
                .get_or_init(|| s.into())
                .store(s, Ordering::Relaxed);
        };

        info!("VoiceOpcode: {:?}", self.op);
        match self.op {
            VoiceOpcode::SessionDescription => {
                let session_description = facet_value::from_value::<SessionDescription>(self.d)?;

                // Init DAVE
                let mut dave_session = voice_gateaway.dave_session.lock().await;
                let profile = discord.profile.read().await;
                let profile = profile.as_ref().unwrap();
                reinit_dave_session(
                    &voice_gateaway.websocket,
                    &mut dave_session,
                    session_description.dave_protocol_version(),
                    voice_gateaway.channel_id,
                    profile.id,
                )
                .await;

                // Commit description to connection
                if let Some(connection) = voice_gateaway.connection.lock().await.as_mut() {
                    connection.set_description(session_description).unwrap();
                };

                discord
                    .voice_events
                    .push(VoiceEvent::CallStatusUpdate(CallStatus::Connected(
                        WeakSocketStream::new(discord.clone().audio().await),
                    )));
            }
            VoiceOpcode::Speaking => {
                let speaking = facet_value::from_value::<SpeakingPayload>(self.d).unwrap();

                voice_gateaway
                    .ssrc_to_user_id
                    .insert(speaking.ssrc, speaking.user_id);
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
                {
                    let mut connection = voice_gateaway.connection.lock().await;
                    *connection = Some(Connection::new(udp, recv_ip_discovery.ssrc.get()));
                }
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
            VoiceOpcode::DAVEProtocolPrepareTransition => {
                let mut dave_session = voice_gateaway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => unreachable!(),
                };

                let packet = facet_value::from_value::<DAVEPrepareEpoch>(self.d).unwrap();

                let transition_id = packet.transition_id;

                voice_gateaway
                    .dave_pending_transitions
                    .insert(transition_id, dave_session.protocol_version());

                if transition_id == 0 {
                    execute_pending_transition(
                        dave_session,
                        &voice_gateaway.dave_pending_transitions,
                        transition_id,
                    );
                } else {
                    // TODO
                    // Upon receiving this message, clients enable passthrough mode on their receive-side
                    // https://daveprotocol.com/#downgrade-to-transport-only-encryption
                }
            }
            VoiceOpcode::DAVEProtocolExecuteTransition => {
                let mut dave_session = voice_gateaway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => unreachable!(),
                };

                let packet = facet_value::from_value::<DAVEPrepareEpoch>(self.d).unwrap();
                let transition_id = packet.transition_id;
                execute_pending_transition(
                    dave_session,
                    &voice_gateaway.dave_pending_transitions,
                    transition_id,
                );
            }
            VoiceOpcode::DAVEProtocolPrepareEpoch => {
                let packet = facet_value::from_value::<DAVEPrepareEpoch>(self.d).unwrap();

                if packet.epoch == 1 {
                    let mut dave_session = voice_gateaway.dave_session.lock().await;
                    // TODO: Investigate if this should be properly added
                    // this.daveProtocolVersion = packet.protocol_version;
                    let profile = discord.profile.read().await;
                    let profile = profile.as_ref().unwrap();
                    reinit_dave_session(
                        &voice_gateaway.websocket,
                        &mut dave_session,
                        packet.protocol_version,
                        voice_gateaway.channel_id,
                        profile.id,
                    )
                    .await;
                }
            }
            VoiceOpcode::MLSExternalSenderPackage => {
                let mut dave_session = voice_gateaway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => unreachable!(),
                };

                let bytes = facet_value::from_value::<Vec<u8>>(self.d)?;
                if let Err(err) = dave_session.set_external_sender(&bytes[1..]) {
                    error!("{err}");
                    return Err(err.into());
                };
            }
            VoiceOpcode::MLSProposals => {
                let mut dave_session = voice_gateaway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => unreachable!(),
                };
                let bytes = facet_value::from_value::<Vec<u8>>(self.d)?;

                let optype = if bytes[1] == 0 {
                    davey::ProposalsOperationType::APPEND
                } else {
                    davey::ProposalsOperationType::REVOKE
                };
                let commit_welcome = match dave_session.process_proposals(
                    optype,
                    &bytes[2..],
                    // TODO: Add this for security purposes, should be recived from CLIENTS_CONNECT
                    None,
                ) {
                    Ok(welcome_message) => welcome_message,
                    Err(err) => {
                        error!("{err:?}");
                        return Err(err.into());
                    }
                };

                if let Some(commit_welcome) = commit_welcome {
                    match commit_welcome.welcome {
                        Some(welcome) => {
                            voice_gateaway
                                .websocket
                                .send_binary(
                                    VoiceOpcode::MLSCommitWelcome as u8,
                                    welcome.into_iter().chain(commit_welcome.commit.into_iter()),
                                )
                                .await?
                        }
                        None => {
                            voice_gateaway
                                .websocket
                                .send_binary(
                                    VoiceOpcode::MLSCommitWelcome as u8,
                                    commit_welcome.commit.into_iter(),
                                )
                                .await?
                        }
                    }
                } else {
                    error!("Potentially a problem?");
                }
            }
            VoiceOpcode::MLSAnnounceCommitTransition => {
                let mut dave_session = voice_gateaway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => unreachable!(),
                };
                let bytes = facet_value::from_value::<Vec<u8>>(self.d)?;

                let transition_id = u16::from_be_bytes(bytes[1..3].try_into().unwrap());
                if let Err(err) = dave_session.process_commit(&bytes[3..]) {
                    error!("{err:?}");
                    voice_gateaway
                        .websocket
                        .send(Message::Text(
                            json!({
                                "op": VoiceOpcode::MLSInvalidCommitWelcome as u8,
                                "d": {
                                  "transition_id": transition_id
                                }
                            })
                            .to_string()
                            .into(),
                        ))
                        .await?
                } else {
                    if transition_id != 0 {
                        voice_gateaway
                            .dave_pending_transitions
                            .insert(transition_id, dave_session.protocol_version());
                        //TODO
                        voice_gateaway
                            .websocket
                            .send(Message::Text(
                                json!({
                                    "op": VoiceOpcode::DAVEProtocolTransitionReady as u8,
                                    "d": {
                                      "transition_id": transition_id
                                    }
                                })
                                .to_string()
                                .into(),
                            ))
                            .await?
                    }
                }
            }
            VoiceOpcode::MLSWelcome => {
                let mut dave_session = voice_gateaway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => unreachable!(),
                };
                let bytes = facet_value::from_value::<Vec<u8>>(self.d)?;

                let transition_id = u16::from_be_bytes(bytes[1..3].try_into().unwrap());
                if let Err(err) = dave_session.process_welcome(&bytes[3..]) {
                    error!("{err:?}");
                    voice_gateaway
                        .websocket
                        .send(Message::Text(
                            json!({
                                "op": VoiceOpcode::MLSInvalidCommitWelcome as u8,
                                "d": {
                                  "transition_id": transition_id
                                }
                            })
                            .to_string()
                            .into(),
                        ))
                        .await?
                } else {
                    info!("{:?}", dave_session.get_user_ids());
                    if transition_id != 0 {
                        voice_gateaway
                            .dave_pending_transitions
                            .insert(transition_id, dave_session.protocol_version());
                        //TODO
                        voice_gateaway
                            .websocket
                            .send(Message::Text(
                                json!({
                                    "op": VoiceOpcode::DAVEProtocolTransitionReady as u8,
                                    "d": {
                                      "transition_id": transition_id
                                    }
                                })
                                .to_string()
                                .into(),
                            ))
                            .await?
                    }
                }
            }
            _ => {
                warn!("Unkown voice-opcode recived: {:?}", self.op);
            }
        }

        Ok(())
    }
}

// TODO: Impl for Gateaway<Voice>
// TODO Move with teh DAVE RELATED stuff
fn execute_pending_transition(
    dave_session: &mut DaveSession,
    dave_pending_transitions: &DashMap<u16, NonZeroU16>,
    transition_id: u16,
) {
    let Some((_, new_version)) = dave_pending_transitions.remove(&transition_id) else {
        warn!(
            "Received execute transition, but we don't have a pending transition for {transition_id}"
        );
        return;
    };

    let old_version = dave_session.protocol_version();
    if old_version != new_version {
        error!("Downgrade or upgrade");
    }
}

async fn reinit_dave_session(
    voice_websocket: &Websocket,
    dave_session: &mut Option<DaveSession>,
    dave_protocol_version: u16,
    channel_id: SNOWFLAKE,
    user_id: SNOWFLAKE,
) {
    if let Some(dave_ver) = NonZeroU16::new(dave_protocol_version) {
        let key_package = if let Some(dave_session) = dave_session {
            dave_session
                .reinit(dave_ver, user_id, channel_id, None)
                .unwrap();
            dave_session.create_key_package()
        } else {
            let mut new_dave_session =
                DaveSession::new(dave_ver, user_id, channel_id, None).unwrap();
            let key_package = new_dave_session.create_key_package();
            *dave_session = Some(new_dave_session);
            key_package
        };

        voice_websocket
            .send_binary(
                VoiceOpcode::MLSKeyPackage as u8,
                key_package.unwrap().into_iter(),
            )
            .await
            .unwrap();
    } else {
        error!("AAAAAAAAAAaa problem for a future me, just became a problem for a current me.");
    };
}
