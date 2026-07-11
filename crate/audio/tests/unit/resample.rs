use super::*;

fn slice_source(
    data: &[AudioSampleType],
    ch: usize,
) -> impl FnMut(&mut [AudioSampleType]) -> bool + '_ {
    let mut pos = 0;
    move |frame: &mut [AudioSampleType]| {
        if pos + ch > data.len() {
            return false;
        }
        frame.copy_from_slice(&data[pos..pos + ch]);
        pos += ch;
        true
    }
}

fn sine(rate: u32, len: usize, freq: f32) -> Vec<AudioSampleType> {
    (0..len)
        .map(|i| (core::f32::consts::TAU * freq * i as f32 / rate as f32).sin())
        .collect()
}

#[test]
fn identity_passthrough() {
    let mut conv = Resampler::new(2, 48_000, 2, 48_000);
    let src = [0.1, 0.2, 0.3, 0.4];
    let mut dst = [0.0; 8];
    let n = conv.pull(slice_source(&src, 2), &mut dst);
    assert_eq!(n, 4);
    assert_eq!(&dst[..4], &src);
}

#[test]
fn mono_to_stereo_duplicates() {
    let mut conv = Resampler::new(1, 48_000, 2, 48_000);
    let src = [0.5, -0.5];
    let mut dst = [0.0; 4];
    let n = conv.pull(slice_source(&src, 1), &mut dst);
    assert_eq!(n, 4);
    assert_eq!(dst, [0.5, 0.5, -0.5, -0.5]);
}

#[test]
fn stereo_to_mono_averages() {
    let mut conv = Resampler::new(2, 48_000, 1, 48_000);
    let src = [0.2, 0.4, -1.0, 1.0];
    let mut dst = [0.0; 2];
    let n = conv.pull(slice_source(&src, 2), &mut dst);
    assert_eq!(n, 2);
    assert!((dst[0] - 0.3).abs() < 1e-6);
    assert!(dst[1].abs() < 1e-6);
}

/// One second of a 1kHz sine must come out the other side with the right
/// length (duration), the right zero-crossing count (frequency, i.e. no pitch
/// shift) and the right RMS (amplitude, i.e. a flat passband).
fn assert_sine_survives(src_rate: u32, dst_rate: u32) {
    let mut conv = Resampler::new(1, src_rate, 1, dst_rate);
    let src = sine(src_rate, src_rate as usize, 1_000.0);
    let mut dst = vec![0.0 as AudioSampleType; dst_rate as usize + 8_000];
    let n = conv.pull(slice_source(&src, 1), &mut dst);

    // One second in ~ one second out; input is consumed in fixed chunks, so
    // allow up to one chunk (plus its resampled size) of slack.
    assert!((n as i64 - dst_rate as i64).unsigned_abs() < 600, "{n}");

    // Judge the steady state only: skip the filter's startup transient and
    // the last partial-chunk remainder.
    let region = &dst[4_800..n - 4_800];

    let rms = (region.iter().map(|s| (s * s) as f64).sum::<f64>() / region.len() as f64).sqrt();
    assert!(
        (rms - core::f64::consts::FRAC_1_SQRT_2).abs() < 0.01,
        "{rms}"
    );

    let crossings = region
        .windows(2)
        .filter(|pair| (pair[0] >= 0.0) != (pair[1] >= 0.0))
        .count();
    let expected = 2.0 * 1_000.0 * region.len() as f64 / dst_rate as f64;
    assert!(
        (crossings as f64 - expected).abs() <= 4.0,
        "{crossings} crossings, expected ~{expected}"
    );
}

#[test]
fn upsample_preserves_a_sine() {
    assert_sine_survives(44_100, 48_000);
}

#[test]
fn downsample_preserves_a_sine() {
    assert_sine_survives(48_000, 44_100);
}

/// Pulling one stream in odd-sized pieces must produce exactly the samples of
/// pulling it in one go — staged chunks and buffered output carry across
/// `pull` calls.
#[test]
fn state_persists_across_pulls() {
    let src = sine(44_100, 8_820, 440.0);

    let mut whole_conv = Resampler::new(1, 44_100, 1, 48_000);
    let mut whole = vec![0.0 as AudioSampleType; 12_000];
    let n_whole = whole_conv.pull(slice_source(&src, 1), &mut whole);
    assert!(n_whole > 0);

    let mut pieces_conv = Resampler::new(1, 44_100, 1, 48_000);
    let mut source = slice_source(&src, 1);
    let mut pieces = vec![0.0 as AudioSampleType; 12_000];
    let mut n_pieces = 0;
    loop {
        let end = (n_pieces + 37).min(pieces.len());
        let wrote = pieces_conv.pull(&mut source, &mut pieces[n_pieces..end]);
        if wrote == 0 {
            break;
        }
        n_pieces += wrote;
    }

    assert_eq!(n_pieces, n_whole);
    assert_eq!(&pieces[..n_pieces], &whole[..n_whole]);
}

#[test]
fn partial_dst_returns_whole_frames_only() {
    let mut conv = Resampler::new(2, 48_000, 2, 48_000);
    let src = [0.1, 0.2, 0.3, 0.4];
    let mut dst = [0.0; 3];
    let n = conv.pull(slice_source(&src, 2), &mut dst);
    assert_eq!(n, 2);
    assert_eq!(&dst[..2], &[0.1, 0.2]);
}
