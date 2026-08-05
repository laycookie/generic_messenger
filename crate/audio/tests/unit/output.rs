use ringbuf::traits::{Consumer as _, Producer as _};

use super::*;
use crate::{SampleFormat, effects::Gain, effects_rack::EffectsRack};

/// A `SampleProducer` wired to a bare ring instead of a device, so the
/// full push path (convert -> effects -> ring) is testable. The rack
/// frontend is returned for live edits.
fn producer(
    format: StreamFormat,
    device: StreamFormat,
) -> (
    SampleProducer,
    SampleConsum<CHANNEL_BUFFER_SIZE>,
    EffectsRack,
) {
    let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
    let (rack, mut effects) = EffectsRack::new();
    effects.prepare(device);
    (
        SampleProducer {
            producer: CachingProd::new(rb.clone()),
            resampler: Resampler::new(
                format.channels as usize,
                format.sample_rate,
                device.channels as usize,
                device.sample_rate,
            ),
            effects,
            scratch: vec![0.0; CONVERT_SCRATCH_LEN].into_boxed_slice(),
            format,
        },
        CachingCons::new(rb),
        rack,
    )
}

fn voice_format() -> StreamFormat {
    // What the voice backends declare and push (see the app's
    // process_audio_event).
    StreamFormat {
        channels: 2,
        sample_format: SampleFormat::I16,
        sample_rate: 48_000,
    }
}

/// Pitch regression: at matching rates the push path must be
/// sample-exact - same count, same order, no stretching. An empty rack
/// must not alter a single sample.
#[test]
fn default_path_is_sample_exact() {
    let (mut producer, mut ring, _rack) = producer(voice_format(), voice_format());

    let samples: Vec<i16> = (0..960).map(|i| (i * 7 % 4096) as i16).collect();
    let consumed = producer.push_iter(&samples);
    assert_eq!(consumed, samples.len());

    let mut out = vec![0.0 as AudioSampleType; samples.len() + 8];
    let popped = ring.pop_slice(&mut out);
    assert_eq!(
        popped,
        samples.len(),
        "ring must hold exactly what was pushed"
    );
    for (got, src) in out[..popped].iter().zip(&samples) {
        assert_eq!(*got, AudioSampleType::from_sample(*src));
    }
}

/// A volume change scales amplitude only: the sample count through the
/// channel is untouched mid-glide and after settling.
#[test]
fn volume_changes_amplitude_not_length() {
    let format = voice_format();
    let (mut producer, mut ring, mut rack) = producer(format, format);
    rack.insert(Box::new(Gain::with_multiplier(0.5)));

    let samples = vec![1000_i16; 960];
    let mut out = vec![0.0 as AudioSampleType; 1024];
    for _ in 0..40 {
        assert_eq!(producer.push_iter(&samples), samples.len());
        assert_eq!(ring.pop_slice(&mut out), samples.len());
    }
    // Settled by now: exactly half amplitude, still one output sample
    // per input sample.
    let expected = AudioSampleType::from_sample(1000_i16) * 0.5;
    assert!(out[..960].iter().all(|s| (s - expected).abs() < 1e-4));
}

#[test]
fn mixer_reads_wrapped_ring_directly_in_whole_frames() {
    let rb = SampleRb::<8>::default();
    let mut producer = CachingProd::new(rb.clone());
    let mut consumer = CachingCons::new(rb);

    assert_eq!(producer.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 6);
    let mut discarded = [0.0; 4];
    assert_eq!(consumer.pop_slice(&mut discarded), 4);
    assert_eq!(producer.push_slice(&[7.0, 8.0, 9.0, 10.0, 11.0, 12.0]), 6);

    let mut mixed = [0.5; 6];
    assert_eq!(mix_channel(&mut consumer, &mut mixed, 2), mixed.len());
    assert_eq!(mixed, [5.5, 6.5, 7.5, 8.5, 9.5, 10.5]);
    assert_eq!(consumer.occupied_len(), 2);
}
