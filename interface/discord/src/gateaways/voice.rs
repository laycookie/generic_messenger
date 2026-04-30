use std::{error::Error, mem, sync::Arc};

use arc_swap::ArcSwapOption;
use facet::Facet;
use futures::channel::oneshot;
use futures::lock::Mutex as AsyncMutex;
use num_enum::TryFromPrimitive;
use simple_audio_channels::{input::SampleConsumer, output::SampleProducer};

use self::gateway::Voice;
use crate::gateaways::Gateaway;
use crate::{ChannelID, api_types::SNOWFLAKE};

pub(super) mod connection;
mod events;
mod gateway;
mod payloads;

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
#[derive(Debug, Facet, TryFromPrimitive)]
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
    Initilizing(oneshot::Receiver<SampleProducer>),
    Connected(SampleProducer),
}

#[derive(Default)]
pub enum InputChannel {
    #[default]
    None,
    Initilizing(oneshot::Receiver<SampleConsumer>),
    Connected(SampleConsumer),
}
