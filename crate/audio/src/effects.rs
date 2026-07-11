use std::sync::mpsc;

use crate::{AudioSampleType, StreamFormat};

mod gain;
mod gate;
mod low_cut;
mod mute;
mod params;

pub use gain::Gain;
pub use gate::Gate;
pub use low_cut::LowCut;
pub use mute::{Mute, MuteSwitch};
pub use params::{AfxParams, ParamKey, ParamSpec, ParamState, ParamUnit};

/// Identifies one effect [`Slot`] inside an [`EffectsRack`], for listing and
/// removal — the slot-level counterpart of [`ParamKey`].
pub type SlotKey = u64;

/// An audio effect applied to a channel's sample stream.
///
/// Effects run on the control thread, not in the real-time audio callback.
/// `false` from [`apply_to`](Afx::apply_to) drops the whole buffer and
/// short-circuits the remaining chain.
pub trait Afx: AfxParams {
    /// Display name for UI listings of a rack's contents.
    fn name(&self) -> &'static str;

    /// Called before the effect processes any audio, with the format it will
    /// process in.
    fn prepare(&mut self, _format: StreamFormat) {}

    /// Process `audio` in `format`. `format` is passed per buffer so effect
    /// logic can read the current pipeline context without storing it.
    fn apply_to(&mut self, format: StreamFormat, audio: &mut [AudioSampleType]) -> bool;
}

/// One effect slot in a rack frontend.
#[derive(Clone, Debug)]
pub struct Slot {
    pub id: SlotKey,
    pub name: &'static str,
    pub specs: &'static [ParamSpec],
    /// Current parameter values, indexed like `specs`.
    pub values: Vec<f32>,
}

impl Slot {
    pub fn params(&self) -> impl Iterator<Item = ParamState> + '_ {
        self.specs
            .iter()
            .zip(&self.values)
            .map(|(spec, value)| ParamState {
                spec,
                value: *value,
            })
    }
}

/// One edit travelling from the rack's frontend to its backend.
enum RackCommand {
    Insert {
        id: SlotKey,
        effect: Box<dyn Afx + Send>,
    },
    Remove {
        id: SlotKey,
    },
    SetParam {
        id: SlotKey,
        key: ParamKey,
        value: f32,
    },
}

/// UI-side half of a channel's insert-effect chain.
///
/// Split like a [fundsp `Net`](fundsp::net::Net) and its backend, so no shared
/// `Mutex` guards rack state: [`EffectsRack::new`] yields this frontend and the
/// [`RackBackend`] that owns the live effects. The frontend keeps the
/// authoritative structure and parameter values for UI reads - sound because
/// parameters only ever change through [`set_param`](EffectsRack::set_param)
/// (an effect must not mutate its own parameters) - and mirrors every edit to
/// the backend over an unbounded channel, adopted before the backend's next
/// buffer. A rack whose channel has closed keeps working as a UI-only record.
pub struct EffectsRack {
    tx: mpsc::Sender<RackCommand>,
    next_id: SlotKey,
    slots: Vec<Slot>,
}

/// Audio-side half of a rack: owned exclusively by its channel, which is the
/// only thread to touch the live effects. Edits queued by the frontend are
/// adopted at the head of [`prepare`](RackBackend::prepare) and
/// [`apply`](RackBackend::apply).
pub struct RackBackend {
    rx: mpsc::Receiver<RackCommand>,
    /// The channel's processing format, known once the backend is attached.
    format: Option<StreamFormat>,
    slots: Vec<(SlotKey, Box<dyn Afx + Send>)>,
}

impl EffectsRack {
    /// A new, empty chain: the frontend to keep for edits and snapshots, and
    /// the backend to hand to the channel that will process audio through it.
    pub fn new() -> (EffectsRack, RackBackend) {
        let (tx, rx) = mpsc::channel();
        (
            EffectsRack {
                tx,
                next_id: 0,
                slots: Vec::new(),
            },
            RackBackend {
                rx,
                format: None,
                slots: Vec::new(),
            },
        )
    }

    /// Append `effect` to the chain. The backend prepares it with the
    /// channel's format when it adopts the edit.
    pub fn insert(&mut self, effect: Box<dyn Afx + Send>) -> SlotKey {
        let id = self.next_id;
        self.next_id += 1;
        let specs = effect.param_specs();
        self.slots.push(Slot {
            id,
            name: effect.name(),
            specs,
            values: specs
                .iter()
                .map(|spec| effect.param_value(spec.key).unwrap_or(spec.default))
                .collect(),
        });
        let _ = self.tx.send(RackCommand::Insert { id, effect });
        id
    }

    /// Remove the effect `id`, if it is still present.
    pub fn remove(&mut self, id: SlotKey) {
        self.slots.retain(|slot| slot.id != id);
        let _ = self.tx.send(RackCommand::Remove { id });
    }

    /// Whether the effect `id` is still in the chain.
    pub fn contains(&self, id: SlotKey) -> bool {
        self.slots.iter().any(|slot| slot.id == id)
    }

    /// Set one parameter on the effect `id`, clamped to its spec's range
    /// (matching the clamp the effect's generated setter applies). Unknown
    /// effects or parameters are ignored.
    pub fn set_param(&mut self, id: SlotKey, param: ParamKey, value: f32) {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.id == id) else {
            return;
        };
        let Some(spec) = slot.specs.get(param.0) else {
            return;
        };
        let Some(slot_value) = slot.values.get_mut(param.0) else {
            return;
        };
        let value = value.clamp(spec.min, spec.max);
        *slot_value = value;
        let _ = self.tx.send(RackCommand::SetParam {
            id,
            key: param,
            value,
        });
    }

    /// The current chain in processing order, with every parameter's value.
    pub fn snapshot(&self) -> Vec<Slot> {
        self.slots.clone()
    }
}

impl RackBackend {
    /// Adopt every edit queued by the frontend.
    fn drain(&mut self) {
        while let Ok(command) = self.rx.try_recv() {
            match command {
                RackCommand::Insert { id, mut effect } => {
                    if let Some(format) = self.format {
                        effect.prepare(format);
                    }
                    self.slots.push((id, effect));
                }
                RackCommand::Remove { id } => self.slots.retain(|(slot_id, _)| *slot_id != id),
                RackCommand::SetParam { id, key, value } => {
                    if let Some((_, effect)) =
                        self.slots.iter_mut().find(|(slot_id, _)| *slot_id == id)
                    {
                        effect.set_param(key, value);
                    }
                }
            }
        }
    }

    /// Attach the backend to a channel processing in `format`: remember the
    /// format for late insertions and prepare everything already present.
    pub(crate) fn prepare(&mut self, format: StreamFormat) {
        self.drain();
        self.format = Some(format);
        for (_, effect) in self.slots.iter_mut() {
            effect.prepare(format);
        }
    }

    /// Run the chain over `audio`. Returns whether the buffer should be kept;
    /// the first effect to discard short-circuits the rest.
    pub(crate) fn apply(&mut self, audio: &mut [AudioSampleType]) -> bool {
        self.drain();
        let Some(format) = self.format else {
            return true;
        };
        self.slots
            .iter_mut()
            .all(|(_, effect)| effect.apply_to(format, audio))
    }
}

#[cfg(test)]
#[path = "../tests/unit/effects.rs"]
mod tests;
