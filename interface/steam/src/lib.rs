#![feature(arbitrary_self_types)]

//! Steam profile, text, and group-voice adapter using `steam_vent`.
//!
//! Protocol futures cross into this futures-based crate through `async_compat`.
//! Password login uses typed Steam Guard code or mobile approval, then replaces
//! password with persisted refresh token. Treat token as password-equivalent;
//! rejected tokens require login again. Console confirmation is avoided because
//! GUI has no usable terminal prompt.

use std::{
    error::Error,
    future::Future,
    sync::{Arc, Mutex, atomic::Ordering},
};

use async_compat::CompatExt;
use dashmap::DashMap;
use futures::channel::mpsc;
use futures::lock::Mutex as AsyncMutex;
use secure_string::SecureString;
use tracing::{error, info};

use async_trait::async_trait;
use messenger_interface::auth::{AuthMethod, Credential, CredentialField, CredentialSchema};
use messenger_interface::interface::{Messenger, VoiceEvent};
use messenger_interface::owner::Owner;
use messenger_interface::types::ID;

use crate::session::Connected;

mod api_types;
#[cfg(test)]
mod capture;
mod downloaders;
mod gns;
mod query;
mod rich;
mod session;
mod voice;

/// Public entry point, mirroring `discord::Discord`.
pub struct Steam;
impl Steam {
    /// Builds messenger; login remains lazy until first use.
    pub async fn create_messenger(
        cred: Credential,
    ) -> Result<Owner<dyn Messenger>, Box<dyn Error + Send + Sync>> {
        <SteamMessenger as Messenger>::create_messenger(cred).await
    }
    /// The login methods Steam offers (username + password, optional Steam Guard).
    pub fn auth_methods() -> &'static [AuthMethod] {
        <SteamMessenger as Messenger>::auth_methods()
    }
}

pub(crate) struct SteamMessenger {
    username: String,
    /// Password replaced by persisted refresh token after login.
    secret: Mutex<SecureString>,
    /// Optional Steam Guard code for the initial password login.
    guard_code: Option<String>,
    /// Lazy session; mutex serializes first login.
    connected: AsyncMutex<Option<Arc<Connected>>>,
    /// Prepared voice-event sink.
    voice_events: Mutex<Option<mpsc::UnboundedSender<VoiceEvent>>>,
    /// Strong handle for weak nested voice source.
    voice_stream: Mutex<Option<Arc<gns::call::VoiceEventStream>>>,
    /// Active call owners by room; disconnect removes and kills.
    active_calls: DashMap<ID, Arc<gns::call::SteamAudioSession>>,
}

impl SteamMessenger {
    fn build(username: String, secret: String, guard_code: Option<String>) -> Owner<dyn Messenger> {
        let owner: Owner<dyn Messenger> = Owner::new(SteamMessenger {
            username,
            secret: Mutex::new(secret.into()),
            guard_code,
            connected: AsyncMutex::new(None),
            voice_events: Mutex::new(None),
            voice_stream: Mutex::new(None),
            active_calls: DashMap::new(),
        });
        owner
    }

    /// Concrete owner for tests requiring hidden session helpers.
    #[cfg(test)]
    pub(crate) fn for_capture(
        username: String,
        secret: String,
        guard_code: Option<String>,
    ) -> Owner<Self> {
        Owner::new(SteamMessenger {
            username,
            secret: Mutex::new(secret.into()),
            guard_code,
            connected: AsyncMutex::new(None),
            voice_events: Mutex::new(None),
            voice_stream: Mutex::new(None),
            active_calls: DashMap::new(),
        })
    }

    /// Run a `steam_vent` future from the futures side by entering the
    /// compatibility context supplied by `async_compat`.
    pub(crate) async fn run<F, T>(&self, fut: F) -> Result<T, Box<dyn Error + Send + Sync>>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        Ok(fut.compat().await)
    }

    /// Get the live session, logging in on first use.
    pub(crate) async fn connected(&self) -> Result<Arc<Connected>, Box<dyn Error + Send + Sync>> {
        let mut guard = self.connected.lock().await;
        if let Some(connected) = guard.as_ref() {
            if connected.alive.load(Ordering::Acquire) {
                return Ok(connected.clone());
            }
            info!("Steam: previous session ended; re-establishing from saved token");
        }
        let username = self.username.clone();
        let secret = self.secret.lock().unwrap().unsecure().to_owned();
        let guard_code = self.guard_code.clone();
        match self
            .run(Connected::establish(username, secret, guard_code))
            .await?
        {
            Ok(connected) => {
                // Persist refreshed token for future restore.
                if let Some(token) = connected.conn.access_token() {
                    *self.secret.lock().unwrap() = token.to_owned().into();
                }
                let connected = Arc::new(connected);
                *guard = Some(connected.clone());
                Ok(connected)
            }
            Err(err) => {
                error!("Steam: login failed: {err}");
                Err(err)
            }
        }
    }
}

/// Interactive password and hidden persisted-session login forms.
#[derive(CredentialSchema)]
enum SteamAuth {
    #[credential(method = "Username & password")]
    Password {
        #[credential(label = "Username")]
        username: SecureString,
        #[credential(label = "Password", secret)]
        password: SecureString,
        #[credential(label = "Steam Guard code (blank = approve on mobile)")]
        otp: Option<SecureString>,
    },
    #[credential(hidden)]
    Session {
        username: SecureString,
        token: SecureString,
    },
}

#[async_trait]
impl Messenger for SteamMessenger {
    /// Builds synchronously; authenticates lazily on first use.
    async fn create_messenger(
        cred: Credential,
    ) -> Result<Owner<dyn Messenger>, Box<dyn Error + Send + Sync>> {
        match SteamAuth::try_from(cred)? {
            SteamAuth::Password {
                username,
                password,
                otp,
            } => Ok(SteamMessenger::build(
                username.unsecure().to_owned(),
                password.unsecure().to_owned(),
                otp.map(|otp| otp.unsecure().to_owned()),
            )),
            SteamAuth::Session { username, token } => Ok(SteamMessenger::build(
                username.unsecure().to_owned(),
                token.unsecure().to_owned(),
                None,
            )),
        }
    }

    fn auth_methods() -> &'static [AuthMethod] {
        SteamAuth::auth_methods()
    }

    /// Client-local identifier only; never sent anywhere.
    fn id(&self) -> String {
        format!("Steam{}", self.username)
    }
    fn name(&self) -> &'static str {
        "Steam"
    }
    /// Persisted username and refresh token.
    fn auth(&self) -> Credential {
        Credential(vec![
            CredentialField::new("username", self.username.clone()),
            CredentialField::new("token", self.secret.lock().unwrap().clone()),
        ])
    }
}
