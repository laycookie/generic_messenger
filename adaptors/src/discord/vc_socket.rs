use async_tungstenite::tungstenite::Message;
use discortp::discord::IpDiscoveryPacket;
use serde::Deserialize;
use serde_json::json;
use serde_repr::Deserialize_repr;
use smol::net::UdpSocket;
use std::time::Duration;

use crate::discord::{Discord, GatewayPayload, websocket::HeartBeatingData};

/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#voice>
/// <https://docs.discord.food/topics/opcodes-and-status-codes#voice-opcodes>
#[repr(u8)]
#[non_exhaustive]
#[derive(Debug, Deserialize_repr)]
pub(super) enum VCOpcode {
    Identify = 0,
    SelectProtocol = 1,
    Ready = 2,
    Heartbeat = 3,
    SessionDescription = 4,
    Speaking = 5,
    Hello = 8,
    ClientConnect = 11,
    Video = 12,
    ClientDisconnect = 13,
    ClientFlags = 18,
    ClientPlatform = 20,
}
impl GatewayPayload<VCOpcode> {
    pub(super) async fn exec(self, discord: &Discord) -> Result<(), ()> {
        let mut discord_socket = discord.socket.lock().await;

        if let Some(s) = self.s {
            discord_socket.vc_last_sequence_number = Some(s);
        };

        println!("VCOpcode: {:?}", &self.op);
        match self.op {
            // Received after we sending Identify payload, and signals to us that we
            // are ready to open UDP connection.
            // https://discord.com/developers/docs/topics/voice-connections#establishing-a-voice-websocket-connection-example-voice-ready-payload
            VCOpcode::Ready => {
                let modes = self
                    .d
                    .get("modes")
                    .and_then(|modes| modes.as_array())
                    .unwrap()
                    .iter()
                    .map(|mode| mode.as_str().unwrap())
                    .collect::<Vec<_>>();

                // TODO: Not hard code it maybe?
                if !modes.contains(&EncryptionMode::aead_xchacha20_poly1305_rtpsize.as_str()) {
                    eprintln!("Encryption not supported");
                    return Err(());
                };

                let ip = self
                    .d
                    .get("ip")
                    .and_then(|id| id.as_str().map(|s| s.to_string()))
                    .unwrap();
                let port = self.d.get("port").and_then(|id| id.as_u64()).unwrap() as u16;
                let ssrc = self.d.get("ssrc").and_then(|id| id.as_u64()).unwrap() as u32;

                let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
                socket.connect((ip.as_str(), port)).await.unwrap();

                let mut address_ascii = [0; 64];
                address_ascii[..ip.len()].copy_from_slice(ip.as_bytes());

                let discovery: [u8; 74] = unsafe {
                    std::mem::transmute(IpDiscovery {
                        _req_or_res: 1u16.to_be(),
                        _length: 70u16.to_be(),
                        _ssrc: ssrc.to_be(),
                        address_ascii,
                        port: port.to_be(),
                    })
                };
                socket.send(&discovery).await.unwrap();

                let mut buf = [0u8; 74];
                match socket.recv(&mut buf).await {
                    Ok(len) => println!("Got {len} bytes\n{buf:?}"),
                    Err(e) => eprintln!("No response: {e:?}"),
                }
                discord_socket.vc_connection = Some(VCConnection::new(socket, ssrc));

                let ip_discovery = IpDiscoveryPacket::new(&buf).unwrap();

                let mut ip_address = ip_discovery.get_address();
                // Get rid of extra nulls if any due to ipv4 being chosen over ipv6
                if let Some(null_position) = ip_address.iter().position(|c| *c == 0) {
                    ip_address.truncate(null_position);
                };

                discord_socket
                    .vc_websocket
                    .as_mut()
                    .unwrap()
                    .send(Message::Text(
                        json!({
                            "op": VCOpcode::SelectProtocol as u8,
                            "d": {
                                "protocol": "udp",
                                "data": {
                                    "address": std::str::from_utf8(&ip_address).unwrap(),
                                    "port": ip_discovery.get_port(),
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
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
            VCOpcode::SessionDescription => {
                let session_description =
                    serde_json::from_value::<SessionDescription>(self.d).unwrap();
                discord_socket
                    .vc_connection
                    .as_mut()
                    .unwrap()
                    .set_description(session_description);

                let vc_connection = discord_socket.vc_connection.as_ref().unwrap();
                let ssrc = vc_connection.client_ssrc;
                discord_socket
                    .vc_websocket
                    .as_mut()
                    .unwrap()
                    .send(Message::Text(
                        json!({
                        "op": VCOpcode::Speaking as u8,
                        "d": {
                            "speaking": 0,
                            "delay": 0,
                            "ssrc": ssrc,
                        },
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
            // Used to init. heart-beating on voice WebSocket.
            // https://discord.com/developers/docs/topics/voice-connections#heartbeating
            VCOpcode::Hello => {
                let heartbeat_interval = self
                    .d
                    .get("heartbeat_interval")
                    .and_then(|v| v.as_u64())
                    .map(Duration::from_millis)
                    .unwrap();
                let v = self.d.get("v").and_then(|v| v.as_u64()).unwrap();
                discord_socket.vc_heart_beating =
                    Some(HeartBeatingData::new(heartbeat_interval, Some(v as u8)));
            }
            VCOpcode::Heartbeat => {
                println!("{:#?}", self);
            }
            VCOpcode::Identify => todo!(),
            VCOpcode::SelectProtocol => todo!(),
            VCOpcode::Speaking => {
                println!("{:#?}", self.d);
            }
            _ => {
                println!("{:?}", self.op)
            }
        };
        Ok(())
    }
}

/// <https://discord.com/developers/docs/topics/voice-connections#ip-discovery>
// Notably req/res, length, port are big endian
#[repr(Rust, packed)]
struct IpDiscovery {
    _req_or_res: u16,
    _length: u16,
    _ssrc: u32,
    address_ascii: [u8; 64],
    port: u16,
}

/// <https://docs.discord.food/topics/voice-connections#encryption-mode>
#[allow(non_camel_case_types)]
#[derive(Debug, Deserialize)]
pub(super) enum EncryptionMode {
    aead_aes256_gcm,
    aead_aes256_gcm_rtpsize,
    aead_xchacha20_poly1305_rtpsize,
    xsalsa20_poly1305,
    xsalsa20_poly1305_suffix,
    xsalsa20_poly1305_lite,
    xsalsa20_poly1305_lite_rtpsize,
}
impl EncryptionMode {
    fn as_str(&self) -> &str {
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
    pub(super) fn tag_len(&self) -> usize {
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
    pub(super) fn nonce_size(&self) -> usize {
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
#[derive(Debug, Deserialize)]
pub(super) struct SessionDescription {
    audio_codec: String,
    video_codec: String,
    media_session_id: String,
    mode: Option<EncryptionMode>,
    secret_key: Option<Vec<u8>>,
    keyframe_interval: Option<u32>,
}
impl SessionDescription {
    pub(super) fn mode(&self) -> Option<&EncryptionMode> {
        self.mode.as_ref()
    }
    pub(super) fn secret_key(&self) -> Option<&Vec<u8>> {
        self.secret_key.as_ref()
    }
}

pub(super) struct VCConnection {
    udp: UdpSocket,
    client_ssrc: u32,
    description: Option<SessionDescription>,
    decoder: opus::Decoder,
}
impl VCConnection {
    pub(super) fn decoder(&mut self) -> &mut opus::Decoder {
        &mut self.decoder
    }
    pub(super) fn udp(&self) -> UdpSocket {
        self.udp.clone()
    }
    pub(super) fn description(&self) -> Option<&SessionDescription> {
        self.description.as_ref()
    }
}

impl VCConnection {
    fn new(udp: UdpSocket, ssrc: u32) -> Self {
        Self {
            udp,
            description: None,
            client_ssrc: ssrc,
            decoder: opus::Decoder::new(48000, opus::Channels::Stereo).unwrap(),
        }
    }
    fn set_description(&mut self, description: SessionDescription) {
        self.description = Some(description);
    }
}
