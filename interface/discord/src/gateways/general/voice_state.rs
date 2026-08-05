//! Voice-presence roster tracking driven by the *main* gateway.
//!
//! Discord broadcasts `VOICE_STATE_UPDATE` (who is in which voice channel) over
//! the general gateway, not the voice gateway — the latter only handles a single
//! call's media transport. [`VoiceRoster`] owns cached state and participants;
//! projection helpers emit [`VoiceEvent`]s after each cached transition. The
//! general dispatch loop in [`super::dispatcher`] drives them.

use dashmap::DashMap;
use messenger_interface::{
    interface::{VideoBroadcastKind, VideoEvent, VoiceEvent},
    types::{Identifier, User as GlobalUser, VideoBroadcast, VoiceState},
};
use tracing::warn;

use super::{events::EventContext, payloads::VoiceStatePayload};
use crate::{api, downloaders::CdnImage};

/// Minimal persistent form of a live Discord voice state.
///
/// `VoiceStatePayload` is a wire object containing member data and fields the
/// adapter never reads after dispatch. Keeping only control/routing fields
/// avoids storing that payload beside its duplicated `user_id` map key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CachedVoiceState {
    pub(crate) guild_id: Option<api::SNOWFLAKE>,
    pub(crate) channel_id: api::SNOWFLAKE,
    pub(crate) self_mute: bool,
    pub(crate) self_deaf: bool,
    pub(crate) self_video: bool,
    pub(crate) self_stream: bool,
}

impl CachedVoiceState {
    fn from_payload(payload: &VoiceStatePayload, channel_id: api::SNOWFLAKE) -> Self {
        Self {
            guild_id: payload.guild_id,
            channel_id,
            self_mute: payload.self_mute,
            self_deaf: payload.self_deaf,
            self_video: payload.self_video,
            self_stream: payload.self_stream.unwrap_or(false),
        }
    }
}

/// Result of one authoritative voice-state cache update. Values are small,
/// stack-only snapshots discarded after dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct VoiceStateTransition {
    pub(crate) user_id: api::SNOWFLAKE,
    pub(crate) previous: Option<CachedVoiceState>,
    pub(crate) current: Option<CachedVoiceState>,
}

/// Sole owner of voice-presence state and per-channel participant rosters.
/// Keeping both maps here makes move/leave eviction an internal invariant.
#[derive(Default)]
pub(crate) struct VoiceRoster {
    states: DashMap<api::SNOWFLAKE, CachedVoiceState>,
    participants: DashMap<api::SNOWFLAKE, Vec<Identifier<GlobalUser>>>,
}

impl VoiceRoster {
    pub(crate) fn clear(&self) {
        self.states.clear();
        self.participants.clear();
    }

    pub(crate) fn remove_channel(&self, channel_id: api::SNOWFLAKE) {
        self.states
            .retain(|_, state| state.channel_id != channel_id);
        self.participants.remove(&channel_id);
    }

    pub(crate) fn remove_guild(&self, guild_id: api::SNOWFLAKE) {
        let channel_ids = self
            .states
            .iter()
            .filter(|state| state.guild_id == Some(guild_id))
            .map(|state| state.channel_id)
            .collect::<Vec<_>>();
        self.states
            .retain(|_, state| state.guild_id != Some(guild_id));
        for channel_id in channel_ids {
            self.participants.remove(&channel_id);
        }
    }

    pub(crate) fn state(&self, user_id: api::SNOWFLAKE) -> Option<CachedVoiceState> {
        self.states.get(&user_id).map(|state| *state)
    }

    pub(crate) fn states_in_channel(
        &self,
        channel_id: api::SNOWFLAKE,
    ) -> impl Iterator<Item = (api::SNOWFLAKE, CachedVoiceState)> + '_ {
        self.states.iter().filter_map(move |state| {
            (state.channel_id == channel_id).then(|| (*state.key(), *state.value()))
        })
    }

    pub(crate) fn participants(&self, channel_id: api::SNOWFLAKE) -> Vec<Identifier<GlobalUser>> {
        self.participants
            .get(&channel_id)
            .map(|participants| participants.clone())
            .unwrap_or_default()
    }

    fn apply(&self, payload: &VoiceStatePayload) -> VoiceStateTransition {
        let user_id = payload.user_id;
        let current = payload
            .channel_id
            .map(|channel_id| CachedVoiceState::from_payload(payload, channel_id));
        let previous = match current {
            Some(current) => self.states.insert(user_id, current),
            None => self.states.remove(&user_id).map(|(_, state)| state),
        };

        if let Some(previous) = previous
            && Some(previous.channel_id) != current.map(|state| state.channel_id)
        {
            self.evict_participant(previous.channel_id, user_id);
        }

        VoiceStateTransition {
            user_id,
            previous,
            current,
        }
    }

    fn evict_participant(&self, channel_id: api::SNOWFLAKE, user_id: api::SNOWFLAKE) {
        if let Some(mut entry) = self.participants.get_mut(&channel_id) {
            entry.retain(|user| *user.id() != user_id);
            if entry.is_empty() {
                drop(entry);
                self.participants.remove(&channel_id);
            }
        }
    }

    fn update_known_participant(
        &self,
        channel_id: api::SNOWFLAKE,
        user_id: api::SNOWFLAKE,
        call_state: VoiceState,
    ) -> Option<Identifier<GlobalUser>> {
        self.participants
            .get_mut(&channel_id)
            .and_then(|mut participants| {
                participants
                    .iter_mut()
                    .find(|user| *user.id() == user_id)
                    .map(|user| {
                        user.voice = Some(call_state);
                        user.clone()
                    })
            })
    }

    fn upsert_participant(
        &self,
        channel_id: api::SNOWFLAKE,
        user_id: api::SNOWFLAKE,
        user: Identifier<GlobalUser>,
    ) {
        let mut participants = self.participants.entry(channel_id).or_default();
        participants.retain(|participant| *participant.id() != user_id);
        participants.push(user);
    }
}

impl EventContext<'_> {
    pub(super) fn advertise_remote_video(
        &self,
        user_id: api::SNOWFLAKE,
        channel_id: api::SNOWFLAKE,
        kind: VideoBroadcastKind,
    ) {
        let (id, metadata) = self
            .media()
            .register_participant_video(user_id, channel_id, kind);
        self.events()
            .submit_video(VideoEvent::VideoStreamAvailable { id, metadata });
    }

    pub(super) async fn apply_voice_state_update(
        &self,
        mut voice_state: VoiceStatePayload,
        member_user: Option<api::User>,
    ) -> VoiceStateTransition {
        let member = voice_state.member.take();
        let call_state = {
            // Server-applied and self-applied states are folded together: the
            // UI only cares whether the user can hear/be heard, not who flipped it.
            let mut video = VideoBroadcast::empty();
            video.set(VideoBroadcast::CAMERA, voice_state.self_video);
            // `self_stream` marks a "Go Live" screen share; Discord omits the
            // field entirely when it was never toggled on.
            video.set(
                VideoBroadcast::SCREENSHARE,
                voice_state.self_stream.unwrap_or(false),
            );
            VoiceState {
                muted: voice_state.mute || voice_state.self_mute,
                deafened: voice_state.deaf || voice_state.self_deaf,
                video,
            }
        };
        let transition = self.cache().voice.apply(&voice_state);
        let user_id = transition.user_id;
        let Some(current) = transition.current else {
            self.events()
                .submit_voice(VoiceEvent::ParticipantLeft { user_id });
            return transition;
        };
        let channel_id = current.channel_id;

        let Some(user) = member.map(|m| m.user).or(member_user) else {
            // Flag-only updates (mute/deafen toggles) can arrive without
            // member data, and DM-call states never carry it. When the user
            // is already on the roster we still know who they are: refresh
            // the flags in place and re-emit instead of dropping the update.
            let known = self
                .cache()
                .voice
                .update_known_participant(channel_id, user_id, call_state);
            match known {
                Some(user) => {
                    self.events().submit_voice(VoiceEvent::ParticipantJoined {
                        room: Identifier::new(channel_id, ()),
                        user,
                    });
                }
                None => warn!("Voice state for user {user_id} is missing member data"),
            }
            return transition;
        };

        let avatar = user.avatar.clone();
        let mut user_identifier = Identifier::new(
            user.id,
            GlobalUser {
                name: user
                    .global_name
                    .clone()
                    .unwrap_or_else(|| user.username.clone()),
                tag: Some(user.username),
                icon: None,
                status: None,
                voice: Some(call_state),
            },
        );

        self.cache()
            .voice
            .upsert_participant(channel_id, user_id, user_identifier.clone());

        self.events().submit_voice(VoiceEvent::ParticipantJoined {
            room: Identifier::new(channel_id, ()),
            user: user_identifier.clone(),
        });

        // Identity and voice presence are authoritative gateway state; avatar
        // I/O is optional decoration. Publish the participant before the first
        // await so cancellation or a slow CDN can never create a partial roster.
        let Some(avatar) = avatar else {
            return transition;
        };
        let Ok(icon) = CdnImage::avatar(user_id, &avatar).fetch().await else {
            return transition;
        };
        if self.cache().voice.state(user_id) != Some(current) {
            return transition;
        }

        user_identifier.icon = Some(icon);
        self.cache()
            .voice
            .upsert_participant(channel_id, user_id, user_identifier.clone());
        self.events().submit_voice(VoiceEvent::ParticipantJoined {
            room: Identifier::new(channel_id, ()),
            user: user_identifier,
        });
        transition
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(user_id: api::SNOWFLAKE, channel_id: Option<api::SNOWFLAKE>) -> VoiceStatePayload {
        VoiceStatePayload {
            guild_id: Some(1),
            channel_id,
            lobby_id: None,
            user_id,
            member: None,
            session_id: String::new(),
            deaf: false,
            mute: false,
            self_deaf: false,
            self_mute: false,
            self_stream: None,
            self_video: false,
            suppress: false,
            discoverable: None,
            user_volume: None,
        }
    }

    #[test]
    fn transition_reuses_one_authoritative_state_entry() {
        let roster = VoiceRoster::default();
        roster.upsert_participant(
            10,
            7,
            Identifier::new(
                7,
                GlobalUser {
                    name: "user".into(),
                    tag: None,
                    icon: None,
                    status: None,
                    voice: None,
                },
            ),
        );

        let joined = roster.apply(&payload(7, Some(10)));
        let moved = roster.apply(&payload(7, Some(20)));

        assert_eq!(joined.previous, None);
        assert_eq!(joined.current.unwrap().channel_id, 10);
        assert_eq!(moved.previous.unwrap().channel_id, 10);
        assert_eq!(moved.current.unwrap().channel_id, 20);
        assert_eq!(roster.state(7).unwrap().channel_id, 20);
        assert_eq!(roster.states.len(), 1);
        assert!(roster.participants(10).is_empty());
    }

    #[test]
    fn leave_removes_authoritative_state() {
        let roster = VoiceRoster::default();
        roster.apply(&payload(7, Some(10)));

        let left = roster.apply(&payload(7, None));

        assert_eq!(left.previous.unwrap().channel_id, 10);
        assert_eq!(left.current, None);
        assert_eq!(roster.state(7), None);
    }

    #[test]
    fn cached_state_is_smaller_than_wire_payload() {
        assert!(std::mem::size_of::<CachedVoiceState>() < std::mem::size_of::<VoiceStatePayload>());
    }
}
