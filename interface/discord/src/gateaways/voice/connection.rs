use dashmap::DashMap;
use davey::{Codec, DaveSession, MediaType};
use facet::Facet;
use futures::lock::Mutex as AsyncMutex;
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
    error::Error,
    num::Wrapping,
    ops::{Add, AddAssign, Sub, SubAssign},
    sync::OnceLock,
    time::Instant,
};
use tracing::{info, trace, warn};

use crate::api_types::SNOWFLAKE;

pub const VOICE_FREQUANCY: usize = 48_000;
pub const VOICE_CHANNELS: usize = 2; // Stereo
pub const VOICE_FRAME_SAMPLES: usize = 960 * VOICE_CHANNELS;

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

pub struct RecvAudioFuture<'a> {
    udp: &'a UdpSocket,
    description: &'a SessionDescription,
    dave_session: &'a AsyncMutex<Option<DaveSession>>,
    ssrc_to_user_id: &'a DashMap<Ssrc, SNOWFLAKE>,
    decoder: opus::Decoder,
    rtp_packet_buf: [u8; 1024],
    decoded_audio_buf: [i16; 8048],
}
impl RecvAudioFuture<'_> {
    pub async fn recv_audio(&mut self) -> Result<(Ssrc, &[i16]), Box<dyn Error>> {
        let n_bytes_recived = self.udp.recv(&mut self.rtp_packet_buf).await?;
        let rtp_packet_buf = &self.rtp_packet_buf[..n_bytes_recived];

        let packet_type = DiscordPacketType::try_from(rtp_packet_buf[1]);

        match packet_type {
            Ok(DiscordPacketType::Voice) => {
                // Continue processing voice packet below
            }
            Ok(DiscordPacketType::RtcpSenderReport | DiscordPacketType::RtcpReceiverReport) => {
                // RTCP control packets - ignore silently
                return Err("Expected voice packet".into());
            }
            _ => {
                trace!("Unknown packet type on UDP: {:?}", rtp_packet_buf[1]);
                return Err("Expected voice packet".into());
            }
        }

        let rtp_packet = RtpPacket::new(rtp_packet_buf).ok_or("Failed to parse rtp packet")?;

        let is_rtp_extended = rtp_packet.get_extension() != 0;

        let rtp_header_len = if is_rtp_extended {
            rtp_packet.packet.len() - rtp_packet.payload().len() + 4
        } else {
            info!("None-extended");
            rtp_packet.packet.len() - rtp_packet.payload().len()
        };
        let (rtp_header, rtp_body) = rtp_packet.packet().split_at(rtp_header_len);

        let mode = self.description.mode().unwrap();
        let decrypted_payload = match mode {
            EncryptionMode::aead_aes256_gcm_rtpsize => unimplemented!(),
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => {
                let (voice_payload, nonce_u32) =
                    rtp_body.split_at(rtp_body.len() - mode.nonce_size());

                let mut nonce = [0; 24];
                nonce[..mode.nonce_size()].copy_from_slice(nonce_u32);
                let nonce = crypto_aead::xchacha20poly1305::Nonce::from_bytes(nonce);

                let key = crypto_aead::xchacha20poly1305::Key::from_bytes(
                    self.description.secret_key().unwrap(),
                )
                .expect("Invalid key length");

                crypto_aead::xchacha20poly1305::decrypt(
                    voice_payload,
                    Some(rtp_header),
                    &nonce,
                    &key,
                )?
            }
            EncryptionMode::aead_aes256_gcm => unimplemented!("Depricated"),
            EncryptionMode::xsalsa20_poly1305 => unimplemented!("Depricated"),
            EncryptionMode::xsalsa20_poly1305_suffix => unimplemented!("Depricated"),
            EncryptionMode::xsalsa20_poly1305_lite => unimplemented!("Depricated"),
            EncryptionMode::xsalsa20_poly1305_lite_rtpsize => unimplemented!("Depricated"),
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
            trace!("RTP extension byte 0 unexpected: {:?}", unkown_const);
        }
        if unkown_const_2 != [16] {
            trace!("RTP extension byte 4 unexpected: {:?}", unkown_const_2);
        }
        if unkown_const_3 != [144] {
            trace!("RTP extension byte 6 unexpected: {:?}", unkown_const_3);
        }

        let voice_data = if rtp_packet.get_padding() == 1
            && let Some(last_byte) = voice_data.last()
        {
            &voice_data[..voice_data.len() - *last_byte as usize]
        } else {
            voice_data
        };
        // Decrypt Dave
        let decrypted;
        let mut dave_session = self.dave_session.lock().await;
        let voice_data = if let Some(dave_session) = dave_session.as_mut() {
            decrypted = dave_session.decrypt(
                *self
                    .ssrc_to_user_id
                    .get(&rtp_packet.get_ssrc())
                    .ok_or("No mapping of ssrc to user_id TODO: Make this better")?,
                MediaType::AUDIO,
                voice_data,
            )?;
            decrypted.as_slice()
        } else {
            voice_data
        };

        // Decode opus
        let n_decoded_samples =
            self.decoder
                .decode(voice_data, &mut self.decoded_audio_buf, false)?;

        Ok((
            rtp_packet.get_ssrc(),
            &self.decoded_audio_buf[..n_decoded_samples * VOICE_CHANNELS],
        ))
    }
}

pub struct SendAudioFuture<'a> {
    udp: &'a UdpSocket,
    description: &'a SessionDescription,
    dave_session: &'a AsyncMutex<Option<DaveSession>>,
    ssrc: Ssrc,
    encoder: opus::Encoder,
    nonce: u32,
    last_send_time: Option<Instant>,
    timestamp: WrapU32,
    sequence: WrapU16,
    rtp_packet_buf: [u8; 1024],
    encoded_audio_buf: [u8; 1276],
}
impl SendAudioFuture<'_> {
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }
    pub fn last_send_time(&self) -> Option<Instant> {
        self.last_send_time
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

        let mode = self.description.mode().ok_or("Missing encryption mode")?;
        let secret_key = self.description.secret_key().ok_or("Missing secret key")?;

        let encoded_len = self
            .encoder
            .encode_float(samples, self.encoded_audio_buf.as_mut_slice())
            .map_err(|err| format!("Opus encode failed: {err:?}"))?;
        let opus_payload = &self.encoded_audio_buf[..encoded_len];

        let samples_per_channel = (samples.len() / VOICE_CHANNELS) as u32;
        let now = Instant::now();

        // Hybrid timestamp approach: sample-based during active speech,
        // but account for silence gaps based on elapsed time
        if let Some(last_time) = self.last_send_time {
            let elapsed = now.duration_since(last_time);
            let expected_frame_duration = std::time::Duration::from_micros(
                // TODO: Get rid of 1M to convert micros to secs
                (samples_per_channel * 1_000_000 / VOICE_FREQUANCY as u32) as u64,
            );

            // If gap is significantly longer than expected frame time, we had silence
            // Advance timestamp to account for the silence gap
            if elapsed > expected_frame_duration * 2 {
                let silence_samples =
                    ((elapsed.as_secs_f64() * 48000.0) as u32) - samples_per_channel;
                self.timestamp += silence_samples;
            }
        }

        let timestamp = self.timestamp;
        let sequence = self.sequence.to_owned();

        self.timestamp += samples_per_channel;
        self.sequence += 1;
        self.last_send_time = Some(now);

        const RTP_HEADER_LEN: usize = 12;
        let rtp_header = &mut self.rtp_packet_buf[0..12];
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

                let mut dave_session = self.dave_session.lock().await;
                let dave_encrypted;
                let audio_payload = if let Some(dave_session) = dave_session.as_mut() {
                    dave_encrypted =
                        dave_session.encrypt(MediaType::AUDIO, Codec::OPUS, opus_payload)?;
                    dave_encrypted.iter().as_slice()
                } else {
                    opus_payload
                };

                let encrypted = crypto_aead::xchacha20poly1305::encrypt(
                    audio_payload,
                    Some(rtp_header),
                    &nonce,
                    &key,
                )?;

                self.rtp_packet_buf[RTP_HEADER_LEN..RTP_HEADER_LEN + encrypted.len()]
                    .copy_from_slice(&encrypted);
                self.rtp_packet_buf[RTP_HEADER_LEN + encrypted.len()
                    ..RTP_HEADER_LEN + encrypted.len() + nonce_u32.len()]
                    .copy_from_slice(&nonce_u32);

                info!(
                    "{:?}",
                    &self.rtp_packet_buf[..RTP_HEADER_LEN + encrypted.len() + nonce_u32.len()]
                );
                self.udp
                    .send(
                        &self.rtp_packet_buf[..RTP_HEADER_LEN + encrypted.len() + nonce_u32.len()],
                    )
                    .await?;
            }
            _ => {
                return Err(format!("Unsupported encryption mode: {:?}", mode).into());
            }
        }

        Ok(())
    }
}

pub type Ssrc = u32;
pub struct Connection {
    ssrc: Ssrc,
    udp: UdpSocket,
    description: OnceLock<SessionDescription>,
}

impl Connection {
    pub fn init_audio<'a>(
        &'a self,
        dave_session: &'a AsyncMutex<Option<DaveSession>>,
        ssrc_to_user_id: &'a DashMap<Ssrc, SNOWFLAKE>,
    ) -> Result<(RecvAudioFuture<'a>, SendAudioFuture<'a>), Box<dyn Error>> {
        let Some(description) = self.description() else {
            return Err("No description provided".into());
        };

        Ok((
            RecvAudioFuture {
                udp: &self.udp,
                description,
                dave_session,
                ssrc_to_user_id,
                rtp_packet_buf: [0; 1024],
                decoder: opus::Decoder::new(VOICE_FREQUANCY as u32, opus::Channels::Stereo)
                    .unwrap(),
                decoded_audio_buf: [0; 8048],
            },
            SendAudioFuture {
                udp: &self.udp,
                description,
                dave_session,
                timestamp: Default::default(),
                ssrc: self.ssrc,
                last_send_time: Default::default(),
                sequence: Default::default(),
                nonce: Default::default(),
                rtp_packet_buf: [0; 1024],
                encoded_audio_buf: [0; 1276],
                encoder: opus::Encoder::new(
                    VOICE_FREQUANCY as u32,
                    opus::Channels::Stereo,
                    opus::Application::Voip,
                )
                .unwrap(),
            },
        ))
    }
    pub fn new(udp: UdpSocket, ssrc: Ssrc) -> Self {
        Self {
            udp,
            description: OnceLock::new(),
            ssrc,
        }
    }
    pub fn description(&self) -> Option<&SessionDescription> {
        self.description.get()
    }
    pub fn set_description(
        &self,
        description: SessionDescription,
    ) -> Result<(), SessionDescription> {
        self.description.set(description)
    }

    // Poll next UDP voice packet.
    //
    // When a new inbound SSRC is discovered, this emits `SocketEvent::AddAudioSource(sender)`.
    // The UI answers by calling `sender.send(audio_source_id)`. We keep the receiver internally
    // and store the mapping once it resolves.
    // TODO: Pass down description from above, that way we get it for free, and we dont need to
    // unwrap
    // pub async fn recv_audio(
    //     &self,
    //     dave_session: &AsyncMutex<Option<DaveSession>>,
    //     ssrc_to_user_id: &DashMap<Ssrc, SNOWFLAKE>,
    // ) -> Result<(Ssrc, &[i16]), Box<dyn Error + Send + Sync>> {
    //     let IncomingData {
    //         ref mut decoder,
    //         ref mut rtp_packet_buf,
    //         ref mut decoded_audio_buf,
    //     } = *self.incoming_call_data.lock().await;

    //     let n_bytes_recived = self.udp.recv(rtp_packet_buf).await?;
    //     let rtp_packet_buf = &rtp_packet_buf[..n_bytes_recived];

    //     let packet_type = DiscordPacketType::try_from(rtp_packet_buf[1]);

    //     match packet_type {
    //         Ok(DiscordPacketType::Voice) => {
    //             // Continue processing voice packet below
    //         }
    //         Ok(DiscordPacketType::RtcpSenderReport | DiscordPacketType::RtcpReceiverReport) => {
    //             // RTCP control packets - ignore silently
    //             return Err("Expected voice packet".into());
    //         }
    //         _ => {
    //             trace!("Unknown packet type on UDP: {:?}", rtp_packet_buf[1]);
    //             return Err("Expected voice packet".into());
    //         }
    //     }

    //     let rtp_packet = RtpPacket::new(rtp_packet_buf).ok_or("Failed to parse rtp packet")?;

    //     let is_rtp_extended = rtp_packet.get_extension() != 0;

    //     let rtp_header_len = if is_rtp_extended {
    //         rtp_packet.packet.len() - rtp_packet.payload().len() + 4
    //     } else {
    //         info!("None-extended");
    //         rtp_packet.packet.len() - rtp_packet.payload().len()
    //     };
    //     let (rtp_header, rtp_body) = rtp_packet.packet().split_at(rtp_header_len);

    //     let description = self.description.get().unwrap();
    //     let mode = description.mode().unwrap();
    //     let decrypted_payload = match mode {
    //         EncryptionMode::aead_aes256_gcm_rtpsize => unimplemented!(),
    //         EncryptionMode::aead_xchacha20_poly1305_rtpsize => {
    //             let (voice_payload, nonce_u32) =
    //                 rtp_body.split_at(rtp_body.len() - mode.nonce_size());

    //             let mut nonce = [0; 24];
    //             nonce[..mode.nonce_size()].copy_from_slice(nonce_u32);
    //             let nonce = crypto_aead::xchacha20poly1305::Nonce::from_bytes(nonce);

    //             let key = crypto_aead::xchacha20poly1305::Key::from_bytes(
    //                 description.secret_key().unwrap(),
    //             )
    //             .expect("Invalid key length");

    //             crypto_aead::xchacha20poly1305::decrypt(
    //                 voice_payload,
    //                 Some(rtp_header),
    //                 &nonce,
    //                 &key,
    //             )?
    //         }
    //         EncryptionMode::aead_aes256_gcm => unimplemented!("Depricated"),
    //         EncryptionMode::xsalsa20_poly1305 => unimplemented!("Depricated"),
    //         EncryptionMode::xsalsa20_poly1305_suffix => unimplemented!("Depricated"),
    //         EncryptionMode::xsalsa20_poly1305_lite => unimplemented!("Depricated"),
    //         EncryptionMode::xsalsa20_poly1305_lite_rtpsize => unimplemented!("Depricated"),
    //     };

    //     // <https://datatracker.ietf.org/doc/html/rfc6464>
    //     let (potentially, voice_data) = decrypted_payload.split_at(8);
    //     let unkown_const = &potentially[..1]; // CONST 55
    //     // let timecode = &potentially[1..4]; // Timecode
    //     let unkown_const_2 = &potentially[4..5]; // CONST 16
    //     // let avrage_volume = &potentially[5..6]; // Avrage volume of the frame?
    //     let unkown_const_3 = &potentially[6..7]; // CONST 144
    //     let channels = &potentially[7]; // Channels?
    //     if unkown_const != [50] {
    //         trace!("RTP extension byte 0 unexpected: {:?}", unkown_const);
    //     }
    //     if unkown_const_2 != [16] {
    //         trace!("RTP extension byte 4 unexpected: {:?}", unkown_const_2);
    //     }
    //     if unkown_const_3 != [144] {
    //         trace!("RTP extension byte 6 unexpected: {:?}", unkown_const_3);
    //     }

    //     let voice_data = if rtp_packet.get_padding() == 1
    //         && let Some(last_byte) = voice_data.last()
    //     {
    //         &voice_data[..voice_data.len() - *last_byte as usize]
    //     } else {
    //         voice_data
    //     };
    //     // Decrypt Dave
    //     let decrypted;
    //     let mut dave_session = dave_session.lock().await;
    //     let voice_data = if let Some(dave_session) = dave_session.as_mut() {
    //         decrypted = dave_session.decrypt(
    //             *ssrc_to_user_id
    //                 .get(&rtp_packet.get_ssrc())
    //                 .ok_or("No mapping of ssrc to user_id TODO: Make this better")?,
    //             MediaType::AUDIO,
    //             voice_data,
    //         )?;
    //         decrypted.as_slice()
    //     } else {
    //         voice_data
    //     };

    //     // Decode opus
    //     let n_decoded_samples = decoder.decode(voice_data, decoded_audio_buf, false)?;

    //     Ok((
    //         rtp_packet.get_ssrc(),
    //         &decoded_audio_buf[..n_decoded_samples * VOICE_CHANNELS],
    //     ))
    // }

    // pub async fn send_audio_frame(
    //     &self,
    //     samples: &[AudioSampleType],
    //     dave_session: Option<&mut DaveSession>,
    // ) -> Result<(), Box<dyn Error + Send + Sync>> {
    //     if samples.is_empty() {
    //         warn!("Noting to send");
    //         return Ok(());
    //     }
    //     if !samples.len().is_multiple_of(2) {
    //         return Err("Expected interleaved stereo samples".into());
    //     }

    //     let description = self
    //         .description
    //         .get()
    //         .ok_or("Missing session description")?;
    //     let mode = description.mode().ok_or("Missing encryption mode")?;
    //     let secret_key = description.secret_key().ok_or("Missing secret key")?;

    //     let OutcomingData {
    //         ref mut encoder,
    //         ref mut sequence,
    //         ref mut nonce,
    //         ref mut rtp_packet_buf,
    //         ref mut encoded_audio_buf,
    //     } = *self.outcoming_call_data.lock().await;
    //     let encoded_len = encoder
    //         .encode_float(samples, encoded_audio_buf.as_mut_slice())
    //         .map_err(|err| format!("Opus encode failed: {err:?}"))?;
    //     let opus_payload = &encoded_audio_buf[..encoded_len];

    //     let samples_per_channel = (samples.len() / VOICE_CHANNELS) as u32;
    //     let now = Instant::now();

    //     let mut call_data = self.call_data.lock().await;
    //     // Hybrid timestamp approach: sample-based during active speech,
    //     // but account for silence gaps based on elapsed time
    //     if let Some(last_time) = call_data.last_send_time {
    //         let elapsed = now.duration_since(last_time);
    //         let expected_frame_duration = std::time::Duration::from_micros(
    //             (samples_per_channel * 1_000_000 / VOICE_FREQUANCY as u32) as u64,
    //         );

    //         // If gap is significantly longer than expected frame time, we had silence
    //         // Advance timestamp to account for the silence gap
    //         if elapsed > expected_frame_duration * 2 {
    //             let silence_samples =
    //                 ((elapsed.as_secs_f64() * 48000.0) as u32) - samples_per_channel;
    //             call_data.timestamp += silence_samples;
    //         }
    //     }

    //     let old_timestamp = call_data.timestamp;
    //     let old_sequence = sequence.to_owned();

    //     call_data.timestamp += samples_per_channel;
    //     *sequence += 1;
    //     call_data.last_send_time = Some(now);

    //     const RTP_HEADER_LEN: usize = 12;
    //     rtp_packet_buf[0] = 0x80;
    //     rtp_packet_buf[1] = DiscordPacketType::Voice as u8;
    //     rtp_packet_buf[2..4].copy_from_slice(&u16::from(old_sequence).to_be_bytes());
    //     rtp_packet_buf[4..8].copy_from_slice(&u32::from(old_timestamp).to_be_bytes());
    //     rtp_packet_buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());

    //     match mode {
    //         EncryptionMode::aead_xchacha20_poly1305_rtpsize => {
    //             let nonce_u32 = nonce.to_be_bytes();
    //             *nonce = nonce.wrapping_add(1);

    //             let mut nonce = [0u8; 24];
    //             nonce[..mode.nonce_size()].copy_from_slice(&nonce_u32);
    //             let nonce = crypto_aead::xchacha20poly1305::Nonce::from_bytes(nonce);
    //             let key = crypto_aead::xchacha20poly1305::Key::from_bytes(&secret_key[..])
    //                 .expect("Invalid key length");

    //             let dave_encrypted;
    //             let audio_payload = if let Some(dave_session) = dave_session {
    //                 dave_encrypted =
    //                     dave_session.encrypt(MediaType::AUDIO, Codec::OPUS, opus_payload)?;
    //                 dave_encrypted.iter().as_slice()
    //             } else {
    //                 opus_payload
    //             };

    //             let encrypted = crypto_aead::xchacha20poly1305::encrypt(
    //                 audio_payload,
    //                 Some(rtp_packet_buf),
    //                 &nonce,
    //                 &key,
    //             )?;

    //             rtp_packet_buf[RTP_HEADER_LEN..RTP_HEADER_LEN + encrypted.len()]
    //                 .copy_from_slice(&encrypted);
    //             rtp_packet_buf[RTP_HEADER_LEN + encrypted.len()
    //                 ..RTP_HEADER_LEN + encrypted.len() + nonce_u32.len()]
    //                 .copy_from_slice(&nonce_u32);

    //             self.udp
    //                 .send(&rtp_packet_buf[..RTP_HEADER_LEN + encrypted.len() + nonce_u32.len()])
    //                 .await?;
    //         }
    //         _ => {
    //             return Err(format!("Unsupported encryption mode: {:?}", mode).into());
    //         }
    //     }

    //     Ok(())
    // }
}

#[derive(Debug, PartialEq, TryFromPrimitive)]
#[repr(u8)]
enum DiscordPacketType {
    Voice = 0x78,
    /// RTCP Sender Report
    RtcpSenderReport = 200,
    /// RTCP Receiver Report
    RtcpReceiverReport = 201,
}

// === RTP defenitions ===
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct WrapU16(pub Wrapping<u16>);

impl WrapU16 {
    #[must_use]
    pub fn new(v: u16be) -> Self {
        Self(Wrapping(v))
    }
}

impl From<WrapU16> for u16 {
    fn from(val: WrapU16) -> Self {
        (val.0).0
    }
}

impl From<u16> for WrapU16 {
    fn from(val: u16) -> Self {
        WrapU16(Wrapping(val))
    }
}

impl PrimitiveValues for WrapU16 {
    type T = (u16be,);

    fn to_primitive_values(&self) -> Self::T {
        ((*self).into(),)
    }
}

impl Add<u16> for WrapU16 {
    type Output = Self;

    fn add(self, other: u16) -> Self::Output {
        WrapU16(self.0 + Wrapping(other))
    }
}

impl AddAssign<u16> for WrapU16 {
    fn add_assign(&mut self, other: u16) {
        self.0 += Wrapping(other);
    }
}

impl Sub<u16> for WrapU16 {
    type Output = Self;

    fn sub(self, other: u16) -> Self::Output {
        WrapU16(self.0 - Wrapping(other))
    }
}

impl SubAssign<u16> for WrapU16 {
    fn sub_assign(&mut self, other: u16) {
        self.0 -= Wrapping(other);
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct WrapU32(pub Wrapping<u32>);

impl WrapU32 {
    #[must_use]
    pub fn new(v: u32be) -> Self {
        Self(Wrapping(v))
    }
}

impl From<WrapU32> for u32 {
    fn from(val: WrapU32) -> Self {
        (val.0).0
    }
}

impl From<u32> for WrapU32 {
    fn from(val: u32) -> Self {
        WrapU32(Wrapping(val))
    }
}

impl PrimitiveValues for WrapU32 {
    type T = (u32be,);

    fn to_primitive_values(&self) -> Self::T {
        ((*self).into(),)
    }
}

impl Add<u32> for WrapU32 {
    type Output = Self;

    fn add(self, other: u32) -> Self::Output {
        WrapU32(self.0 + Wrapping(other))
    }
}

impl AddAssign<u32> for WrapU32 {
    fn add_assign(&mut self, other: u32) {
        self.0 += Wrapping(other);
    }
}

impl Sub<u32> for WrapU32 {
    type Output = Self;

    fn sub(self, other: u32) -> Self::Output {
        WrapU32(self.0 - Wrapping(other))
    }
}

impl SubAssign<u32> for WrapU32 {
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
    pub sequence: WrapU16,
    #[construct_with(u32be)]
    pub timestamp: WrapU32,
    pub ssrc: u32be,
    #[length = "csrc_count"]
    pub csrc_list: Vec<u32be>,
    #[payload]
    pub payload: Vec<u8>,
}
