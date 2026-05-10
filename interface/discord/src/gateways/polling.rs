use std::{
    error::Error,
    io,
    pin::pin,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use async_trait::async_trait;
use async_tungstenite::tungstenite::Message as WebsocketMessage;
use futures::{
    FutureExt as _, StreamExt,
    channel::oneshot,
    future::{self, Either, select},
    pending, select, stream,
};
use futures_timer::Delay;
use messenger_interface::{
    interface::{AudioEvent, CallStatus, Voice as VoiceTrait, VoiceEvent},
    stream::WeakSocketStream,
    types::{Identifier, Place, Room},
};
use smol::future::yield_now;
use surf::http::convert::json;
use tracing::{debug, error, info, warn};

use super::{
    general::Opcode,
    voice::{VoiceOpcode, connection::VOICE_FRAME_SAMPLES},
};
use crate::{
    AudioManager, InnerDiscord, Owned, UnitStruct, VoiceDiscord, gateways::GatewayStreamReciver,
};

fn speaking_payload(ssrc: u32, speaking: bool) -> WebsocketMessage {
    json!({
        "op": VoiceOpcode::Speaking as u8,
        "d": {
            "speaking": speaking as u8,
            "delay": 0,
            "ssrc": ssrc,
        }
    })
    .to_string()
    .into()
}

impl<T: UnitStruct> InnerDiscord<T> {
    pub async fn poll_audio(&self) -> Option<()> {
        const MAX_MICROPHONE_RETRIES: u8 = 3;

        let AudioManager {
            ref mut microphone,
            ref microphone_recv,
            ref mut microphone_retries,
        } = *self.audio_manager.lock().await;
        if microphone.is_none() {
            let Some(receiver) = microphone_recv.take() else {
                if *microphone_retries >= MAX_MICROPHONE_RETRIES {
                    error!(
                        "Microphone acquisition failed after {MAX_MICROPHONE_RETRIES} retries, giving up"
                    );
                    return None;
                }
                let (sender, receiver) = oneshot::channel();
                self.audio_events.push(AudioEvent::AddAudioInput(sender));
                microphone_recv.set(Some(receiver));
                return Some(());
            };
            match receiver.await {
                Ok(consumer) => {
                    *microphone = Some(consumer);
                    *microphone_retries = 0;
                }
                Err(_) => {
                    *microphone_retries += 1;
                    warn!(
                        "Microphone input sender was dropped (attempt {}/{})",
                        *microphone_retries, MAX_MICROPHONE_RETRIES
                    );
                    return Some(());
                }
            }
        };
        let microphone_input = microphone.as_mut().unwrap();

        let gateway = self.gateway.load();
        let Some(gateway) = gateway.as_ref() else {
            warn!("Not connected to the gateway");
            return None;
        };
        debug!("gateway initialized.");
        let Some(voice_gateway) = gateway.voice.full_load_gateway() else {
            warn!("Not connected to the voice_gateway");
            return None;
        };
        debug!("voice gateway initialized.");
        let connection = voice_gateway.connection.load();
        let Some(ref connection) = *connection else {
            warn!("Not connected to the udp");
            return None;
        };
        debug!("udp initialized.");

        let (mut audio_receiver, mut audio_sender) = connection
            .init_audio(&voice_gateway.dave_session, &voice_gateway.ssrc_to_user_id)
            .ok()?;
        debug!("initialized audio receiver, and sender.");

        let mut frame = [0.0; VOICE_FRAME_SAMPLES];
        let mut stop_speaking_delay = None;
        let mut microphone_input_stream = pin!(stream::unfold::<_, _, _, ()>(
            (
                microphone_input,
                &mut audio_sender,
                &mut frame,
                &mut stop_speaking_delay
            ),
            async |(microphone_input, audio_sender, frame, stop_speaking_delay)| {
                loop {
                    let received_audio_fut = async {
                        loop {
                            let microphone_sample_iter = microphone_input.pop_iter().await;
                            if microphone_sample_iter.len() >= VOICE_FRAME_SAMPLES {
                                for (i, s) in
                                    microphone_sample_iter.take(VOICE_FRAME_SAMPLES).enumerate()
                                {
                                    frame[i] = s;
                                }
                                return;
                            }
                        }
                    };
                    match select(
                        pin!(received_audio_fut),
                        stop_speaking_delay
                            .take()
                            .unwrap_or_else(|| future::pending().boxed()),
                    )
                    .await
                    {
                        Either::Right((_, _)) => {
                            info!("NO AUDIO");
                            if voice_gateway.is_speaking.swap(false, Ordering::Relaxed) {
                                debug!("Send stop speaking packet");
                                let speaking_payload = speaking_payload(audio_sender.ssrc(), false);

                                if let Err(err) =
                                    voice_gateway.websocket.send(speaking_payload).await
                                {
                                    error!("Failed to send speaking update: {err}");
                                }
                            }
                            continue;
                        }
                        Either::Left((_, _)) => {}
                    };

                    *stop_speaking_delay = Some(Box::pin(Delay::new(Duration::from_secs(2))));
                    if !voice_gateway.is_speaking.load(Ordering::Relaxed) {
                        debug!("Send speaking packet");
                        let speaking_payload = speaking_payload(audio_sender.ssrc(), true);
                        match voice_gateway.websocket.send(speaking_payload).await {
                            Ok(_) => voice_gateway.is_speaking.store(true, Ordering::Relaxed),
                            Err(err) => {
                                error!("Failed to send speaking update: {err}");
                                return None;
                            }
                        };
                    }
                    if let Err(err) = audio_sender.send_audio_frame(frame.as_slice()).await {
                        warn!("Failed to send voice audio frame: {err}");
                    }
                }
            }
        ));

        let poll_udp = pin!(voice_gateway.poll_udp(&self.audio_events, &mut audio_receiver));
        select(microphone_input_stream.next(), poll_udp).await;
        warn!("Restarting audio loop");
        Some(())
    }

    pub async fn poll_for_events(self: &Arc<Self>) {
        let gateway = self.gateway.load();
        let Some(ref_gateway) = gateway.as_ref() else {
            warn!("Stream has not started, or has been killed");
            pending!();
            return;
        };
        // If someone else is already pulling we just wait until they finish by looking at
        // the lock state. We also need to yield here, as try_lock isn't a future which means
        // that a stream polling at the moment might relock before ever yielding to us.
        yield_now().await;
        let Some(mut gateway_receiver) = ref_gateway.websocket.receiver.try_lock() else {
            self.pulled_notification.notified().await;
            return;
        };
        let mut gateway_receiver = pin!(gateway_receiver.filter_payload::<Opcode>());

        let voice_gateway = ref_gateway.voice.full_load_gateway();
        let voice_gateway_clone = voice_gateway.clone();

        let mut websocket_reciver_guard;
        let mut voice_gateway_reciver = pin!(match voice_gateway.as_ref() {
            Some(voice_gateway) => {
                websocket_reciver_guard = voice_gateway.websocket.receiver.lock().await;
                Either::Right(websocket_reciver_guard.filter_payload::<VoiceOpcode>())
            }
            // Eternally hang this
            None => Either::Left(stream::empty()),
        });

        // TODO: Investigate using Websocket::next_payload() diractly
        select! {
        // Main gateway
        event = gateway_receiver.next() => {
            let Some(event) = event else {
                error!("Gateway closed?");
                return;
            };
            if let Err(err) = event.exec(self).await {
                warn!("Failed to execute gateway event: {err}");
            }
        }
        // voice gateway
        event = voice_gateway_reciver.next() => {
            let Some(event) = event else {
                error!("Gateway closed?");
                return;
            };
            if  let Err(err) = event.exec(self).await
            {
                warn!("Failed to execute voice gateway event: {err}")
            }
        }
        // heartbeat over main gateway
        _ = ref_gateway.heartbeat().fuse() => {}
        // voice heartbeat over main gateway
        _ = async {
                match voice_gateway_clone {
                    Some(voice_gateway) => voice_gateway.heartbeat().await,
                    None => futures::future::pending().await,
                }
            }.fuse() => {}
        };
        self.pulled_notification.notify_all();
    }
}

#[async_trait]
impl VoiceTrait for InnerDiscord<Owned> {
    async fn connect(
        &self,
        location: &Identifier<Place<Room>>,
    ) -> Result<CallStatus, Box<dyn Error + Sync + Send>> {
        let load_gateway = self.gateway.load();
        let Some(gateway) = load_gateway.as_ref() else {
            return Err(
                io::Error::new(io::ErrorKind::NotConnected, "gateway not connected").into(),
            );
        };

        let channels_map = self.channel_id_mappings.read().await;
        let channel = match channels_map.get(location.id()) {
            Some(c) => c,
            None => {
                // TODO(discord-migration): ensure all Rooms returned by Query have a mapping,
                // and support guild voice channels too.
                warn!("Tried to connect voice for a Room without a discord channel mapping");
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no channel mapping for this room",
                )
                .into());
            }
        };

        gateway.voice.initiate_connection(channel.to_owned()).await;

        let payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": channel.guild_id,
                "channel_id": channel.id,
                "self_mute": false,
                "self_deaf": false
              }
        });

        if let Err(err) = gateway.websocket.send(payload.to_string().into()).await {
            gateway.voice.disconnect().await;
            return Err(err.into());
        };
        Ok(CallStatus::Connecting("Awaiting call start"))
    }

    async fn disconnect(&self, location: &Identifier<Place<Room>>) {
        let load_gateway = self.gateway.load();
        let Some(gateway) = load_gateway.as_ref() else {
            error!("Not connected to the socket");
            return;
        };
        gateway.voice.disconnect().await;

        let channels_map = self.channel_id_mappings.read().await;
        let channel = match channels_map.get(location.id()) {
            Some(c) => c,
            None => {
                // TODO(discord-migration): ensure all Rooms returned by Query have a mapping,
                // and support guild voice channels too.
                warn!("Tried to disconnect voice for a Room without a discord channel mapping");
                return;
            }
        };

        let payload = json!({
            "op": Opcode::VoiceStateUpdate as u8,
            "d": {
                "guild_id": channel.guild_id,
                "channel_id": null,
                "self_mute": false,
                "self_deaf": false
              }
        });

        if let Err(err) = gateway.websocket.send(payload.to_string().into()).await {
            error!("Failed to send voice disconnect: {err}");
        }
    }

    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<VoiceEvent>, Box<dyn Error + Sync + Send>> {
        self.listen_as::<VoiceDiscord, _>().await
    }
}
