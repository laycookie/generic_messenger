use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use simple_video_capture::bench_internals::{
    bgr_to_rgb_scalar, bgr_to_rgb_simd, packed_4_to_rgb_scalar, packed_4_to_rgb_simd,
};

const DIMENSIONS: [(usize, usize); 3] = [(640, 360), (1_280, 720), (1_920, 1_080)];

fn pixels(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| index.wrapping_mul(131).wrapping_add(17) as u8)
        .collect()
}

fn packed_4(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("capture normalization/packed 4 to RGB");
    for (width, height) in DIMENSIONS {
        let source = pixels(width * height * 4);
        group.throughput(Throughput::Bytes(source.len() as u64));
        for (layout_name, channels) in [
            ("RGBx", [0, 1, 2]),
            ("BGRA", [2, 1, 0]),
            ("XBGR", [3, 2, 1]),
        ] {
            for (name, kernel) in [
                (
                    "scalar",
                    packed_4_to_rgb_scalar as fn(&[u8], &mut Vec<u8>, [usize; 3]),
                ),
                (
                    "simd",
                    packed_4_to_rgb_simd as fn(&[u8], &mut Vec<u8>, [usize; 3]),
                ),
            ] {
                let mut output = Vec::with_capacity(width * height * 3);
                group.bench_with_input(
                    BenchmarkId::new(format!("{layout_name}/{name}"), format!("{width}x{height}")),
                    &source,
                    |bencher, source| {
                        bencher.iter(|| {
                            kernel(black_box(source), black_box(&mut output), channels);
                            black_box(&output);
                        });
                    },
                );
            }
        }
    }
    group.finish();
}

fn bgr(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("capture normalization/BGR24 to RGB24");
    for (width, height) in DIMENSIONS {
        let source = pixels(width * height * 3);
        group.throughput(Throughput::Bytes(source.len() as u64));
        for (name, kernel) in [
            ("scalar", bgr_to_rgb_scalar as fn(&mut [u8])),
            ("simd", bgr_to_rgb_simd as fn(&mut [u8])),
        ] {
            group.bench_with_input(
                BenchmarkId::new(name, format!("{width}x{height}")),
                &source,
                |bencher, source| {
                    bencher.iter_batched_ref(
                        || source.clone(),
                        |pixels| kernel(black_box(pixels)),
                        BatchSize::LargeInput,
                    );
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, packed_4, bgr);
criterion_main!(benches);
