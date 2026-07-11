use super::*;

fn stereo_48k() -> StreamFormat {
    StreamFormat {
        channels: 2,
        sample_format: cpal::SampleFormat::F32,
        sample_rate: 48_000,
    }
}

#[test]
fn gain_scales_and_keeps() {
    let mut audio = [0.1, -0.2, 0.3, -0.4];
    let kept = Gain::with_multiplier(2.0).apply_to(stereo_48k(), &mut audio);
    assert!(kept);
    assert_eq!(audio, [0.2, -0.4, 0.6, -0.8]);
}

#[test]
fn unity_is_untouched() {
    let mut gain = Gain::default();
    let mut audio = [0.5, -0.5];
    gain.apply_to(stereo_48k(), &mut audio);
    assert_eq!(audio, [0.5, -0.5]);
}

#[test]
fn change_glides_instead_of_snapping() {
    let mut gain = Gain::default();
    let format = StreamFormat {
        channels: 1,
        sample_format: cpal::SampleFormat::F32,
        sample_rate: 48_000,
    };
    gain.desired_multiplier = 0.0;
    let mut audio = [1.0; 480];
    gain.apply_to(format, &mut audio);
    // Mid-glide: quieter than before, but not yet silent at the start.
    assert!(audio[0] < 1.0 && audio[0] > audio[479]);
    assert!(audio[479] > 0.0);

    for _ in 0..20 {
        let mut next = [1.0; 480];
        gain.apply_to(format, &mut next);
    }
    let mut settled = [1.0; 4];
    gain.apply_to(format, &mut settled);
    assert_eq!(settled, [0.0; 4]);
}
