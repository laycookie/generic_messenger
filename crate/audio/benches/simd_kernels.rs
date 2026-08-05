use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use simple_audio_channels::bench_internals::{
    add_assign_scalar, add_assign_simd, all_magnitude_le_scalar, all_magnitude_le_simd,
    max_abs_scalar, max_abs_simd, multiply_scalar, multiply_simd,
};

const FRAMES: [usize; 5] = [128, 256, 512, 1_024, 2_048];
const CHANNEL_COUNTS: [usize; 2] = [1, 2];
const MIXED_CHANNELS: [usize; 4] = [1, 2, 4, 8];

fn input(len: usize, seed: usize) -> Vec<f32> {
    (0..len)
        .map(|index| {
            let value = index
                .wrapping_mul(1_103_515_245)
                .wrapping_add(seed.wrapping_mul(12_345));
            (value % 2_001) as f32 / 1_000.0 - 1.0
        })
        .collect()
}

fn sample_kernels(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("audio SIMD/sample kernels");
    for channels in CHANNEL_COUNTS {
        for frames in FRAMES {
            let len = frames * channels;
            let samples = input(len, channels);
            group.throughput(Throughput::Elements(len as u64));

            for (name, kernel) in [
                ("scalar", multiply_scalar as fn(&mut [f32], f32)),
                ("simd", multiply_simd as fn(&mut [f32], f32)),
            ] {
                group.bench_with_input(
                    BenchmarkId::new(format!("multiply/{name}/{channels}ch"), frames),
                    &samples,
                    |bencher, samples| {
                        bencher.iter_batched_ref(
                            || samples.clone(),
                            |samples| kernel(black_box(samples), black_box(0.375)),
                            BatchSize::SmallInput,
                        );
                    },
                );
            }

            for (name, kernel) in [
                ("scalar", max_abs_scalar as fn(&[f32]) -> f32),
                ("simd", max_abs_simd as fn(&[f32]) -> f32),
            ] {
                group.bench_with_input(
                    BenchmarkId::new(format!("peak/{name}/{channels}ch"), frames),
                    &samples,
                    |bencher, samples| bencher.iter(|| black_box(kernel(black_box(samples)))),
                );
            }

            let quiet = input(len, channels)
                .into_iter()
                .map(|sample| sample * 0.01)
                .collect::<Vec<_>>();
            for (name, kernel) in [
                ("scalar", all_magnitude_le_scalar as fn(&[f32], f32) -> bool),
                ("simd", all_magnitude_le_simd as fn(&[f32], f32) -> bool),
            ] {
                group.bench_with_input(
                    BenchmarkId::new(format!("quiet scan/{name}/{channels}ch"), frames),
                    &quiet,
                    |bencher, samples| {
                        bencher.iter(|| black_box(kernel(black_box(samples), black_box(0.02))))
                    },
                );
            }

            let mut active = quiet.clone();
            active[0] = 1.0;
            for (name, kernel) in [
                ("scalar", all_magnitude_le_scalar as fn(&[f32], f32) -> bool),
                ("simd", all_magnitude_le_simd as fn(&[f32], f32) -> bool),
            ] {
                group.bench_with_input(
                    BenchmarkId::new(format!("active scan/{name}/{channels}ch"), frames),
                    &active,
                    |bencher, samples| {
                        bencher.iter(|| black_box(kernel(black_box(samples), black_box(0.02))))
                    },
                );
            }
        }
    }
    group.finish();
}

fn mixing(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("audio SIMD/mixing");
    for channels in CHANNEL_COUNTS {
        for frames in FRAMES {
            let len = frames * channels;
            for mixed_channels in MIXED_CHANNELS {
                let inputs = (0..mixed_channels)
                    .map(|channel| input(len, channel + channels))
                    .collect::<Vec<_>>();
                group.throughput(Throughput::Elements((len * mixed_channels) as u64));
                for (name, kernel) in [
                    ("scalar", add_assign_scalar as fn(&mut [f32], &[f32])),
                    ("simd", add_assign_simd as fn(&mut [f32], &[f32])),
                ] {
                    group.bench_with_input(
                        BenchmarkId::new(
                            format!("{name}/{channels}ch/{mixed_channels} inputs"),
                            frames,
                        ),
                        &inputs,
                        |bencher, inputs| {
                            bencher.iter_batched_ref(
                                || vec![0.0; len],
                                |mix| {
                                    for input in inputs {
                                        kernel(
                                            black_box(mix.as_mut_slice()),
                                            black_box(input.as_slice()),
                                        );
                                    }
                                    black_box(mix.as_slice());
                                },
                                BatchSize::SmallInput,
                            );
                        },
                    );
                }
            }
        }
    }
    group.finish();
}

criterion_group!(benches, sample_kernels, mixing);
criterion_main!(benches);
