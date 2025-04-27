use std::path::PathBuf;

use uuid::Uuid;

// Legacy
#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub username: String,
}

// New
#[derive(Debug, Clone)]
pub struct Store {
    pub origin_uuid: Uuid,
    pub(crate) hash: Option<String>, // Used in cases where ID can change
    pub(crate) id: String,           // ID of a location
    pub name: String,
    pub icon: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub(crate) id: String,
    pub sender: Store,
    pub text: String,
}
