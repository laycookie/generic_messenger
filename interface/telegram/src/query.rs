use std::{error::Error, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_timer::Delay;
use messenger_interface::{
    interface::{Query, QueryEvent},
    stream::{ArcStream, WeakSocketStream},
    types::{Details, House, Identifier, Place, Room, RoomCapabilities, User},
};
use tracing::warn;

use crate::{
    BoxError, StreamPollGuard, TelegramMessenger, TelegramPeer, TelegramQueryStream, TelegramRoom,
    TelegramUser, stable_id, telegram_task,
};

pub(crate) fn user_identifier(user: &TelegramUser) -> Identifier<User> {
    Identifier::new(
        stable_id("user", &user.external_id.to_string()),
        User {
            name: user.name.clone(),
            tag: user.username.clone(),
            icon: None,
            status: None,
            voice: None,
        },
    )
}

fn room_data(peer: &TelegramPeer) -> Room {
    let members = peer.user.as_ref().map(|user| vec![user_identifier(user)]);
    Room::new(RoomCapabilities::Text, members, None, None)
}

pub(crate) fn room_identifier(room: &TelegramRoom) -> Identifier<Place<Room>> {
    Identifier::new(
        room.target.room_id(),
        Place::new(room.peer.name.clone(), None, room_data(&room.peer)),
    )
}

impl TelegramMessenger {
    pub(crate) async fn ensure_dialogs(&self) -> Result<(), BoxError> {
        if self.state.lock().unwrap().dialogs_loaded {
            return Ok(());
        }

        let _refresh = self.dialog_lock.lock().await;
        if self.state.lock().unwrap().dialogs_loaded {
            return Ok(());
        }

        let client = self.client.clone();
        let dialogs = telegram_task(async move {
            let mut iterator = client.iter_dialogs();
            let mut dialogs = Vec::new();
            while let Some(dialog) = iterator.next().await? {
                let peer_ref = dialog.peer_ref();
                let peer = TelegramPeer::from_peer(dialog.peer());
                let last_activity = dialog
                    .last_message
                    .as_ref()
                    .map(|message| message.date().timestamp())
                    .unwrap_or_default();
                dialogs.push((peer_ref, peer, last_activity));
            }
            Ok::<_, BoxError>(dialogs)
        })
        .await??;

        let mut state = self.state.lock().unwrap();
        for (peer_ref, peer, last_activity) in dialogs {
            let target = peer.target;
            state.room_targets.insert(target.room_id(), target);
            state.peer_refs.insert(target, peer_ref);
            if let Some(user) = &peer.user {
                state.users.insert(user.external_id, user.clone());
            }
            state
                .rooms
                .entry(target)
                .and_modify(|room| {
                    room.peer = peer.clone();
                    room.last_activity = room.last_activity.max(last_activity);
                })
                .or_insert(TelegramRoom {
                    target,
                    peer,
                    last_activity,
                });
        }
        state.dialogs_loaded = true;
        Ok(())
    }

    fn pop_query_event(&self) -> Option<QueryEvent> {
        self.state.lock().unwrap().query_events.pop_front()
    }
}

#[async_trait]
impl Query for TelegramMessenger {
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        Ok(user_identifier(&self.me))
    }

    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        self.ensure_dialogs().await?;
        let mut contacts = self
            .state
            .lock()
            .unwrap()
            .users
            .values()
            .filter(|user| !user.is_bot && user.external_id != self.account_id)
            .map(user_identifier)
            .collect::<Vec<_>>();
        contacts.sort_by_key(|user| user.name.to_lowercase());
        contacts.dedup_by_key(|user| *user.id());
        Ok(contacts)
    }

    async fn rooms(&self) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        self.ensure_dialogs().await?;
        let mut rooms = self
            .state
            .lock()
            .unwrap()
            .rooms
            .values()
            .cloned()
            .collect::<Vec<_>>();
        rooms.sort_by(|left, right| {
            right
                .last_activity
                .cmp(&left.last_activity)
                .then_with(|| left.peer.name.cmp(&right.peer.name))
        });
        Ok(rooms.iter().map(room_identifier).collect())
    }

    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        Ok(Vec::new())
    }

    async fn room_details(
        &self,
        room: Identifier<Place<Room>>,
    ) -> Result<Room, Box<dyn Error + Sync + Send>> {
        self.ensure_dialogs().await?;
        let target = self.room_target(*room.id())?;
        let state = self.state.lock().unwrap();
        let room = state
            .rooms
            .get(&target)
            .ok_or("Telegram dialog is no longer available")?;
        Ok(room_data(&room.peer))
    }

    async fn inspect_user(
        &self,
        user: &Identifier<User>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        self.ensure_dialogs().await?;
        let state = self.state.lock().unwrap();
        let telegram_user = state
            .users
            .values()
            .find(|candidate| user_identifier(candidate).id() == user.id());

        let mut details = Details::default();
        details.push("Name", user.name.clone());
        if let Some(telegram_user) = telegram_user {
            details.push("Telegram user ID", telegram_user.external_id.to_string());
            if let Some(username) = &telegram_user.username {
                details.push("Username", format!("@{username}"));
            }
            if let Some(phone) = &telegram_user.phone {
                details.push("Phone", phone.clone());
            }
        }
        Ok(details)
    }

    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<QueryEvent>, Box<dyn Error + Sync + Send>> {
        self.ensure_dialogs().await?;
        let source = Arc::new(TelegramQueryStream {
            owner: Arc::downgrade(&self),
        });
        *self.query_stream.lock().unwrap() = Some(source.clone());
        Ok(WeakSocketStream::from_arc(source))
    }
}

#[async_trait]
impl ArcStream for TelegramQueryStream {
    type Item = QueryEvent;

    async fn next(self: Arc<Self>) -> Option<Self::Item> {
        let owner = self.owner.upgrade()?;
        let _stream = StreamPollGuard::new(&owner.active_streams);

        loop {
            if let Some(event) = owner.pop_query_event() {
                return Some(event);
            }
            if owner.query_owner_dropped() {
                return None;
            }

            let result = {
                let _poll = owner.poll_lock.lock().await;
                if let Some(event) = owner.pop_query_event() {
                    return Some(event);
                }
                if owner.query_owner_dropped() {
                    return None;
                }
                owner.pump_update().await
            };

            if let Err(error) = result {
                warn!("Telegram update stream failed: {error}");
                Delay::new(Duration::from_secs(2)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatTarget, TelegramPeer};

    #[test]
    fn direct_dialog_exposes_other_user_as_member() {
        let user = TelegramUser {
            external_id: 42,
            name: "Ada Lovelace".to_owned(),
            username: Some("ada".to_owned()),
            phone: None,
            is_bot: false,
        };
        let peer = TelegramPeer {
            target: ChatTarget(42),
            name: user.name.clone(),
            user: Some(user),
        };

        let room = room_data(&peer);
        assert_eq!(room.members.as_ref().map(Vec::len), Some(1));
    }
}
