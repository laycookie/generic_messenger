use std::{
    io,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

use facet_pretty::FacetPretty as _;
use messenger_interface::{interface::VoiceEvent, stream::WeakStreamSource, types::Identifier};
use smol::net::UdpSocket;
use tracing::{debug, error, trace, warn};

use async_tungstenite::tungstenite::Message;

use super::events::{execute_pending_transition, reinit_dave_session, respond_to_mls_transition};
use super::{
    ConnectionRole, Media, VoiceOpcode,
    connection::{Connection, EncryptionMode, SessionDescription},
    gateway::MediaConnection,
    payloads::{
        ClientDisconnectPayload, DAVEPrepareEpoch, MediaSinkWantsPayload, MlsProposalsPayload,
        MlsTransitionPayload, ReadyPayload, SelectProtocolConnection, SelectProtocolPayload,
        SessionUpdateEvent, SpeakingPayload, VideoPayload, supported_codecs,
    },
};
use crate::DiscordRuntimeState;
use crate::gateways::{Gateway, GatewayFrame};

// Local types for IP discovery packet layout.
// Only used in the VoiceOpcode::Ready handler below.
// <https://docs.discord.food/topics/voice-connections#ip-discovery>
struct IpDiscovery {
    /// 1 = request, 2 = response.
    req_or_res: u16,
    length: u16,
    ssrc: u32,
    address_ascii: [u8; 64],
    port: u16,
}

/// Opcode 28 payload is MLS Commit followed by optional Welcome. Welcome has
/// no outer length prefix, so reversing these messages makes member-add
/// transitions unparseable and strands late Go Live viewers outside DAVE.
fn mls_commit_welcome_payload(mut commit: Vec<u8>, welcome: Option<Vec<u8>>) -> Vec<u8> {
    if let Some(welcome) = welcome {
        commit.extend(welcome);
    }
    commit
}

#[cfg(test)]
#[allow(
    clippy::items_after_test_module,
    reason = "codec test stays beside private wire-layout type it verifies"
)]
mod tests {
    use super::*;

    #[test]
    fn ip_discovery_codec_is_byte_exact_without_layout_casts() {
        let mut address_ascii = [0; 64];
        address_ascii[..9].copy_from_slice(b"127.0.0.1");
        let packet = IpDiscovery {
            req_or_res: 2,
            length: 70,
            ssrc: 0x1122_3344,
            address_ascii,
            port: 50_000,
        };
        let bytes = packet.encode();
        assert_eq!(&bytes[0..2], &2u16.to_be_bytes());
        assert_eq!(&bytes[2..4], &70u16.to_be_bytes());
        assert_eq!(&bytes[4..8], &0x1122_3344u32.to_be_bytes());
        assert_eq!(IpDiscovery::decode(&bytes).ssrc, packet.ssrc);
        assert_eq!(IpDiscovery::decode(&bytes).port, packet.port);
        assert_eq!(IpDiscovery::decode(&bytes).address_ascii, address_ascii);
    }

    #[test]
    fn mls_commit_precedes_optional_welcome() {
        assert_eq!(
            mls_commit_welcome_payload(vec![1, 2], Some(vec![3, 4])),
            vec![1, 2, 3, 4]
        );
        assert_eq!(mls_commit_welcome_payload(vec![1, 2], None), vec![1, 2]);
    }
}

impl IpDiscovery {
    const BYTE_LEN: usize = 74;

    fn encode(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0; Self::BYTE_LEN];
        bytes[0..2].copy_from_slice(&self.req_or_res.to_be_bytes());
        bytes[2..4].copy_from_slice(&self.length.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.ssrc.to_be_bytes());
        bytes[8..72].copy_from_slice(&self.address_ascii);
        bytes[72..74].copy_from_slice(&self.port.to_be_bytes());
        bytes
    }

    fn decode(bytes: &[u8; Self::BYTE_LEN]) -> Self {
        let mut address_ascii = [0; 64];
        address_ascii.copy_from_slice(&bytes[8..72]);
        Self {
            req_or_res: u16::from_be_bytes(bytes[0..2].try_into().unwrap()),
            length: u16::from_be_bytes(bytes[2..4].try_into().unwrap()),
            ssrc: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            address_ascii,
            port: u16::from_be_bytes(bytes[72..74].try_into().unwrap()),
        }
    }
}

impl GatewayFrame<VoiceOpcode> {
    /// Handle one voice-gateway opcode against `voice_gateway`. The gateway is
    /// passed in (rather than loaded from `discord`) so the same handler serves
    /// both the call's voice connection and a Go Live stream's separate one;
    /// `role` selects the two behaviours that diverge (see [`ConnectionRole`]).
    ///
    /// <https://docs.discord.food/topics/voice-connections#voice-opcodes>
    pub async fn exec(
        self,
        discord: &DiscordRuntimeState,
        voice_gateway: &Arc<Gateway<Media>>,
        role: ConnectionRole,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let payload = self.payload()?;
        let remote_video_kind = role.remote_video_kind();
        if let Some(s) = payload.sequence() {
            voice_gateway
                .state
                .last_sequence_number
                .get_or_init(|| s.into())
                .store(s, Ordering::Relaxed);
        };

        debug!("VoiceOpcode: {:?}", payload.opcode());
        match payload.opcode() {
            VoiceOpcode::SessionDescription => {
                let session_description = payload.decode::<SessionDescription>()?;
                let dave_protocol_version = session_description.dave_protocol_version();
                debug!(
                    "Session description: video_codec={} dave_protocol_version={}",
                    session_description.video_codec(),
                    dave_protocol_version
                );

                // Every nonzero Select Protocol ACK requires a fresh MLS key
                // package, including repeated transport descriptions.
                let profile = discord.cache.profile.load();
                let profile = profile.as_ref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "user profile not loaded")
                })?;
                trace!("SessionDescription: waiting for dave_session lock");
                let key_package = {
                    let mut dave_session = voice_gateway.dave_session.lock().await;
                    trace!("SessionDescription: dave_session lock acquired");
                    reinit_dave_session(
                        &mut dave_session,
                        dave_protocol_version,
                        voice_gateway.dave_group_id,
                        profile.id,
                    )?
                };
                voice_gateway
                    .websocket
                    .send_binary(VoiceOpcode::MLSKeyPackage as u8, key_package.into_iter())
                    .await?;
                trace!("SessionDescription: DAVE session initialized, key package sent");

                // Commit description to connection. A repeated session
                // description is a key rotation: the swap makes the new
                // keys take effect (send/recv load per packet), and we
                // must not announce a second call stream for it.
                let first_description = match voice_gateway.connection.load().as_ref() {
                    Some(connection) => connection.set_description(session_description),
                    None => {
                        warn!("Session description arrived before the UDP connection was set up");
                        true
                    }
                };

                // Go Live connections join existing shared coordination; only
                // call connection creates media event stream.
                if first_description && role == ConnectionRole::Call {
                    // Runs while holding both receiver locks.
                    trace!("SessionDescription: creating audio stream");
                    if let Some(gateway) = discord.gateway.current.load().as_ref()
                        && let Some(audio) = gateway.voice.open_audio_source_for(voice_gateway)
                    {
                        gateway.events.submit_voice(VoiceEvent::CallStreamReady {
                            room: Identifier::new(voice_gateway.channel_id, ()),
                            source: WeakStreamSource::from_arc(&audio),
                        });
                    }
                    trace!("SessionDescription: CallStreamReady emitted");
                }
            }
            VoiceOpcode::Speaking => {
                let speaking = payload.decode::<SpeakingPayload>()?;

                if let Some(connection) = voice_gateway.connection.load_full() {
                    connection
                        .ssrc_to_user_id
                        .insert(speaking.ssrc, speaking.user_id);
                }
            }
            VoiceOpcode::Video => {
                // Keep raw payload logging so negotiated stream state can be
                // compared with our outgoing announce during RTC debugging.
                debug!(
                    "Video payload: {}",
                    payload.raw_json().unwrap_or("<binary>")
                );
                let video = payload.decode::<VideoPayload>()?;

                // Only the server→client form names a user; our own echoed
                // announcements (client→server has no user_id) carry nothing
                // to route.
                let Some(user_id) = video.user_id else {
                    return Ok(());
                };
                let Some(connection) = voice_gateway.connection.load_full() else {
                    warn!("Video event arrived before the UDP connection was set up");
                    return Ok(());
                };

                // The audio SSRC arrives here too — same mapping Speaking
                // maintains, learned earlier for users who show video before
                // ever speaking (DAVE decrypt needs it per packet).
                if let Some(audio_ssrc) = video.audio_ssrc
                    && audio_ssrc != 0
                {
                    connection.ssrc_to_user_id.insert(audio_ssrc, user_id);
                }

                // `streams` is authoritative when present. Older/audio-only
                // announcements may omit it, so retain top-level video_ssrc as
                // the compatibility path.
                let announced_streams = match video.streams.as_deref() {
                    Some(streams) => streams
                        .iter()
                        .filter_map(|stream| {
                            let ssrc = stream.ssrc.filter(|ssrc| *ssrc != 0)?;
                            let rtx_ssrc = stream
                                .rtx_ssrc
                                .filter(|rtx_ssrc| *rtx_ssrc != 0)
                                .or_else(|| ssrc.checked_add(1));
                            Some((ssrc, rtx_ssrc))
                        })
                        .collect(),
                    None => video
                        .video_ssrc
                        .filter(|ssrc| *ssrc != 0)
                        .map(|ssrc| {
                            let rtx_ssrc = video
                                .rtx_ssrc
                                .filter(|rtx_ssrc| *rtx_ssrc != 0)
                                .or_else(|| ssrc.checked_add(1));
                            vec![(ssrc, rtx_ssrc)]
                        })
                        .unwrap_or_default(),
                };

                if announced_streams.is_empty() {
                    // Video ended (or an audio-only participant's periodic
                    // announcement): retire everything owned by this user. Log
                    // the end only when a stream existed — audio-only users
                    // produce these announcements constantly.
                    let stale = connection.retire_user_video(discord, user_id, remote_video_kind);
                    if !stale.is_empty() {
                        debug!("Video ended for user {user_id} (ssrcs {stale:?})");
                    }
                } else {
                    debug!("Video SSRCs {announced_streams:?} announced for user {user_id}");
                    for (video_ssrc, rtx_ssrc) in announced_streams {
                        connection.video_ssrc_to_user_id.insert(video_ssrc, user_id);
                        if let Some(rtx_ssrc) = rtx_ssrc {
                            connection.rtx_to_video_ssrc.insert(rtx_ssrc, video_ssrc);
                        }
                    }
                    // No lifecycle-only event here. Camera media arrives
                    // automatically and emits AddVideoStream on its first
                    // decoded frame. Go Live availability comes from the
                    // main gateway before this requested connection opens.
                }
            }
            VoiceOpcode::ClientDisconnect => {
                let disconnect = payload.decode::<ClientDisconnectPayload>()?;
                let user_id = disconnect.user_id;

                if let Some(connection) = voice_gateway.connection.load_full() {
                    connection.retire_user_video(discord, user_id, remote_video_kind);
                }
                // TODO: retire the user's *audio* SSRC mappings/channels here
                // too; today they linger until the call ends (harmless but
                // untidy — the roster leave event already hides the user).
            }
            VoiceOpcode::SessionUpdate => {
                let update = payload.decode::<SessionUpdateEvent>()?;
                if update.video_codec.is_some() || update.keyframe_interval.is_some() {
                    if let Some(connection) = voice_gateway.connection.load_full() {
                        if let Some(video_codec) = update.video_codec {
                            match connection.set_video_codec_name(&video_codec) {
                                Ok(true) => debug!("Voice video codec changed to {video_codec}"),
                                Ok(false) => trace!("Voice video codec remains {video_codec}"),
                                Err(err) => {
                                    warn!("Ignoring unsupported negotiated video codec: {err}")
                                }
                            }
                        }
                        if let Some(keyframe_interval_ms) = update.keyframe_interval {
                            if connection.set_video_keyframe_interval_ms(keyframe_interval_ms) {
                                debug!(
                                    "Voice video keyframe interval changed to {keyframe_interval_ms}ms"
                                );
                            } else {
                                trace!(
                                    "Voice video keyframe interval remains {keyframe_interval_ms}ms"
                                );
                            }
                        }
                    } else {
                        warn!("Session update arrived before UDP connection setup");
                    }
                }
            }
            VoiceOpcode::Ready => {
                let ready = payload.decode::<ReadyPayload>()?;
                debug!("Voice ready: {}", ready.pretty());

                // TODO: Not hard code it maybe?
                if !ready
                    .modes
                    .contains(&EncryptionMode::aead_xchacha20_poly1305_rtpsize)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "required encryption mode not available",
                    )
                    .into());
                }

                let mut address_ascii = [0; 64];
                if ready.ip.len() > address_ascii.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "voice server address exceeds IP discovery field",
                    )
                    .into());
                }
                address_ascii[..ready.ip.len()].copy_from_slice(ready.ip.as_bytes());

                let send_ip_discovery = IpDiscovery {
                    req_or_res: 1,
                    length: 70,
                    ssrc: ready.ssrc,
                    address_ascii,
                    port: ready.port,
                }
                .encode();
                let udp = UdpSocket::bind("0.0.0.0:0").await?;
                debug!("Addr: {:?}", udp.local_addr());
                udp.connect((ready.ip.as_str(), ready.port)).await?;
                udp.send(&send_ip_discovery).await?;

                // Wait for the discovery *response* (type 2). A recv error
                // is fatal — proceeding would select the protocol with a
                // zeroed address. Skip unrelated packets (keepalives/early
                // RTP can land before the response), within a sane bound.
                const MAX_DISCOVERY_ATTEMPTS: usize = 64;
                let mut buf = [0u8; IpDiscovery::BYTE_LEN];
                let recv_ip_discovery = 'discovery: {
                    for _ in 0..MAX_DISCOVERY_ATTEMPTS {
                        let len = udp.recv(&mut buf).await?;
                        if len != buf.len() {
                            debug!("Ignoring non-discovery packet ({len} bytes)");
                            continue;
                        }
                        let response = IpDiscovery::decode(&buf);
                        if response.req_or_res != 2 {
                            debug!("Ignoring IP discovery packet that is not a response");
                            continue;
                        }
                        debug!("Got IP discovery response\n{buf:?}");
                        break 'discovery response;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "no IP discovery response among received packets",
                    )
                    .into());
                };

                let mut ip_address = str::from_utf8(&recv_ip_discovery.address_ascii)?;
                if let Some(null_position) =
                    recv_ip_discovery.address_ascii.iter().position(|c| *c == 0)
                {
                    ip_address = &ip_address[..null_position]
                };
                // Send-capable connections use server-assigned video + RTX
                // SSRCs, with the historical audio+1/+2 fallback. Watch-only
                // Go Live connections have no outbound video SSRC.
                let audio_ssrc = recv_ip_discovery.ssrc;
                let assigned_pair = |rid: &str| {
                    ready
                        .streams
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .filter(|stream| stream.rid.as_deref() == Some(rid))
                        .find_map(|stream| {
                            let ssrc = stream.ssrc.filter(|s| *s != 0)?;
                            Some((ssrc, stream.rtx_ssrc.unwrap_or(ssrc + 1)))
                        })
                };
                let (video_ssrc, rtx_ssrc) = if voice_gateway.outbound_video_kind.is_some() {
                    assigned_pair("100").unwrap_or((audio_ssrc + 1, audio_ssrc + 2))
                } else {
                    (0, 0)
                };
                debug!("Voice SSRCs: audio={audio_ssrc} video={video_ssrc} rtx={rtx_ssrc}");
                {
                    voice_gateway.connection.store(Some(
                        MediaConnection::new(
                            Connection::new(udp, audio_ssrc, video_ssrc, rtx_ssrc),
                            voice_gateway.channel_id,
                        )
                        .into(),
                    ));
                }
                // <https://docs.discord.food/topics/voice-connections#select-protocol-structure>
                let protocol_select = SelectProtocolPayload::udp(
                    SelectProtocolConnection {
                        address: ip_address.to_owned(),
                        port: recv_ip_discovery.port,
                        mode: EncryptionMode::aead_xchacha20_poly1305_rtpsize
                            .as_str()
                            .to_owned(),
                    },
                    supported_codecs(
                        discord.media.preferred_video_codec(),
                        voice_gateway.outbound_video_kind.is_some(),
                    ),
                    ready.experiments,
                );
                let protocol_select = facet_json::to_vec(&protocol_select)?;
                voice_gateway
                    .websocket
                    .send(Message::Text(String::from_utf8(protocol_select)?.into()))
                    .await?;
            }
            VoiceOpcode::DAVEProtocolPrepareTransition => {
                let mut dave_session = voice_gateway.require_dave_session().await?;

                let packet = payload.decode::<DAVEPrepareEpoch>()?;

                let transition_id = packet.transition_id;

                voice_gateway
                    .dave_pending_transitions
                    .insert(transition_id, dave_session.protocol_version());
                tracing::trace!(
                    target: "video_timing",
                    stage = "outgoing.control",
                    kind = "DAVE",
                    action = "dave_transition_prepared",
                    transition_id,
                    protocol_version = u64::from(dave_session.protocol_version().get()),
                    pending_transitions = voice_gateway.dave_pending_transitions.len(),
                    "DAVE transition prepared"
                );

                if transition_id == 0 {
                    execute_pending_transition(
                        &mut dave_session,
                        &voice_gateway.dave_pending_transitions,
                        transition_id,
                    );
                } else {
                    // TODO
                    // Upon receiving this message, clients enable passthrough mode on their receive-side
                    // https://daveprotocol.com/#downgrade-to-transport-only-encryption
                }
            }
            VoiceOpcode::DAVEProtocolExecuteTransition => {
                let mut dave_session = voice_gateway.require_dave_session().await?;

                let packet = payload.decode::<DAVEPrepareEpoch>()?;
                let transition_id = packet.transition_id;
                execute_pending_transition(
                    &mut dave_session,
                    &voice_gateway.dave_pending_transitions,
                    transition_id,
                );
            }
            VoiceOpcode::DAVEProtocolPrepareEpoch => {
                let packet = payload.decode::<DAVEPrepareEpoch>()?;

                if packet.epoch == 1 {
                    // TODO: Investigate if this should be properly added
                    // this.daveProtocolVersion = packet.protocol_version;
                    let profile = discord.cache.profile.load();
                    let profile = profile.as_ref().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "user profile not loaded")
                    })?;
                    let key_package = {
                        let mut dave_session = voice_gateway.dave_session.lock().await;
                        reinit_dave_session(
                            &mut dave_session,
                            packet.protocol_version,
                            voice_gateway.dave_group_id,
                            profile.id,
                        )?
                    };
                    voice_gateway
                        .websocket
                        .send_binary(VoiceOpcode::MLSKeyPackage as u8, key_package.into_iter())
                        .await?;
                }
            }
            VoiceOpcode::MLSExternalSenderPackage => {
                let mut dave_session = voice_gateway.require_dave_session().await?;

                let bytes = payload.binary()?;
                if let Err(err) = dave_session.set_external_sender(bytes) {
                    error!("{err}");
                    return Err(err.into());
                };
            }
            VoiceOpcode::MLSProposals => {
                let bytes = payload.binary()?;
                let proposals = MlsProposalsPayload::try_from(bytes)?;

                let commit_welcome = {
                    let mut dave_session = voice_gateway.require_dave_session().await?;
                    match dave_session.process_proposals(
                        proposals.operation_type,
                        proposals.data,
                        // TODO: Add this for security purposes, should be received from CLIENTS_CONNECT
                        None,
                    ) {
                        Ok(welcome_message) => welcome_message,
                        Err(err) => {
                            error!("{err:?}");
                            return Err(err.into());
                        }
                    }
                };

                if let Some(commit_welcome) = commit_welcome {
                    let payload =
                        mls_commit_welcome_payload(commit_welcome.commit, commit_welcome.welcome);
                    voice_gateway
                        .websocket
                        .send_binary(VoiceOpcode::MLSCommitWelcome as u8, payload.into_iter())
                        .await?;
                } else {
                    error!("Potentially a problem?");
                }
            }
            VoiceOpcode::MLSAnnounceCommitTransition => {
                let bytes = payload.binary()?;
                let payload = MlsTransitionPayload::try_from(bytes)?;

                let process_started = Instant::now();
                let (protocol_version, result) = {
                    let mut dave_session = voice_gateway.require_dave_session().await?;
                    let protocol_version = dave_session.protocol_version();
                    if payload.transition_id != 0 {
                        // davey installs both receive and send ratchets during
                        // commit processing. Gate outbound media until Discord
                        // explicitly executes this transition (opcode 22).
                        voice_gateway
                            .dave_pending_transitions
                            .insert(payload.transition_id, protocol_version);
                    }
                    tracing::trace!(
                        target: "video_timing",
                        stage = "outgoing.control",
                        kind = "DAVE",
                        action = "dave_commit_processing",
                        transition_id = payload.transition_id,
                        protocol_version = u64::from(protocol_version.get()),
                        pending_transitions = voice_gateway.dave_pending_transitions.len(),
                        "DAVE transition processing"
                    );
                    let result = dave_session.process_commit(payload.data);
                    (protocol_version, result)
                };
                let succeeded = result.is_ok();
                respond_to_mls_transition(
                    &voice_gateway.websocket,
                    &voice_gateway.dave_pending_transitions,
                    protocol_version,
                    payload.transition_id,
                    result,
                )
                .await?;
                tracing::trace!(
                    target: "video_timing",
                    stage = "outgoing.control",
                    kind = "DAVE",
                    action = if succeeded {
                        "dave_commit_ready"
                    } else {
                        "dave_commit_rejected"
                    },
                    transition_id = payload.transition_id,
                    protocol_version = u64::from(protocol_version.get()),
                    elapsed_us = u64::try_from(process_started.elapsed().as_micros())
                        .unwrap_or(u64::MAX),
                    pending_transitions = voice_gateway.dave_pending_transitions.len(),
                    failed = !succeeded,
                    "DAVE transition processed"
                );
            }
            VoiceOpcode::MLSWelcome => {
                let bytes = payload.binary()?;
                let payload = MlsTransitionPayload::try_from(bytes)?;

                let process_started = Instant::now();
                let (protocol_version, result) = {
                    let mut dave_session = voice_gateway.require_dave_session().await?;
                    let protocol_version = dave_session.protocol_version();
                    if payload.transition_id != 0 {
                        voice_gateway
                            .dave_pending_transitions
                            .insert(payload.transition_id, protocol_version);
                    }
                    tracing::trace!(
                        target: "video_timing",
                        stage = "outgoing.control",
                        kind = "DAVE",
                        action = "dave_welcome_processing",
                        transition_id = payload.transition_id,
                        protocol_version = u64::from(protocol_version.get()),
                        pending_transitions = voice_gateway.dave_pending_transitions.len(),
                        "DAVE transition processing"
                    );
                    let result = dave_session.process_welcome(payload.data);
                    if result.is_ok() {
                        debug!("{:?}", dave_session.get_user_ids());
                    }
                    (protocol_version, result)
                };
                let succeeded = result.is_ok();
                respond_to_mls_transition(
                    &voice_gateway.websocket,
                    &voice_gateway.dave_pending_transitions,
                    protocol_version,
                    payload.transition_id,
                    result,
                )
                .await?;
                tracing::trace!(
                    target: "video_timing",
                    stage = "outgoing.control",
                    kind = "DAVE",
                    action = if succeeded {
                        "dave_welcome_ready"
                    } else {
                        "dave_welcome_rejected"
                    },
                    transition_id = payload.transition_id,
                    protocol_version = u64::from(protocol_version.get()),
                    elapsed_us = u64::try_from(process_started.elapsed().as_micros())
                        .unwrap_or(u64::MAX),
                    pending_transitions = voice_gateway.dave_pending_transitions.len(),
                    failed = !succeeded,
                    "DAVE transition processed"
                );
            }
            VoiceOpcode::MediaSinkWants => {
                let media_sink_wants = payload.decode::<MediaSinkWantsPayload>()?;
                let connection = voice_gateway.connection.load_full();
                let video_ssrc = connection
                    .as_ref()
                    .map(|connection| connection.video_ssrc())
                    .filter(|ssrc| *ssrc != 0);
                if voice_gateway.outbound_video_kind.is_some()
                    && let Some(quality) = media_sink_wants.quality_for(video_ssrc)
                    && let Some(connection) = connection
                {
                    let previous = connection
                        .outbound_video_quality
                        .swap(quality, Ordering::AcqRel);
                    if previous != quality {
                        debug!(
                            "Media sink quality changed: {previous} -> {quality} \
                             (video_ssrc={video_ssrc:?})"
                        );
                    }
                } else {
                    trace!(
                        "Media sink wants without local quality target: {:?}",
                        media_sink_wants
                    );
                }
            }
            _ => {
                warn!("Unknown voice-opcode received: {:?}", payload.opcode());
            }
        }

        Ok(())
    }
}
