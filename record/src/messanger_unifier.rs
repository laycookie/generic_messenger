//! Contains all data needed to interface with the messenger

use std::{collections::HashMap, ops::Deref, sync::Arc};

use messenger_interface::{
    interface::Messenger,
    types::{CallStatus, House, ID, Identifier, Message, Place, Room, User},
};

#[derive(Debug, Clone)]
pub struct Call {
    messanger_handle: MessangerHandle,
    source: Identifier<Place<Room>>,
    status: CallStatus,
}

impl Call {
    pub fn new(messanger_handle: MessangerHandle, source: Identifier<Place<Room>>) -> Self {
        Self {
            messanger_handle,
            source,
            status: CallStatus::Connecting,
        }
    }
    pub fn handle(&self) -> MessangerHandle {
        self.messanger_handle
    }
    pub fn source(&self) -> &Identifier<Place<Room>> {
        &self.source
    }
    pub fn id(&self) -> ID {
        *self.source.id()
    }
    pub fn status_str(&self) -> &str {
        match self.status {
            CallStatus::Connected => "Connected",
            CallStatus::Connecting => "Connecting",
            CallStatus::Disconnected => "Disconnected",
        }
    }
}
#[derive(Debug)]
pub struct MessangerData {
    handle: MessangerHandle,

    pub profile: Option<Identifier<User>>,
    pub contacts: Vec<Identifier<User>>,
    pub conversations: Vec<Identifier<Place<Room>>>,
    pub guilds: Vec<Identifier<Place<House>>>,
    pub chats: HashMap<ID, Vec<Identifier<Message>>>,
    pub calls: Vec<Call>,
}

impl MessangerData {
    pub fn new(handle: MessangerHandle) -> Self {
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
    pub fn handle(&self) -> MessangerHandle {
        self.handle
    }
}
// ===
#[derive(Debug, Clone, Copy)]
pub struct MessangerHandle {
    id: usize,
    index: usize,
}
impl PartialEq for MessangerHandle {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[derive(Clone)]
pub struct MessangerInterface {
    pub handle: MessangerHandle,
    pub api: Arc<dyn Messenger>,
}
impl Deref for MessangerInterface {
    type Target = Arc<dyn Messenger>;

    fn deref(&self) -> &Self::Target {
        &self.api
    }
}

#[derive(Default)]
pub struct Messangers {
    id_counter: usize,
    interface: Vec<MessangerInterface>,
    data: Vec<MessangerData>,
}

impl Messangers {
    pub fn len(&self) -> usize {
        self.interface.len()
    }
    pub fn interface_iter(&self) -> std::slice::Iter<'_, MessangerInterface> {
        self.interface.iter()
    }
    pub fn data_iter(&self) -> std::slice::Iter<'_, MessangerData> {
        self.data.iter()
    }
    pub fn interface_from_handle(
        &self,
        messanger_handle: MessangerHandle,
    ) -> Option<&MessangerInterface> {
        if self.interface.len() > messanger_handle.index
            && messanger_handle == self.interface[messanger_handle.index].handle
        {
            return Some(&self.interface[messanger_handle.index]);
        }

        self.interface.iter().find(|a| a.handle == messanger_handle)
    }
    pub fn data_from_handle(&self, messanger_handle: MessangerHandle) -> Option<&MessangerData> {
        if self.data.len() > messanger_handle.index
            && messanger_handle == self.data[messanger_handle.index].handle
        {
            return Some(&self.data[messanger_handle.index]);
        }

        self.data
            .iter()
            .find(|data| data.handle == messanger_handle)
    }
    pub fn mut_data_from_handle(
        &mut self,
        messanger_handle: MessangerHandle,
    ) -> Option<&mut MessangerData> {
        if self.data.len() > messanger_handle.index
            && messanger_handle == self.data[messanger_handle.index].handle
        {
            return Some(&mut self.data[messanger_handle.index]);
        }

        self.data
            .iter_mut()
            .find(|data| data.handle == messanger_handle)
    }
    pub fn add_messanger(&mut self, api: Arc<dyn Messenger>) -> MessangerHandle {
        let handle = MessangerHandle {
            id: self.id_counter,
            index: self.interface.len(),
        };

        self.data.push(MessangerData::new(handle));
        self.interface.push(MessangerInterface { handle, api });

        self.id_counter += 1;

        handle
    }
    pub fn remove_by_handle(&mut self, handle_to_remove: MessangerHandle) {
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
