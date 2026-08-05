//! Discord Voice Gateway JSON and binary payloads.
//!
//! <https://docs.discord.food/topics/voice-connections>

use std::{collections::HashMap, io};

use facet::Facet;

use super::super::connection::{EncryptionMode, Ssrc};
use crate::api::SNOWFLAKE;

/// Opcode 15 (MediaSinkWants), keyed by media SSRC or `any`.
///
/// `pixelCounts` shares top-level object with quality entries, so conversion
/// adapts keys and values at raw serialization boundary.
/// <https://docs.discord.food/topics/voice-connections#media-sink-wants-structure>
#[derive(Debug, Facet)]
#[facet(proxy = facet_value::Value)]
pub(crate) struct MediaSinkWantsPayload {
    pub(crate) qualities: HashMap<Ssrc, u8>,
    pub(crate) any: Option<u8>,
    pub(crate) pixel_counts: HashMap<Ssrc, f64>,
}

impl TryFrom<facet_value::Value> for MediaSinkWantsPayload {
    type Error = String;

    fn try_from(value: facet_value::Value) -> Result<Self, Self::Error> {
        let mut any = None;
        let mut qualities = HashMap::new();
        let mut pixel_counts = HashMap::new();

        for (key, value) in facet_value::from_value::<HashMap<String, facet_value::Value>>(value)
            .map_err(|err| err.to_string())?
            .into_iter()
        {
            match key.as_str() {
                "any" => any = Some(facet_value::from_value(value).map_err(|err| err.to_string())?),
                "pixelCounts" => {
                    pixel_counts = facet_value::from_value::<HashMap<String, f64>>(value)
                        .map_err(|err| err.to_string())?
                        .into_iter()
                        .map(|(ssrc, pixels)| {
                            ssrc.parse::<Ssrc>()
                                .map(|ssrc| (ssrc, pixels))
                                .map_err(|err| err.to_string())
                        })
                        .collect::<Result<_, _>>()?;
                }
                ssrc => {
                    let ssrc = ssrc
                        .parse()
                        .map_err(|err: std::num::ParseIntError| err.to_string())?;
                    let quality = facet_value::from_value(value).map_err(|err| err.to_string())?;
                    qualities.insert(ssrc, quality);
                }
            }
        }

        Ok(Self {
            qualities,
            any,
            pixel_counts,
        })
    }
}

impl From<&MediaSinkWantsPayload> for facet_value::Value {
    fn from(payload: &MediaSinkWantsPayload) -> Self {
        let mut raw: HashMap<String, facet_value::Value> = payload
            .qualities
            .iter()
            .map(|(ssrc, quality)| (ssrc.to_string(), facet_value::to_value(quality).unwrap()))
            .collect();
        if let Some(quality) = payload.any {
            raw.insert("any".to_owned(), facet_value::to_value(&quality).unwrap());
        }
        if !payload.pixel_counts.is_empty() {
            raw.insert(
                "pixelCounts".to_owned(),
                facet_value::to_value(
                    &payload
                        .pixel_counts
                        .iter()
                        .map(|(ssrc, pixels)| (ssrc.to_string(), *pixels))
                        .collect::<HashMap<_, _>>(),
                )
                .unwrap(),
            );
        }
        facet_value::to_value(&raw).unwrap()
    }
}

impl MediaSinkWantsPayload {
    pub(crate) fn quality_for(&self, media_ssrc: Option<Ssrc>) -> Option<u8> {
        media_ssrc
            .and_then(|ssrc| self.qualities.get(&ssrc))
            .or(self.any.as_ref())
            .map(|quality| (*quality).min(100))
    }
}

/// <https://docs.discord.food/topics/voice-connections#hello-structure>
#[derive(Facet)]
pub(crate) struct HelloPayload {
    pub(crate) v: u8,
    pub(crate) heartbeat_interval: u64,
}

/// Server confirmation for Voice Gateway opcode 14 Session Update.
///
/// Discord may return only fields changed by the client, so every property is
/// optional. `video_codec` is authoritative before outbound encoder switches.
/// <https://docs.discord.food/topics/voice-connections#session-update-structure>
#[derive(Facet)]
pub(crate) struct SessionUpdateEvent {
    pub(crate) audio_codec: Option<String>,
    pub(crate) video_codec: Option<String>,
    pub(crate) media_session_id: Option<String>,
    pub(crate) keyframe_interval: Option<u32>,
}

/// <https://docs.discord.food/topics/voice-connections#ready-structure>
#[derive(Facet)]
pub(crate) struct ReadyPayload {
    pub(crate) ssrc: Ssrc,
    pub(crate) ip: String,
    pub(crate) port: u16,
    pub(crate) modes: Vec<EncryptionMode>,
    pub(crate) experiments: Vec<String>,
    /// Send SSRCs the server assigned for the simulcast streams we offered in
    /// Identify. Absent on servers that don't echo them — we derive
    /// `audio_ssrc + 1/+2` as a fallback (see the Ready handler).
    pub(crate) streams: Option<Vec<VideoStreamPayload>>,
}

/// One Discord video `streams` entry, shared by Ready and opcode 12 Video.
///
/// Fields are optional because their presence depends on direction and
/// negotiation state. For example, Identify has no assigned SSRCs, while a
/// received Video entry may omit `type`.
/// <https://docs.discord.food/topics/voice-connections#stream-structure>
#[derive(Facet)]
pub(crate) struct VideoStreamPayload {
    #[facet(rename = "type")]
    pub(crate) stream_type: Option<String>,
    pub(crate) rid: Option<String>,
    pub(crate) quality: Option<u8>,
    pub(crate) active: Option<bool>,
    pub(crate) max_bitrate: Option<u32>,
    pub(crate) max_framerate: Option<u32>,
    pub(crate) max_resolution: Option<VideoStreamResolutionPayload>,
    pub(crate) ssrc: Option<Ssrc>,
    pub(crate) rtx_ssrc: Option<Ssrc>,
}

/// Discord video stream resolution constraint.
/// <https://docs.discord.food/topics/voice-connections#stream-resolution-structure>
#[derive(Facet)]
pub(crate) struct VideoStreamResolutionPayload {
    #[facet(rename = "type")]
    pub(crate) resolution_type: Option<String>,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
}

/// <https://docs.discord.food/topics/voice-connections#speaking-structure>
#[derive(Facet)]
pub(crate) struct SpeakingPayload {
    pub(crate) speaking: u8,
    pub(crate) ssrc: Ssrc,
    pub(crate) user_id: SNOWFLAKE, // Only sent by the voice server.
    pub(crate) delay: Option<u32>, // Not sent by the voice server.
}

/// Complete opcode 12 Video wire shape.
///
/// `user_id` exists only server→client; top-level `rtx_ssrc` exists only
/// client→server. Other fields remain optional for compatibility with
/// audio-only and older server announcements. `streams`, when present, is the
/// authoritative list of every simulcast layer and its primary/RTX SSRCs.
/// <https://docs.discord.food/topics/voice-connections#video>
#[derive(Facet)]
pub(crate) struct VideoPayload {
    pub(crate) audio_ssrc: Option<Ssrc>,
    pub(crate) video_ssrc: Option<Ssrc>,
    pub(crate) rtx_ssrc: Option<Ssrc>,
    pub(crate) streams: Option<Vec<VideoStreamPayload>>,
    pub(crate) user_id: Option<SNOWFLAKE>,
}

/// Opcode 13 (ClientDisconnect): a user fully left the call; every SSRC they
/// owned is dead.
/// <https://docs.discord.food/topics/voice-connections#client-disconnect-structure>
#[derive(Facet)]
pub(crate) struct ClientDisconnectPayload {
    pub(crate) user_id: SNOWFLAKE,
}

/// <https://docs.discord.food/topics/voice-connections#end-to-end-encryption>
#[derive(Facet)]
pub(crate) struct DAVEPrepareEpoch {
    pub(crate) transition_id: u16,
    pub(crate) epoch: u64,
    pub(crate) protocol_version: u16,
}

/// Binary payload for MLSProposals (opcode 27).
///
/// Wire format (opcode byte already stripped):
/// `[operation_type: u8][proposals_data: ...]`
///
/// <https://docs.discord.food/topics/voice-connections#end-to-end-encryption>
pub(crate) struct MlsProposalsPayload<'a> {
    pub(crate) operation_type: davey::ProposalsOperationType,
    pub(crate) data: &'a [u8],
}

impl<'a> TryFrom<&'a [u8]> for MlsProposalsPayload<'a> {
    type Error = io::Error;
    fn try_from(bytes: &'a [u8]) -> Result<Self, Self::Error> {
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MLSProposals payload is empty",
            ));
        }
        let operation_type = if bytes[0] == 0 {
            davey::ProposalsOperationType::APPEND
        } else {
            davey::ProposalsOperationType::REVOKE
        };
        Ok(Self {
            operation_type,
            data: &bytes[1..],
        })
    }
}

/// Binary payload with a transition ID prefix — used by
/// MLSAnnounceCommitTransition (opcode 29) and MLSWelcome (opcode 30).
///
/// Wire format (opcode byte already stripped):
/// `[transition_id: u16 BE][data: ...]`
///
/// <https://docs.discord.food/topics/voice-connections#end-to-end-encryption>
pub(crate) struct MlsTransitionPayload<'a> {
    pub(crate) transition_id: u16,
    pub(crate) data: &'a [u8],
}

impl<'a> TryFrom<&'a [u8]> for MlsTransitionPayload<'a> {
    type Error = io::Error;
    fn try_from(bytes: &'a [u8]) -> Result<Self, Self::Error> {
        if bytes.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MLS transition payload too short for transition_id",
            ));
        }
        let transition_id = u16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Self {
            transition_id,
            data: &bytes[2..],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_sink_wants_prefers_exact_ssrc_and_clamps() {
        let value = facet_json::from_str::<facet_value::Value>(
            r#"{"4242":37,"any":150,"pixelCounts":{"4242":921600.5}}"#,
        )
        .unwrap();
        let payload = facet_value::from_value::<MediaSinkWantsPayload>(value).unwrap();

        assert_eq!(payload.quality_for(Some(4242)), Some(37));
        assert_eq!(payload.quality_for(Some(7)), Some(100));
        assert_eq!(payload.quality_for(None), Some(100));
        assert_eq!(payload.pixel_counts.get(&4242), Some(&921600.5));

        let round_trip = facet_value::from_value::<MediaSinkWantsPayload>(
            facet_value::to_value(&payload).unwrap(),
        )
        .unwrap();
        assert_eq!(round_trip.quality_for(Some(4242)), Some(37));
        assert_eq!(round_trip.pixel_counts.get(&4242), Some(&921600.5));
    }

    #[test]
    fn media_sink_wants_ignores_metadata_only_payload() {
        let value =
            facet_json::from_str::<facet_value::Value>(r#"{"pixelCounts":{"4242":921600}}"#)
                .unwrap();
        let payload = facet_value::from_value::<MediaSinkWantsPayload>(value).unwrap();

        assert_eq!(payload.quality_for(Some(4242)), None);
    }

    #[test]
    fn video_payload_preserves_documented_stream_shape() {
        let value = facet_json::from_str::<facet_value::Value>(
            r#"{
                "user_id": 852892297661906993,
                "audio_ssrc": 13959,
                "video_ssrc": 13960,
                "streams": [{
                    "type": "video",
                    "rid": "100",
                    "ssrc": 13960,
                    "rtx_ssrc": 13961,
                    "active": true,
                    "quality": 100,
                    "max_bitrate": 9000000,
                    "max_framerate": 60,
                    "max_resolution": {
                        "type": "source",
                        "width": 0,
                        "height": 0
                    }
                }]
            }"#,
        )
        .unwrap();
        let payload = facet_value::from_value::<VideoPayload>(value).unwrap();
        let stream = &payload.streams.as_ref().unwrap()[0];
        let resolution = stream.max_resolution.as_ref().unwrap();

        assert_eq!(payload.user_id, Some(852892297661906993));
        assert_eq!(payload.audio_ssrc, Some(13959));
        assert_eq!(payload.video_ssrc, Some(13960));
        assert_eq!(payload.rtx_ssrc, None);
        assert_eq!(stream.stream_type.as_deref(), Some("video"));
        assert_eq!(stream.rid.as_deref(), Some("100"));
        assert_eq!(stream.ssrc, Some(13960));
        assert_eq!(stream.rtx_ssrc, Some(13961));
        assert_eq!(stream.active, Some(true));
        assert_eq!(stream.quality, Some(100));
        assert_eq!(stream.max_bitrate, Some(9_000_000));
        assert_eq!(stream.max_framerate, Some(60));
        assert_eq!(resolution.resolution_type.as_deref(), Some("source"));
        assert_eq!(resolution.width, Some(0));
        assert_eq!(resolution.height, Some(0));
    }

    #[test]
    fn session_update_accepts_codec_only_confirmation() {
        let payload =
            facet_json::from_str::<SessionUpdateEvent>(r#"{"video_codec":"AV1"}"#).unwrap();

        assert_eq!(payload.video_codec.as_deref(), Some("AV1"));
        assert_eq!(payload.audio_codec, None);
        assert_eq!(payload.media_session_id, None);
        assert_eq!(payload.keyframe_interval, None);
    }
}
