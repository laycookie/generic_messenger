use std::{error::Error, fmt::Debug};

use async_trait::async_trait;
use types::{Message, MsgsStore, User};

pub mod discord;
mod network;
pub mod types;

pub trait Messanger: Send + Sync {
    // ID & Auth
    fn name(&self) -> String;
    fn auth(&self) -> String;
    // Features - TODO: Replace when up-casting will become stable https://github.com/rust-lang/rust/issues/65991
    fn query(&self) -> Option<&dyn MessangerQuery> {
        None
    }
    fn param_query(&self) -> Option<&dyn ParameterizedMessangerQuery> {
        None
    }
}
impl PartialEq for dyn Messanger {
    fn eq(&self, other: &Self) -> bool {
        format!("{}{}", self.name(), self.auth()) == format!("{}{}", other.name(), other.auth())
    }
}
impl Debug for dyn Messanger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Messanger")
            .field("Service", &self.name())
            .finish()
    }
}

// TODO: Remove the async trait when we will be able to create safe objects out
// of traits with async functions
#[async_trait]
pub trait MessangerQuery {
    async fn get_profile(&self) -> Result<User, Box<dyn Error + Sync + Send>>; // Fetch client profile
    async fn get_contacts(&self) -> Result<Vec<User>, Box<dyn Error + Sync + Send>>; // Users from friend list etc
    async fn get_conversation(&self) -> Result<Vec<MsgsStore>, Box<dyn Error + Sync + Send>>; // List of DMs
    async fn get_guilds(&self) -> Result<Vec<MsgsStore>, Box<dyn Error + Sync + Send>>; // Large groups that can have over a 100 people in them.
}

#[async_trait]
pub trait ParameterizedMessangerQuery: Send + Sync {
    async fn get_messanges(
        &self,
        msgs_location: MsgsStore,
        load_from_msg: Option<Message>,
    ) -> Result<Vec<Message>, Box<dyn Error + Sync + Send>>;
}
