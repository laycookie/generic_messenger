use facet::Facet;

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
    pub fn nonce_size(&self) -> usize {
        match self {
            EncryptionMode::aead_aes256_gcm => unimplemented!(),
            EncryptionMode::aead_aes256_gcm_rtpsize => unimplemented!(),
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => 4,
            EncryptionMode::xsalsa20_poly1305 => unimplemented!(),
            EncryptionMode::xsalsa20_poly1305_suffix => unimplemented!(),
            EncryptionMode::xsalsa20_poly1305_lite => unimplemented!(),
            EncryptionMode::xsalsa20_poly1305_lite_rtpsize => unimplemented!(),
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
#[derive(Debug, Facet)]
pub struct SessionDescription {
    audio_codec: String,
    video_codec: String,
    media_session_id: String,
    mode: Option<EncryptionMode>,
    secret_key: Option<Vec<u8>>,
    keyframe_interval: Option<u32>,
    dave_protocol_version: u16,
}
impl SessionDescription {
    pub fn mode(&self) -> Option<&EncryptionMode> {
        self.mode.as_ref()
    }
    pub fn secret_key(&self) -> Option<&Vec<u8>> {
        self.secret_key.as_ref()
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
