use crate::AudioSampleType;

use super::Afx;

/// Multiplies every sample by a constant factor.
pub struct Gain {
    pub multiplier: AudioSampleType,
}

impl Afx for Gain {
    fn apply_to(&mut self, audio: &mut [AudioSampleType]) -> bool {
        for sample in audio.iter_mut() {
            *sample *= self.multiplier;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_scales_and_keeps() {
        let mut audio = [0.1, -0.2, 0.3, -0.4];
        let kept = Gain { multiplier: 2.0 }.apply_to(&mut audio);
        assert!(kept);
        assert_eq!(audio, [0.2, -0.4, 0.6, -0.8]);
    }
}
