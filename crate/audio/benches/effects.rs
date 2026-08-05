use std::hint::black_box;

use criterion::{
    BatchSize, Criterion, Throughput, criterion_group, criterion_main, measurement::WallTime,
};
use simple_audio_channels::{
    AudioSampleType, SampleFormat, StreamFormat,
    effects::{Afx, Compressor, Gain, Gate, LowCut},
};

const CHANNELS: usize = 2;
const FRAMES: usize = 1_024;

fn stream_format() -> StreamFormat {
    StreamFormat {
        channels: CHANNELS as u16,
        sample_format: SampleFormat::F32,
        sample_rate: 48_000,
    }
}

fn bench_effect<E>(
    group: &mut criterion::BenchmarkGroup<'_, WallTime>,
    name: &str,
    mut effect: E,
    input: &[AudioSampleType],
) where
    E: Afx,
{
    let format = stream_format();
    effect.prepare(format);
    group.bench_function(name, |bencher| {
        bencher.iter_batched_ref(
            || input.to_vec(),
            |audio| {
                black_box(effect.apply_to(format, black_box(audio.as_mut_slice())));
            },
            BatchSize::SmallInput,
        );
    });
}

fn effects(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("audio effects/stereo 48k");
    group.throughput(Throughput::Elements((FRAMES * CHANNELS) as u64));

    let active = (0..FRAMES * CHANNELS)
        .map(|sample| if sample % 2 == 0 { 0.5 } else { -0.25 })
        .collect::<Vec<_>>();
    let quiet = vec![0.001; FRAMES * CHANNELS];

    bench_effect(
        &mut group,
        "gain/non-unity",
        Gain::with_multiplier(0.5),
        &active,
    );
    bench_effect(&mut group, "compressor/active", Compressor::new(), &active);
    bench_effect(&mut group, "compressor/quiet", Compressor::new(), &quiet);
    bench_effect(&mut group, "gate/open", Gate::new(), &active);
    bench_effect(&mut group, "low-cut", LowCut::new(), &active);

    group.finish();
}

criterion_group!(benches, effects);
criterion_main!(benches);
