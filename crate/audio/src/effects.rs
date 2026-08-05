use std::fmt;

use crate::{AudioSampleType, StreamFormat, params::AfxParams};

mod compressor;
mod gain;
mod gate;
mod low_cut;
mod mute;
mod normalizer;

pub use compressor::Compressor;
pub use gain::Gain;
pub use gate::Gate;
pub use low_cut::LowCut;
pub use mute::{Mute, MuteSwitch};
pub use normalizer::Normalizer;

/// One effect available for user insertion into an effects rack.
///
/// Registrations live in the audio crate, keeping UI code independent from
/// concrete effect types. Internal plumbing effects such as [`Mute`] are not
/// registered and therefore never appear in effect pickers.
#[derive(Clone, Copy)]
pub struct AfxRegistration {
    name: &'static str,
    build: fn() -> Box<dyn Afx + Send>,
}

impl AfxRegistration {
    const fn new(name: &'static str, build: fn() -> Box<dyn Afx + Send>) -> Self {
        Self { name, build }
    }

    pub fn name(self) -> &'static str {
        self.name
    }

    pub fn build(self) -> Box<dyn Afx + Send> {
        (self.build)()
    }
}

impl fmt::Debug for AfxRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AfxRegistration")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// An audio effect applied to a channel's sample stream.
///
/// Effects run on the control thread, not in the real-time audio callback.
/// `false` from [`apply_to`](Afx::apply_to) drops the whole buffer and
/// short-circuits the remaining chain.
pub trait Afx: AfxParams {
    /// Display name for UI listings of a rack's contents.
    fn name(&self) -> &'static str;

    /// Called before the effect processes any audio, with the format it will
    /// process in.
    fn prepare(&mut self, _format: StreamFormat) {}

    /// Process `audio` in `format`. `format` is passed per buffer so effect
    /// logic can read the current pipeline context without storing it.
    fn apply_to(&mut self, format: StreamFormat, audio: &mut [AudioSampleType]) -> bool;
}

fn default_afx<T>() -> Box<dyn Afx + Send>
where
    T: Afx + Default + Send + 'static,
{
    Box::new(T::default())
}

/// User-insertable built-in effects. [`EffectsRack`](crate::effects_rack::EffectsRack)
/// exposes this catalog to clients.
pub(crate) const REGISTERED_AFX: &[AfxRegistration] = &[
    AfxRegistration::new(Gate::NAME, default_afx::<Gate>),
    AfxRegistration::new(Gain::NAME, default_afx::<Gain>),
    AfxRegistration::new(LowCut::NAME, default_afx::<LowCut>),
    AfxRegistration::new(Compressor::NAME, default_afx::<Compressor>),
    AfxRegistration::new(Normalizer::NAME, default_afx::<Normalizer>),
];
