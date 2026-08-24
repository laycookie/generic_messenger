//! Read-only state fetching ([`Query`]) and its realtime [`QueryEvent`]s.

use std::error::Error;

use async_trait::async_trait;

use super::MessengerError;
use crate::types::{Details, House, ID, Identifier, Place, Room, User, UserStatus};

/// Read-only messenger state API.
#[async_trait]
pub trait Query: Send + Sync {
    /// Fetch the current "client user" (the authenticated account / profile).
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch users that the messenger considers "contacts" (friends, following, etc).
    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Fetch all rooms/channels available to the client.
    async fn rooms(&self) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch all houses/servers/guilds available to the client.
    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch detailed information about a specific room/channel.
    async fn room_details(
        &self,
        _room: Identifier<Place<Room>>,
    ) -> Result<Room, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch detailed information about a specific house/server/guild.
    async fn house_details(
        &self,
        _house: Identifier<Place<House>>,
    ) -> Result<House, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch backend-specific user details.
    async fn inspect_user(
        &self,
        _user: &Identifier<User>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch backend-specific house details.
    async fn inspect_house(
        &self,
        _house: &Identifier<Place<House>>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Prepare this backend's query event source.
    async fn prepare_listener(&self) -> Result<(), Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Polls next prepared event.
    fn next_event(self: crate::owner::Retained<Self>) -> crate::stream::EventFuture<QueryEvent> {
        Box::pin(async { None })
    }
}

pub enum QueryEvent {
    /// A channel/room was created (optionally within a server/place).
    ChannelCreated {
        r#where: Option<Identifier<()>>,
        room: Identifier<Place<Room>>,
    },
    /// A room disappeared; `where` carries its former house when known.
    ChannelDeleted {
        r#where: Option<Identifier<()>>,
        room_id: ID,
    },
    /// A user's presence/status changed.
    UserStatusChanged { user_id: ID, status: UserStatus },
}
