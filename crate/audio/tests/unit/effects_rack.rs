use super::*;
use crate::effects::{Gain, Gate};
use crate::params::AfxParams;
use crate::{SampleFormat, SampleRate};
use std::sync::{Arc, Mutex};

/// Adds a constant, records `prepare` calls, optionally discards.
/// Opaque: its fields are not reflectable, so it has no parameters -
/// the legitimate shape for an effect with nothing to tune.
#[derive(AfxParams)]
struct Probe {
    offset: AudioSampleType,
    keep: bool,
    prepared: Arc<Mutex<Vec<SampleRate>>>,
}

impl Afx for Probe {
    fn name(&self) -> &'static str {
        "Probe"
    }
    fn prepare(&mut self, format: StreamFormat) {
        self.prepared.lock().unwrap().push(format.sample_rate);
    }
    fn apply_to(&mut self, _format: StreamFormat, audio: &mut [AudioSampleType]) -> bool {
        for sample in audio.iter_mut() {
            *sample += self.offset;
        }
        self.keep
    }
}

fn probe(offset: AudioSampleType, keep: bool) -> (Box<Probe>, Arc<Mutex<Vec<SampleRate>>>) {
    let prepared = Arc::new(Mutex::new(Vec::new()));
    (
        Box::new(Probe {
            offset,
            keep,
            prepared: prepared.clone(),
        }),
        prepared,
    )
}

fn format() -> StreamFormat {
    StreamFormat {
        channels: 2,
        sample_format: SampleFormat::F32,
        sample_rate: 48_000,
    }
}

fn first_param(rack: &EffectsRack) -> ParamState {
    rack.snapshot()[0].params().next().unwrap()
}

#[test]
fn applies_in_insertion_order_and_removes() {
    let (mut rack, mut backend) = EffectsRack::new();
    let (a, _) = probe(1.0, true);
    let (b, _) = probe(10.0, true);
    let a_id = rack.insert(a);
    rack.insert(b);
    backend.prepare(format());

    let mut audio = [0.0; 2];
    assert!(backend.apply(&mut audio));
    assert_eq!(audio, [11.0, 11.0]);

    rack.remove(a_id);
    assert!(!rack.contains(a_id));
    assert_eq!(rack.snapshot().len(), 1);
    let mut audio = [0.0; 2];
    assert!(backend.apply(&mut audio));
    assert_eq!(audio, [10.0, 10.0]);
}

#[test]
fn discard_short_circuits_the_rest() {
    let (mut rack, mut backend) = EffectsRack::new();
    let (dropper, _) = probe(0.0, false);
    let (after, _) = probe(100.0, true);
    rack.insert(dropper);
    rack.insert(after);
    backend.prepare(format());

    let mut audio = [0.0; 2];
    assert!(!backend.apply(&mut audio));
    // The second effect never ran.
    assert_eq!(audio, [0.0, 0.0]);
}

#[test]
fn late_insert_is_prepared_with_the_attached_format() {
    let (mut rack, mut backend) = EffectsRack::new();
    let (early, early_prepared) = probe(0.0, true);
    rack.insert(early);
    // Not attached yet: nothing to prepare against.
    backend.apply(&mut []);
    assert!(early_prepared.lock().unwrap().is_empty());

    backend.prepare(format());
    assert_eq!(*early_prepared.lock().unwrap(), vec![48_000]);

    // An insert after attachment is prepared when the backend adopts it.
    let (late, late_prepared) = probe(0.0, true);
    rack.insert(late);
    backend.apply(&mut []);
    assert_eq!(*late_prepared.lock().unwrap(), vec![48_000]);
}

/// The frontend answers UI reads by itself — an effect is listed the moment
/// it is inserted, before the backend has adopted anything.
#[test]
fn frontend_lists_without_the_backend() {
    let (mut rack, backend) = EffectsRack::new();
    let (a, _) = probe(1.0, true);
    let id = rack.insert(a);

    let snapshot = rack.snapshot();
    assert_eq!((snapshot[0].id, snapshot[0].name), (id, "Probe"));
    // A settings-less effect still lists cleanly.
    assert!(snapshot[0].specs.is_empty());

    // A rack whose channel is gone keeps working as a UI-only record.
    drop(backend);
    rack.remove(id);
    assert!(rack.snapshot().is_empty());
}

#[test]
fn set_param_reaches_the_effect_and_clamps() {
    let (mut rack, mut backend) = EffectsRack::new();
    let id = rack.insert(Box::new(Gain::with_multiplier(1.0)));
    backend.prepare(format());
    let param = first_param(&rack);
    assert_eq!(param.value, 1.0);

    // The write lands in the frontend's copy and in the live effect (whose
    // glide settles, then snaps exactly onto the target).
    rack.set_param(id, param.spec.key, 0.0);
    assert_eq!(first_param(&rack).value, 0.0);
    let mut audio = [0.5; 48];
    for _ in 0..400 {
        backend.apply(&mut audio);
        audio = [0.5; 48];
    }
    backend.apply(&mut audio);
    assert_eq!(audio, [0.0; 48], "the live Gain must have adopted the edit");

    // Out-of-range values clamp to the spec.
    rack.set_param(id, param.spec.key, 99.0);
    assert_eq!(first_param(&rack).value, param.spec.max);

    // Unknown parameter or effect ids are ignored, not panics.
    rack.set_param(id, ParamKey(7), 1.0);
    rack.set_param(id + 1, param.spec.key, 1.0);
    assert_eq!(first_param(&rack).value, param.spec.max);
}

#[test]
fn gate_params_come_from_field_attributes() {
    let (mut rack, _backend) = EffectsRack::new();
    let id = rack.insert(Box::new(Gate::default()));
    let snapshot = rack.snapshot();
    let params: Vec<_> = snapshot[0].params().collect();
    let names: Vec<&str> = params.iter().map(|p| p.spec.name).collect();
    assert_eq!(
        names,
        [
            "Threshold",
            "Hysteresis",
            "Attack",
            "Hold",
            "Release",
            "Floor"
        ]
    );
    // Fresh from the constructor, every knob sits on its spec default.
    assert!(params.iter().all(|p| p.value == p.spec.default));

    // The floor knob renders "off" at its minimum, which the gate maps
    // to a full close internally.
    let floor = params[5];
    assert!(floor.spec.min_is_off);
    assert_eq!(floor.value, floor.spec.min);

    // The generated per-field key consts index the same spec table.
    assert_eq!(params[0].spec.key, Gate::THRESHOLD_DB);

    // A write lands in the snapshot.
    rack.set_param(id, Gate::THRESHOLD_DB, -30.0);
    assert_eq!(first_param(&rack).value, -30.0);
}
