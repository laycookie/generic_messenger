use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use adaptors::{
    types::{Chan, Identifier, Msg, Server, Usr},
    Messanger, Socket,
};
use futures::{future::join_all, Stream};
// ==
// TODO: MAKE READ ONLY USING GETTERS
#[derive(Debug)]
pub struct MessangerData {
    messenger_id: usize,

    pub(crate) profile: Option<Identifier<Usr>>,
    pub(crate) contacts: Vec<Identifier<Usr>>,
    pub(crate) conversations: Vec<Identifier<Chan>>,
    pub(crate) guilds: Vec<Identifier<Server>>,

    pub(crate) chats: HashMap<Identifier<()>, Vec<Identifier<Msg>>>,
}

impl MessangerData {
    pub fn new(id: usize) -> Self {
        Self {
            messenger_id: id,
            profile: None,
            contacts: Vec::new(),
            conversations: Vec::new(),
            guilds: Vec::new(),
            chats: HashMap::new(),
        }
    }
}
// ===

#[derive(Debug, Clone, Copy)]
pub struct MessangerHandle {
    id: usize,
    index: usize,
}

#[derive(Default)]
pub struct Messangers {
    id_counter: usize,
    interface: Vec<(MessangerHandle, Arc<dyn Messanger>)>,
    data: Vec<MessangerData>,
}

impl Messangers {
    pub fn retain_by_handle(&mut self, handle: MessangerHandle) {
        self.interface.retain(|(h, _)| h.id == handle.id);
        self.data.retain(|d| d.messenger_id == handle.id);
    }
    pub fn len(&self) -> usize {
        self.interface.len()
    }
    pub fn interface_iter(
        &self,
    ) -> std::slice::Iter<'_, (MessangerHandle, Arc<dyn Messanger + 'static>)> {
        self.interface.iter()
    }
    pub fn data_iter(&self) -> std::slice::Iter<'_, MessangerData> {
        self.data.iter()
    }
    pub fn interface_from_handle(
        &self,
        messanger_handle: MessangerHandle,
    ) -> Option<&(MessangerHandle, Arc<dyn Messanger>)> {
        if self.interface.len() > messanger_handle.index
            && messanger_handle.id == self.interface[messanger_handle.index].0.id
        {
            return Some(&self.interface[messanger_handle.index]);
        }

        self.interface
            .iter()
            .find(|a| a.0.id == messanger_handle.id)
    }
    pub fn data_from_handle(&self, messanger_handle: MessangerHandle) -> Option<&MessangerData> {
        if self.data.len() > messanger_handle.index
            && messanger_handle.id == self.data[messanger_handle.index].messenger_id
        {
            return Some(&self.data[messanger_handle.index]);
        }

        self.data
            .iter()
            .find(|a| a.messenger_id == messanger_handle.id)
    }
    pub fn mut_data_from_handle(
        &mut self,
        messanger_handle: MessangerHandle,
    ) -> Option<&mut MessangerData> {
        if self.data.len() > messanger_handle.index
            && messanger_handle.id == self.data[messanger_handle.index].messenger_id
        {
            return Some(&mut self.data[messanger_handle.index]);
        }

        self.data
            .iter_mut()
            .find(|a| a.messenger_id == messanger_handle.id)
    }

    pub fn add_messanger(&mut self, messanger: Arc<dyn Messanger>) -> MessangerHandle {
        let handle = MessangerHandle {
            id: self.id_counter,
            index: self.interface.len(),
        };

        self.data.push(MessangerData::new(self.id_counter));
        self.interface.push((handle, messanger));

        self.id_counter += 1;

        handle
    }
}
