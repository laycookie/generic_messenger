use async_tungstenite::tungstenite::Message;
use discortp::discord::{IpDiscoveryPacket, IpDiscoveryType};
use serde::Deserialize;
use serde_json::json;
use serde_repr::Deserialize_repr;
use smol::{lock::Mutex, net::UdpSocket};
use std::{sync::Arc, time::Duration};

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
    ClientDisconnect = 13,
    ClientFlags = 18,
    ClientPlatform = 20,
}
impl GatewayPayload<VCOpcode> {
    pub(super) async fn exec(self, discord: &Discord) -> Result<(), ()> {
        let mut discord_socket = discord.socket.lock().await;

        if let Some(s) = self.s {
            println!("Updating VC seq: {s:?}");
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
                if !modes.contains(&"aead_xchacha20_poly1305_rtpsize") {
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
                println!("AAAAAAAAAAAA: {:?}", ip_discovery.get_address());

                discord_socket
                    .vc_websocket
                    .as_mut()
                    .unwrap()
                    .send(Message::Text(json!({
                    "op": VCOpcode::SelectProtocol as u8,
                    "d": {
                        "protocol": "udp",
                        "data": {
                            "address": std::str::from_utf8(&ip_discovery.get_address()).unwrap(),
                            "port": ip_discovery.get_port(),
                            // TODO: We are hard coding it just for rn
                            "mode": "aead_xchacha20_poly1305_rtpsize",
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
                }).to_string().into()))
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
                let ssrc = vc_connection.my_ssrc;
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

/// <https://docs.discord.food/topics/voice-connections#session-description-structure>
#[derive(Debug, Deserialize)]
pub(super) struct SessionDescription {
    audio_codec: String,
    video_codec: String,
    media_session_id: String,
    mode: Option<String>,
    secret_key: Option<Vec<u32>>,
    keyframe_interval: Option<u32>,
}

pub(super) struct VCConnection {
    udp: Arc<Mutex<UdpSocket>>,
    description: Option<SessionDescription>,
    my_ssrc: u32,
}
impl VCConnection {
    pub(super) fn udp(&self) -> Arc<Mutex<UdpSocket>> {
        self.udp.clone()
    }
}

impl VCConnection {
    fn new(udp: UdpSocket, ssrc: u32) -> Self {
        Self {
            udp: Arc::new(Mutex::new(udp)),
            description: None,
            my_ssrc: ssrc,
        }
    }
    fn set_description(&mut self, description: SessionDescription) {
        self.description = Some(description);
    }
}
