use std::{collections::BTreeMap, ops::Deref, sync::Arc};

use messenger_interface::{
    interface::Messenger,
    types::{House, ID, Identifier, Message, Place, Room, User},
};

use std::collections::HashMap;

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
}

impl Call {
    pub fn new(messenger_id: MessengerId, source: Identifier<Place<Room>>) -> Self {
        Self {
            messenger_id,
            source,
        }
    }
    pub fn messenger_id(&self) -> MessengerId {
        self.messenger_id
    }
    pub fn source(&self) -> &Identifier<Place<Room>> {
        &self.source
    }
    pub fn id(&self) -> ID {
        *self.source.id()
    }
    pub fn status_str(&self) -> &str {
        "Sample"
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
    pub chats: HashMap<ID, Vec<Identifier<Message>>>,
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
            chats: HashMap::new(),
            calls: Vec::new(),
            pending_sends: Vec::new(),
        }
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
