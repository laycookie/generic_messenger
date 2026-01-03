use std::error::Error;
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use crate::types::MessageContents;
use crate::types::{Chan, Identifier, Message, Server, Usr};
use async_trait::async_trait;
use futures::Stream;

#[derive(Debug, thiserror::Error)]
pub enum MessamgerError {
    #[error("Feature not implimented on this messanger")]
    NotImplimented,
}

#[async_trait]
pub trait Messanger: Send + Sync {
    fn id(&self) -> String;
    fn name(&self) -> &'static str;
    fn auth(&self) -> String;

    fn query(&self) -> Result<&dyn MessangerQuery, MessamgerError> {
        Err(MessamgerError::NotImplimented)
    }
    fn param_query(&self) -> Result<&dyn ParameterizedMessangerQuery, MessamgerError> {
        Err(MessamgerError::NotImplimented)
    }
    async fn socket(
        self: Arc<Self>,
    ) -> Result<Weak<dyn Socket + Send + Sync>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }
    fn vc(&self) -> Result<&dyn VC, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }
}
impl PartialEq for dyn Messanger {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

/// Allows us to get data, without knowing anything about it.
#[async_trait]
pub trait MessangerQuery: Send + Sync {
    // Fetch client profile
    async fn fetch_profile(&self) -> Result<Identifier<Usr>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }
    // Users from friend list etc
    async fn fetch_contacts(&self) -> Result<Vec<Identifier<Usr>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }
    // List of DMs
    async fn fetch_conversation(
        &self,
    ) -> Result<Vec<Identifier<Chan>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }
    // Multi-channel groups.
    async fn fetch_guilds(&self) -> Result<Vec<Identifier<Server>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessamgerError::NotImplimented))
    }
}

/// Used to get data that we know something about. E.g. Getting messages from
/// a specific channel requires us to know from which channel.
#[async_trait]
pub trait ParameterizedMessangerQuery: Send + Sync {
    async fn get_server_conversations(
        &self,
        location: &Identifier<Server>,
    ) -> Vec<Identifier<Chan>>;
    async fn get_messages(
        &self,
        msgs_location: &Identifier<Chan>,
        load_from_msg: Option<Identifier<Message>>,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>>;
    async fn send_message(
        &self,
        location: &Identifier<Chan>,
        contents: MessageContents,
    ) -> Result<(), Box<dyn Error + Sync + Send>>;
}

// === Sockets ===
#[derive(Debug, PartialEq)]
pub enum SocketEvent {
    MessageCreated {
        channel: Identifier<()>,
        msg: Identifier<Message>,
    },
    ChannelCreated {
        server: Option<Identifier<()>>,
        channel: Identifier<Chan>,
    },
    Disconnected,
    Skip,
}
#[async_trait]
pub trait Socket: Stream<Item = SocketEvent> {
    async fn next(self: Arc<Self>) -> Option<SocketEvent>;
}

#[async_trait]
pub trait VC {
    async fn connect<'a>(&'a self, location: &Identifier<Chan>);
    async fn disconnect<'a>(&'a self, location: &Identifier<Chan>);
}
