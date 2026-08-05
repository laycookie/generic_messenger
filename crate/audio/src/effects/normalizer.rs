//! Streaming linked-channel RMS normalization with peak protection.

use fundsp::prelude::db_amp;

use crate::{
    AudioSampleType, StreamFormat,
    params::{AfxParams, ParamUnit},
};

use super::Afx;

/// Continuously moves linked-channel RMS toward a target level.
///
/// Detector follows loudest channel's mean-square envelope, preserving stereo
/// balance. Gain reduction reacts faster than boost; low-level input is not
/// boosted past [`noise_floor_db`](Self::noise_floor_db). An immediate peak
/// ceiling catches transients while slower envelope catches up.
#[derive(AfxParams, Debug)]
pub struct Normalizer {
    /// Desired RMS level in dBFS.
    #[afx(param(name = "Target RMS", unit = ParamUnit::Decibels, min = -36.0, max = -6.0, default = Normalizer::DEFAULT_TARGET_RMS_DB, min_is_off = false))]
    pub target_rms_db: f32,
    /// Maximum upward gain. Downward gain remains unrestricted.
    #[afx(param(name = "Max boost", unit = ParamUnit::Decibels, min = 0.0, max = 30.0, default = Normalizer::DEFAULT_MAX_BOOST_DB, min_is_off = false))]
    pub max_boost_db: f32,
    /// Input RMS below which upward gain is suppressed. Minimum disables floor.
    #[afx(param(name = "Noise floor", unit = ParamUnit::Decibels, min = Normalizer::NOISE_FLOOR_OFF_DB, max = -30.0, default = Normalizer::DEFAULT_NOISE_FLOOR_DB, min_is_off = true))]
    pub noise_floor_db: f32,
    /// Time for gain to fall when input is louder than target.
    #[afx(param(name = "Attack", unit = ParamUnit::Milliseconds, min = 0.0, max = 500.0, default = Normalizer::DEFAULT_ATTACK_MS, min_is_off = false))]
    pub attack_ms: f32,
    /// Time for gain to rise when input is quieter than target.
    #[afx(param(name = "Release", unit = ParamUnit::Milliseconds, min = 0.0, max = 5000.0, default = Normalizer::DEFAULT_RELEASE_MS, min_is_off = false))]
    pub release_ms: f32,
    /// Hard output peak ceiling in dBFS.
    #[afx(param(name = "Ceiling", unit = ParamUnit::Decibels, min = -12.0, max = 0.0, default = Normalizer::DEFAULT_CEILING_DB, min_is_off = false))]
    pub ceiling_db: f32,

    detector_power: AudioSampleType,
    current_gain: AudioSampleType,
    derived_sample_rate: u32,
    detector_attack_coefficient: AudioSampleType,
    detector_release_coefficient: AudioSampleType,
    gain_attack_coefficient: AudioSampleType,
    gain_release_coefficient: AudioSampleType,
    target_amplitude: AudioSampleType,
    maximum_gain: AudioSampleType,
    noise_floor_power: AudioSampleType,
    ceiling_amplitude: AudioSampleType,
    derived_params: [f32; 6],
}

impl Normalizer {
    /// Display name used by effects-rack catalogs.
    pub const NAME: &str = "Normalizer";
    /// Default target RMS level in dBFS.
    pub const DEFAULT_TARGET_RMS_DB: f32 = -18.0;
    /// Default maximum upward gain in decibels.
    pub const DEFAULT_MAX_BOOST_DB: f32 = 18.0;
    /// Default RMS threshold below which input is not boosted.
    pub const DEFAULT_NOISE_FLOOR_DB: f32 = -55.0;
    /// Default gain-reduction attack time.
    pub const DEFAULT_ATTACK_MS: f32 = 20.0;
    /// Default upward-gain release time.
    pub const DEFAULT_RELEASE_MS: f32 = 750.0;
    /// Default hard peak ceiling in dBFS.
    pub const DEFAULT_CEILING_DB: f32 = -1.0;
    /// Noise-floor parameter value that disables upward-gain suppression.
    pub const NOISE_FLOOR_OFF_DB: f32 = -90.0;

    const DETECTOR_ATTACK_MS: f32 = 10.0;
    const DETECTOR_RELEASE_MS: f32 = 300.0;
    const POWER_EPSILON: f32 = 1e-20;

    /// Normalizer using voice-friendly defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            target_rms_db: Self::DEFAULT_TARGET_RMS_DB,
            max_boost_db: Self::DEFAULT_MAX_BOOST_DB,
            noise_floor_db: Self::DEFAULT_NOISE_FLOOR_DB,
            attack_ms: Self::DEFAULT_ATTACK_MS,
            release_ms: Self::DEFAULT_RELEASE_MS,
            ceiling_db: Self::DEFAULT_CEILING_DB,
            detector_power: 0.0,
            current_gain: 1.0,
            derived_sample_rate: 0,
            detector_attack_coefficient: 1.0,
            detector_release_coefficient: 1.0,
            gain_attack_coefficient: 1.0,
            gain_release_coefficient: 1.0,
            target_amplitude: 1.0,
            maximum_gain: 1.0,
            noise_floor_power: 0.0,
            ceiling_amplitude: 1.0,
            derived_params: [f32::NAN; 6],
        }
    }

    /// Apply code-level parameter edits without resetting live detector state.
    #[must_use]
    pub fn tuned(mut self, tweak: impl FnOnce(&mut Self)) -> Self {
        tweak(&mut self);
        self
    }

    #[inline]
    fn refresh_derived(&mut self, sample_rate: u32) {
        let params = [
            self.target_rms_db,
            self.max_boost_db,
            self.noise_floor_db,
            self.attack_ms,
            self.release_ms,
            self.ceiling_db,
        ];
        if self.derived_sample_rate == sample_rate && self.derived_params == params {
            return;
        }
        self.recompute_derived(sample_rate, params);
    }

    #[cold]
    #[inline(never)]
    fn recompute_derived(&mut self, sample_rate: u32, params: [f32; 6]) {
        self.derived_sample_rate = sample_rate;
        self.detector_attack_coefficient =
            Self::smoothing_coefficient(Self::DETECTOR_ATTACK_MS, sample_rate);
        self.detector_release_coefficient =
            Self::smoothing_coefficient(Self::DETECTOR_RELEASE_MS, sample_rate);
        self.gain_attack_coefficient = Self::smoothing_coefficient(self.attack_ms, sample_rate);
        self.gain_release_coefficient = Self::smoothing_coefficient(self.release_ms, sample_rate);
        self.target_amplitude = db_amp(self.target_rms_db);
        self.maximum_gain = db_amp(self.max_boost_db);
        self.noise_floor_power = if self.noise_floor_db <= Self::NOISE_FLOOR_OFF_DB {
            0.0
        } else {
            db_amp(self.noise_floor_db).powi(2)
        };
        self.ceiling_amplitude = db_amp(self.ceiling_db);
        self.derived_params = params;
    }

    fn smoothing_coefficient(time_ms: f32, sample_rate: u32) -> f32 {
        let frames = time_ms.max(0.0) / 1000.0 * sample_rate.max(1) as f32;
        if frames <= 1.0 {
            1.0
        } else {
            1.0 - (-1.0 / frames).exp()
        }
    }

    #[cold]
    fn linked_mean_square(audio: &[AudioSampleType], channels: usize) -> AudioSampleType {
        let mut power_sum = 0.0;
        let mut frames = 0.0;
        for frame in audio.chunks(channels) {
            power_sum += Self::frame_power(frame);
            frames += 1.0;
        }
        if frames > 0.0 {
            power_sum / frames
        } else {
            0.0
        }
    }

    #[inline]
    fn frame_power(frame: &[AudioSampleType]) -> AudioSampleType {
        frame
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(*sample * *sample))
    }

    #[inline]
    fn update_detector(&mut self, frame_power: AudioSampleType) {
        let coefficient = if frame_power > self.detector_power {
            self.detector_attack_coefficient
        } else {
            self.detector_release_coefficient
        };
        self.detector_power += (frame_power - self.detector_power) * coefficient;
        if frame_power == 0.0 && self.detector_power < Self::POWER_EPSILON {
            self.detector_power = 0.0;
        }
    }

    #[inline]
    fn desired_gain(&self) -> AudioSampleType {
        if self.detector_power <= Self::POWER_EPSILON {
            return 1.0;
        }
        let gain = self.target_amplitude / self.detector_power.sqrt();
        if gain > 1.0 && self.detector_power < self.noise_floor_power {
            1.0
        } else {
            gain.min(self.maximum_gain)
        }
    }
}

impl Default for Normalizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Afx for Normalizer {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn prepare(&mut self, format: StreamFormat) {
        self.detector_power = 0.0;
        self.current_gain = 1.0;
        self.refresh_derived(format.sample_rate);
    }

    fn apply_to(&mut self, format: StreamFormat, audio: &mut [AudioSampleType]) -> bool {
        self.refresh_derived(format.sample_rate);
        let channels = (format.channels as usize).max(1);
        if self.detector_power <= Self::POWER_EPSILON {
            self.detector_power = Self::linked_mean_square(audio, channels);
        }

        for frame in audio.chunks_mut(channels) {
            let frame_power = Self::frame_power(frame);
            self.update_detector(frame_power);

            let desired_gain = self.desired_gain();
            let coefficient = if desired_gain < self.current_gain {
                self.gain_attack_coefficient
            } else {
                self.gain_release_coefficient
            };
            self.current_gain += (desired_gain - self.current_gain) * coefficient;

            // Ceiling acts immediately and lowers envelope state too. Gain can
            // then recover only through configured release, avoiding rebound.
            if frame_power > 0.0 {
                let ceiling_gain = self.ceiling_amplitude / frame_power.sqrt();
                self.current_gain = self.current_gain.min(ceiling_gain);
            }
            for sample in frame {
                *sample *= self.current_gain;
            }
        }

        true
    }
}

#[cfg(test)]
#[path = "../../tests/unit/effects/normalizer.rs"]
mod tests;
