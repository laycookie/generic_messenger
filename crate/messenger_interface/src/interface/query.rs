//! Read-only state fetching ([`Query`]) and its realtime [`QueryEvent`]s.

use std::{error::Error, sync::Arc};

use async_trait::async_trait;

use super::MessengerError;
use crate::stream::WeakSocketStream;
use crate::types::{Details, House, ID, Identifier, Place, Room, User, UserStatus};

/// Query-only API for fetching state from a messenger.
///
/// This is intentionally read-only; mutations belong in more specialized traits.
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
    /// Fetch a backend-specific detail sheet about a user, for an
    /// inspection UI.
    async fn inspect_user(
        &self,
        _user: &Identifier<User>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
    /// Fetch a backend-specific detail sheet about a house/server/guild,
    /// for an inspection UI.
    async fn inspect_house(
        &self,
        _house: &Identifier<Place<House>>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }

    /// Subscribe to realtime state changes that invalidate or extend query
    /// results, such as newly-created rooms and presence updates.
    ///
    /// Intended for one long-lived consumer per backend. The returned weak
    /// stream does not keep the backend alive. REST-only implementations may
    /// synthesize these events with polling. The default reports
    /// [`MessengerError::NotImplemented`].
    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<QueryEvent>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
}

pub enum QueryEvent {
    /// A channel/room was created (optionally within a server/place).
    ChannelCreated {
        r#where: Option<Identifier<()>>,
        // r#where: Option<Identifier<Place<House>>>,
        room: Identifier<Place<Room>>,
    },
    /// A channel/room disappeared. `where` identifies its former house when
    /// known; consumers may remove by `room_id` globally within this backend.
    ChannelDeleted {
        r#where: Option<Identifier<()>>,
        room_id: ID,
    },
    /// A user's presence/status changed.
    UserStatusChanged { user_id: ID, status: UserStatus },
}
