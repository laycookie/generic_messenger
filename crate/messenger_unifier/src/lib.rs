#![cfg_attr(test, feature(arbitrary_self_types))]

//! Registry and application state shared across messenger backends.
//! Each backend receives stable [`MessengerId`] scope and [`MessengerData`].

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt,
    future::Future,
    hash::{Hash, Hasher},
    marker::PhantomData,
    ops::Deref,
};

use messenger_interface::{
    interface::{
        CallState, Messenger, MessengerError, QueryEvent, TextDelete, TextEdit, TextEvent,
        TextReactions, VideoCodec, VideoEvent, VoiceEvent,
    },
    owner::Owner,
    stream::{EventFuture, LeaseStream, StreamScope, WeakSocketStream, WeakStreamSource},
    types::{House, ID, Identifier, Message as InterfaceMessage, Place, Room, User},
};

const MAX_MESSAGE_TOMBSTONES: usize = 8192;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessengerId(usize);

/// Entity ID scoped by messenger and marker type.
/// Prevents cross-backend and cross-kind comparisons without allocation.
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

/// Backend facade storing erased owner and metadata only.
/// Root streams are created on demand and owned by caller.
pub struct MessengerInterface {
    pub id: MessengerId,
    owner: Owner<dyn Messenger>,
    /// Capability flags resolved once from the concrete backend type.
    pub edit: bool,
    pub delete: bool,
    pub reactions: bool,
}

fn poll_query(
    messenger: messenger_interface::owner::Retained<dyn Messenger>,
) -> EventFuture<QueryEvent> {
    match messenger.retained_query() {
        Ok(query) => query.next_event(),
        Err(_) => Box::pin(std::future::ready(None)),
    }
}

fn poll_text(
    messenger: messenger_interface::owner::Retained<dyn Messenger>,
) -> EventFuture<TextEvent> {
    match messenger.retained_text() {
        Ok(text) => text.next_event(),
        Err(_) => Box::pin(std::future::ready(None)),
    }
}

fn poll_voice(
    messenger: messenger_interface::owner::Retained<dyn Messenger>,
) -> EventFuture<VoiceEvent> {
    match messenger.retained_audio() {
        Ok(audio) => audio.next_event(),
        Err(_) => Box::pin(std::future::ready(None)),
    }
}

fn poll_video(
    messenger: messenger_interface::owner::Retained<dyn Messenger>,
) -> EventFuture<VideoEvent> {
    match messenger.retained_video() {
        Ok(video) => video.next_event(),
        Err(_) => Box::pin(std::future::ready(None)),
    }
}

impl MessengerInterface {
    fn new(id: MessengerId, owner: Owner<dyn Messenger>) -> Self {
        let messenger = owner.get();
        let text = messenger.text().ok();
        let edit = text.is_some_and(|text| text.edit().is_ok());
        let delete = text.is_some_and(|text| text.delete().is_ok());
        let reactions = text.is_some_and(|text| text.reactions().is_ok());

        Self {
            owner,
            id,
            edit,
            delete,
            reactions,
        }
    }

    /// Returns borrowed access to backend messenger operations.
    pub fn messenger(&self) -> &(dyn Messenger + 'static) {
        self.owner.get()
    }

    /// Returns text editing capability when supported by this backend.
    pub fn text_edit(&self) -> Result<&dyn TextEdit, MessengerError> {
        self.messenger().text()?.edit()
    }

    /// Returns text deletion capability when supported by this backend.
    pub fn text_delete(&self) -> Result<&dyn TextDelete, MessengerError> {
        self.messenger().text()?.delete()
    }

    /// Returns text reaction capability when supported by this backend.
    pub fn text_reactions(&self) -> Result<&dyn TextReactions, MessengerError> {
        self.messenger().text()?.reactions()
    }

    /// Binds nested source to messenger lifetime.
    pub fn bind_stream<Event>(&self, source: WeakStreamSource<Event>) -> WeakSocketStream<Event>
    where
        Event: Send + 'static,
    {
        StreamScope::new(self.owner.downgrade()).bind(source)
    }

    /// Open query listener within messenger lifetime scope.
    pub async fn listen_query(
        &self,
    ) -> Result<LeaseStream<QueryEvent>, Box<dyn std::error::Error + Send + Sync>> {
        let query = self.messenger().query()?;
        LeaseStream::open(&self.owner, query.prepare_listener(), poll_query).await
    }

    /// Open text listener within messenger lifetime scope.
    pub async fn listen_text(
        &self,
    ) -> Result<LeaseStream<TextEvent>, Box<dyn std::error::Error + Send + Sync>> {
        let text = self.messenger().text()?;
        LeaseStream::open(&self.owner, text.prepare_listener(), poll_text).await
    }

    /// Open voice listener within messenger lifetime scope.
    pub async fn listen_voice(
        &self,
    ) -> Result<LeaseStream<VoiceEvent>, Box<dyn std::error::Error + Send + Sync>> {
        let audio = self.messenger().audio()?;
        LeaseStream::open(&self.owner, audio.prepare_listener(), poll_voice).await
    }

    /// Opens configured video listener.
    pub async fn listen_video(
        &self,
        preferred_codec: VideoCodec,
    ) -> Result<LeaseStream<VideoEvent>, Box<dyn std::error::Error + Send + Sync>> {
        let video = self.messenger().video()?;
        let setup = async move {
            video.set_preferred_codec(preferred_codec).await?;
            video.prepare_listener().await
        };
        LeaseStream::open(&self.owner, setup, poll_video).await
    }

    fn close(&self) {
        self.owner.close();
    }
}

impl Deref for MessengerInterface {
    type Target = dyn Messenger + 'static;

    fn deref(&self) -> &Self::Target {
        self.messenger()
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
    /// Recent deletions blocking stale HTTP/send resurrection.
    message_tombstones: HashSet<(ID, ID)>,
    message_tombstone_order: VecDeque<(ID, ID)>,
    /// Full edit payloads received before a room's initial snapshot.
    pending_message_updates: HashMap<(ID, ID), Identifier<InterfaceMessage>>,
    /// Room-to-conversation/guild index without duplicated payloads.
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

        // Repair stale index after direct public-vector mutation.
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
            // Repair only shifted conversation edges.
            for (index, room) in self.conversations[..=pos].iter().enumerate() {
                self.room_index
                    .insert(*room.id(), RoomLocation::Conversation(index));
            }
        }
    }

    pub fn message_is_deleted(&self, room_id: ID, message_id: ID) -> bool {
        self.message_tombstones.contains(&(room_id, message_id))
    }

    /// Applies edit or defers it until snapshot provides graph position.
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

    /// Confirms optimistic row unless newer deletion won.
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

    /// Merges HTTP rows with newer socket state; tombstones win.
    pub fn merge_message_snapshot(
        &mut self,
        room_id: ID,
        snapshot: Vec<Identifier<InterfaceMessage>>,
    ) {
        if self.room(room_id).is_none() {
            return;
        }
        let live = self
            .room_mut(room_id)
            .and_then(|room| room.messages.take())
            .unwrap_or_default();
        let capacity = snapshot.len() + live.len();
        let mut positions = HashMap::with_capacity(capacity);
        let mut deduplicated = Vec::with_capacity(capacity);
        for message in snapshot.into_iter().chain(live) {
            let message_id = *message.id();
            if self.message_tombstones.contains(&(room_id, message_id)) {
                continue;
            }
            if let Some(index) = positions.get(&message_id).copied() {
                deduplicated[index] = message;
            } else {
                positions.insert(message_id, deduplicated.len());
                deduplicated.push(message);
            }
        }

        // Apply deferred edits only where snapshot supplies ordering.
        for ((_, message_id), message) in self
            .pending_message_updates
            .extract_if(|(update_room, _), _| *update_room == room_id)
        {
            if !self.message_tombstones.contains(&(room_id, message_id))
                && let Some(index) = positions.get(&message_id).copied()
            {
                deduplicated[index] = message;
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
    pub interface: Owner<MessengerInterface>,
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

    pub fn add(&mut self, owner: Owner<dyn Messenger>) -> MessengerId {
        let id = MessengerId(self.next_id);
        self.next_id += 1;
        let interface = Owner::new(MessengerInterface::new(id, owner));

        self.entries.insert(
            id,
            MessengerEntry {
                interface,
                data: MessengerData::new(),
            },
        );

        id
    }

    pub fn remove(&mut self, id: MessengerId) {
        if let Some(entry) = self.entries.remove(&id) {
            entry.interface.get().close();
        }
    }

    pub fn get(&self, id: MessengerId) -> Option<&MessengerEntry> {
        self.entries.get(&id)
    }

    pub fn get_mut(&mut self, id: MessengerId) -> Option<&mut MessengerEntry> {
        self.entries.get_mut(&id)
    }

    /// Builds registry-independent operation retained by [`MessengerId`].
    pub fn with_interface_async<F, R>(
        &self,
        id: MessengerId,
        operation: F,
    ) -> Option<impl Future<Output = Option<R>> + 'static + use<F, R>>
    where
        F: for<'a> AsyncFnOnce(&'a MessengerInterface) -> R + Send + 'static,
        R: Send + 'static,
    {
        let interface = self.entries.get(&id)?.interface.downgrade();
        Some(async move { interface.with_async(operation).await })
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
    use std::sync::Arc;

    use super::*;
    use async_trait::async_trait;
    use futures::StreamExt as _;
    use messenger_interface::auth::{AuthMethod, Credential};
    use messenger_interface::interface::Query;
    use messenger_interface::owner::Owner;
    use messenger_interface::stream::{ArcStream, WeakStreamSource};
    use messenger_interface::types::{Revision, RoomCapabilities};

    struct TestMessenger;
    struct QueryMessenger;

    struct PendingStream;

    #[async_trait]
    impl ArcStream for PendingStream {
        type Item = ();

        async fn next(self: Arc<Self>) -> Option<Self::Item> {
            futures::future::pending().await
        }
    }

    #[async_trait]
    impl Messenger for TestMessenger {
        async fn create_messenger(
            _: Credential,
        ) -> Result<Owner<dyn Messenger>, Box<dyn std::error::Error + Send + Sync>> {
            unreachable!()
        }

        fn auth_methods() -> &'static [AuthMethod] {
            &[]
        }

        fn name(&self) -> &'static str {
            "test"
        }

        fn id(&self) -> String {
            String::from("test")
        }

        fn auth(&self) -> Credential {
            Credential(Vec::new())
        }
    }

    #[async_trait]
    impl Messenger for QueryMessenger {
        async fn create_messenger(
            _: Credential,
        ) -> Result<Owner<dyn Messenger>, Box<dyn std::error::Error + Send + Sync>> {
            unreachable!()
        }

        fn auth_methods() -> &'static [AuthMethod] {
            &[]
        }

        fn name(&self) -> &'static str {
            "query-test"
        }

        fn id(&self) -> String {
            String::from("query-test")
        }

        fn auth(&self) -> Credential {
            Credential(Vec::new())
        }
    }

    #[async_trait]
    impl Query for QueryMessenger {
        async fn prepare_listener(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }

        fn next_event(self: messenger_interface::owner::Retained<Self>) -> EventFuture<QueryEvent> {
            Box::pin(async {
                Some(QueryEvent::ChannelDeleted {
                    r#where: None,
                    room_id: 7,
                })
            })
        }
    }

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

        // Marker type prevents cross-kind use.
        let _: ScopedId<UserEntity> = ScopedId::new(MessengerId(1), 7);
    }

    #[test]
    fn unsupported_feature_flags_are_false() {
        let owner: Owner<dyn Messenger> = Owner::new(TestMessenger);
        let interface = MessengerInterface::new(MessengerId(0), owner);

        assert!(!interface.edit);
        assert!(!interface.delete);
        assert!(!interface.reactions);
    }

    #[test]
    fn root_listener_projects_retained_erased_owner_on_demand() {
        futures::executor::block_on(async {
            let owner: Owner<dyn Messenger> = Owner::new(QueryMessenger);
            let interface = MessengerInterface::new(MessengerId(0), owner);
            let mut stream = interface
                .listen_query()
                .await
                .expect("query listener must be supported");

            assert!(matches!(
                stream.next().await,
                Some(QueryEvent::ChannelDeleted {
                    r#where: None,
                    room_id: 7,
                })
            ));
        });
    }

    #[test]
    fn removal_cancels_owned_interface_future() {
        let owner: Owner<dyn Messenger> = Owner::new(TestMessenger);
        let lifetime = owner.downgrade();
        let mut registry = MessengerRegistry::default();
        let id = registry.add(owner);
        let interface = match registry.with_interface_async(id, async |interface| interface.name())
        {
            Some(interface) => interface,
            None => unreachable!("registered messenger must yield an interface future"),
        };

        registry.remove(id);

        assert!(!lifetime.is_active());
        assert!(futures::executor::block_on(interface).is_none());
    }

    #[test]
    fn removal_closes_wrapper_bound_stream_while_source_survives() {
        futures::executor::block_on(async {
            let owner: Owner<dyn Messenger> = Owner::new(TestMessenger);
            let mut registry = MessengerRegistry::default();
            let id = registry.add(owner);
            let source = Arc::new(PendingStream);
            let mut stream = registry
                .get(id)
                .unwrap()
                .interface
                .bind_stream(WeakStreamSource::from_arc(&source));

            registry.remove(id);

            assert!(stream.next().await.is_none());
            assert_eq!(Arc::strong_count(&source), 1);
        });
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
    fn pending_socket_edit_overrides_live_and_snapshot_rows() {
        let mut data = data_with_room();
        data.room_mut(1).unwrap().messages = Some(vec![message(7, false)]);
        data.pending_message_updates
            .insert((1, 7), message(7, true));

        data.merge_message_snapshot(1, vec![message(7, false)]);

        let messages = data.room(1).unwrap().messages.as_ref().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].history.len(), 1);
    }

    #[test]
    fn unmatched_socket_edit_does_not_create_unordered_row() {
        let mut data = data_with_room();
        data.update_message(1, message(8, true));

        data.merge_message_snapshot(1, vec![message(7, false)]);

        let messages = data.room(1).unwrap().messages.as_ref().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(*messages[0].id(), 7);
        assert!(data.pending_message_updates.is_empty());
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
