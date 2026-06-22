use std::time::Duration;

use crate::{AudioSampleType, StreamFormat};

use super::Afx;

/// Tunable parameters for a [`Gate`]. Levels are dBFS; the envelope stages are
/// wall-clock durations, resolved into sample counts once the gate is
/// [`prepare`](Afx::prepare)d with a concrete format.
#[derive(Clone, Copy, Debug)]
pub struct GateSettings {
    /// Level the signal must rise above for the gate to open.
    pub threshold_db: AudioSampleType,
    /// How far below `threshold_db` the signal must fall for an open gate to
    /// re-close (hysteresis). Keeps the gate from chattering on a signal that
    /// hovers right at the threshold.
    pub hysteresis_db: AudioSampleType,
    /// Time to ramp from the closed floor up to fully open once triggered.
    pub attack: Duration,
    /// Time the gate is held open after the signal last exceeded the
    /// threshold. Bridges short dips — the gaps between words — so speech is
    /// not chopped and a consumer does not park mid-utterance.
    pub hold: Duration,
    /// Time to ramp from open back down to the floor after `hold` elapses.
    pub release: Duration,
    /// Attenuation applied while closed, in dB below unity. [`f32::NEG_INFINITY`]
    /// closes fully (true silence); a finite value (e.g. `-40.0`) merely ducks.
    /// Note that park-on-silence — dropping the buffer so a consumer's `pop`
    /// blocks — only happens when the gate closes fully.
    pub range_db: AudioSampleType,
}

impl Default for GateSettings {
    fn default() -> Self {
        // Voice-tuned: open near a typical mic noise floor, hold across the
        // gaps between words, fade out, then close fully so sustained silence
        // parks the consumer.
        GateSettings {
            threshold_db: -50.0,
            hysteresis_db: 6.0,
            attack: Duration::from_millis(5),
            hold: Duration::from_millis(200),
            release: Duration::from_millis(150),
            range_db: AudioSampleType::NEG_INFINITY,
        }
    }
}

/// A noise gate with attack / hold / release envelope shaping.
///
/// While the signal sits above [`GateSettings::threshold_db`] the gate opens to
/// unity gain; when it falls away the gate holds, then releases down to the
/// [`range`](GateSettings::range_db) floor. The detector is the per-frame peak
/// across channels, so the envelope tracks the loudest channel.
///
/// When the floor is full silence (`range_db` = -∞) and an entire buffer lands
/// on it, `apply_to` *drops* the buffer rather than forwarding silence — so a
/// gated input channel falls quiet and its consumer's `pop` parks until real
/// audio returns, which the voice-activity logic relies on. With a finite floor
/// the gate only ducks, never silences, so every buffer is kept.
pub struct Gate {
    settings: GateSettings,
    // Resolved against the stream format in `configure` (seeded in `new`).
    channels: usize,
    open_thresh: AudioSampleType,
    close_thresh: AudioSampleType,
    floor_gain: AudioSampleType,
    attack_coef: AudioSampleType,
    release_coef: AudioSampleType,
    hold_frames: u32,
    // Envelope state, carried across buffers for a continuous stream.
    gain: AudioSampleType,
    open: bool,
    hold_counter: u32,
}

/// Output below this magnitude counts as silence when deciding whether a
/// fully-closed buffer can be dropped.
const SILENCE_EPS: AudioSampleType = 1e-6;

impl Gate {
    pub fn new(settings: GateSettings) -> Self {
        let mut gate = Gate {
            settings,
            channels: 1,
            open_thresh: 0.0,
            close_thresh: 0.0,
            floor_gain: 0.0,
            attack_coef: 1.0,
            release_coef: 1.0,
            hold_frames: 0,
            gain: 0.0,
            open: false,
            hold_counter: 0,
        };
        // Seed against a default format so the gate is usable even if it is
        // never `prepare`d; the real format overwrites this.
        gate.configure(48_000.0, 2);
        gate
    }

    /// Resolve the dB/duration settings into the linear gains, smoothing
    /// coefficients and frame counts used per sample, then reset to closed.
    fn configure(&mut self, sample_rate: AudioSampleType, channels: usize) {
        self.channels = channels.max(1);
        self.open_thresh = db_to_linear(self.settings.threshold_db);
        self.close_thresh =
            db_to_linear(self.settings.threshold_db - self.settings.hysteresis_db);
        self.floor_gain = db_to_linear(self.settings.range_db).min(1.0);
        self.attack_coef = time_to_coef(self.settings.attack, sample_rate);
        self.release_coef = time_to_coef(self.settings.release, sample_rate);
        self.hold_frames = (self.settings.hold.as_secs_f32() * sample_rate) as u32;
        self.gain = self.floor_gain;
        self.open = false;
        self.hold_counter = 0;
    }
}

impl Afx for Gate {
    fn prepare(&mut self, format: StreamFormat) {
        self.configure(format.sample_rate as AudioSampleType, format.channels as usize);
    }

    fn apply_to(&mut self, audio: &mut [AudioSampleType]) -> bool {
        if audio.is_empty() {
            return true;
        }
        let floor = self.floor_gain;
        let mut produced_signal = false;

        for frame in audio.chunks_mut(self.channels) {
            // Detector: peak magnitude across the frame's channels.
            let level: AudioSampleType = frame.iter().fold(0.0, |peak, &s| peak.max(s.abs()));

            // Open/close decision with hold and hysteresis.
            if level > self.open_thresh {
                self.open = true;
                self.hold_counter = self.hold_frames;
            } else if self.open {
                if self.hold_counter > 0 {
                    self.hold_counter -= 1;
                } else if level < self.close_thresh {
                    self.open = false;
                }
            }

            // Glide the gain toward the target; attack rising, release falling.
            let target = if self.open { 1.0 } else { floor };
            let coef = if target > self.gain {
                self.attack_coef
            } else {
                self.release_coef
            };
            self.gain += (target - self.gain) * coef;

            if self.gain > floor + SILENCE_EPS {
                produced_signal = true;
            }
            for sample in frame.iter_mut() {
                *sample *= self.gain;
            }
        }

        // A finite floor always emits (it ducks, never silences). A full-close
        // floor drops only once the whole buffer has settled to silence, so the
        // release tail is preserved and sustained silence parks the consumer.
        produced_signal || floor > SILENCE_EPS
    }
}

fn db_to_linear(db: AudioSampleType) -> AudioSampleType {
    if db == AudioSampleType::NEG_INFINITY {
        0.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

/// One-pole smoothing coefficient that reaches ~63% of a step over `time`.
/// A stage of one frame or less is treated as instantaneous.
fn time_to_coef(time: Duration, sample_rate: AudioSampleType) -> AudioSampleType {
    let frames = time.as_secs_f32() * sample_rate;
    if frames <= 1.0 {
        1.0
    } else {
        1.0 - (-1.0 / frames).exp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(overrides: GateSettings) -> Gate {
        let mut gate = Gate::new(overrides);
        gate.configure(48_000.0, 1);
        gate
    }

    fn voice() -> GateSettings {
        GateSettings {
            threshold_db: -40.0,
            ..Default::default()
        }
    }

    #[test]
    fn opens_and_ramps_up_over_attack() {
        let mut gate = gate(GateSettings {
            attack: Duration::from_millis(5),
            ..voice()
        });
        let mut buf = [0.5; 480];
        assert!(gate.apply_to(&mut buf));
        // Attack glides the gain up across the buffer rather than snapping.
        assert!(buf[0].abs() < buf[479].abs());
        assert!(buf[479].abs() > 0.0);
    }

    #[test]
    fn holds_through_a_short_gap() {
        let mut gate = gate(GateSettings {
            attack: Duration::ZERO,
            hold: Duration::from_millis(100),
            ..voice()
        });
        let mut loud = [0.5; 8];
        assert!(gate.apply_to(&mut loud));
        // A 10ms gap sits well inside the 100ms hold: kept, not dropped.
        let mut gap = [0.0; 480];
        assert!(gate.apply_to(&mut gap));
    }

    #[test]
    fn drops_buffer_once_fully_closed() {
        let mut gate = gate(GateSettings {
            attack: Duration::ZERO,
            hold: Duration::ZERO,
            release: Duration::ZERO,
            ..voice()
        });
        let mut loud = [0.5; 8];
        assert!(gate.apply_to(&mut loud));
        // No hold or release, full-close floor: the next silent buffer is
        // dropped so the consumer parks.
        let mut quiet = [0.0; 8];
        assert!(!gate.apply_to(&mut quiet));
    }

    #[test]
    fn finite_range_ducks_and_never_drops() {
        let mut gate = gate(GateSettings {
            range_db: -20.0,
            ..voice()
        });
        // -20dB floor: silence is attenuated but still emitted, so it is kept.
        let mut quiet = [0.0; 8];
        assert!(gate.apply_to(&mut quiet));
    }

    #[test]
    fn keeps_empty_buffer() {
        assert!(gate(voice()).apply_to(&mut []));
    }
}
