use facet::Facet;
use uuid::Uuid;

use super::super::{
    DiscordAudioCodec, DiscordVideoCodec, VoiceOpcode,
    connection::rtp::{
        AV1_PAYLOAD_TYPE, AV1_RTX_PAYLOAD_TYPE, H264_PAYLOAD_TYPE, H264_RTX_PAYLOAD_TYPE,
        OPUS_PAYLOAD_TYPE,
    },
};

const PREFERRED_VIDEO_PRIORITY: u32 = 1000;
const FALLBACK_VIDEO_PRIORITY: u32 = 2000;

/// Voice transport protocol selected in Select Protocol.
///
/// Discord may add protocols later. Unknown values must fail decoding instead
/// of silently selecting an incompatible transport.
/// <https://docs.discord.food/topics/voice-connections#protocol-type>
#[derive(Clone, Copy, Facet)]
#[non_exhaustive]
#[repr(u8)]
pub(crate) enum ProtocolType {
    #[facet(rename = "udp")]
    Udp,
    #[facet(rename = "webrtc")]
    WebRtc,
    #[facet(rename = "webrtc-p2p")]
    WebRtcP2p,
}

/// Build codec capabilities for Select Protocol and Session Update.
///
/// Send-capable connections advertise both encoders so a preference change can
/// be negotiated without rebuilding UDP transport. Receive-only Go Live links
/// omit AV1 because this client does not decode it.
pub(crate) fn supported_codecs(
    preferred: DiscordVideoCodec,
    can_encode_video: bool,
) -> Vec<SelectProtocolCodec> {
    let mut codecs = vec![SelectProtocolCodec::audio(
        DiscordAudioCodec::Opus,
        PREFERRED_VIDEO_PRIORITY,
        None,
        None,
    )];

    if !can_encode_video {
        codecs.push(SelectProtocolCodec::video(
            DiscordVideoCodec::H264,
            PREFERRED_VIDEO_PRIORITY,
            Some(false),
            Some(true),
        ));
        return codecs;
    }

    let fallback = match preferred {
        DiscordVideoCodec::Av1 => DiscordVideoCodec::H264,
        DiscordVideoCodec::H264 => DiscordVideoCodec::Av1,
    };
    for (codec, priority) in [
        (preferred, PREFERRED_VIDEO_PRIORITY),
        (fallback, FALLBACK_VIDEO_PRIORITY),
    ] {
        codecs.push(SelectProtocolCodec::video(
            codec,
            priority,
            Some(true),
            Some(matches!(codec, DiscordVideoCodec::H264)),
        ));
    }
    codecs
}

/// Media category for a supported Voice Gateway codec.
/// <https://docs.discord.food/topics/voice-connections#supported-codecs>
#[derive(Clone, Copy, Debug, Eq, Facet, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub(crate) enum CodecType {
    #[facet(rename = "audio")]
    Audio,
    #[facet(rename = "video")]
    Video,
}

/// Voice Gateway opcode 1 Select Protocol payload.
/// <https://docs.discord.food/topics/voice-connections#select-protocol-structure>
#[derive(Facet)]
pub(crate) struct SelectProtocolPayload {
    pub(crate) op: VoiceOpcode,
    pub(crate) d: SelectProtocolData,
}

#[derive(Facet)]
pub(crate) struct SelectProtocolData {
    pub(crate) protocol: ProtocolType,
    pub(crate) data: SelectProtocolConnection,
    pub(crate) codecs: Vec<SelectProtocolCodec>,
    pub(crate) rtc_connection_id: String,
    pub(crate) experiments: Vec<String>,
}

impl SelectProtocolPayload {
    /// Build one UDP protocol selection for a fresh voice transport.
    ///
    /// Discord uses the connection UUID only for analytics. Generate it with
    /// the protocol selection so reconnects receive a new identity while a
    /// resumed transport keeps its existing identity and sends no opcode 1.
    pub(crate) fn udp(
        data: SelectProtocolConnection,
        codecs: Vec<SelectProtocolCodec>,
        experiments: Vec<String>,
    ) -> Self {
        Self {
            op: VoiceOpcode::SelectProtocol,
            d: SelectProtocolData {
                protocol: ProtocolType::Udp,
                data,
                codecs,
                rtc_connection_id: Uuid::new_v4().to_string(),
                experiments,
            },
        }
    }
}

#[derive(Facet)]
pub(crate) struct SelectProtocolConnection {
    pub(crate) address: String,
    pub(crate) port: u16,
    pub(crate) mode: String,
}

#[derive(Facet)]
pub(crate) struct SelectProtocolCodec {
    name: String,
    #[facet(rename = "type")]
    codec_type: CodecType,
    priority: u32,
    payload_type: u8,
    rtx_payload_type: Option<u8>,
    encode: Option<bool>,
    decode: Option<bool>,
}

impl SelectProtocolCodec {
    fn new(
        name: &'static str,
        codec_type: CodecType,
        priority: u32,
        payload_type: u8,
        rtx_payload_type: Option<u8>,
        encode: Option<bool>,
        decode: Option<bool>,
    ) -> Self {
        Self {
            name: name.to_owned(),
            codec_type,
            priority,
            payload_type,
            rtx_payload_type,
            encode,
            decode,
        }
    }

    fn audio(
        codec: DiscordAudioCodec,
        priority: u32,
        encode: Option<bool>,
        decode: Option<bool>,
    ) -> Self {
        Self::new(
            codec.wire_name(),
            CodecType::Audio,
            priority,
            OPUS_PAYLOAD_TYPE,
            None,
            encode,
            decode,
        )
    }

    fn video(
        codec: DiscordVideoCodec,
        priority: u32,
        encode: Option<bool>,
        decode: Option<bool>,
    ) -> Self {
        let (payload_type, rtx_payload_type) = match codec {
            DiscordVideoCodec::Av1 => (AV1_PAYLOAD_TYPE, Some(AV1_RTX_PAYLOAD_TYPE)),
            DiscordVideoCodec::H264 => (H264_PAYLOAD_TYPE, Some(H264_RTX_PAYLOAD_TYPE)),
        };
        Self::new(
            codec.wire_name(),
            CodecType::Video,
            priority,
            payload_type,
            rtx_payload_type,
            encode,
            decode,
        )
    }
}

/// Voice Gateway opcode 14 Session Update payload.
/// <https://docs.discord.food/topics/voice-connections#session-update-structure>
#[derive(Facet)]
pub(crate) struct SessionUpdatePayload {
    pub(crate) op: VoiceOpcode,
    pub(crate) d: SessionUpdateData,
}

#[derive(Facet)]
pub(crate) struct SessionUpdateData {
    pub(crate) codecs: Vec<SelectProtocolCodec>,
}

impl SessionUpdatePayload {
    pub(crate) fn new(preferred: DiscordVideoCodec, can_encode_video: bool) -> Self {
        Self {
            op: VoiceOpcode::SessionUpdate,
            d: SessionUpdateData {
                codecs: supported_codecs(preferred, can_encode_video),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_codecs(
        preferred: DiscordVideoCodec,
        can_encode_video: bool,
    ) -> Vec<SelectProtocolCodec> {
        supported_codecs(preferred, can_encode_video)
            .into_iter()
            .filter(|codec| codec.codec_type == CodecType::Video)
            .collect()
    }

    #[test]
    fn av1_preference_keeps_h264_fallback_with_unique_wire_values() {
        let codecs = video_codecs(DiscordVideoCodec::Av1, true);

        assert_eq!(codecs.len(), 2);
        assert_eq!(codecs[0].name, "AV1");
        assert_eq!(codecs[0].priority, PREFERRED_VIDEO_PRIORITY);
        assert_eq!(codecs[0].payload_type, AV1_PAYLOAD_TYPE);
        assert_eq!(codecs[0].rtx_payload_type, Some(AV1_RTX_PAYLOAD_TYPE));
        assert_eq!(codecs[0].encode, Some(true));
        assert_eq!(codecs[0].decode, Some(false));
        assert_eq!(codecs[1].name, "H264");
        assert_eq!(codecs[1].priority, FALLBACK_VIDEO_PRIORITY);
        assert_eq!(codecs[1].payload_type, H264_PAYLOAD_TYPE);
        assert_eq!(codecs[1].rtx_payload_type, Some(H264_RTX_PAYLOAD_TYPE));
        assert_eq!(codecs[1].encode, Some(true));
        assert_eq!(codecs[1].decode, Some(true));
    }

    #[test]
    fn h264_preference_reverses_video_priorities() {
        let codecs = video_codecs(DiscordVideoCodec::H264, true);

        assert_eq!(codecs.len(), 2);
        assert_eq!(codecs[0].name, "H264");
        assert_eq!(codecs[0].priority, PREFERRED_VIDEO_PRIORITY);
        assert_eq!(codecs[1].name, "AV1");
        assert_eq!(codecs[1].priority, FALLBACK_VIDEO_PRIORITY);
    }

    #[test]
    fn receive_only_offer_omits_av1_encoder() {
        let codecs = video_codecs(DiscordVideoCodec::Av1, false);

        assert_eq!(codecs.len(), 1);
        assert_eq!(codecs[0].name, "H264");
        assert_eq!(codecs[0].encode, Some(false));
        assert_eq!(codecs[0].decode, Some(true));
    }

    #[test]
    fn session_update_serializes_opcode_and_av1_name() {
        let payload = SessionUpdatePayload::new(DiscordVideoCodec::Av1, true);
        let json = String::from_utf8(facet_json::to_vec(&payload).unwrap()).unwrap();

        assert!(json.starts_with(r#"{"op":14,"d":{"codecs":["#));
        assert!(json.contains(r#""name":"AV1""#));
        assert!(json.contains(r#""payload_type":101"#));
        assert!(json.contains(r#""name":"H264""#));
        assert!(json.contains(r#""payload_type":103"#));
    }

    #[test]
    fn select_protocol_echoes_experiments_with_fresh_rtc_connection_id() {
        let data = || SelectProtocolConnection {
            address: "127.0.0.1".to_owned(),
            port: 50_000,
            mode: "aead_xchacha20_poly1305_rtpsize".to_owned(),
        };
        let experiments = vec!["fixed_keyframe_interval".to_owned()];
        let first = SelectProtocolPayload::udp(
            data(),
            supported_codecs(DiscordVideoCodec::Av1, true),
            experiments.clone(),
        );
        let second = SelectProtocolPayload::udp(
            data(),
            supported_codecs(DiscordVideoCodec::Av1, true),
            experiments.clone(),
        );

        assert_eq!(first.d.experiments, experiments);
        assert!(Uuid::parse_str(&first.d.rtc_connection_id).is_ok());
        assert!(Uuid::parse_str(&second.d.rtc_connection_id).is_ok());
        assert_ne!(first.d.rtc_connection_id, second.d.rtc_connection_id);

        let json = String::from_utf8(facet_json::to_vec(&first).unwrap()).unwrap();
        assert!(json.contains(r#""experiments":["fixed_keyframe_interval"]"#));
        assert!(json.contains(r#""rtc_connection_id":"#));
    }
}
