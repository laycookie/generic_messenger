use std::error::Error;
use std::fmt::Debug;

use async_trait::async_trait;
use types::{Message, Store, User};
use uuid::Uuid;

pub mod discord;
mod network;
pub mod types;

pub trait Messanger: Send + Sync + Debug {
    // ID & Auth
    fn name(&self) -> String;
    fn auth(&self) -> String;
    fn uuid(&self) -> Uuid;
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

// TODO: Remove the async trait when we will be able to create safe objects out
// of traits with async functions
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
