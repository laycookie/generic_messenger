//! Minimal Steam messenger adapter.
//!
//! Talks to the Steam network with [`steam_vent`], a reverse-engineered
//! implementation of the Steam client protocol (the same approach as
//! SteamKit, but native Rust — so nothing needs to be embedded). It covers
//! just the basics: the client profile, the friends list, reading recent
//! direct-message history, and sending a direct message.
//!
//! # Module layout
//!
//! - [`api_types`]: Steam protocol constants and the small cache/value types
//!   ([`FriendEntry`](api_types::FriendEntry), the chat-room/group structs) plus
//!   the ID-translation helpers.
//! - [`downloaders`]: best-effort avatar/emoticon/sticker caching and the
//!   `Identifier<User>` resolution built on it.
//! - [`session`]: the live [`Connected`](session::Connected) session and the
//!   friend/persona/message update streams it drives.
//! - [`query`]: the `Query`/`Text` implementations.
//! - [`voice`]: the `Audio` capability — currently a signaling-only scaffold
//!   (join/leave a Steam voice session; no audio transport yet).
//! - [`gns`]: GameNetworkingSockets P2P media — an integration skeleton for the
//!   not-yet-functional audio path (see the module docs for what's blocked).
//! - [`rich`]: BBCode → display-content parsing for Steam messages.
//!
//! # Steam worker bridge
//!
//! `steam_vent` manages its protocol socket loop and heartbeat internally,
//! while the rest of this app runs on the futures side. To keep this
//! interface crate futures-first, [`SteamMessenger::run`] wraps self-contained
//! `steam_vent` futures with `async_compat`. Adapter-local work such as cache
//! downloads stays outside that bridge.
//!
//! # Auth & the "secret"
//!
//! Steam logs in with username + password (+ Steam Guard), not a single token.
//! To make restarts seamless without re-prompting for Steam Guard, the adapter
//! keeps a single mutable **secret** that starts as the password and becomes a
//! **refresh token** after the first successful login:
//!
//! - First login uses the password and a Steam Guard confirmation (see below).
//!   The session it yields carries a refresh token ([`Connection::access_token`]);
//!   we store that as the new secret.
//! - [`Messenger::auth`] serializes `"username:secret"`, so the host app's
//!   `LoginInfo` file persists the refresh token (no separate cache file). A
//!   refresh token is a JWT and contains no `:`, so the existing
//!   `split_once(':')` round-trip stays unambiguous.
//! - On the next start the secret looks like a JWT, so we reconnect with
//!   [`Connection::access`] — re-establishing the session straight from the
//!   token, with no password and no Steam Guard.
//!
//! If the saved token is rejected (expired/revoked) the user simply logs in
//! again. Treat the stored secret as password-equivalent — it grants account
//! access until revoked.
//!
//! ## Steam Guard confirmation (first login only)
//!
//! Chosen by whether a code was supplied to [`Steam::login`]:
//! - **A typed code** (TOTP from the authenticator app, or an emailed code) is
//!   fed to steam-vent through an in-memory reader.
//! - **No code** falls back to **mobile-app approval**: steam-vent polls until
//!   you approve the login on your phone.
//!
//! We deliberately do *not* use steam-vent's `ConsoleAuthConfirmationHandler`:
//! it reads the code from the process's terminal stdin, which in a GUI app is
//! an invisible prompt that never receives input, so login hangs forever.

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
    /// Construct a Steam messenger from a [`Credential`]; see
    /// [`Messenger::create_messenger`]. Both supported variants build
    /// synchronously — the actual login happens lazily on first use (Steam Guard,
    /// if needed, is mobile approval unless a code is supplied).
    pub async fn create_messenger(
        cred: Credential,
    ) -> Result<Arc<dyn Messenger>, Box<dyn Error + Send + Sync>> {
        <SteamMessenger as Messenger>::create_messenger(cred).await
    }
    /// The login methods Steam offers (username + password, optional Steam Guard).
    pub fn auth_methods() -> &'static [AuthMethod] {
        <SteamMessenger as Messenger>::auth_methods()
    }
}

pub(crate) struct SteamMessenger {
    username: String,
    /// Current credential: the user's password on first login, replaced by a
    /// Steam refresh token after a successful login. [`Messenger::auth`]
    /// serializes this, so the host app persists the token to `LoginInfo`.
    secret: Mutex<SecureString>,
    /// Optional Steam Guard code for the initial password login.
    guard_code: Option<String>,
    /// Lazily-established session, behind an async mutex so concurrent first
    /// callers log in exactly once.
    connected: AsyncMutex<Option<Arc<Connected>>>,
    /// Sink for [`Audio::listen`]'s event stream, set when the app starts
    /// listening. `connect` pushes `CallStreamReady` here to hand the UI the
    /// call's audio stream. `None` until `listen` is called.
    voice_events: Mutex<Option<mpsc::UnboundedSender<VoiceEvent>>>,
    /// Strong handle to the `listen()` stream object. `WeakSocketStream` only
    /// holds a `Weak`, so the source must be kept alive here or the app's stream
    /// dies immediately.
    voice_stream: Mutex<Option<Arc<gns::call::VoiceEventStream>>>,
    /// Active calls by room, keeping each audio session alive (again, the app
    /// holds only a `Weak`). `disconnect` removes + kills the entry.
    active_calls: DashMap<ID, Arc<gns::call::SteamAudioSession>>,
}

impl SteamMessenger {
    fn build(username: String, secret: String, guard_code: Option<String>) -> Arc<dyn Messenger> {
        Arc::new(SteamMessenger {
            username,
            secret: Mutex::new(secret.into()),
            guard_code,
            connected: AsyncMutex::new(None),
            voice_events: Mutex::new(None),
            voice_stream: Mutex::new(None),
            active_calls: DashMap::new(),
        })
    }

    /// Concrete-typed constructor for the in-crate voice-signaling capture test,
    /// which needs the `pub(crate)` session helpers the `dyn Messenger` object hides.
    #[cfg(test)]
    pub(crate) fn for_capture(
        username: String,
        secret: String,
        guard_code: Option<String>,
    ) -> Arc<Self> {
        Arc::new(SteamMessenger {
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
                // Persist the (possibly refreshed) session token, so `auth()`
                // saves it to LoginInfo and the next start skips Steam Guard.
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

/// Steam's login shapes. `Password` is the interactive form; `Session` is the
/// hidden shape (username + refreshed token) reconstructed from storage, never
/// shown as a login method.
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
    /// `Password` (username + password [+ Steam Guard]) and `Session` (username +
    /// refreshed token) both build synchronously; login happens lazily on first
    /// use.
    async fn create_messenger(
        cred: Credential,
    ) -> Result<Arc<dyn Messenger>, Box<dyn Error + Send + Sync>> {
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
    /// Username + refreshed session token, persisted so the next start restores
    /// the session (and skips Steam Guard).
    fn auth(&self) -> Credential {
        Credential(vec![
            CredentialField::new("username", self.username.clone()),
            CredentialField::new("token", self.secret.lock().unwrap().clone()),
        ])
    }
}
