//! Allocation-free peak limiting for final mixed output.
//!
//! Mixer channels may each be inside `[-1, 1]` while their sum is not. Hard
//! clipping that sum creates strong distortion. This limiter applies immediate
//! peak reduction per callback chunk, then releases smoothly while headroom is
//! available. State stays owned by audio callback; no locks or allocations.

use cpal::{FromSample, Sample};

use crate::AudioSampleType;

const CEILING: AudioSampleType = 0.95;
const RELEASE_SECONDS: f32 = 0.100;

pub(crate) struct PeakLimiter {
    gain: AudioSampleType,
    release_step: AudioSampleType,
}

impl PeakLimiter {
    pub(crate) fn new(samples_per_second: u32) -> Self {
        let release_samples = (samples_per_second as f32 * RELEASE_SECONDS).max(1.0);
        Self {
            gain: 1.0,
            release_step: 1.0 / release_samples,
        }
    }

    /// Limit `samples` while converting directly into `output`. Keeping gain
    /// application and device conversion in one pass avoids writing the mixed
    /// buffer only to read it again immediately.
    pub(crate) fn process_into<T>(&mut self, samples: &[AudioSampleType], output: &mut [T])
    where
        T: Sample + FromSample<AudioSampleType>,
    {
        debug_assert_eq!(samples.len(), output.len());
        let peak = samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        let target = if peak > CEILING { CEILING / peak } else { 1.0 };

        // Attack before first sample of this block. Release only on blocks
        // with enough headroom, avoiding gain growth into a known peak.
        self.gain = self.gain.min(target);

        if self.gain == 1.0 {
            for (sample, out) in samples.iter().zip(output) {
                *out = T::from_sample(sample.clamp(-1.0, 1.0));
            }
        } else if target < 1.0 {
            let gain = self.gain;
            for (sample, out) in samples.iter().zip(output) {
                *out = T::from_sample((sample * gain).clamp(-1.0, 1.0));
            }
        } else {
            for (sample, out) in samples.iter().zip(output) {
                *out = T::from_sample((sample * self.gain).clamp(-1.0, 1.0));
                self.gain = (self.gain + self.release_step).min(1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_mixed_peak_without_hard_clipping() {
        let mut limiter = PeakLimiter::new(48_000);
        let mixed = [2.0_f32, -2.0, 1.0, -1.0];
        let mut limited = [0.0_f32; 4];

        limiter.process_into(&mixed, &mut limited);

        assert!(limited.iter().all(|sample| sample.abs() <= CEILING));
        assert_eq!(limited[0], CEILING);
        assert_eq!(limited[1], -CEILING);
        assert!(limited[2].abs() < 1.0);
    }

    #[test]
    fn gain_recovers_after_loud_block() {
        let mut limiter = PeakLimiter::new(100);
        let loud = [2.0_f32];
        limiter.process_into(&loud, &mut [0.0_f32]);
        let reduced = limiter.gain;

        let quiet = [0.1_f32; 20];
        limiter.process_into(&quiet, &mut [0.0_f32; 20]);

        assert!(limiter.gain > reduced);
        assert!(limiter.gain <= 1.0);
    }

    #[test]
    fn unity_path_copies_without_changing_samples() {
        let mut limiter = PeakLimiter::new(48_000);
        let mixed = [0.25_f32, -0.5, 0.75];
        let mut output = [0.0_f32; 3];

        limiter.process_into(&mixed, &mut output);

        assert_eq!(output, mixed);
        assert_eq!(limiter.gain, 1.0);
    }
}
