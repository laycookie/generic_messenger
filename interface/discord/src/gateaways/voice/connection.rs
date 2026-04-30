use std::{error::Error, sync::OnceLock, time::Instant};

use dashmap::DashMap;
use davey::{Codec, DaveSession, MediaType};
use futures::lock::Mutex as AsyncMutex;
use libsodium_rs::crypto_aead;
use pnet_macros_support::packet::Packet;
use simple_audio_channels::AudioSampleType;
use smol::net::UdpSocket;
use tracing::{info, trace, warn};

use crate::api_types::SNOWFLAKE;
use rtp::{DiscordPacketType, RtpPacket, WrapU16, WrapU32};

pub mod encryption;
pub mod rtp;

pub use encryption::{EncryptionMode, SessionDescription};
pub use rtp::Ssrc;

pub const VOICE_FREQUANCY: usize = 48_000;
pub const VOICE_CHANNELS: usize = 2; // Stereo
pub const VOICE_FRAME_SAMPLES: usize = 960 * VOICE_CHANNELS;

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
            rtp_packet.packet().len() - rtp_packet.payload().len() + 4
        } else {
            info!("None-extended");
            rtp_packet.packet().len() - rtp_packet.payload().len()
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
}
