//! Contains all data needed to interface with the messenger

use std::{collections::HashMap, ops::Deref, sync::Arc};

use adaptors::{
    Messanger,
    types::{CallStatus, Chan, ID, Identifier, Msg, Server, Usr},
};

#[derive(Debug, Clone)]
pub(crate) struct Call {
    messanger_handle: MessangerHandle,
    source: Identifier<Chan>,
    status: CallStatus,
}

impl Call {
    pub(crate) fn new(messanger_handle: MessangerHandle, source: Identifier<Chan>) -> Self {
        Self {
            messanger_handle,
            source,
            status: CallStatus::Connecting,
        }
    }
    pub(crate) fn handle(&self) -> MessangerHandle {
        self.messanger_handle
    }
    pub(crate) fn source(&self) -> &Identifier<Chan> {
        &self.source
    }
    pub(crate) fn id(&self) -> ID {
        *self.source.get_id()
    }
    pub(crate) fn status_str(&self) -> &str {
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

    pub(crate) profile: Option<Identifier<Usr>>,
    pub(crate) contacts: Vec<Identifier<Usr>>,
    pub(crate) conversations: Vec<Identifier<Chan>>,
    pub(crate) guilds: Vec<Identifier<Server>>,
    pub(crate) chats: HashMap<Identifier<()>, Vec<Identifier<Msg>>>,
    pub(crate) calls: Vec<Call>,
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
    pub(crate) fn handle(&self) -> MessangerHandle {
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

#[derive(Debug, Clone)]
pub(crate) struct MessangerInterface {
    pub(crate) handle: MessangerHandle,
    pub(crate) api: Arc<dyn Messanger>,
}
impl Deref for MessangerInterface {
    type Target = Arc<dyn Messanger>;

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

        eprintln!("Cache hit occured");
        self.interface.iter().find(|a| a.handle == messanger_handle)
    }
    pub fn data_from_handle(&self, messanger_handle: MessangerHandle) -> Option<&MessangerData> {
        if self.data.len() > messanger_handle.index
            && messanger_handle == self.data[messanger_handle.index].handle
        {
            return Some(&self.data[messanger_handle.index]);
        }

        eprintln!("Cache hit occured");
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

        eprintln!("Cache hit occured");
        self.data
            .iter_mut()
            .find(|data| data.handle == messanger_handle)
    }
    pub fn add_messanger(&mut self, api: Arc<dyn Messanger>) -> MessangerHandle {
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
