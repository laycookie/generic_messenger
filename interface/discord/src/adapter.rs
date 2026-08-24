//! Adapters from Discord-specific state and protocols to `messenger_interface`.

use async_trait::async_trait;
use messenger_interface::{
    auth::{AuthMethod, Credential, InteractiveAuthMethod},
    interface::Messenger,
    owner::Owner,
};

use crate::{INTERFACE_NAME, InnerDiscord, auth};

pub(crate) mod audio;
pub(crate) mod query;
pub(crate) mod text;
pub(crate) mod video;

#[async_trait]
impl Messenger for InnerDiscord {
    /// A `Token` builds immediately; a `Password` performs the username/password
    /// (+ optional MFA) exchange and builds from the resolved token.
    async fn create_messenger(
        cred: Credential,
    ) -> Result<Owner<dyn Messenger>, Box<dyn std::error::Error + Send + Sync>> {
        let credential = auth::login_with_credentials(cred).await?;
        let auth::DiscordAuth::Token { token } = auth::DiscordAuth::try_from(credential)? else {
            unreachable!("credential resolution always returns a token")
        };
        Ok(Self::build(token))
    }

    fn auth_methods() -> &'static [AuthMethod] {
        auth::DiscordAuth::auth_methods()
    }

    fn interactive_auth_methods() -> &'static [InteractiveAuthMethod] {
        auth::INTERACTIVE_AUTH_METHODS
    }

    /// NOTE: the id is meant to be used only on the client, for identification
    /// purposes, and is never supposed to be sent anywhere, so embedding the
    /// token here does not expose it to Discord.
    fn id(&self) -> String {
        INTERFACE_NAME.to_string() + self.api.token().unsecure()
    }

    fn name(&self) -> &'static str {
        INTERFACE_NAME
    }

    /// Persist the resolved token so password login becomes token login next
    /// start.
    fn auth(&self) -> Credential {
        auth::DiscordAuth::Token {
            token: self.api.token().clone(),
        }
        .into()
    }
}
