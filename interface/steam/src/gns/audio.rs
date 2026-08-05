//! The Opus ⇄ audio-graph bridge — the transport-agnostic heart of Steam voice
//! media.
//!
//! Given a connected [`VoiceTransport`], this encodes the local microphone
//! outward and decodes peers inward, bridging the messenger-agnostic audio graph
//! (`simple_audio_channels`) to Opus. It mirrors Discord's
//! `gateways/voice/audio.rs`, minus the RTP/encryption Discord layers on top —
//! here that transport concern lives behind [`VoiceTransport`].
//!
//! Because it is written against the transport trait and the [`MicSource`] /
//! [`PeerSinks`] seams, the whole bridge is exercisable with an in-memory
//! loopback transport and fake mic/sink — see the tests. So the audio plane is
//! proven even though the GNS transport under it is not built yet.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::pin::pin;

use async_trait::async_trait;
use futures::{FutureExt as _, select};
use opus::{Application, Channels, Decoder, Encoder};
use simple_audio_channels::input::SampleConsumer;
use tracing::warn;

use super::transport::{TransportError, VoiceFrame, VoiceTransport};

/// Steam chat voice — like the audio graph and Discord — runs at 48 kHz.
pub(crate) const SAMPLE_RATE: u32 = 48_000;
/// Stereo, interleaved (L, R, L, R, …).
pub(crate) const CHANNELS: usize = 2;
/// One 20 ms frame: 960 samples per channel.
pub(crate) const FRAME_SAMPLES: usize = 960 * CHANNELS;
/// Max size of one encoded Opus frame (the Opus hard cap is 1275 bytes).
pub(crate) const MAX_OPUS_FRAME: usize = 1276;
/// Opus can decode up to 120 ms @ 48 kHz stereo = 11520 samples.
pub(crate) const MAX_DECODE_SAMPLES: usize = 11_520;

/// Source of local microphone PCM (48 kHz stereo interleaved f32).
///
/// `next_frame` must be **cancel-safe**: the bridge races it against incoming
/// audio in a `select!`, so a partially-filled frame has to persist in `self`
/// and resume on the next call, or `select` cancellation would drop mic samples.
#[async_trait]
pub(crate) trait MicSource: Send {
    /// Resolve once a whole frame is captured; `None` when mixer writer ends.
    async fn next_frame(&mut self) -> Option<[f32; FRAME_SAMPLES]>;
}

/// Sink for decoded per-peer PCM (48 kHz stereo interleaved f32).
///
/// The production impl routes each `peer` to its own mixer playback channel
/// (`SampleProducer`, obtained via `AudioEvent::AddAudioSource`); see the
/// messenger-stream integration in `VOICE_PROTOCOL.md`.
pub(crate) trait PeerSinks: Send {
    /// Play `samples` back as coming from `peer`.
    fn push(&mut self, peer: u64, samples: &[f32]);
}

/// Drive one call's audio over `transport`: encode the mic outward, decode peers
/// inward. Runs until the transport closes (then returns its error).
///
/// A **per-peer** decoder is kept because Opus carries inter-frame state —
/// feeding interleaved speakers through one decoder corrupts them.
pub(crate) async fn run(
    transport: &dyn VoiceTransport,
    mic: &mut dyn MicSource,
    sinks: &mut dyn PeerSinks,
) -> Result<(), TransportError> {
    let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Voip)
        .map_err(|err| TransportError::Io(format!("opus encoder init: {err}")))?;
    let mut decoders: HashMap<u64, Decoder> = HashMap::new();
    let mut opus_out = [0u8; MAX_OPUS_FRAME];
    let mut decoded = [0f32; MAX_DECODE_SAMPLES];

    loop {
        let mut incoming = pin!(transport.recv().fuse());
        let mut outgoing = pin!(mic.next_frame().fuse());

        select! {
            frame = outgoing => {
                let Some(frame) = frame else {
                    return Err(TransportError::Io("microphone input closed".to_owned()));
                };
                let encoded_len = match encoder.encode_float(&frame, &mut opus_out) {
                    Ok(len) => len,
                    Err(err) => {
                        warn!("Steam voice: opus encode failed: {err}");
                        continue;
                    }
                };
                transport.send(&opus_out[..encoded_len]).await?;
            }
            packet = incoming => {
                let VoiceFrame { peer, opus } = packet?;
                let decoder = match decoders.entry(peer) {
                    Entry::Occupied(decoder) => decoder.into_mut(),
                    Entry::Vacant(slot) => match Decoder::new(SAMPLE_RATE, Channels::Stereo) {
                        Ok(decoder) => slot.insert(decoder),
                        Err(err) => {
                            warn!("Steam voice: opus decoder init for {peer}: {err}");
                            continue;
                        }
                    },
                };
                match decoder.decode_float(&opus, &mut decoded, false) {
                    Ok(samples_per_channel) => {
                        sinks.push(peer, &decoded[..samples_per_channel * CHANNELS]);
                    }
                    Err(err) => warn!("Steam voice: opus decode failed for {peer}: {err}"),
                }
            }
        }
    }
}

/// Production microphone: a mixer input channel.
///
/// Accumulates whole frames from the [`SampleConsumer`], persisting a partial
/// fill across cancellation (the bridge races [`MicSource::next_frame`] in a
/// `select!`). `SampleConsumer::pop` parks on silence — the input gate drops
/// silent buffers — so this naturally transmits only while the mic is live.
pub(crate) struct MixerMic {
    consumer: SampleConsumer,
    frame: [f32; FRAME_SAMPLES],
    filled: usize,
}

impl MixerMic {
    pub(crate) fn new(consumer: SampleConsumer) -> Self {
        Self {
            consumer,
            frame: [0.0; FRAME_SAMPLES],
            filled: 0,
        }
    }
}

#[async_trait]
impl MicSource for MixerMic {
    async fn next_frame(&mut self) -> Option<[f32; FRAME_SAMPLES]> {
        while self.filled < FRAME_SAMPLES {
            self.filled += self
                .consumer
                .pop_or_closed(&mut self.frame[self.filled..])
                .await?;
        }
        let frame = self.frame;
        self.filled = 0;
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use super::super::transport::LoopbackTransport;
    use super::*;

    /// One 20 ms stereo frame of a 0.5-amplitude 440 Hz sine.
    fn sine_frame() -> [f32; FRAME_SAMPLES] {
        let mut frame = [0f32; FRAME_SAMPLES];
        for sample in 0..FRAME_SAMPLES / CHANNELS {
            let value = (2.0 * std::f32::consts::PI * 440.0 * sample as f32 / SAMPLE_RATE as f32)
                .sin()
                * 0.5;
            frame[sample * CHANNELS] = value;
            frame[sample * CHANNELS + 1] = value;
        }
        frame
    }

    /// A mic that emits a fixed list of frames, then goes (permanently) silent.
    struct FakeMic {
        frames: VecDeque<[f32; FRAME_SAMPLES]>,
    }

    #[async_trait]
    impl MicSource for FakeMic {
        async fn next_frame(&mut self) -> Option<[f32; FRAME_SAMPLES]> {
            match self.frames.pop_front() {
                Some(frame) => Some(frame),
                // No more mic input: park forever, like a silent gate.
                None => std::future::pending().await,
            }
        }
    }

    /// A sink that concatenates all delivered samples per peer.
    #[derive(Default)]
    struct CollectSinks {
        received: HashMap<u64, Vec<f32>>,
    }

    impl PeerSinks for CollectSinks {
        fn push(&mut self, peer: u64, samples: &[f32]) {
            self.received
                .entry(peer)
                .or_default()
                .extend_from_slice(samples);
        }
    }

    /// The Opus config the bridge uses round-trips a full 20 ms frame back to a
    /// full 20 ms frame (960 samples/channel).
    #[test]
    fn opus_roundtrip_preserves_frame() {
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Voip).unwrap();
        let mut decoder = Decoder::new(SAMPLE_RATE, Channels::Stereo).unwrap();

        let frame = sine_frame();
        let mut opus_buf = [0u8; MAX_OPUS_FRAME];
        let encoded_len = encoder.encode_float(&frame, &mut opus_buf).unwrap();
        assert!(encoded_len > 0, "encoder produced a packet");

        let mut decoded = [0f32; MAX_DECODE_SAMPLES];
        let samples_per_channel = decoder
            .decode_float(&opus_buf[..encoded_len], &mut decoded, false)
            .unwrap();
        assert_eq!(samples_per_channel, FRAME_SAMPLES / CHANNELS);
    }

    /// The full bridge: a mic frame is encoded, carried by the loopback
    /// transport, decoded, and delivered to the peer's sink — proving the audio
    /// plane end-to-end without any network.
    #[tokio::test]
    async fn bridge_encodes_carries_and_delivers_audio() {
        const PEER: u64 = 76561190000000000;
        let transport = LoopbackTransport::new(PEER);
        let mut mic = FakeMic {
            frames: VecDeque::from(vec![sine_frame()]),
        };
        let mut sinks = CollectSinks::default();

        // `run` loops until the transport closes; here it never does, so bound it
        // with a timeout — by then the single frame has made the full round trip.
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            run(&transport, &mut mic, &mut sinks),
        )
        .await;

        let delivered = sinks
            .received
            .get(&PEER)
            .expect("decoded audio delivered for the peer");
        assert_eq!(
            delivered.len(),
            FRAME_SAMPLES,
            "one 20 ms stereo frame decoded"
        );
        let energy: f32 = delivered.iter().map(|sample| sample * sample).sum();
        assert!(
            energy > 1.0,
            "decoded frame carries signal energy (got {energy})"
        );
    }
}
