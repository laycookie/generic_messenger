//! Per-call audio send/receive loop.
//!
//! [`InnerDiscord::poll_audio`] drives one call's media for the lifetime of its
//! [`ArcStream`]: it acquires the mic and builds the Opus codecs once, then
//! loops `pull` → `process`, transmitting one outgoing frame per turn and
//! dispatching incoming packets to per-speaker playback channels. The live call
//! is re-loaded every round ([`InnerDiscord::load_live_call`]) so a disconnect
//! or reconnect ends the session instead of transmitting into a dead
//! connection — no cross-task teardown signal is needed.
use std::{
    ops::ControlFlow,
    pin::pin,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use async_trait::async_trait;
use async_tungstenite::tungstenite::Message as WebsocketMessage;
use futures::{FutureExt as _, channel::oneshot, future, select};
use futures_timer::Delay;
use messenger_interface::{interface::AudioEvent, stream::ArcStream};
use simple_audio_channels::{AudioSampleType, input::SampleConsumer};
use surf::http::convert::json;
use tracing::{debug, error, trace, warn};

use super::{
    Voice, VoiceOpcode,
    connection::{Connection, RecvCodec, SendCodec, UdpPacket, VOICE_FRAME_SAMPLES},
};
use crate::{
    AudioDiscord, AudioManager, InnerDiscord, StreamPollGuard, UnitStruct, gateways::Gateway,
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

const MAX_MICROPHONE_RETRIES: u8 = 3;
/// How long the mic may stay silent before we announce we stopped speaking.
const STOP_SPEAKING: Duration = Duration::from_secs(2);
/// Idle re-verification cadence. With no audio in either direction the loop
/// still wakes this often to confirm the call is alive, so a disconnect during
/// silence tears the session down promptly without any cross-task signal.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// One unit of outgoing work handed from [`InnerDiscord::pull`] to
/// [`InnerDiscord::process`]. Incoming playback isn't an `AudioWork`; it is
/// dispatched directly in [`InnerDiscord::pull`].
enum AudioWork {
    /// A full outgoing frame is buffered, ready to encode and transmit.
    SendMic,
    /// The mic went silent past the threshold; announce we stopped speaking.
    StopSpeaking,
}

/// What one [`InnerDiscord::pull`] turn resolved to.
enum Pulled {
    /// Outgoing work to hand to [`InnerDiscord::process`].
    Work(AudioWork),
    /// A new playback channel is initializing: `poll_audio` must return to the
    /// stream so the queued [`AudioEvent::AddAudioSource`] reaches the UI — the
    /// only consumer that can supply the channel's producer — then re-enter.
    Yield,
    /// The call is gone, or we were killed — end the audio stream.
    End,
}

/// The voice gateway and media connection an audio session is bound to. Loaded
/// fresh each round so a disconnect (either is gone) or a reconnect (a
/// different [`Connection`]) is detected instead of transmitted into.
struct LiveCall {
    gateway: Arc<Gateway<Voice>>,
    connection: Arc<Connection>,
}

/// State owned for the lifetime of one call's audio. The codecs live here (not
/// on the per-round transport) so a round — or a new speaker — never resets
/// them; `call_identity` is the [`Connection`] the session started on, checked
/// every round by [`InnerDiscord::load_live_call`].
struct AudioSession<'mic> {
    /// `None` once the mic retry budget is spent (receive-only).
    send: Option<SendCodec>,
    recv: RecvCodec,
    microphone: Option<&'mic mut SampleConsumer>,
    frame: [AudioSampleType; VOICE_FRAME_SAMPLES],
    /// Partial fill carried across select cancellations.
    frame_filled: usize,
    stop_speaking_delay: Option<Delay>,
    call_identity: Arc<Connection>,
}

/// Outcome of [`InnerDiscord::prepare_session`]. The session is boxed because
/// it owns the (large) codec buffers, which would otherwise make every
/// `Prepared` value that big even for the empty outcomes.
enum Prepared<'mic> {
    Ready(Box<AudioSession<'mic>>),
    /// Mic request in flight (or a transient failure) — re-enter `poll_audio`.
    Retry,
    /// No live call to attach to — end the audio stream.
    Disconnected,
}

/// Fill `frame` from the mic, parking on silence (the input gate drops silent
/// buffers, so `pop` blocks). Resolves only once a whole frame is buffered;
/// `frame_filled` persists a partial fill across select cancellations.
async fn fill_frame(
    mic: &mut SampleConsumer,
    frame: &mut [AudioSampleType; VOICE_FRAME_SAMPLES],
    frame_filled: &mut usize,
) {
    while *frame_filled < VOICE_FRAME_SAMPLES {
        *frame_filled += mic.pop(&mut frame[*frame_filled..]).await;
    }
}

/// The pending stop-speaking timeout, or a never-resolving future when we are
/// not currently speaking (so that select arm is effectively absent).
async fn stop_speaking_or_pending(delay: &mut Option<Delay>) {
    match delay {
        Some(delay) => delay.await,
        None => future::pending::<()>().await,
    }
}

impl<T: UnitStruct> InnerDiscord<T> {
    pub async fn poll_audio(&self) -> Option<()> {
        if self.killed.load(Ordering::Acquire) {
            return None;
        }

        // Held for the whole session: serializes audio loops so only one call
        // transmits at a time. A stale loop now exits promptly (see `pull`), so
        // the next call isn't left blocked waiting on this lock.
        trace!("poll_audio: waiting for audio_manager lock");
        let mut manager = self.audio_manager.lock().await;
        trace!("poll_audio: audio_manager lock acquired");

        let mut session = match self.prepare_session(&mut manager).await {
            Prepared::Ready(session) => session,
            Prepared::Retry => return Some(()),
            Prepared::Disconnected => return None,
        };

        // pull → process, one outgoing frame per turn. `pull` re-verifies the
        // call each round and yields `None` once it's gone, ending this call's
        // audio stream and dropping the connection the session held.
        loop {
            match self.pull(&mut session).await {
                Pulled::Work(work) => {
                    if self.process(&mut session, work).await.is_break() {
                        return None;
                    }
                }
                // Return so `ArcStream::next` can drain the queued audio events
                // (e.g. AddAudioSource) to the UI, then re-enter `poll_audio`.
                Pulled::Yield => return Some(()),
                Pulled::End => return None,
            }
        }
    }

    /// Acquire the mic and build the owned codec state for a new audio session.
    /// Mic acquisition is two-phase (request, then await it on the next call)
    /// so a kill or owner-drop is observed between the two steps.
    async fn prepare_session<'mic>(&self, manager: &'mic mut AudioManager) -> Prepared<'mic> {
        if manager.microphone.is_none() && manager.microphone_retries < MAX_MICROPHONE_RETRIES {
            let Some(receiver) = manager.microphone_recv.take() else {
                let (sender, receiver) = oneshot::channel();
                let _ = self
                    .audio_events
                    .force_push(AudioEvent::AddAudioInput(sender));
                manager.microphone_recv = Some(receiver);
                return Prepared::Retry;
            };
            match receiver.await {
                Ok(consumer) => {
                    manager.microphone = Some(consumer);
                    manager.microphone_retries = 0;
                }
                Err(_) => {
                    manager.microphone_retries += 1;
                    if manager.microphone_retries >= MAX_MICROPHONE_RETRIES {
                        error!(
                            "Microphone acquisition failed after {MAX_MICROPHONE_RETRIES} retries; continuing receive-only"
                        );
                    } else {
                        warn!(
                            "Microphone input sender was dropped (attempt {}/{})",
                            manager.microphone_retries, MAX_MICROPHONE_RETRIES
                        );
                    }
                    return Prepared::Retry;
                }
            }
        }

        let Some(call) = self.load_call() else {
            warn!("No live voice call to attach audio to");
            return Prepared::Disconnected;
        };
        let (send, recv) = match call.connection.new_codecs() {
            Ok(codecs) => codecs,
            Err(err) => {
                error!("Failed to initialize audio codecs: {err}");
                return Prepared::Disconnected;
            }
        };
        // Drop the send codec in receive-only mode (no mic to encode from).
        let has_mic = manager.microphone.is_some();
        Prepared::Ready(Box::new(AudioSession {
            send: has_mic.then_some(send),
            recv,
            microphone: manager.microphone.as_mut(),
            frame: [0.0; VOICE_FRAME_SAMPLES],
            frame_filled: 0,
            stop_speaking_delay: None,
            call_identity: call.connection,
        }))
    }

    /// Await the next event on the live call: an incoming UDP packet (classified
    /// and dispatched to playback right here, so the per-packet divergence is
    /// visible) or an outgoing action for [`InnerDiscord::process`]. Re-loads the
    /// call each round so a disconnect or reconnect ends the session instead of
    /// transmitting into a dead connection.
    async fn pull(&self, session: &mut AudioSession<'_>) -> Pulled {
        loop {
            if self.killed.load(Ordering::Acquire) {
                return Pulled::End;
            }
            let Some(live) = self.load_live_call(&session.call_identity) else {
                return Pulled::End;
            };
            let recv_t = live
                .connection
                .recv_transport(&live.gateway.dave_session, &live.gateway.ssrc_to_user_id);

            let AudioSession {
                recv,
                microphone,
                frame,
                frame_filled,
                stop_speaking_delay,
                ..
            } = &mut *session;
            let mic = microphone.as_deref_mut();

            let mut incoming = pin!(recv_t.recv(recv).fuse());
            let mut outgoing = pin!(
                async move {
                    match mic {
                        Some(mic) => fill_frame(mic, frame, frame_filled).await,
                        None => future::pending::<()>().await,
                    }
                }
                .fuse()
            );
            let mut silence = pin!(stop_speaking_or_pending(stop_speaking_delay).fuse());
            let mut heartbeat = pin!(Delay::new(HEARTBEAT).fuse());
            let mut killed = pin!(self.killed_signal().fuse());

            select! {
                packet = incoming => {
                    match packet {
                        Ok(UdpPacket::Voice { ssrc, samples }) => {
                            // `false` ⇒ a new channel is initializing; yield so
                            // its queued AddAudioSource can be delivered.
                            if !live.gateway.dispatch_incoming_audio(&self.audio_events, ssrc, samples) {
                                return Pulled::Yield;
                            }
                        }
                        Ok(UdpPacket::Rtcp(rtcp_type)) => trace!("RTCP: {rtcp_type:?}"),
                        Ok(UdpPacket::UnhandledRtp { ssrc, payload_type }) => {
                            debug!("Unhandled RTP payload type {payload_type} from SSRC {ssrc}")
                        }
                        Err(err) => error!("{err}"),
                    }
                    continue;
                }
                _ = outgoing => return Pulled::Work(AudioWork::SendMic),
                _ = silence => return Pulled::Work(AudioWork::StopSpeaking),
                _ = heartbeat => continue,
                _ = killed => return Pulled::End,
            }
        }
    }

    /// Carry out one [`AudioWork`] item on the connection `pull` verified.
    /// `Break` ends the session (the call vanished between pull and now).
    async fn process(&self, session: &mut AudioSession<'_>, work: AudioWork) -> ControlFlow<()> {
        let Some(live) = self.load_live_call(&session.call_identity) else {
            return ControlFlow::Break(());
        };
        match work {
            AudioWork::SendMic => {
                self.update_speaking(&live, true).await;
                let send_t = live.connection.send_transport(&live.gateway.dave_session);
                if let Some(codec) = session.send.as_mut()
                    && let Err(err) = send_t.send_frame(codec, &session.frame).await
                {
                    warn!("Failed to send voice audio frame: {err}");
                }
                session.frame_filled = 0;
                session.stop_speaking_delay = Some(Delay::new(STOP_SPEAKING));
            }
            AudioWork::StopSpeaking => {
                self.update_speaking(&live, false).await;
                session.stop_speaking_delay = None;
            }
        }
        ControlFlow::Continue(())
    }

    /// Send a speaking-state update, but only on an actual transition.
    async fn update_speaking(&self, live: &LiveCall, speaking: bool) {
        if live.gateway.is_speaking.swap(speaking, Ordering::Relaxed) == speaking {
            return;
        }
        let payload = speaking_payload(live.connection.ssrc(), speaking);
        if let Err(err) = live.gateway.websocket.send(payload).await {
            error!("Failed to send speaking update: {err}");
        }
    }

    /// Load the current voice gateway and media connection, if any.
    fn load_call(&self) -> Option<LiveCall> {
        let gateway = self.gateway.load();
        let voice_gateway = gateway.as_ref()?.voice.full_load_gateway()?;
        let connection = voice_gateway.connection.load_full()?;
        Some(LiveCall {
            gateway: voice_gateway,
            connection,
        })
    }

    /// Like [`Self::load_call`], but `None` unless the loaded connection is
    /// still the exact one identified by `identity`. A different `Connection`
    /// means the original call ended (and possibly a new one took its place),
    /// so the caller must tear down rather than carry on.
    fn load_live_call(&self, identity: &Arc<Connection>) -> Option<LiveCall> {
        let call = self.load_call()?;
        Arc::ptr_eq(&call.connection, identity).then_some(call)
    }
}

#[async_trait]
impl ArcStream for InnerDiscord<AudioDiscord> {
    type Item = AudioEvent;
    /// Await the next item. Works with shared ownership via `Arc`.
    async fn next(self: Arc<Self>) -> Option<<Self as ArcStream>::Item> {
        let _guard = StreamPollGuard::new(&self.active_streams);
        loop {
            if self.killed.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            if self.owner_dropped() {
                self.kill();
                return None;
            }
            if let Some(event) = self.audio_events.pop() {
                return Some(event);
            }
            self.poll_audio().await?;
        }
    }
}
