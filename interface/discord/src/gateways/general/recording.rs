//! Per-fetch event recording. While a REST call is in flight, gateway
//! dispatches that could affect its returned snapshot are appended to a
//! shared buffer; once REST returns, the caller closes its window and folds
//! captured deltas into the snapshot before handing it to the UI.
//!
//! Pay-for-use: the event handler only pushes while at least one recording
//! window is active, so an idle session pays one atomic load per dispatch and
//! nothing else. Under sustained overlapping fetches the buffer is kept
//! trim by tracking each active window's start sequence and dropping
//! front slots that no window still references. See
//! `crate/messenger_interface/docs/races.md`.

use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use crate::api::{self, Message, Reaction, SNOWFLAKE};

/// Events relevant to in-flight REST snapshots. Extend as more handlers
/// are wired up (channel/guild/relationship updates, etc.).
#[derive(Clone)]
pub enum RecordedEvent {
    MessageCreated {
        channel_id: SNOWFLAKE,
        message: Message,
    },
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
        emoji: api::Emoji,
        is_self: bool,
    },
    ReactionRemoved {
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
        emoji: api::Emoji,
        is_self: bool,
    },
}

/// Per-fetch gateway event recorder.
///
/// Owns both the cheap active-window gate and shared event buffer so callers
/// cannot observe or mutate partially-coordinated recording state.
#[derive(Default)]
pub struct Recorder {
    active_windows: AtomicUsize,
    state: Mutex<RecordingState>,
}

/// Mutex-protected state for the recording buffer. Slot at deque index
/// `i` corresponds to absolute sequence number `front_seq + i`; popping
/// from the front increments `front_seq` so window sequence numbers
/// remain stable as the deque trims.
#[derive(Default)]
struct RecordingState {
    buffer: VecDeque<RecordedEvent>,
    front_seq: u64,
    /// Start sequence of every currently-open `RecordingWindow`. The
    /// minimum bounds what can be dropped from the front of `buffer`.
    /// Duplicates are allowed (windows opened with no events between).
    active_starts: Vec<u64>,
}

/// RAII guard for a single in-flight REST call. Constructing it registers a
/// start sequence; `apply_to_messages` atomically closes the window before
/// folding its events into the REST snapshot. `Drop` closes without applying,
/// guaranteeing cleanup when the caller returns early.
pub struct RecordingWindow<'a> {
    recorder: &'a Recorder,
    start_seq: Option<u64>,
}

impl<'a> RecordingWindow<'a> {
    /// Close this recording window and fold its captured gateway events into
    /// a REST-fetched message list. Closing and capturing share one lock
    /// acquisition, giving gateway dispatch a clean before/after boundary.
    /// Events apply in capture order; other channels are ignored.
    pub fn apply_to_messages(
        mut self,
        mut messages: Vec<Message>,
        channel_id: SNOWFLAKE,
        include_created: bool,
    ) -> Vec<Message> {
        let events = self.close(true);
        for event in events {
            match event {
                RecordedEvent::MessageCreated {
                    channel_id: c,
                    message,
                } if include_created
                    && c == channel_id
                    && !messages.iter().any(|cached| cached.id == message.id) =>
                {
                    messages.push(message);
                    // Discord REST pages are newest-first. Preserve that
                    // contract before the caller optionally reverses them.
                    messages.sort_by_key(|message| std::cmp::Reverse(message.id));
                }
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
                        if let Some(existing) = reactions.iter_mut().find(|r| r.emoji == emoji) {
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
                    if let Some(msg) = messages.iter_mut().find(|m| m.id == message_id)
                        && let Some(reactions) = msg.reactions.as_mut()
                        && let Some(idx) = reactions
                            .iter()
                            .position(|reaction| reaction.emoji == emoji)
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
                _ => {}
            }
        }
        messages
    }

    fn close(&mut self, capture_events: bool) -> Vec<RecordedEvent> {
        let Some(start_seq) = self.start_seq.take() else {
            return Vec::new();
        };
        let mut state = self.recorder.state.lock().unwrap();

        // Clone before trimming. Other active windows may still reference
        // these slots, so this window cannot drain them directly.
        let events = if capture_events {
            let offset = (start_seq - state.front_seq) as usize;
            state.buffer.iter().skip(offset).cloned().collect()
        } else {
            Vec::new()
        };

        // Remove this window's entry from active_starts (swap_remove is
        // fine — order doesn't matter, only the minimum does).
        if let Some(pos) = state.active_starts.iter().position(|&s| s == start_seq) {
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

        self.recorder.active_windows.fetch_sub(1, Ordering::AcqRel);

        events
    }
}

impl Drop for RecordingWindow<'_> {
    fn drop(&mut self) {
        let _ = self.close(false);
    }
}

impl Recorder {
    /// Open a recording window. Registers a start sequence under the
    /// state lock so it's atomic with respect to concurrent pushes.
    pub fn start(&self) -> RecordingWindow<'_> {
        let mut state = self.state.lock().unwrap();
        let start_seq = state.front_seq + state.buffer.len() as u64;
        state.active_starts.push(start_seq);
        // Bump under the lock so a concurrent `maybe_record` that
        // already saw counter == 0 cannot race past us.
        self.active_windows.fetch_add(1, Ordering::AcqRel);
        drop(state);
        RecordingWindow {
            recorder: self,
            start_seq: Some(start_seq),
        }
    }

    /// Called from gateway event handlers. Cheap no-op when no fetch is
    /// active: one atomic load, no lock, no allocation.
    pub(super) fn maybe_record(&self, event: impl FnOnce() -> RecordedEvent) {
        if self.active_windows.load(Ordering::Acquire) == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap();
        // Re-check under the lock: a concurrent `Drop` could have
        // emptied `active_starts` between the atomic load and now.
        if !state.active_starts.is_empty() {
            state.buffer.push_back(event());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Emoji, User};

    fn apply_to_messages(
        messages: Vec<Message>,
        events: Vec<RecordedEvent>,
        channel_id: SNOWFLAKE,
    ) -> Vec<Message> {
        let recorder = Recorder::default();
        let window = recorder.start();
        for event in events {
            recorder.maybe_record(|| event);
        }
        window.apply_to_messages(messages, channel_id, true)
    }

    fn message(id: SNOWFLAKE, content: &str, reactions: Option<Vec<Reaction>>) -> Message {
        Message {
            author: User {
                avatar: None,
                global_name: None,
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
    fn overlapping_windows_keep_independent_slices_and_trim_after_close() {
        let recorder = Recorder::default();
        let first = recorder.start();
        recorder.maybe_record(|| RecordedEvent::MessageDeleted {
            channel_id: 10,
            message_id: 1,
        });

        let second = recorder.start();
        recorder.maybe_record(|| RecordedEvent::MessageDeleted {
            channel_id: 10,
            message_id: 2,
        });

        let snapshot = vec![message(1, "a", None), message(2, "b", None)];
        let second_merged = second.apply_to_messages(snapshot.clone(), 10, true);
        assert_eq!(second_merged.len(), 1);
        assert_eq!(second_merged[0].id, 1);

        let first_merged = first.apply_to_messages(snapshot.clone(), 10, true);
        assert!(first_merged.is_empty());

        let after_cleanup = recorder.start().apply_to_messages(snapshot, 10, true);
        assert_eq!(after_cleanup.len(), 2);
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

    #[test]
    fn create_is_merged_once_in_newest_first_order() {
        let snapshot = vec![message(2, "existing", None), message(1, "older", None)];
        let created = message(3, "new", None);
        let events = vec![
            RecordedEvent::MessageCreated {
                channel_id: 10,
                message: created.clone(),
            },
            RecordedEvent::MessageCreated {
                channel_id: 10,
                message: created,
            },
        ];

        let merged = apply_to_messages(snapshot, events, 10);
        assert_eq!(
            merged.iter().map(|message| message.id).collect::<Vec<_>>(),
            [3, 2, 1]
        );
    }

    #[test]
    fn create_is_not_added_to_older_page() {
        let recorder = Recorder::default();
        let window = recorder.start();
        recorder.maybe_record(|| RecordedEvent::MessageCreated {
            channel_id: 10,
            message: message(3, "new", None),
        });

        let merged = window.apply_to_messages(vec![message(1, "old", None)], 10, false);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, 1);
    }
}
