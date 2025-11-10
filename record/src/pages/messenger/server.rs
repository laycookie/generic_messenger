use adaptors::types::{Chan, Identifier};

use crate::messanger_unifier::MessangerHandle;

#[derive(Debug, Clone)]
pub struct Server {
    pub handle: MessangerHandle,
    pub channels: Vec<Identifier<Chan>>, // TODO: Move this into cache
}

impl Server {
    pub fn new(handle: MessangerHandle, channels: Vec<Identifier<Chan>>) -> Self {
        Self { handle, channels }
    }
}
