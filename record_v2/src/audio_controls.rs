//! App-side registry of live voice audio controls.
//!
//! The UI keeps each channel's [`EffectsRack`] frontend; mixer channels own the
//! matching backend. Edits are mirrored to audio processing without a shared
//! rack lock, and UI snapshots come from the frontend copy.

use std::collections::HashMap;

use messenger_interface::types::ID;
use messenger_unifier::{MessengerId, ScopedId, UserEntity};
use simple_audio_channels::{
    effects::{AfxRegistration, Gain, MuteSwitch},
    effects_rack::{EffectsRack, Slot, SlotKey},
    params::ParamKey,
};

/// Which live channel an afx edit is aimed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfxTarget {
    /// Local microphone channel for one messenger backend.
    Mic { messenger: MessengerId },
    /// A voice participant's playback channel.
    User { messenger: MessengerId, user: ID },
}

/// One live channel: its rack frontend, plus which of its effects are hidden
/// control plumbing (mute/deafen) rather than user-editable afx slots.
struct ChannelAudio {
    rack: EffectsRack,
    hidden_effects: Vec<SlotKey>,
}

impl ChannelAudio {
    fn visible_effects(&self) -> Vec<Slot> {
        let mut effects = self.rack.snapshot();
        effects.retain(|info| !self.hidden_effects.contains(&info.id));
        effects
    }
}

/// One voice participant's audio state.
struct UserAudio {
    /// Desired volume multiplier (1.0 = unity). Outlives the channel.
    volume: f32,
    /// The seeded [`Gain`] backing the participant slider, while live.
    gain: Option<SlotKey>,
    /// The live playback channel, while it exists.
    channel: Option<ChannelAudio>,
    /// Playback mute owned by this user's live channel.
    output_mute: Option<MuteSwitch>,
}

impl UserAudio {
    fn new(volume: f32) -> Self {
        Self {
            volume,
            gain: None,
            channel: None,
            output_mute: None,
        }
    }
}

struct MicAudio {
    channel: ChannelAudio,
    mute: MuteSwitch,
}

#[derive(Default)]
pub struct AudioControls {
    /// Live microphone channels grouped by backend. Several backends—and
    /// repeated backend input requests—must all obey global mute.
    mics: HashMap<MessengerId, Vec<MicAudio>>,
    /// Desired mic mute. Outlives the channel.
    mic_muted: bool,
    /// Live output mute switches. Includes sources without a known user, so
    /// deafen still silences them.
    anonymous_output_mutes: Vec<MuteSwitch>,
    /// Desired deafen. Same optimistic/reconciled lifecycle as `mic_muted`.
    deafened: bool,
    users: HashMap<ScopedId<UserEntity>, UserAudio>,
}

impl AudioControls {
    pub fn mic_muted(&self) -> bool {
        self.mic_muted
    }

    pub fn deafened(&self) -> bool {
        self.deafened
    }

    /// Store the desired mic mute and apply it to the live switch, if any.
    pub fn set_mic_muted(&mut self, muted: bool) {
        self.mic_muted = muted;
        for mic in self.mics.values_mut().flatten() {
            mic.mute.set(muted);
        }
    }

    /// Store the desired deafen state and silence/restore playback.
    pub fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
        for state in self.users.values() {
            if let Some(mute) = &state.output_mute {
                mute.set(deafened);
            }
        }
        self.anonymous_output_mutes.retain(MuteSwitch::is_attached);
        for mute in &self.anonymous_output_mutes {
            mute.set(deafened);
        }
    }

    /// Track one output channel's deafen mute, seeding it from stored state.
    pub fn attach_output_mute(
        &mut self,
        messenger: MessengerId,
        user: Option<ID>,
        mute: MuteSwitch,
    ) {
        mute.set(self.deafened);
        if let Some(user) = user {
            self.users
                .entry(ScopedId::new(messenger, user))
                .or_insert_with(|| UserAudio::new(1.0))
                .output_mute = Some(mute);
        } else {
            self.anonymous_output_mutes.retain(MuteSwitch::is_attached);
            self.anonymous_output_mutes.push(mute);
        }
    }

    /// Adopt a freshly created capture channel's rack and mute, seeding the
    /// mute from the stored state.
    pub fn attach_mic(
        &mut self,
        messenger: MessengerId,
        rack: EffectsRack,
        mute: MuteSwitch,
        hidden: Vec<SlotKey>,
    ) {
        mute.set(self.mic_muted);
        let mics = self.mics.entry(messenger).or_default();
        mics.retain(|mic| mic.mute.is_attached());
        mics.push(MicAudio {
            channel: ChannelAudio {
                rack,
                hidden_effects: hidden,
            },
            mute,
        });
    }

    /// Drop the live mic handles (the capture stream closed); the stored
    /// mute state survives for the next channel.
    pub fn detach_all_mics(&mut self) {
        self.mics.clear();
    }

    /// The volume a (possibly future) playback channel for this user should
    /// run at.
    pub fn user_volume(&self, messenger: MessengerId, user: ID) -> f32 {
        self.users
            .get(&ScopedId::new(messenger, user))
            .map(|state| state.volume)
            .unwrap_or(1.0)
    }

    /// Store the desired volume and apply it to the live Gain, if any —
    /// re-inserting the Gain if the user removed it from the rack.
    pub fn set_user_volume(&mut self, messenger: MessengerId, user: ID, volume: f32) {
        let state = self
            .users
            .entry(ScopedId::new(messenger, user))
            .or_insert_with(|| UserAudio::new(volume));
        state.volume = volume;
        let Some(channel) = state.channel.as_mut() else {
            return;
        };
        let gain = match state.gain {
            Some(gain) if channel.rack.contains(gain) => gain,
            _ => channel.rack.insert(Box::new(Gain::with_multiplier(volume))),
        };
        state.gain = Some(gain);
        channel
            .rack
            .set_param(gain, Gain::DESIRED_MULTIPLIER, volume);
    }

    /// Adopt a freshly created playback channel's rack for `user`, seeding
    /// its `gain` effect with the stored volume.
    pub fn attach_user(
        &mut self,
        messenger: MessengerId,
        user: ID,
        mut rack: EffectsRack,
        gain: SlotKey,
        hidden: Vec<SlotKey>,
    ) {
        let state = self
            .users
            .entry(ScopedId::new(messenger, user))
            .or_insert_with(|| UserAudio::new(1.0));
        rack.set_param(gain, Gain::DESIRED_MULTIPLIER, state.volume);
        state.gain = Some(gain);
        state.channel = Some(ChannelAudio {
            rack,
            hidden_effects: hidden,
        });
    }

    /// Drop the live handles for `user` (their channel is gone); the stored
    /// volume survives for the next one.
    pub fn detach_user(&mut self, messenger: MessengerId, user: ID) {
        if let Some(state) = self.users.get_mut(&ScopedId::new(messenger, user)) {
            state.gain = None;
            state.channel = None;
            state.output_mute = None;
        }
    }

    /// Drop every live playback handle — the output stream closed.
    pub fn detach_all_users(&mut self) {
        for state in self.users.values_mut() {
            state.gain = None;
            state.channel = None;
            state.output_mute = None;
        }
        self.anonymous_output_mutes.clear();
    }

    /// Forget live controls owned by one discarded backend instance.
    pub fn detach_messenger(&mut self, messenger: MessengerId) {
        self.mics.remove(&messenger);
        self.users
            .retain(|scoped, _| scoped.messenger() != messenger);
    }

    fn channel(&self, target: AfxTarget) -> Option<&ChannelAudio> {
        match target {
            AfxTarget::Mic { messenger } => self
                .mics
                .get(&messenger)
                .and_then(|mics| mics.last())
                .map(|mic| &mic.channel),
            AfxTarget::User { messenger, user } => self
                .users
                .get(&ScopedId::new(messenger, user))
                .and_then(|state| state.channel.as_ref()),
        }
    }

    fn channel_mut(&mut self, target: AfxTarget) -> Option<&mut ChannelAudio> {
        match target {
            AfxTarget::Mic { messenger } => self
                .mics
                .get_mut(&messenger)
                .and_then(|mics| mics.last_mut())
                .map(|mic| &mut mic.channel),
            AfxTarget::User { messenger, user } => self
                .users
                .get_mut(&ScopedId::new(messenger, user))
                .and_then(|state| state.channel.as_mut()),
        }
    }

    /// Visible effects for the afx sheet, snapshotted from the live rack.
    pub fn effects(&self, target: AfxTarget) -> Option<Vec<Slot>> {
        self.channel(target).map(ChannelAudio::visible_effects)
    }

    #[cfg(test)]
    pub(crate) fn has_channel(&self, target: AfxTarget) -> bool {
        self.channel(target).is_some()
    }

    /// Whether an effect is hidden control plumbing (mute/deafen), not a
    /// user-editable afx slot.
    pub(crate) fn is_hidden_effect(&self, target: AfxTarget, id: SlotKey) -> bool {
        self.channel(target)
            .is_some_and(|channel| channel.hidden_effects.contains(&id))
    }

    pub fn apply(&mut self, op: AudioOp) {
        match op {
            AudioOp::SetUserVolume {
                messenger,
                user,
                volume,
            } => self.set_user_volume(messenger, user, volume),
            AudioOp::AddEffect { target, effect } => {
                if let Some(channel) = self.channel_mut(target) {
                    channel.rack.insert_registered(effect);
                }
            }
            AudioOp::RemoveEffect { target, id } => {
                if self.is_hidden_effect(target, id) {
                    return;
                }
                if let Some(channel) = self.channel_mut(target) {
                    channel.rack.remove(id);
                }
                if let AfxTarget::User { messenger, user } = target
                    && let Some(state) = self.users.get_mut(&ScopedId::new(messenger, user))
                    && state.gain == Some(id)
                {
                    state.gain = None;
                }
            }
            AudioOp::SetEffectParam {
                target,
                effect,
                param,
                value,
            } => {
                if self.is_hidden_effect(target, effect) {
                    return;
                }
                if let Some(channel) = self.channel_mut(target) {
                    channel.rack.set_param(effect, param, value);
                }
            }
        }
    }
}

/// An audio edit made in the UI, bubbled up to the app which owns
/// [`AudioControls`].
#[derive(Debug, Clone, Copy)]
pub enum AudioOp {
    SetUserVolume {
        messenger: MessengerId,
        user: ID,
        volume: f32,
    },
    AddEffect {
        target: AfxTarget,
        effect: AfxRegistration,
    },
    RemoveEffect {
        target: AfxTarget,
        id: SlotKey,
    },
    /// One knob of one effect moved (the key comes from the rack's UI snapshot).
    SetEffectParam {
        target: AfxTarget,
        effect: SlotKey,
        param: ParamKey,
        value: f32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_audio_channels::effects::Mute;

    fn ids() -> (MessengerId, ID) {
        (MessengerId::default(), 42)
    }

    fn attach_user_with_gain(
        controls: &mut AudioControls,
        messenger: MessengerId,
        user: ID,
        volume: f32,
    ) -> SlotKey {
        // The backend is dropped: these tests exercise the UI-side state,
        // which must work the same with or without a live channel.
        let (mut rack, _backend) = EffectsRack::new();
        let gain = rack.insert(Box::new(Gain::with_multiplier(volume)));
        controls.attach_user(messenger, user, rack, gain, Vec::new());
        gain
    }

    fn param_value(controls: &AudioControls, target: AfxTarget, gain: SlotKey) -> f32 {
        controls
            .effects(target)
            .unwrap()
            .iter()
            .find(|info| info.id == gain)
            .unwrap()
            .params()
            .find(|param| param.spec.key == Gain::DESIRED_MULTIPLIER)
            .unwrap()
            .value
    }

    fn effect_names(controls: &AudioControls, target: AfxTarget) -> Vec<&'static str> {
        controls
            .effects(target)
            .unwrap()
            .iter()
            .map(|info| info.name)
            .collect()
    }

    /// The slider's path: a set on an attached user must land on the live rack.
    #[test]
    fn set_volume_reaches_the_attached_gain() {
        let (messenger, user) = ids();
        let target = AfxTarget::User { messenger, user };
        let mut controls = AudioControls::default();
        let gain = attach_user_with_gain(&mut controls, messenger, user, 1.0);

        controls.set_user_volume(messenger, user, 0.25);
        assert_eq!(param_value(&controls, target, gain), 0.25);
        assert_eq!(controls.user_volume(messenger, user), 0.25);
    }

    /// A volume chosen before the user's channel exists (or between channels)
    /// must seed the gain at attach time.
    #[test]
    fn attach_seeds_gain_with_stored_volume() {
        let (messenger, user) = ids();
        let target = AfxTarget::User { messenger, user };
        let mut controls = AudioControls::default();
        controls.set_user_volume(messenger, user, 1.5);

        let gain = attach_user_with_gain(&mut controls, messenger, user, 1.0);
        assert_eq!(param_value(&controls, target, gain), 1.5);
    }

    /// Detach drops the live handles but the stored volume survives for the
    /// user's next channel.
    #[test]
    fn detach_keeps_volume_for_the_next_channel() {
        let (messenger, user) = ids();
        let target = AfxTarget::User { messenger, user };
        let mut controls = AudioControls::default();
        attach_user_with_gain(&mut controls, messenger, user, 1.0);
        controls.set_user_volume(messenger, user, 0.5);

        controls.detach_user(messenger, user);
        assert!(!controls.has_channel(target));
        assert_eq!(controls.user_volume(messenger, user), 0.5);

        let gain = attach_user_with_gain(&mut controls, messenger, user, 1.0);
        assert_eq!(param_value(&controls, target, gain), 0.5);
    }

    /// The "+ afx" path edits the rack frontend and queues live backend edits.
    #[test]
    fn afx_ops_edit_the_live_channel() {
        let (messenger, user) = ids();
        let target = AfxTarget::User { messenger, user };
        let mut controls = AudioControls::default();
        attach_user_with_gain(&mut controls, messenger, user, 1.0);

        controls.apply(AudioOp::AddEffect {
            target,
            effect: EffectsRack::available_effects()
                .iter()
                .copied()
                .find(|effect| effect.name() == "Compressor")
                .unwrap(),
        });
        let effects = controls.effects(target).unwrap();
        assert_eq!(effects.len(), 2);
        let compressor = effects
            .iter()
            .find(|effect| effect.name == "Compressor")
            .unwrap();
        assert!(
            compressor
                .params()
                .all(|param| param.value == param.spec.default)
        );
        let (compressor_id, threshold) = (compressor.id, compressor.specs[0].key);

        controls.apply(AudioOp::SetEffectParam {
            target,
            effect: compressor_id,
            param: threshold,
            value: -30.0,
        });
        let compressor = controls
            .effects(target)
            .unwrap()
            .into_iter()
            .find(|effect| effect.name == "Compressor")
            .unwrap();
        assert_eq!(compressor.values[0], -30.0);

        controls.apply(AudioOp::RemoveEffect {
            target,
            id: compressor_id,
        });
        assert_eq!(effect_names(&controls, target), vec!["Gain"]);
    }

    #[test]
    fn removed_slider_gain_is_reinserted_on_next_volume_change() {
        let (messenger, user) = ids();
        let target = AfxTarget::User { messenger, user };
        let mut controls = AudioControls::default();
        let gain = attach_user_with_gain(&mut controls, messenger, user, 1.0);

        controls.apply(AudioOp::RemoveEffect { target, id: gain });
        controls.set_user_volume(messenger, user, 0.3);

        let effects = controls.effects(target).unwrap();
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0].name, "Gain");
        assert_eq!(effects[0].values[0], 0.3);
    }

    #[test]
    fn deafen_toggles_all_output_mutes_and_seeds_new_ones() {
        let mut controls = AudioControls::default();
        let first = MuteSwitch::default();
        let (messenger, user) = ids();
        controls.attach_output_mute(messenger, Some(user), first.clone());

        controls.set_deafened(true);
        assert!(first.get());

        let second = MuteSwitch::default();
        controls.attach_output_mute(messenger, None, second.clone());
        assert!(second.get());

        controls.set_deafened(false);
        assert!(!first.get());
        assert!(!second.get());
    }

    #[test]
    fn detach_user_removes_its_output_mute() {
        let (messenger, user) = ids();
        let mut controls = AudioControls::default();
        let mute = MuteSwitch::default();
        controls.attach_output_mute(messenger, Some(user), mute.clone());

        controls.detach_user(messenger, user);
        controls.set_deafened(true);

        assert!(!mute.get());
    }

    #[test]
    fn global_mute_reaches_every_live_microphone() {
        let (messenger, _) = ids();
        let mut controls = AudioControls::default();
        let mut switches = Vec::new();
        for _ in 0..2 {
            let (mut rack, _backend) = EffectsRack::new();
            let mute = Mute::new();
            let switch = mute.switch();
            let mute_id = rack.insert(Box::new(mute));
            controls.attach_mic(messenger, rack, switch.clone(), vec![mute_id]);
            switches.push(switch);
        }

        controls.set_mic_muted(true);
        assert!(switches.iter().all(MuteSwitch::get));
        controls.set_mic_muted(false);
        assert!(switches.iter().all(|switch| !switch.get()));
    }

    #[test]
    fn hidden_effects_are_not_removed_by_afx_ops() {
        let (mut rack, _backend) = EffectsRack::new();
        let mute = Mute::new();
        let switch = mute.switch();
        let mute_id = rack.insert(Box::new(mute));
        let mut controls = AudioControls::default();
        let (messenger, _) = ids();
        controls.attach_mic(messenger, rack, switch, vec![mute_id]);

        controls.apply(AudioOp::RemoveEffect {
            target: AfxTarget::Mic { messenger },
            id: mute_id,
        });
        assert!(controls.is_hidden_effect(AfxTarget::Mic { messenger }, mute_id));
        // Still in the live rack, just never listed for the afx sheet.
        assert!(
            controls
                .mics
                .get(&messenger)
                .and_then(|mics| mics.last())
                .is_some_and(|mic| mic.channel.rack.contains(mute_id))
        );
        assert!(
            controls
                .effects(AfxTarget::Mic { messenger })
                .unwrap()
                .is_empty()
        );
    }

    /// Ops aimed at a channel that is gone must be a no-op, not a panic.
    #[test]
    fn ops_on_missing_channels_are_ignored() {
        let (messenger, user) = ids();
        let mut controls = AudioControls::default();
        controls.apply(AudioOp::AddEffect {
            target: AfxTarget::Mic { messenger },
            effect: EffectsRack::available_effects()[0],
        });
        controls.apply(AudioOp::SetUserVolume {
            messenger,
            user,
            volume: 0.1,
        });
        // The volume is still remembered for when the channel appears.
        assert_eq!(controls.user_volume(messenger, user), 0.1);
    }
}
