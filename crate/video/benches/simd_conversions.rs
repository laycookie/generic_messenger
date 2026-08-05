use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use simple_video::bench_internals::{
    rgb_to_rgba_scalar, rgb_to_rgba_simd, rgb_to_yuv420_scalar, rgb_to_yuv420_simd,
};

const DIMENSIONS: [(usize, usize); 4] = [(640, 360), (1_280, 720), (1_920, 1_080), (641, 361)];

type YuvKernel = fn(&[u8], usize, usize, usize, &mut [u8], &mut [u8], &mut [u8]);

fn pixels(width: usize, height: usize, bytes_per_pixel: usize) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(width * height * bytes_per_pixel);
    for index in 0..width * height {
        let red = (index.wrapping_mul(17) % 256) as u8;
        let green = (index.wrapping_mul(67).wrapping_add(31) % 256) as u8;
        let blue = (index.wrapping_mul(131).wrapping_add(7) % 256) as u8;
        pixels.extend_from_slice(&[red, green, blue, 255][..bytes_per_pixel]);
    }
    pixels
}

fn yuv420(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("video SIMD/RGB to YUV420");
    for (width, height) in DIMENSIONS {
        for bytes_per_pixel in [3, 4] {
            let source = pixels(width, height, bytes_per_pixel);
            let chroma_len = width.div_ceil(2) * height.div_ceil(2);
            group.throughput(Throughput::Bytes(source.len() as u64));
            for (name, kernel) in [
                ("scalar", rgb_to_yuv420_scalar as YuvKernel),
                ("simd", rgb_to_yuv420_simd as YuvKernel),
            ] {
                let mut y = vec![0_u8; width * height];
                let mut u = vec![0_u8; chroma_len];
                let mut v = vec![0_u8; chroma_len];
                group.bench_with_input(
                    BenchmarkId::new(
                        format!("{name}/RGB{bytes_per_pixel}"),
                        format!("{width}x{height}"),
                    ),
                    &source,
                    |bencher, source| {
                        bencher.iter(|| {
                            kernel(
                                black_box(source),
                                width,
                                height,
                                bytes_per_pixel,
                                black_box(&mut y),
                                black_box(&mut u),
                                black_box(&mut v),
                            );
                            black_box((&y, &u, &v));
                        });
                    },
                );
            }
        }
    }
    group.finish();
}

fn rgb_expansion(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("video SIMD/RGB24 to RGBA32");
    for (width, height) in DIMENSIONS {
        let source = pixels(width, height, 3);
        group.throughput(Throughput::Bytes(source.len() as u64));
        for (name, kernel) in [
            ("scalar", rgb_to_rgba_scalar as fn(&[u8], &mut Vec<u8>)),
            ("simd", rgb_to_rgba_simd as fn(&[u8], &mut Vec<u8>)),
        ] {
            let mut output = Vec::with_capacity(width * height * 4);
            group.bench_with_input(
                BenchmarkId::new(name, format!("{width}x{height}")),
                &source,
                |bencher, source| {
                    bencher.iter(|| {
                        kernel(black_box(source), black_box(&mut output));
                        black_box(&output);
                    });
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, yuv420, rgb_expansion);
criterion_main!(benches);
