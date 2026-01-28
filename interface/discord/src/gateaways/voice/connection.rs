use facet::Facet;
use libsodium_rs::crypto_aead;
use num_enum::TryFromPrimitive;
use pnet_macros::packet;
use pnet_macros_support::{
    packet::Packet,
    packet::PrimitiveValues,
    types::{u1, u2, u4, u7, u16be, u32be},
};
use simple_audio_channels::AudioSampleType;
use smol::net::UdpSocket;
use std::{
    collections::vec_deque::Iter,
    error::Error,
    num::Wrapping,
    ops::{Add, AddAssign, Sub, SubAssign},
};
use tracing::{error, info, warn};

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
            EncryptionMode::aead_aes256_gcm_rtpsize => todo!(),
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => 16,
            EncryptionMode::xsalsa20_poly1305 => todo!(),
            EncryptionMode::xsalsa20_poly1305_suffix => todo!(),
            EncryptionMode::xsalsa20_poly1305_lite => todo!(),
            EncryptionMode::xsalsa20_poly1305_lite_rtpsize => todo!(),
        }
    }
    pub fn nonce_size(&self) -> usize {
        match self {
            EncryptionMode::aead_aes256_gcm => todo!(),
            EncryptionMode::aead_aes256_gcm_rtpsize => todo!(),
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => 4,
            EncryptionMode::xsalsa20_poly1305 => todo!(),
            EncryptionMode::xsalsa20_poly1305_suffix => todo!(),
            EncryptionMode::xsalsa20_poly1305_lite => todo!(),
            EncryptionMode::xsalsa20_poly1305_lite_rtpsize => todo!(),
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
#[derive(Facet)]
pub struct SessionDescription {
    audio_codec: String,
    video_codec: String,
    media_session_id: String,
    mode: Option<EncryptionMode>,
    secret_key: Option<Vec<u8>>,
    keyframe_interval: Option<u32>,
}
impl SessionDescription {
    pub fn mode(&self) -> Option<&EncryptionMode> {
        self.mode.as_ref()
    }
    pub fn secret_key(&self) -> Option<&Vec<u8>> {
        self.secret_key.as_ref()
    }
}

pub type Ssrc = u32;
pub struct Connection {
    udp: UdpSocket,
    ssrc: Ssrc,
    description: Option<SessionDescription>,
    decoder: opus::Decoder,
    encoder: opus::Encoder,
    sequence: Wrap16,
    timestamp: Wrap32,
    nonce: u32,
    rtp_packet_buf: [u8; 1024],
    decoded_audio_buf: [i16; 8048],
    encoded_audio_buf: [u8; 1276],
}
impl Connection {
    pub fn new(udp: UdpSocket, ssrc: u32) -> Self {
        Self {
            udp,
            description: None,
            ssrc,
            decoder: opus::Decoder::new(48000, opus::Channels::Stereo).unwrap(),
            encoder: opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::Voip)
                .unwrap(),
            sequence: Wrap16::from(0),
            timestamp: Wrap32::from(0),
            nonce: 0,
            rtp_packet_buf: [0; 1024],
            decoded_audio_buf: [0; 8048],
            encoded_audio_buf: [0; 1276],
        }
    }
    pub fn ssrc(&self) -> Ssrc {
        self.ssrc
    }
    pub fn description(&self) -> Option<&SessionDescription> {
        self.description.as_ref()
    }
    pub fn set_description(&mut self, description: SessionDescription) {
        self.description = Some(description);
    }

    /// Poll next UDP voice packet.
    ///
    /// When a new inbound SSRC is discovered, this emits `SocketEvent::AddAudioSource(sender)`.
    /// The UI answers by calling `sender.send(audio_source_id)`. We keep the receiver internally
    /// and store the mapping once it resolves.
    pub async fn recv_audio(&mut self) -> Option<(Ssrc, &[i16])> {
        let n_bytes_recived = self.udp.recv(&mut self.rtp_packet_buf).await.ok()?;
        let rtp_packet_buf = &self.rtp_packet_buf[..n_bytes_recived];

        let packet_type = DiscordPacketType::try_from(rtp_packet_buf[1]);

        if packet_type.is_err() || packet_type != Ok(DiscordPacketType::Voice) {
            warn!("Unkown packet type on udp: {:?}", rtp_packet_buf[1]);
            return None;
        };

        let rtp_packet = RtpPacket::new(rtp_packet_buf).unwrap();

        let is_rtp_extended = rtp_packet.get_extension() != 0;

        let rtp_header_len = if is_rtp_extended {
            rtp_packet.packet.len() - rtp_packet.payload().len() + 4
        } else {
            info!("None-extended");
            rtp_packet.packet.len() - rtp_packet.payload().len()
        };
        let (rtp_header, rtp_body) = rtp_packet.packet().split_at(rtp_header_len);

        let description = self.description.as_ref().unwrap();
        let mode = description.mode().unwrap();
        let decrypted_payload = match mode {
            EncryptionMode::aead_aes256_gcm_rtpsize => todo!(),
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => {
                let (voice_payload, nonce_u32) =
                    rtp_body.split_at(rtp_body.len() - mode.nonce_size());

                let mut nonce = [0; 24];
                nonce[..mode.nonce_size()].copy_from_slice(nonce_u32);
                let nonce = crypto_aead::xchacha20poly1305::Nonce::from_bytes(nonce);

                let key = crypto_aead::xchacha20poly1305::Key::from_bytes(
                    &description.secret_key().unwrap()[..],
                )
                .expect("Invalid key length");

                crypto_aead::xchacha20poly1305::decrypt(
                    voice_payload,
                    Some(rtp_header),
                    &nonce,
                    &key,
                )
                .unwrap()
            }
            EncryptionMode::aead_aes256_gcm => todo!("Depricated"),
            EncryptionMode::xsalsa20_poly1305 => todo!("Depricated"),
            EncryptionMode::xsalsa20_poly1305_suffix => todo!("Depricated"),
            EncryptionMode::xsalsa20_poly1305_lite => todo!("Depricated"),
            EncryptionMode::xsalsa20_poly1305_lite_rtpsize => todo!("Depricated"),
        };

        // <https://datatracker.ietf.org/doc/html/rfc6464>
        let (potentially, voice_data) = decrypted_payload.split_at(8);
        let unkown_const = &potentially[..1]; // CONST 55
        // let timecode = &potentially[1..4]; // Timecode
        let unkown_const_2 = &potentially[4..5]; // CONST 16
        // let avrage_volume = &potentially[5..6]; // Avrage volume of the frame?
        let unkown_const_3 = &potentially[6..7]; // CONST 144
        let channels = &potentially[7]; // Channels?
        if unkown_const != [50] {
            warn!("ANOMOLY const1");
        }
        if unkown_const_2 != [16] {
            warn!("ANOMOLY const2");
        }
        if unkown_const_3 != [144] {
            warn!("ANOMOLY const2");
        }

        let n_decoded_samples =
            match self
                .decoder
                .decode(voice_data, &mut self.decoded_audio_buf, false)
            {
                Ok(n_samples) => n_samples,
                Err(err) => {
                    error!("{:?}", err);
                    return None;
                }
            };

        Some((
            rtp_packet.get_ssrc(),
            &self.decoded_audio_buf[..n_decoded_samples * *channels as usize],
        ))
    }

    pub async fn send_audio_frame(
        &mut self,
        samples: &[AudioSampleType],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if samples.is_empty() {
            warn!("Noting to send");
            return Ok(());
        }
        if !samples.len().is_multiple_of(2) {
            return Err("Expected interleaved stereo samples".into());
        }

        let description = self
            .description
            .as_ref()
            .ok_or("Missing session description")?;
        let mode = description.mode().ok_or("Missing encryption mode")?;
        let secret_key = description.secret_key().ok_or("Missing secret key")?;

        let encoded_len = self
            .encoder
            .encode_float(samples, &mut self.encoded_audio_buf)
            .map_err(|err| format!("Opus encode failed: {err:?}"))?;
        let opus_payload = &self.encoded_audio_buf[..encoded_len];

        let sequence = self.sequence;
        let timestamp = self.timestamp;
        let samples_per_channel = (samples.len() / 2) as u32;
        self.sequence += 1;
        self.timestamp += samples_per_channel;

        let mut rtp_header = [0u8; 12];
        rtp_header[0] = 0x80;
        rtp_header[1] = DiscordPacketType::Voice as u8;
        rtp_header[2..4].copy_from_slice(&u16::from(sequence).to_be_bytes());
        rtp_header[4..8].copy_from_slice(&u32::from(timestamp).to_be_bytes());
        rtp_header[8..12].copy_from_slice(&self.ssrc.to_be_bytes());

        match mode {
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => {
                let nonce_u32 = self.nonce.to_be_bytes();
                self.nonce = self.nonce.wrapping_add(1);

                let mut nonce = [0u8; 24];
                nonce[..mode.nonce_size()].copy_from_slice(&nonce_u32);
                let nonce = crypto_aead::xchacha20poly1305::Nonce::from_bytes(nonce);
                let key = crypto_aead::xchacha20poly1305::Key::from_bytes(&secret_key[..])
                    .expect("Invalid key length");

                let encrypted = crypto_aead::xchacha20poly1305::encrypt(
                    opus_payload,
                    Some(&rtp_header),
                    &nonce,
                    &key,
                )?;

                let mut packet =
                    Vec::with_capacity(rtp_header.len() + encrypted.len() + nonce_u32.len());
                packet.extend_from_slice(&rtp_header);
                packet.extend_from_slice(&encrypted);
                packet.extend_from_slice(&nonce_u32);

                self.udp.send(&packet).await?;
            }
            _ => {
                return Err(format!("Unsupported encryption mode: {:?}", mode).into());
            }
        }

        Ok(())
    }
}

#[derive(Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
enum DiscordPacketType {
    Voice = 0x78,
    Unkown1 = 0xc9,
}

// === RTP defenitions ===
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Wrap16(pub Wrapping<u16>);

impl Wrap16 {
    #[must_use]
    pub fn new(v: u16be) -> Self {
        Self(Wrapping(v))
    }
}

impl From<Wrap16> for u16 {
    fn from(val: Wrap16) -> Self {
        (val.0).0
    }
}

impl From<u16> for Wrap16 {
    fn from(val: u16) -> Self {
        Wrap16(Wrapping(val))
    }
}

impl PrimitiveValues for Wrap16 {
    type T = (u16be,);

    fn to_primitive_values(&self) -> Self::T {
        ((*self).into(),)
    }
}

impl Add<u16> for Wrap16 {
    type Output = Self;

    fn add(self, other: u16) -> Self::Output {
        Wrap16(self.0 + Wrapping(other))
    }
}

impl AddAssign<u16> for Wrap16 {
    fn add_assign(&mut self, other: u16) {
        self.0 += Wrapping(other);
    }
}

impl Sub<u16> for Wrap16 {
    type Output = Self;

    fn sub(self, other: u16) -> Self::Output {
        Wrap16(self.0 - Wrapping(other))
    }
}

impl SubAssign<u16> for Wrap16 {
    fn sub_assign(&mut self, other: u16) {
        self.0 -= Wrapping(other);
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Wrap32(pub Wrapping<u32>);

impl Wrap32 {
    #[must_use]
    pub fn new(v: u32be) -> Self {
        Self(Wrapping(v))
    }
}

impl From<Wrap32> for u32 {
    fn from(val: Wrap32) -> Self {
        (val.0).0
    }
}

impl From<u32> for Wrap32 {
    fn from(val: u32) -> Self {
        Wrap32(Wrapping(val))
    }
}

impl PrimitiveValues for Wrap32 {
    type T = (u32be,);

    fn to_primitive_values(&self) -> Self::T {
        ((*self).into(),)
    }
}

impl Add<u32> for Wrap32 {
    type Output = Self;

    fn add(self, other: u32) -> Self::Output {
        Wrap32(self.0 + Wrapping(other))
    }
}

impl AddAssign<u32> for Wrap32 {
    fn add_assign(&mut self, other: u32) {
        self.0 += Wrapping(other);
    }
}

impl Sub<u32> for Wrap32 {
    type Output = Self;

    fn sub(self, other: u32) -> Self::Output {
        Wrap32(self.0 - Wrapping(other))
    }
}

impl SubAssign<u32> for Wrap32 {
    fn sub_assign(&mut self, other: u32) {
        self.0 -= Wrapping(other);
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
/// [IANA page]: https://www.iana.org/assignments/rtp-parameters/rtp-parameters.xhtml#rtp-parameters-1
pub enum RtpType {
    /// Code 0.
    Pcmu,
    /// Code 3.
    Gsm,
    /// Code 4.
    G723,
    /// Code 5--6, 16--17.
    Dvi4(u8),
    /// Code 7.
    Lpc,
    /// Code 8.
    Pcma,
    /// Code 9.
    G722,
    /// Code 10.
    L16Stereo,
    /// Code 11.
    L16Mono,
    /// Code 12.
    Qcelp,
    /// Code 13.
    Cn,
    /// Code 14.
    Mpa,
    /// Code 15.
    G728,
    /// Code 18.
    G729,
    /// Code 25.
    CelB,
    /// Code 26.
    Jpeg,
    /// Code 28.
    Nv,
    /// Code 31.
    H261,
    /// Code 32.
    Mpv,
    /// Code 33.
    Mp2t,
    /// Code 34.
    H263,
    /// Dynamically assigned payload type (codes >= 96).
    Dynamic(u8),
    /// Reserved payload types, typically to mitigate RTCP packet type collisions (1--2, 19, 72--76).
    Reserved(u8),
    /// Unassigned payload type (all remaining < 128).
    Unassigned(u8),
    /// Code point too high for u7: application error?
    Illegal(u8),
}
impl RtpType {
    #[must_use]
    pub fn new(val: u7) -> Self {
        match val {
            0 => Self::Pcmu,
            3 => Self::Gsm,
            4 => Self::G723,
            5 | 6 | 16 | 17 => Self::Dvi4(val),
            7 => Self::Lpc,
            8 => Self::Pcma,
            9 => Self::G722,
            10 => Self::L16Stereo,
            11 => Self::L16Mono,
            12 => Self::Qcelp,
            13 => Self::Cn,
            14 => Self::Mpa,
            15 => Self::G728,
            18 => Self::G729,
            25 => Self::CelB,
            26 => Self::Jpeg,
            28 => Self::Nv,
            31 => Self::H261,
            32 => Self::Mpv,
            33 => Self::Mp2t,
            34 => Self::H263,
            1..=2 | 19 | 72..=76 => Self::Reserved(val),
            96..=127 => Self::Dynamic(val),
            128..=255 => Self::Illegal(val),
            _ => Self::Unassigned(val),
        }
    }
}

impl PrimitiveValues for RtpType {
    type T = (u7,);

    fn to_primitive_values(&self) -> Self::T {
        match self {
            Self::Pcmu => (0,),
            Self::Gsm => (3,),
            Self::G723 => (4,),
            Self::Lpc => (7,),
            Self::Pcma => (8,),
            Self::G722 => (9,),
            Self::L16Stereo => (10,),
            Self::L16Mono => (11,),
            Self::Qcelp => (12,),
            Self::Cn => (13,),
            Self::Mpa => (14,),
            Self::G728 => (15,),
            Self::G729 => (18,),
            Self::CelB => (25,),
            Self::Jpeg => (26,),
            Self::Nv => (28,),
            Self::H261 => (31,),
            Self::Mpv => (32,),
            Self::Mp2t => (33,),
            Self::H263 => (34,),

            Self::Dvi4(val)
            | Self::Dynamic(val)
            | Self::Unassigned(val)
            | Self::Reserved(val)
            | Self::Illegal(val) => (*val,),
        }
    }
}

/// [Real-time Transport Protocol]: https://tools.ietf.org/html/rfc3550
#[packet]
#[derive(Eq, PartialEq)]
pub struct Rtp {
    pub version: u2,
    pub padding: u1,
    pub extension: u1,
    pub csrc_count: u4,
    pub marker: u1,
    #[construct_with(u7)]
    pub payload_type: RtpType,
    #[construct_with(u16be)]
    pub sequence: Wrap16,
    #[construct_with(u32be)]
    pub timestamp: Wrap32,
    pub ssrc: u32be,
    #[length = "csrc_count"]
    pub csrc_list: Vec<u32be>,
    #[payload]
    pub payload: Vec<u8>,
}
