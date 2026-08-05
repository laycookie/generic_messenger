//! Cross-messenger unifier.
//!
//! Aggregates the multiple [`Messenger`] backends the app is logged into behind
//! a single [`MessengerRegistry`], assigning each a stable [`MessengerId`] and
//! holding the per-backend fetched state ([`MessengerData`]: profile, contacts,
//! conversations, guilds, calls, optimistic sends). This is application-level
//! state that sits *above* the per-backend `messenger_interface` abstraction.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    ops::Deref,
    sync::Arc,
};

use messenger_interface::{
    interface::{CallState, Messenger, TextDelete, TextEdit, TextReactions},
    types::{House, ID, Identifier, Message as InterfaceMessage, Place, Room, User},
};

const MAX_MESSAGE_TOMBSTONES: usize = 8192;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessengerId(usize);

/// Entity id paired with its owning messenger instance.
///
/// Backend adapters deliberately expose compact hashed `u64` ids. Their
/// uniqueness contract stops at one messenger instance and one entity kind;
/// comparing raw ids across accounts/backends is therefore a bug. This
/// zero-allocation key makes messenger scope part of equality/hash, while `K`
/// prevents mixing rooms, users, and messages at compile time.
pub struct ScopedId<K> {
    messenger: MessengerId,
    raw: ID,
    kind: PhantomData<fn() -> K>,
}

impl<K> ScopedId<K> {
    pub const fn new(messenger: MessengerId, raw: ID) -> Self {
        Self {
            messenger,
            raw,
            kind: PhantomData,
        }
    }

    pub const fn messenger(self) -> MessengerId {
        self.messenger
    }

    pub const fn raw(self) -> ID {
        self.raw
    }
}

impl<K> Clone for ScopedId<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K> Copy for ScopedId<K> {}

impl<K> fmt::Debug for ScopedId<K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScopedId")
            .field("messenger", &self.messenger)
            .field("raw", &self.raw)
            .finish()
    }
}

impl<K> PartialEq for ScopedId<K> {
    fn eq(&self, other: &Self) -> bool {
        self.messenger == other.messenger && self.raw == other.raw
    }
}

impl<K> Eq for ScopedId<K> {}

impl<K> Hash for ScopedId<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.messenger.hash(state);
        self.raw.hash(state);
    }
}

/// Marker types for [`ScopedId`].
pub enum UserEntity {}
pub enum RoomEntity {}
pub enum MessageEntity {}

#[derive(Clone)]
pub struct MessengerInterface {
    pub id: MessengerId,
    pub api: Arc<dyn Messenger>,
    /// Pre-cast optional text capabilities. `None` means the backend's *type*
    /// doesn't implement the capability (decided at compile time by the
    /// `TextCaster*` specialization in `messenger_interface`) — the UI hides
    /// the corresponding affordance instead of calling and failing.
    pub edit: Option<Arc<dyn TextEdit>>,
    pub delete: Option<Arc<dyn TextDelete>>,
    pub reactions: Option<Arc<dyn TextReactions>>,
}

impl MessengerInterface {
    fn new(id: MessengerId, api: Arc<dyn Messenger>) -> Self {
        let text = api.clone().arc_text().ok();
        Self {
            id,
            api,
            edit: text.clone().and_then(|t| t.arc_edit().ok()),
            delete: text.clone().and_then(|t| t.arc_delete().ok()),
            reactions: text.and_then(|t| t.arc_reactions().ok()),
        }
    }
}

impl Deref for MessengerInterface {
    type Target = Arc<dyn Messenger>;

    fn deref(&self) -> &Self::Target {
        &self.api
    }
}

#[derive(Debug, Clone)]
pub struct Call {
    messenger_id: MessengerId,
    source: Identifier<Place<Room>>,
    state: CallState,
}

impl Call {
    pub fn new(
        messenger_id: MessengerId,
        source: Identifier<Place<Room>>,
        state: CallState,
    ) -> Self {
        Self {
            messenger_id,
            source,
            state,
        }
    }
    pub fn messenger_id(&self) -> MessengerId {
        self.messenger_id
    }
    pub fn source(&self) -> &Identifier<Place<Room>> {
        &self.source
    }
    pub fn source_mut(&mut self) -> &mut Identifier<Place<Room>> {
        &mut self.source
    }
    pub fn id(&self) -> ID {
        *self.source.id()
    }
    pub fn state_str(&self) -> &str {
        self.state.as_str()
    }
    pub fn is_connected(&self) -> bool {
        matches!(self.state, CallState::Connected)
    }
    pub fn set_state(&mut self, state: CallState) {
        self.state = state;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PendingSend {
    pub pending_id: ID,
    pub room_id: ID,
}

#[derive(Debug)]
pub struct MessengerData {
    pub profile: Option<Identifier<User>>,
    pub contacts: Vec<Identifier<User>>,
    pub conversations: Vec<Identifier<Place<Room>>>,
    pub guilds: Vec<Identifier<Place<House>>>,
    pub calls: Vec<Call>,
    /// Tracks optimistic messages that haven't been confirmed by the server yet.
    pub pending_sends: Vec<PendingSend>,
    /// Recent server deletions. Prevents stale HTTP snapshots and late send
    /// confirmations from resurrecting rows deleted by ordered socket events.
    message_tombstones: HashSet<(ID, ID)>,
    message_tombstone_order: VecDeque<(ID, ID)>,
    /// Full edit payloads received before a room's initial snapshot.
    pending_message_updates: HashMap<(ID, ID), Identifier<InterfaceMessage>>,
    /// Graph-style edge from room id to its owning top-level conversation or
    /// guild. Stores no duplicated room/user/message payloads.
    room_index: HashMap<ID, RoomLocation>,
}

#[derive(Debug, Clone, Copy)]
enum RoomLocation {
    Conversation(usize),
    Guild { guild: usize, room: usize },
}

impl MessengerData {
    fn new() -> Self {
        Self {
            profile: None,
            contacts: Vec::new(),
            conversations: Vec::new(),
            guilds: Vec::new(),
            calls: Vec::new(),
            pending_sends: Vec::new(),
            message_tombstones: HashSet::new(),
            message_tombstone_order: VecDeque::new(),
            pending_message_updates: HashMap::new(),
            room_index: HashMap::new(),
        }
    }

    /// Rebuild normalized room edges after a structural backend update.
    pub fn rebuild_room_index(&mut self) {
        self.room_index.clear();
        self.room_index.extend(
            self.conversations
                .iter()
                .enumerate()
                .map(|(index, room)| (*room.id(), RoomLocation::Conversation(index))),
        );
        for (guild_index, guild) in self.guilds.iter().enumerate() {
            let Some(rooms) = guild.rooms.as_deref() else {
                continue;
            };
            self.room_index
                .extend(rooms.iter().enumerate().map(|(room_index, room)| {
                    (
                        *room.id(),
                        RoomLocation::Guild {
                            guild: guild_index,
                            room: room_index,
                        },
                    )
                }));
        }
    }

    fn indexed_room(&self, location: RoomLocation) -> Option<&Identifier<Place<Room>>> {
        match location {
            RoomLocation::Conversation(index) => self.conversations.get(index),
            RoomLocation::Guild { guild, room } => self
                .guilds
                .get(guild)
                .and_then(|guild| guild.rooms.as_deref())
                .and_then(|rooms| rooms.get(room)),
        }
    }

    pub fn room(&self, room_id: ID) -> Option<&Identifier<Place<Room>>> {
        if let Some(room) = self
            .room_index
            .get(&room_id)
            .and_then(|location| self.indexed_room(*location))
            .filter(|room| *room.id() == room_id)
        {
            return Some(room);
        }

        // Public vectors remain source-compatible while migration toward
        // normalized mutations is in progress. Fall back safely if external
        // code changed structure without rebuilding the index.
        self.conversations
            .iter()
            .find(|room| *room.id() == room_id)
            .or_else(|| {
                self.guilds
                    .iter()
                    .filter_map(|guild| guild.rooms.as_deref())
                    .flatten()
                    .find(|room| *room.id() == room_id)
            })
    }

    pub fn room_mut(&mut self, room_id: ID) -> Option<&mut Identifier<Place<Room>>> {
        let index_is_valid = self
            .room_index
            .get(&room_id)
            .and_then(|location| self.indexed_room(*location))
            .is_some_and(|room| *room.id() == room_id);
        if !index_is_valid {
            self.rebuild_room_index();
        }

        match self.room_index.get(&room_id).copied()? {
            RoomLocation::Conversation(index) => self.conversations.get_mut(index),
            RoomLocation::Guild { guild, room } => self
                .guilds
                .get_mut(guild)
                .and_then(|guild| guild.rooms.as_mut())
                .and_then(|rooms| rooms.get_mut(room)),
        }
    }

    pub fn move_conversation_to_front(&mut self, room_id: ID) {
        let indexed = self.room_index.get(&room_id).copied().and_then(|location| {
            let RoomLocation::Conversation(index) = location else {
                return None;
            };
            self.conversations
                .get(index)
                .is_some_and(|room| *room.id() == room_id)
                .then_some(index)
        });
        let pos = indexed.or_else(|| {
            self.conversations
                .iter()
                .position(|room| *room.id() == room_id)
        });
        if let Some(pos) = pos
            && pos != 0
        {
            let conversation = self.conversations.remove(pos);
            self.conversations.insert(0, conversation);
            // Only shifted conversation edges changed. Guild edges and later
            // conversations remain valid; avoid scanning the whole graph on
            // every incoming DM.
            for (index, room) in self.conversations[..=pos].iter().enumerate() {
                self.room_index
                    .insert(*room.id(), RoomLocation::Conversation(index));
            }
        }
    }

    pub fn message_is_deleted(&self, room_id: ID, message_id: ID) -> bool {
        self.message_tombstones.contains(&(room_id, message_id))
    }

    /// Apply or defer a socket edit. Deferral covers edit-before-snapshot
    /// without manufacturing a partial room/message graph.
    pub fn update_message(&mut self, room_id: ID, message: Identifier<InterfaceMessage>) {
        let message_id = *message.id();
        if self.message_is_deleted(room_id, message_id) {
            return;
        }
        if let Some(room) = self.room_mut(room_id)
            && let Some(messages) = room.messages.as_mut()
            && let Some(existing) = messages
                .iter_mut()
                .find(|existing| *existing.id() == message_id)
        {
            *existing = message;
            return;
        }
        self.pending_message_updates
            .insert((room_id, message_id), message);
    }

    /// Record deletion even when message/room snapshot is not present yet.
    pub fn delete_message(&mut self, room_id: ID, message_id: ID) {
        let key = (room_id, message_id);
        self.pending_message_updates.remove(&key);
        if self.message_tombstones.insert(key) {
            self.message_tombstone_order.push_back(key);
            if self.message_tombstone_order.len() > MAX_MESSAGE_TOMBSTONES
                && let Some(expired) = self.message_tombstone_order.pop_front()
            {
                self.message_tombstones.remove(&expired);
            }
        }
        if let Some(room) = self.room_mut(room_id)
            && let Some(messages) = room.messages.as_mut()
        {
            messages.retain(|message| *message.id() != message_id);
        }
    }

    /// Resolve one optimistic row without resurrecting a server ID already
    /// deleted by a newer socket event.
    pub fn confirm_pending_message(
        &mut self,
        room_id: ID,
        pending_id: ID,
        confirmed: Identifier<InterfaceMessage>,
    ) {
        self.pending_sends
            .retain(|pending| pending.pending_id != pending_id);
        let confirmed_id = *confirmed.id();
        if self.message_is_deleted(room_id, confirmed_id) {
            if let Some(room) = self.room_mut(room_id)
                && let Some(messages) = room.messages.as_mut()
            {
                messages
                    .retain(|message| *message.id() != pending_id && *message.id() != confirmed_id);
            }
            return;
        }

        if let Some(room) = self.room_mut(room_id) {
            let messages = room.messages.get_or_insert_with(Vec::new);
            if let Some(existing) = messages
                .iter_mut()
                .find(|message| *message.id() == confirmed_id)
            {
                *existing = confirmed;
                if confirmed_id != pending_id {
                    messages.retain(|message| *message.id() != pending_id);
                }
            } else if let Some(pending) = messages
                .iter_mut()
                .find(|message| *message.id() == pending_id)
            {
                *pending = confirmed;
            } else {
                messages.push(confirmed);
            }
        }
    }

    /// Merge initial/older HTTP rows with socket mutations which landed while
    /// request was in flight. Socket state wins conflicts; tombstones win all.
    pub fn merge_message_snapshot(
        &mut self,
        room_id: ID,
        snapshot: Vec<Identifier<InterfaceMessage>>,
    ) {
        if self.room(room_id).is_none() {
            return;
        }
        let mut merged = snapshot
            .into_iter()
            .filter(|message| !self.message_is_deleted(room_id, *message.id()))
            .collect::<Vec<_>>();
        let update_keys = self
            .pending_message_updates
            .keys()
            .filter(|(update_room, _)| *update_room == room_id)
            .copied()
            .collect::<Vec<_>>();
        let updates = update_keys
            .into_iter()
            .filter_map(|key| self.pending_message_updates.remove(&key))
            .collect::<Vec<_>>();
        let live = self
            .room_mut(room_id)
            .and_then(|room| room.messages.take())
            .unwrap_or_default();

        let mut positions = HashMap::with_capacity(merged.len() + updates.len() + live.len());
        let mut deduplicated = Vec::with_capacity(positions.capacity());
        for message in merged.drain(..).chain(updates).chain(live) {
            let message_id = *message.id();
            if self.message_is_deleted(room_id, message_id) {
                continue;
            }
            if let Some(index) = positions.get(&message_id).copied() {
                deduplicated[index] = message;
            } else {
                positions.insert(message_id, deduplicated.len());
                deduplicated.push(message);
            }
        }
        if let Some(room) = self.room_mut(room_id) {
            room.messages = Some(deduplicated);
        }
    }
}

impl Default for MessengerData {
    fn default() -> Self {
        Self::new()
    }
}

pub struct MessengerEntry {
    pub interface: MessengerInterface,
    pub data: MessengerData,
}

#[derive(Default)]
pub struct MessengerRegistry {
    next_id: usize,
    entries: BTreeMap<MessengerId, MessengerEntry>,
}

impl MessengerRegistry {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn add(&mut self, api: Arc<dyn Messenger>) -> MessengerId {
        let id = MessengerId(self.next_id);
        self.next_id += 1;

        self.entries.insert(
            id,
            MessengerEntry {
                interface: MessengerInterface::new(id, api),
                data: MessengerData::new(),
            },
        );

        id
    }

    pub fn remove(&mut self, id: MessengerId) {
        self.entries.remove(&id);
    }

    pub fn get(&self, id: MessengerId) -> Option<&MessengerEntry> {
        self.entries.get(&id)
    }

    pub fn get_mut(&mut self, id: MessengerId) -> Option<&mut MessengerEntry> {
        self.entries.get_mut(&id)
    }

    pub fn interface(&self, id: MessengerId) -> Option<&MessengerInterface> {
        self.entries.get(&id).map(|e| &e.interface)
    }

    pub fn data(&self, id: MessengerId) -> Option<&MessengerData> {
        self.entries.get(&id).map(|e| &e.data)
    }

    pub fn data_mut(&mut self, id: MessengerId) -> Option<&mut MessengerData> {
        self.entries.get_mut(&id).map(|e| &mut e.data)
    }

    pub fn iter(&self) -> impl Iterator<Item = (MessengerId, &MessengerEntry)> {
        self.entries.iter().map(|(&id, entry)| (id, entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use messenger_interface::types::{Revision, RoomCapabilities};

    fn room(id: ID) -> Identifier<Place<Room>> {
        Identifier::new(
            id,
            Place::new(
                format!("room-{id}"),
                None,
                Room::new(RoomCapabilities::Text, None, None, None),
            ),
        )
    }

    fn message(id: ID, edited: bool) -> Identifier<InterfaceMessage> {
        Identifier::new(
            id,
            InterfaceMessage {
                history: edited.then(Revision::default).into_iter().collect(),
                ..InterfaceMessage::default()
            },
        )
    }

    fn data_with_room() -> MessengerData {
        let mut data = MessengerData {
            conversations: vec![room(1)],
            ..MessengerData::default()
        };
        data.rebuild_room_index();
        data
    }

    #[test]
    fn scoped_ids_include_backend_and_entity_type() {
        let first = ScopedId::<RoomEntity>::new(MessengerId(1), 7);
        let same = ScopedId::<RoomEntity>::new(MessengerId(1), 7);
        let other_backend = ScopedId::<RoomEntity>::new(MessengerId(2), 7);
        assert_eq!(first, same);
        assert_ne!(first, other_backend);

        // Different entity kinds cannot be compared or inserted into the same
        // typed collection; this assignment is the positive compile-time side.
        let _: ScopedId<UserEntity> = ScopedId::new(MessengerId(1), 7);
    }

    #[test]
    fn room_index_repairs_after_legacy_vector_mutation() {
        let mut data = MessengerData {
            conversations: vec![room(1), room(2)],
            ..MessengerData::default()
        };
        data.rebuild_room_index();
        assert_eq!(data.room(2).map(|room| *room.id()), Some(2));

        data.conversations.swap(0, 1);
        assert_eq!(data.room_mut(2).map(|room| *room.id()), Some(2));
        assert_eq!(data.room(1).map(|room| *room.id()), Some(1));
    }

    #[test]
    fn socket_edit_before_snapshot_wins_over_stale_http_row() {
        let mut data = data_with_room();
        data.update_message(1, message(7, true));

        data.merge_message_snapshot(1, vec![message(7, false)]);

        let messages = data.room(1).unwrap().messages.as_ref().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].history.len(), 1);
    }

    #[test]
    fn socket_delete_before_snapshot_prevents_resurrection() {
        let mut data = data_with_room();
        data.delete_message(1, 7);

        data.merge_message_snapshot(1, vec![message(7, false)]);

        assert!(data.room(1).unwrap().messages.as_ref().unwrap().is_empty());
    }

    #[test]
    fn deleted_gateway_echo_is_not_readded_by_send_confirmation() {
        let mut data = data_with_room();
        let pending_id = u64::MAX;
        data.pending_sends.push(PendingSend {
            pending_id,
            room_id: 1,
        });
        data.room_mut(1).unwrap().messages =
            Some(vec![message(pending_id, false), message(7, false)]);
        data.delete_message(1, 7);

        data.confirm_pending_message(1, pending_id, message(7, false));

        assert!(data.pending_sends.is_empty());
        assert!(data.room(1).unwrap().messages.as_ref().unwrap().is_empty());
    }
}
