use super::*;
use crate::SampleFormat;

fn mono_48k() -> StreamFormat {
    StreamFormat {
        channels: 1,
        sample_format: SampleFormat::F32,
        sample_rate: 48_000,
    }
}

fn sine(freq: f32, frames: usize, channels: usize, rate: f32) -> Vec<AudioSampleType> {
    (0..frames * channels)
        .map(|i| {
            let frame = (i / channels) as f32;
            (core::f32::consts::TAU * freq * frame / rate).sin()
        })
        .collect()
}

fn rms(samples: &[AudioSampleType]) -> f64 {
    (samples.iter().map(|s| (s * s) as f64).sum::<f64>() / samples.len() as f64).sqrt()
}

/// Steady-state RMS of `freq` through the filter: run half the signal to let
/// the filter settle, measure the rest.
fn settled_rms(cut: &mut LowCut, freq: f32) -> f64 {
    let mut audio = sine(freq, 9_600, 1, 48_000.0);
    assert!(cut.apply_to(mono_48k(), &mut audio));
    rms(&audio[4_800..])
}

#[test]
fn cuts_rumble_and_passes_voice() {
    let mut cut = LowCut::new();
    cut.prepare(mono_48k());
    // Two octaves below a 200Hz cutoff: a 2nd-order slope leaves ~-24dB.
    assert!(cut.set_param(LowCut::CUTOFF_HZ, 200.0));
    assert!(settled_rms(&mut cut, 50.0) < 0.1);

    let mut cut = LowCut::new();
    cut.prepare(mono_48k());
    cut.set_param(LowCut::CUTOFF_HZ, 200.0);
    // Voice band far above the cutoff passes at full level (unit sine RMS).
    let passed = settled_rms(&mut cut, 2_000.0);
    assert!(
        (passed - core::f64::consts::FRAC_1_SQRT_2).abs() < 0.03,
        "{passed}"
    );
}

/// Channels must be filtered independently: rumble on the left must not bleed
/// filter state into a clean right channel.
#[test]
fn stereo_channels_are_independent() {
    let mut cut = LowCut::new();
    cut.prepare(StreamFormat {
        channels: 2,
        ..mono_48k()
    });
    cut.set_param(LowCut::CUTOFF_HZ, 200.0);

    let frames = 9_600;
    let low = sine(50.0, frames, 1, 48_000.0);
    let high = sine(2_000.0, frames, 1, 48_000.0);
    let mut audio: Vec<AudioSampleType> =
        low.iter().zip(&high).flat_map(|(l, r)| [*l, *r]).collect();
    assert!(cut.apply_to(
        StreamFormat {
            channels: 2,
            ..mono_48k()
        },
        &mut audio
    ));

    let settled = &audio[frames..];
    let left: Vec<_> = settled.iter().step_by(2).copied().collect();
    let right: Vec<_> = settled.iter().skip(1).step_by(2).copied().collect();
    assert!(rms(&left) < 0.1, "{}", rms(&left));
    assert!(
        (rms(&right) - core::f64::consts::FRAC_1_SQRT_2).abs() < 0.03,
        "{}",
        rms(&right)
    );
}
