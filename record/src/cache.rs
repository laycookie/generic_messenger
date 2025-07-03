use std::collections::HashMap;

use adaptors::{
    types::{Chan, Identifier, Msg, Server, Usr},
    Messanger,
};

pub struct MessangerData<T: Messanger> {
    pub(crate) auth: T,

    pub(crate) profile: Identifier<Usr>,
    pub(crate) contacts: Vec<Identifier<Usr>>,
    pub(crate) conversations: Vec<Identifier<Chan>>,
    pub(crate) guilds: Vec<Identifier<Server>>,

    pub(crate) chats: HashMap<Identifier<()>, Vec<Identifier<Msg>>>,
}
