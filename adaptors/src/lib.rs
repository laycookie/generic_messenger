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
    fn vc(&self) -> Option<&dyn VC> {
        None
    }
}
impl PartialEq for dyn Messanger {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

/// Allows us to get data, without knowing anything about it.
#[async_trait]
pub trait MessangerQuery {
    async fn fetch_profile(&self) -> Result<Identifier<Usr>, Box<dyn Error + Sync + Send>>; // Fetch client profile
    async fn fetch_contacts(&self) -> Result<Vec<Identifier<Usr>>, Box<dyn Error + Sync + Send>>; // Users from friend list etc
    async fn fetch_conversation(
        &self,
    ) -> Result<Vec<Identifier<Chan>>, Box<dyn Error + Sync + Send>>; // List of DMs
    async fn fetch_guilds(&self) -> Result<Vec<Identifier<Server>>, Box<dyn Error + Sync + Send>>; // Multi-channel groups.
}

/// Used to get data that we know something about. E.g. Getting messages from
/// a specific channel requiers us to know from which channel.
#[async_trait]
pub trait ParameterizedMessangerQuery {
    async fn get_server_conversations(
        &self,
        location: &Identifier<Server>,
    ) -> Vec<Identifier<Chan>>;
    async fn get_messages(
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
    Disconnected,
    Skip,
}
#[async_trait]
pub trait Socket {
    async fn next(self: Arc<Self>) -> Option<SocketEvent>;
}

pub enum VCLocation<'a> {
    Direct(&'a Identifier<Chan>),
    Server,
}

#[async_trait]
pub trait VC {
    async fn connect<'a>(&'a self, location: &Identifier<Chan>);
    async fn disconnect<'a>(&'a self, location: &Identifier<Chan>);
}
