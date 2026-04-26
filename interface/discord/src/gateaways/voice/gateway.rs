use std::{
    io,
    num::NonZeroU16,
    sync::{
        OnceLock,
        atomic::AtomicBool,
    },
    time::Duration,
};

use arc_swap::ArcSwapOption;
use async_tungstenite::async_std::connect_async;
use dashmap::DashMap;
use davey::{DAVE_PROTOCOL_VERSION, DaveSession};
use futures::lock::Mutex as AsyncMutex;
use surf::http::convert::json;
use tracing::info;

use crate::api_types::SNOWFLAKE;
use crate::gateaways::{Gateaway, GateawayStream, HeartBeatingData, Websocket};
use super::{
    Endpoint, SessionId, VoiceOpcode,
    connection::{Connection, Ssrc},
    payloads::HelloPayload,
};

pub struct Voice {
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
    pub async fn new(
        endpoint: &Endpoint,
        session_id: &SessionId,
        guild_id: Option<SNOWFLAKE>,
        channel_id: SNOWFLAKE,
        user_id: u64,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
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
            websocket: Websocket::new(voice_websocket),
            heart_beating: HeartBeatingData::new(heart_beating_duration).into(),
            last_sequence_number: OnceLock::new(),
            type_specific_data: Voice {
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
    pub async fn heartbeat(&self) -> Result<(), async_tungstenite::tungstenite::Error> {
        self.heart_beating.lock().await.await_until_beat().await;
        self.websocket
            .send(
                json!({
                        "op": VoiceOpcode::Heartbeat as u8,
                        "d": self.last_sequence_number.get(),
                })
                .to_string()
                .into(),
            )
            .await
    }
}
