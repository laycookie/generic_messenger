//! Adapters from Discord-specific state and protocols to `messenger_interface`.

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use messenger_interface::{
    auth::{AuthMethod, Credential, InteractiveAuthMethod},
    interface::Messenger,
    owner::Owner,
};
use rsa::sha2::{Digest as _, Sha256};

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

    /// Stable per-account identifier without exposing the bearer token through
    /// logs, persistence, telemetry, or UI diagnostics that carry messenger IDs.
    fn id(&self) -> String {
        let digest = Sha256::digest(self.api.token().unsecure().as_bytes());
        format!("{INTERFACE_NAME}:{}", URL_SAFE_NO_PAD.encode(digest))
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

#[cfg(test)]
mod tests {
    use secure_string::SecureString;

    use super::*;

    #[test]
    fn messenger_id_is_stable_and_never_contains_token() {
        let first = InnerDiscord::build(SecureString::from("secret-token-one"));
        let same = InnerDiscord::build(SecureString::from("secret-token-one"));
        let other = InnerDiscord::build(SecureString::from("secret-token-two"));

        let id = first.get().id();
        assert_eq!(id, same.get().id());
        assert_ne!(id, other.get().id());
        assert!(!id.contains("secret-token-one"));
    }
}
