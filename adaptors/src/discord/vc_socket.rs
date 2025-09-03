use async_std::net::UdpSocket;
use async_tungstenite::tungstenite::Message;
use serde_json::json;
use serde_repr::Deserialize_repr;
use std::time::Duration;

use crate::discord::{
    Discord, DiscordSockets,
    websocket::{GateawayPayload, HeartBeatingData},
};

/// https://discord.com/developers/docs/topics/opcodes-and-status-codes#voice
/// https://docs.discord.food/topics/opcodes-and-status-codes#voice-opcodes
#[repr(u8)]
#[derive(Debug, Deserialize_repr)]
pub(super) enum VCOpcode {
    Identify = 0,
    SelectProtocol = 1,
    Ready = 2,
    Heartbeat = 3,
    SessionDescription = 4,
    Hello = 8,
}

/// https://discord.com/developers/docs/topics/voice-connections#ip-discovery
// Notably req/res, length, port are big endian
#[repr(Rust, packed)]
struct IpDiscovery {
    _req_or_res: u16,
    _length: u16,
    _ssrc: u32,
    address_ascii: [u8; 64],
    port: u16,
}

impl Discord {
    pub(super) async fn vc_event_exec(
        json: GateawayPayload<VCOpcode>,
        discord_socket: &mut DiscordSockets,
    ) -> Result<(), ()> {
        println!("Opcode: {:?}", &json.op);
        match json.op {
            // Received after we sending Identify payload, and signals to us that we
            // are ready to open UDP connection.
            // https://discord.com/developers/docs/topics/voice-connections#establishing-a-voice-websocket-connection-example-voice-ready-payload
            VCOpcode::Ready => {
                let modes = json
                    .d
                    .get("modes")
                    .and_then(|modes| modes.as_array())
                    .unwrap()
                    .iter()
                    .map(|mode| mode.as_str().unwrap())
                    .collect::<Vec<_>>();

                println!("{modes:#?}");

                // TODO: Not hard code it maybe?
                if !modes.contains(&"aead_xchacha20_poly1305_rtpsize") {
                    eprintln!("Encyption not supported");
                    return Err(());
                };

                let ip = json
                    .d
                    .get("ip")
                    .and_then(|id| id.as_str().map(|s| s.to_string()))
                    .unwrap();
                let port = json.d.get("port").and_then(|id| id.as_u64()).unwrap() as u16;
                let ssrc = json.d.get("ssrc").and_then(|id| id.as_u64()).unwrap() as u32;

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
                discord_socket.vc_connection = Some(socket);

                let ip_discovery = unsafe { std::mem::transmute::<[u8; 74], IpDiscovery>(buf) };

                discord_socket
                    .vc_websocket
                    .as_mut()
                    .unwrap()
                    .send(Message::Text(json!({
                    "op": VCOpcode::SelectProtocol as u8,
                    "d": {
                        "protocol": "udp",
                        "data": {
                            "address": std::str::from_utf8(&ip_discovery.address_ascii).unwrap(),
                            "port": ip_discovery.port.to_le(),
                            // TODO: We are hard coding it just for rn
                            "mode": "aead_xchacha20_poly1305_rtpsize",
                        }
                    },
                }).to_string().into()))
                    .await
                    .unwrap();
            }
            VCOpcode::SessionDescription => {
                println!("{json:#?}");
            }
            // Used to init. heart-beating on voice WebSocket.
            // https://discord.com/developers/docs/topics/voice-connections#heartbeating
            VCOpcode::Hello => {
                discord_socket.vc_heart_beating = Some(HeartBeatingData::new(
                    json.d
                        .get("heartbeat_interval")
                        .and_then(|v| v.as_u64())
                        .map(Duration::from_millis)
                        .unwrap(),
                ));
            }
            VCOpcode::Heartbeat => todo!(),
            VCOpcode::Identify => todo!(),
            VCOpcode::SelectProtocol => todo!(),
        };
        Ok(())
    }
}
