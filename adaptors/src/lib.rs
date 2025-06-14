use std::error::Error;
use std::fmt::Debug;
use std::sync::Weak;

use async_trait::async_trait;
use types::{Message, Store, User};

pub mod discord;
mod network;
pub mod types;

#[async_trait]
pub trait Messanger: Send + Sync + Debug {
    fn id(&self) -> String;
    fn name(&self) -> &'static str;
    fn auth(&self) -> String;

    fn query(&self) -> Option<&dyn MessangerQuery> {
        None
    }
    fn param_query(&self) -> Option<&dyn ParameterizedMessangerQuery> {
        None
    }
    async fn socket(&self) -> Option<Weak<dyn Socket + Send + Sync>> {
        None
    }
}
impl PartialEq for dyn Messanger {
    fn eq(&self, other: &Self) -> bool {
        format!("{}", self.id()) == format!("{}", other.id())
    }
}

#[async_trait]
pub trait MessangerQuery {
    async fn get_profile(&self) -> Result<User, Box<dyn Error + Sync + Send>>; // Fetch client profile
    async fn get_contacts(&self) -> Result<Vec<User>, Box<dyn Error + Sync + Send>>; // Users from friend list etc
    async fn get_conversation(&self) -> Result<Vec<Store>, Box<dyn Error + Sync + Send>>; // List of DMs
    async fn get_guilds(&self) -> Result<Vec<Store>, Box<dyn Error + Sync + Send>>; // Large groups that can have over a 100 people in them.
}

#[async_trait]
pub trait ParameterizedMessangerQuery {
    async fn get_messanges(
        &self,
        msgs_location: &Store,
        load_from_msg: Option<Message>,
    ) -> Result<Vec<Message>, Box<dyn Error + Sync + Send>>;
    async fn send_message(
        &self,
        location: &Store,
        contents: String,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

// === Sockets
#[derive(Debug)]
pub enum SocketUpdate {
    MessageCreated,
    Skip,
}
#[async_trait]
pub trait Socket {
    async fn next(&self) -> Option<SocketUpdate>;
}
