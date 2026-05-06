use crate::AudioSampleType;

// TODO: Wire up effects pipeline into audio stream processing.

pub trait Afx {
    fn apply_to(&mut self, audio: &mut [AudioSampleType]);
}

#[allow(dead_code)]
struct Gain {
    multiplier: f32,
}
impl Afx for Gain {
    fn apply_to(&mut self, audio: &mut [AudioSampleType]) {
        for sample in audio.iter_mut() {
            *sample *= self.multiplier;
        }
    }
}
