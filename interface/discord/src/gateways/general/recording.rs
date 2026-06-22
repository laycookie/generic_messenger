//! Per-fetch event recording. While a REST call is in flight, gateway
//! dispatches that could affect its returned snapshot are appended to a
//! shared buffer; once REST returns, the caller drains its slice of the
//! buffer and folds those deltas into the snapshot before handing it to
//! the UI.
//!
//! Pay-for-use: the event handler only pushes when `recording_refs` is
//! non-zero, so an idle session pays one atomic load per dispatch and
//! nothing else. Under sustained overlapping fetches the buffer is kept
//! trim by tracking each active window's start sequence and dropping
//! front slots that no window still references. See
//! `crate/messenger_interface/docs/races.md`.

use std::{collections::VecDeque, sync::atomic::Ordering};

use crate::api_types::{self, Message, Reaction, SNOWFLAKE};

use super::General;

/// Events relevant to in-flight REST snapshots. Extend as more handlers
/// are wired up (channel/guild/relationship updates, etc.).
#[derive(Clone)]
pub enum RecordedEvent {
    MessageUpdated {
        channel_id: SNOWFLAKE,
        message: Message,
    },
    MessageDeleted {
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
    },
    ReactionAdded {
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
        emoji: api_types::Emoji,
        is_self: bool,
    },
    ReactionRemoved {
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
        emoji: api_types::Emoji,
        is_self: bool,
    },
}

/// Mutex-protected state for the recording buffer. Slot at deque index
/// `i` corresponds to absolute sequence number `front_seq + i`; popping
/// from the front increments `front_seq` so window sequence numbers
/// remain stable as the deque trims.
pub struct RecordingState {
    buffer: VecDeque<RecordedEvent>,
    front_seq: u64,
    /// Start sequence of every currently-open `RecordingWindow`. The
    /// minimum bounds what can be dropped from the front of `buffer`.
    /// Duplicates are allowed (windows opened with no events between).
    active_starts: Vec<u64>,
}

impl Default for RecordingState {
    fn default() -> Self {
        Self {
            buffer: VecDeque::new(),
            front_seq: 0,
            active_starts: Vec::new(),
        }
    }
}

/// RAII guard for a single in-flight REST call. Constructing it
/// registers a start sequence; `take` returns the events recorded since
/// that point. `Drop` deregisters the window and trims any front slots
/// that no remaining window references — so cleanup is guaranteed even
/// if the caller bubbles an error before reaching `take`.
pub struct RecordingWindow<'a> {
    gateway: &'a General,
    start_seq: u64,
}

impl<'a> RecordingWindow<'a> {
    /// Clone this window's events from the shared buffer. Non-destructive:
    /// concurrent windows that opened later still see their own slices.
    pub fn take(&self) -> Vec<RecordedEvent> {
        let state = self.gateway.recorded.lock().unwrap();
        let offset = (self.start_seq - state.front_seq) as usize;
        state.buffer.iter().skip(offset).cloned().collect()
    }
}

impl Drop for RecordingWindow<'_> {
    fn drop(&mut self) {
        let mut state = self.gateway.recorded.lock().unwrap();

        // Remove this window's entry from active_starts (swap_remove is
        // fine — order doesn't matter, only the minimum does).
        if let Some(pos) = state
            .active_starts
            .iter()
            .position(|&s| s == self.start_seq)
        {
            state.active_starts.swap_remove(pos);
        }

        // Trim front slots no remaining window references. If no windows
        // remain, drop everything.
        if state.active_starts.is_empty() {
            state.front_seq += state.buffer.len() as u64;
            state.buffer.clear();
        } else {
            let new_front = *state.active_starts.iter().min().unwrap();
            let drain_count = (new_front - state.front_seq) as usize;
            state.buffer.drain(..drain_count);
            state.front_seq = new_front;
        }

        self.gateway.recording_refs.fetch_sub(1, Ordering::AcqRel);
    }
}

impl General {
    /// Open a recording window. Registers a start sequence under the
    /// state lock so it's atomic with respect to concurrent pushes.
    pub fn start_recording(&self) -> RecordingWindow<'_> {
        let mut state = self.recorded.lock().unwrap();
        let start_seq = state.front_seq + state.buffer.len() as u64;
        state.active_starts.push(start_seq);
        // Bump under the lock so a concurrent `maybe_record` that
        // already saw counter == 0 cannot race past us.
        self.recording_refs.fetch_add(1, Ordering::AcqRel);
        drop(state);
        RecordingWindow {
            gateway: self,
            start_seq,
        }
    }

    /// Called from gateway event handlers. Cheap no-op when no fetch is
    /// active: one atomic load, no lock, no allocation.
    pub(super) fn maybe_record(&self, event: impl FnOnce() -> RecordedEvent) {
        if self.recording_refs.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut state = self.recorded.lock().unwrap();
        // Re-check under the lock: a concurrent `Drop` could have
        // emptied `active_starts` between the atomic load and now.
        if !state.active_starts.is_empty() {
            state.buffer.push_back(event());
        }
    }
}

/// Fold recorded events into a REST-fetched message list scoped to one
/// channel. Events for other channels or unrelated message ids are
/// ignored. Events are applied in capture order, so later events win
/// over earlier ones for the same field.
pub fn apply_to_messages(
    mut messages: Vec<Message>,
    events: Vec<RecordedEvent>,
    channel_id: SNOWFLAKE,
) -> Vec<Message> {
    for event in events {
        match event {
            RecordedEvent::MessageUpdated {
                channel_id: c,
                message,
            } if c == channel_id => {
                if let Some(slot) = messages.iter_mut().find(|m| m.id == message.id) {
                    let prev_reactions = slot.reactions.take();
                    *slot = message;
                    // MESSAGE_UPDATE payloads frequently omit `reactions`;
                    // absent means "unchanged", not "cleared" — keep the
                    // REST snapshot's reactions in that case.
                    if slot.reactions.is_none() {
                        slot.reactions = prev_reactions;
                    }
                }
            }
            RecordedEvent::MessageDeleted {
                channel_id: c,
                message_id,
            } if c == channel_id => {
                messages.retain(|m| m.id != message_id);
            }
            RecordedEvent::ReactionAdded {
                channel_id: c,
                message_id,
                emoji,
                is_self,
            } if c == channel_id => {
                if let Some(msg) = messages.iter_mut().find(|m| m.id == message_id) {
                    let reactions = msg.reactions.get_or_insert_with(Vec::new);
                    if let Some(existing) =
                        reactions.iter_mut().find(|r| reaction_matches(r, &emoji))
                    {
                        existing.count = existing.count.saturating_add(1);
                        existing.me |= is_self;
                    } else {
                        reactions.push(Reaction {
                            count: 1,
                            emoji,
                            me: is_self,
                        });
                    }
                }
            }
            RecordedEvent::ReactionRemoved {
                channel_id: c,
                message_id,
                emoji,
                is_self,
            } if c == channel_id => {
                if let Some(msg) = messages.iter_mut().find(|m| m.id == message_id) {
                    if let Some(reactions) = msg.reactions.as_mut() {
                        if let Some(idx) =
                            reactions.iter().position(|r| reaction_matches(r, &emoji))
                        {
                            let r = &mut reactions[idx];
                            r.count = r.count.saturating_sub(1);
                            if is_self {
                                r.me = false;
                            }
                            if r.count == 0 {
                                reactions.remove(idx);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    messages
}

fn reaction_matches(reaction: &Reaction, emoji: &api_types::Emoji) -> bool {
    reaction.emoji.id == emoji.id && reaction.emoji.name == emoji.name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_types::{Emoji, User};

    fn message(id: SNOWFLAKE, content: &str, reactions: Option<Vec<Reaction>>) -> Message {
        Message {
            author: User {
                avatar: None,
                id: 1,
                username: "user".to_string(),
            },
            channel_id: 10,
            content: content.to_string(),
            edited_timestamp: None,
            id,
            reactions,
            sticker_items: None,
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
        }
    }

    fn reaction(name: &str, count: u32, me: bool) -> Reaction {
        Reaction {
            count,
            emoji: Emoji {
                id: None,
                name: name.to_string(),
            },
            me,
        }
    }

    #[test]
    fn update_without_reactions_preserves_snapshot_reactions() {
        let snapshot = vec![message(1, "old", Some(vec![reaction("👍", 2, true)]))];
        let events = vec![RecordedEvent::MessageUpdated {
            channel_id: 10,
            message: message(1, "new", None),
        }];

        let merged = apply_to_messages(snapshot, events, 10);
        assert_eq!(merged[0].content, "new");
        let reactions = merged[0].reactions.as_ref().expect("reactions kept");
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].count, 2);
    }

    #[test]
    fn update_with_reactions_replaces_snapshot_reactions() {
        let snapshot = vec![message(1, "old", Some(vec![reaction("👍", 2, true)]))];
        let events = vec![RecordedEvent::MessageUpdated {
            channel_id: 10,
            message: message(1, "new", Some(vec![reaction("🎉", 1, false)])),
        }];

        let merged = apply_to_messages(snapshot, events, 10);
        let reactions = merged[0].reactions.as_ref().unwrap();
        assert_eq!(reactions[0].emoji.name, "🎉");
    }

    #[test]
    fn delete_removes_and_other_channels_ignored() {
        let snapshot = vec![message(1, "a", None), message(2, "b", None)];
        let events = vec![
            RecordedEvent::MessageDeleted {
                channel_id: 10,
                message_id: 1,
            },
            RecordedEvent::MessageDeleted {
                channel_id: 99, // different channel — must be ignored
                message_id: 2,
            },
        ];

        let merged = apply_to_messages(snapshot, events, 10);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, 2);
    }

    #[test]
    fn reaction_add_and_remove_roundtrip() {
        let snapshot = vec![message(1, "a", None)];
        let emoji = Emoji {
            id: None,
            name: "👍".to_string(),
        };
        let events = vec![
            RecordedEvent::ReactionAdded {
                channel_id: 10,
                message_id: 1,
                emoji: emoji.clone(),
                is_self: true,
            },
            RecordedEvent::ReactionRemoved {
                channel_id: 10,
                message_id: 1,
                emoji,
                is_self: true,
            },
        ];

        let merged = apply_to_messages(snapshot, events, 10);
        // Count dropped to zero → reaction entry removed entirely.
        assert!(merged[0].reactions.as_ref().unwrap().is_empty());
    }
}
