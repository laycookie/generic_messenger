use fundsp::prelude::db_amp;

use crate::{
    AudioSampleType, StreamFormat,
    params::{AfxParams, ParamUnit},
    simd,
};

use super::Afx;

/// Reduces signal level above a threshold while preserving quieter audio.
///
/// Detection is peak-based and linked across every channel in a frame. The
/// same gain is therefore applied to all channels, preserving the stereo
/// image. Gain reduction is smoothed independently on attack and release;
/// makeup gain is applied after the compressor envelope.
#[derive(AfxParams, Debug)]
pub struct Compressor {
    /// Level above which compression begins.
    #[afx(param(name = "Threshold", unit = ParamUnit::Decibels, min = -60.0, max = 0.0, default = Compressor::DEFAULT_THRESHOLD_DB, min_is_off = false))]
    pub threshold_db: f32,
    /// Input-level change required for 1dB of output-level change above the
    /// threshold. `1.0` disables gain reduction.
    #[afx(param(name = "Ratio", unit = ParamUnit::Ratio, min = 1.0, max = 20.0, default = Compressor::DEFAULT_RATIO, min_is_off = false))]
    pub ratio: f32,
    /// Time for gain reduction to respond to a louder signal.
    #[afx(param(name = "Attack", unit = ParamUnit::Milliseconds, min = 0.0, max = 100.0, default = Compressor::DEFAULT_ATTACK_MS, min_is_off = false))]
    pub attack_ms: f32,
    /// Time for gain reduction to recover after the signal falls.
    #[afx(param(name = "Release", unit = ParamUnit::Milliseconds, min = 0.0, max = 1000.0, default = Compressor::DEFAULT_RELEASE_MS, min_is_off = false))]
    pub release_ms: f32,
    /// Gain added after compression.
    #[afx(param(name = "Makeup", unit = ParamUnit::Decibels, min = 0.0, max = 24.0, default = Compressor::DEFAULT_MAKEUP_DB, min_is_off = false))]
    pub makeup_db: f32,

    /// Current positive gain reduction in dB, carried across buffers.
    gain_reduction_db: AudioSampleType,
    derived_sample_rate: u32,
    attack_coefficient: AudioSampleType,
    release_coefficient: AudioSampleType,
    threshold_amplitude: AudioSampleType,
    ratio_slope: AudioSampleType,
    makeup_gain: AudioSampleType,
    derived_params: [f32; 5],
}

impl Compressor {
    pub const NAME: &str = "Compressor";
    pub const DEFAULT_THRESHOLD_DB: f32 = -18.0;
    pub const DEFAULT_RATIO: f32 = 4.0;
    pub const DEFAULT_ATTACK_MS: f32 = 10.0;
    pub const DEFAULT_RELEASE_MS: f32 = 100.0;
    pub const DEFAULT_MAKEUP_DB: f32 = 0.0;

    pub fn new() -> Self {
        Self {
            threshold_db: Self::DEFAULT_THRESHOLD_DB,
            ratio: Self::DEFAULT_RATIO,
            attack_ms: Self::DEFAULT_ATTACK_MS,
            release_ms: Self::DEFAULT_RELEASE_MS,
            makeup_db: Self::DEFAULT_MAKEUP_DB,
            gain_reduction_db: 0.0,
            derived_sample_rate: 0,
            attack_coefficient: 1.0,
            release_coefficient: 1.0,
            threshold_amplitude: 0.0,
            ratio_slope: 0.0,
            makeup_gain: 1.0,
            derived_params: [f32::NAN; 5],
        }
    }

    /// Apply code-level parameter edits without resetting the live envelope.
    pub fn tuned(mut self, tweak: impl FnOnce(&mut Self)) -> Self {
        tweak(&mut self);
        self
    }

    #[inline]
    fn refresh_derived(&mut self, sample_rate: u32) {
        let params = [
            self.threshold_db,
            self.ratio,
            self.attack_ms,
            self.release_ms,
            self.makeup_db,
        ];
        if self.derived_sample_rate == sample_rate && self.derived_params == params {
            return;
        }
        self.recompute_derived(sample_rate, params);
    }

    #[cold]
    #[inline(never)]
    fn recompute_derived(&mut self, sample_rate: u32, params: [f32; 5]) {
        let ratio = self.ratio.max(1.0);
        self.derived_sample_rate = sample_rate;
        self.attack_coefficient = Self::smoothing_coefficient(self.attack_ms, sample_rate);
        self.release_coefficient = Self::smoothing_coefficient(self.release_ms, sample_rate);
        self.threshold_amplitude = db_amp(self.threshold_db);
        self.ratio_slope = 1.0 - ratio.recip();
        self.makeup_gain = db_amp(self.makeup_db);
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
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Afx for Compressor {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn prepare(&mut self, format: StreamFormat) {
        self.gain_reduction_db = 0.0;
        self.refresh_derived(format.sample_rate);
    }

    fn apply_to(&mut self, format: StreamFormat, audio: &mut [AudioSampleType]) -> bool {
        let channels = (format.channels as usize).max(1);
        self.refresh_derived(format.sample_rate);
        let attack = self.attack_coefficient;
        let release = self.release_coefficient;
        let threshold_amplitude = self.threshold_amplitude;
        let ratio_slope = self.ratio_slope;
        let makeup_gain = self.makeup_gain;

        // A whole quiet buffer needs neither logarithms nor per-frame peak
        // detection. Active buffers usually fail this scan on their first
        // sample, leaving their inner loop as lean as the original path.
        let releases_only =
            ratio_slope == 0.0 || simd::all_magnitude_le(audio, threshold_amplitude);
        if releases_only {
            if self.gain_reduction_db == 0.0 {
                if makeup_gain != 1.0 {
                    simd::multiply(audio, makeup_gain);
                }
                return true;
            }

            for frame in audio.chunks_mut(channels) {
                self.gain_reduction_db += -self.gain_reduction_db * release;
                let gain = db_amp(self.makeup_db - self.gain_reduction_db);
                for sample in frame {
                    *sample *= gain;
                }
            }
            if self.gain_reduction_db.abs() < 1e-4 {
                self.gain_reduction_db = 0.0;
            }
            return true;
        }

        for frame in audio.chunks_mut(channels) {
            let peak = frame
                .iter()
                .fold(0.0_f32, |frame_peak, sample| frame_peak.max(sample.abs()));
            let level_db = if peak > 0.0 {
                20.0 * peak.log10()
            } else {
                f32::NEG_INFINITY
            };
            let target_reduction = (level_db - self.threshold_db).max(0.0) * ratio_slope;
            let coefficient = if target_reduction > self.gain_reduction_db {
                attack
            } else {
                release
            };
            self.gain_reduction_db += (target_reduction - self.gain_reduction_db) * coefficient;

            let gain = db_amp(self.makeup_db - self.gain_reduction_db);
            for sample in frame {
                *sample *= gain;
            }
        }

        true
    }
}

#[cfg(test)]
#[path = "../../tests/unit/effects/compressor.rs"]
mod tests;
