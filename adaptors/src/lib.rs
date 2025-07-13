use std::fmt::Debug;
use std::sync::Weak;
use std::{error::Error, sync::Arc};

use async_trait::async_trait;
use types::{Chan, Identifier, Msg, Server, Usr};

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
    async fn socket(self: Arc<Self>) -> Option<Weak<dyn Socket + Send + Sync>> {
        None
    }
}
impl PartialEq for dyn Messanger {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

#[async_trait]
pub trait MessangerQuery {
    async fn get_profile(&self) -> Result<Identifier<Usr>, Box<dyn Error + Sync + Send>>; // Fetch client profile
    async fn get_contacts(&self) -> Result<Vec<Identifier<Usr>>, Box<dyn Error + Sync + Send>>; // Users from friend list etc
    async fn get_conversation(&self)
    -> Result<Vec<Identifier<Chan>>, Box<dyn Error + Sync + Send>>; // List of DMs
    async fn get_guilds(&self) -> Result<Vec<Identifier<Server>>, Box<dyn Error + Sync + Send>>; // Large groups that can have over a 100 people in them.
}

#[async_trait]
pub trait ParameterizedMessangerQuery {
    async fn get_messanges(
        &self,
        msgs_location: &Identifier<Chan>,
        load_from_msg: Option<Identifier<Msg>>,
    ) -> Result<Vec<Identifier<Msg>>, Box<dyn Error + Sync + Send>>;
    async fn send_message(
        &self,
        location: &Identifier<Chan>,
        contents: String,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

// === Sockets
#[derive(Debug, PartialEq)]
pub enum SocketEvent {
    MessageCreated {
        channel: Identifier<()>,
        msg: Identifier<Msg>,
    },
    Disconected,
    Skip,
}
#[async_trait]
pub trait Socket {
    async fn next(self: Arc<Self>) -> Option<SocketEvent>;
}
