use fundsp::prelude::db_amp;

use super::*;
use crate::SampleFormat;

fn format(channels: u16, sample_rate: u32) -> StreamFormat {
    StreamFormat {
        channels,
        sample_format: SampleFormat::F32,
        sample_rate,
    }
}

fn instant_normalizer(tweak: impl FnOnce(&mut Normalizer)) -> Normalizer {
    Normalizer::new().tuned(|normalizer| {
        normalizer.attack_ms = 0.0;
        normalizer.release_ms = 0.0;
        normalizer.max_boost_db = 30.0;
        normalizer.noise_floor_db = Normalizer::NOISE_FLOOR_OFF_DB;
        normalizer.ceiling_db = 0.0;
        tweak(normalizer);
    })
}

#[test]
fn exposes_normalizer_parameters() {
    let normalizer = Normalizer::new();
    let specs = normalizer.param_specs();

    assert_eq!(specs.len(), 6);
    assert_eq!(specs[0].key, Normalizer::TARGET_RMS_DB);
    assert_eq!(specs[1].key, Normalizer::MAX_BOOST_DB);
    assert_eq!(specs[2].key, Normalizer::NOISE_FLOOR_DB);
    assert!(specs[2].min_is_off);
    assert_eq!(specs[5].key, Normalizer::CEILING_DB);
}

#[test]
fn raises_quiet_signal_to_target_rms() {
    let mut normalizer = instant_normalizer(|_| {});
    let stream_format = format(1, 48_000);
    normalizer.prepare(stream_format);
    let mut audio = [0.01; 4];

    assert!(normalizer.apply_to(stream_format, &mut audio));

    let target = db_amp(Normalizer::DEFAULT_TARGET_RMS_DB);
    assert!(audio.iter().all(|sample| (*sample - target).abs() < 1e-6));
}

#[test]
fn reduces_loud_signal_to_target_rms() {
    let mut normalizer = instant_normalizer(|_| {});
    let stream_format = format(1, 48_000);
    normalizer.prepare(stream_format);
    let mut audio = [0.5; 4];

    normalizer.apply_to(stream_format, &mut audio);

    let target = db_amp(Normalizer::DEFAULT_TARGET_RMS_DB);
    assert!(audio.iter().all(|sample| (*sample - target).abs() < 1e-6));
}

#[test]
fn detector_links_channels_without_changing_balance() {
    let mut normalizer = instant_normalizer(|_| {});
    let stream_format = format(2, 48_000);
    normalizer.prepare(stream_format);
    let mut audio = [0.5, 0.125];

    normalizer.apply_to(stream_format, &mut audio);

    let left_gain = audio[0] / 0.5;
    let right_gain = audio[1] / 0.125;
    assert!((left_gain - right_gain).abs() < 1e-6);
}

#[test]
fn noise_floor_prevents_boosting_low_level_input() {
    let mut normalizer = instant_normalizer(|normalizer| {
        normalizer.noise_floor_db = -40.0;
    });
    let stream_format = format(1, 48_000);
    normalizer.prepare(stream_format);
    let mut audio = [0.001; 4];

    normalizer.apply_to(stream_format, &mut audio);

    assert!(audio.iter().all(|sample| (*sample - 0.001).abs() < 1e-7));
}

#[test]
fn maximum_boost_bounds_quiet_signal_gain() {
    let mut normalizer = instant_normalizer(|normalizer| {
        normalizer.max_boost_db = 6.0;
    });
    let stream_format = format(1, 48_000);
    normalizer.prepare(stream_format);
    let mut audio = [0.001];

    normalizer.apply_to(stream_format, &mut audio);

    let expected = 0.001 * db_amp(6.0);
    assert!((audio[0] - expected).abs() < 1e-7);
}

#[test]
fn ceiling_catches_peaks_immediately() {
    let mut normalizer = instant_normalizer(|normalizer| {
        normalizer.target_rms_db = -6.0;
        normalizer.ceiling_db = -12.0;
    });
    let stream_format = format(1, 48_000);
    normalizer.prepare(stream_format);
    let mut audio = [1.0];

    normalizer.apply_to(stream_format, &mut audio);

    assert!((audio[0] - db_amp(-12.0)).abs() < 1e-6);
}

#[test]
fn release_smooths_upward_gain_changes() {
    let mut normalizer = Normalizer::new().tuned(|normalizer| {
        normalizer.max_boost_db = 30.0;
        normalizer.noise_floor_db = Normalizer::NOISE_FLOOR_OFF_DB;
        normalizer.release_ms = 100.0;
        normalizer.ceiling_db = 0.0;
    });
    let stream_format = format(1, 1_000);
    normalizer.prepare(stream_format);
    let mut audio = [0.01; 100];

    normalizer.apply_to(stream_format, &mut audio);

    let target = db_amp(Normalizer::DEFAULT_TARGET_RMS_DB);
    assert!(audio[0] > 0.01);
    assert!(audio[0] < audio[99]);
    assert!(audio[99] < target);
}

#[test]
fn prepare_resets_detector_and_gain() {
    let mut normalizer = instant_normalizer(|_| {});
    let stream_format = format(1, 48_000);
    normalizer.prepare(stream_format);
    let mut quiet = [0.01];
    normalizer.apply_to(stream_format, &mut quiet);
    assert!(normalizer.current_gain > 1.0);

    normalizer.prepare(stream_format);

    assert_eq!(normalizer.detector_power, 0.0);
    assert_eq!(normalizer.current_gain, 1.0);
}
