use crate::AudioSampleType;

pub trait Afx {
    fn apply_to(&mut self, audio: &mut [AudioSampleType]);
}

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
