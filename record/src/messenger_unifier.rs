//! Contains all data needed to interface with the messenger

use std::{collections::HashMap, ops::Deref, sync::Arc};

use messenger_interface::{
    interface::{CallState, Messenger},
    types::{House, ID, Identifier, Message, Place, Room, User},
};

#[derive(Debug, Clone)]
pub struct Call {
    messenger_handle: MessengerHandle,
    source: Identifier<Place<Room>>,
    state: CallState,
}

impl Call {
    pub fn new(
        messenger_handle: MessengerHandle,
        source: Identifier<Place<Room>>,
        state: CallState,
    ) -> Self {
        Self {
            messenger_handle,
            source,
            state,
        }
    }
    pub fn handle(&self) -> MessengerHandle {
        self.messenger_handle
    }
    pub fn source(&self) -> &Identifier<Place<Room>> {
        &self.source
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
#[derive(Debug)]
pub struct MessengerData {
    handle: MessengerHandle,

    pub profile: Option<Identifier<User>>,
    pub contacts: Vec<Identifier<User>>,
    pub conversations: Vec<Identifier<Place<Room>>>,
    pub guilds: Vec<Identifier<Place<House>>>,
    pub chats: HashMap<ID, Vec<Identifier<Message>>>,
    pub calls: Vec<Call>,
}

impl MessengerData {
    pub fn new(handle: MessengerHandle) -> Self {
        Self {
            handle,
            profile: None,
            contacts: Vec::new(),
            conversations: Vec::new(),
            guilds: Vec::new(),
            chats: HashMap::new(),
            calls: Vec::new(),
        }
    }
    pub fn handle(&self) -> MessengerHandle {
        self.handle
    }
}
// ===
#[derive(Debug, Clone, Copy)]
pub struct MessengerHandle {
    id: usize,
    index: usize,
}
impl PartialEq for MessengerHandle {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[derive(Clone)]
pub struct MessengerInterface {
    pub handle: MessengerHandle,
    pub api: Arc<dyn Messenger>,
}
impl Deref for MessengerInterface {
    type Target = Arc<dyn Messenger>;

    fn deref(&self) -> &Self::Target {
        &self.api
    }
}

#[derive(Default)]
pub struct Messengers {
    id_counter: usize,
    interface: Vec<MessengerInterface>,
    data: Vec<MessengerData>,
}

impl Messengers {
    pub fn len(&self) -> usize {
        self.interface.len()
    }
    pub fn interface_iter(&self) -> std::slice::Iter<'_, MessengerInterface> {
        self.interface.iter()
    }
    pub fn data_iter(&self) -> std::slice::Iter<'_, MessengerData> {
        self.data.iter()
    }
    pub fn interface_from_handle(
        &self,
        messenger_handle: MessengerHandle,
    ) -> Option<&MessengerInterface> {
        if self.interface.len() > messenger_handle.index
            && messenger_handle == self.interface[messenger_handle.index].handle
        {
            return Some(&self.interface[messenger_handle.index]);
        }

        self.interface.iter().find(|a| a.handle == messenger_handle)
    }
    pub fn data_from_handle(&self, messenger_handle: MessengerHandle) -> Option<&MessengerData> {
        if self.data.len() > messenger_handle.index
            && messenger_handle == self.data[messenger_handle.index].handle
        {
            return Some(&self.data[messenger_handle.index]);
        }

        self.data
            .iter()
            .find(|data| data.handle == messenger_handle)
    }
    pub fn mut_data_from_handle(
        &mut self,
        messenger_handle: MessengerHandle,
    ) -> Option<&mut MessengerData> {
        if self.data.len() > messenger_handle.index
            && messenger_handle == self.data[messenger_handle.index].handle
        {
            return Some(&mut self.data[messenger_handle.index]);
        }

        self.data
            .iter_mut()
            .find(|data| data.handle == messenger_handle)
    }
    pub fn add_messenger(&mut self, api: Arc<dyn Messenger>) -> MessengerHandle {
        let handle = MessengerHandle {
            id: self.id_counter,
            index: self.interface.len(),
        };

        self.data.push(MessengerData::new(handle));
        self.interface.push(MessengerInterface { handle, api });

        self.id_counter += 1;

        handle
    }
    pub fn remove_by_handle(&mut self, handle_to_remove: MessengerHandle) {
        self.interface.remove(handle_to_remove.index);
        // Updates all cached indexes that are now wrong.
        for (i, interface) in self.interface[handle_to_remove.index..]
            .iter_mut()
            .enumerate()
        {
            interface.handle.index = handle_to_remove.index + i;
        }
        self.data.remove(handle_to_remove.index);
    }
}
