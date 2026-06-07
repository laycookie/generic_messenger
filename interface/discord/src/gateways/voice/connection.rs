//! Discord voice connection: RTP send/receive over UDP.
//!
//! Relevant RFCs (referenced by name throughout this file):
//! - [RFC 3550] — RTP: A Transport Protocol for Real-Time Applications
//! - [RFC 5285] — A General Mechanism for RTP Header Extensions
//! - [RFC 6464] — RTP Header Extension for Client-to-Mixer Audio Level Indication
//!
//! [RFC 3550]: https://datatracker.ietf.org/doc/html/rfc3550
//! [RFC 5285]: https://datatracker.ietf.org/doc/html/rfc5285
//! [RFC 6464]: https://datatracker.ietf.org/doc/html/rfc6464

use std::{error::Error, io, sync::OnceLock, time::Instant};

use dashmap::DashMap;
use davey::{Codec, DaveSession, MediaType};
use futures::lock::Mutex as AsyncMutex;
use libsodium_rs::crypto_aead;
use pnet_macros_support::packet::Packet;
use simple_audio_channels::AudioSampleType;
use smol::net::UdpSocket;
use tracing::{debug, trace, warn};

use crate::api_types::SNOWFLAKE;
use rtp::{OPUS_PAYLOAD_TYPE, PacketClass, RtpPacket, WrapU16, WrapU32};

pub mod encryption;
pub mod rtp;

pub use encryption::{EncryptionMode, SessionDescription};
pub use rtp::{RtcpType, Ssrc};

/// A classified and (for voice) decoded UDP packet.
pub enum UdpPacket<'a> {
    /// Decoded Opus audio frame.
    Voice { ssrc: Ssrc, samples: &'a [i16] },
    /// RTCP control packet.
    Rtcp(RtcpType),
    /// RTP packet with an unhandled payload type (e.g., video).
    UnhandledRtp { ssrc: Ssrc, payload_type: u8 },
}

pub const VOICE_FREQUENCY: usize = 48_000;
pub const VOICE_CHANNELS: usize = 2; // Stereo
pub const VOICE_FRAME_SAMPLES: usize = 960 * VOICE_CHANNELS;

const RTP_HEADER_LEN: usize = 12;

/// RFC 5285 extension elements Discord emits on Opus voice packets.
/// Discriminants are the RFC 5285 element IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum RtpExtension {
    /// RFC 6464 audio level indication.
    AudioLevel = 1,
    /// Discord-specific (purpose unconfirmed; possibly timing).
    Timecode = 3,
    /// Discord-specific (purpose unconfirmed; possibly channel info).
    Channels = 9,
}

impl RtpExtension {
    const fn from_id(id: u8) -> Option<Self> {
        Some(match id {
            1 => Self::AudioLevel,
            3 => Self::Timecode,
            9 => Self::Channels,
            _ => return None,
        })
    }

    /// Expected size of the data portion (excluding the sub-header byte).
    const fn expected_data_len(self) -> usize {
        match self {
            Self::AudioLevel => 1,
            Self::Timecode => 3,
            Self::Channels => 1,
        }
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
    pub async fn recv(&mut self) -> Result<UdpPacket<'_>, Box<dyn Error>> {
        let n_bytes_received = self.udp.recv(&mut self.rtp_packet_buf).await?;
        if n_bytes_received < 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "UDP packet too short").into());
        }
        let rtp_packet_buf = &self.rtp_packet_buf[..n_bytes_received];

        match PacketClass::classify(rtp_packet_buf[1]) {
            PacketClass::Rtcp(rtcp_type) => return Ok(UdpPacket::Rtcp(rtcp_type)),
            PacketClass::Rtp { payload_type, .. } if payload_type != OPUS_PAYLOAD_TYPE => {
                let ssrc = if rtp_packet_buf.len() >= RTP_HEADER_LEN {
                    u32::from_be_bytes(rtp_packet_buf[8..RTP_HEADER_LEN].try_into().unwrap())
                } else {
                    0
                };
                return Ok(UdpPacket::UnhandledRtp { ssrc, payload_type });
            }
            PacketClass::Rtp { .. } => {} // Opus voice — decode below
        }

        let rtp_packet = RtpPacket::new(rtp_packet_buf).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "failed to parse RTP packet")
        })?;

        let is_rtp_extended = rtp_packet.get_extension() != 0;

        let rtp_header_len = if is_rtp_extended {
            rtp_packet.packet().len() - rtp_packet.payload().len() + 4
        } else {
            debug!("None-extended");
            rtp_packet.packet().len() - rtp_packet.payload().len()
        };
        let (rtp_header, rtp_body) = rtp_packet.packet().split_at(rtp_header_len);

        let mode = self
            .description
            .mode()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing encryption mode"))?;
        let decrypted_payload = match mode {
            EncryptionMode::aead_aes256_gcm_rtpsize => unimplemented!(),
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => {
                let (voice_payload, nonce_u32) =
                    rtp_body.split_at(rtp_body.len() - mode.nonce_size());

                let mut nonce = [0; 24];
                nonce[..mode.nonce_size()].copy_from_slice(nonce_u32);
                let nonce = crypto_aead::xchacha20poly1305::Nonce::from_bytes(nonce);

                let key = crypto_aead::xchacha20poly1305::Key::from_bytes(
                    self.description.secret_key().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "missing secret key")
                    })?,
                )
                .expect("Invalid key length");

                crypto_aead::xchacha20poly1305::decrypt(
                    voice_payload,
                    Some(rtp_header),
                    &nonce,
                    &key,
                )?
            }
            EncryptionMode::aead_aes256_gcm => unimplemented!("Deprecated"),
            EncryptionMode::xsalsa20_poly1305 => unimplemented!("Deprecated"),
            EncryptionMode::xsalsa20_poly1305_suffix => unimplemented!("Deprecated"),
            EncryptionMode::xsalsa20_poly1305_lite => unimplemented!("Deprecated"),
            EncryptionMode::xsalsa20_poly1305_lite_rtpsize => unimplemented!("Deprecated"),
        };

        // RFC 5285 one-byte RTP header extensions live at the start of the
        // decrypted payload. The block size is given by the length field
        // (in 32-bit words) of the 0xBEDE preamble — the last 2 bytes of
        // rtp_header when the X bit is set.
        // Each sub-header byte encodes ID:4 | (length-1):4 followed by
        // `length` bytes of data. ID=0 is alignment padding; ID=15 is
        // reserved and terminates parsing.
        let ext_size_bytes = if is_rtp_extended {
            // The last 4 bytes of rtp_header are the RFC 5285 extension
            // preamble: profile (2 bytes) + length-in-32-bit-words (2 bytes).
            let &[profile_hi, profile_lo, length_hi, length_lo] = rtp_header
                .last_chunk::<4>()
                .expect("X bit set ⇒ rtp_header includes the 4-byte preamble");
            let ext_profile = u16::from_be_bytes([profile_hi, profile_lo]);
            let ext_length_words = u16::from_be_bytes([length_hi, length_lo]);
            if ext_profile != 0xBEDE {
                trace!(
                    "RTP extension profile {ext_profile:#06x} is not RFC 5285 one-byte form (0xBEDE); parser may yield garbage"
                );
            }
            usize::from(ext_length_words) * 4
        } else {
            0
        };
        if decrypted_payload.len() < ext_size_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "decrypted payload {} bytes < declared RTP extension size {ext_size_bytes}",
                    decrypted_payload.len()
                ),
            )
            .into());
        }
        let (rtp_extensions, voice_data) = decrypted_payload.split_at(ext_size_bytes);

        let mut timecode = None; // ID=3, Discord-specific (purpose unconfirmed)
        let mut audio_level = None; // ID=1, RFC 6464: V:1 | level:7
        let mut channels = None; // ID=9, Discord-specific (purpose unconfirmed)
        let mut cursor = rtp_extensions;
        while !cursor.is_empty() {
            let subheader = cursor[0];
            let id = subheader >> 4;
            if id == 0 {
                // alignment padding per RFC 5285 §4.2
                cursor = &cursor[1..];
                continue;
            }
            if id == 15 {
                // reserved; terminates extension processing per RFC 5285 §4.2
                // Maybe consider just erroring here, if we get an invalide RTP header?
                break;
            }
            let data_len = ((subheader & 0x0F) as usize) + 1;
            let total_len = 1 + data_len;
            if total_len > cursor.len() {
                trace!(
                    "Truncated RFC 5285 sub-header: ID={id} claims {data_len} data bytes, only {} available",
                    cursor.len() - 1
                );
                break;
            }
            let data = &cursor[1..total_len];
            match RtpExtension::from_id(id) {
                Some(ext) if ext.expected_data_len() == data_len => match ext {
                    RtpExtension::AudioLevel => audio_level = Some(data[0]),
                    RtpExtension::Timecode => timecode = Some(data),
                    RtpExtension::Channels => channels = Some(data[0]),
                },
                Some(ext) => trace!(
                    "RFC 5285 sub-header {ext:?}: expected {} data bytes, got {data_len}",
                    ext.expected_data_len()
                ),
                None => {
                    trace!("Unknown RFC 5285 sub-header: ID={id}, len={data_len}, data={data:02x?}")
                }
            }
            cursor = &cursor[total_len..];
        }
        trace!(
            "RTP extensions parsed: timecode={timecode:02x?} audio_level={audio_level:?} channels={channels:?}"
        );

        // RTP padding per RFC 3550 §5.1: when the P bit is set, the last
        // octet of the payload counts padding bytes (including itself).
        let voice_data = if rtp_packet.get_padding() == 1
            && let Some(last_byte) = voice_data.last()
        {
            // TODO: overflow was previously reported here. Now that the
            // extension block is sized from the preamble, any remaining
            // overflow means Discord set the P bit without real RFC 3550
            // padding bytes. Use checked_sub to drop the packet rather
            // than panic if this still triggers.
            trace!(
                "RTP padding strip: voice_data.len()={}, padding_count={}",
                voice_data.len(),
                last_byte
            );
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
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "no SSRC to user mapping")
                    })?,
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

        Ok(UdpPacket::Voice {
            ssrc: rtp_packet.get_ssrc(),
            samples: &self.decoded_audio_buf[..n_decoded_samples * VOICE_CHANNELS],
        })
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
            warn!("Nothing to send");
            return Ok(());
        }
        if !samples.len().is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected interleaved stereo samples",
            )
            .into());
        }

        let mode = self
            .description
            .mode()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing encryption mode"))?;
        let secret_key = self
            .description
            .secret_key()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing secret key"))?;

        let encoded_len = self
            .encoder
            .encode_float(samples, self.encoded_audio_buf.as_mut_slice())
            .map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("opus encode failed: {err:?}"),
                )
            })?;
        let opus_payload = &self.encoded_audio_buf[..encoded_len];

        let samples_per_channel = (samples.len() / VOICE_CHANNELS) as u32;
        let now = Instant::now();

        // Hybrid timestamp approach: sample-based during active speech,
        // but account for silence gaps based on elapsed time
        if let Some(last_time) = self.last_send_time {
            let elapsed = now.duration_since(last_time);
            let expected_frame_duration = std::time::Duration::from_micros(
                // TODO: Get rid of 1M to convert micros to secs
                (samples_per_channel * 1_000_000 / VOICE_FREQUENCY as u32) as u64,
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

        let rtp_header = &mut self.rtp_packet_buf[0..RTP_HEADER_LEN];
        rtp_header[0] = 0x80;
        rtp_header[1] = OPUS_PAYLOAD_TYPE;
        rtp_header[2..4].copy_from_slice(&u16::from(sequence).to_be_bytes());
        rtp_header[4..8].copy_from_slice(&u32::from(timestamp).to_be_bytes());
        rtp_header[8..RTP_HEADER_LEN].copy_from_slice(&self.ssrc.to_be_bytes());

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
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("unsupported encryption mode: {mode:?}"),
                )
                .into());
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
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no session description provided",
            )
            .into());
        };

        Ok((
            RecvAudioFuture {
                udp: &self.udp,
                description,
                dave_session,
                ssrc_to_user_id,
                rtp_packet_buf: [0; 1024],
                decoder: opus::Decoder::new(VOICE_FREQUENCY as u32, opus::Channels::Stereo)?,
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
                    VOICE_FREQUENCY as u32,
                    opus::Channels::Stereo,
                    opus::Application::Voip,
                )?,
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
