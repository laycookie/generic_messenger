use facet::Facet;

// <https://docs.discord.food/topics/voice-connections#encryption-mode>
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
    pub fn tag_len(&self) -> usize {
        match self {
            EncryptionMode::aead_aes256_gcm => 16,
            EncryptionMode::aead_aes256_gcm_rtpsize => unimplemented!(),
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => 16,
            EncryptionMode::xsalsa20_poly1305 => unimplemented!(),
            EncryptionMode::xsalsa20_poly1305_suffix => unimplemented!(),
            EncryptionMode::xsalsa20_poly1305_lite => unimplemented!(),
            EncryptionMode::xsalsa20_poly1305_lite_rtpsize => unimplemented!(),
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
impl From<&str> for EncryptionMode {
    fn from(value: &str) -> Self {
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
                return mode;
            }
        }
        panic!("Not implemented: {}", value);
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
}
