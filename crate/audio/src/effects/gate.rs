use fundsp::prelude::{AFollow, An, afollow, db_amp};

use crate::{
    AudioSampleType, StreamFormat,
    params::{AfxParams, ParamUnit},
};

use super::Afx;

/// A noise gate with attack / hold / release envelope shaping.
///
/// While the signal sits above [`threshold_db`](Gate::threshold_db) the gate
/// opens to unity gain; when it falls away the gate holds, then releases down
/// to the [`floor`](Gate::floor_db). The detector is the per-frame peak
/// across channels, so the envelope tracks the loudest channel.
///
/// When the floor is full silence (`floor_db` at [`Gate::FLOOR_OFF`]) and an
/// entire buffer lands on it, `apply_to` *drops* the buffer rather than
/// forwarding silence — so a gated input channel falls quiet and its
/// consumer's `pop` parks until real audio returns, which the voice-activity
/// logic relies on. With a higher floor the gate only ducks, never silences,
/// so every buffer is kept.
///
/// The parameters are fields on the gate itself, stored in the units the UI
/// shows: levels in dBFS, envelope stages in milliseconds. They are resolved
/// into linear gains and sample counts when the gate is prepared with a
/// concrete format, and again after every parameter edit. Code-level tweaks go
/// through [`Gate::tuned`] so the resolved values follow. Field defaults live
/// next to the fields as consts, so [`Gate::new`] can't drift from what the UI
/// shows.
#[derive(AfxParams)]
#[afx(after_set = recompute)]
pub struct Gate {
    /// Level the signal must rise above for the gate to open.
    #[afx(param(name = "Threshold", unit = ParamUnit::Decibels, min = -90.0, max = 0.0, default = Gate::DEFAULT_THRESHOLD, min_is_off = false))]
    pub threshold_db: f32,
    /// How far below `threshold_db` the signal must fall for an open gate to
    /// re-close (hysteresis). Keeps the gate from chattering on a signal that
    /// hovers right at the threshold.
    #[afx(param(name = "Hysteresis", unit = ParamUnit::Decibels, min = 0.0, max = 24.0, default = Gate::DEFAULT_HYSTERESIS, min_is_off = false))]
    pub hysteresis_db: f32,
    /// Time to ramp from the closed floor up to fully open once triggered.
    #[afx(param(name = "Attack", unit = ParamUnit::Milliseconds, min = 0.0, max = 100.0, default = Gate::DEFAULT_ATTACK_MS, min_is_off = false))]
    pub attack_ms: f32,
    /// Time the gate is held open after the signal last exceeded the
    /// threshold. Bridges short dips — the gaps between words — so speech is
    /// not chopped and a consumer does not park mid-utterance.
    #[afx(param(name = "Hold", unit = ParamUnit::Milliseconds, min = 0.0, max = 1000.0, default = Gate::DEFAULT_HOLD_MS, min_is_off = false))]
    pub hold_ms: f32,
    /// Time to ramp from open back down to the floor after `hold` elapses.
    #[afx(param(name = "Release", unit = ParamUnit::Milliseconds, min = 0.0, max = 1000.0, default = Gate::DEFAULT_RELEASE_MS, min_is_off = false))]
    pub release_ms: f32,
    /// Attenuation applied while closed, in dB below unity. At
    /// [`Gate::FLOOR_OFF`] (the range minimum, rendered "off") the gate
    /// closes fully — true silence; above it, it merely ducks. Note that
    /// park-on-silence — dropping the buffer so a consumer's `pop` blocks —
    /// only happens when the gate closes fully.
    #[afx(param(name = "Floor", unit = ParamUnit::Decibels, min = Gate::FLOOR_OFF, max = 0.0, default = Gate::FLOOR_OFF, min_is_off = true))]
    pub floor_db: f32,

    // The fields below are runtime state, not parameters: un-annotated, so
    // reflection skips them and the UI never sees or writes them.
    //
    // Resolved from parameters. Stream-format data is passed through
    // `apply_to`, so it is not stored here.
    open_thresh: AudioSampleType,
    close_thresh: AudioSampleType,
    floor_gain: AudioSampleType,
    // Envelope state, carried across buffers for a continuous stream. The
    // attack/release glide is a fundsp smoother (its state survives live
    // `set_time` retunes); the open/hold decision is the gate's own.
    glide: An<AFollow<f32>>,
    open: bool,
    hold_counter: u32,
    sample_rate: u32,
    hold_frames: u32,
}

/// Output below this magnitude counts as silence when deciding whether a
/// fully-closed buffer can be dropped.
const SILENCE_EPS: AudioSampleType = 1e-6;

impl Gate {
    pub const NAME: &str = "Gate";
    // Voice-tuned defaults: open near a typical mic noise floor, hold across
    // the gaps between words, fade out, then close fully so sustained
    // silence parks the consumer.
    pub const DEFAULT_THRESHOLD: f32 = -50.0;
    pub const DEFAULT_HYSTERESIS: f32 = 6.0;
    pub const DEFAULT_ATTACK_MS: f32 = 5.0;
    // Hold + release is how long transmission continues after speech stops
    // (the gate only parks the consumer once fully closed). Keep the sum
    // short, while still bridging ordinary inter-word gaps.
    pub const DEFAULT_HOLD_MS: f32 = 150.0;
    pub const DEFAULT_RELEASE_MS: f32 = 100.0;
    /// Floor value that stands for −∞: at (or below) it the gate closes
    /// fully, which is what enables the park-on-silence buffer drop.
    pub const FLOOR_OFF: f32 = -80.0;

    /// A gate on the voice-tuned defaults.
    pub fn new() -> Self {
        let mut gate = Gate {
            threshold_db: Self::DEFAULT_THRESHOLD,
            hysteresis_db: Self::DEFAULT_HYSTERESIS,
            attack_ms: Self::DEFAULT_ATTACK_MS,
            hold_ms: Self::DEFAULT_HOLD_MS,
            release_ms: Self::DEFAULT_RELEASE_MS,
            floor_db: Self::FLOOR_OFF,
            open_thresh: 0.0,
            close_thresh: 0.0,
            floor_gain: 0.0,
            glide: afollow(
                Self::DEFAULT_ATTACK_MS / 1000.0,
                Self::DEFAULT_RELEASE_MS / 1000.0,
            ),
            open: false,
            hold_counter: 0,
            sample_rate: 0,
            hold_frames: 0,
        };
        gate.recompute();
        gate.glide.filter_mono(gate.floor_gain);
        gate
    }

    /// Apply code-level parameter tweaks and re-resolve the derived state,
    /// e.g. `Gate::new().tuned(|gate| gate.threshold_db = -54.0)`. UI writes
    /// go through the generated parameter setter, which re-resolves on its own.
    pub fn tuned(mut self, tweak: impl FnOnce(&mut Self)) -> Self {
        tweak(&mut self);
        self.recompute();
        self
    }

    /// Resolve parameters into runtime values that do not depend on the
    /// stream format.
    fn recompute(&mut self) {
        self.open_thresh = db_amp(self.threshold_db);
        self.close_thresh = db_amp(self.threshold_db - self.hysteresis_db);
        // The slider floor stands for −∞: a full close (and its
        // park-on-silence buffer drop) rather than an −80dB duck.
        self.floor_gain = if self.floor_db <= Self::FLOOR_OFF {
            0.0
        } else {
            db_amp(self.floor_db).min(1.0)
        };
        self.glide
            .set_time(self.attack_ms / 1000.0, self.release_ms / 1000.0);
        self.hold_frames =
            (self.hold_ms / 1000.0 * self.sample_rate.max(1) as AudioSampleType) as u32;
    }

    fn adopt_sample_rate(&mut self, sample_rate: u32) {
        if self.sample_rate != sample_rate {
            self.sample_rate = sample_rate;
            self.hold_frames =
                (self.hold_ms / 1000.0 * sample_rate.max(1) as AudioSampleType) as u32;
            self.glide.set_sample_rate(sample_rate.max(1) as f64);
        }
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Afx for Gate {
    fn name(&self) -> &'static str {
        Gate::NAME
    }

    /// Adopt a stream format: re-derive the per-sample values and reset the
    /// envelope to closed (a new stream starts from silence).
    fn prepare(&mut self, format: StreamFormat) {
        self.glide.reset();
        self.sample_rate = format.sample_rate;
        self.glide.set_sample_rate(format.sample_rate.max(1) as f64);
        self.recompute();
        // A freshly reset smoother jumps straight to its first input; feed it
        // the closed floor so the stream starts silent with smoothing armed.
        self.glide.filter_mono(self.floor_gain);
        self.open = false;
        self.hold_counter = 0;
    }

    fn apply_to(&mut self, format: StreamFormat, audio: &mut [AudioSampleType]) -> bool {
        if audio.is_empty() {
            return true;
        }
        let channels = (format.channels as usize).max(1);
        self.adopt_sample_rate(format.sample_rate);
        let hold_frames = self.hold_frames;
        let floor = self.floor_gain;
        let mut produced_signal = false;

        for frame in audio.chunks_mut(channels) {
            // Detector: peak magnitude across the frame's channels.
            let level: AudioSampleType = frame.iter().fold(0.0, |peak, &s| peak.max(s.abs()));

            // Open/close decision with hold and hysteresis.
            if level > self.open_thresh {
                self.open = true;
                self.hold_counter = hold_frames;
            } else if self.open {
                if self.hold_counter > 0 {
                    self.hold_counter -= 1;
                } else if level < self.close_thresh {
                    self.open = false;
                }
            }

            // Glide the gain toward the target; the smoother applies its
            // attack time rising and its release time falling. A zero-length
            // stage jumps instead: the smoother's fastest step is one sample
            // per pole, which would leave a tail where none was asked for.
            let target = if self.open { 1.0 } else { floor };
            let instant = if target > self.glide.value() {
                self.attack_ms <= 0.0
            } else {
                self.release_ms <= 0.0
            };
            if instant {
                self.glide.set_value(target);
            }
            let gain = self.glide.filter_mono(target);

            if gain > floor + SILENCE_EPS {
                produced_signal = true;
            }
            for sample in frame.iter_mut() {
                *sample *= gain;
            }
        }

        // A raised floor always emits (it ducks, never silences). A full-close
        // floor drops only once the whole buffer has settled to silence, so the
        // release tail is preserved and sustained silence parks the consumer.
        produced_signal || floor > SILENCE_EPS
    }
}

#[cfg(test)]
#[path = "../../tests/unit/effects/gate.rs"]
mod tests;
