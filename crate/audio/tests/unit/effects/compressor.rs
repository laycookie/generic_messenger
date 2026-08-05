use super::*;
use crate::SampleFormat;

fn format(channels: u16, sample_rate: u32) -> StreamFormat {
    StreamFormat {
        channels,
        sample_format: SampleFormat::F32,
        sample_rate,
    }
}

fn instant_compressor() -> Compressor {
    Compressor::new().tuned(|compressor| {
        compressor.threshold_db = -20.0;
        compressor.ratio = 4.0;
        compressor.attack_ms = 0.0;
        compressor.release_ms = 0.0;
    })
}

#[test]
fn exposes_compressor_parameters() {
    let compressor = Compressor::new();
    let specs = compressor.param_specs();

    assert_eq!(specs.len(), 5);
    assert_eq!(specs[0].key, Compressor::THRESHOLD_DB);
    assert_eq!(specs[1].key, Compressor::RATIO);
    assert_eq!(specs[1].unit, ParamUnit::Ratio);
    assert_eq!(specs[4].key, Compressor::MAKEUP_DB);
}

#[test]
fn applies_ratio_above_threshold() {
    let mut compressor = instant_compressor();
    let mut audio = [1.0];

    assert!(compressor.apply_to(format(1, 48_000), &mut audio));

    // 0dB input is 20dB above threshold. At 4:1, output is -15dB.
    let expected = db_amp(-15.0);
    assert!((audio[0] - expected).abs() < 1e-6, "{}", audio[0]);
}

#[test]
fn leaves_signal_below_threshold_unchanged() {
    let mut compressor = instant_compressor();
    let mut audio = [0.05, -0.05];

    compressor.apply_to(format(1, 48_000), &mut audio);

    assert_eq!(audio, [0.05, -0.05]);
}

#[test]
fn ratio_one_disables_gain_reduction() {
    let mut compressor = instant_compressor().tuned(|compressor| compressor.ratio = 1.0);
    let mut audio = [1.0, -1.0];

    compressor.apply_to(format(1, 48_000), &mut audio);

    assert_eq!(audio, [1.0, -1.0]);
}

#[test]
fn detector_is_linked_across_channels() {
    let mut compressor = instant_compressor();
    let mut audio = [1.0, 0.25];

    compressor.apply_to(format(2, 48_000), &mut audio);

    let left_gain = audio[0] / 1.0;
    let right_gain = audio[1] / 0.25;
    assert!((left_gain - right_gain).abs() < 1e-6);
}

#[test]
fn attack_and_release_smooth_gain_reduction() {
    let mut compressor = Compressor::new().tuned(|compressor| {
        compressor.threshold_db = -20.0;
        compressor.ratio = 20.0;
        compressor.attack_ms = 10.0;
        compressor.release_ms = 100.0;
    });
    let stream_format = format(1, 48_000);
    let mut loud = [1.0; 480];

    compressor.apply_to(stream_format, &mut loud);

    assert!(loud[0] > loud[479]);
    assert!(loud[479] < 1.0);

    let reduction_after_loud = compressor.gain_reduction_db;
    let mut quiet = [0.01; 480];
    compressor.apply_to(stream_format, &mut quiet);
    assert!(compressor.gain_reduction_db < reduction_after_loud);
    assert!(compressor.gain_reduction_db > 0.0);
}

#[test]
fn makeup_gain_applies_below_threshold() {
    let mut compressor = instant_compressor().tuned(|compressor| compressor.makeup_db = 6.0);
    let mut audio = [0.01];

    compressor.apply_to(format(1, 48_000), &mut audio);

    assert!((audio[0] - 0.01 * db_amp(6.0)).abs() < 1e-6);
}

#[test]
fn prepare_resets_envelope() {
    let mut compressor = instant_compressor();
    let stream_format = format(1, 48_000);
    let mut loud = [1.0];
    compressor.apply_to(stream_format, &mut loud);
    assert!(compressor.gain_reduction_db > 0.0);

    compressor.prepare(stream_format);

    assert_eq!(compressor.gain_reduction_db, 0.0);
}

#[test]
fn direct_parameter_edits_refresh_cached_coefficients() {
    let stream_format = format(1, 48_000);
    let mut compressor = instant_compressor();
    compressor.prepare(stream_format);
    compressor.threshold_db = -6.0;
    let mut audio = [0.25];

    compressor.apply_to(stream_format, &mut audio);

    assert_eq!(audio, [0.25]);
}
