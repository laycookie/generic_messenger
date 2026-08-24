use std::{error::Error, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_timer::Delay;
use grammers_client::{
    client::UpdatesConfiguration, message::Message as GrammersMessage, update::Update,
};
use messenger_interface::{
    interface::{Ordering, Text, TextEvent},
    owner::Retained,
    stream::EventFuture,
    types::{Identifier, Message, Place, Revision, RichText, Room},
};
use tracing::warn;

use crate::{
    BoxError, CachedMessage, ChatTarget, MAX_CACHED_MESSAGES_PER_CHAT, TelegramMessenger,
    TelegramPeer, TelegramRoom, TelegramState, TelegramUser, WireMessage, message_error,
    query::{room_identifier, user_identifier},
    telegram_task,
};

const PAGE_SIZE: usize = 50;

enum WireUpdate {
    New(WireMessage),
    Edited(WireMessage),
    Deleted {
        channel_id: Option<i64>,
        message_ids: Vec<i32>,
    },
}

async fn wire_message(
    message: GrammersMessage,
    fallback_peer_ref: Option<grammers_session::types::PeerRef>,
    me: &TelegramUser,
) -> Result<WireMessage, BoxError> {
    let peer_id = message.peer_id();
    let target = ChatTarget::from_peer_id(peer_id)
        .ok_or_else(|| message_error("Telegram message used unresolved self peer"))?;
    let peer_ref = match fallback_peer_ref {
        Some(peer_ref) => peer_ref,
        None => message
            .peer_ref()
            .await?
            .ok_or_else(|| message_error("Telegram message peer reference is unavailable"))?,
    };
    let peer = match message.peer() {
        Some(peer) => TelegramPeer::from_peer(peer),
        None => TelegramPeer::unknown(peer_id)?,
    };
    let author = message
        .sender()
        .map(TelegramUser::from_peer)
        .or_else(|| message.outgoing().then(|| me.clone()));

    Ok(WireMessage {
        target,
        peer_ref,
        peer,
        message_id: message.id(),
        author,
        date: message.date(),
        edit_date: message.edit_date(),
        text: message.text().to_owned(),
    })
}

impl CachedMessage {
    fn new(wire: WireMessage, edited: bool) -> Self {
        let history = if edited {
            vec![Revision {
                at: Some(wire.date),
                text: RichText::default(),
            }]
        } else {
            Vec::new()
        };
        Self { wire, history }
    }

    fn revision(&self) -> Revision {
        Revision {
            at: Some(self.wire.edit_date.unwrap_or(self.wire.date)),
            text: RichText::plain(self.wire.text.clone()),
        }
    }

    fn update(&mut self, wire: WireMessage) -> bool {
        let unchanged = self.wire.edit_date == wire.edit_date && self.wire.text == wire.text;
        if unchanged {
            self.wire = wire;
            return false;
        }

        self.history.push(self.revision());
        self.wire = wire;
        true
    }

    fn identifier(&self, target: ChatTarget) -> Identifier<Message> {
        Identifier::new(
            target.message_id(self.wire.message_id),
            Message {
                content: self.revision(),
                history: self.history.clone(),
                reactions: Vec::new(),
                author: self.wire.author.as_ref().map(user_identifier),
            },
        )
    }
}

impl TelegramState {
    fn remember_room(&mut self, wire: &WireMessage) -> bool {
        let target = wire.target;
        let new_room = !self.rooms.contains_key(&target);
        self.room_targets.insert(target.room_id(), target);
        self.peer_refs.insert(target, wire.peer_ref);
        if let Some(user) = &wire.peer.user {
            self.users.insert(user.external_id, user.clone());
        }
        if let Some(author) = &wire.author
            && author.external_id > 0
        {
            self.users.insert(author.external_id, author.clone());
        }
        let last_activity = wire.edit_date.unwrap_or(wire.date).timestamp();
        self.rooms
            .entry(target)
            .and_modify(|room| {
                room.peer = wire.peer.clone();
                room.last_activity = room.last_activity.max(last_activity);
            })
            .or_insert_with(|| TelegramRoom {
                target,
                peer: wire.peer.clone(),
                last_activity,
            });
        new_room
    }

    fn push_new_room_event(
        &mut self,
        target: ChatTarget,
        initial_message: Option<Identifier<Message>>,
    ) {
        let Some(room) = self.rooms.get(&target) else {
            return;
        };
        let mut room = room_identifier(room);
        if let Some(message) = initial_message {
            room.messages = Some(vec![message]);
        }
        self.query_events
            .push_back(messenger_interface::interface::QueryEvent::ChannelCreated {
                r#where: None,
                room,
            });
    }

    fn ingest_message(
        &mut self,
        wire: WireMessage,
        edited: bool,
        emit_events: bool,
    ) -> Option<Identifier<Message>> {
        let target = wire.target;
        let new_room = self.remember_room(&wire);
        let updates_cached_text = edited
            && self
                .messages
                .get(&target)
                .is_some_and(|messages| messages.contains_key(&wire.message_id));
        if wire.text.is_empty() && !updates_cached_text {
            if emit_events && new_room {
                self.push_new_room_event(target, None);
            }
            return None;
        }

        let external_message_id = wire.message_id;
        let interface_message_id = target.message_id(external_message_id);
        let (identifier, changed, is_update, removed_message_ids) = {
            let messages = self.messages.entry(target).or_default();
            let is_update = edited || messages.contains_key(&external_message_id);
            let changed = match messages.get_mut(&external_message_id) {
                Some(cached) => cached.update(wire),
                None => {
                    messages.insert(external_message_id, CachedMessage::new(wire, edited));
                    true
                }
            };
            let identifier = messages
                .get(&external_message_id)
                .expect("message was inserted above")
                .identifier(target);

            let mut removed_message_ids = Vec::new();
            while messages.len() > MAX_CACHED_MESSAGES_PER_CHAT {
                let Some(oldest) = messages.keys().next().copied() else {
                    break;
                };
                messages.remove(&oldest);
                removed_message_ids.push(oldest);
            }
            (identifier, changed, is_update, removed_message_ids)
        };

        self.message_targets
            .insert(interface_message_id, (target, external_message_id));
        for removed_message_id in removed_message_ids {
            self.message_targets
                .remove(&target.message_id(removed_message_id));
        }

        if emit_events && new_room {
            self.push_new_room_event(target, Some(identifier.clone()));
        }
        if emit_events && changed {
            let event = if is_update {
                TextEvent::MessageUpdated {
                    room: Identifier::new(target.room_id(), ()),
                    message: identifier.clone(),
                }
            } else {
                TextEvent::MessageCreated {
                    room: Identifier::new(target.room_id(), ()),
                    message: identifier.clone(),
                }
            };
            self.events.push_back(event);
        }

        Some(identifier)
    }

    fn ingest_deletion(&mut self, channel_id: Option<i64>, message_ids: &[i32], emit_events: bool) {
        for &message_id in message_ids {
            let targets = match channel_id {
                Some(channel_id) => ChatTarget::from_channel_id(channel_id)
                    .into_iter()
                    .collect::<Vec<_>>(),
                None => self
                    .messages
                    .iter()
                    .filter_map(|(target, messages)| {
                        (!target.is_channel() && messages.contains_key(&message_id))
                            .then_some(*target)
                    })
                    .collect(),
            };

            for target in targets {
                let removed = self
                    .messages
                    .get_mut(&target)
                    .and_then(|messages| messages.remove(&message_id))
                    .is_some();
                self.message_targets.remove(&target.message_id(message_id));
                if emit_events && removed {
                    self.events.push_back(TextEvent::MessageDeleted {
                        room: Identifier::new(target.room_id(), ()),
                        message_id: target.message_id(message_id),
                    });
                }
            }
        }
    }
}

impl TelegramMessenger {
    async fn next_wire_update(&self) -> Result<Option<WireUpdate>, BoxError> {
        let client = self.client.clone();
        let updates = Arc::clone(&self.updates);
        let me = self.me.clone();
        telegram_task(async move {
            let mut updates = updates.lock().await;
            if updates.stream.is_none() {
                let raw = updates
                    .raw
                    .take()
                    .ok_or_else(|| message_error("Telegram update receiver is unavailable"))?;
                updates.stream = Some(
                    client
                        .stream_updates(
                            raw,
                            UpdatesConfiguration {
                                catch_up: true,
                                ..Default::default()
                            },
                        )
                        .await?,
                );
            }

            let update = {
                let stream = updates
                    .stream
                    .as_mut()
                    .expect("Telegram update stream was initialized above");
                let update = stream.next().await?;
                if let Err(error) = stream.sync_update_state().await {
                    warn!("Telegram update-state persistence failed: {error}");
                }
                update
            };

            match update {
                Update::NewMessage(message) => Ok(Some(WireUpdate::New(
                    wire_message(message.into_inner(), None, &me).await?,
                ))),
                Update::MessageEdited(message) => Ok(Some(WireUpdate::Edited(
                    wire_message(message.into_inner(), None, &me).await?,
                ))),
                Update::MessageDeleted(deletion) => Ok(Some(WireUpdate::Deleted {
                    channel_id: deletion.channel_id(),
                    message_ids: deletion.into_messages(),
                })),
                _ => Ok(None),
            }
        })
        .await?
    }

    pub(crate) async fn pump_update(&self) -> Result<(), BoxError> {
        match self.next_wire_update().await? {
            Some(WireUpdate::New(message)) => {
                self.state
                    .lock()
                    .unwrap()
                    .ingest_message(message, false, true);
            }
            Some(WireUpdate::Edited(message)) => {
                self.state
                    .lock()
                    .unwrap()
                    .ingest_message(message, true, true);
            }
            Some(WireUpdate::Deleted {
                channel_id,
                message_ids,
            }) => {
                self.state
                    .lock()
                    .unwrap()
                    .ingest_deletion(channel_id, &message_ids, true);
            }
            None => {}
        }
        Ok(())
    }

    fn pop_text_event(&self) -> Option<TextEvent> {
        self.state.lock().unwrap().events.pop_front()
    }

    pub(crate) async fn next_text_event(&self) -> Option<TextEvent> {
        loop {
            if let Some(event) = self.pop_text_event() {
                return Some(event);
            }

            let result = {
                let _poll = self.poll_lock.lock().await;
                if let Some(event) = self.pop_text_event() {
                    return Some(event);
                }
                self.pump_update().await
            };

            if let Err(error) = result {
                warn!("Telegram update stream failed: {error}");
                Delay::new(Duration::from_secs(2)).await;
            }
        }
    }
}

#[async_trait]
impl Text for TelegramMessenger {
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
        ordering: Ordering,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        self.ensure_dialogs().await?;
        let target = self.room_target(*location.id())?;
        let peer_ref = self.room_ref(target)?;
        let before = match load_messages_before {
            Some(message) => {
                let state = self.state.lock().unwrap();
                let (message_target, message_id) = state
                    .message_targets
                    .get(message.id())
                    .copied()
                    .ok_or("No Telegram message mapping for pagination")?;
                if message_target != target {
                    return Err("Telegram pagination message belongs to another room".into());
                }
                Some(message_id)
            }
            None => None,
        };

        let client = self.client.clone();
        let me = self.me.clone();
        let messages = telegram_task(async move {
            let mut iterator = client.iter_messages(peer_ref).limit(PAGE_SIZE);
            if let Some(before) = before {
                iterator = iterator.offset_id(before);
            }
            let mut messages = Vec::new();
            while let Some(message) = iterator.next().await? {
                let message = wire_message(message, Some(peer_ref), &me).await?;
                if !message.text.is_empty() {
                    messages.push(message);
                }
            }
            Ok::<_, BoxError>(messages)
        })
        .await??;

        let mut state = self.state.lock().unwrap();
        let mut page = messages
            .into_iter()
            .filter_map(|message| state.ingest_message(message, false, false))
            .collect::<Vec<_>>();
        if ordering == Ordering::Time {
            page.reverse();
        }
        Ok(page)
    }

    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
        self.ensure_dialogs().await?;
        let target = self.room_target(*location.id())?;
        let peer_ref = self.room_ref(target)?;
        let text = contents.content.text.to_plain();
        if text.is_empty() {
            return Err("Telegram text message cannot be empty".into());
        }
        if text.chars().count() > 4_096 {
            return Err("Telegram text message exceeds 4096 characters".into());
        }

        let client = self.client.clone();
        let me = self.me.clone();
        let message = telegram_task(async move {
            let message = client.send_message(peer_ref, text).await?;
            wire_message(message, Some(peer_ref), &me).await
        })
        .await??;

        self.state
            .lock()
            .unwrap()
            .ingest_message(message, false, false)
            .ok_or_else(|| "Telegram send response had no text".into())
    }

    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        self.ensure_dialogs().await
    }

    fn next_event(self: Retained<Self>) -> EventFuture<TextEvent> {
        Box::pin(async move { self.next_text_event().await })
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use grammers_session::types::{PeerAuth, PeerId, PeerRef};

    use super::*;
    use crate::TelegramState;

    fn wire_message(message_id: i32, text: &str) -> WireMessage {
        let peer_id = PeerId::user(42).unwrap();
        let target = ChatTarget::from_peer_id(peer_id).unwrap();
        let user = TelegramUser {
            external_id: 42,
            name: "Ada".to_owned(),
            username: Some("ada".to_owned()),
            phone: None,
            is_bot: false,
        };
        WireMessage {
            target,
            peer_ref: PeerRef {
                id: peer_id,
                auth: PeerAuth::default(),
            },
            peer: TelegramPeer {
                target,
                name: "Ada".to_owned(),
                user: Some(user.clone()),
            },
            message_id,
            author: Some(user),
            date: chrono::Utc
                .timestamp_opt(1_700_000_000 + i64::from(message_id), 0)
                .unwrap(),
            edit_date: None,
            text: text.to_owned(),
        }
    }

    #[test]
    fn edit_keeps_previous_revision_and_emits_update() {
        let mut state = TelegramState::default();
        let original = state
            .ingest_message(wire_message(7, "before"), false, false)
            .unwrap();
        assert!(!original.is_edited());

        let mut edited = wire_message(7, "after");
        edited.edit_date = Some(
            chrono::Utc
                .timestamp_opt(1_700_000_100, 0)
                .single()
                .unwrap(),
        );
        let edited = state.ingest_message(edited, true, true).unwrap();

        assert!(edited.is_edited());
        assert_eq!(edited.history[0].text.to_plain(), "before");
        assert_eq!(edited.content.text.to_plain(), "after");
        assert!(matches!(
            state.events.pop_front(),
            Some(TextEvent::MessageUpdated { .. })
        ));
    }

    #[test]
    fn new_dialog_event_contains_initial_message() {
        let mut state = TelegramState::default();
        state.ingest_message(wire_message(1, "first"), false, true);

        let room = match state.query_events.pop_front() {
            Some(messenger_interface::interface::QueryEvent::ChannelCreated {
                r#where: None,
                room,
            }) => room,
            _ => panic!("expected direct room creation"),
        };
        let messages = room.messages.as_ref().expect("initial messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content.text.to_plain(), "first");
        assert!(matches!(
            state.events.pop_front(),
            Some(TextEvent::MessageCreated { .. })
        ));
    }

    #[test]
    fn peer_is_part_of_message_identity() {
        let user = ChatTarget::from_peer_id(PeerId::user(42).unwrap()).unwrap();
        let group = ChatTarget::from_peer_id(PeerId::chat(42).unwrap()).unwrap();
        assert_ne!(user.room_id(), group.room_id());
        assert_ne!(user.message_id(1), group.message_id(1));
    }
}
