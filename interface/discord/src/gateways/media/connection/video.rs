//! H264 video receive: RFC 6184 depacketization and per-frame reassembly
//! into Annex-B access units, plus decode to RGBA frames.
//!
//! Discord packetizes H264 per [RFC 6184]: a video frame (one RTP timestamp)
//! spans one or more RTP packets — single NAL units, STAP-A aggregates, or
//! FU-A fragments — and the last packet of the frame carries the RTP marker
//! bit. The assembler rebuilds the frame's NAL units with Annex-B start
//! codes, which is the layout both the DAVE frame processors and the
//! openh264 decoder consume.
//!
//! [RFC 6184]: https://datatracker.ietf.org/doc/html/rfc6184

use std::{
    io,
    time::{Duration, Instant},
};

use messenger_interface::interface::VideoFrame;
use simple_video::{H264Decoder, MAX_H264_ACCESS_UNIT_BYTES, VideoError};
#[cfg(test)]
use simple_video::{H264Encoder, PixelData};
use tracing::trace;

const ANNEX_B_START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Keep normal frame capacity for reuse, but release unusually large broken
/// frames instead of pinning their allocation for whole call.
const MAX_RETAINED_ASSEMBLY_CAPACITY: usize = 1024 * 1024;

// RFC 6184 §5.2 packetization NAL types.
const NAL_STAP_A: u8 = 24;
const NAL_FU_A: u8 = 28;

/// Max RTP payload per video packet. Kept well under a 1200-byte MTU budget
/// so a packet still fits after the transport AEAD tag + nonce suffix.
const MAX_VIDEO_RTP_PAYLOAD: usize = 1100;
/// Bounds copied out-of-order RTP payloads per remote SSRC.
const MAX_REORDERED_VIDEO_PACKETS: usize = 64;
/// One RTCP Generic NACK PID/BLP pair represents at most 17 sequences.
const MAX_NACK_SEQUENCES: usize = 17;

fn extend_access_unit(target: &mut Vec<u8>, bytes: &[u8]) -> Result<(), io::Error> {
    let len = target
        .len()
        .checked_add(bytes.len())
        .filter(|len| *len <= MAX_H264_ACCESS_UNIT_BYTES)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "H264 access unit exceeds size limit",
            )
        })?;
    target.reserve(len - target.len());
    target.extend_from_slice(bytes);
    Ok(())
}

/// Reassembles one SSRC's RTP payloads into Annex-B H264 access units.
///
/// Packets reach this assembler in order through [`VideoPacketReorder`]. A
/// gap beyond that bounded repair window poisons current frame; normal short
/// gaps are held while transport sends Generic NACK and consumes RTX repair.
pub struct FrameAssembler {
    /// RTP timestamp of the frame being assembled.
    timestamp: u32,
    /// Sequence number expected next; `None` before the first packet.
    next_seq: Option<u16>,
    /// Annex-B bytes of the frame assembled so far.
    buffer: Vec<u8>,
    /// Start offset of FU-A bytes written directly into `buffer`.
    fragment_start: Option<usize>,
    /// A gap or malformed payload poisoned the current frame.
    broken: bool,
    /// Whether `timestamp` is meaningful yet.
    started: bool,
    /// Earliest local arrival time observed for current RTP timestamp.
    assembly_started_at: Option<Instant>,
    /// Age captured when latest complete access unit left assembler.
    completed_assembly_age: Option<Duration>,
}

/// Missing sequence snapshot suitable for one RTCP Generic NACK PID/BLP.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct MissingVideoSequences {
    sequences: [u16; MAX_NACK_SEQUENCES],
    len: u8,
}

impl MissingVideoSequences {
    pub(super) fn as_slice(&self) -> &[u16] {
        &self.sequences[..usize::from(self.len)]
    }

    fn push(&mut self, sequence: u16) {
        let index = usize::from(self.len);
        if index < self.sequences.len() {
            self.sequences[index] = sequence;
            self.len += 1;
        }
    }
}

/// Result of one packet entering per-SSRC reorder/reassembly state.
pub(super) struct VideoPacketOutcome {
    pub(super) access_unit: Option<Vec<u8>>,
    pub(super) missing: MissingVideoSequences,
    pub(super) assembly_age: Option<Duration>,
}

struct BufferedVideoPacket {
    sequence: u16,
    timestamp: u32,
    marker: bool,
    payload: Vec<u8>,
    arrived_at: Option<Instant>,
}

struct VideoPacketReorder {
    expected: Option<u16>,
    pending: Vec<BufferedVideoPacket>,
}

impl VideoPacketReorder {
    fn new() -> Self {
        Self {
            expected: None,
            pending: Vec::with_capacity(MAX_REORDERED_VIDEO_PACKETS),
        }
    }

    /// `Some(distance)` only when `sequence` is current or ahead in wrapping
    /// RTP order. Values in older half of sequence space are late duplicates.
    fn forward_distance(expected: u16, sequence: u16) -> Option<u16> {
        let distance = sequence.wrapping_sub(expected);
        (distance < 0x8000).then_some(distance)
    }

    fn contains(&self, sequence: u16) -> bool {
        self.pending
            .iter()
            .any(|packet| packet.sequence == sequence)
    }

    fn take(&mut self, sequence: u16) -> Option<BufferedVideoPacket> {
        let index = self
            .pending
            .iter()
            .position(|packet| packet.sequence == sequence)?;
        Some(self.pending.swap_remove(index))
    }

    fn missing(&self) -> MissingVideoSequences {
        let mut missing = MissingVideoSequences::default();
        let Some(expected) = self.expected else {
            return missing;
        };
        let Some(furthest) = self
            .pending
            .iter()
            .filter_map(|packet| Self::forward_distance(expected, packet.sequence))
            .max()
        else {
            return missing;
        };
        for offset in 0..=furthest.min((MAX_NACK_SEQUENCES - 1) as u16) {
            let sequence = expected.wrapping_add(offset);
            if !self.contains(sequence) {
                missing.push(sequence);
            }
        }
        missing
    }

    fn drain_until_frame(&mut self, assembler: &mut FrameAssembler) -> Option<Vec<u8>> {
        loop {
            let expected = self.expected?;
            let packet = self.take(expected)?;
            self.expected = Some(expected.wrapping_add(1));
            if let Some(frame) = assembler.push_arrived_at(
                packet.sequence,
                packet.timestamp,
                packet.marker,
                &packet.payload,
                packet.arrived_at,
            ) {
                return Some(frame);
            }
        }
    }

    fn push(
        &mut self,
        assembler: &mut FrameAssembler,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> VideoPacketOutcome {
        let arrived_at =
            tracing::enabled!(target: "video_timing", tracing::Level::TRACE).then(Instant::now);
        let Some(expected) = self.expected else {
            self.expected = Some(sequence.wrapping_add(1));
            return VideoPacketOutcome {
                access_unit: assembler
                    .push_arrived_at(sequence, timestamp, marker, payload, arrived_at),
                missing: MissingVideoSequences::default(),
                assembly_age: None,
            };
        };

        let Some(distance) = Self::forward_distance(expected, sequence) else {
            // Already delivered or buffered duplicate from older sequence
            // half. It must not poison current frame.
            return VideoPacketOutcome {
                access_unit: self.drain_until_frame(assembler),
                missing: self.missing(),
                assembly_age: None,
            };
        };

        if distance as usize >= MAX_REORDERED_VIDEO_PACKETS {
            // Gap exceeded repair window. Bound memory/latency: abandon held
            // payloads and let assembler's sequence check poison this frame.
            self.pending.clear();
            self.expected = Some(sequence.wrapping_add(1));
            return VideoPacketOutcome {
                access_unit: assembler
                    .push_arrived_at(sequence, timestamp, marker, payload, arrived_at),
                missing: MissingVideoSequences::default(),
                assembly_age: None,
            };
        }

        let access_unit = if distance == 0 {
            self.expected = Some(expected.wrapping_add(1));
            assembler
                .push_arrived_at(sequence, timestamp, marker, payload, arrived_at)
                .or_else(|| self.drain_until_frame(assembler))
        } else {
            if !self.contains(sequence) {
                self.pending.push(BufferedVideoPacket {
                    sequence,
                    timestamp,
                    marker,
                    payload: payload.to_vec(),
                    arrived_at,
                });
            }
            self.drain_until_frame(assembler)
        };

        VideoPacketOutcome {
            access_unit,
            missing: self.missing(),
            assembly_age: None,
        }
    }
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            timestamp: 0,
            next_seq: None,
            buffer: Vec::new(),
            fragment_start: None,
            broken: false,
            started: false,
            assembly_started_at: None,
            completed_assembly_age: None,
        }
    }

    fn take_completed_assembly_age(&mut self) -> Option<Duration> {
        self.completed_assembly_age.take()
    }

    fn discard_buffer(&mut self) {
        if self.buffer.capacity() > MAX_RETAINED_ASSEMBLY_CAPACITY {
            self.buffer = Vec::new();
        } else {
            self.buffer.clear();
        }
    }

    /// Return a consumed completed access-unit allocation for next frame.
    /// Call only after DAVE/decode no longer borrows `buffer`.
    pub(super) fn recycle(&mut self, mut buffer: Vec<u8>) {
        if self.buffer.is_empty() && buffer.capacity() <= MAX_RETAINED_ASSEMBLY_CAPACITY {
            buffer.clear();
            self.buffer = buffer;
        }
    }

    /// Feed one RTP payload. Returns a complete Annex-B access unit when
    /// this packet finished an intact frame (marker bit, no losses).
    #[cfg(test)]
    pub fn push(
        &mut self,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> Option<Vec<u8>> {
        let arrived_at =
            tracing::enabled!(target: "video_timing", tracing::Level::TRACE).then(Instant::now);
        self.push_arrived_at(sequence, timestamp, marker, payload, arrived_at)
    }

    fn push_arrived_at(
        &mut self,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
        arrived_at: Option<Instant>,
    ) -> Option<Vec<u8>> {
        // A new timestamp starts a new frame. Reaching here with leftovers
        // means the previous frame's marker packet was lost — drop it.
        if !self.started || timestamp != self.timestamp {
            if self.started && !self.buffer.is_empty() {
                trace!("Dropping incomplete video frame (timestamp advanced without marker)");
            }
            self.timestamp = timestamp;
            self.discard_buffer();
            self.fragment_start = None;
            self.broken = false;
            self.started = true;
            self.assembly_started_at = arrived_at;
            self.completed_assembly_age = None;
        } else if let Some(arrived_at) = arrived_at {
            self.assembly_started_at = Some(
                self.assembly_started_at
                    .map_or(arrived_at, |started_at| started_at.min(arrived_at)),
            );
        }

        // Any gap inside the frame makes it undecodable.
        if let Some(expected) = self.next_seq
            && sequence != expected
        {
            trace!("Video sequence gap: expected {expected}, got {sequence}");
            self.broken = true;
            self.fragment_start = None;
        }
        self.next_seq = Some(sequence.wrapping_add(1));

        if !self.broken
            && let Err(err) = self.append_payload(payload)
        {
            trace!("Poisoning video frame: {err}");
            self.broken = true;
            self.fragment_start = None;
        }

        if !marker {
            return None;
        }
        // Frame boundary: emit only if fully intact (no losses, no dangling
        // FU-A fragment whose end packet never came).
        let complete = !self.broken && self.fragment_start.is_none() && !self.buffer.is_empty();
        let frame = if complete {
            self.completed_assembly_age =
                self.assembly_started_at.take().map(|start| start.elapsed());
            Some(std::mem::take(&mut self.buffer))
        } else {
            self.assembly_started_at = None;
            self.completed_assembly_age = None;
            self.discard_buffer();
            None
        };
        self.fragment_start = None;
        self.broken = false;
        frame
    }

    /// Append one RTP payload's NAL unit(s) to the frame buffer per RFC 6184.
    fn append_payload(&mut self, payload: &[u8]) -> Result<(), io::Error> {
        let (&indicator, rest) = payload
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty video payload"))?;

        match indicator & 0x1F {
            // Single NAL unit packet: the payload *is* the NAL.
            1..=23 => {
                extend_access_unit(&mut self.buffer, &ANNEX_B_START_CODE)?;
                extend_access_unit(&mut self.buffer, payload)?;
                Ok(())
            }
            // STAP-A: [u16 size, NAL] repeated.
            NAL_STAP_A => {
                let mut cursor = rest;
                while !cursor.is_empty() {
                    let (size_bytes, tail) = cursor.split_at_checked(2).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "truncated STAP-A size")
                    })?;
                    let size = u16::from_be_bytes([size_bytes[0], size_bytes[1]]) as usize;
                    if size == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "empty STAP-A NAL",
                        ));
                    }
                    let (nal, tail) = tail.split_at_checked(size).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "truncated STAP-A NAL")
                    })?;
                    extend_access_unit(&mut self.buffer, &ANNEX_B_START_CODE)?;
                    extend_access_unit(&mut self.buffer, nal)?;
                    cursor = tail;
                }
                Ok(())
            }
            // FU-A: one NAL split across packets. Header byte 2 is
            // S:1 | E:1 | R:1 | type:5; the original NAL header is
            // reconstructed from the indicator's NRI bits + the type.
            NAL_FU_A => {
                let (&fu_header, fragment_data) = rest.split_first().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "FU-A missing header")
                })?;
                let start = fu_header & 0x80 != 0;
                let end = fu_header & 0x40 != 0;
                if fragment_data.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "empty FU-A fragment",
                    ));
                }

                if start {
                    if let Some(fragment_start) = self.fragment_start.take() {
                        trace!("FU-A start while a fragment was in progress; dropping the old one");
                        self.buffer.truncate(fragment_start);
                    }
                    let nal_header = (indicator & 0xE0) | (fu_header & 0x1F);
                    let fragment_start = self.buffer.len();
                    extend_access_unit(&mut self.buffer, &ANNEX_B_START_CODE)?;
                    extend_access_unit(&mut self.buffer, &[nal_header])?;
                    extend_access_unit(&mut self.buffer, fragment_data)?;
                    self.fragment_start = Some(fragment_start);
                } else {
                    self.fragment_start.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "FU-A continuation without a start",
                        )
                    })?;
                    extend_access_unit(&mut self.buffer, fragment_data)?;
                }

                if end {
                    self.fragment_start = None;
                }
                Ok(())
            }
            other => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported RFC 6184 NAL type {other}"),
            )),
        }
    }
}

/// Per-SSRC video receive state: the frame assembler plus an H264 decoder.
/// One per sender, like the per-SSRC Opus decoders — decoders carry
/// inter-frame state that must not interleave across senders.
pub struct VideoRecv {
    pub assembler: FrameAssembler,
    reorder: VideoPacketReorder,
    decoder: H264Decoder,
}

impl VideoRecv {
    pub fn new() -> Result<Self, VideoError> {
        Ok(Self {
            assembler: FrameAssembler::new(),
            reorder: VideoPacketReorder::new(),
            decoder: H264Decoder::new()?,
        })
    }

    /// Decode one Annex-B access unit to an RGBA frame. `Ok(None)` when the
    /// decoder buffered it without output (e.g. waiting on a keyframe after
    /// join or loss — expected, not an error).
    pub fn decode_frame(&mut self, access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        self.decoder.decode(access_unit)
    }

    /// Reorder one media/RTX payload and return a completed access unit plus
    /// current bounded missing-sequence snapshot for Generic NACK feedback.
    pub(super) fn push_packet(
        &mut self,
        sequence: u16,
        timestamp: u32,
        marker: bool,
        payload: &[u8],
    ) -> VideoPacketOutcome {
        let mut outcome =
            self.reorder
                .push(&mut self.assembler, sequence, timestamp, marker, payload);
        if outcome.access_unit.is_some() {
            outcome.assembly_age = self.assembler.take_completed_assembly_age();
        }
        outcome
    }

    /// Discard decoder state after a corrupt/incomplete access unit. OpenH264
    /// cannot recover inter-frame state reliably without a fresh keyframe.
    pub fn reset(&mut self) -> Result<(), VideoError> {
        self.assembler = FrameAssembler::new();
        self.reorder = VideoPacketReorder::new();
        self.decoder = H264Decoder::new()?;
        Ok(())
    }
}

/// Split an Annex-B stream into NAL unit bodies (start codes excluded).
///
/// Mirrors libwebrtc's `FindNaluIndices` byte-for-byte: a start code is
/// `00 00 01`, optionally preceded by exactly one zero (the four-byte form),
/// and a NAL's body ends where the next start code begins. The exactness
/// matters beyond tidiness: the receiving client reassembles the frame as
/// `00 00 00 01` + body per packet, and DAVE authenticates those exact bytes
/// — a body that leaks a byte of the neighboring start code (what
/// `openh264::nal_units` produces for four-byte codes) makes every encrypted
/// frame fail its AEAD check on the viewer's side.
fn annex_b_nal_bodies(stream: &[u8]) -> Vec<&[u8]> {
    let mut bodies: Vec<&[u8]> = Vec::new();
    let mut body_start: Option<usize> = None;
    let mut i = 0;
    while i + 2 < stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            if let Some(start) = body_start.take() {
                // Exclude the single leading zero of a four-byte start code;
                // any further zeros are the previous NAL's own trailing bytes.
                let end = i - usize::from(i > start && stream[i - 1] == 0);
                if end > start {
                    bodies.push(&stream[start..end]);
                }
            }
            body_start = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(start) = body_start
        && start < stream.len()
    {
        bodies.push(&stream[start..]);
    }
    bodies
}

/// Whether an Annex-B access unit carries an IDR slice (NAL type 5).
#[cfg(test)]
fn access_unit_contains_idr(access_unit: &[u8]) -> bool {
    annex_b_nal_bodies(access_unit)
        .iter()
        .any(|nal| nal.first().is_some_and(|header| header & 0x1F == 5))
}

/// Packetize one Annex-B access unit into RFC 6184 RTP payloads.
///
/// Consecutive small NALs are packed into STAP-A payloads; a lone small NAL
/// stays a single-NAL packet. Oversized NALs become FU-A fragments. `for_each`
/// receives every payload with a flag that is true only for the *last* payload
/// of the whole access unit — the caller sets the RTP marker bit on that one.
///
/// Operates on the bytes as-is: when DAVE is active the input is the
/// codec-aware-encrypted access unit, whose NAL framing is preserved, so
/// packetization is identical to the plaintext path.
pub fn packetize_access_unit(access_unit: &[u8], mut for_each: impl FnMut(&[u8], bool)) {
    // Collect NAL boundaries first so we know which packet is the last.
    let nals = annex_b_nal_bodies(access_unit);

    let mut packet = Vec::with_capacity(MAX_VIDEO_RTP_PAYLOAD + 2);
    let mut aggregate = Vec::new();
    let mut aggregate_size = 0;
    for (nal_idx, nal) in nals.iter().enumerate() {
        let last_nal = nal_idx + 1 == nals.len();

        if nal.len() <= MAX_VIDEO_RTP_PAYLOAD {
            // STAP-A costs one aggregate header plus a 16-bit size before
            // each NAL. A first NAL may exceed the aggregate cap after that
            // overhead but is still valid alone; flush it before adding a
            // second NAL.
            let stap_nal_size = 2 + nal.len();
            if !aggregate.is_empty() && aggregate_size + stap_nal_size > MAX_VIDEO_RTP_PAYLOAD {
                emit_small_nals(&aggregate, false, &mut packet, &mut for_each);
                aggregate.clear();
                aggregate_size = 0;
            }
            if aggregate.is_empty() {
                aggregate_size = 1;
            }
            aggregate.push(*nal);
            aggregate_size += stap_nal_size;

            if last_nal {
                emit_small_nals(&aggregate, true, &mut packet, &mut for_each);
                aggregate.clear();
            }
            continue;
        }

        if !aggregate.is_empty() {
            emit_small_nals(&aggregate, false, &mut packet, &mut for_each);
            aggregate.clear();
            aggregate_size = 0;
        }

        // FU-A: fragment across packets. The original NAL header byte is
        // replaced by a 2-byte FU indicator + FU header; its NRI/type are
        // carried in those.
        let nal_header = nal[0];
        let fu_indicator = (nal_header & 0xE0) | NAL_FU_A;
        let nal_type = nal_header & 0x1F;
        let body = &nal[1..];
        // Leave room for the 2-byte FU header in each fragment.
        let chunk_size = MAX_VIDEO_RTP_PAYLOAD - 2;
        let chunk_count = body.len().div_ceil(chunk_size);

        for (chunk_idx, chunk) in body.chunks(chunk_size).enumerate() {
            let start = chunk_idx == 0;
            let end = chunk_idx + 1 == chunk_count;
            let fu_header = ((start as u8) << 7) | ((end as u8) << 6) | nal_type;

            packet.clear();
            packet.push(fu_indicator);
            packet.push(fu_header);
            packet.extend_from_slice(chunk);
            for_each(&packet, last_nal && end);
        }
    }
}

/// Emit one or more small NALs. Aggregating parameter sets and slices in one
/// STAP-A packet matches WebRTC's H264 packetizer and preserves one marker for
/// the complete access unit.
fn emit_small_nals(
    nals: &[&[u8]],
    last: bool,
    packet: &mut Vec<u8>,
    for_each: &mut impl FnMut(&[u8], bool),
) {
    debug_assert!(!nals.is_empty());
    if nals.len() == 1 {
        for_each(nals[0], last);
        return;
    }

    let forbidden = nals.iter().fold(0, |bits, nal| bits | (nal[0] & 0x80));
    let max_nri = nals.iter().map(|nal| nal[0] & 0x60).max().unwrap_or(0);
    packet.clear();
    packet.push(forbidden | max_nri | NAL_STAP_A);
    for nal in nals {
        packet.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        packet.extend_from_slice(nal);
    }
    debug_assert!(packet.len() <= MAX_VIDEO_RTP_PAYLOAD);
    for_each(packet, last);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NAL type 1 (non-IDR slice) with NRI set.
    const NAL_HEADER: u8 = 0x61;

    #[test]
    fn detects_idr_access_units() {
        assert!(access_unit_contains_idr(&[0, 0, 0, 1, 0x65, 0xAA]));
        assert!(!access_unit_contains_idr(&[0, 0, 1, NAL_HEADER, 0xAA]));
    }

    #[test]
    fn packetize_small_nals_are_aggregated() {
        // Two small NALs → one STAP-A packet carrying both, marked complete.
        let access_unit = [
            &[0, 0, 0, 1, NAL_HEADER, 0xAA][..],
            &[0, 0, 1, 0x41, 0xBB, 0xCC][..],
        ]
        .concat();
        let mut packets = Vec::new();
        packetize_access_unit(&access_unit, |payload, last| {
            packets.push((payload.to_vec(), last))
        });
        assert_eq!(
            packets,
            vec![(
                vec![0x78, 0, 2, NAL_HEADER, 0xAA, 0, 3, 0x41, 0xBB, 0xCC],
                true,
            )]
        );
    }

    #[test]
    fn packetize_large_nal_fragments_into_fu_a() {
        // One NAL far larger than the payload cap → several FU-A fragments,
        // start bit on the first, end (and marker) on the last.
        let mut nal = vec![NAL_HEADER];
        nal.extend(std::iter::repeat_n(0xEE, MAX_VIDEO_RTP_PAYLOAD * 2));
        let access_unit = [&[0, 0, 0, 1][..], &nal].concat();

        let mut packets = Vec::new();
        packetize_access_unit(&access_unit, |payload, last| {
            packets.push((payload.to_vec(), last))
        });

        assert!(packets.len() >= 3, "expected multiple fragments");
        let fu_indicator = (NAL_HEADER & 0xE0) | NAL_FU_A;
        // First fragment: start bit set, not the marker.
        assert_eq!(packets[0].0[0], fu_indicator);
        assert_eq!(packets[0].0[1] & 0x80, 0x80);
        assert!(!packets[0].1);
        // Last fragment: end bit set, marker set.
        let last = packets.last().unwrap();
        assert_eq!(last.0[1] & 0x40, 0x40);
        assert!(last.1);
        // Middle fragments: neither start nor end.
        assert_eq!(packets[1].0[1] & 0xC0, 0x00);

        // Reassembling the fragments through the receive assembler yields the
        // original NAL.
        let mut assembler = FrameAssembler::new();
        let mut reassembled = None;
        for (i, (payload, marker)) in packets.iter().enumerate() {
            if let Some(frame) = assembler.push(i as u16, 7, *marker, payload) {
                reassembled = Some(frame);
            }
        }
        let expected = [&[0, 0, 0, 1][..], &nal].concat();
        assert_eq!(reassembled.unwrap(), expected);
    }

    #[test]
    fn packetize_four_byte_start_codes_yield_clean_nals() {
        // openh264 (and DAVE's normalization) emit four-byte start codes; the
        // leading zero of the *next* code must not leak into a NAL's payload.
        let access_unit = [
            &[0, 0, 0, 1, 0x67, 0xAA][..], // SPS
            &[0, 0, 0, 1, 0x68, 0xBB][..], // PPS
            &[0, 0, 0, 1, 0x65, 0xCC][..], // IDR slice
        ]
        .concat();
        let mut packets = Vec::new();
        packetize_access_unit(&access_unit, |payload, last| {
            packets.push((payload.to_vec(), last))
        });
        assert_eq!(
            packets,
            vec![(
                vec![0x78, 0, 2, 0x67, 0xAA, 0, 2, 0x68, 0xBB, 0, 2, 0x65, 0xCC],
                true,
            )]
        );
    }

    #[test]
    fn packetize_reassembles_multi_nal_frame_byte_exact() {
        // The DAVE invariant: the receiver rebuilds `00 00 00 01` + body per
        // NAL, and frame decryption authenticates those exact bytes. A frame
        // in DAVE's normalized layout (four-byte codes, one NAL large enough
        // to fragment, trailing supplemental bytes glued to the last NAL)
        // must survive packetize → reassemble without a single byte of drift.
        let mut big_nal = vec![0x65];
        big_nal.extend(std::iter::repeat_n(0xEE, MAX_VIDEO_RTP_PAYLOAD * 2));
        // Ciphertext-like tail ending in zeros right before the next code.
        big_nal.extend_from_slice(&[0x00, 0x00]);
        let access_unit = [
            &[0, 0, 0, 1, 0x67, 0xAA][..],
            &[0, 0, 0, 1][..],
            &big_nal,
            &[0, 0, 0, 1, 0x61, 0xDD, 0xFA, 0xFA][..],
        ]
        .concat();

        let mut packets = Vec::new();
        packetize_access_unit(&access_unit, |payload, last| {
            packets.push((payload.to_vec(), last))
        });
        let mut assembler = FrameAssembler::new();
        let mut reassembled = None;
        for (i, (payload, marker)) in packets.iter().enumerate() {
            if let Some(frame) = assembler.push(i as u16, 7, *marker, payload) {
                reassembled = Some(frame);
            }
        }
        assert_eq!(reassembled.unwrap(), access_unit);
    }

    #[test]
    fn black_frame_survives_the_full_send_pipeline() {
        // Viewer simulation on real encoder output: encode → packetize →
        // reassemble → decode with a fresh decoder, as a client that just
        // started watching would. Also checks the reassembly matches the
        // 4-byte-normalized original — the byte layout DAVE authenticates.
        let mut send = H264Encoder::new().unwrap();
        send.request_keyframe();
        let (width, height) = (640, 360);
        let frame = VideoFrame::new(
            width,
            height,
            PixelData::Rgba8([0, 0, 0, 255].repeat((width * height) as usize)),
        );
        let encoded = send
            .encode(&frame)
            .unwrap()
            .expect("forced keyframe must produce an access unit");
        let access_unit = encoded.data;

        let mut packets = Vec::new();
        packetize_access_unit(&access_unit, |payload, last| {
            packets.push((payload.to_vec(), last))
        });
        assert!(!packets.is_empty());

        let mut assembler = FrameAssembler::new();
        let mut reassembled = None;
        for (i, (payload, marker)) in packets.iter().enumerate() {
            if let Some(frame) = assembler.push(i as u16, 3000, *marker, payload) {
                reassembled = Some(frame);
            }
        }
        let reassembled = reassembled.expect("marker packet must complete the frame");

        let normalized: Vec<u8> = annex_b_nal_bodies(&access_unit)
            .iter()
            .flat_map(|body| [&ANNEX_B_START_CODE[..], body].concat())
            .collect();
        assert_eq!(reassembled, normalized);

        let mut recv = VideoRecv::new().unwrap();
        let decoded = recv
            .decode_frame(&reassembled)
            .expect("reassembled keyframe must decode")
            .expect("IDR access unit must yield a picture");
        assert_eq!((decoded.width, decoded.height), (width, height));
    }

    #[test]
    fn single_nal_units_gain_start_codes() {
        let mut assembler = FrameAssembler::new();
        assert!(
            assembler
                .push(10, 1000, false, &[NAL_HEADER, 0xAA])
                .is_none()
        );
        let frame = assembler.push(11, 1000, true, &[NAL_HEADER, 0xBB]).unwrap();
        assert_eq!(
            frame,
            [
                &[0, 0, 0, 1, NAL_HEADER, 0xAA][..],
                &[0, 0, 0, 1, NAL_HEADER, 0xBB][..],
            ]
            .concat()
        );
    }

    #[test]
    fn completed_access_unit_allocation_is_recycled() {
        let mut assembler = FrameAssembler::new();
        let first = assembler.push(0, 100, true, &[NAL_HEADER, 0xAA]).unwrap();
        let pointer = first.as_ptr();
        assembler.recycle(first);

        let second = assembler.push(1, 200, true, &[NAL_HEADER, 0xBB]).unwrap();

        assert_eq!(second.as_ptr(), pointer);
        assert_eq!(second, [0, 0, 0, 1, NAL_HEADER, 0xBB]);
    }

    #[test]
    fn stap_a_unpacks_every_nal() {
        let mut assembler = FrameAssembler::new();
        // STAP-A carrying two NALs of 2 and 3 bytes.
        let payload = [
            NAL_STAP_A, 0, 2, NAL_HEADER, 0xAA, 0, 3, NAL_HEADER, 0xBB, 0xCC,
        ];
        let frame = assembler.push(0, 500, true, &payload).unwrap();
        assert_eq!(
            frame,
            [
                &[0, 0, 0, 1, NAL_HEADER, 0xAA][..],
                &[0, 0, 0, 1, NAL_HEADER, 0xBB, 0xCC][..],
            ]
            .concat()
        );
    }

    #[test]
    fn fu_a_reassembles_the_original_nal() {
        let mut assembler = FrameAssembler::new();
        let indicator = 0x60 | NAL_FU_A; // NRI from the original NAL
        let fu_type = 0x01; // original NAL type
        assert!(
            assembler
                .push(0, 42, false, &[indicator, 0x80 | fu_type, 0xAA])
                .is_none()
        );
        assert!(
            assembler
                .push(1, 42, false, &[indicator, fu_type, 0xBB])
                .is_none()
        );
        let frame = assembler
            .push(2, 42, true, &[indicator, 0x40 | fu_type, 0xCC])
            .unwrap();
        // Reconstructed header: NRI bits from the indicator + original type.
        assert_eq!(frame, [0, 0, 0, 1, 0x61, 0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn sequence_gap_drops_the_frame_but_not_the_next() {
        let mut assembler = FrameAssembler::new();
        assert!(assembler.push(0, 100, false, &[NAL_HEADER, 0xAA]).is_none());
        // Packet 1 lost.
        assert!(assembler.push(2, 100, true, &[NAL_HEADER, 0xBB]).is_none());
        // Next frame is intact and unaffected.
        let frame = assembler.push(3, 200, true, &[NAL_HEADER, 0xCC]).unwrap();
        assert_eq!(frame, [0, 0, 0, 1, NAL_HEADER, 0xCC]);
    }

    #[test]
    fn reorder_repairs_gap_before_frame_reassembly() {
        let mut reorder = VideoPacketReorder::new();
        let mut assembler = FrameAssembler::new();
        assert!(
            reorder
                .push(&mut assembler, 10, 100, false, &[NAL_HEADER, 0x10])
                .access_unit
                .is_none()
        );

        let waiting = reorder.push(&mut assembler, 12, 100, true, &[NAL_HEADER, 0x12]);
        assert!(waiting.access_unit.is_none());
        assert_eq!(waiting.missing.as_slice(), &[11]);

        let repaired = reorder.push(&mut assembler, 11, 100, false, &[NAL_HEADER, 0x11]);
        assert!(repaired.missing.as_slice().is_empty());
        assert_eq!(
            repaired.access_unit.unwrap(),
            [
                0, 0, 0, 1, NAL_HEADER, 0x10, 0, 0, 0, 1, NAL_HEADER, 0x11, 0, 0, 0, 1, NAL_HEADER,
                0x12,
            ]
        );
    }

    #[test]
    fn assembly_age_includes_reordered_packet_wait() {
        let mut assembler = FrameAssembler::new();
        let now = Instant::now();
        let buffered_arrival = now - Duration::from_millis(25);

        assert!(
            assembler
                .push_arrived_at(10, 100, false, &[NAL_HEADER, 0x10], Some(now))
                .is_none()
        );
        assert!(
            assembler
                .push_arrived_at(11, 100, true, &[NAL_HEADER, 0x11], Some(buffered_arrival),)
                .is_some()
        );

        assert!(
            assembler.take_completed_assembly_age().unwrap() >= Duration::from_millis(25),
            "assembly timing must start at earliest buffered arrival"
        );
    }

    #[test]
    fn reorder_handles_wrapping_sequence_numbers() {
        let mut reorder = VideoPacketReorder::new();
        let mut assembler = FrameAssembler::new();
        reorder.push(&mut assembler, u16::MAX, 100, false, &[NAL_HEADER, 1]);
        let waiting = reorder.push(&mut assembler, 1, 100, true, &[NAL_HEADER, 3]);
        assert_eq!(waiting.missing.as_slice(), &[0]);

        let repaired = reorder.push(&mut assembler, 0, 100, false, &[NAL_HEADER, 2]);
        assert!(repaired.access_unit.is_some());
        assert!(repaired.missing.as_slice().is_empty());
    }

    #[test]
    fn reorder_window_stays_bounded_on_large_gap() {
        let mut reorder = VideoPacketReorder::new();
        let mut assembler = FrameAssembler::new();
        reorder.push(&mut assembler, 0, 100, false, &[NAL_HEADER, 1]);
        let outcome = reorder.push(
            &mut assembler,
            MAX_REORDERED_VIDEO_PACKETS as u16 + 1,
            200,
            true,
            &[NAL_HEADER, 2],
        );

        assert!(outcome.access_unit.is_none());
        assert!(outcome.missing.as_slice().is_empty());
        assert!(reorder.pending.is_empty());
    }

    #[test]
    fn lost_marker_drops_the_stale_frame() {
        let mut assembler = FrameAssembler::new();
        assert!(assembler.push(0, 100, false, &[NAL_HEADER, 0xAA]).is_none());
        // Marker packet of ts=100 lost; ts advances (and its seq gap also
        // poisons the new frame) — then a clean frame follows.
        assert!(assembler.push(2, 200, true, &[NAL_HEADER, 0xBB]).is_none());
        let frame = assembler.push(3, 300, true, &[NAL_HEADER, 0xCC]).unwrap();
        assert_eq!(frame, [0, 0, 0, 1, NAL_HEADER, 0xCC]);
    }

    #[test]
    fn dangling_fu_a_fragment_invalidates_the_frame() {
        let mut assembler = FrameAssembler::new();
        let indicator = 0x60 | NAL_FU_A;
        // Start fragment but never the end; marker arrives mid-fragment.
        assert!(
            assembler
                .push(0, 7, false, &[indicator, 0x81, 0xAA])
                .is_none()
        );
        assert!(
            assembler
                .push(1, 7, true, &[indicator, 0x01, 0xBB])
                .is_none()
        );
    }

    #[test]
    fn oversized_access_unit_is_dropped_and_next_frame_recovers() {
        let mut assembler = FrameAssembler::new();
        assembler.started = true;
        assembler.timestamp = 7;
        assembler.next_seq = Some(1);
        assembler.buffer.resize(MAX_H264_ACCESS_UNIT_BYTES, 0);

        assert!(assembler.push(1, 7, true, &[NAL_HEADER, 0xAA]).is_none());
        assert!(assembler.buffer.is_empty());

        let frame = assembler.push(2, 8, true, &[NAL_HEADER, 0xBB]).unwrap();
        assert_eq!(frame, [0, 0, 0, 1, NAL_HEADER, 0xBB]);
    }

    #[test]
    fn encoder_rejects_frames_above_pixel_limit() {
        let mut codec = H264Encoder::new().unwrap();
        let frame = VideoFrame::new(4097, 2160, PixelData::Rgba8(Vec::new()));
        assert!(codec.encode(&frame).is_err());
    }

    #[test]
    fn encoder_accepts_rgb8_frame() {
        let mut codec = H264Encoder::new().unwrap();
        codec.request_keyframe();
        let (width, height) = (640, 360);
        let frame = VideoFrame::new(
            width,
            height,
            PixelData::Rgb8([0, 0, 0].repeat((width * height) as usize)),
        );
        assert!(codec.encode(&frame).unwrap().is_some());
    }
}
