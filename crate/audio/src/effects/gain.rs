use crate::{AudioSampleType, StreamFormat};

use super::{Afx, ParamUnit};

/// Multiplies every sample by a linear gain.
///
/// Parameter edits glide over a short window, avoiding zipper noise while
/// keeping the gain a normal rack effect.
#[derive(credential_macro::AfxParams, Clone, Copy, Debug)]
pub struct Gain {
    /// Target linear amplitude multiplier set by UI/automation.
    ///
    /// `apply_to` glides the runtime multiplier toward this value to avoid
    /// zipper noise when the parameter changes.
    #[afx(param(name = Gain::NAME, unit = ParamUnit::Amplitude, min = 0.0, max = 4.0, default = Gain::DEFAULT, min_is_off = false))]
    pub desired_multiplier: f32,
    /// Smoothed multiplier currently being applied to samples.
    current_multiplier: AudioSampleType,
}

impl Gain {
    pub const DEFAULT: f32 = 1.0;
    pub const NAME: &str = "Gain";
    const SMOOTHING_MS: f32 = 15.0;

    pub fn with_multiplier(desired_multiplier: f32) -> Self {
        let desired_multiplier = desired_multiplier.max(0.0);
        Gain {
            desired_multiplier,
            current_multiplier: desired_multiplier,
        }
    }
}

impl Default for Gain {
    fn default() -> Self {
        Gain::with_multiplier(Self::DEFAULT)
    }
}

impl Afx for Gain {
    fn name(&self) -> &'static str {
        Gain::NAME
    }

    fn apply_to(&mut self, format: StreamFormat, audio: &mut [AudioSampleType]) -> bool {
        let channels = (format.channels as usize).max(1);
        // One-pole coefficient reaching ~63% of a step over the smoothing
        // window; a window of one frame or less is instantaneous.
        let frames = Self::SMOOTHING_MS / 1000.0 * format.sample_rate as AudioSampleType;
        let coef = if frames <= 1.0 {
            1.0
        } else {
            1.0 - (-1.0 / frames).exp()
        };

        let target = self.desired_multiplier.max(0.0);
        if target == self.current_multiplier {
            if self.current_multiplier != 1.0 {
                for sample in audio.iter_mut() {
                    *sample *= self.current_multiplier;
                }
            }
            return true;
        }

        for frame in audio.chunks_mut(channels) {
            self.current_multiplier += (target - self.current_multiplier) * coef;
            for sample in frame.iter_mut() {
                *sample *= self.current_multiplier;
            }
        }
        if (target - self.current_multiplier).abs() < 1e-4 {
            self.current_multiplier = target;
        }
        true
    }
}

#[cfg(test)]
#[path = "../../tests/unit/effects/gain.rs"]
mod tests;
