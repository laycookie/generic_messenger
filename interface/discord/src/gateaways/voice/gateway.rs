use std::{
    io,
    num::NonZeroU16,
    sync::{OnceLock, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwapOption;
use async_tungstenite::async_std::connect_async;
use dashmap::DashMap;
use davey::{DAVE_PROTOCOL_VERSION, DaveSession};
use facet_pretty::FacetPretty;
use futures::lock::Mutex as AsyncMutex;
use surf::http::convert::json;
use tracing::info;

use super::{
    Endpoint, SessionId, VoiceOpcode,
    connection::{Connection, Ssrc},
    payloads::HelloPayload,
};
use crate::api_types::SNOWFLAKE;
use crate::gateaways::{Gateaway, GateawayStream, HeartBeatingData, Websocket};

pub struct Voice {
    heartbeat_version: u8,
    pub channel_id: SNOWFLAKE,
    pub guild_id: Option<SNOWFLAKE>,
    pub dave_pending_transitions: DashMap<u16, NonZeroU16>, // transition_id, dave_protocol_version
    pub dave_session: AsyncMutex<Option<DaveSession>>,
    pub connection: ArcSwapOption<Connection>,
    pub ssrc_to_audio_channel: DashMap<Ssrc, std::sync::Mutex<super::AudioChannel>>,
    pub ssrc_to_user_id: DashMap<Ssrc, SNOWFLAKE>,
    pub is_speaking: AtomicBool,
}

impl Gateaway<Voice> {
    const VERSION: usize = 7; // TODO: Upgrade to 9
    pub async fn new(
        endpoint: &Endpoint,
        session_id: &SessionId,
        guild_id: Option<SNOWFLAKE>,
        channel_id: SNOWFLAKE,
        user_id: u64,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (mut voice_websocket, _) = connect_async(
            "wss://".to_string() + &endpoint.wss + "/?v=" + &Self::VERSION.to_string(),
        )
        .await?;

        // <https://discord.com/developers/docs/topics/voice-connections#establishing-a-voice-websocket-connection>
        let identify_payload = json!({
          "op": VoiceOpcode::Identify as u8,
          "d": {
            // The ID of the guild, private channel, stream, or lobby being connected to
            "server_id": guild_id,
            "channel_id": channel_id,
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

        let hello_d = facet_value::from_value::<HelloPayload>(hello_event.d)?;
        println!("{}", hello_d.pretty());
        let heart_beating_duration = Duration::from_millis(hello_d.heartbeat_interval);

        Ok(Self {
            websocket: Websocket::new(voice_websocket),
            heart_beating: HeartBeatingData::new(heart_beating_duration).into(),
            last_sequence_number: OnceLock::new(),
            type_specific_data: Voice {
                heartbeat_version: hello_d.v,
                channel_id,
                guild_id,
                dave_pending_transitions: DashMap::new(),
                dave_session: AsyncMutex::new(None),
                connection: Default::default(),
                ssrc_to_audio_channel: DashMap::new(),
                ssrc_to_user_id: Default::default(),
                is_speaking: false.into(),
            },
        })
    }
    /// https://docs.discord.food/topics/voice-connections#heartbeating
    pub async fn heartbeat(&self) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.heart_beating.lock().await.await_until_beat().await;

        let payload = match self.heartbeat_version {
            ver if 7 < ver => json!({
                    "op": VoiceOpcode::Heartbeat as u8,
                    "d": {
                        "t": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
                        "seq_ack": self.last_sequence_number.get()
                    },
            })
            .to_string(),
            // Everything beyond this point is kind of a guess and isn't tested
            1..=7 => json!({
                    "op": VoiceOpcode::Heartbeat as u8,
                    "d": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
            })
            .to_string(),
            _ => todo!(),
        };

        self.websocket.send(payload.into()).await
    }
}
