//! Steam voice event source and per-call media session.
//!
//! Mixer attachment is two-phase: emit oneshot request, return it to UI, then
//! adopt response on later poll. Waiting in same poll would deadlock delivery.
//! [`super::audio`] handles Opus framing over [`super::transport`].

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::ops::ControlFlow;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::{mpsc, oneshot};
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt as _, StreamExt as _, select};
use futures_timer::Delay;
use opus::{Application, Channels, Decoder, Encoder};
use simple_audio_channels::input::SampleConsumer;
use simple_audio_channels::output::SampleProducer;
use tracing::{debug, warn};

use messenger_interface::interface::{ArcStream, AudioEvent, VoiceEvent};

use super::audio::{
    CHANNELS, FRAME_SAMPLES, MAX_DECODE_SAMPLES, MAX_OPUS_FRAME, MicSource, MixerMic, SAMPLE_RATE,
};
use super::transport::{VoiceFrame, VoiceTransport};

/// Give up acquiring the mic after this many declined requests and run the call
/// receive-only.
const MAX_MIC_RETRIES: u8 = 3;
/// Bounds disconnect latency because atomic kill flag cannot wake stream.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// Voice events pushed by `connect`; ends when sender drops.
pub(crate) struct VoiceEventStream {
    events: AsyncMutex<mpsc::UnboundedReceiver<VoiceEvent>>,
}

impl VoiceEventStream {
    pub(crate) fn new(events: mpsc::UnboundedReceiver<VoiceEvent>) -> Arc<Self> {
        Arc::new(Self {
            events: AsyncMutex::new(events),
        })
    }
}

#[async_trait]
impl ArcStream for VoiceEventStream {
    type Item = VoiceEvent;
    async fn next(self: Arc<Self>) -> Option<VoiceEvent> {
        self.events.lock().await.next().await
    }
}

/// One outcome of a single media step.
enum Step {
    /// A full mic frame is ready to encode and transmit.
    Send,
    /// Mixer capture writer ended; request replacement input.
    MicClosed,
    /// A peer's Opus frame arrived.
    Recv(VoiceFrame),
    /// The idle heartbeat fired — loop back and re-check the kill flag.
    Idle,
    /// The transport closed.
    Closed,
}

/// Call media state behind async mutex for shared poll receiver.
struct SessionState {
    encoder: Encoder,
    /// Per-speaker decoders — Opus carries inter-frame state, so speakers can't
    /// share one.
    decoders: HashMap<u64, Decoder>,
    /// The mic, once the UI has attached it. `None` = not yet, or receive-only.
    mic: Option<MixerMic>,
    /// An in-flight mic request (phase 2 awaits it).
    mic_pending: Option<oneshot::Receiver<SampleConsumer>>,
    mic_retries: u8,
    /// Per-peer playback channels.
    producers: HashMap<u64, SampleProducer>,
    /// In-flight per-peer playback requests, adopted once fulfilled.
    producers_pending: HashMap<u64, oneshot::Receiver<SampleProducer>>,
    /// Reused scratch so a step never reallocates.
    opus_out: [u8; MAX_OPUS_FRAME],
    decoded: [i16; MAX_DECODE_SAMPLES],
}

/// The `AudioEvent` stream for one call: pumps the transport (mic → Opus → send;
/// recv → Opus → playback) and requests the mixer channels it needs.
pub(crate) struct SteamAudioSession {
    transport: Arc<dyn VoiceTransport>,
    /// Set by [`kill`](Self::kill) (from `disconnect`); ends the audio stream.
    killed: AtomicBool,
    /// `AudioEvent`s awaiting delivery to the UI (channel-attach requests).
    outbox: Mutex<VecDeque<AudioEvent>>,
    state: AsyncMutex<SessionState>,
}

impl SteamAudioSession {
    pub(crate) fn new(
        transport: Arc<dyn VoiceTransport>,
    ) -> Result<Arc<Self>, Box<dyn Error + Send + Sync>> {
        let encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Voip)
            .map_err(|err| format!("opus encoder init: {err}"))?;
        Ok(Arc::new(Self {
            transport,
            killed: AtomicBool::new(false),
            outbox: Mutex::new(VecDeque::new()),
            state: AsyncMutex::new(SessionState {
                encoder,
                decoders: HashMap::new(),
                mic: None,
                mic_pending: None,
                mic_retries: 0,
                producers: HashMap::new(),
                producers_pending: HashMap::new(),
                opus_out: [0; MAX_OPUS_FRAME],
                decoded: [0; MAX_DECODE_SAMPLES],
            }),
        }))
    }

    /// Ends stream by next heartbeat and drops mixer channels.
    pub(crate) fn kill(&self) {
        self.killed.store(true, Ordering::Release);
    }

    fn queue_event(&self, event: AudioEvent) {
        self.outbox.lock().unwrap().push_back(event);
    }

    fn pop_event(&self) -> Option<AudioEvent> {
        self.outbox.lock().unwrap().pop_front()
    }

    /// Queues attachment or runs one media step; `Break` ends stream.
    async fn poll_audio(&self) -> ControlFlow<()> {
        let mut state = self.state.lock().await;

        adopt_ready_producers(&mut state);

        // Return between attachment request and adoption.
        if let MicStep::Pending = self.ensure_mic(&mut state).await {
            return ControlFlow::Continue(());
        }

        let mut outgoing_frame = None;
        let step = {
            // Limit outgoing borrow to microphone field.
            let mic = &mut state.mic;
            let mut outgoing = pin!(
                async move {
                    match mic.as_mut() {
                        Some(mic) => mic.next_frame().await,
                        // Receive-only: never produce an outgoing frame.
                        None => std::future::pending::<Option<[f32; FRAME_SAMPLES]>>().await,
                    }
                }
                .fuse()
            );
            let mut incoming = pin!(self.transport.recv().fuse());
            let mut heartbeat = pin!(Delay::new(HEARTBEAT).fuse());
            select! {
                frame = outgoing => match frame {
                    Some(frame) => {
                        outgoing_frame = Some(frame);
                        Step::Send
                    },
                    None => Step::MicClosed,
                },
                packet = incoming => match packet {
                    Ok(frame) => Step::Recv(frame),
                    Err(_) => Step::Closed,
                },
                _ = heartbeat => Step::Idle,
            }
        };

        match step {
            Step::Send => {
                let frame = outgoing_frame
                    .as_ref()
                    .expect("Send step always stores its outgoing frame");
                let SessionState {
                    encoder, opus_out, ..
                } = &mut *state;
                match encoder.encode_float(frame, opus_out) {
                    Ok(len) => {
                        if let Err(err) = self.transport.send(&opus_out[..len]).await {
                            debug!("Steam voice: transport closed on send: {err}");
                            return ControlFlow::Break(());
                        }
                    }
                    Err(err) => warn!("Steam voice: opus encode failed: {err}"),
                }
            }
            Step::Recv(frame) => self.handle_incoming(&mut state, frame),
            Step::MicClosed => {
                debug!("Steam voice: microphone input closed; requesting replacement");
                state.mic = None;
                state.mic_retries = 0;
            }
            Step::Idle => {}
            Step::Closed => {
                debug!("Steam voice: transport closed");
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    /// Ensure the mic is attached. Two-phase: first call queues `AddAudioInput`
    /// and returns `Pending`; a later call (after the UI answered) awaits and
    /// installs the channel. Gives up receive-only after [`MAX_MIC_RETRIES`].
    async fn ensure_mic(&self, state: &mut SessionState) -> MicStep {
        if state.mic.is_some() || state.mic_retries >= MAX_MIC_RETRIES {
            return MicStep::Ready;
        }
        match state.mic_pending.take() {
            None => {
                let (sender, receiver) = oneshot::channel();
                self.queue_event(AudioEvent::AddAudioInput(sender));
                state.mic_pending = Some(receiver);
                MicStep::Pending
            }
            Some(receiver) => match receiver.await {
                Ok(consumer) => {
                    state.mic = Some(MixerMic::new(consumer));
                    state.mic_retries = 0;
                    MicStep::Ready
                }
                Err(_) => {
                    state.mic_retries += 1;
                    warn!(
                        "Steam voice: mic request declined (attempt {}/{MAX_MIC_RETRIES})",
                        state.mic_retries
                    );
                    MicStep::Pending
                }
            },
        }
    }

    /// Decode an incoming frame and route it to its speaker's playback channel,
    /// requesting one (via `AddAudioSource`) on first sight of a peer.
    fn handle_incoming(&self, state: &mut SessionState, frame: VoiceFrame) {
        let VoiceFrame { peer, opus } = frame;
        let sample_count = {
            let decoder = match state.decoders.entry(peer) {
                Entry::Occupied(decoder) => decoder.into_mut(),
                Entry::Vacant(slot) => match Decoder::new(SAMPLE_RATE, Channels::Stereo) {
                    Ok(decoder) => slot.insert(decoder),
                    Err(err) => {
                        warn!("Steam voice: opus decoder init for {peer}: {err}");
                        return;
                    }
                },
            };
            match decoder.decode(&opus, &mut state.decoded, false) {
                Ok(samples_per_channel) => samples_per_channel * CHANNELS,
                Err(err) => {
                    warn!("Steam voice: opus decode failed for {peer}: {err}");
                    return;
                }
            }
        };

        if state
            .producers
            .get(&peer)
            .is_some_and(|producer| !producer.is_connected())
        {
            state.producers.remove(&peer);
            debug!(
                peer,
                "Steam voice: playback mixer channel closed; re-requesting"
            );
        }

        if let Some(producer) = state.producers.get_mut(&peer) {
            // Playback channel is I16 (see the app's create_output_channel).
            producer.push_iter(&state.decoded[..sample_count]);
        } else if let Entry::Vacant(slot) = state.producers_pending.entry(peer) {
            // First frame from this speaker: request a playback channel. This and
            // the next few frames are dropped until the UI attaches it. The peer
            // SteamID doubles as the unified user ID (see `query.rs`).
            let (sender, receiver) = oneshot::channel();
            self.queue_event(AudioEvent::AddAudioSource {
                user: Some(peer),
                channel: sender,
            });
            slot.insert(receiver);
        }
    }
}

#[async_trait]
impl ArcStream for SteamAudioSession {
    type Item = AudioEvent;
    async fn next(self: Arc<Self>) -> Option<AudioEvent> {
        loop {
            if self.killed.load(Ordering::Acquire) {
                return None;
            }
            if let Some(event) = self.pop_event() {
                return Some(event);
            }
            match self.poll_audio().await {
                ControlFlow::Continue(()) => continue,
                ControlFlow::Break(()) => return None,
            }
        }
    }
}

enum MicStep {
    /// Mic attached, or given up (receive-only) — proceed to the media step.
    Ready,
    /// A request was queued or is being awaited — return to `next`.
    Pending,
}

/// Move any playback channels the UI has fulfilled from `producers_pending` into
/// `producers`, and drop requests the UI declined.
fn adopt_ready_producers(state: &mut SessionState) {
    let mut ready = Vec::new();
    let mut declined = Vec::new();
    for (peer, receiver) in state.producers_pending.iter_mut() {
        match receiver.try_recv() {
            Ok(Some(producer)) => ready.push((*peer, producer)),
            Ok(None) => {}
            Err(_) => declined.push(*peer),
        }
    }
    for (peer, producer) in ready {
        state.producers_pending.remove(&peer);
        state.producers.insert(peer, producer);
    }
    for peer in declined {
        state.producers_pending.remove(&peer);
    }
}

#[cfg(test)]
mod tests {
    use super::super::transport::LoopbackTransport;
    use super::*;

    /// The session's first act is to request the mic (the yield state machine's
    /// phase 1), and `kill` ends the stream on the next poll.
    #[tokio::test]
    async fn session_requests_mic_then_stops_on_kill() {
        let transport: Arc<dyn VoiceTransport> = Arc::new(LoopbackTransport::new(1));
        let session = SteamAudioSession::new(transport).unwrap();

        let first = session.clone().next().await;
        assert!(
            matches!(first, Some(AudioEvent::AddAudioInput(_))),
            "the session requests a microphone before anything else"
        );

        session.kill();
        assert!(
            session.next().await.is_none(),
            "kill() ends the audio stream"
        );
    }
}
