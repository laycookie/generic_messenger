use std::{fmt::Debug, sync::RwLock};

use uuid::Uuid;

use crate::{Messanger, MessangerQuery, ParameterizedMessangerQuery};

pub mod json_structs;
pub mod rest_api;

pub struct Discord {
    uuid: Uuid,
    token: String, // TODO: Make it secure
    // Data
    dms: RwLock<Vec<json_structs::Channel>>,
}

impl Discord {
    pub fn new(token: &str) -> Discord {
        Discord {
            uuid: Uuid::new_v4(),
            token: token.into(),
            dms: RwLock::new(Vec::new()),
        }
    }
}

impl Debug for Discord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discord").finish()
    }
}

impl Messanger for Discord {
    fn name(&self) -> String {
        "Discord".into()
    }
    fn auth(&self) -> String {
        self.token.clone()
    }
    fn uuid(&self) -> Uuid {
        self.uuid
    }
    fn query(&self) -> Option<&dyn MessangerQuery> {
        Some(self)
    }
    fn param_query(&self) -> Option<&dyn ParameterizedMessangerQuery> {
        Some(self)
    }
}
