use std::{fmt, io};

use facet::Facet;

use super::super::{DiscordAudioCodec, DiscordVideoCodec};

// <https://docs.discord.food/topics/voice-connections#transport-encryption-mode>
#[allow(non_camel_case_types)]
#[derive(Debug, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum EncryptionMode {
    aead_aes256_gcm,
    aead_aes256_gcm_rtpsize,
    aead_xchacha20_poly1305_rtpsize,
    xsalsa20_poly1305,
    xsalsa20_poly1305_suffix,
    xsalsa20_poly1305_lite,
    xsalsa20_poly1305_lite_rtpsize,
}
impl EncryptionMode {
    pub fn as_str(&self) -> &str {
        match self {
            EncryptionMode::aead_aes256_gcm => "aead_aes256_gcm",
            EncryptionMode::aead_aes256_gcm_rtpsize => "aead_aes256_gcm_rtpsize",
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => "aead_xchacha20_poly1305_rtpsize",
            EncryptionMode::xsalsa20_poly1305 => "xsalsa20_poly1305",
            EncryptionMode::xsalsa20_poly1305_lite_rtpsize => "xsalsa20_poly1305_lite_rtpsize",
            EncryptionMode::xsalsa20_poly1305_suffix => "xsalsa20_poly1305_suffix",
            EncryptionMode::xsalsa20_poly1305_lite => "xsalsa20_poly1305_lite",
        }
    }
    pub const fn nonce_size(&self) -> Option<usize> {
        match self {
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => Some(4),
            _ => None,
        }
    }
}
impl TryFrom<&str> for EncryptionMode {
    type Error = String;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        for mode in [
            EncryptionMode::aead_aes256_gcm_rtpsize,
            EncryptionMode::aead_xchacha20_poly1305_rtpsize,
            EncryptionMode::xsalsa20_poly1305_lite_rtpsize,
            EncryptionMode::aead_aes256_gcm,
            EncryptionMode::xsalsa20_poly1305,
            EncryptionMode::xsalsa20_poly1305_suffix,
            EncryptionMode::xsalsa20_poly1305_lite,
        ] {
            if mode.as_str() == value {
                return Ok(mode);
            }
        }
        Err(format!("Unknown encryption mode: {}", value))
    }
}

/// <https://docs.discord.food/topics/voice-connections#session-description-structure>
#[derive(Facet)]
pub struct SessionDescription {
    audio_codec: String,
    video_codec: String,
    media_session_id: String,
    mode: Option<EncryptionMode>,
    secret_key: Option<Vec<u8>>,
    keyframe_interval: Option<u32>,
    dave_protocol_version: u16,
}

impl fmt::Debug for SessionDescription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionDescription")
            .field("audio_codec", &self.audio_codec)
            .field("video_codec", &self.video_codec)
            .field("media_session_id", &"[redacted]")
            .field("mode", &self.mode)
            .field(
                "secret_key",
                &self.secret_key.as_ref().map(|_| "[redacted]"),
            )
            .field("keyframe_interval", &self.keyframe_interval)
            .field("dave_protocol_version", &self.dave_protocol_version)
            .finish()
    }
}

impl SessionDescription {
    pub fn mode(&self) -> Option<&EncryptionMode> {
        self.mode.as_ref()
    }
    pub fn validated_secret_key(&self) -> io::Result<&[u8; 32]> {
        let key = self
            .secret_key
            .as_deref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing secret key"))?;
        key.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "voice transport secret key must contain exactly 32 bytes",
            )
        })
    }
    pub fn validate(&self) -> io::Result<()> {
        if !self
            .audio_codec
            .eq_ignore_ascii_case(DiscordAudioCodec::Opus.wire_name())
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported voice audio codec",
            ));
        }
        if DiscordVideoCodec::from_wire_name(&self.video_codec).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported voice video codec",
            ));
        }
        match self.mode() {
            Some(EncryptionMode::aead_xchacha20_poly1305_rtpsize) => {}
            Some(mode) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("unsupported encryption mode: {mode:?}"),
                ));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing encryption mode",
                ));
            }
        }
        self.validated_secret_key()?;
        Ok(())
    }
    pub fn dave_protocol_version(&self) -> u16 {
        self.dave_protocol_version
    }
    /// The codec the server expects us to *send* video with.
    pub fn video_codec(&self) -> &str {
        &self.video_codec
    }
    /// Server-selected interval between outbound video keyframes, in
    /// milliseconds. Missing means retain transport default/current value.
    pub fn keyframe_interval_ms(&self) -> Option<u32> {
        self.keyframe_interval
    }
    /// Test-only constructor; production values are gateway-deserialized.
    #[cfg(test)]
    pub(crate) fn for_tests(mode: EncryptionMode, secret_key: Vec<u8>) -> Self {
        Self::for_tests_with_keyframe_interval(mode, secret_key, None)
    }

    #[cfg(test)]
    pub(crate) fn for_tests_with_keyframe_interval(
        mode: EncryptionMode,
        secret_key: Vec<u8>,
        keyframe_interval: Option<u32>,
    ) -> Self {
        Self {
            audio_codec: "opus".into(),
            video_codec: "H264".into(),
            media_session_id: String::new(),
            mode: Some(mode),
            secret_key: Some(secret_key),
            keyframe_interval,
            dave_protocol_version: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_modes_have_no_panic_capable_nonce_size() {
        assert_eq!(EncryptionMode::aead_aes256_gcm.nonce_size(), None);
        assert_eq!(
            EncryptionMode::aead_xchacha20_poly1305_rtpsize.nonce_size(),
            Some(4)
        );
    }

    #[test]
    fn session_description_rejects_invalid_transport_state() {
        let short_key = SessionDescription::for_tests(
            EncryptionMode::aead_xchacha20_poly1305_rtpsize,
            vec![7; 31],
        );
        assert_eq!(
            short_key.validate().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let unsupported =
            SessionDescription::for_tests(EncryptionMode::aead_aes256_gcm, vec![7; 32]);
        assert_eq!(
            unsupported.validate().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );

        let mut unsupported_audio = SessionDescription::for_tests(
            EncryptionMode::aead_xchacha20_poly1305_rtpsize,
            vec![7; 32],
        );
        unsupported_audio.audio_codec = "aac".into();
        assert_eq!(
            unsupported_audio.validate().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );

        let mut unsupported_video = SessionDescription::for_tests(
            EncryptionMode::aead_xchacha20_poly1305_rtpsize,
            vec![7; 32],
        );
        unsupported_video.video_codec = "VP9".into();
        assert_eq!(
            unsupported_video.validate().unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn session_description_debug_redacts_secret_material() {
        let description = SessionDescription::for_tests(
            EncryptionMode::aead_xchacha20_poly1305_rtpsize,
            vec![251; 32],
        );
        let debug = format!("{description:?}");

        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("251"));
    }
}
