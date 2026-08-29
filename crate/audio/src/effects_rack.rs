use std::sync::{
    Arc, Weak,
    atomic::{AtomicU32, Ordering},
};

use crossbeam_queue::SegQueue;

use crate::{
    AudioSampleType, StreamFormat,
    effects::{Afx, AfxRegistration, REGISTERED_AFX},
    params::{ParamKey, ParamSpec, ParamState},
};

/// Identifies one effect [`Slot`] inside an [`EffectsRack`], for listing and
/// removal — the slot-level counterpart of [`ParamKey`].
pub type SlotKey = u64;

#[derive(Debug)]
struct LiveParams {
    revision: AtomicU32,
    values: Box<[AtomicU32]>,
}

impl LiveParams {
    fn new(values: &[f32]) -> Self {
        Self {
            revision: AtomicU32::new(0),
            values: values
                .iter()
                .map(|value| AtomicU32::new(value.to_bits()))
                .collect(),
        }
    }

    fn set(&self, key: ParamKey, value: f32) {
        let Some(slot) = self.values.get(key.0) else {
            return;
        };
        slot.store(value.to_bits(), Ordering::Relaxed);
        // Publishes the preceding value write. Backend's Acquire revision load
        // makes it visible before reading parameter bits.
        self.revision.fetch_add(1, Ordering::Release);
    }
}

/// One effect slot in a rack frontend.
#[derive(Clone, Debug)]
pub struct Slot {
    pub id: SlotKey,
    pub name: &'static str,
    pub specs: &'static [ParamSpec],
    /// Current parameter values, indexed like `specs`.
    pub values: Vec<f32>,
    live_params: Arc<LiveParams>,
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
        live_params: Arc<LiveParams>,
    },
    Remove {
        id: SlotKey,
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
/// the backend. Rare structural edits use a lossless lock-free queue; parameter
/// values use per-slot atomics and coalesce to the newest value. Frontend holds
/// only a weak queue reference, so edits do not accumulate after audio backend
/// closes. Such rack keeps working as a UI-only record.
pub struct EffectsRack {
    commands: Weak<SegQueue<RackCommand>>,
    next_id: SlotKey,
    slots: Vec<Slot>,
}

/// Audio-side half of a rack: owned exclusively by its channel, which is the
/// only thread to touch the live effects. Edits queued by the frontend are
/// adopted at the head of its `prepare` and `apply` calls.
pub struct RackBackend {
    commands: Arc<SegQueue<RackCommand>>,
    /// The channel's processing format, known once the backend is attached.
    format: Option<StreamFormat>,
    slots: Vec<BackendSlot>,
}

struct BackendSlot {
    id: SlotKey,
    effect: Box<dyn Afx + Send>,
    live_params: Arc<LiveParams>,
    observed_revision: u32,
}

impl EffectsRack {
    /// All built-in effects intended for user insertion.
    ///
    /// UI clients can render this catalog directly. Adding a registration in
    /// the audio crate then makes the effect available without app-side type
    /// matching or factory changes.
    pub fn available_effects() -> &'static [AfxRegistration] {
        REGISTERED_AFX
    }

    /// A new, empty chain: the frontend to keep for edits and snapshots, and
    /// the backend to hand to the channel that will process audio through it.
    pub fn new() -> (EffectsRack, RackBackend) {
        let commands = Arc::new(SegQueue::new());
        (
            EffectsRack {
                commands: Arc::downgrade(&commands),
                next_id: 0,
                slots: Vec::new(),
            },
            RackBackend {
                commands,
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
        let values = specs
            .iter()
            .map(|spec| effect.param_value(spec.key).unwrap_or(spec.default))
            .collect::<Vec<_>>();
        let live_params = Arc::new(LiveParams::new(&values));
        self.slots.push(Slot {
            id,
            name: effect.name(),
            specs,
            values,
            live_params: live_params.clone(),
        });
        if let Some(commands) = self.commands.upgrade() {
            commands.push(RackCommand::Insert {
                id,
                effect,
                live_params,
            });
        }
        id
    }

    /// Construct and append one effect from [`available_effects`](Self::available_effects).
    pub fn insert_registered(&mut self, registration: AfxRegistration) -> SlotKey {
        self.insert(registration.build())
    }

    /// Remove the effect `id`, if it is still present.
    pub fn remove(&mut self, id: SlotKey) {
        self.slots.retain(|slot| slot.id != id);
        if let Some(commands) = self.commands.upgrade() {
            commands.push(RackCommand::Remove { id });
        }
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
        slot.live_params.set(param, value);
    }

    /// The current chain in processing order, with every parameter's value.
    pub fn snapshot(&self) -> Vec<Slot> {
        self.slots.clone()
    }
}

impl RackBackend {
    /// Adopt every edit queued by the frontend.
    fn drain(&mut self) {
        while let Some(command) = self.commands.pop() {
            match command {
                RackCommand::Insert {
                    id,
                    mut effect,
                    live_params,
                } => {
                    if let Some(format) = self.format {
                        effect.prepare(format);
                    }
                    self.slots.push(BackendSlot {
                        id,
                        effect,
                        live_params,
                        // Force one synchronization. A parameter may have
                        // changed while this structural command was queued.
                        observed_revision: u32::MAX,
                    });
                }
                RackCommand::Remove { id } => self.slots.retain(|slot| slot.id != id),
            }
        }
        for slot in &mut self.slots {
            let revision = slot.live_params.revision.load(Ordering::Acquire);
            if revision == slot.observed_revision {
                continue;
            }
            for (index, value) in slot.live_params.values.iter().enumerate() {
                slot.effect.set_param(
                    ParamKey(index),
                    f32::from_bits(value.load(Ordering::Relaxed)),
                );
            }
            slot.observed_revision = revision;
        }
    }

    /// Attach the backend to a channel processing in `format`: remember the
    /// format for late insertions and prepare everything already present.
    pub(crate) fn prepare(&mut self, format: StreamFormat) {
        self.drain();
        self.format = Some(format);
        for slot in &mut self.slots {
            slot.effect.prepare(format);
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
            .all(|slot| slot.effect.apply_to(format, audio))
    }
}

#[cfg(test)]
#[path = "../tests/unit/effects_rack.rs"]
mod tests;
