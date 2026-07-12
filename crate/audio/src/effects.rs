use crate::{AudioSampleType, StreamFormat, params::AfxParams};

mod gain;
mod gate;
mod low_cut;
mod mute;

pub use gain::Gain;
pub use gate::Gate;
pub use low_cut::LowCut;
pub use mute::{Mute, MuteSwitch};

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
