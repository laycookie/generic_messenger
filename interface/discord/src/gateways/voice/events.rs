use std::{
    io,
    mem,
    num::NonZeroU16,
    sync::{Arc, atomic::Ordering},
};

use dashmap::DashMap;
use davey::DaveSession;
use messenger_interface::{
    interface::{CallStatus, VoiceEvent},
    stream::WeakSocketStream,
};
use smol::net::UdpSocket;
use surf::http::convert::json;
use tracing::{debug, error, warn};

use async_tungstenite::tungstenite::Message;

use super::{
    VoiceOpcode,
    connection::{Connection, EncryptionMode, SessionDescription},
    payloads::{DAVEPrepareEpoch, ReadyPayload, SpeakingPayload},
};
use crate::api_types::SNOWFLAKE;
use crate::gateways::{GatewayPayload, Websocket};
use crate::{AudioDiscord, InnerDiscord, UnitStruct};

// Local types for IP discovery packet layout.
// Only used in the VoiceOpcode::Ready handler below.
// <https://discord.com/developers/docs/topics/voice-connections#ip-discovery>
#[allow(non_camel_case_types)]
#[derive(Copy, Clone)]
#[repr(transparent)]
struct u16be(u16);
impl u16be {
    pub fn get(self) -> u16 {
        u16::from_be(self.0)
    }
}
impl From<u16> for u16be {
    fn from(value: u16) -> Self {
        Self(value.to_be())
    }
}

#[allow(non_camel_case_types)]
#[derive(Copy, Clone)]
#[repr(transparent)]
struct u32be(u32);
impl u32be {
    pub fn get(self) -> u32 {
        u32::from_be(self.0)
    }
}
impl From<u32> for u32be {
    fn from(value: u32) -> Self {
        Self(value.to_be())
    }
}

#[repr(C, packed)]
struct IpDiscovery {
    _req_or_res: u16be,
    _length: u16be,
    ssrc: u32be,
    address_ascii: [u8; 64],
    port: u16be,
}

impl GatewayPayload<VoiceOpcode> {
    pub async fn exec<T: UnitStruct>(
        self,
        discord: &Arc<InnerDiscord<T>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let gateway = discord.gateway.load();
        let Some(gateway) = gateway.as_ref() else {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "gateway not connected").into());
        };
        let Some(voice_gateway) = gateway.voice.full_load_gateway() else {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "voice gateway not connected").into());
        };

        if let Some(s) = self.s {
            voice_gateway
                .last_sequence_number
                .get_or_init(|| s.into())
                .store(s, Ordering::Relaxed);
        };

        debug!("VoiceOpcode: {:?}", self.op);
        match self.op {
            VoiceOpcode::SessionDescription => {
                let session_description = facet_value::from_value::<SessionDescription>(self.d)?;

                // Init DAVE
                let mut dave_session = voice_gateway.dave_session.lock().await;
                let profile = discord.profile.read().await;
                let profile = profile.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user profile not loaded"))?;
                reinit_dave_session(
                    &voice_gateway.websocket,
                    &mut dave_session,
                    session_description.dave_protocol_version(),
                    voice_gateway.channel_id,
                    profile.id,
                )
                .await?;

                // Commit description to connection
                if let Some(connection) = voice_gateway.connection.load().as_ref() {
                    connection.set_description(session_description).map_err(|_| io::Error::new(io::ErrorKind::AlreadyExists, "session description already set"))?;
                };

                discord
                    .voice_events
                    .push(VoiceEvent::CallStatusUpdate(CallStatus::Connected(
                        WeakSocketStream::new(unsafe {
                            discord
                                .to_owned()
                                .cast_and_downgrade::<AudioDiscord>()
                                .await
                        }),
                    )));
            }
            VoiceOpcode::Speaking => {
                let speaking = facet_value::from_value::<SpeakingPayload>(self.d)?;

                voice_gateway
                    .ssrc_to_user_id
                    .insert(speaking.ssrc, speaking.user_id);
            }
            VoiceOpcode::Ready => {
                let ready = facet_value::from_value::<ReadyPayload>(self.d)?;

                // TODO: Not hard code it maybe?
                if !ready
                    .modes
                    .contains(&EncryptionMode::aead_xchacha20_poly1305_rtpsize)
                {
                    return Err(io::Error::new(io::ErrorKind::Unsupported, "required encryption mode not available").into());
                }

                let mut address_ascii = [0; 64];
                address_ascii[..ready.ip.len()].copy_from_slice(ready.ip.as_bytes());

                let send_ip_discovery = unsafe {
                    std::mem::transmute::<IpDiscovery, [u8; 74]>(IpDiscovery {
                        _req_or_res: 1.into(),
                        _length: 70.into(),
                        ssrc: ready.ssrc.into(),
                        address_ascii,
                        port: ready.port.into(),
                    })
                };
                let udp = UdpSocket::bind("0.0.0.0:0").await?;
                debug!("Addr: {:?}", udp.local_addr());
                udp.connect((ready.ip.as_str(), ready.port)).await?;
                udp.send(&send_ip_discovery).await?;

                let mut buf = [0u8; 74];
                match udp.recv(&mut buf).await {
                    Ok(len) => debug!("Got {len} bytes\n{buf:?}"),
                    Err(e) => error!("No response: {e:?}"),
                }
                let recv_ip_discovery = unsafe { mem::transmute::<[u8; 74], IpDiscovery>(buf) };

                let mut ip_address = str::from_utf8(&recv_ip_discovery.address_ascii)?;
                if let Some(null_position) =
                    recv_ip_discovery.address_ascii.iter().position(|c| *c == 0)
                {
                    ip_address = &ip_address[..null_position]
                };
                {
                    voice_gateway.connection.store(Some(
                        Connection::new(udp, recv_ip_discovery.ssrc.get()).into(),
                    ));
                }
                let protocol_select = json!({
                    "op": VoiceOpcode::SelectProtocol as u8,
                    "d": {
                        "protocol": "udp",
                        "data": {
                            "address": ip_address,
                            "port": recv_ip_discovery.port.get(),
                            // TODO: We are hard coding it just for rn
                            "mode": EncryptionMode::aead_xchacha20_poly1305_rtpsize.as_str(),
                        },
                        "codecs": [
                            {
                                "name": "opus",
                                "type": "audio",
                                "priority": 1000,
                                "payload_type": 120
                            }
                        ]
                    },
                });
                voice_gateway
                    .websocket
                    .send(Message::Text(protocol_select.to_string().into()))
                    .await?;
            }
            VoiceOpcode::DAVEProtocolPrepareTransition => {
                let mut dave_session = voice_gateway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => return Err(io::Error::new(io::ErrorKind::InvalidData, "DAVE session not initialized").into()),
                };

                let packet = facet_value::from_value::<DAVEPrepareEpoch>(self.d)?;

                let transition_id = packet.transition_id;

                voice_gateway
                    .dave_pending_transitions
                    .insert(transition_id, dave_session.protocol_version());

                if transition_id == 0 {
                    execute_pending_transition(
                        dave_session,
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
                let mut dave_session = voice_gateway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => return Err(io::Error::new(io::ErrorKind::InvalidData, "DAVE session not initialized").into()),
                };

                let packet = facet_value::from_value::<DAVEPrepareEpoch>(self.d)?;
                let transition_id = packet.transition_id;
                execute_pending_transition(
                    dave_session,
                    &voice_gateway.dave_pending_transitions,
                    transition_id,
                );
            }
            VoiceOpcode::DAVEProtocolPrepareEpoch => {
                let packet = facet_value::from_value::<DAVEPrepareEpoch>(self.d)?;

                if packet.epoch == 1 {
                    let mut dave_session = voice_gateway.dave_session.lock().await;
                    // TODO: Investigate if this should be properly added
                    // this.daveProtocolVersion = packet.protocol_version;
                    let profile = discord.profile.read().await;
                    let profile = profile.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "user profile not loaded"))?;
                    reinit_dave_session(
                        &voice_gateway.websocket,
                        &mut dave_session,
                        packet.protocol_version,
                        voice_gateway.channel_id,
                        profile.id,
                    )
                    .await?;
                }
            }
            VoiceOpcode::MLSExternalSenderPackage => {
                let mut dave_session = voice_gateway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => return Err(io::Error::new(io::ErrorKind::InvalidData, "DAVE session not initialized").into()),
                };

                let bytes = facet_value::from_value::<Vec<u8>>(self.d)?;
                if let Err(err) = dave_session.set_external_sender(&bytes[1..]) {
                    error!("{err}");
                    return Err(err.into());
                };
            }
            VoiceOpcode::MLSProposals => {
                let mut dave_session = voice_gateway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => return Err(io::Error::new(io::ErrorKind::InvalidData, "DAVE session not initialized").into()),
                };
                let bytes = facet_value::from_value::<Vec<u8>>(self.d)?;

                let optype = if bytes[1] == 0 {
                    davey::ProposalsOperationType::APPEND
                } else {
                    davey::ProposalsOperationType::REVOKE
                };
                let commit_welcome = match dave_session.process_proposals(
                    optype,
                    &bytes[2..],
                    // TODO: Add this for security purposes, should be received from CLIENTS_CONNECT
                    None,
                ) {
                    Ok(welcome_message) => welcome_message,
                    Err(err) => {
                        error!("{err:?}");
                        return Err(err.into());
                    }
                };

                if let Some(commit_welcome) = commit_welcome {
                    match commit_welcome.welcome {
                        Some(welcome) => {
                            voice_gateway
                                .websocket
                                .send_binary(
                                    VoiceOpcode::MLSCommitWelcome as u8,
                                    welcome.into_iter().chain(commit_welcome.commit.into_iter()),
                                )
                                .await?
                        }
                        None => {
                            voice_gateway
                                .websocket
                                .send_binary(
                                    VoiceOpcode::MLSCommitWelcome as u8,
                                    commit_welcome.commit.into_iter(),
                                )
                                .await?
                        }
                    }
                } else {
                    error!("Potentially a problem?");
                }
            }
            VoiceOpcode::MLSAnnounceCommitTransition => {
                let mut dave_session = voice_gateway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => return Err(io::Error::new(io::ErrorKind::InvalidData, "DAVE session not initialized").into()),
                };
                let bytes = facet_value::from_value::<Vec<u8>>(self.d)?;

                let transition_id = u16::from_be_bytes(bytes[1..3].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "transition_id slice too short"))?);
                if let Err(err) = dave_session.process_commit(&bytes[3..]) {
                    error!("{err:?}");
                    voice_gateway
                        .websocket
                        .send(async_tungstenite::tungstenite::Message::Text(
                            json!({
                                "op": VoiceOpcode::MLSInvalidCommitWelcome as u8,
                                "d": {
                                  "transition_id": transition_id
                                }
                            })
                            .to_string()
                            .into(),
                        ))
                        .await?
                } else {
                    if transition_id != 0 {
                        voice_gateway
                            .dave_pending_transitions
                            .insert(transition_id, dave_session.protocol_version());
                        //TODO
                        voice_gateway
                            .websocket
                            .send(async_tungstenite::tungstenite::Message::Text(
                                json!({
                                    "op": VoiceOpcode::DAVEProtocolTransitionReady as u8,
                                    "d": {
                                      "transition_id": transition_id
                                    }
                                })
                                .to_string()
                                .into(),
                            ))
                            .await?
                    }
                }
            }
            VoiceOpcode::MLSWelcome => {
                let mut dave_session = voice_gateway.dave_session.lock().await;
                let dave_session = match dave_session.as_mut() {
                    Some(dave_session) => dave_session,
                    None => return Err(io::Error::new(io::ErrorKind::InvalidData, "DAVE session not initialized").into()),
                };
                let bytes = facet_value::from_value::<Vec<u8>>(self.d)?;

                let transition_id = u16::from_be_bytes(bytes[1..3].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "transition_id slice too short"))?);
                if let Err(err) = dave_session.process_welcome(&bytes[3..]) {
                    error!("{err:?}");
                    voice_gateway
                        .websocket
                        .send(async_tungstenite::tungstenite::Message::Text(
                            json!({
                                "op": VoiceOpcode::MLSInvalidCommitWelcome as u8,
                                "d": {
                                  "transition_id": transition_id
                                }
                            })
                            .to_string()
                            .into(),
                        ))
                        .await?
                } else {
                    debug!("{:?}", dave_session.get_user_ids());
                    if transition_id != 0 {
                        voice_gateway
                            .dave_pending_transitions
                            .insert(transition_id, dave_session.protocol_version());
                        //TODO
                        voice_gateway
                            .websocket
                            .send(async_tungstenite::tungstenite::Message::Text(
                                json!({
                                    "op": VoiceOpcode::DAVEProtocolTransitionReady as u8,
                                    "d": {
                                      "transition_id": transition_id
                                    }
                                })
                                .to_string()
                                .into(),
                            ))
                            .await?
                    }
                }
            }
            _ => {
                warn!("Unknown voice-opcode received: {:?}", self.op);
            }
        }

        Ok(())
    }
}

// TODO: Move with DAVE related stuff when dave.rs is created
fn execute_pending_transition(
    dave_session: &mut DaveSession,
    dave_pending_transitions: &DashMap<u16, NonZeroU16>,
    transition_id: u16,
) {
    let Some((_, new_version)) = dave_pending_transitions.remove(&transition_id) else {
        warn!(
            "Received execute transition, but we don't have a pending transition for {transition_id}"
        );
        return;
    };

    let old_version = dave_session.protocol_version();
    if old_version != new_version {
        // TODO: Actually apply the version transition here. Currently the new protocol
        // version is acknowledged (removed from pending) but never applied to the session.
        error!("DAVE protocol version mismatch: old={old_version:?}, new={new_version:?}. Transition not applied");
    }
}

async fn reinit_dave_session(
    voice_websocket: &Websocket,
    dave_session: &mut Option<DaveSession>,
    dave_protocol_version: u16,
    channel_id: SNOWFLAKE,
    user_id: SNOWFLAKE,
) -> Result<(), Box<dyn std::error::Error>> {
    let dave_ver = NonZeroU16::new(dave_protocol_version)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "DAVE protocol version is zero"))?;

    let key_package = if let Some(dave_session) = dave_session {
        dave_session
            .reinit(dave_ver, user_id, channel_id, None)?;
        dave_session.create_key_package()
    } else {
        let mut new_dave_session = DaveSession::new(dave_ver, user_id, channel_id, None)?;
        let key_package = new_dave_session.create_key_package();
        *dave_session = Some(new_dave_session);
        key_package
    };

    voice_websocket
        .send_binary(
            VoiceOpcode::MLSKeyPackage as u8,
            key_package?.into_iter(),
        )
        .await?;

    Ok(())
}
