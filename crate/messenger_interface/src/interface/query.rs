//! Read-only state fetching ([`Query`]) and its realtime [`QueryEvent`]s.

use std::{error::Error, sync::Arc};

use async_trait::async_trait;

use super::MessengerError;
use super::caster::capability_caster;
use crate::stream::WeakSocketStream;
use crate::types::{Details, House, ID, Identifier, Place, Room, User, UserStatus};

capability_caster!(
    /// Capability: fetching user presence/status indicators.
    QueryCasterUserStatuses: Query => UserStatuses { statuses, arc_statuses }
);

/// Query-only API for fetching state from a messenger.
///
/// This is intentionally read-only; mutations belong in more specialized traits.
#[async_trait]
pub trait Query: Send + Sync + QueryCasterUserStatuses {
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
    async fn listen(
        self: Arc<Self>,
    ) -> Result<WeakSocketStream<QueryEvent>, Box<dyn Error + Sync + Send>> {
        Err(Box::new(MessengerError::NotImplemented))
    }
}

/// Capability: user presence/status indicators.
///
/// Backends that do not implement this leave [`User::status`] as `None`, and
/// callers should render no status indicator for those users.
#[async_trait]
pub trait UserStatuses: Query {
    /// Fetch statuses for known users. Missing entries mean the backend did
    /// not report a status for that specific user.
    async fn user_statuses(
        &self,
        users: &[Identifier<User>],
    ) -> Result<Vec<(ID, UserStatus)>, Box<dyn Error + Sync + Send>>;
}

pub enum QueryEvent {
    /// A channel/room was created (optionally within a server/place).
    ChannelCreated {
        r#where: Option<Identifier<()>>,
        // r#where: Option<Identifier<Place<House>>>,
        room: Identifier<Place<Room>>,
    },
    /// A user's presence/status changed.
    UserStatusChanged { user_id: ID, status: UserStatus },
}
