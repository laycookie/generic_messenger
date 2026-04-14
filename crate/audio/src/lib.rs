use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};

use cpal::{ChannelCount, traits::HostTrait};
pub use cpal::{SampleFormat, SampleRate};
use futures::task::AtomicWaker;
use ringbuf::{StaticRb, wrap::caching::Caching};

pub use ringbuf::traits::{Consumer, Producer};

use crate::{
    effects::Afx,
    input::{Input, InputRxEvent},
    output::{Output, OutputRxEvent},
};

pub mod effects;
pub mod input;
pub mod output;

pub type SampleRb<const N: usize> = Arc<StaticRb<AudioSampleType, N>>;
pub type SampleProd<const N: usize> = Caching<SampleRb<N>, true, false>;
pub type SampleConsum<const N: usize> = Caching<SampleRb<N>, false, true>;

pub type AudioSampleType = f32;

// TODO: Reduce
/// Number of samples in audio channel ring buffers (~129ms at 48kHz stereo).
pub const CHANNEL_BUFFER_SIZE: usize = 12400;

trait ChannelType {
    fn new() -> Self;
}

struct Channel<C: ChannelType> {
    channel_count: ChannelCount,
    sample_format: SampleFormat,
    sample_rate: SampleRate,
    effects: Vec<Box<dyn Afx + Send + Sync>>,
    interface: C,
}

impl<C: ChannelType> Channel<C> {
    fn new(
        channel_mode: ChannelCount,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> Channel<C> {
        Channel {
            channel_count: channel_mode,
            sample_format,
            sample_rate,
            effects: Vec::new(),
            interface: C::new(),
        }
    }
}

#[derive(Default)]
struct InnerNotify {
    waker: AtomicWaker,
    notified: AtomicBool,
}

pub struct Notified<'a>(&'a InnerNotify);
impl Future for Notified<'_> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // Fast path: check first
        if self.0.notified.swap(false, Ordering::Acquire) {
            return Poll::Ready(());
        }

        // Register the waker
        self.0.waker.register(cx.waker());

        // Check AGAIN to avoid race condition:
        if self.0.notified.swap(false, Ordering::Acquire) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[derive(Clone, Default)]
pub struct Notify(Arc<InnerNotify>);
impl Notify {
    fn new() -> Self {
        Self(Arc::new(InnerNotify {
            waker: AtomicWaker::new(),
            notified: AtomicBool::new(false),
        }))
    }
    pub fn notified(&self) -> Notified<'_> {
        Notified(&self.0)
    }
    fn notify(&self) {
        self.0.notified.store(true, Ordering::Release);
        self.0.waker.wake();
    }
}

pub(crate) struct OutputStream {
    stream: cpal::Stream,
    to_audio_thread: Caching<Arc<StaticRb<OutputRxEvent, 8>>, true, false>,
    // reciver: oneshot::Receiver<TxEvent>,
}

pub(crate) struct InputStream {
    stream: cpal::Stream,
    to_audio_thread: Caching<Arc<StaticRb<InputRxEvent, 8>>, true, false>,
    // reciver: oneshot::Receiver<TxEvent>,
}

struct OutputMaster {
    device: cpal::Device,
    stream: Option<OutputStream>,
}

struct InputMaster {
    device: cpal::Device,
    stream: Option<InputStream>,
}

pub struct AudioMixer {
    output: Option<OutputMaster>,
    input: Option<InputMaster>,
    output_channels: Vec<Channel<Output>>,
    input_channels: Vec<Channel<Input>>,
}
impl Default for AudioMixer {
    fn default() -> Self {
        let mut audio_mixer = AudioMixer {
            output: None,
            input: None,
            output_channels: Vec::new(),
            input_channels: Vec::new(),
        };

        let host = cpal::default_host();

        if let Some(output) = host.default_output_device() {
            let main = OutputMaster {
                device: output,
                stream: None,
            };
            audio_mixer.output = Some(main);
        }
        if let Some(input) = host.default_input_device() {
            let main = InputMaster {
                device: input,
                stream: None,
            };
            audio_mixer.input = Some(main);
        }

        audio_mixer
    }
}
impl AudioMixer {
    pub fn is_streaming_output(&self) -> bool {
        if let Some(output) = &self.output
            && output.stream.is_some()
        {
            true
        } else {
            false
        }
    }

    pub fn is_streaming_input(&self) -> bool {
        if let Some(input) = &self.input
            && input.stream.is_some()
        {
            true
        } else {
            false
        }
    }
}
