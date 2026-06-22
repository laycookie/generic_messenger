pub mod auth;
pub mod connection;
mod eresult;
mod game_coordinator;
pub mod message;
mod net;
mod serverlist;
mod service_method;
mod session;
mod transport;

pub use connection::{Connection, ConnectionTrait, ReadonlyConnection};
pub use eresult::EResult;
pub use game_coordinator::{GameCoordinator, handshake::GenericGCHandshake};
pub use message::NetMessage;
pub use net::{NetMessageHeader, NetworkError, RawNetMessage};
pub use serverlist::{DiscoverOptions, ServerDiscoveryError, ServerList};
pub use service_method::ServiceMethodRequest;
pub use session::{ConnectionError, LoginError};
