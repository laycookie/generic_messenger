use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{AudioSampleType, StreamFormat, params::AfxParams};

use super::Afx;

/// Shared handle to a hard mute. Cloneable; setting is lock-free.
///
/// Unlike gain at zero, mute discards buffers. Input consumers then park as
/// they do on silence, so voice speaking detection drops out and unmuting
/// never replays backlog.
#[derive(Clone, Default)]
pub struct MuteSwitch(Arc<AtomicBool>);

impl MuteSwitch {
    pub fn set(&self, muted: bool) {
        self.0.store(muted, Ordering::Relaxed);
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Paramless rack effect controlled by a [`MuteSwitch`].
#[derive(AfxParams, Clone, Default)]
pub struct Mute {
    switch: MuteSwitch,
}

impl Mute {
    const NAME: &str = "Mute";

    pub fn new() -> Self {
        Self::default()
    }

    pub fn switch(&self) -> MuteSwitch {
        self.switch.clone()
    }
}

impl Afx for Mute {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn apply_to(&mut self, _format: StreamFormat, _audio: &mut [AudioSampleType]) -> bool {
        !self.switch.get()
    }
}

#[cfg(test)]
#[path = "../../tests/unit/effects/mute.rs"]
mod tests;
