use std::{collections::BTreeMap, ops::Deref, sync::Arc};

use messenger_interface::{
    interface::{CallState, Messenger},
    types::{House, ID, Identifier, Place, Room, User},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessengerId(usize);

#[derive(Clone)]
pub struct MessengerInterface {
    pub id: MessengerId,
    pub api: Arc<dyn Messenger>,
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
        }
    }

    pub fn room(&self, room_id: ID) -> Option<&Identifier<Place<Room>>> {
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
        if let Some(pos) = self
            .conversations
            .iter()
            .position(|room| *room.id() == room_id)
        {
            return self.conversations.get_mut(pos);
        }

        for guild in &mut self.guilds {
            let Some(rooms) = guild.rooms.as_mut() else {
                continue;
            };

            if let Some(pos) = rooms.iter().position(|room| *room.id() == room_id) {
                return rooms.get_mut(pos);
            }
        }

        None
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

    pub fn add(&mut self, api: Arc<dyn Messenger>) -> MessengerId {
        let id = MessengerId(self.next_id);
        self.next_id += 1;

        self.entries.insert(
            id,
            MessengerEntry {
                interface: MessengerInterface { id, api },
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
