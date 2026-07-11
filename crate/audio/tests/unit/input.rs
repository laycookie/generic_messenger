use std::sync::Arc;

use ringbuf::traits::{Observer as _, Producer as _};

use super::*;
use crate::{
    SampleFormat,
    effects::{Afx, EffectsRack},
};

#[derive(credential_macro::AfxParams)]
struct DropAll {
    _private: (),
}

impl Afx for DropAll {
    fn name(&self) -> &'static str {
        "DropAll"
    }

    fn apply_to(&mut self, _format: StreamFormat, _audio: &mut [AudioSampleType]) -> bool {
        false
    }
}

fn voice_format() -> StreamFormat {
    StreamFormat {
        channels: 2,
        sample_format: SampleFormat::F32,
        sample_rate: 48_000,
    }
}

fn gated_consumer() -> (SampleConsumer, SampleProd<CHANNEL_BUFFER_SIZE>) {
    let format = voice_format();
    let rb = SampleRb::<CHANNEL_BUFFER_SIZE>::default();
    let notify = Arc::new(Notify::new());
    let (mut rack, mut effects) = EffectsRack::new();
    rack.insert(Box::new(DropAll { _private: () }));
    effects.prepare(format);

    (
        SampleConsumer {
            cons: CachingCons::new(rb.clone()),
            notify,
            resampler: Resampler::new(
                format.channels as usize,
                format.sample_rate,
                format.channels as usize,
                format.sample_rate,
            ),
            effects,
            scratch: vec![0.0; CONVERT_SCRATCH_LEN].into_boxed_slice(),
            format,
        },
        CachingProd::new(rb),
    )
}

#[test]
fn dropped_buffers_do_not_drain_the_whole_ring() {
    let (mut consumer, mut producer) = gated_consumer();
    let samples = vec![0.0; 1024];
    assert_eq!(producer.push_slice(&samples), samples.len());

    let before = consumer.cons.occupied_len();
    let mut out = vec![0.0; 128];
    assert_eq!(consumer.pop_now::<f32>(&mut out), 0);

    let drained = before - consumer.cons.occupied_len();
    assert_eq!(
        drained,
        out.len(),
        "one discarded read may consume one output-sized chunk, not the whole queued ring"
    );
}
