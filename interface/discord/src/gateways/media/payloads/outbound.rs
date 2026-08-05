use facet::Facet;
use simple_video::VideoCodec;
use uuid::Uuid;

use super::super::{
    VoiceOpcode,
    connection::rtp::{
        AV1_PAYLOAD_TYPE, AV1_RTX_PAYLOAD_TYPE, H264_PAYLOAD_TYPE, H264_RTX_PAYLOAD_TYPE,
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

/// Codec names accepted in the Voice Gateway Select Protocol payload.
///
/// Discord may add codecs later. New values must be added explicitly so codec
/// negotiation does not silently emit an unsupported wire name.
/// <https://docs.discord.food/topics/voice-connections#supported-codecs>
#[derive(Clone, Copy, Debug, Eq, Facet, PartialEq)]
#[non_exhaustive]
#[repr(u8)]
pub(crate) enum SupportedCodec {
    #[facet(rename = "opus")]
    Opus,
    #[facet(rename = "AV1")]
    Av1,
    #[facet(rename = "H264")]
    H264,
}

impl SupportedCodec {
    pub(crate) const fn codec_type(self) -> CodecType {
        match self {
            Self::Opus => CodecType::Audio,
            Self::Av1 | Self::H264 => CodecType::Video,
        }
    }

    pub(crate) const fn payload_type(self) -> u8 {
        match self {
            Self::Opus => 120,
            Self::Av1 => AV1_PAYLOAD_TYPE,
            Self::H264 => H264_PAYLOAD_TYPE,
        }
    }

    pub(crate) const fn rtx_payload_type(self) -> Option<u8> {
        match self {
            Self::Opus => None,
            Self::Av1 => Some(AV1_RTX_PAYLOAD_TYPE),
            Self::H264 => Some(H264_RTX_PAYLOAD_TYPE),
        }
    }

    pub(crate) fn into_payload(
        self,
        priority: u32,
        encode: Option<bool>,
        decode: Option<bool>,
    ) -> SelectProtocolCodec {
        SelectProtocolCodec {
            name: self,
            codec_type: self.codec_type(),
            priority,
            payload_type: self.payload_type(),
            rtx_payload_type: self.rtx_payload_type(),
            encode,
            decode,
        }
    }
}

/// Build codec capabilities for Select Protocol and Session Update.
///
/// Send-capable connections advertise both encoders so a preference change can
/// be negotiated without rebuilding UDP transport. Receive-only Go Live links
/// omit AV1 because this client does not decode it.
pub(crate) fn supported_codecs(
    preferred: VideoCodec,
    can_encode_video: bool,
) -> Vec<SelectProtocolCodec> {
    let mut codecs = vec![SupportedCodec::Opus.into_payload(PREFERRED_VIDEO_PRIORITY, None, None)];

    if !can_encode_video {
        codecs.push(SupportedCodec::H264.into_payload(
            PREFERRED_VIDEO_PRIORITY,
            Some(false),
            Some(true),
        ));
        return codecs;
    }

    let preferred = match preferred {
        VideoCodec::Av1 => SupportedCodec::Av1,
        VideoCodec::H264 => SupportedCodec::H264,
        _ => SupportedCodec::H264,
    };
    let fallback = match preferred {
        SupportedCodec::Av1 => SupportedCodec::H264,
        SupportedCodec::H264 => SupportedCodec::Av1,
        SupportedCodec::Opus => unreachable!("audio cannot be preferred video codec"),
    };
    for (codec, priority) in [
        (preferred, PREFERRED_VIDEO_PRIORITY),
        (fallback, FALLBACK_VIDEO_PRIORITY),
    ] {
        codecs.push(codec.into_payload(
            priority,
            Some(true),
            Some(matches!(codec, SupportedCodec::H264)),
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
    pub(crate) name: SupportedCodec,
    #[facet(rename = "type")]
    pub(crate) codec_type: CodecType,
    pub(crate) priority: u32,
    pub(crate) payload_type: u8,
    pub(crate) rtx_payload_type: Option<u8>,
    pub(crate) encode: Option<bool>,
    pub(crate) decode: Option<bool>,
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
    pub(crate) fn new(preferred: VideoCodec, can_encode_video: bool) -> Self {
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

    fn video_codecs(preferred: VideoCodec, can_encode_video: bool) -> Vec<SelectProtocolCodec> {
        supported_codecs(preferred, can_encode_video)
            .into_iter()
            .filter(|codec| codec.codec_type == CodecType::Video)
            .collect()
    }

    #[test]
    fn av1_preference_keeps_h264_fallback_with_unique_wire_values() {
        let codecs = video_codecs(VideoCodec::Av1, true);

        assert_eq!(codecs.len(), 2);
        assert_eq!(codecs[0].name, SupportedCodec::Av1);
        assert_eq!(codecs[0].priority, PREFERRED_VIDEO_PRIORITY);
        assert_eq!(codecs[0].payload_type, AV1_PAYLOAD_TYPE);
        assert_eq!(codecs[0].rtx_payload_type, Some(AV1_RTX_PAYLOAD_TYPE));
        assert_eq!(codecs[0].encode, Some(true));
        assert_eq!(codecs[0].decode, Some(false));
        assert_eq!(codecs[1].name, SupportedCodec::H264);
        assert_eq!(codecs[1].priority, FALLBACK_VIDEO_PRIORITY);
        assert_eq!(codecs[1].payload_type, H264_PAYLOAD_TYPE);
        assert_eq!(codecs[1].rtx_payload_type, Some(H264_RTX_PAYLOAD_TYPE));
        assert_eq!(codecs[1].encode, Some(true));
        assert_eq!(codecs[1].decode, Some(true));
    }

    #[test]
    fn h264_preference_reverses_video_priorities() {
        let codecs = video_codecs(VideoCodec::H264, true);

        assert_eq!(codecs.len(), 2);
        assert_eq!(codecs[0].name, SupportedCodec::H264);
        assert_eq!(codecs[0].priority, PREFERRED_VIDEO_PRIORITY);
        assert_eq!(codecs[1].name, SupportedCodec::Av1);
        assert_eq!(codecs[1].priority, FALLBACK_VIDEO_PRIORITY);
    }

    #[test]
    fn receive_only_offer_omits_av1_encoder() {
        let codecs = video_codecs(VideoCodec::Av1, false);

        assert_eq!(codecs.len(), 1);
        assert_eq!(codecs[0].name, SupportedCodec::H264);
        assert_eq!(codecs[0].encode, Some(false));
        assert_eq!(codecs[0].decode, Some(true));
    }

    #[test]
    fn session_update_serializes_opcode_and_av1_name() {
        let payload = SessionUpdatePayload::new(VideoCodec::Av1, true);
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
            supported_codecs(VideoCodec::Av1, true),
            experiments.clone(),
        );
        let second = SelectProtocolPayload::udp(
            data(),
            supported_codecs(VideoCodec::Av1, true),
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
