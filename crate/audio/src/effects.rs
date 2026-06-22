use crate::{AudioSampleType, StreamFormat};

mod gain;
mod gate;

pub use gain::Gain;
pub use gate::{Gate, GateSettings};

/// A boxed effect chain, as accepted by the channel constructors.
pub type EffectChain = Vec<Box<dyn Afx + Send>>;

/// An audio effect applied to a channel's sample stream.
///
/// Effects run on the control thread — inside `SampleProducer::push` and
/// `SampleConsumer::pop` — never inside the real-time audio callback. They
/// process interleaved [`AudioSampleType`] audio *after* format conversion, i.e. in the
/// stream's destination format:
/// - output channels: the device format (device sample rate / channel count),
/// - input channels: the format the channel was created with.
///
/// `audio` always contains a whole number of frames.
///
/// Returns whether the buffer should be kept. `false` discards it entirely, so
/// it is never emitted (output) nor returned to the caller (input) — a gate
/// uses this to drop silence rather than forward it. Effects form a chain
/// applied in order; the first to return `false` discards the buffer and
/// short-circuits the rest.
pub trait Afx {
    /// Called once, before any `apply_to`, with the format the effect will
    /// process in — the device format for output channels, the channel's
    /// declared format for input channels. Lets time- or rate-dependent
    /// effects (e.g. a [`Gate`]'s attack/hold/release) resolve their settings
    /// into sample counts. Stateless effects ignore it (the default no-op).
    fn prepare(&mut self, _format: StreamFormat) {}

    fn apply_to(&mut self, audio: &mut [AudioSampleType]) -> bool;
}
