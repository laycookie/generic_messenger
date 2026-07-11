//! Voice-presence roster tracking driven by the *main* gateway.
//!
//! Discord broadcasts `VOICE_STATE_UPDATE` (who is in which voice channel) over
//! the general gateway, not the voice gateway — the latter only handles a single
//! call's media transport. These helpers maintain `voice_states` /
//! `voice_participants` and emit [`VoiceEvent`]s; the general dispatch loop in
//! [`super::events`] calls them from the relevant event arms.

use messenger_interface::{
    interface::VoiceEvent,
    types::{Identifier, User as GlobalUser, VoiceState},
};
use tracing::warn;

use super::payloads::VoiceStatePayload;
use crate::{InnerDiscord, UnitStruct, api_types, downloaders::CdnImage};

impl<T: UnitStruct> InnerDiscord<T> {
    /// Evict `user_id` from the per-channel voice roster for `channel_id`,
    /// dropping the channel entry entirely if it becomes empty. Done as a
    /// helper so the leave path and the channel-switch path stay consistent.
    fn evict_voice_participant(
        &self,
        channel_id: api_types::SNOWFLAKE,
        user_id: api_types::SNOWFLAKE,
    ) {
        if let Some(mut entry) = self.voice_participants.get_mut(&channel_id) {
            entry.retain(|u| *u.id() != user_id);
            if entry.is_empty() {
                drop(entry);
                self.voice_participants.remove(&channel_id);
            }
        }
    }

    pub(super) async fn emit_voice_state_participant(
        &self,
        user_id: api_types::SNOWFLAKE,
        mut voice_state: VoiceStatePayload,
        member_user: Option<api_types::User>,
    ) {
        let Some(channel_id) = voice_state.channel_id else {
            // Leave: remove voice_state and evict from the roster of whatever
            // channel the user was last in.
            if let Some((_, old)) = self.voice_states.remove(&user_id)
                && let Some(old_channel_id) = old.channel_id
            {
                self.evict_voice_participant(old_channel_id, user_id);
            }
            let _ = self
                .voice_events
                .force_push(VoiceEvent::ParticipantLeft { user_id });
            return;
        };

        let member = voice_state.member.take();
        // Server-applied and self-applied states are folded together: the UI
        // only cares whether the user can hear/be heard, not who flipped it.
        let call_state = VoiceState {
            muted: voice_state.mute || voice_state.self_mute,
            deafened: voice_state.deaf || voice_state.self_deaf,
        };
        // Capture the previous channel (if any) so we can evict the user from
        // its roster after the insert overwrites the state.
        let prev_channel_id = self
            .voice_states
            .insert(user_id, voice_state)
            .and_then(|old| old.channel_id);
        if let Some(prev) = prev_channel_id
            && prev != channel_id
        {
            self.evict_voice_participant(prev, user_id);
        }

        let Some(user) = member.map(|m| m.user).or(member_user) else {
            // Flag-only updates (mute/deafen toggles) can arrive without
            // member data, and DM-call states never carry it. When the user
            // is already on the roster we still know who they are: refresh
            // the flags in place and re-emit instead of dropping the update.
            let known = self
                .voice_participants
                .get_mut(&channel_id)
                .and_then(|mut entry| {
                    entry.iter_mut().find(|u| *u.id() == user_id).map(|user| {
                        user.voice = Some(call_state);
                        user.clone()
                    })
                });
            match known {
                Some(user) => {
                    let _ = self.voice_events.force_push(VoiceEvent::ParticipantJoined {
                        room: Identifier::new(channel_id, ()),
                        user,
                    });
                }
                None => warn!("Voice state for user {user_id} is missing member data"),
            }
            return;
        };

        let icon = match &user.avatar {
            Some(hash) => CdnImage::avatar(user.id, hash).fetch().await.ok(),
            None => None,
        };

        let user_identifier = Identifier::new(
            user.id,
            GlobalUser {
                name: user
                    .global_name
                    .clone()
                    .unwrap_or_else(|| user.username.clone()),
                tag: Some(user.username),
                icon,
                status: self.interface_user_status(user.id),
                voice: Some(call_state),
            },
        );

        {
            let mut entry = self.voice_participants.entry(channel_id).or_default();
            entry.retain(|u| *u.id() != user_id);
            entry.push(user_identifier.clone());
        }

        let _ = self.voice_events.force_push(VoiceEvent::ParticipantJoined {
            room: Identifier::new(channel_id, ()),
            user: user_identifier,
        });
    }
}
