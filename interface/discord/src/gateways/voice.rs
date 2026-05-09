use std::{error::Error, io, mem, sync::Arc};

use arc_swap::ArcSwapOption;
use facet::Facet;
use futures::channel::oneshot;
use futures::lock::Mutex as AsyncMutex;
use num_enum::TryFromPrimitive;
use simple_audio_channels::output::SampleProducer;

use self::gateway::Voice;
use crate::gateways::Gateway;
use crate::{ChannelID, api_types::SNOWFLAKE};

pub(super) mod connection;
mod events;
mod gateway;
mod payloads;

/// Voice gateway protocol version.
pub(crate) const VOICE_GATEWAY_VERSION: usize = 9;

pub type SessionId = String;
pub struct Endpoint {
    pub wss: String,
    pub token: String,
}
impl Endpoint {
    pub fn new(wss: String, token: String) -> Self {
        Self { wss, token }
    }
}

/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#voice>
/// <https://docs.discord.food/topics/opcodes-and-status-codes#voice-opcodes>
#[repr(u8)]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Facet, TryFromPrimitive)]
#[facet(is_numeric)]
pub enum VoiceOpcode {
    Identify = 0,
    SelectProtocol = 1,
    Ready = 2,
    Heartbeat = 3,
    SessionDescription = 4,
    Speaking = 5,
    HeartbeatACK = 6,
    Resume = 7,
    Hello = 8,
    Resumed = 9,
    ClientConnect = 11,
    Video = 12,
    ClientDisconnect = 13,
    SessionUpdate = 14,
    MediaSinkWants = 15,
    VoiceBackendVersion = 16,
    ClientFlags = 18,
    SpeedTest = 19,
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

/// Parsed binary frame from Discord's voice gateway.
///
/// v7 wire format:  `[opcode: u8][payload: ...]`
/// v8+ wire format: `[sequence: u16 BE][opcode: u8][payload: ...]`
/// <https://docs.discord.food/topics/voice-connections#binary-websocket-messages>
pub struct VoiceBinaryFrame {
    pub(crate) sequence: Option<u16>,
    pub(crate) opcode: VoiceOpcode,
    pub(crate) payload: Vec<u8>,
}

impl VoiceBinaryFrame {
    pub fn parse(bytes: &[u8]) -> Result<Self, io::Error> {
        if VOICE_GATEWAY_VERSION > 7 {
            // v8+: [sequence: u16 BE][opcode: u8][payload: ...]
            if bytes.len() < 3 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "binary frame too short for v8+ format",
                ));
            }
            let sequence = u16::from_be_bytes([bytes[0], bytes[1]]);
            let opcode = VoiceOpcode::try_from(bytes[2]).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown voice opcode {}: {err}", bytes[2]),
                )
            })?;
            Ok(Self {
                sequence: Some(sequence),
                opcode,
                payload: bytes[3..].to_vec(),
            })
        } else {
            // v7: [opcode: u8][payload: ...]
            let (&opcode_byte, payload) = bytes
                .split_first()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty binary frame"))?;
            let opcode = VoiceOpcode::try_from(opcode_byte).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown voice opcode {opcode_byte}: {err}"),
                )
            })?;
            Ok(Self {
                sequence: None,
                opcode,
                payload: payload.to_vec(),
            })
        }
    }
}

#[derive(Default)]
pub enum VoiceGatewayStatus {
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
impl VoiceGatewayStatus {
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
                session_id,
                channel_id,
                ..
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
                channel_id,
                ..
            } => Self::Ready {
                endpoint,
                session_id,
                channel_id,
            },
        }
    }
}

#[derive(Default)]
pub struct VoiceGateway {
    status: AsyncMutex<VoiceGatewayStatus>,
    voice_gateway: ArcSwapOption<Gateway<Voice>>,
}
impl VoiceGateway {
    pub async fn initiate_connection(&self, channel_id: ChannelID) {
        let mut status = self.status.lock().await;
        *status = VoiceGatewayStatus::AwaitingData { channel_id };
    }
    pub async fn replace_status(&self, new_status: VoiceGatewayStatus) {
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
    pub fn full_load_gateway(&self) -> Option<Arc<Gateway<Voice>>> {
        self.voice_gateway.load_full()
    }
    pub async fn connect(&self, user_id: SNOWFLAKE) -> Result<(), Box<dyn Error + Send + Sync>> {
        let status = self.status.lock().await;

        let (endpoint, session_id, channel_id) = match &*status {
            VoiceGatewayStatus::Ready {
                endpoint,
                session_id,
                channel_id,
            } => (endpoint, session_id, channel_id),
            _ => {
                return Err(
                    io::Error::new(io::ErrorKind::NotConnected, "voice gateway not ready").into(),
                );
            }
        };

        let gateway = Gateway::<Voice>::new(
            endpoint,
            session_id,
            channel_id.guild_id,
            channel_id.id,
            user_id,
        )
        .await?;
        self.voice_gateway.store(Some(Arc::new(gateway)));

        Ok(())
    }

    pub async fn disconnect(&self) {
        let mut status = self.status.lock().await;
        *status = VoiceGatewayStatus::default();
        self.voice_gateway.store(None);
    }
}

pub enum AudioChannel {
    Initializing(oneshot::Receiver<SampleProducer>),
    Connected(SampleProducer),
}
