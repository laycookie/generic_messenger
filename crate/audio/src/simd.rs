//! Portable SIMD kernels for independent contiguous samples.

use std::simd::{cmp::SimdPartialOrd, f32x8, num::SimdFloat};

const LANES: usize = 8;

#[inline]
pub(crate) fn add_assign(output: &mut [f32], input: &[f32]) {
    debug_assert_eq!(output.len(), input.len());
    let vector_len = output.len().min(input.len()) / LANES * LANES;
    for (output, input) in output[..vector_len]
        .chunks_exact_mut(LANES)
        .zip(input[..vector_len].chunks_exact(LANES))
    {
        (f32x8::from_slice(output) + f32x8::from_slice(input)).copy_to_slice(output);
    }
    for (output, input) in output[vector_len..].iter_mut().zip(&input[vector_len..]) {
        *output += input;
    }
}

#[inline]
#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn max_abs(samples: &[f32]) -> f32 {
    let vector_len = samples.len() / LANES * LANES;
    let mut vector_peak = f32x8::splat(0.0);
    for samples in samples[..vector_len].chunks_exact(LANES) {
        vector_peak = vector_peak.simd_max(f32x8::from_slice(samples).abs());
    }
    samples[vector_len..]
        .iter()
        .fold(vector_peak.reduce_max(), |peak, sample| {
            peak.max(sample.abs())
        })
}

#[inline]
pub(crate) fn multiply(samples: &mut [f32], multiplier: f32) {
    let vector_len = samples.len() / LANES * LANES;
    let vector_multiplier = f32x8::splat(multiplier);
    for samples in samples[..vector_len].chunks_exact_mut(LANES) {
        (f32x8::from_slice(samples) * vector_multiplier).copy_to_slice(samples);
    }
    for sample in &mut samples[vector_len..] {
        *sample *= multiplier;
    }
}

#[inline]
pub(crate) fn all_magnitude_le(samples: &[f32], limit: f32) -> bool {
    let Some((first, samples)) = samples.split_first() else {
        return true;
    };
    if first
        .abs()
        .partial_cmp(&limit)
        .is_none_or(std::cmp::Ordering::is_gt)
    {
        return false;
    }
    let vector_len = samples.len() / LANES * LANES;
    let limit_vector = f32x8::splat(limit);
    for samples in samples[..vector_len].chunks_exact(LANES) {
        if !f32x8::from_slice(samples).abs().simd_le(limit_vector).all() {
            return false;
        }
    }
    samples[vector_len..]
        .iter()
        .all(|sample| sample.abs() <= limit)
}

#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn add_assign_scalar(output: &mut [f32], input: &[f32]) {
    debug_assert_eq!(output.len(), input.len());
    for (output, input) in output.iter_mut().zip(input) {
        *output += input;
    }
}

#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn max_abs_scalar(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0, |peak, sample| peak.max(sample.abs()))
}

#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn multiply_scalar(samples: &mut [f32], multiplier: f32) {
    for sample in samples {
        *sample *= multiplier;
    }
}

#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn all_magnitude_le_scalar(samples: &[f32], limit: f32) -> bool {
    samples.iter().all(|sample| sample.abs() <= limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_random(state: &mut u64) -> f32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let unit = (*state >> 40) as f32 / (1_u32 << 24) as f32;
        unit * 4.0 - 2.0
    }

    #[test]
    fn arithmetic_matches_scalar_for_lengths_and_alignments() {
        let mut state = 0x5eed_u64;
        for offset in 0..LANES {
            for len in 0..=257 {
                let input_storage = (0..offset + len + LANES)
                    .map(|_| next_random(&mut state))
                    .collect::<Vec<_>>();
                let input = &input_storage[offset..offset + len];
                let mut expected = (0..len)
                    .map(|_| next_random(&mut state))
                    .collect::<Vec<_>>();
                let mut actual = expected.clone();

                add_assign_scalar(&mut expected, input);
                add_assign(&mut actual, input);
                assert_eq!(actual, expected, "add offset={offset} len={len}");

                multiply_scalar(&mut expected, -0.375);
                multiply(&mut actual, -0.375);
                assert_eq!(actual, expected, "multiply offset={offset} len={len}");
                assert_eq!(max_abs(input), max_abs_scalar(input));
                assert_eq!(
                    all_magnitude_le(input, 0.75),
                    all_magnitude_le_scalar(input, 0.75)
                );
            }
        }
    }

    #[test]
    fn scans_preserve_special_value_behavior() {
        let samples = [
            f32::NAN,
            -0.0,
            f32::from_bits(1),
            -2.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.5,
            -0.25,
            f32::NAN,
        ];

        assert_eq!(max_abs(&samples), max_abs_scalar(&samples));
        assert_eq!(
            all_magnitude_le(&samples, f32::INFINITY),
            all_magnitude_le_scalar(&samples, f32::INFINITY)
        );
        assert!(!all_magnitude_le(&samples, f32::INFINITY));
    }
}
