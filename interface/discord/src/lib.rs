use std::sync::Arc;

use crate::api::SNOWFLAKE;
use messenger_interface::{
    auth::{AuthMethod, Credential, InteractiveAuthMethod},
    interface::Messenger,
    types::Identifier,
};

mod adapter;
pub(crate) mod api;
mod auth;
mod backend;
mod cache;
mod downloaders;
mod gateways;
mod lazy_arc;
mod remote_auth;
mod rest_api;
mod stream_poll;

pub(crate) use backend::{
    Capabilities, DISCORD_API, INTERFACE_NAME, InnerDiscord, Intents, LocalMediaState, Owned,
};
pub(crate) use unitstruct::UnitStruct;

/// Public interface for creating the discord messenger
pub struct Discord;
impl Discord {
    /// Construct a Discord messenger from a [`Credential`]; see
    /// [`Messenger::create_messenger`]. A `Token` builds immediately; a
    /// `Password` (email/phone + password, optional MFA) fetches the token from
    /// Discord up front, so a rejected login surfaces here rather than on first use.
    pub async fn create_messenger(
        cred: Credential,
    ) -> Result<Arc<dyn Messenger>, Box<dyn std::error::Error + Send + Sync>> {
        <InnerDiscord<Owned> as Messenger>::create_messenger(cred).await
    }
    /// Credential-form login methods Discord offers: token or email/phone + password.
    pub fn auth_methods() -> &'static [AuthMethod] {
        <InnerDiscord<Owned> as Messenger>::auth_methods()
    }
    /// Interactive login methods Discord offers.
    pub fn interactive_auth_methods() -> &'static [InteractiveAuthMethod] {
        <InnerDiscord<Owned> as Messenger>::interactive_auth_methods()
    }
    fn identifier_generator<D>(id: SNOWFLAKE, data: D) -> Identifier<D> {
        Identifier::new(id, data)
    }
}
