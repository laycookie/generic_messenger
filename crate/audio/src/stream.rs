//! Shared real-time stream lifecycle. The capture and playback sides are
//! near-mirror images; everything that does not depend on the direction lives
//! here: the per-direction [`Master`] (device + config + the running stream),
//! the start/stop scaffolding, sample-format dispatch, and channel-presence
//! tracking. The direction-specific handles and callbacks live in
//! [`crate::input`] and [`crate::output`].

use std::{error::Error, sync::Arc};

use cpal::{SupportedStreamConfig, traits::StreamTrait as _};
use ringbuf::{CachingCons, CachingProd, StaticRb, traits::Split as _};
use tracing::{debug, warn};

use crate::{MIX_SCRATCH_LEN, Notify};

/// Producer/consumer ends of the channel-event ring: control-thread → audio-thread
/// messages that hand a newly created channel to an already-running stream. Eight
/// slots is ample; channels are added rarely and drained every callback.
pub(crate) type EventProd<E> = CachingProd<Arc<StaticRb<E, 8>>>;
pub(crate) type EventCons<E> = CachingCons<Arc<StaticRb<E, 8>>>;

/// A running stream and the live wiring to it: the kept-alive `cpal::Stream`,
/// the producer end of the channel-event ring, and the close notification.
struct StreamHandle<E> {
    _stream: cpal::Stream,
    channel_event_tx: EventProd<E>,
    close_notify: Arc<Notify>,
}

/// One audio direction's device, its preferred config, and its
/// optionally-running stream. `E` is the channel-event type
/// ([`crate::input::InputRxEvent`] or [`crate::output::OutputRxEvent`]).
pub(crate) struct Master<E> {
    device: cpal::Device,
    pub(crate) config: SupportedStreamConfig,
    stream: Option<StreamHandle<E>>,
}

impl<E> Master<E> {
    pub(crate) fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// The close notification, if a stream is currently running.
    pub(crate) fn running_notify(&self) -> Option<Arc<Notify>> {
        self.stream.as_ref().map(|stream| stream.close_notify.clone())
    }

    /// The channel-event producer for the running stream, used to hand a
    /// late-created channel to the audio thread. `None` when not streaming.
    pub(crate) fn event_tx(&mut self) -> Option<&mut EventProd<E>> {
        self.stream.as_mut().map(|stream| &mut stream.channel_event_tx)
    }

    /// Tear down the running stream. Channels survive; a later [`start`](Self::start)
    /// adopts them again.
    pub(crate) fn stop(&mut self) {
        self.stream = None;
    }

    /// Start a stream over this device's preferred config. `build` turns the
    /// resolved config plus the channel snapshot into the typed `cpal::Stream`
    /// — the one piece that differs by direction. Everything around it (channel
    /// validation, the event ring, the close notification, playback, and
    /// installing the handle) is shared.
    pub(crate) fn start<C>(
        &mut self,
        direction: &str,
        channels: C,
        build: impl FnOnce(
            &cpal::Device,
            &cpal::StreamConfig,
            cpal::SampleFormat,
            C,
            EventCons<E>,
            Arc<Notify>,
        ) -> Result<cpal::Stream, Box<dyn Error>>,
    ) -> Result<Option<Arc<Notify>>, Box<dyn Error>> {
        let stream_config = self.config.config();
        let sample_format = self.config.sample_format();
        let device_channels = stream_config.channels as usize;
        if device_channels == 0 || device_channels > MIX_SCRATCH_LEN {
            return Err(format!("unsupported {direction} channel count: {device_channels}").into());
        }
        debug!("Starting {direction} stream with config {stream_config:?}, format {sample_format:?}");

        let (event_prod, event_cons) = StaticRb::<E, 8>::default().split();
        let close_notify = Arc::new(Notify::new());

        let stream = build(
            &self.device,
            &stream_config,
            sample_format,
            channels,
            event_cons,
            close_notify.clone(),
        )?;
        stream.play()?;
        self.stream = Some(StreamHandle {
            _stream: stream,
            channel_event_tx: event_prod,
            close_notify: close_notify.clone(),
        });
        Ok(Some(close_notify))
    }
}

/// Open a direction's default device and resolve its default config into a
/// [`Master`], logging and yielding `None` when either is unavailable.
pub(crate) fn open_master<E>(
    device: Option<cpal::Device>,
    config_of: impl FnOnce(
        &cpal::Device,
    ) -> Result<SupportedStreamConfig, cpal::DefaultStreamConfigError>,
    label: &str,
) -> Option<Master<E>> {
    let device = device?;
    match config_of(&device) {
        Ok(config) => Some(Master {
            device,
            config,
            stream: None,
        }),
        Err(err) => {
            warn!("{label} device has no usable default config: {err}");
            None
        }
    }
}

/// Fire `close_notify` exactly once, on the transition to zero live channels —
/// keeping the notification off the steady-state path. Called by each real-time
/// callback after it has synced its channel list against the event ring.
pub(crate) fn update_presence(any_channels: bool, had_channels: &mut bool, close_notify: &Notify) {
    if any_channels {
        *had_channels = true;
    } else if *had_channels {
        *had_channels = false;
        close_notify.notify_one();
    }
}

/// Monomorphise a typed stream builder over cpal's runtime [`cpal::SampleFormat`],
/// mapping the per-type build error into `Box<dyn Error>`. Centralises the
/// ten-arm dispatch both directions would otherwise spell out by hand.
macro_rules! for_each_sample_format {
    ($fmt:expr, $build:ident $(, $arg:expr)* $(,)?) => {{
        match $fmt {
            ::cpal::SampleFormat::I8 => $build::<i8>($($arg),*).map_err(::core::convert::Into::into),
            ::cpal::SampleFormat::I16 => $build::<i16>($($arg),*).map_err(::core::convert::Into::into),
            ::cpal::SampleFormat::I32 => $build::<i32>($($arg),*).map_err(::core::convert::Into::into),
            ::cpal::SampleFormat::I64 => $build::<i64>($($arg),*).map_err(::core::convert::Into::into),
            ::cpal::SampleFormat::U8 => $build::<u8>($($arg),*).map_err(::core::convert::Into::into),
            ::cpal::SampleFormat::U16 => $build::<u16>($($arg),*).map_err(::core::convert::Into::into),
            ::cpal::SampleFormat::U32 => $build::<u32>($($arg),*).map_err(::core::convert::Into::into),
            ::cpal::SampleFormat::U64 => $build::<u64>($($arg),*).map_err(::core::convert::Into::into),
            ::cpal::SampleFormat::F32 => $build::<f32>($($arg),*).map_err(::core::convert::Into::into),
            ::cpal::SampleFormat::F64 => $build::<f64>($($arg),*).map_err(::core::convert::Into::into),
            other => Err(format!("unsupported device sample format: {other:?}").into()),
        }
    }};
}
pub(crate) use for_each_sample_format;
