//! AV1 RTP payload packetization per the AOMedia AV1 RTP specification.
//!
//! Encoder output uses AV1 low-overhead OBU framing. RTP OBU elements omit
//! each OBU's internal LEB128 size field, and large elements are fragmented
//! with the aggregation header's Z/Y continuation bits.

use std::io;

/// Match the existing H264 packetizer's payload ceiling. One byte belongs to
/// the AV1 aggregation header; remaining bytes carry one OBU element.
pub(super) const MAX_AV1_RTP_PAYLOAD: usize = 1_100;

const AGGREGATION_Z: u8 = 0x80;
const AGGREGATION_Y: u8 = 0x40;
#[cfg(test)]
const AGGREGATION_W_ONE: u8 = 0x10;
const AGGREGATION_N: u8 = 0x08;
const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_TILE_LIST: u8 = 8;
const OBU_PADDING: u8 = 15;

#[derive(Debug)]
struct Obu<'a> {
    header: Vec<u8>,
    payload: &'a [u8],
    obu_type: u8,
}

impl Obu<'_> {
    fn len(&self) -> usize {
        self.header.len() + self.payload.len()
    }

    fn extend_range(&self, output: &mut Vec<u8>, start: usize, len: usize) {
        let end = start + len;
        debug_assert!(end <= self.len());

        if start < self.header.len() {
            output.extend_from_slice(&self.header[start..end.min(self.header.len())]);
        }
        if end > self.header.len() {
            let payload_start = start.saturating_sub(self.header.len());
            let payload_end = end - self.header.len();
            output.extend_from_slice(&self.payload[payload_start..payload_end]);
        }
    }
}

/// Packet layout calculated before serialization. Sizes exclude the one-byte
/// AV1 aggregation header.
#[derive(Debug)]
struct Packet {
    first_obu: usize,
    first_obu_offset: usize,
    obu_elements: usize,
    last_obu_size: usize,
    payload_size: usize,
}

impl Packet {
    fn new(first_obu: usize) -> Self {
        Self {
            first_obu,
            first_obu_offset: 0,
            obu_elements: 0,
            last_obu_size: 0,
            payload_size: 0,
        }
    }
}

/// Parse one unsigned LEB128 value and return `(value, encoded length)`.
fn parse_leb128(input: &[u8]) -> Result<(usize, usize), io::Error> {
    let mut value = 0u128;
    for (index, byte) in input.iter().copied().enumerate().take(10) {
        let shift = index * 7;
        value |= u128::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return usize::try_from(value)
                .map(|value| (value, index + 1))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "AV1 OBU size overflow"));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "unterminated AV1 OBU size",
    ))
}

fn parse_obus(mut input: &[u8]) -> Result<Vec<Obu<'_>>, io::Error> {
    let mut obus = Vec::new();
    while !input.is_empty() {
        let header_byte = input[0];
        if header_byte & 0x81 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid AV1 OBU header",
            ));
        }
        let obu_type = (header_byte >> 3) & 0x0f;
        let extension_flag = header_byte & 0x04 != 0;
        let has_size_field = header_byte & 0x02 != 0;
        let header_len = 1 + usize::from(extension_flag);
        if input.len() < header_len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated AV1 OBU extension header",
            ));
        }

        let (payload, consumed) = if has_size_field {
            let (payload_len, size_len) = parse_leb128(&input[header_len..])?;
            let payload_start = header_len.checked_add(size_len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "AV1 OBU length overflow")
            })?;
            let total_len = payload_start.checked_add(payload_len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "AV1 OBU length overflow")
            })?;
            if total_len > input.len() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated AV1 OBU payload",
                ));
            }
            (&input[payload_start..total_len], total_len)
        } else {
            // Without a size field, one OBU consumes the rest of the temporal
            // unit. No boundary for another low-overhead OBU exists.
            (&input[header_len..], input.len())
        };

        if !matches!(
            obu_type,
            OBU_TEMPORAL_DELIMITER | OBU_TILE_LIST | OBU_PADDING
        ) {
            let mut header = input[..header_len].to_vec();
            // RTP's OBU-element length provides framing; internal size fields
            // should be absent on the wire.
            header[0] &= !0x02;
            obus.push(Obu {
                header,
                payload,
                obu_type,
            });
        }
        input = &input[consumed..];
    }
    Ok(obus)
}

fn leb128_size(mut value: usize) -> usize {
    let mut size = 1;
    while value >= 0x80 {
        size += 1;
        value >>= 7;
    }
    size
}

fn write_leb128(mut value: usize, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

/// Largest fragment whose bytes and LEB128 element length fit in `remaining`.
fn max_sized_fragment(remaining: usize) -> usize {
    if remaining <= 1 {
        return 0;
    }

    let mut fragment = remaining - 1;
    while fragment + leb128_size(fragment) > remaining {
        fragment -= 1;
    }
    fragment
}

/// Adding another OBU makes the previous last element non-final, so it may
/// need the length field that W previously allowed it to omit.
fn previous_obu_length_size(packet: &Packet) -> usize {
    if packet.payload_size == 0 || packet.obu_elements > 3 {
        0
    } else {
        leb128_size(packet.last_obu_size)
    }
}

/// Mirror WebRTC's greedy packet layout: fill remaining packet capacity with
/// later OBUs, fragmenting the final element when necessary. Besides reducing
/// overhead, this prevents packet-aligned OBUs from becoming separate encoded
/// chunks before Discord's DAVE decrypting frame transform.
fn plan_packets(obus: &[Obu<'_>]) -> Vec<Packet> {
    if obus.is_empty() {
        return Vec::new();
    }

    let max_packet_data = MAX_AV1_RTP_PAYLOAD - 1;
    let mut packets = vec![Packet::new(0)];
    let mut remaining = max_packet_data;

    for (obu_index, obu) in obus.iter().enumerate() {
        let obu_size = obu.len();
        let mut previous_length_size = previous_obu_length_size(
            packets
                .last()
                .expect("packet plan must contain an active packet"),
        );
        let minimum_current_size = if packets
            .last()
            .is_some_and(|packet| packet.obu_elements >= 3)
        {
            // W=0 requires a one-byte length plus at least one OBU byte.
            2
        } else {
            1
        };
        if remaining < previous_length_size + minimum_current_size {
            packets.push(Packet::new(obu_index));
            remaining = max_packet_data;
            previous_length_size = 0;
        }

        let packet = packets
            .last_mut()
            .expect("packet plan must contain an active packet");
        packet.payload_size += previous_length_size;
        remaining -= previous_length_size;
        packet.obu_elements += 1;

        let current_needs_length = packet.obu_elements > 3;
        let current_length_size = if current_needs_length {
            leb128_size(obu_size)
        } else {
            0
        };
        if obu_size + current_length_size <= remaining {
            packet.last_obu_size = obu_size;
            packet.payload_size += obu_size + current_length_size;
            remaining -= obu_size + current_length_size;
            continue;
        }

        let first_fragment_size = if current_needs_length {
            max_sized_fragment(remaining)
        } else {
            remaining
        }
        .min(obu_size - 1);

        if first_fragment_size == 0 {
            // Leave the previous element as this packet's last element. Its
            // temporarily reserved length is no longer needed.
            packet.obu_elements -= 1;
            packet.payload_size -= previous_length_size;
        } else {
            packet.last_obu_size = first_fragment_size;
            packet.payload_size += first_fragment_size;
            if current_needs_length {
                packet.payload_size += leb128_size(first_fragment_size);
            }
        }

        let mut obu_offset = first_fragment_size;
        while obu_offset + max_packet_data < obu_size {
            let mut middle = Packet::new(obu_index);
            middle.first_obu_offset = obu_offset;
            middle.obu_elements = 1;
            middle.last_obu_size = max_packet_data;
            middle.payload_size = max_packet_data;
            packets.push(middle);
            obu_offset += max_packet_data;
        }

        let last_fragment_size = obu_size - obu_offset;
        let mut last = Packet::new(obu_index);
        last.first_obu_offset = obu_offset;
        last.obu_elements = 1;
        last.last_obu_size = last_fragment_size;
        last.payload_size = last_fragment_size;
        packets.push(last);
        remaining = max_packet_data - last_fragment_size;
    }

    packets
}

/// Packetize one temporal unit. Callback receives RTP payload and marker bit.
pub(super) fn packetize_temporal_unit(
    temporal_unit: &[u8],
    is_keyframe: bool,
    mut for_each: impl FnMut(&[u8], bool),
) -> Result<(), io::Error> {
    let obus = parse_obus(temporal_unit)?;
    let packets = plan_packets(&obus);
    let last_packet = packets.len().saturating_sub(1);

    for (packet_index, layout) in packets.iter().enumerate() {
        let last_obu_index = layout.first_obu + layout.obu_elements - 1;
        let last_obu_offset = if layout.obu_elements == 1 {
            layout.first_obu_offset
        } else {
            0
        };
        let continues_previous = layout.first_obu_offset > 0;
        let continues_next = last_obu_offset + layout.last_obu_size < obus[last_obu_index].len();

        let mut aggregation = 0;
        if continues_previous {
            aggregation |= AGGREGATION_Z;
        }
        if continues_next {
            aggregation |= AGGREGATION_Y;
        }
        if layout.obu_elements <= 3 {
            aggregation |= (layout.obu_elements as u8) << 4;
        }
        if packet_index == 0 && is_keyframe && obus[0].obu_type == OBU_SEQUENCE_HEADER {
            aggregation |= AGGREGATION_N;
        }

        let mut packet = Vec::with_capacity(layout.payload_size + 1);
        packet.push(aggregation);
        let mut obu_offset = layout.first_obu_offset;
        for element_index in 0..layout.obu_elements {
            let obu = &obus[layout.first_obu + element_index];
            let is_last_element = element_index + 1 == layout.obu_elements;
            let fragment_size = if is_last_element {
                layout.last_obu_size
            } else {
                obu.len() - obu_offset
            };
            if !is_last_element || layout.obu_elements > 3 {
                write_leb128(fragment_size, &mut packet);
            }
            obu.extend_range(&mut packet, obu_offset, fragment_size);
            obu_offset = 0;
        }
        debug_assert_eq!(packet.len(), layout.payload_size + 1);
        debug_assert!(packet.len() <= MAX_AV1_RTP_PAYLOAD);
        for_each(&packet, packet_index == last_packet);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use davey::{Codec, DAVE_PROTOCOL_VERSION, DaveSession, MediaType, ProposalsOperationType};
    use openmls::prelude::tls_codec::Serialize as _;
    use openmls::prelude::{
        BasicCredential, DeserializeBytes as _, ExternalProposal, ExternalSender, KeyPackageIn,
        OpenMlsProvider as _, ProtocolVersion, SenderExtensionIndex, VLBytes,
    };
    use openmls_basic_credential::SignatureKeyPair;
    use openmls_rust_crypto::OpenMlsRustCrypto;
    use simple_video::{Av1Encoder, Av1EncoderUsage, PixelData, VideoFrame};
    use std::num::NonZeroU16;

    const ALICE_USER_ID: u64 = 1;
    const BOB_USER_ID: u64 = 2;
    const TEST_CHANNEL_ID: u64 = 3;

    fn sized_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < 128);
        let mut obu = vec![(obu_type << 3) | 0x02, payload.len() as u8];
        obu.extend_from_slice(payload);
        obu
    }

    #[test]
    fn packetizes_single_obu_without_internal_size() {
        let temporal_unit = sized_obu(6, &[1, 2, 3]);
        let mut packets = Vec::new();
        packetize_temporal_unit(&temporal_unit, false, |payload, marker| {
            packets.push((payload.to_vec(), marker));
        })
        .unwrap();
        assert_eq!(packets, vec![(vec![0x10, 6 << 3, 1, 2, 3], true)]);
    }

    #[test]
    fn drops_temporal_delimiter_and_marks_sequence_header() {
        let temporal_unit = [sized_obu(2, &[]), sized_obu(1, &[9]), sized_obu(6, &[7])].concat();
        let mut packets = Vec::new();
        packetize_temporal_unit(&temporal_unit, true, |payload, marker| {
            packets.push((payload.to_vec(), marker));
        })
        .unwrap();
        assert_eq!(packets, vec![(vec![0x28, 2, 1 << 3, 9, 6 << 3, 7], true)]);
    }

    #[test]
    fn aggregates_four_obus_with_explicit_lengths() {
        let temporal_unit = [
            sized_obu(1, &[1]),
            sized_obu(3, &[2]),
            sized_obu(5, &[3]),
            sized_obu(6, &[4]),
        ]
        .concat();
        let mut packets = Vec::new();
        packetize_temporal_unit(&temporal_unit, true, |payload, marker| {
            packets.push((payload.to_vec(), marker));
        })
        .unwrap();

        assert_eq!(
            packets,
            vec![(
                vec![
                    AGGREGATION_N,
                    2,
                    1 << 3,
                    1,
                    2,
                    3 << 3,
                    2,
                    2,
                    5 << 3,
                    3,
                    2,
                    6 << 3,
                    4,
                ],
                true,
            )]
        );
    }

    #[test]
    fn non_keyframe_sequence_header_does_not_set_new_sequence_flag() {
        let temporal_unit = sized_obu(OBU_SEQUENCE_HEADER, &[9]);
        let mut packet = Vec::new();
        packetize_temporal_unit(&temporal_unit, false, |payload, _| {
            packet.extend_from_slice(payload);
        })
        .unwrap();

        assert_eq!(packet[0] & AGGREGATION_N, 0);
    }

    #[test]
    fn fragments_obu_with_continuation_flags() {
        let payload = vec![0xaa; MAX_AV1_RTP_PAYLOAD * 2];
        let mut temporal_unit = vec![(6 << 3) | 0x02, 0x98, 0x11];
        temporal_unit.extend_from_slice(&payload);
        let mut packets = Vec::new();
        packetize_temporal_unit(&temporal_unit, false, |packet, marker| {
            packets.push((packet.to_vec(), marker));
        })
        .unwrap();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].0[0], AGGREGATION_W_ONE | AGGREGATION_Y);
        assert_eq!(
            packets[1].0[0],
            AGGREGATION_W_ONE | AGGREGATION_Z | AGGREGATION_Y
        );
        assert_eq!(packets[2].0[0], AGGREGATION_W_ONE | AGGREGATION_Z);
        assert!(!packets[0].1);
        assert!(!packets[1].1);
        assert!(packets[2].1);
    }

    #[test]
    fn rejects_truncated_sized_obu() {
        let err = packetize_temporal_unit(&[(6 << 3) | 0x02, 4, 1], false, |_, _| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    fn discord_depacketize(packets: &[Vec<u8>]) -> Vec<u8> {
        let mut obu_elements: Vec<Vec<u8>> = Vec::new();
        let mut expecting_continuation = false;

        for packet in packets {
            let aggregation = packet[0];
            let continues_previous = aggregation & AGGREGATION_Z != 0;
            assert_eq!(continues_previous, expecting_continuation);
            let element_count = usize::from((aggregation >> 4) & 0x03);
            let mut input = &packet[1..];
            let mut element_index = 0;
            while !input.is_empty() {
                element_index += 1;
                let has_length = element_count == 0 || element_index != element_count;
                let fragment_size = if has_length {
                    let (size, consumed) = parse_leb128(input).unwrap();
                    input = &input[consumed..];
                    size
                } else {
                    input.len()
                };
                let (fragment, rest) = input.split_at(fragment_size);
                input = rest;
                if element_index == 1 && continues_previous {
                    obu_elements.last_mut().unwrap().extend_from_slice(fragment);
                } else {
                    obu_elements.push(fragment.to_vec());
                }
            }
            if element_count != 0 {
                assert_eq!(element_index, element_count);
            }
            expecting_continuation = aggregation & AGGREGATION_Y != 0;
        }
        assert!(!expecting_continuation);

        let last_obu = obu_elements.len() - 1;
        let mut temporal_unit = Vec::new();
        for (index, obu) in obu_elements.into_iter().enumerate() {
            let header_len = 1 + usize::from(obu[0] & 0x04 != 0);
            let mut header = obu[..header_len].to_vec();
            if index == last_obu {
                header[0] &= !0x02;
            } else {
                header[0] |= 0x02;
            }
            temporal_unit.extend_from_slice(&header);
            if index != last_obu {
                write_leb128(obu.len() - header_len, &mut temporal_unit);
            }
            temporal_unit.extend_from_slice(&obu[header_len..]);
        }
        temporal_unit
    }

    /// Simulate WebRTC's AV1 `Parse` flags plus packet-buffer grouping before
    /// Discord's patched `AssembleFrame` runs. `Z=0` starts a frame and `Y=0`
    /// ends one; the RTP marker can also end the current frame.
    fn web_rtc_frame_groups(packets: &[(Vec<u8>, bool)]) -> Vec<Vec<Vec<u8>>> {
        let mut groups = Vec::new();
        let mut current = Vec::new();

        for (packet, marker) in packets {
            let aggregation = packet[0];
            let starts_frame = aggregation & AGGREGATION_Z == 0;
            let ends_frame = aggregation & AGGREGATION_Y == 0 || *marker;

            if starts_frame {
                assert!(
                    current.is_empty(),
                    "new WebRTC AV1 frame started before previous frame ended"
                );
            } else {
                assert!(
                    !current.is_empty(),
                    "continued WebRTC AV1 frame has no starting packet"
                );
            }

            current.push(packet.clone());
            if ends_frame {
                groups.push(std::mem::take(&mut current));
            }
        }

        assert!(current.is_empty(), "WebRTC AV1 frame never ended");
        groups
    }

    fn ready_dave_pair() -> (DaveSession, DaveSession) {
        let version = NonZeroU16::new(DAVE_PROTOCOL_VERSION).unwrap();
        let mut alice = DaveSession::new(version, ALICE_USER_ID, TEST_CHANNEL_ID, None).unwrap();
        let mut bob = DaveSession::new(version, BOB_USER_ID, TEST_CHANNEL_ID, None).unwrap();

        let delivery_service_signer =
            SignatureKeyPair::new(alice.ciphersuite().signature_algorithm()).unwrap();
        let external_sender = ExternalSender::new(
            delivery_service_signer.public().into(),
            BasicCredential::new(b"test-delivery-service".to_vec()).into(),
        )
        .tls_serialize_detached()
        .unwrap();
        alice.set_external_sender(&external_sender).unwrap();
        bob.set_external_sender(&external_sender).unwrap();

        let bob_key_package = bob.create_key_package().unwrap();
        let key_package_provider = OpenMlsRustCrypto::default();
        let bob_key_package = KeyPackageIn::tls_deserialize_exact_bytes(&bob_key_package)
            .unwrap()
            .validate(key_package_provider.crypto(), ProtocolVersion::Mls10)
            .unwrap();
        let (group_id, epoch) = {
            let group = alice
                .group()
                .expect("external sender creates pending group");
            (group.group_id().clone(), group.epoch())
        };
        let proposal = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            bob_key_package,
            group_id,
            epoch,
            &delivery_service_signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap();
        let proposals = VLBytes::new(proposal.tls_serialize_detached().unwrap())
            .tls_serialize_detached()
            .unwrap();
        let commit_welcome = alice
            .process_proposals(
                ProposalsOperationType::APPEND,
                &proposals,
                Some(&[BOB_USER_ID]),
            )
            .unwrap()
            .expect("add proposal must produce commit and welcome");

        bob.process_welcome(
            commit_welcome
                .welcome
                .as_deref()
                .expect("member add commit must include welcome"),
        )
        .unwrap();
        alice.process_commit(&commit_welcome.commit).unwrap();
        assert!(alice.is_ready());
        assert!(bob.is_ready());
        (alice, bob)
    }

    #[test]
    fn preserves_synthetic_dave_layout_across_discord_depacketization() {
        let mut temporal_unit = sized_obu(OBU_SEQUENCE_HEADER, &[9, 8, 7]);
        temporal_unit.push(6 << 3); // DAVE removes the final OBU size field.
        temporal_unit.extend(std::iter::repeat_n(0xaa, MAX_AV1_RTP_PAYLOAD * 2));
        temporal_unit.extend_from_slice(&[0x0b, 0xfa, 0xfa]); // DAVE footer size + marker.

        let mut packets = Vec::new();
        packetize_temporal_unit(&temporal_unit, true, |payload, _| {
            packets.push(payload.to_vec());
        })
        .unwrap();

        assert!(packets.len() > 1);
        assert!(
            packets[..packets.len() - 1]
                .iter()
                .all(|packet| packet[0] & AGGREGATION_Y != 0),
            "non-final packets must remain part of one WebRTC encoded chunk"
        );
        assert_eq!(discord_depacketize(&packets), temporal_unit);
    }

    #[test]
    fn rav1e_dave_frame_survives_discord_webrtc_round_trip() {
        let frame = VideoFrame::new(64, 64, PixelData::Rgba8([24, 48, 72, 255].repeat(64 * 64)));
        let mut encoder = Av1Encoder::new().unwrap();
        encoder.request_keyframe();
        let encoded = (0..8)
            .find_map(|_| encoder.encode(&frame).unwrap())
            .expect("low-latency rav1e must emit within eight frames");

        let (mut alice, mut bob) = ready_dave_pair();
        let encrypted = alice
            .encrypt(MediaType::VIDEO, Codec::AV1, &encoded.data)
            .unwrap()
            .into_owned();
        assert!(encrypted.ends_with(&[0xfa, 0xfa]));

        let mut packets = Vec::new();
        packetize_temporal_unit(&encrypted, encoded.keyframe, |payload, marker| {
            packets.push((payload.to_vec(), marker));
        })
        .unwrap();
        let groups = web_rtc_frame_groups(&packets);
        assert_eq!(
            groups.len(),
            1,
            "one DAVE transform must remain one WebRTC encoded frame"
        );

        let reconstructed = discord_depacketize(&groups[0]);
        assert_eq!(reconstructed, encrypted);
        let decrypted = bob
            .decrypt(ALICE_USER_ID, MediaType::VIDEO, &reconstructed)
            .expect("receiver must authenticate and decrypt reconstructed AV1 frame");
        assert!(!decrypted.is_empty());
        assert!(!decrypted.ends_with(&[0xfa, 0xfa]));
    }

    #[test]
    fn packetizes_real_rav1e_temporal_unit() {
        let frame = VideoFrame::new(64, 64, PixelData::Rgba8([24, 48, 72, 255].repeat(64 * 64)));
        let mut encoder = Av1Encoder::new().unwrap();
        encoder.request_keyframe();
        let encoded = (0..8)
            .find_map(|_| encoder.encode(&frame).unwrap())
            .expect("low-latency rav1e must emit within eight frames");

        let mut packets = Vec::new();
        packetize_temporal_unit(&encoded.data, encoded.keyframe, |payload, marker| {
            packets.push((payload.to_vec(), marker));
        })
        .unwrap();

        assert!(!packets.is_empty());
        assert!(
            packets
                .iter()
                .all(|(payload, _)| payload.len() <= MAX_AV1_RTP_PAYLOAD)
        );
        assert_eq!(packets.iter().filter(|(_, marker)| *marker).count(), 1);
        assert!(packets.last().is_some_and(|(_, marker)| *marker));
    }

    #[test]
    fn sustained_screen_frames_survive_dave_and_rtp_round_trip() {
        let (width, height) = (1280, 720);
        let mut encoder =
            Av1Encoder::new_for_frame_rate_and_bitrate(Av1EncoderUsage::Screen, 30, 2_000_000)
                .unwrap();
        let (mut alice, mut bob) = ready_dave_pair();
        encoder.request_keyframe();
        let mut emitted = 0;

        for frame_index in 0..12_u32 {
            let mut pixels = vec![0; width * height * 3];
            for (pixel_index, pixel) in pixels.chunks_exact_mut(3).enumerate() {
                let x = pixel_index % width;
                let y = pixel_index / width;
                pixel[0] = (x as u32 + frame_index * 17) as u8;
                pixel[1] = (y as u32 + frame_index * 29) as u8;
                pixel[2] = ((x ^ y) as u32 + frame_index * 37) as u8;
            }
            let frame = VideoFrame::new(width as u32, height as u32, PixelData::Rgb8(pixels));
            let Some(encoded) = encoder.encode(&frame).unwrap() else {
                continue;
            };
            emitted += 1;

            let encrypted = alice
                .encrypt(MediaType::VIDEO, Codec::AV1, &encoded.data)
                .unwrap()
                .into_owned();
            let mut packets = Vec::new();
            packetize_temporal_unit(&encrypted, encoded.keyframe, |payload, marker| {
                packets.push((payload.to_vec(), marker));
            })
            .unwrap();
            let groups = web_rtc_frame_groups(&packets);
            assert_eq!(groups.len(), 1);
            let reconstructed = discord_depacketize(&groups[0]);
            assert_eq!(reconstructed, encrypted);
            let decrypted = bob
                .decrypt(ALICE_USER_ID, MediaType::VIDEO, &reconstructed)
                .unwrap();
            assert!(!parse_obus(&decrypted).unwrap().is_empty());
        }

        assert!(emitted >= 8, "rav1e emitted only {emitted}/12 frames");
    }
}
