use fundsp::{
    audiounit::AudioUnit,
    prelude::{Shared, highpass_q, pass, shared, var},
};

use crate::{AudioSampleType, StreamFormat, graph::GraphHost};

use super::{Afx, ParamUnit};

/// Removes rumble below the cutoff: a 2nd-order Butterworth high-pass,
/// typically parked just under the voice band to cut desk thumps, plosive
/// pops and traffic without touching speech.
#[derive(credential_macro::AfxParams)]
#[afx(after_set = retune)]
pub struct LowCut {
    /// Frequency below which the signal is rolled off.
    #[afx(param(name = "Cutoff", unit = ParamUnit::Hertz, min = 20.0, max = 500.0, default = LowCut::DEFAULT_CUTOFF, min_is_off = false))]
    pub cutoff_hz: f32,
    /// Live control the graph's `var` node reads; shared by every channel.
    cutoff: Shared,
    /// The mono filter, cloned per channel by the host on `prepare`.
    prototype: Box<dyn AudioUnit>,
    /// Built in `prepare`, so the channel's actual format drives the graph.
    host: Option<GraphHost>,
}

impl LowCut {
    pub const NAME: &str = "Low cut";
    pub const DEFAULT_CUTOFF: f32 = 100.0;

    pub fn new() -> Self {
        let cutoff = shared(Self::DEFAULT_CUTOFF);
        let prototype: Box<dyn AudioUnit> =
            Box::new((pass() | var(&cutoff)) >> highpass_q::<f32>(0.707));
        LowCut {
            cutoff_hz: Self::DEFAULT_CUTOFF,
            cutoff,
            prototype,
            host: None,
        }
    }

    fn retune(&mut self) {
        self.cutoff.set_value(self.cutoff_hz);
    }
}

impl Default for LowCut {
    fn default() -> Self {
        Self::new()
    }
}

impl Afx for LowCut {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn prepare(&mut self, format: StreamFormat) {
        self.host = Some(GraphHost::per_channel(self.prototype.clone(), format));
    }

    fn apply_to(&mut self, _format: StreamFormat, audio: &mut [AudioSampleType]) -> bool {
        if let Some(host) = &mut self.host {
            host.process(audio);
        }
        true
    }
}

#[cfg(test)]
#[path = "../../tests/unit/effects/low_cut.rs"]
mod tests;
