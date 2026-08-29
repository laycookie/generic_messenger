//! Discord voice connection: RTP send/receive over UDP.
//!
//! Discord transport documentation:
//! - <https://docs.discord.food/topics/voice-connections#udp-connections>
//! - <https://docs.discord.food/topics/voice-connections#sending-and-receiving-media>
//!
//! Relevant RFCs (referenced by name throughout this file):
//! - [RFC 3550] — RTP: A Transport Protocol for Real-Time Applications
//! - [RFC 5285] — A General Mechanism for RTP Header Extensions
//! - [RFC 6464] — RTP Header Extension for Client-to-Mixer Audio Level Indication
//!
//! [RFC 3550]: https://datatracker.ietf.org/doc/html/rfc3550
//! [RFC 5285]: https://datatracker.ietf.org/doc/html/rfc5285
//! [RFC 6464]: https://datatracker.ietf.org/doc/html/rfc6464

use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    error::Error,
    io,
    num::NonZeroU16,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use davey::{Codec, DaveSession, MediaType};
use futures::lock::Mutex as AsyncMutex;
use futures_timer::Delay;
use libsodium_rs::crypto_aead;
use messenger_interface::interface::{VideoBroadcastKind, VideoFrame};
use pnet_macros_support::packet::Packet;
use simple_audio_channels::AudioSampleType;
use smol::net::UdpSocket;
use tracing::{debug, trace, warn};

#[cfg(test)]
use messenger_interface::interface::PixelData;
use simple_video::{
    Av1Encoder, Av1EncoderUsage, EncodedVideoFrame, H264Encoder, H264EncoderUsage,
    MAX_H264_ACCESS_UNIT_BYTES, VideoCodec, VideoError, VideoFrameTiming,
};

use super::DiscordVideoCodec;
use crate::api::SNOWFLAKE;
use rtp::{
    AV1_PAYLOAD_TYPE, AV1_RTX_PAYLOAD_TYPE, H264_PAYLOAD_TYPE, H264_RTX_PAYLOAD_TYPE,
    OPUS_PAYLOAD_TYPE, PacketClass, RtpPacket, WrapU16, WrapU32,
};
use video::{VideoRecv, packetize_access_unit};

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

mod av1;
pub mod encryption;
pub mod rtp;
pub mod video;

pub use encryption::{EncryptionMode, SessionDescription};
pub use rtp::{RtcpType, Ssrc};

/// Defensive cap for per-SSRC codec state. Discord calls cannot approach this
/// count, while forged SSRC churn otherwise grows decoder maps for whole call.
const MAX_REMOTE_MEDIA_STREAMS: usize = 128;
const VIDEO_NACK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const VIDEO_CODEC_UNKNOWN: u8 = 0;
const VIDEO_CODEC_H264: u8 = 1;
const VIDEO_CODEC_AV1: u8 = 2;
const VIDEO_CODEC_VALUE_MASK: u64 = u8::MAX as u64;
const VIDEO_CODEC_GENERATION_INCREMENT: u64 = VIDEO_CODEC_VALUE_MASK + 1;
const DEFAULT_VIDEO_KEYFRAME_INTERVAL_MS: u32 = 2_000;

const fn video_codec_value(codec: DiscordVideoCodec) -> u8 {
    match codec {
        DiscordVideoCodec::H264 => VIDEO_CODEC_H264,
        DiscordVideoCodec::Av1 => VIDEO_CODEC_AV1,
    }
}

const fn video_codec_from_state(state: u64) -> Option<DiscordVideoCodec> {
    match (state & VIDEO_CODEC_VALUE_MASK) as u8 {
        VIDEO_CODEC_H264 => Some(DiscordVideoCodec::H264),
        VIDEO_CODEC_AV1 => Some(DiscordVideoCodec::Av1),
        _ => None,
    }
}

const fn next_video_codec_state(current: u64, codec_value: u8) -> u64 {
    current
        .wrapping_add(VIDEO_CODEC_GENERATION_INCREMENT)
        .wrapping_sub(current & VIDEO_CODEC_VALUE_MASK)
        .wrapping_add(codec_value as u64)
}

/// A classified and (for media) decoded UDP packet.
pub enum UdpPacket<'a> {
    /// Decoded Opus audio frame.
    Voice { ssrc: Ssrc, samples: &'a [i16] },
    /// Decoded RGBA video frame (one H264 access unit's worth).
    Video { ssrc: Ssrc, frame: VideoFrame },
    /// RTCP control packet, optionally carrying actionable video feedback.
    Rtcp {
        packet_type: RtcpType,
        feedback: Option<RtcpFeedback>,
    },
    /// Consumed without output: a mid-frame video packet, a frame the
    /// decoder buffered (awaiting a keyframe), or an RTX packet that repaired
    /// reorder state without completing a frame.
    Consumed,
    /// RTP packet with an unhandled payload type.
    UnhandledRtp { ssrc: Ssrc, payload_type: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtcpFeedback {
    /// PSFB PLI (FMT=1) or FIR (FMT=4): receiver requests an immediate
    /// intra frame because its current references are unusable.
    RequestKeyframe { media_ssrc: Ssrc },
    /// RTPFB Generic NACK (FMT=1): resend these primary-video sequence
    /// numbers through the stream's RTX SSRC.
    Retransmit {
        media_ssrc: Ssrc,
        sequences: Vec<u16>,
    },
    /// RFC 3550 receiver-report loss since previous report interval.
    ReceiverReport { reports: Vec<ReceptionReport> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceptionReport {
    pub media_ssrc: Ssrc,
    /// Lost fraction in 1/256 units.
    pub fraction_lost: u8,
}

pub const VOICE_FREQUENCY: usize = 48_000;
pub const VOICE_CHANNELS: usize = 2; // Stereo
pub const VOICE_FRAME_SAMPLES: usize = 960 * VOICE_CHANNELS;

const RTP_HEADER_LEN: usize = 12;

/// Clear portion of an RTP packet carrying RFC 5285 extensions: fixed RTP
/// header plus the 0xBEDE profile/length preamble. With Discord's RTP-size
/// AEAD modes, extension elements themselves are encrypted with the payload.
const RTP_EXTENDED_CLEAR_HEADER_LEN: usize = RTP_HEADER_LEN + 4;

/// Native UDP video RTP extensions required to make Discord's SFU/browser
/// path recognize a primary layer.
///
/// - ID 6: playout delay, 3 bytes. min=0 ms, max=100 ms (10 ms units).
/// - ID 11: primary RID, ASCII `100`.
///
/// Both elements exactly fill two 32-bit words, so no alignment padding is
/// needed. IDs come from Discord's native RTP extension map.
const VIDEO_RTP_EXTENSIONS: [u8; 8] = [
    0x62, 0x00, 0x00, 0x0A, // ID 6, length 3: min=0, max=10 (100 ms)
    0xB2, b'1', b'0', b'0', // ID 11, length 3: RID "100"
];

/// Screen content type belongs only on last packet of keyframes. WebRTC's
/// extension value 1 means screenshare; two zero bytes align to 32 bits.
const SCREEN_KEYFRAME_RTP_EXTENSIONS: [u8; 12] = [
    0x62, 0x00, 0x00, 0x0A, // ID 6: playout delay
    0x70, 0x01, // ID 7, length 1: screenshare
    0xB2, b'1', b'0', b'0', // ID 11: primary RID "100"
    0x00, 0x00, // alignment padding
];

/// RTX uses repaired RID (ID 12), never primary RID (ID 11).
const VIDEO_RTX_EXTENSIONS: [u8; 8] = [
    0x62, 0x00, 0x00, 0x0A, // ID 6: playout delay
    0xC2, b'1', b'0', b'0', // ID 12: repaired RID "100"
];

fn video_rtp_extensions(
    kind: VideoBroadcastKind,
    is_keyframe: bool,
    last_packet: bool,
) -> &'static [u8] {
    if kind == VideoBroadcastKind::Screen && is_keyframe && last_packet {
        &SCREEN_KEYFRAME_RTP_EXTENSIONS
    } else {
        &VIDEO_RTP_EXTENSIONS
    }
}

fn write_video_rtp_header(
    packet: &mut [u8],
    payload_type: u8,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    ssrc: Ssrc,
    extension_len: usize,
) -> (&mut [u8], &mut [u8]) {
    debug_assert!(extension_len.is_multiple_of(4));
    let (clear_header, rest) = packet.split_at_mut(RTP_EXTENDED_CLEAR_HEADER_LEN);
    clear_header[0] = 0x90; // RTP v2 + extension bit.
    clear_header[1] = payload_type | if marker { 0x80 } else { 0 };
    clear_header[2..4].copy_from_slice(&sequence.to_be_bytes());
    clear_header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    clear_header[8..RTP_HEADER_LEN].copy_from_slice(&ssrc.to_be_bytes());
    clear_header[RTP_HEADER_LEN..RTP_HEADER_LEN + 2].copy_from_slice(&0xBEDEu16.to_be_bytes());
    clear_header[RTP_HEADER_LEN + 2..RTP_EXTENDED_CLEAR_HEADER_LEN]
        .copy_from_slice(&(extension_len as u16 / 4).to_be_bytes());
    (clear_header, rest)
}

/// Build the 24-byte XChaCha20 nonce for an RTP-size AEAD op from its wire
/// prefix — the counter suffix a received packet carries, or our own 4-byte
/// send counter — zero-padded to full width. Every send and receive path
/// shares this so the padding convention can't drift between them.
fn rtpsize_nonce(prefix: &[u8]) -> crypto_aead::xchacha20poly1305::Nonce {
    let mut nonce = [0u8; 24];
    let len = prefix.len().min(nonce.len());
    nonce[..len].copy_from_slice(&prefix[..len]);
    crypto_aead::xchacha20poly1305::Nonce::from_bytes(nonce)
}

fn transport_key(
    description: &SessionDescription,
) -> io::Result<crypto_aead::xchacha20poly1305::Key> {
    Ok((*description.validated_secret_key()?).into())
}

/// Reserve one wire-sized transport nonce without permitting wraparound.
/// Counter uses one extra bit of state so `u32::MAX` remains usable once;
/// subsequent sends fail and force connection/key renewal.
fn next_transport_nonce(counter: &AtomicU64) -> Result<[u8; 4], io::Error> {
    let value = counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            (value <= u64::from(u32::MAX)).then_some(value + 1)
        })
        .map_err(|_| {
            io::Error::other("Discord media transport nonce exhausted; reconnect required")
        })?;
    let value =
        u32::try_from(value).expect("nonce reservation only succeeds inside the u32 wire range");
    Ok(value.to_be_bytes())
}

/// Split RTP-size AEAD body into ciphertext+tag and nonce suffix without
/// allowing malformed network input to underflow slice arithmetic.
fn split_rtpsize_body(body: &[u8], nonce_size: usize) -> Result<(&[u8], &[u8]), io::Error> {
    const AEAD_TAG_LEN: usize = 16;
    let encrypted_len = body
        .len()
        .checked_sub(nonce_size)
        .filter(|encrypted_len| *encrypted_len >= AEAD_TAG_LEN)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "encrypted RTP packet is shorter than its tag and nonce",
            )
        })?;
    Ok(body.split_at(encrypted_len))
}

/// Strip RFC 3550 payload padding after transport decryption. A zero count or
/// a count larger than the payload is malformed, not a reason to panic.
fn strip_rtp_padding(payload: &[u8], padded: bool) -> Result<&[u8], io::Error> {
    if !padded {
        return Ok(payload);
    }
    let count = usize::from(*payload.last().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "padded RTP payload is empty")
    })?);
    if count == 0 || count > payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid RTP padding count",
        ));
    }
    Ok(&payload[..payload.len() - count])
}

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

/// Borrowed transport state for receiving. Re-derived each loop round from
/// the freshly-loaded [`Connection`] (see [`Connection::recv_transport`]) so
/// it never keeps the connection's `Arc` alive across media coordination. The
/// owned decode state lives in [`RecvCodec`].
pub struct RecvTransport<'a> {
    udp: &'a UdpSocket,
    description: &'a ArcSwapOption<SessionDescription>,
    dave_session: &'a AsyncMutex<Option<DaveSession>>,
    /// Local RTP sender identity used in RTCP feedback packets.
    local_ssrc: Ssrc,
    /// Shared with every outbound packet on this connection. Receive-only
    /// streams still send RTCP feedback, so their PLI packets must draw from
    /// the same transport nonce sequence.
    nonce: &'a AtomicU64,
    ssrc_to_user_id: &'a DashMap<Ssrc, SNOWFLAKE>,
    /// Video SSRC → owner, from opcode 12 (Video) events; DAVE video
    /// decryption is keyed by the sender the same way audio is.
    video_ssrc_to_user_id: &'a DashMap<Ssrc, SNOWFLAKE>,
    /// RTX SSRC → primary video SSRC, also supplied by opcode 12.
    rtx_to_video_ssrc: &'a DashMap<Ssrc, Ssrc>,
}

/// Owned decode state for the whole call. Held by the audio session (not the
/// per-round transport) so a loop round never resets a decoder or reallocates
/// the shared buffers. Opus carries per-stream inter-frame state, so each
/// speaker (SSRC) gets its own decoder — created on first sight — rather than
/// sharing one, which would corrupt audio when people talk simultaneously.
pub struct RecvCodec {
    decoders: HashMap<Ssrc, opus::Decoder>,
    audio_last_active: HashMap<Ssrc, Instant>,
    /// Per-SSRC video reassembly + H264 decode state, created on first
    /// sight of a video packet from that SSRC.
    video: HashMap<Ssrc, VideoRecv>,
    video_last_active: HashMap<Ssrc, Instant>,
    /// Rate-limit repeated NACKs while a requested RTX packet is in flight.
    video_nack_sent: HashMap<(Ssrc, u16), Instant>,
    // Sized for a worst-case media packet: 12-byte RTP header + extensions
    // + payload (max Opus frame is 1276; video packets stay under the
    // ~1200-byte MTU budget) + AEAD tag + nonce suffix.
    rtp_packet_buf: [u8; 2048],
    // Opus can decode up to 120 ms @ 48 kHz stereo = 11520 samples.
    decoded_audio_buf: [i16; 11520],
}
impl RecvCodec {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            decoders: HashMap::new(),
            audio_last_active: HashMap::new(),
            video: HashMap::new(),
            video_last_active: HashMap::new(),
            video_nack_sent: HashMap::new(),
            rtp_packet_buf: [0; 2048],
            decoded_audio_buf: [0; 11520],
        })
    }
}

/// Make room for a newly observed SSRC by retiring the least recently active
/// decoder. The cap remains a memory bound under forged SSRC churn without
/// permanently rejecting legitimate streams that join later in a long call.
fn retire_lru_if_full<T>(
    states: &mut HashMap<Ssrc, T>,
    last_active: &mut HashMap<Ssrc, Instant>,
    incoming: Ssrc,
) -> Option<Ssrc> {
    if states.contains_key(&incoming) || states.len() < MAX_REMOTE_MEDIA_STREAMS {
        return None;
    }

    let retired = states
        .keys()
        .copied()
        // Missing activity metadata sorts first and repairs any inconsistent
        // state instead of turning the defensive cap into a permanent error.
        .min_by_key(|ssrc| (last_active.get(ssrc).copied(), *ssrc))?;
    states.remove(&retired);
    last_active.remove(&retired);
    Some(retired)
}

impl RecvTransport<'_> {
    /// Ask the sender for an H264 keyframe using RTCP PSFB PLI (RFC 4585).
    ///
    /// Joining a running stream commonly starts on a delta frame. OpenH264
    /// reports `dsNoParamSets` until the next IDR carries SPS/PPS; requesting
    /// one immediately avoids leaving the UI in its pending state.
    async fn send_keyframe_request(
        &self,
        description: &SessionDescription,
        media_ssrc: Ssrc,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mode = description
            .mode()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing encryption mode"))?;
        if !matches!(mode, EncryptionMode::aead_xchacha20_poly1305_rtpsize) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported encryption mode: {mode:?}"),
            )
            .into());
        }
        let key = transport_key(description)?;

        // RTCP PSFB PLI: V=2, FMT=1, PT=206, length=2, sender SSRC,
        // media SSRC. RTP-size encryption leaves common header + sender SSRC
        // clear and encrypts the media SSRC.
        let mut header = [0u8; 8];
        header[0] = 0x81;
        header[1] = RtcpType::PayloadFeedback as u8;
        header[2..4].copy_from_slice(&2u16.to_be_bytes());
        header[4..8].copy_from_slice(&self.ssrc().to_be_bytes());

        let nonce_u32 = next_transport_nonce(self.nonce)?;
        let nonce = rtpsize_nonce(&nonce_u32);
        let encrypted = crypto_aead::xchacha20poly1305::encrypt(
            &media_ssrc.to_be_bytes(),
            Some(&header),
            &nonce,
            &key,
        )?;

        let mut packet = Vec::with_capacity(header.len() + encrypted.len() + nonce_u32.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&encrypted);
        packet.extend_from_slice(&nonce_u32);
        self.udp.send(&packet).await?;
        debug!("Sent RTCP PLI for video SSRC {media_ssrc}");
        Ok(())
    }

    /// Request retransmission of missing primary H264 packets with RTPFB
    /// Generic NACK (RFC 4585). Reorder state exposes at most one PID/BLP
    /// window (17 sequence numbers), keeping feedback bounded.
    async fn send_video_nack(
        &self,
        description: &SessionDescription,
        media_ssrc: Ssrc,
        missing: &[u16],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let Some((&packet_id, rest)) = missing.split_first() else {
            return Ok(());
        };
        let mode = description
            .mode()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing encryption mode"))?;
        if !matches!(mode, EncryptionMode::aead_xchacha20_poly1305_rtpsize) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported encryption mode: {mode:?}"),
            )
            .into());
        }
        let key = transport_key(description)?;

        let mut lost_mask = 0u16;
        for sequence in rest {
            let offset = sequence.wrapping_sub(packet_id);
            if (1..=16).contains(&offset) {
                lost_mask |= 1 << (offset - 1);
            }
        }

        // RTPFB: V=2/FMT=1, PT=205, sender SSRC remains clear. Encrypted body
        // contains media SSRC followed by one Generic NACK PID/BLP block.
        let mut header = [0u8; 8];
        header[0] = 0x81;
        header[1] = RtcpType::TransportFeedback as u8;
        header[2..4].copy_from_slice(&3u16.to_be_bytes());
        header[4..8].copy_from_slice(&self.ssrc().to_be_bytes());
        let mut body = [0u8; 8];
        body[..4].copy_from_slice(&media_ssrc.to_be_bytes());
        body[4..6].copy_from_slice(&packet_id.to_be_bytes());
        body[6..8].copy_from_slice(&lost_mask.to_be_bytes());

        let nonce_u32 = next_transport_nonce(self.nonce)?;
        let nonce = rtpsize_nonce(&nonce_u32);
        let encrypted =
            crypto_aead::xchacha20poly1305::encrypt(&body, Some(&header), &nonce, &key)?;
        let mut packet = Vec::with_capacity(header.len() + encrypted.len() + nonce_u32.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&encrypted);
        packet.extend_from_slice(&nonce_u32);
        self.udp.send(&packet).await?;
        trace!(
            ?missing,
            "Sent RTCP Generic NACK for video SSRC {media_ssrc}"
        );
        Ok(())
    }

    fn ssrc(&self) -> Ssrc {
        // Sender SSRC for receiver feedback is this connection's local audio
        // SSRC. It is stored in the clear sender field and need not identify a
        // video source.
        self.local_ssrc
    }

    fn parse_receiver_reports(body: &[u8], report_count: u8) -> Vec<ReceptionReport> {
        const REPORT_BLOCK_LEN: usize = 24;
        body.chunks_exact(REPORT_BLOCK_LEN)
            .take(usize::from(report_count))
            .map(|report| ReceptionReport {
                media_ssrc: u32::from_be_bytes(report[..4].try_into().unwrap()),
                fraction_lost: report[4],
            })
            .collect()
    }

    fn parse_rtcp(
        &self,
        packet: &[u8],
        packet_type: RtcpType,
        description: &SessionDescription,
    ) -> Result<UdpPacket<'static>, Box<dyn Error>> {
        if packet.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "RTCP packet too short").into());
        }

        let feedback = if matches!(
            packet_type,
            RtcpType::ReceiverReport
                | RtcpType::PayloadFeedback
                | RtcpType::TransportFeedback
                | RtcpType::ApplicationDefined
        ) {
            let mode = description.mode().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing encryption mode")
            })?;
            if !matches!(mode, EncryptionMode::aead_xchacha20_poly1305_rtpsize) {
                None
            } else {
                // RTP-size RTCP keeps common header + sender SSRC clear and
                // encrypts media SSRC + feedback control information.
                let nonce_size = mode.nonce_size().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "encryption mode has no RTP nonce",
                    )
                })?;
                if packet.len() < 8 + nonce_size + 16 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "encrypted RTCP packet too short",
                    )
                    .into());
                }
                let header = &packet[..8];
                let encrypted_end = packet.len() - nonce_size;
                let nonce = rtpsize_nonce(&packet[encrypted_end..]);
                let key = transport_key(description)?;
                let body = crypto_aead::xchacha20poly1305::decrypt(
                    &packet[8..encrypted_end],
                    Some(header),
                    &nonce,
                    &key,
                )?;
                let fmt = packet[0] & 0x1F;
                let media_ssrc = body
                    .get(..4)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u32::from_be_bytes)
                    .unwrap_or(0);
                match packet_type {
                    RtcpType::ReceiverReport => Some(RtcpFeedback::ReceiverReport {
                        reports: Self::parse_receiver_reports(&body, fmt),
                    }),
                    RtcpType::PayloadFeedback => match fmt {
                        1 | 4 => Some(RtcpFeedback::RequestKeyframe { media_ssrc }),
                        _ => {
                            debug!("RTCP PSFB fmt={fmt} media_ssrc={media_ssrc}");
                            None
                        }
                    },
                    RtcpType::TransportFeedback => (fmt == 1).then(|| RtcpFeedback::Retransmit {
                        media_ssrc,
                        sequences: parse_generic_nack(body.get(4..).unwrap_or_default()),
                    }),
                    RtcpType::ApplicationDefined => {
                        // APP: subtype in fmt; the encrypted body begins with
                        // the 4-ASCII-char name. Identification only — none
                        // of these are consumed yet.
                        let name: String = body
                            .iter()
                            .take(4)
                            .map(|&b| if b.is_ascii_graphic() { b as char } else { '.' })
                            .collect();
                        debug!(
                            "RTCP APP '{name}' subtype={fmt} ({} data bytes): {:02x?}",
                            body.len().saturating_sub(4),
                            &body[..body.len().min(24)]
                        );
                        None
                    }
                    _ => None,
                }
            }
        } else {
            None
        };

        Ok(UdpPacket::Rtcp {
            packet_type,
            feedback,
        })
    }

    pub async fn recv<'c>(
        &self,
        codec: &'c mut RecvCodec,
    ) -> Result<UdpPacket<'c>, Box<dyn Error>> {
        // Load per packet so a mid-call key rotation (new session
        // description) takes effect immediately.
        let description = self
            .description
            .load_full()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no session description"))?;
        let n_bytes_received = self.udp.recv(&mut codec.rtp_packet_buf).await?;
        if n_bytes_received < 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "UDP packet too short").into());
        }
        let rtp_packet_buf = &codec.rtp_packet_buf[..n_bytes_received];

        let payload_type = match PacketClass::classify(rtp_packet_buf[1]) {
            PacketClass::Rtcp(rtcp_type) => {
                return self.parse_rtcp(rtp_packet_buf, rtcp_type, &description);
            }
            PacketClass::Rtp { payload_type } => payload_type,
        };
        match payload_type {
            // Media handled below (parse + transport-decrypt first).
            OPUS_PAYLOAD_TYPE | H264_PAYLOAD_TYPE | H264_RTX_PAYLOAD_TYPE => {}
            other => {
                let ssrc = if rtp_packet_buf.len() >= RTP_HEADER_LEN {
                    u32::from_be_bytes(rtp_packet_buf[8..RTP_HEADER_LEN].try_into().unwrap())
                } else {
                    0
                };
                return Ok(UdpPacket::UnhandledRtp {
                    ssrc,
                    payload_type: other,
                });
            }
        }

        let rtp_packet = RtpPacket::new(rtp_packet_buf).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "failed to parse RTP packet")
        })?;

        let is_rtp_extended = rtp_packet.get_extension() != 0;

        let base_header_len = rtp_packet
            .packet()
            .len()
            .checked_sub(rtp_packet.payload().len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid RTP header"))?;
        let rtp_header_len = if is_rtp_extended {
            base_header_len.checked_add(4)
        } else {
            debug!("None-extended");
            Some(base_header_len)
        }
        .filter(|header_len| *header_len <= rtp_packet.packet().len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated RTP header"))?;
        let (rtp_header, rtp_body) = rtp_packet.packet().split_at(rtp_header_len);

        let mode = description
            .mode()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing encryption mode"))?;
        let decrypted_payload = match mode {
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => {
                let nonce_size = mode.nonce_size().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "encryption mode has no RTP nonce",
                    )
                })?;
                let (voice_payload, nonce_u32) = split_rtpsize_body(rtp_body, nonce_size)?;

                let nonce = rtpsize_nonce(nonce_u32);

                let key = transport_key(&description)?;

                crypto_aead::xchacha20poly1305::decrypt(
                    voice_payload,
                    Some(rtp_header),
                    &nonce,
                    &key,
                )?
            }
            unsupported => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("unsupported encryption mode: {unsupported:?}"),
                )
                .into());
            }
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
            let &[profile_hi, profile_lo, length_hi, length_lo] =
                rtp_header.last_chunk::<4>().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "extended RTP header has no extension preamble",
                    )
                })?;
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
        // Extension parsing is diagnostic-only. Keep it entirely outside the
        // media hot path unless packet-level tracing was explicitly enabled.
        if tracing::enabled!(tracing::Level::TRACE) {
            trace_rtp_extensions(rtp_extensions);
        }

        // RTP padding per RFC 3550 §5.1: when the P bit is set, the last
        // octet of the payload counts padding bytes (including itself).
        let voice_data = strip_rtp_padding(voice_data, rtp_packet.get_padding() == 1)?;
        let wire_ssrc = rtp_packet.get_ssrc();

        if matches!(payload_type, H264_PAYLOAD_TYPE | H264_RTX_PAYLOAD_TYPE) {
            let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
            let (ssrc, sequence, video_data) = if payload_type == H264_RTX_PAYLOAD_TYPE {
                let Some(ssrc) = self.rtx_to_video_ssrc.get(&wire_ssrc).map(|entry| *entry) else {
                    trace!("Ignoring RTX packet from unknown SSRC {wire_ssrc}");
                    return Ok(UdpPacket::Consumed);
                };
                let (original_sequence, payload) =
                    voice_data.split_at_checked(2).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "RTX payload has no original sequence",
                        )
                    })?;
                (
                    ssrc,
                    u16::from_be_bytes(original_sequence.try_into().unwrap()),
                    payload,
                )
            } else {
                (wire_ssrc, u16::from(rtp_packet.get_sequence()), voice_data)
            };
            let timestamp = u32::from(rtp_packet.get_timestamp());
            let marker = rtp_packet.get_marker() == 1;

            if let Some(retired) =
                retire_lru_if_full(&mut codec.video, &mut codec.video_last_active, ssrc)
            {
                codec
                    .video_nack_sent
                    .retain(|(source, _), _| *source != retired);
                debug!("Retired inactive video decoder for SSRC {retired}");
            }
            let new_video_ssrc = !codec.video.contains_key(&ssrc);
            if new_video_ssrc {
                codec.video.insert(ssrc, VideoRecv::new()?);
                if let Err(err) = self.send_keyframe_request(&description, ssrc).await {
                    warn!("Failed to request keyframe for video SSRC {ssrc}: {err}");
                }
            }
            codec.video_last_active.insert(ssrc, Instant::now());
            let assemble_started = timing_enabled.then(Instant::now);
            let outcome = codec
                .video
                .get_mut(&ssrc)
                .expect("video receive state was inserted above")
                .push_packet(sequence, timestamp, marker, video_data);
            let packet_assemble_us = assemble_started.map(elapsed_micros).unwrap_or_default();
            let assembly_age_us = outcome
                .assembly_age
                .map(duration_micros)
                .unwrap_or_default();
            let frame_started =
                (timing_enabled && outcome.access_unit.is_some()).then(Instant::now);

            // Missing snapshots repeat while reorder waits. Retry at a short,
            // bounded interval instead of emitting one NACK per received RTP
            // packet; clear repaired sequences immediately.
            let now = Instant::now();
            let missing = outcome.missing.as_slice();
            codec
                .video_nack_sent
                .retain(|(source, sequence), _| *source != ssrc || missing.contains(sequence));
            let nack = missing
                .iter()
                .copied()
                .filter(|sequence| {
                    codec
                        .video_nack_sent
                        .get(&(ssrc, *sequence))
                        .is_none_or(|sent| now.duration_since(*sent) >= VIDEO_NACK_RETRY_INTERVAL)
                })
                .collect::<Vec<_>>();
            for sequence in &nack {
                codec.video_nack_sent.insert((ssrc, *sequence), now);
            }
            if !nack.is_empty()
                && let Err(err) = self.send_video_nack(&description, ssrc, &nack).await
            {
                warn!("Failed to NACK missing video packets for SSRC {ssrc}: {err}");
            }

            // Collect until frame completes; most packets are reordered or
            // mid-frame and produce nothing.
            let Some(access_unit) = outcome.access_unit else {
                return Ok(UdpPacket::Consumed);
            };
            let video_recv = codec
                .video
                .get_mut(&ssrc)
                .expect("video receive state remains present");
            let decoder_id = video_recv.decoder_timing_id();
            let cadence = frame_started
                .map(|arrived_at| video_recv.observe_access_unit(timestamp, arrived_at))
                .unwrap_or_default();
            let receive_bookkeeping_us = frame_started.map(elapsed_micros).unwrap_or_default();

            // DAVE decrypts whole video frames — the sender encrypted the
            // frame before packetizing — so unlike audio (one packet = one
            // frame) it runs after reassembly. Same guard discipline as the
            // audio path: scope is just the decrypt.
            let dave_lock_started = timing_enabled.then(Instant::now);
            let mut dave_lock_contended = false;
            let dave_lock_wait_us;
            let dave_decrypt_us;
            let dave_decrypt_transform_us;
            let dave_decrypt_attempts;
            let dave_decrypted = {
                let mut dave_session = match self.dave_session.try_lock() {
                    Some(guard) => guard,
                    None => {
                        dave_lock_contended = true;
                        trace!("recv: waiting for dave_session lock (video)");
                        self.dave_session.lock().await
                    }
                };
                dave_lock_wait_us = dave_lock_started.map(elapsed_micros).unwrap_or_default();
                match dave_session.as_mut() {
                    Some(dave_session) => {
                        let Some(user_id) =
                            self.video_ssrc_to_user_id.get(&ssrc).map(|entry| *entry)
                        else {
                            video_recv.assembler.recycle(access_unit);
                            return Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                "no video SSRC to user mapping",
                            )
                            .into());
                        };
                        let stats_before = if timing_enabled {
                            dave_session
                                .get_decryption_stats(user_id, MediaType::VIDEO)
                                .ok()
                                .flatten()
                                .map(|stats| (stats.attempts, stats.duration))
                        } else {
                            None
                        };
                        let decrypt_started = timing_enabled.then(Instant::now);
                        let decrypt_result =
                            dave_session.decrypt(user_id, MediaType::VIDEO, &access_unit);
                        dave_decrypt_us = decrypt_started.map(elapsed_micros).unwrap_or_default();
                        let stats_after = if timing_enabled {
                            dave_session
                                .get_decryption_stats(user_id, MediaType::VIDEO)
                                .ok()
                                .flatten()
                                .map(|stats| (stats.attempts, stats.duration))
                        } else {
                            None
                        };
                        (dave_decrypt_attempts, dave_decrypt_transform_us) =
                            match (stats_before, stats_after) {
                                (
                                    Some((attempts_before, duration_before)),
                                    Some((attempts, duration)),
                                ) => (
                                    u64::from(attempts.wrapping_sub(attempts_before)),
                                    u64::from(duration.wrapping_sub(duration_before)),
                                ),
                                _ => (0, 0),
                            };
                        match decrypt_result {
                            Ok(decrypted) => Some(decrypted),
                            Err(err) => {
                                video_recv.assembler.recycle(access_unit);
                                return Err(err.into());
                            }
                        }
                    }
                    None => {
                        dave_decrypt_us = 0;
                        dave_decrypt_transform_us = 0;
                        dave_decrypt_attempts = 0;
                        None
                    }
                }
            };
            let dave_decrypt_preflight_us =
                dave_decrypt_us.saturating_sub(dave_decrypt_transform_us);
            let decode_started = timing_enabled.then(Instant::now);
            let decode_span = tracing::trace_span!(
                target: "video_timing",
                "incoming_frame",
                ssrc,
                rtp_timestamp = timestamp,
            );
            let decode_result = decode_span.in_scope(|| {
                video_recv.decode_frame(dave_decrypted.as_deref().unwrap_or(access_unit.as_slice()))
            });
            let decode_us = decode_started.map(elapsed_micros).unwrap_or_default();
            let output_gap_us = if matches!(&decode_result, Ok(Some(_))) {
                frame_started
                    .map(|_| video_recv.observe_output(Instant::now()))
                    .unwrap_or_default()
            } else {
                0
            };
            if let Some(frame_started) = frame_started {
                let (width, height) = match &decode_result {
                    Ok(Some(frame)) => (frame.width, frame.height),
                    Ok(None) | Err(_) => (0, 0),
                };
                tracing::trace!(
                    target: "video_timing",
                    stage = "incoming.receive",
                    ssrc,
                    decoder_id,
                    rtp_timestamp = timestamp,
                    stream_frame_index = cadence.stream_frame_index,
                    rtp_delta_ticks = cadence.rtp_delta_ticks,
                    source_interval_us = cadence.source_interval_us,
                    access_unit_gap_us = cadence.access_unit_gap_us,
                    output_gap_us,
                    access_unit_bytes = access_unit.len(),
                    decrypted_bytes = dave_decrypted.as_ref().map_or(0, Vec::len),
                    width,
                    height,
                    receive_bookkeeping_us,
                    packet_assemble_us,
                    assembly_age_us,
                    dave_lock_contended,
                    dave_lock_wait_us,
                    dave_decrypt_us,
                    dave_decrypt_preflight_us,
                    dave_decrypt_transform_us,
                    dave_decrypt_attempts,
                    decode_us,
                    total_us = elapsed_micros(frame_started),
                    emitted = matches!(&decode_result, Ok(Some(_))),
                    failed = decode_result.is_err(),
                    "video frame complete"
                );
            }

            return match decode_result {
                // Buffered without output (e.g. awaiting a keyframe).
                Ok(None) => {
                    video_recv.assembler.recycle(access_unit);
                    Ok(UdpPacket::Consumed)
                }
                Ok(Some(frame)) => {
                    video_recv.assembler.recycle(access_unit);
                    Ok(UdpPacket::Video { ssrc, frame })
                }
                Err(err) => {
                    // OpenH264 reports missing parameter sets and damaged
                    // access units as decode errors. Treat both as stream
                    // loss: preserve transport, reset this SSRC's decoder,
                    // and wait for the next keyframe instead of killing the
                    // media poll loop and retrying the same broken state.
                    debug!("Dropping undecodable H264 access unit for SSRC {ssrc}: {err}");
                    let reset = video_recv.reset();
                    video_recv.assembler.recycle(access_unit);
                    reset?;
                    if let Err(request_err) = self.send_keyframe_request(&description, ssrc).await {
                        warn!(
                            "Failed to request recovery keyframe for video SSRC {ssrc}: {request_err}"
                        );
                    }
                    Ok(UdpPacket::Consumed)
                }
            };
        }

        let ssrc = wire_ssrc;

        // Decrypt Dave. The guard's scope is just the decrypt — holding it
        // through the Opus decode below would block send_frame and the DAVE
        // event handlers for nothing. Uncontended acquisition is silent; the
        // trace fires only when someone else actually holds the lock.
        let decrypted;
        let voice_data = {
            let mut dave_session = match self.dave_session.try_lock() {
                Some(guard) => guard,
                None => {
                    trace!("recv: waiting for dave_session lock");
                    self.dave_session.lock().await
                }
            };
            if let Some(dave_session) = dave_session.as_mut() {
                decrypted = dave_session.decrypt(
                    *self.ssrc_to_user_id.get(&ssrc).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "no SSRC to user mapping")
                    })?,
                    MediaType::AUDIO,
                    voice_data,
                )?;
                decrypted.as_slice()
            } else {
                voice_data
            }
        };

        // Decode Opus with a per-SSRC decoder: Opus carries inter-frame state,
        // so feeding interleaved speakers through one decoder corrupts both.
        if let Some(retired) =
            retire_lru_if_full(&mut codec.decoders, &mut codec.audio_last_active, ssrc)
        {
            debug!("Retired inactive audio decoder for SSRC {retired}");
        }
        let decoder = match codec.decoders.entry(ssrc) {
            Entry::Occupied(decoder) => decoder.into_mut(),
            Entry::Vacant(slot) => slot.insert(opus::Decoder::new(
                VOICE_FREQUENCY as u32,
                opus::Channels::Stereo,
            )?),
        };
        codec.audio_last_active.insert(ssrc, Instant::now());
        let n_decoded_samples = decoder.decode(voice_data, &mut codec.decoded_audio_buf, false)?;

        Ok(UdpPacket::Voice {
            ssrc,
            samples: &codec.decoded_audio_buf[..n_decoded_samples * VOICE_CHANNELS],
        })
    }
}

fn parse_generic_nack(fci: &[u8]) -> Vec<u16> {
    const MAX_REQUESTED_PACKETS: usize = 256;
    let mut requested = Vec::new();
    for block in fci.chunks_exact(4) {
        let packet_id = u16::from_be_bytes([block[0], block[1]]);
        let lost = u16::from_be_bytes([block[2], block[3]]);
        requested.push(packet_id);
        for bit in 0..16 {
            if lost & (1 << bit) != 0 {
                requested.push(packet_id.wrapping_add(bit + 1));
            }
        }
        if requested.len() >= MAX_REQUESTED_PACKETS {
            requested.truncate(MAX_REQUESTED_PACKETS);
            break;
        }
    }
    requested
}

/// Parse and log the RFC 5285 one-byte RTP header extensions Discord attaches
/// to voice packets. Purely diagnostic — none of the parsed values
/// (timecode / audio level / channels) are used downstream — so it exists only
/// to keep that nesting out of [`RecvTransport::recv`]'s hot path.
fn trace_rtp_extensions(rtp_extensions: &[u8]) {
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
}

/// Per-SSRC RTP send state (audio). Lives on [`Connection`] (shared by every
/// per-round [`SendTransport`]) rather than being rebuilt with the transport
/// each round, so the RTP sequence number never regresses (receivers treat a
/// regression as a replay). The AEAD nonce is *not* here — it is shared across
/// audio and video (same key) via [`Connection::nonce`].
struct SendState {
    sequence: WrapU16,
    timestamp: WrapU32,
    last_send_time: Option<Instant>,
}

/// Per-SSRC RTP send state for outgoing video. Separate from [`SendState`]
/// because RTP sequence and timestamp are per-SSRC (video has its own SSRC and
/// a 90 kHz clock), but it draws AEAD nonces from the same shared
/// [`Connection::nonce`] so no nonce is ever reused under the session key.
struct VideoSendState {
    sequence: WrapU16,
    rtx_sequence: WrapU16,
    /// Wall-clock origin for the 90 kHz RTP timestamp; set on the first frame.
    start: Option<Instant>,
    packet_count: u32,
    octet_count: u32,
    last_sender_report: Option<Instant>,
    /// Completion time of previous fully transmitted primary-video frame.
    last_frame_sent_at: Option<Instant>,
    history: VecDeque<SentVideoPacket>,
}

const VIDEO_RTCP_SENDER_REPORT_INTERVAL: Duration = Duration::from_secs(5);
const VIDEO_RTX_HISTORY_PACKETS: usize = 2048;
/// Bound time spent spreading one encoded frame. Video shares this executor
/// loop with audio, so full-rate pacing cannot stall microphone work.
const MAX_VIDEO_FRAME_PACING: Duration = Duration::from_millis(8);
const MAX_VIDEO_PACING_GROUPS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FramePacing {
    packets_per_group: usize,
    delay: Duration,
}

impl FramePacing {
    fn for_frame(packet_count: usize, encoded_bytes: usize, bitrate_bps: u32) -> Option<Self> {
        if packet_count < 2 || bitrate_bps == 0 {
            return None;
        }
        // Permit a 2x target-rate burst, but spread larger frames enough to
        // avoid dumping every fragment into one router/SFU scheduling tick.
        let desired =
            Duration::from_secs_f64(encoded_bytes as f64 * 8.0 / (f64::from(bitrate_bps) * 2.0))
                .min(MAX_VIDEO_FRAME_PACING);
        if desired < Duration::from_millis(1) {
            return None;
        }
        let groups = packet_count.min(MAX_VIDEO_PACING_GROUPS);
        Some(Self {
            packets_per_group: packet_count.div_ceil(groups),
            delay: desired / (groups.saturating_sub(1).max(1) as u32),
        })
    }

    fn pause_after(self, packet_index: usize, packet_count: usize) -> bool {
        let sent = packet_index + 1;
        sent < packet_count && sent.is_multiple_of(self.packets_per_group)
    }
}

#[derive(Clone)]
struct SentVideoPacket {
    sequence: u16,
    timestamp: u32,
    marker: bool,
    codec_state: u64,
    rtx_payload_type: u8,
    payload: Vec<u8>,
}

/// Borrowed transport state for sending audio. Re-derived each loop round from
/// the freshly-loaded [`Connection`] (see [`Connection::send_transport`]); the
/// owned encode state lives in [`SendCodec`].
pub struct SendTransport<'a> {
    udp: &'a UdpSocket,
    description: &'a ArcSwapOption<SessionDescription>,
    dave_session: &'a AsyncMutex<Option<DaveSession>>,
    send_state: &'a Mutex<SendState>,
    /// Shared with audio and video sends so AEAD nonces never collide.
    nonce: &'a AtomicU64,
}

/// Owned encode state for the whole call. Held by the audio session (not the
/// per-round transport) so a loop round never resets the Opus encoder — which
/// would drop its inter-frame prediction — or reallocates its buffers.
pub struct SendCodec {
    ssrc: Ssrc,
    encoder: opus::Encoder,
    rtp_packet_buf: [u8; 2048],
    encoded_audio_buf: [u8; 1276],
}

/// Codec-neutral owned state for one outgoing Discord video stream.
pub(crate) enum VideoSendCodec {
    H264(Box<H264Encoder>),
    Av1(Box<Av1Encoder>),
}

impl VideoSendCodec {
    #[must_use]
    pub(crate) const fn codec(&self) -> VideoCodec {
        match self {
            Self::H264(_) => VideoCodec::H264,
            Self::Av1(_) => VideoCodec::Av1,
        }
    }

    pub(crate) fn encode(
        &mut self,
        frame: &VideoFrame,
    ) -> Result<Option<EncodedVideoFrame>, VideoError> {
        match self {
            Self::H264(encoder) => encoder.encode(frame),
            Self::Av1(encoder) => encoder.encode(frame),
        }
    }

    pub(crate) fn request_keyframe(&mut self) {
        match self {
            Self::H264(encoder) => encoder.request_keyframe(),
            Self::Av1(encoder) => encoder.request_keyframe(),
        }
    }
}

impl SendCodec {
    pub fn new(ssrc: Ssrc) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            ssrc,
            encoder: opus::Encoder::new(
                VOICE_FREQUENCY as u32,
                opus::Channels::Stereo,
                opus::Application::Voip,
            )?,
            rtp_packet_buf: [0; 2048],
            encoded_audio_buf: [0; 1276],
        })
    }
}
impl SendTransport<'_> {
    /// Whether DAVE can encrypt immediately without blocking realtime audio.
    /// A missing DAVE session means transport-only encryption is active.
    pub fn is_dave_ready(&self) -> bool {
        self.dave_session
            .try_lock()
            .is_some_and(|session| session.as_ref().is_none_or(DaveSession::is_ready))
    }

    /// Encode and send one realtime audio frame.
    ///
    /// Returns `Ok(false)` when DAVE is initializing or rotating. Caller must
    /// drop that frame instead of sending DAVE plaintext or accumulating lag.
    pub async fn send_frame(
        &self,
        codec: &mut SendCodec,
        samples: &[AudioSampleType],
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        if samples.is_empty() {
            warn!("Nothing to send");
            return Ok(false);
        }
        if !samples.len().is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected interleaved stereo samples",
            )
            .into());
        }

        // Load per frame so a mid-call key rotation takes effect.
        let description = self
            .description
            .load_full()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no session description"))?;
        let mode = description
            .mode()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing encryption mode"))?;
        let key = transport_key(&description)?;

        let encoded_len = codec
            .encoder
            .encode_float(samples, codec.encoded_audio_buf.as_mut_slice())
            .map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("opus encode failed: {err:?}"),
                )
            })?;
        let opus_payload = &codec.encoded_audio_buf[..encoded_len];

        // Dave (E2E) encrypt, scoped so the guard is dropped before the
        // transport encrypt + UDP send — a wedged send must not keep the
        // other dave_session users (event handlers, recv) blocked.
        // Never wait for MLS/key-update work while holding a realtime audio
        // frame. One dropped 20 ms frame is preferable to accumulated lag.
        let dave_encrypted;
        let audio_payload = {
            let Some(mut dave_session) = self.dave_session.try_lock() else {
                return Ok(false);
            };
            match dave_session.as_mut() {
                Some(dave_session) if !dave_session.is_ready() => return Ok(false),
                Some(dave_session) => {
                    dave_encrypted =
                        dave_session.encrypt(MediaType::AUDIO, Codec::OPUS, opus_payload)?;
                    &dave_encrypted
                }
                None => opus_payload,
            }
        };

        let samples_per_channel = (samples.len() / VOICE_CHANNELS) as u32;
        let now = Instant::now();

        // The AEAD nonce is drawn from the connection-shared counter so it is
        // never reused under the session key by a concurrent video send.
        let nonce_u32 = next_transport_nonce(self.nonce)?;

        // Grab-and-advance the per-SSRC send state in one short critical
        // section (no awaits while the lock is held).
        let (sequence, timestamp) = {
            let mut state = self.send_state.lock().unwrap();

            // Hybrid timestamp approach: sample-based during active speech,
            // but account for silence gaps based on elapsed time
            if let Some(last_time) = state.last_send_time {
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
                    state.timestamp += silence_samples;
                }
            }

            let sequence = state.sequence;
            let timestamp = state.timestamp;

            state.timestamp += samples_per_channel;
            state.sequence += 1;
            state.last_send_time = Some(now);

            (sequence, timestamp)
        };

        let rtp_header = &mut codec.rtp_packet_buf[0..RTP_HEADER_LEN];
        rtp_header[0] = 0x80;
        rtp_header[1] = OPUS_PAYLOAD_TYPE;
        rtp_header[2..4].copy_from_slice(&u16::from(sequence).to_be_bytes());
        rtp_header[4..8].copy_from_slice(&u32::from(timestamp).to_be_bytes());
        rtp_header[8..RTP_HEADER_LEN].copy_from_slice(&codec.ssrc.to_be_bytes());

        match mode {
            EncryptionMode::aead_xchacha20_poly1305_rtpsize => {
                let nonce = rtpsize_nonce(&nonce_u32);
                let encrypted = crypto_aead::xchacha20poly1305::encrypt(
                    audio_payload,
                    Some(rtp_header),
                    &nonce,
                    &key,
                )?;

                codec.rtp_packet_buf[RTP_HEADER_LEN..RTP_HEADER_LEN + encrypted.len()]
                    .copy_from_slice(&encrypted);
                codec.rtp_packet_buf[RTP_HEADER_LEN + encrypted.len()
                    ..RTP_HEADER_LEN + encrypted.len() + nonce_u32.len()]
                    .copy_from_slice(&nonce_u32);

                self.udp
                    .send(
                        &codec.rtp_packet_buf[..RTP_HEADER_LEN + encrypted.len() + nonce_u32.len()],
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

        Ok(true)
    }
}

/// Borrowed transport state for sending video. Mirrors [`SendTransport`]; the
/// owned encode state lives in the coordinator's codec-specific encoder.
pub struct VideoSendTransport<'a> {
    udp: &'a UdpSocket,
    description: &'a ArcSwapOption<SessionDescription>,
    dave_session: &'a AsyncMutex<Option<DaveSession>>,
    /// Non-empty between MLS commit/welcome processing and Discord's execute
    /// opcode. davey installs the new outbound ratchet during processing, so
    /// sending must pause until the server executes the transition.
    dave_pending_transitions: Option<&'a DashMap<u16, NonZeroU16>>,
    video_send_state: &'a Mutex<VideoSendState>,
    selected_video_codec_state: &'a AtomicU64,
    /// Shared with the audio send so AEAD nonces never collide.
    nonce: &'a AtomicU64,
    video_ssrc: Ssrc,
    rtx_ssrc: Ssrc,
}

impl VideoSendTransport<'_> {
    fn dave_transition_pending(&self) -> bool {
        self.dave_pending_transitions
            .is_some_and(|transitions| !transitions.is_empty())
    }

    /// Whether DAVE can encrypt immediately without blocking realtime video.
    /// A missing DAVE session means transport-only encryption is active.
    pub(crate) fn is_dave_ready(&self) -> bool {
        !self.dave_transition_pending()
            && self
                .dave_session
                .try_lock()
                .is_some_and(|session| session.as_ref().is_none_or(DaveSession::is_ready))
    }

    async fn send_sender_report(
        &self,
        secret_key: &[u8; 32],
        rtp_timestamp: u32,
        packet_count: u32,
        octet_count: u32,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // RTCP SR fixed header stays clear for RTP-size AEAD; sender-info body
        // is encrypted. Length 6 means 28 plaintext bytes total before
        // transport encryption: 8-byte header + 20-byte sender info.
        let mut header = [0u8; 8];
        header[0] = 0x80;
        header[1] = RtcpType::SenderReport as u8;
        header[2..4].copy_from_slice(&6u16.to_be_bytes());
        header[4..8].copy_from_slice(&self.video_ssrc.to_be_bytes());

        let unix = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let ntp_seconds = unix.as_secs().wrapping_add(2_208_988_800) as u32;
        let ntp_fraction = ((u64::from(unix.subsec_nanos()) << 32) / 1_000_000_000) as u32;
        let mut sender_info = [0u8; 20];
        sender_info[0..4].copy_from_slice(&ntp_seconds.to_be_bytes());
        sender_info[4..8].copy_from_slice(&ntp_fraction.to_be_bytes());
        sender_info[8..12].copy_from_slice(&rtp_timestamp.to_be_bytes());
        sender_info[12..16].copy_from_slice(&packet_count.to_be_bytes());
        sender_info[16..20].copy_from_slice(&octet_count.to_be_bytes());

        let nonce_u32 = next_transport_nonce(self.nonce)?;
        let nonce = rtpsize_nonce(&nonce_u32);
        let key = (*secret_key).into();
        let encrypted =
            crypto_aead::xchacha20poly1305::encrypt(&sender_info, Some(&header), &nonce, &key)?;

        let mut packet = Vec::with_capacity(header.len() + encrypted.len() + nonce_u32.len());
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&encrypted);
        packet.extend_from_slice(&nonce_u32);
        self.udp.send(&packet).await?;
        trace!(
            "Sent video RTCP sender report: ssrc={} rtp_timestamp={} packets={} octets={}",
            self.video_ssrc, rtp_timestamp, packet_count, octet_count
        );
        Ok(())
    }

    /// Encode, DAVE-encrypt, packetize (RFC 6184), transport-encrypt, and send
    /// one raw frame as H264 over RTP. Returns whether a frame was emitted;
    /// `false` means rate control skipped it or produced no NAL units.
    #[cfg(test)]
    async fn send_video_frame(
        &self,
        codec: &mut H264Encoder,
        frame: &VideoFrame,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        self.send_video_frame_for(codec, frame, VideoBroadcastKind::Camera)
            .await
    }

    #[cfg(test)]
    async fn send_video_frame_for(
        &self,
        codec: &mut H264Encoder,
        frame: &VideoFrame,
        kind: VideoBroadcastKind,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let bitrate_bps = codec.bitrate_bps();
        let Some(encoded_frame) = codec.encode(frame).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("h264 encode failed: {err}"),
            )
        })?
        else {
            return Ok(false);
        };
        self.send_encoded_video_frame_for(&encoded_frame, bitrate_bps, kind)
            .await
    }

    /// Encrypt, packetize, and send one already-encoded frame. Keeping
    /// CPU-bound encoding outside async media scheduling avoids duplicating
    /// transport state.
    #[cfg(test)]
    pub(crate) async fn send_encoded_video_frame_for(
        &self,
        encoded_frame: &EncodedVideoFrame,
        bitrate_bps: u32,
        kind: VideoBroadcastKind,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        self.send_encoded_video_frame_for_timed(encoded_frame, bitrate_bps, kind, None)
            .await
    }

    pub(crate) async fn send_encoded_video_frame_for_timed(
        &self,
        encoded_frame: &EncodedVideoFrame,
        bitrate_bps: u32,
        kind: VideoBroadcastKind,
        frame_timing: Option<VideoFrameTiming>,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let timing_enabled = tracing::enabled!(target: "video_timing", tracing::Level::TRACE);
        let total_started = timing_enabled.then(Instant::now);
        let codec_state = self.selected_video_codec_state.load(Ordering::Acquire);
        let selected_codec = video_codec_from_state(codec_state)
            .map(VideoCodec::from)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "server has not selected an outbound video codec",
                )
            })?;
        if selected_codec != encoded_frame.codec {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "encoded {:?} frame does not match server-selected {selected_codec:?}",
                    encoded_frame.codec
                ),
            )
            .into());
        }
        let (dave_codec, payload_type, rtx_payload_type) = match encoded_frame.codec {
            VideoCodec::H264 => (Codec::H264, H264_PAYLOAD_TYPE, H264_RTX_PAYLOAD_TYPE),
            VideoCodec::Av1 => (Codec::AV1, AV1_PAYLOAD_TYPE, AV1_RTX_PAYLOAD_TYPE),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported encoded video codec",
                )
                .into());
            }
        };
        let access_unit = encoded_frame.data.as_slice();
        let is_keyframe = encoded_frame.keyframe;

        let description = self
            .description
            .load_full()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no session description"))?;
        let mode = description
            .mode()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing encryption mode"))?;
        if !matches!(mode, EncryptionMode::aead_xchacha20_poly1305_rtpsize) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported encryption mode: {mode:?}"),
            )
            .into());
        }
        let secret_key = description.validated_secret_key()?;

        // DAVE encrypts the whole access unit codec-aware (NAL framing is
        // preserved) *before* packetization — same order the receive path
        // undoes. Guard scope is just the encrypt.
        let dave_encrypted;
        let dave_lock_started = timing_enabled.then(Instant::now);
        let mut dave_lock_contended = false;
        let dave_lock_wait_us;
        let dave_encrypt_us;
        let dave_transform_us;
        let dave_encrypt_attempts;
        let access_unit: &[u8] = {
            if self.dave_transition_pending() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "DAVE epoch transition is pending execution",
                )
                .into());
            }
            let mut dave_session = match self.dave_session.try_lock() {
                Some(guard) => guard,
                None => {
                    dave_lock_contended = true;
                    self.dave_session.lock().await
                }
            };
            // Transition processing inserts the gate while holding this same
            // mutex. Recheck after a contended acquisition so a frame encoded
            // during that race cannot use the new ratchet before opcode 22.
            if self.dave_transition_pending() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "DAVE epoch transition is pending execution",
                )
                .into());
            }
            dave_lock_wait_us = dave_lock_started.map(elapsed_micros).unwrap_or_default();
            match dave_session.as_mut() {
                Some(dave_session) => {
                    if !dave_session.is_ready() {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "DAVE video send state is not ready",
                        )
                        .into());
                    }
                    let stats_before = if timing_enabled {
                        dave_session
                            .get_encryption_stats(Some(MediaType::VIDEO))
                            .map(|stats| (stats.attempts, stats.duration))
                    } else {
                        None
                    };
                    let encrypt_started = timing_enabled.then(Instant::now);
                    dave_encrypted =
                        dave_session.encrypt(MediaType::VIDEO, dave_codec, access_unit)?;
                    dave_encrypt_us = encrypt_started.map(elapsed_micros).unwrap_or_default();
                    let stats_after = if timing_enabled {
                        dave_session
                            .get_encryption_stats(Some(MediaType::VIDEO))
                            .map(|stats| (stats.attempts, stats.duration))
                    } else {
                        None
                    };
                    (dave_encrypt_attempts, dave_transform_us) = match (stats_before, stats_after) {
                        (Some((attempts_before, duration_before)), Some((attempts, duration))) => (
                            u64::from(attempts.wrapping_sub(attempts_before)),
                            u64::from(duration.wrapping_sub(duration_before)),
                        ),
                        _ => (0, 0),
                    };
                    &dave_encrypted
                }
                None => {
                    dave_encrypt_us = 0;
                    dave_transform_us = 0;
                    dave_encrypt_attempts = 0;
                    access_unit
                }
            }
        };
        let dave_preflight_us = dave_encrypt_us.saturating_sub(dave_transform_us);
        if access_unit.len() > MAX_H264_ACCESS_UNIT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encrypted video frame exceeds size limit",
            )
            .into());
        }

        // Collect codec-specific RTP payloads so send-state lock never spans
        // network awaits below.
        let packetize_started = timing_enabled.then(Instant::now);
        let mut payloads: Vec<(Vec<u8>, bool)> = Vec::new();
        match encoded_frame.codec {
            VideoCodec::H264 => packetize_access_unit(access_unit, |payload, last| {
                payloads.push((payload.to_vec(), last));
            }),
            VideoCodec::Av1 => av1::packetize_temporal_unit(
                access_unit,
                encoded_frame.keyframe,
                |payload, last| {
                    payloads.push((payload.to_vec(), last));
                },
            )?,
            _ => unreachable!("unsupported codec rejected above"),
        }
        let packetize_us = packetize_started.map(elapsed_micros).unwrap_or_default();
        if payloads.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded video frame produced no RTP payloads",
            )
            .into());
        }
        let pacing = FramePacing::for_frame(payloads.len(), access_unit.len(), bitrate_bps);

        // One RTP timestamp for the whole frame (90 kHz clock); sequence
        // advances per packet. Reserve the block up front so the lock is not
        // held across the sends.
        let history_started = timing_enabled.then(Instant::now);
        let (base_sequence, timestamp) = {
            let mut state = self.video_send_state.lock().unwrap();
            if self.selected_video_codec_state.load(Ordering::Acquire) != codec_state {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "outbound video codec changed during packetization",
                )
                .into());
            }
            let start = *state.start.get_or_insert_with(Instant::now);
            let timestamp = (start.elapsed().as_secs_f64() * 90_000.0) as u32;
            let base = u16::from(state.sequence);
            state.sequence += payloads.len() as u16;
            for (index, (payload, marker)) in payloads.iter().enumerate() {
                state.history.push_back(SentVideoPacket {
                    sequence: base.wrapping_add(index as u16),
                    timestamp,
                    marker: *marker,
                    codec_state,
                    rtx_payload_type,
                    payload: payload.clone(),
                });
            }
            while state.history.len() > VIDEO_RTX_HISTORY_PACKETS {
                state.history.pop_front();
            }
            (base, timestamp)
        };
        let history_us = history_started.map(elapsed_micros).unwrap_or_default();

        let key = (*secret_key).into();

        let mut packet_buf = [0u8; 2048];
        let mut aead_us = 0_u64;
        let mut udp_send_us = 0_u64;
        let mut pacing_us = 0_u64;
        let mut packets_sent = 0_usize;
        let mut wire_bytes = 0_usize;
        for (i, (payload, last)) in payloads.iter().enumerate() {
            if self.selected_video_codec_state.load(Ordering::Acquire) != codec_state {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "outbound video codec changed during RTP send",
                )
                .into());
            }
            let sequence = base_sequence.wrapping_add(i as u16);
            // Fresh nonce per packet from the shared counter.
            let nonce_u32 = next_transport_nonce(self.nonce)?;

            // Discord native UDP video requires RFC 5285 header extensions.
            // RTP-size AEAD leaves only the fixed header + extension preamble
            // clear; extension elements are prepended to encrypted payload.
            let extensions = video_rtp_extensions(kind, is_keyframe, *last);
            let (rtp_header, encrypted_output) = write_video_rtp_header(
                &mut packet_buf,
                payload_type,
                *last,
                sequence,
                timestamp,
                self.video_ssrc,
                extensions.len(),
            );

            let nonce = rtpsize_nonce(&nonce_u32);

            let mut plaintext = Vec::with_capacity(extensions.len() + payload.len());
            plaintext.extend_from_slice(extensions);
            plaintext.extend_from_slice(payload);
            let aead_started = timing_enabled.then(Instant::now);
            let encrypted = crypto_aead::xchacha20poly1305::encrypt(
                &plaintext,
                Some(rtp_header),
                &nonce,
                &key,
            )?;
            aead_us = aead_us.saturating_add(aead_started.map(elapsed_micros).unwrap_or_default());

            let body_len = encrypted.len() + nonce_u32.len();
            if body_len > encrypted_output.len() {
                warn!(
                    "Skipping oversized video RTP packet ({} bytes)",
                    RTP_EXTENDED_CLEAR_HEADER_LEN + body_len
                );
                continue;
            }
            encrypted_output[..encrypted.len()].copy_from_slice(&encrypted);
            encrypted_output[encrypted.len()..body_len].copy_from_slice(&nonce_u32);
            let packet_len = RTP_EXTENDED_CLEAR_HEADER_LEN + body_len;
            let udp_started = timing_enabled.then(Instant::now);
            self.udp.send(&packet_buf[..packet_len]).await?;
            udp_send_us =
                udp_send_us.saturating_add(udp_started.map(elapsed_micros).unwrap_or_default());
            packets_sent += 1;
            wire_bytes += packet_len;
            if pacing.is_some_and(|pacing| pacing.pause_after(i, payloads.len())) {
                let pacing_started = timing_enabled.then(Instant::now);
                Delay::new(pacing.unwrap().delay).await;
                pacing_us = pacing_us
                    .saturating_add(pacing_started.map(elapsed_micros).unwrap_or_default());
            }
        }

        if packets_sent != payloads.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "sent only {packets_sent}/{} packets for encoded video frame",
                    payloads.len()
                ),
            )
            .into());
        }

        // Discord uses sender reports for stream clock/health. Send one after
        // first frame, then approximately every five seconds.
        if self.selected_video_codec_state.load(Ordering::Acquire) != codec_state {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "outbound video codec changed after RTP send",
            )
            .into());
        }
        let report = {
            let mut state = self.video_send_state.lock().unwrap();
            let sent_at = Instant::now();
            let output_gap_us = state
                .last_frame_sent_at
                .replace(sent_at)
                .map_or(0, |previous| {
                    duration_micros(sent_at.saturating_duration_since(previous))
                });
            state.packet_count = state.packet_count.wrapping_add(payloads.len() as u32);
            state.octet_count = state.octet_count.wrapping_add(
                payloads
                    .iter()
                    .map(|(payload, _)| payload.len() as u32)
                    .sum::<u32>(),
            );
            let due = state
                .last_sender_report
                .is_none_or(|last| last.elapsed() >= VIDEO_RTCP_SENDER_REPORT_INTERVAL);
            let report = due.then(|| {
                state.last_sender_report = Some(Instant::now());
                (state.packet_count, state.octet_count)
            });
            (report, output_gap_us)
        };
        let sender_report_started = timing_enabled.then(Instant::now);
        let (report, output_gap_us) = report;
        let sent_sender_report = report.is_some();
        if let Some((packet_count, octet_count)) = report {
            self.send_sender_report(secret_key, timestamp, packet_count, octet_count)
                .await?;
            // Piggyback DAVE observability on the report cadence: whether
            // outgoing video is E2EE at all, and whether encrypts succeed.
            // try_lock only — diagnostics never contend with media.
            if let Some(guard) = self.dave_session.try_lock() {
                match guard.as_ref() {
                    Some(dave) => {
                        if let Some(stats) = dave.get_encryption_stats(Some(MediaType::VIDEO)) {
                            debug!(
                                "DAVE video encrypt: successes={} failures={} attempts={} \
                                 max_attempts_reached={}",
                                stats.successes, stats.failures, stats.attempts, stats.max_attempts
                            );
                        }
                    }
                    None => debug!("DAVE video: no session (transport-only send)"),
                }
            }
        }

        if let Some(total_started) = total_started {
            tracing::trace!(
                target: "video_timing",
                stage = "outgoing.transport",
                ?kind,
                frame_id = frame_timing.map_or(0, VideoFrameTiming::id),
                pipeline_elapsed_us = frame_timing
                    .map_or(0, VideoFrameTiming::elapsed_micros),
                keyframe = is_keyframe,
                encoded_bytes = encoded_frame.data.len(),
                encrypted_access_unit_bytes = access_unit.len(),
                packet_count = payloads.len(),
                packets_sent,
                wire_bytes,
                dave_lock_contended,
                dave_lock_wait_us,
                dave_encrypt_us,
                dave_preflight_us,
                dave_transform_us,
                dave_encrypt_attempts,
                packetize_us,
                history_us,
                aead_us,
                udp_send_us,
                pacing_us,
                output_gap_us,
                sender_report_us = sender_report_started.map(elapsed_micros).unwrap_or_default(),
                sent_sender_report,
                total_us = elapsed_micros(total_started),
                "video stage complete"
            );
        }

        Ok(true)
    }

    /// Resend primary-video packets requested by RTCP Generic NACK using
    /// RFC 4588 RTX payloads (`original sequence || original payload`).
    pub async fn retransmit(
        &self,
        requested: &[u16],
    ) -> Result<usize, Box<dyn Error + Send + Sync>> {
        let description = self
            .description
            .load_full()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no session description"))?;
        let secret_key = description.validated_secret_key()?;
        let codec_state = self.selected_video_codec_state.load(Ordering::Acquire);
        let packets = {
            let mut state = self.video_send_state.lock().unwrap();
            requested
                .iter()
                .filter_map(|requested| {
                    let packet = state
                        .history
                        .iter()
                        .rev()
                        .find(|packet| {
                            packet.sequence == *requested && packet.codec_state == codec_state
                        })?
                        .clone();
                    let sequence = u16::from(state.rtx_sequence);
                    state.rtx_sequence += 1;
                    Some((sequence, packet))
                })
                .collect::<Vec<_>>()
        };

        let key = (*secret_key).into();
        let mut packet_buf = [0u8; 2048];
        let mut sent = 0;
        for (sequence, original) in &packets {
            if self.selected_video_codec_state.load(Ordering::Acquire) != codec_state {
                break;
            }
            let (rtp_header, encrypted_output) = write_video_rtp_header(
                &mut packet_buf,
                original.rtx_payload_type,
                original.marker,
                *sequence,
                original.timestamp,
                self.rtx_ssrc,
                VIDEO_RTX_EXTENSIONS.len(),
            );
            let nonce_u32 = next_transport_nonce(self.nonce)?;
            let nonce = rtpsize_nonce(&nonce_u32);
            let mut plaintext =
                Vec::with_capacity(VIDEO_RTX_EXTENSIONS.len() + 2 + original.payload.len());
            plaintext.extend_from_slice(&VIDEO_RTX_EXTENSIONS);
            plaintext.extend_from_slice(&original.sequence.to_be_bytes());
            plaintext.extend_from_slice(&original.payload);
            let encrypted = crypto_aead::xchacha20poly1305::encrypt(
                &plaintext,
                Some(rtp_header),
                &nonce,
                &key,
            )?;
            let body_len = encrypted.len() + nonce_u32.len();
            if body_len > encrypted_output.len() {
                continue;
            }
            encrypted_output[..encrypted.len()].copy_from_slice(&encrypted);
            encrypted_output[encrypted.len()..body_len].copy_from_slice(&nonce_u32);
            self.udp
                .send(&packet_buf[..RTP_EXTENDED_CLEAR_HEADER_LEN + body_len])
                .await?;
            sent += 1;
        }
        Ok(sent)
    }
}

pub struct Connection {
    ssrc: Ssrc,
    /// Send SSRCs for our own outbound video and its retransmission stream,
    /// assigned by the voice server (Ready `streams`) or derived from `ssrc`.
    video_ssrc: Ssrc,
    rtx_ssrc: Ssrc,
    udp: UdpSocket,
    description: ArcSwapOption<SessionDescription>,
    /// Server-confirmed outbound codec plus generation. Low byte zero means
    /// negotiation has not selected a supported codec; upper bits invalidate
    /// in-flight RTP/RTX work across live switches.
    selected_video_codec_state: AtomicU64,
    /// Server-selected fixed keyframe cadence. Session Update changes this
    /// atomically so active camera/screen encoders adopt it without teardown.
    video_keyframe_interval_ms: AtomicU32,
    /// Shared with every [`SendTransport`] handed out by `send_transport`;
    /// see [`SendState`] for why it must outlive a loop round.
    send_state: Mutex<SendState>,
    video_send_state: Mutex<VideoSendState>,
    /// AEAD nonce counter shared by audio and video sends. A single monotonic
    /// counter guarantees no nonce is ever reused under the session key,
    /// which sharing the key across SSRCs would otherwise risk.
    nonce: AtomicU64,
}

impl Connection {
    /// Build the owned codec state for a new audio session. The cheap,
    /// borrowing transport views are made per loop round via
    /// [`Self::send_transport`] / [`Self::recv_transport`] so they never keep
    /// this connection's `Arc` alive across the loop.
    pub fn new_codecs(&self) -> Result<(SendCodec, RecvCodec), Box<dyn Error>> {
        Ok((SendCodec::new(self.ssrc)?, RecvCodec::new()?))
    }
    /// Build the owned H264 encode state for outgoing video. Separate from
    /// [`Self::new_codecs`] because video sending starts and stops
    /// independently of the audio session (camera toggles mid-call).
    #[cfg(test)]
    fn new_video_send_codec(
        &self,
        usage: H264EncoderUsage,
        frames_per_second: u32,
    ) -> Result<H264Encoder, Box<dyn Error>> {
        let bitrate_bps = H264Encoder::recommended_bitrate_bps(usage, frames_per_second);
        self.video_send_state.lock().unwrap().history.clear();
        Ok(H264Encoder::new_for_frame_rate_and_bitrate(
            usage,
            frames_per_second,
            bitrate_bps,
        )?)
    }

    pub(crate) fn new_video_send_codec_with_bitrate(
        &self,
        codec: VideoCodec,
        kind: VideoBroadcastKind,
        frames_per_second: u32,
        bitrate_bps: u32,
    ) -> Result<VideoSendCodec, Box<dyn Error>> {
        // Fresh broadcast must not answer delayed NACKs with packets retained
        // from previous stream on this connection.
        self.video_send_state.lock().unwrap().history.clear();
        Ok(match codec {
            VideoCodec::H264 => {
                VideoSendCodec::H264(Box::new(H264Encoder::new_for_frame_rate_and_bitrate(
                    match kind {
                        VideoBroadcastKind::Camera => H264EncoderUsage::Camera,
                        VideoBroadcastKind::Screen => H264EncoderUsage::Screen,
                    },
                    frames_per_second,
                    bitrate_bps,
                )?))
            }
            VideoCodec::Av1 => {
                VideoSendCodec::Av1(Box::new(Av1Encoder::new_for_frame_rate_and_bitrate(
                    match kind {
                        VideoBroadcastKind::Camera => Av1EncoderUsage::Camera,
                        VideoBroadcastKind::Screen => Av1EncoderUsage::Screen,
                    },
                    frames_per_second,
                    bitrate_bps,
                )?))
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported outbound video codec",
                )
                .into());
            }
        })
    }
    pub fn send_transport<'a>(
        &'a self,
        dave_session: &'a AsyncMutex<Option<DaveSession>>,
    ) -> SendTransport<'a> {
        SendTransport {
            udp: &self.udp,
            description: &self.description,
            dave_session,
            send_state: &self.send_state,
            nonce: &self.nonce,
        }
    }
    pub fn video_send_transport<'a>(
        &'a self,
        dave_session: &'a AsyncMutex<Option<DaveSession>>,
    ) -> VideoSendTransport<'a> {
        VideoSendTransport {
            udp: &self.udp,
            description: &self.description,
            dave_session,
            dave_pending_transitions: None,
            video_send_state: &self.video_send_state,
            selected_video_codec_state: &self.selected_video_codec_state,
            nonce: &self.nonce,
            video_ssrc: self.video_ssrc,
            rtx_ssrc: self.rtx_ssrc,
        }
    }
    pub(crate) fn video_send_transport_with_transition_gate<'a>(
        &'a self,
        dave_session: &'a AsyncMutex<Option<DaveSession>>,
        dave_pending_transitions: &'a DashMap<u16, NonZeroU16>,
    ) -> VideoSendTransport<'a> {
        let mut transport = self.video_send_transport(dave_session);
        transport.dave_pending_transitions = Some(dave_pending_transitions);
        transport
    }
    pub fn recv_transport<'a>(
        &'a self,
        dave_session: &'a AsyncMutex<Option<DaveSession>>,
        ssrc_to_user_id: &'a DashMap<Ssrc, SNOWFLAKE>,
        video_ssrc_to_user_id: &'a DashMap<Ssrc, SNOWFLAKE>,
        rtx_to_video_ssrc: &'a DashMap<Ssrc, Ssrc>,
    ) -> RecvTransport<'a> {
        RecvTransport {
            udp: &self.udp,
            description: &self.description,
            dave_session,
            nonce: &self.nonce,
            local_ssrc: self.ssrc,
            ssrc_to_user_id,
            video_ssrc_to_user_id,
            rtx_to_video_ssrc,
        }
    }
    pub fn ssrc(&self) -> Ssrc {
        self.ssrc
    }
    /// UDP media cannot be decrypted until opcode 4 supplies encryption keys.
    /// Callers use this to park their receive future instead of busy-looping on
    /// the expected pre-session error.
    pub fn has_description(&self) -> bool {
        self.description.load().is_some()
    }
    pub fn video_ssrc(&self) -> Ssrc {
        self.video_ssrc
    }
    pub fn rtx_ssrc(&self) -> Ssrc {
        self.rtx_ssrc
    }
    pub fn new(udp: UdpSocket, ssrc: Ssrc, video_ssrc: Ssrc, rtx_ssrc: Ssrc) -> Self {
        Self {
            udp,
            description: ArcSwapOption::empty(),
            selected_video_codec_state: AtomicU64::new(VIDEO_CODEC_UNKNOWN.into()),
            video_keyframe_interval_ms: AtomicU32::new(DEFAULT_VIDEO_KEYFRAME_INTERVAL_MS),
            ssrc,
            video_ssrc,
            rtx_ssrc,
            send_state: Mutex::new(SendState {
                sequence: Default::default(),
                timestamp: Default::default(),
                last_send_time: None,
            }),
            video_send_state: Mutex::new(VideoSendState {
                sequence: Default::default(),
                rtx_sequence: Default::default(),
                start: None,
                packet_count: 0,
                octet_count: 0,
                last_sender_report: None,
                last_frame_sent_at: None,
                history: VecDeque::new(),
            }),
            nonce: AtomicU64::new(0),
        }
    }
    /// Install (or rotate) a validated session description. Returns `true` when
    /// this was the first description for the connection — i.e. the call just
    /// became ready — and `false` for a key rotation.
    pub fn set_description(&self, description: SessionDescription) -> io::Result<bool> {
        description.validate()?;

        let first = self.description.load().is_none();
        self.set_video_codec_name(description.video_codec())?;
        if let Some(keyframe_interval_ms) = description.keyframe_interval_ms() {
            self.set_video_keyframe_interval_ms(keyframe_interval_ms);
        }
        self.description.store(Some(Arc::new(description)));
        Ok(first)
    }

    /// Update server-confirmed outbound codec after Session Description or
    /// Session Update. Returns whether codec changed.
    pub(crate) fn set_video_codec_name(&self, name: &str) -> Result<bool, io::Error> {
        let codec = DiscordVideoCodec::from_wire_name(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported video codec selected by server",
            )
        })?;
        let codec_value = video_codec_value(codec);
        // Serialize generation change with history reservation. Old send jobs
        // either finish reserving before switch or observe new generation and
        // abort; new jobs cannot have their freshly-written history cleared.
        let mut video_send_state = self.video_send_state.lock().unwrap();
        let mut current = self.selected_video_codec_state.load(Ordering::Acquire);
        let changed = loop {
            if (current & VIDEO_CODEC_VALUE_MASK) as u8 == codec_value {
                break false;
            }
            let next = next_video_codec_state(current, codec_value);
            match self.selected_video_codec_state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break true,
                Err(actual) => current = actual,
            }
        };
        if changed {
            video_send_state.history.clear();
        }
        Ok(changed)
    }

    pub(crate) fn video_codec(&self) -> Option<VideoCodec> {
        video_codec_from_state(self.selected_video_codec_state.load(Ordering::Acquire))
            .map(VideoCodec::from)
    }

    /// Update fixed keyframe cadence from Session Description/Update.
    /// Returns whether value changed.
    pub(crate) fn set_video_keyframe_interval_ms(&self, interval_ms: u32) -> bool {
        self.video_keyframe_interval_ms
            .swap(interval_ms, Ordering::AcqRel)
            != interval_ms
    }

    /// Current keyframe cadence for active camera or screen encoder.
    pub(crate) fn video_keyframe_interval(&self) -> Duration {
        Duration::from_millis(u64::from(
            self.video_keyframe_interval_ms.load(Ordering::Acquire),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_stream_cap_retires_lru_instead_of_permanently_rejecting_new_ssrcs() {
        let mut states = HashMap::new();
        let mut last_active = HashMap::new();
        let now = Instant::now();
        for ssrc in 1..=MAX_REMOTE_MEDIA_STREAMS as Ssrc {
            states.insert(ssrc, ());
            last_active.insert(ssrc, now + Duration::from_nanos(u64::from(ssrc)));
        }

        let incoming = 10_000;
        assert_eq!(
            retire_lru_if_full(&mut states, &mut last_active, incoming),
            Some(1)
        );
        assert!(!states.contains_key(&1));
        assert!(!last_active.contains_key(&1));
        states.insert(incoming, ());
        last_active.insert(incoming, Instant::now());

        // Missing activity metadata is repaired first rather than making the
        // cap reject every future legitimate stream.
        last_active.remove(&2);
        assert_eq!(
            retire_lru_if_full(&mut states, &mut last_active, incoming + 1),
            Some(2)
        );
    }

    #[test]
    fn generic_nack_expands_packet_id_and_bitmask() {
        assert_eq!(
            parse_generic_nack(&[
                0x12,
                0x34,
                0x00,
                0b0000_0101, // 0x1234, +1, +3
                0xFF,
                0xFE,
                0x00,
                0b0000_0011, // 0xfffe, 0xffff, wrap to 0
            ]),
            vec![0x1234, 0x1235, 0x1237, 0xfffe, 0xffff, 0]
        );
    }

    #[test]
    fn rtpsize_body_rejects_truncated_nonce_or_tag() {
        assert!(split_rtpsize_body(&[0; 3], 4).is_err());
        assert!(split_rtpsize_body(&[0; 19], 4).is_err());

        let body = [0u8; 20];
        let (encrypted, nonce) = split_rtpsize_body(&body, 4).unwrap();
        assert_eq!(encrypted.len(), 16);
        assert_eq!(nonce.len(), 4);
    }

    #[test]
    fn transport_nonce_uses_last_value_once_then_refuses_wrap() {
        let counter = AtomicU64::new(u64::from(u32::MAX) - 1);

        assert_eq!(
            next_transport_nonce(&counter).unwrap(),
            (u32::MAX - 1).to_be_bytes()
        );
        assert_eq!(
            next_transport_nonce(&counter).unwrap(),
            u32::MAX.to_be_bytes()
        );
        assert!(next_transport_nonce(&counter).is_err());
        assert_eq!(counter.load(Ordering::Relaxed), u64::from(u32::MAX) + 1);
    }

    #[test]
    fn session_keyframe_interval_updates_live_and_survives_omitted_rotation() {
        smol::block_on(async {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let connection = Connection::new(udp, 1000, 1001, 1002);
            assert_eq!(
                connection.video_keyframe_interval(),
                Duration::from_millis(DEFAULT_VIDEO_KEYFRAME_INTERVAL_MS.into())
            );

            assert!(
                connection
                    .set_description(SessionDescription::for_tests_with_keyframe_interval(
                        EncryptionMode::aead_xchacha20_poly1305_rtpsize,
                        vec![7u8; 32],
                        Some(5_000),
                    ))
                    .unwrap()
            );
            assert_eq!(connection.video_keyframe_interval(), Duration::from_secs(5));

            assert!(connection.set_video_keyframe_interval_ms(750));
            assert_eq!(
                connection.video_keyframe_interval(),
                Duration::from_millis(750)
            );

            assert!(
                !connection
                    .set_description(SessionDescription::for_tests(
                        EncryptionMode::aead_xchacha20_poly1305_rtpsize,
                        vec![8u8; 32],
                    ))
                    .unwrap()
            );
            assert_eq!(
                connection.video_keyframe_interval(),
                Duration::from_millis(750),
                "omitted interval on key rotation must retain current cadence"
            );
        });
    }

    #[test]
    fn audio_send_drops_frame_while_dave_is_not_ready() {
        smol::block_on(async {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let connection = Connection::new(udp, 1000, 0, 0);
            assert!(
                connection
                    .set_description(SessionDescription::for_tests(
                        EncryptionMode::aead_xchacha20_poly1305_rtpsize,
                        vec![7u8; 32],
                    ))
                    .unwrap()
            );
            let pending_dave = DaveSession::new(
                std::num::NonZeroU16::new(davey::DAVE_PROTOCOL_VERSION).unwrap(),
                1,
                2,
                None,
            )
            .unwrap();
            let dave_session = AsyncMutex::new(Some(pending_dave));
            let transport = connection.send_transport(&dave_session);
            let (mut codec, _) = connection.new_codecs().unwrap();

            assert!(!transport.is_dave_ready());
            assert!(
                !transport
                    .send_frame(&mut codec, &[0.0; VOICE_FRAME_SAMPLES])
                    .await
                    .unwrap()
            );
            assert_eq!(connection.nonce.load(Ordering::Relaxed), 0);
            assert_eq!(u16::from(connection.send_state.lock().unwrap().sequence), 0);
        });
    }

    #[test]
    fn video_send_gate_waits_for_dave_execute_transition() {
        smol::block_on(async {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let connection = Connection::new(udp, 1000, 1001, 1002);
            let dave_session = AsyncMutex::new(None);
            let pending = DashMap::new();
            pending.insert(17, NonZeroU16::new(1).unwrap());

            let transport =
                connection.video_send_transport_with_transition_gate(&dave_session, &pending);
            assert!(!transport.is_dave_ready());

            pending.remove(&17);
            assert!(transport.is_dave_ready());
        });
    }

    #[test]
    fn rtp_padding_rejects_invalid_counts() {
        assert!(strip_rtp_padding(&[], true).is_err());
        assert!(strip_rtp_padding(&[1, 2, 0], true).is_err());
        assert!(strip_rtp_padding(&[1, 2, 4], true).is_err());
        assert_eq!(strip_rtp_padding(&[1, 2, 2], true).unwrap(), &[1]);
        assert_eq!(strip_rtp_padding(&[1, 2, 0], false).unwrap(), &[1, 2, 0]);
    }

    #[test]
    fn keyframe_request_roundtrips_as_encrypted_rtcp_pli() {
        smol::block_on(async {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = udp.local_addr().unwrap();
            udp.connect(addr).await.unwrap();

            let connection = Connection::new(udp, 1000, 0, 0);
            assert!(
                connection
                    .set_description(SessionDescription::for_tests(
                        EncryptionMode::aead_xchacha20_poly1305_rtpsize,
                        vec![7u8; 32],
                    ))
                    .unwrap()
            );

            let dave_session = AsyncMutex::new(None);
            let ssrc_to_user_id = DashMap::new();
            let video_ssrc_to_user_id = DashMap::new();
            let rtx_to_video_ssrc = DashMap::new();
            let recv_t = connection.recv_transport(
                &dave_session,
                &ssrc_to_user_id,
                &video_ssrc_to_user_id,
                &rtx_to_video_ssrc,
            );
            let description = connection.description.load_full().unwrap();

            recv_t
                .send_keyframe_request(&description, 4242)
                .await
                .unwrap();

            let mut codec = RecvCodec::new().unwrap();
            match recv_t.recv(&mut codec).await.unwrap() {
                UdpPacket::Rtcp {
                    packet_type: RtcpType::PayloadFeedback,
                    feedback: Some(RtcpFeedback::RequestKeyframe { media_ssrc }),
                } => assert_eq!(media_ssrc, 4242),
                _ => panic!("expected encrypted RTCP PLI"),
            }
        });
    }

    #[test]
    fn video_nack_roundtrips_as_encrypted_transport_feedback() {
        smol::block_on(async {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = udp.local_addr().unwrap();
            udp.connect(addr).await.unwrap();
            let connection = Connection::new(udp, 1000, 0, 0);
            assert!(
                connection
                    .set_description(SessionDescription::for_tests(
                        EncryptionMode::aead_xchacha20_poly1305_rtpsize,
                        vec![7u8; 32],
                    ))
                    .unwrap()
            );
            let dave_session = AsyncMutex::new(None);
            let audio_users = DashMap::new();
            let video_users = DashMap::new();
            let rtx_routes = DashMap::new();
            let recv =
                connection.recv_transport(&dave_session, &audio_users, &video_users, &rtx_routes);
            let description = connection.description.load_full().unwrap();

            recv.send_video_nack(&description, 4242, &[65_534, 65_535, 0, 2])
                .await
                .unwrap();

            let mut codec = RecvCodec::new().unwrap();
            let packet = match futures::future::select(
                Box::pin(recv.recv(&mut codec)),
                Box::pin(futures_timer::Delay::new(Duration::from_secs(2))),
            )
            .await
            {
                futures::future::Either::Left((packet, _)) => packet.unwrap(),
                futures::future::Either::Right(((), _)) => {
                    panic!("timed out waiting for encrypted RTCP Generic NACK")
                }
            };
            match packet {
                UdpPacket::Rtcp {
                    packet_type: RtcpType::TransportFeedback,
                    feedback:
                        Some(RtcpFeedback::Retransmit {
                            media_ssrc,
                            sequences,
                        }),
                } => {
                    assert_eq!(media_ssrc, 4242);
                    assert_eq!(sequences, vec![65_534, 65_535, 0, 2]);
                }
                _ => panic!("expected encrypted RTCP Generic NACK"),
            }
        });
    }

    #[test]
    fn receiver_report_roundtrips_as_encrypted_rtcp() {
        smol::block_on(async {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let connection = Connection::new(udp, 1000, 0, 0);
            let description = SessionDescription::for_tests(
                EncryptionMode::aead_xchacha20_poly1305_rtpsize,
                vec![7u8; 32],
            );
            let dave_session = AsyncMutex::new(None);
            let ssrc_to_user_id = DashMap::new();
            let video_ssrc_to_user_id = DashMap::new();
            let rtx_to_video_ssrc = DashMap::new();
            let recv_t = connection.recv_transport(
                &dave_session,
                &ssrc_to_user_id,
                &video_ssrc_to_user_id,
                &rtx_to_video_ssrc,
            );

            // One RR block: 8-byte clear header + 24-byte encrypted report.
            let mut header = [0u8; 8];
            header[0] = 0x81;
            header[1] = RtcpType::ReceiverReport as u8;
            header[2..4].copy_from_slice(&7u16.to_be_bytes());
            header[4..8].copy_from_slice(&999u32.to_be_bytes());
            let mut report = [0u8; 24];
            report[..4].copy_from_slice(&4242u32.to_be_bytes());
            report[4] = 32;
            let nonce_u32 = 9u32.to_be_bytes();
            let nonce = rtpsize_nonce(&nonce_u32);
            let key = crypto_aead::xchacha20poly1305::Key::from_bytes(&[7u8; 32]).unwrap();
            let encrypted =
                crypto_aead::xchacha20poly1305::encrypt(&report, Some(&header), &nonce, &key)
                    .unwrap();
            let packet = [header.as_slice(), &encrypted, &nonce_u32].concat();

            match recv_t
                .parse_rtcp(&packet, RtcpType::ReceiverReport, &description)
                .unwrap()
            {
                UdpPacket::Rtcp {
                    packet_type: RtcpType::ReceiverReport,
                    feedback: Some(RtcpFeedback::ReceiverReport { reports }),
                } => assert_eq!(
                    reports,
                    vec![ReceptionReport {
                        media_ssrc: 4242,
                        fraction_lost: 32,
                    }]
                ),
                _ => panic!("expected encrypted RTCP receiver report"),
            }
        });
    }

    /// Full send→receive mirror over a loopback UDP socket: the exact
    /// encrypted packets `send_video_frame` emits (RTP header + extension
    /// preamble, encrypted extension elements + RFC 6184 payload, nonce
    /// suffix, plus the RTCP sender report) must survive the real receive
    /// path — transport decrypt, extension strip, reassembly, H264 decode.
    /// Covers both the STAP-A path (small black keyframe) and the FU-A path
    /// (a noise keyframe too large for one packet).
    #[test]
    fn video_send_loops_back_through_receive_path() {
        smol::block_on(async {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = udp.local_addr().unwrap();
            udp.connect(addr).await.unwrap();

            let connection = Connection::new(udp, 1000, 1001, 1002);
            assert!(
                connection
                    .set_description(SessionDescription::for_tests(
                        EncryptionMode::aead_xchacha20_poly1305_rtpsize,
                        vec![7u8; 32],
                    ))
                    .unwrap()
            );

            let dave_session = AsyncMutex::new(None);
            let mut send_codec = connection
                .new_video_send_codec(H264EncoderUsage::Camera, 30)
                .unwrap();
            let (width, height) = (640u32, 360u32);

            let black = VideoFrame::new(
                width,
                height,
                PixelData::Rgba8([0, 0, 0, 255].repeat((width * height) as usize)),
            );
            // Small deterministic noise region makes the second keyframe
            // exceed one packet (FU-A) while remaining inside realtime rate
            // control's per-frame budget.
            let mut lcg = 1u32;
            let mut noise_data = [0, 0, 0, 255].repeat((width * height) as usize);
            for pixel in noise_data
                .chunks_exact_mut(4)
                .take((width * height / 64) as usize)
            {
                for channel in &mut pixel[..3] {
                    lcg = lcg.wrapping_mul(1664525).wrapping_add(1013904223);
                    *channel = (lcg >> 24) as u8;
                }
            }
            let noise = VideoFrame::new(width, height, PixelData::Rgba8(noise_data));

            let ssrc_to_user_id = DashMap::new();
            let video_ssrc_to_user_id = DashMap::new();
            let rtx_to_video_ssrc = DashMap::new();
            let recv_t = connection.recv_transport(
                &dave_session,
                &ssrc_to_user_id,
                &video_ssrc_to_user_id,
                &rtx_to_video_ssrc,
            );
            let mut recv_codec = RecvCodec::new().unwrap();

            /// Drain packets until one decodes to a frame; `None` on a
            /// drained socket (timeout) so a mirror failure reports instead
            /// of hanging.
            async fn recv_one_frame(
                recv_t: &RecvTransport<'_>,
                recv_codec: &mut RecvCodec,
            ) -> Option<(Ssrc, VideoFrame, u32)> {
                let mut consumed = 0;
                loop {
                    use futures::FutureExt as _;
                    let mut recv = std::pin::pin!(recv_t.recv(recv_codec).fuse());
                    let mut deadline =
                        std::pin::pin!(futures_timer::Delay::new(Duration::from_secs(2)).fuse());
                    let packet = futures::select! {
                        packet = recv => packet,
                        _ = deadline => return None,
                    };
                    match packet.unwrap() {
                        UdpPacket::Video { ssrc, frame } => return Some((ssrc, frame, consumed)),
                        UdpPacket::Consumed => consumed += 1,
                        UdpPacket::Rtcp { .. } => {}
                        UdpPacket::Voice { .. } => panic!("voice packet from a video send"),
                        UdpPacket::UnhandledRtp { payload_type, .. } => {
                            panic!("unhandled payload type {payload_type}")
                        }
                    }
                }
            }

            let send_t = connection.video_send_transport(&dave_session);
            send_t
                .send_video_frame(&mut send_codec, &black)
                .await
                .unwrap();
            let (ssrc, first, consumed) = recv_one_frame(&recv_t, &mut recv_codec)
                .await
                .expect("STAP-A keyframe must survive the mirror");
            eprintln!("STAP-A frame mirrored after {consumed} mid-frame packets");
            assert_eq!(ssrc, 1001);
            assert_eq!((first.width, first.height), (width, height));
            // The black frame must still be black (lossy headroom allowed).
            let first_data = match first.pixel_data {
                PixelData::Rgba8(data) => data,
                PixelData::Rgb8(_) => panic!("H264 decoder returned RGB8 instead of RGBA8"),
                _ => panic!("H264 decoder returned unsupported pixel format"),
            };
            assert!(first_data[0] < 32 && first_data[3] == 255);

            // Match realtime cadence. Bitrate control may intentionally skip
            // an immediate second frame to prevent transport queue growth.
            futures_timer::Delay::new(Duration::from_millis(34)).await;
            send_codec.request_keyframe();
            send_t
                .send_video_frame(&mut send_codec, &noise)
                .await
                .unwrap();
            let (ssrc, second, consumed) = recv_one_frame(&recv_t, &mut recv_codec)
                .await
                .expect("FU-A fragmented keyframe must survive the mirror");
            eprintln!("FU-A frame mirrored after {consumed} mid-frame packets");
            assert_eq!(ssrc, 1001);
            assert_eq!((second.width, second.height), (width, height));
            assert!(
                consumed > 2,
                "noise keyframe should have spanned several packets (got {consumed})"
            );
        });
    }

    #[test]
    fn nack_retransmits_video_packet_over_rtx_ssrc() {
        smol::block_on(async {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = udp.local_addr().unwrap();
            udp.connect(addr).await.unwrap();

            let connection = Connection::new(udp, 1000, 1001, 1002);
            assert!(
                connection
                    .set_description(SessionDescription::for_tests(
                        EncryptionMode::aead_xchacha20_poly1305_rtpsize,
                        vec![7u8; 32],
                    ))
                    .unwrap()
            );
            let dave_session = AsyncMutex::new(None);
            let send_t = connection.video_send_transport(&dave_session);
            let mut codec = connection
                .new_video_send_codec(H264EncoderUsage::Camera, 30)
                .unwrap();
            codec.request_keyframe();
            send_t
                .send_video_frame(
                    &mut codec,
                    &VideoFrame::new(160, 90, PixelData::Rgba8([0, 0, 0, 255].repeat(160 * 90))),
                )
                .await
                .unwrap();

            let mut packet = [0u8; 2048];
            loop {
                let len = connection.udp.recv(&mut packet).await.unwrap();
                if packet[1] & 0x7f == H264_PAYLOAD_TYPE && packet[1] & 0x80 != 0 {
                    break;
                }
                assert!(len > RTP_EXTENDED_CLEAR_HEADER_LEN);
            }
            // First video frame also queues its RTCP sender report.
            let _ = connection.udp.recv(&mut packet).await.unwrap();

            assert_eq!(send_t.retransmit(&[0]).await.unwrap(), 1);
            let len = connection.udp.recv(&mut packet).await.unwrap();
            assert_eq!(packet[1] & 0x7f, H264_RTX_PAYLOAD_TYPE);
            assert_eq!(u32::from_be_bytes(packet[8..12].try_into().unwrap()), 1002);

            let encrypted_end = len - 4;
            let nonce = rtpsize_nonce(&packet[encrypted_end..len]);
            let key = crypto_aead::xchacha20poly1305::Key::from_bytes(&[7u8; 32]).unwrap();
            let plaintext = crypto_aead::xchacha20poly1305::decrypt(
                &packet[RTP_EXTENDED_CLEAR_HEADER_LEN..encrypted_end],
                Some(&packet[..RTP_EXTENDED_CLEAR_HEADER_LEN]),
                &nonce,
                &key,
            )
            .unwrap();
            assert_eq!(
                &plaintext[..VIDEO_RTX_EXTENSIONS.len()],
                &VIDEO_RTX_EXTENSIONS
            );
            assert_eq!(
                u16::from_be_bytes(
                    plaintext[VIDEO_RTX_EXTENSIONS.len()..VIDEO_RTX_EXTENSIONS.len() + 2]
                        .try_into()
                        .unwrap()
                ),
                0
            );
        });
    }

    #[test]
    fn av1_transport_uses_negotiated_payload_and_switch_invalidates_history() {
        smol::block_on(async {
            let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = udp.local_addr().unwrap();
            udp.connect(addr).await.unwrap();

            let connection = Connection::new(udp, 1000, 1001, 1002);
            assert!(
                connection
                    .set_description(SessionDescription::for_tests(
                        EncryptionMode::aead_xchacha20_poly1305_rtpsize,
                        vec![7u8; 32],
                    ))
                    .unwrap()
            );
            assert_eq!(connection.video_codec(), Some(VideoCodec::H264));
            assert!(connection.set_video_codec_name("AV1").unwrap());
            let av1_state = connection
                .selected_video_codec_state
                .load(Ordering::Acquire);

            let dave_session = AsyncMutex::new(None);
            let send_t = connection.video_send_transport(&dave_session);
            let encoded = EncodedVideoFrame::new(
                VideoCodec::Av1,
                true,
                // One sized frame OBU. RTP packetizer removes size field.
                vec![(6 << 3) | 0x02, 1, 0xaa],
            );
            assert!(
                send_t
                    .send_encoded_video_frame_for(&encoded, 1_000_000, VideoBroadcastKind::Camera,)
                    .await
                    .unwrap()
            );

            let mut packet = [0u8; 2048];
            let len = connection.udp.recv(&mut packet).await.unwrap();
            assert_eq!(packet[1] & 0x7f, AV1_PAYLOAD_TYPE);
            assert_ne!(packet[1] & 0x80, 0);
            let encrypted_end = len - 4;
            let nonce = rtpsize_nonce(&packet[encrypted_end..len]);
            let key = crypto_aead::xchacha20poly1305::Key::from_bytes(&[7u8; 32]).unwrap();
            let plaintext = crypto_aead::xchacha20poly1305::decrypt(
                &packet[RTP_EXTENDED_CLEAR_HEADER_LEN..encrypted_end],
                Some(&packet[..RTP_EXTENDED_CLEAR_HEADER_LEN]),
                &nonce,
                &key,
            )
            .unwrap();
            assert_eq!(
                &plaintext[VIDEO_RTP_EXTENSIONS.len()..],
                &[0x10, 6 << 3, 0xaa]
            );
            {
                let state = connection.video_send_state.lock().unwrap();
                assert!(!state.history.is_empty());
                assert!(state.history.iter().all(|packet| {
                    packet.codec_state == av1_state
                        && packet.rtx_payload_type == AV1_RTX_PAYLOAD_TYPE
                }));
            }

            assert!(connection.set_video_codec_name("H264").unwrap());
            assert!(
                connection
                    .video_send_state
                    .lock()
                    .unwrap()
                    .history
                    .is_empty()
            );
            assert_ne!(
                connection
                    .selected_video_codec_state
                    .load(Ordering::Acquire),
                av1_state
            );

            let h264_state = connection
                .selected_video_codec_state
                .load(Ordering::Acquire);
            assert!(connection.set_video_codec_name("future-codec").is_err());
            assert_eq!(connection.video_codec(), Some(VideoCodec::H264));
            assert_eq!(
                connection
                    .selected_video_codec_state
                    .load(Ordering::Acquire),
                h264_state
            );
        });
    }

    #[test]
    fn video_rtp_header_declares_encrypted_extensions() {
        let mut packet = [0u8; 64];
        let (header, body) = write_video_rtp_header(
            &mut packet,
            H264_PAYLOAD_TYPE,
            true,
            7,
            90_000,
            42,
            VIDEO_RTP_EXTENSIONS.len(),
        );

        assert_eq!(header.len(), RTP_EXTENDED_CLEAR_HEADER_LEN);
        assert_eq!(body.len(), 64 - RTP_EXTENDED_CLEAR_HEADER_LEN);
        assert_eq!(header[0], 0x90);
        assert_eq!(header[1], H264_PAYLOAD_TYPE | 0x80);
        assert_eq!(&header[2..4], &7u16.to_be_bytes());
        assert_eq!(&header[4..8], &90_000u32.to_be_bytes());
        assert_eq!(&header[8..12], &42u32.to_be_bytes());
        assert_eq!(&header[12..14], &0xBEDEu16.to_be_bytes());
        assert_eq!(&header[14..16], &2u16.to_be_bytes());
        assert_eq!(
            VIDEO_RTP_EXTENSIONS,
            [0x62, 0x00, 0x00, 0x0A, 0xB2, b'1', b'0', b'0']
        );
    }

    #[test]
    fn screen_keyframe_last_packet_carries_content_type() {
        assert_eq!(
            video_rtp_extensions(VideoBroadcastKind::Screen, true, true),
            &SCREEN_KEYFRAME_RTP_EXTENSIONS
        );
        assert_eq!(SCREEN_KEYFRAME_RTP_EXTENSIONS[4..6], [0x70, 0x01]);
        assert_eq!(
            video_rtp_extensions(VideoBroadcastKind::Screen, true, false),
            &VIDEO_RTP_EXTENSIONS
        );
        assert_eq!(
            video_rtp_extensions(VideoBroadcastKind::Camera, true, true),
            &VIDEO_RTP_EXTENSIONS
        );
    }

    #[test]
    fn receiver_report_extracts_interval_loss() {
        let mut body = [0u8; 48];
        body[..4].copy_from_slice(&1001u32.to_be_bytes());
        body[4] = 32;
        body[24..28].copy_from_slice(&2001u32.to_be_bytes());
        body[28] = 4;

        assert_eq!(
            RecvTransport::parse_receiver_reports(&body, 2),
            vec![
                ReceptionReport {
                    media_ssrc: 1001,
                    fraction_lost: 32,
                },
                ReceptionReport {
                    media_ssrc: 2001,
                    fraction_lost: 4,
                },
            ]
        );
    }

    #[test]
    fn frame_pacing_is_bounded() {
        assert!(FramePacing::for_frame(1, 50_000, 1_000_000).is_none());
        let pacing = FramePacing::for_frame(100, 250_000, 1_000_000).unwrap();
        assert!(pacing.delay > Duration::ZERO);
        assert!(pacing.delay * (MAX_VIDEO_PACING_GROUPS as u32 - 1) <= MAX_VIDEO_FRAME_PACING);
    }
}
