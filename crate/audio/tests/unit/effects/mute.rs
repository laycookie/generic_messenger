use super::*;
use crate::{SampleFormat, StreamFormat};

fn format() -> StreamFormat {
    StreamFormat {
        channels: 2,
        sample_format: SampleFormat::F32,
        sample_rate: 48_000,
    }
}

#[test]
fn switch_discards_while_muted() {
    let mut mute = Mute::new();
    let switch = mute.switch();
    let mut audio = [0.25, -0.5];

    assert!(mute.apply_to(format(), &mut audio));
    switch.set(true);
    assert!(!mute.apply_to(format(), &mut audio));
    switch.set(false);
    assert!(mute.apply_to(format(), &mut audio));
}
