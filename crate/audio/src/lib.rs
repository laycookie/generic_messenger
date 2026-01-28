use std::{
    marker::PhantomData,
    ops::DerefMut,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    task::{Poll, Waker},
};

use cpal::{ChannelCount, traits::HostTrait};
pub use cpal::{SampleFormat, SampleRate};
use ringbuf::{StaticRb, wrap::caching::Caching};

pub use ringbuf::traits::{Consumer, Producer};
use tracing::{error, warn};

use crate::{
    effects::Afx,
    input::{Input, InputRxEvent},
    output::{Output, OutputRxEvent},
};

pub mod effects;
pub mod input;
pub mod output;

type SampleRb<const N: usize> = Arc<StaticRb<AudioSampleType, N>>;
type SampleProducer<const N: usize> = Caching<SampleRb<N>, true, false>;
type SampleConsumer<const N: usize> = Caching<SampleRb<N>, false, true>;

pub type AudioSampleType = f32;

struct Channel<const N: usize, C: ChannelType<N>> {
    channel_count: ChannelCount, // Mono(1), Setro(2), etc.
    sample_format: SampleFormat,
    sample_rate: SampleRate,
    effects: Vec<Box<dyn Afx + Send + Sync>>,
    rb: SampleRb<N>,
    _marker: PhantomData<C>,
}

trait ChannelType<const N: usize>: DerefMut {
    fn from(rb: SampleRb<N>) -> Self;
}

impl<const N: usize, C: ChannelType<N>> Channel<N, C> {
    fn new(
        channel_mode: ChannelCount,
        sample_format: SampleFormat,
        sample_rate: SampleRate,
    ) -> (Channel<N, C>, C) {
        let rb: SampleRb<N> = StaticRb::default().into();
        let c = C::from(rb.clone());

        let channel = Channel {
            channel_count: channel_mode,
            sample_format,
            sample_rate,
            effects: Vec::new(),
            rb,
            _marker: PhantomData,
        };
        (channel, c)
    }
}

#[derive(Clone)]
pub struct Notify(Arc<InnerNotify>);
struct InnerNotify {
    waker: OnceLock<Waker>,
    notified: AtomicBool,
}
impl Future for Notify {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.0.notified.load(Ordering::Relaxed) {
            true => Poll::Ready(()),
            false => {
                self.0.waker.set(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
impl Notify {
    fn new() -> Self {
        Self(Arc::new(InnerNotify {
            waker: OnceLock::new(),
            notified: AtomicBool::new(false),
        }))
    }

    fn notify(&self) {
        self.0.notified.store(true, Ordering::Relaxed);
        if let Some(waker) = &self.0.waker.get() {
            waker.wake_by_ref();
        }
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
    output_channels: Vec<Channel<5120, Output<5120>>>,
    input_channels: Vec<Channel<5120, Input<5120>>>,
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
