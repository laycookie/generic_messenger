use super::*;
use crate::SampleFormat;

/// A gate prepared for mono processing at a -40dB voice threshold,
/// with `tweak` applied on top.
fn gate(tweak: impl FnOnce(&mut Gate)) -> Gate {
    let mut gate = Gate::new().tuned(|gate| {
        gate.threshold_db = -40.0;
        tweak(gate);
    });
    gate.prepare(mono_format(48_000));
    gate
}

fn mono_format(sample_rate: u32) -> StreamFormat {
    StreamFormat {
        channels: 1,
        sample_format: SampleFormat::F32,
        sample_rate,
    }
}

#[test]
fn hold_uses_apply_format_sample_rate() {
    fn holds_after_2000_quiet_frames(format: StreamFormat) -> bool {
        let mut gate = Gate::new().tuned(|gate| {
            gate.threshold_db = -40.0;
            gate.attack_ms = 0.0;
            gate.hold_ms = 100.0;
            gate.release_ms = 0.0;
        });
        gate.prepare(format);
        let mut loud = [0.5; 8];
        assert!(gate.apply_to(format, &mut loud));
        let mut gap = [0.0; 2_000];
        assert!(gate.apply_to(format, &mut gap));
        let mut next = [0.0; 1];
        gate.apply_to(format, &mut next)
    }

    assert!(!holds_after_2000_quiet_frames(mono_format(16_000)));
    assert!(holds_after_2000_quiet_frames(mono_format(48_000)));
}

#[test]
fn opens_and_ramps_up_over_attack() {
    let mut gate = gate(|gate| gate.attack_ms = 5.0);
    let format = mono_format(48_000);
    let mut buf = [0.5; 480];
    assert!(gate.apply_to(format, &mut buf));
    // Attack glides the gain up across the buffer rather than snapping.
    assert!(buf[0].abs() < buf[479].abs());
    assert!(buf[479].abs() > 0.0);
}

#[test]
fn holds_through_a_short_gap() {
    let mut gate = gate(|gate| {
        gate.attack_ms = 0.0;
        gate.hold_ms = 100.0;
    });
    let format = mono_format(48_000);
    let mut loud = [0.5; 8];
    assert!(gate.apply_to(format, &mut loud));
    // A 10ms gap sits well inside the 100ms hold: kept, not dropped.
    let mut gap = [0.0; 480];
    assert!(gate.apply_to(format, &mut gap));
}

#[test]
fn drops_buffer_once_fully_closed() {
    let mut gate = gate(|gate| {
        gate.attack_ms = 0.0;
        gate.hold_ms = 0.0;
        gate.release_ms = 0.0;
    });
    let format = mono_format(48_000);
    let mut loud = [0.5; 8];
    assert!(gate.apply_to(format, &mut loud));
    // No hold or release, full-close floor: the next silent buffer is
    // dropped so the consumer parks.
    let mut quiet = [0.0; 8];
    assert!(!gate.apply_to(format, &mut quiet));
}

#[test]
fn raised_floor_ducks_and_never_drops() {
    let mut gate = gate(|gate| gate.floor_db = -20.0);
    // -20dB floor: silence is attenuated but still emitted, so it is kept.
    let mut quiet = [0.0; 8];
    assert!(gate.apply_to(mono_format(48_000), &mut quiet));
}

#[test]
fn keeps_empty_buffer() {
    assert!(gate(|_| {}).apply_to(mono_format(48_000), &mut []));
}

#[test]
fn floor_off_closes_fully() {
    // The FLOOR_OFF sentinel resolves to a zero floor gain (-infinity), which is
    // what park-on-silence relies on; anything above it merely ducks.
    let closed = gate(|_| {});
    assert_eq!(closed.floor_gain, 0.0);
    let ducked = gate(|gate| gate.floor_db = -20.0);
    assert!(ducked.floor_gain > 0.0);
}

#[test]
fn live_edits_do_not_reset_an_open_gate() {
    let mut gate = gate(|gate| gate.attack_ms = 0.0);
    let format = mono_format(48_000);
    let mut loud = [0.5; 8];
    assert!(gate.apply_to(format, &mut loud));
    // The gate is open; a mid-stream tweak must not slam it shut.
    gate.set_param(Gate::THRESHOLD_DB, -35.0);
    let mut still_loud = [0.5; 8];
    assert!(gate.apply_to(format, &mut still_loud));
    assert!(still_loud[0] > 0.4);
}
