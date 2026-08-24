#![feature(arbitrary_self_types)]

use messenger_interface::{
    auth::{AuthMethod, Credential, InteractiveAuthMethod},
    interface::Messenger,
    owner::Owner,
};

mod adapter;
pub(crate) mod api;
mod auth;
mod backend;
mod cache;
mod downloaders;
mod gateways;
mod remote_auth;
mod rest_api;

pub(crate) use backend::{
    Capabilities, DISCORD_API, DiscordRuntimeState, INTERFACE_NAME, InnerDiscord, Intents,
    LocalMediaState,
};

/// Public interface for creating the discord messenger
pub struct Discord;
impl Discord {
    /// Construct a Discord messenger from a [`Credential`]; see
    /// [`Messenger::create_messenger`]. A `Token` builds immediately; a
    /// `Password` (email/phone + password, optional MFA) fetches the token from
    /// Discord up front, so a rejected login surfaces here rather than on first use.
    pub async fn create_messenger(
        cred: Credential,
    ) -> Result<Owner<dyn Messenger>, Box<dyn std::error::Error + Send + Sync>> {
        <InnerDiscord as Messenger>::create_messenger(cred).await
    }
    /// Credential-form login methods Discord offers: token or email/phone + password.
    pub fn auth_methods() -> &'static [AuthMethod] {
        <InnerDiscord as Messenger>::auth_methods()
    }
    /// Interactive login methods Discord offers.
    pub fn interactive_auth_methods() -> &'static [InteractiveAuthMethod] {
        <InnerDiscord as Messenger>::interactive_auth_methods()
    }
}
