//! Audio capture driver state and polling.

use futures::{FutureExt as _, future, select};
use futures_timer::Delay;
use simple_audio_channels::{AudioSampleType, input::SampleConsumer};

use super::connection::{SendCodec, VOICE_FRAME_SAMPLES};

/// Work emitted by the audio driver for the media coordinator to dispatch.
pub(crate) enum AudioWork {
    Receive {
        ssrc: u32,
        samples: Vec<i16>,
    },
    /// A full outgoing frame is buffered, ready to encode and transmit.
    SendMic,
    /// The mic went silent past the threshold; announce we stopped speaking.
    StopSpeaking,
    /// Mixer capture writer closed. Coordinator releases and reacquires mic.
    MicrophoneEnded,
}

pub(crate) struct AudioProcessor {
    pub(crate) send: Option<SendCodec>,
    pub(crate) microphone: Option<SampleConsumer>,
    pub(crate) frame: [AudioSampleType; VOICE_FRAME_SAMPLES],
    /// Partial fill carried across select cancellations.
    pub(crate) frame_filled: usize,
    pub(crate) stop_speaking_delay: Option<Delay>,
    /// Rate-limits DAVE startup/rotation diagnostics while realtime frames are dropped.
    pub(crate) waiting_for_dave: bool,
}

impl AudioProcessor {
    pub(crate) async fn poll(&mut self) -> AudioWork {
        let Self {
            microphone,
            frame,
            frame_filled,
            stop_speaking_delay,
            ..
        } = self;
        let mut outgoing = std::pin::pin!(
            async move {
                match microphone.as_mut() {
                    Some(microphone) => fill_frame(microphone, frame, frame_filled).await,
                    None => future::pending().await,
                }
            }
            .fuse()
        );
        let mut silence = std::pin::pin!(stop_speaking_or_pending(stop_speaking_delay).fuse());
        select! {
            filled = outgoing => if filled {
                AudioWork::SendMic
            } else {
                AudioWork::MicrophoneEnded
            },
            _ = silence => AudioWork::StopSpeaking,
        }
    }
}

async fn fill_frame(
    mic: &mut SampleConsumer,
    frame: &mut [AudioSampleType; VOICE_FRAME_SAMPLES],
    frame_filled: &mut usize,
) -> bool {
    while *frame_filled < VOICE_FRAME_SAMPLES {
        let Some(written) = mic.pop_or_closed(&mut frame[*frame_filled..]).await else {
            return false;
        };
        *frame_filled += written;
    }
    true
}

async fn stop_speaking_or_pending(delay: &mut Option<Delay>) {
    match delay {
        Some(delay) => delay.await,
        None => future::pending::<()>().await,
    }
}
