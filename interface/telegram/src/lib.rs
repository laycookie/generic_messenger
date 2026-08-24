#![feature(arbitrary_self_types)]

//! Text-only Telegram user-client adapter.
//!
//! Uses Telegram's MTProto API through `grammers`. Login is interactive:
//! submitting API credentials and phone number sends a code; subsequent
//! submissions complete code and optional two-factor-password challenges.
//! Successful authorization is kept in a local SQLite session.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    error::Error,
    fs,
    future::Future,
    hash::Hasher as _,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::lock::Mutex as AsyncMutex;
use grammers_client::{
    Client, SenderPool, SignInError,
    client::{LoginToken, PasswordToken, UpdateStream},
    peer::{Peer, User as GrammersUser},
};
use grammers_session::{
    storages::SqliteSession,
    types::{PeerId, PeerKind, PeerRef},
    updates::UpdatesLike,
};
use messenger_interface::{
    auth::{AuthMethod, Credential, CredentialField, CredentialSchema},
    interface::{Messenger, QueryEvent, TextEvent},
    owner::Owner,
    types::{ID, Revision},
};
use secure_string::SecureString;
use tokio::{
    runtime::{Builder, Runtime},
    sync::{Mutex as TokioMutex, mpsc::UnboundedReceiver},
};

mod query;
mod text;

pub(crate) type BoxError = Box<dyn Error + Send + Sync>;

pub(crate) const INTERFACE_NAME: &str = "Telegram";
pub(crate) const MAX_CACHED_MESSAGES_PER_CHAT: usize = 1_000;
const SESSION_DIRECTORY: &str = "cache/Telegram";
const PENDING_LOGIN_LIFETIME: Duration = Duration::from_secs(10 * 60);

static TELEGRAM_RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("telegram")
        .build()
        .expect("Telegram Tokio runtime should initialize")
});

static PENDING_LOGINS: LazyLock<Mutex<HashMap<ID, PendingLogin>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Public entry point, matching other adapters.
pub struct Telegram;

impl Telegram {
    pub async fn create_messenger(cred: Credential) -> Result<Owner<dyn Messenger>, BoxError> {
        <TelegramMessenger as Messenger>::create_messenger(cred).await
    }

    pub fn auth_methods() -> &'static [AuthMethod] {
        <TelegramMessenger as Messenger>::auth_methods()
    }
}

pub(crate) async fn telegram_task<F, T>(future: F) -> Result<T, BoxError>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let task = TELEGRAM_RUNTIME.spawn(future);
    let _abort_on_drop = AbortOnDrop(task.abort_handle());
    task.await
        .map_err(|error| message_error(format!("Telegram runtime task failed: {error}")))
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn message_error(message: impl Into<String>) -> BoxError {
    Box::new(std::io::Error::other(message.into()))
}

struct ClientBundle {
    client: Client,
    updates: UnboundedReceiver<UpdatesLike>,
    session: Arc<SqliteSession>,
}

async fn open_client(api_id: i32, session_path: &Path) -> Result<ClientBundle, BoxError> {
    prepare_session_path(session_path)?;
    let path = session_path.to_owned();
    let bundle = telegram_task(async move {
        let session = Arc::new(SqliteSession::open(path).await?);
        let SenderPool {
            runner,
            updates,
            handle,
        } = SenderPool::new(Arc::clone(&session), api_id);
        let client = Client::new(handle);
        drop(tokio::spawn(runner.run()));
        Ok::<_, BoxError>(ClientBundle {
            client,
            updates,
            session,
        })
    })
    .await??;
    harden_session_file(session_path)?;
    Ok(bundle)
}

fn prepare_session_path(path: &Path) -> Result<(), BoxError> {
    if !valid_session_path(path) {
        return Err(message_error(
            "Telegram session path must name one .session file inside ./cache/Telegram",
        ));
    }

    fs::create_dir_all(SESSION_DIRECTORY)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(SESSION_DIRECTORY, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn harden_session_file(path: &Path) -> Result<(), BoxError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn valid_session_path(path: &Path) -> bool {
    let mut components = path.components();
    if matches!(components.clone().next(), Some(Component::CurDir)) {
        components.next();
    }
    let cache_matches =
        matches!(components.next(), Some(Component::Normal(value)) if value == "cache");
    let telegram_matches =
        matches!(components.next(), Some(Component::Normal(value)) if value == "Telegram");
    let file_matches = matches!(
        components.next(),
        Some(Component::Normal(value))
            if value.to_str().is_some_and(|name| name.ends_with(".session"))
    );
    cache_matches && telegram_matches && file_matches && components.next().is_none()
}

fn session_path(api_id: i32, phone: &str) -> PathBuf {
    let key = stable_id("session", &format!("{api_id}:{phone}"));
    Path::new(SESSION_DIRECTORY).join(format!("{key:016x}.session"))
}

fn parse_api_id(value: &SecureString) -> Result<i32, BoxError> {
    let api_id = value
        .unsecure()
        .trim()
        .parse::<i32>()
        .map_err(|_| message_error("Telegram API ID must be a positive integer"))?;
    if api_id <= 0 {
        return Err(message_error("Telegram API ID must be a positive integer"));
    }
    Ok(api_id)
}

fn parse_api_hash(value: &SecureString) -> Result<String, BoxError> {
    let hash = value.unsecure().trim();
    if hash.len() != 32 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(message_error(
            "Telegram API hash must contain 32 hexadecimal characters",
        ));
    }
    Ok(hash.to_owned())
}

fn parse_phone(value: &SecureString) -> Result<String, BoxError> {
    let phone = value.unsecure().trim();
    if phone.is_empty() {
        return Err(message_error("Telegram phone number cannot be empty"));
    }
    Ok(phone.to_owned())
}

fn optional_trimmed(value: Option<SecureString>) -> Option<String> {
    value.and_then(|value| {
        let value = value.unsecure().trim().to_owned();
        (!value.is_empty()).then_some(value)
    })
}

fn optional_password(value: Option<SecureString>) -> Option<String> {
    value.and_then(|value| {
        let password = value.unsecure();
        (!password.trim().is_empty()).then(|| password.to_owned())
    })
}

fn pending_key(api_id: i32, api_hash: &str, phone: &str) -> ID {
    stable_id("pending-login", &format!("{api_id}:{api_hash}:{phone}"))
}

fn take_pending_login(key: ID) -> Option<PendingLogin> {
    let mut pending = PENDING_LOGINS.lock().unwrap();
    pending.retain(|_, login| login.started.elapsed() < PENDING_LOGIN_LIFETIME);
    pending.remove(&key)
}

fn store_pending_login(key: ID, pending: PendingLogin) {
    PENDING_LOGINS.lock().unwrap().insert(key, pending);
}

enum PendingPhase {
    Code(LoginToken),
    Password(Box<PasswordToken>),
}

struct PendingLogin {
    bundle: ClientBundle,
    phase: PendingPhase,
    started: Instant,
}

struct LoginAttempt {
    key: ID,
    api_id: i32,
    api_hash: String,
    phone: String,
    session_path: PathBuf,
    code: Option<String>,
    password: Option<String>,
}

#[derive(CredentialSchema)]
enum TelegramAuth {
    #[credential(method = "User account")]
    UserLogin {
        #[credential(label = "API ID")]
        api_id: SecureString,
        #[credential(label = "API hash", secret)]
        api_hash: SecureString,
        #[credential(label = "Phone number")]
        phone: SecureString,
        #[credential(label = "Login code")]
        code: Option<SecureString>,
        #[credential(label = "Two-factor password", secret)]
        password: Option<SecureString>,
    },
    #[credential(hidden)]
    Session {
        api_id: SecureString,
        api_hash: SecureString,
        session: SecureString,
    },
}

pub(crate) struct TelegramMessenger {
    api_id: i32,
    api_hash: SecureString,
    session_path: PathBuf,
    account_id: i64,
    client: Client,
    _session: Arc<SqliteSession>,
    updates: Arc<TokioMutex<TelegramUpdates>>,
    me: TelegramUser,
    state: Mutex<TelegramState>,
    dialog_lock: AsyncMutex<()>,
    poll_lock: AsyncMutex<()>,
}

impl TelegramMessenger {
    async fn finish_login(
        api_id: i32,
        api_hash: String,
        session_path: PathBuf,
        bundle: ClientBundle,
    ) -> Result<Owner<dyn Messenger>, BoxError> {
        let client = bundle.client.clone();
        let me = telegram_task(async move { client.get_me().await }).await??;
        let me = TelegramUser::from_user(&me);

        let owner: Owner<dyn Messenger> = Owner::new(Self {
            api_id,
            api_hash: api_hash.into(),
            session_path,
            account_id: me.external_id,
            client: bundle.client,
            _session: bundle.session,
            updates: Arc::new(TokioMutex::new(TelegramUpdates {
                raw: Some(bundle.updates),
                stream: None,
            })),
            me,
            state: Mutex::new(TelegramState::default()),
            dialog_lock: AsyncMutex::new(()),
            poll_lock: AsyncMutex::new(()),
        });
        Ok(owner)
    }

    async fn restore(
        api_id: SecureString,
        api_hash: SecureString,
        session: SecureString,
    ) -> Result<Owner<dyn Messenger>, BoxError> {
        let api_id = parse_api_id(&api_id)?;
        let api_hash = parse_api_hash(&api_hash)?;
        let session_path = PathBuf::from(session.unsecure().trim());
        if !valid_session_path(&session_path) {
            return Err(message_error("Stored Telegram session path is invalid"));
        }

        let bundle = open_client(api_id, &session_path).await?;
        let client = bundle.client.clone();
        let authorized = telegram_task(async move { client.is_authorized().await }).await??;
        if !authorized {
            return Err(message_error(
                "Stored Telegram session expired or was revoked; remove saved login and sign in again",
            ));
        }
        Self::finish_login(api_id, api_hash, session_path, bundle).await
    }

    async fn user_login(
        api_id: SecureString,
        api_hash: SecureString,
        phone: SecureString,
        code: Option<SecureString>,
        password: Option<SecureString>,
    ) -> Result<Owner<dyn Messenger>, BoxError> {
        let api_id = parse_api_id(&api_id)?;
        let api_hash = parse_api_hash(&api_hash)?;
        let phone = parse_phone(&phone)?;
        let code = optional_trimmed(code);
        let password = optional_password(password);
        let key = pending_key(api_id, &api_hash, &phone);
        let path = session_path(api_id, &phone);

        if let Some(pending) = take_pending_login(key) {
            return Self::continue_login(
                LoginAttempt {
                    key,
                    api_id,
                    api_hash,
                    phone,
                    session_path: path,
                    code,
                    password,
                },
                pending,
            )
            .await;
        }

        let bundle = open_client(api_id, &path).await?;
        let client = bundle.client.clone();
        let authorized = telegram_task(async move { client.is_authorized().await }).await??;
        if authorized {
            return Self::finish_login(api_id, api_hash, path, bundle).await;
        }

        let client = bundle.client.clone();
        let login_phone = phone;
        let login_hash = api_hash.clone();
        let token =
            telegram_task(
                async move { client.request_login_code(&login_phone, &login_hash).await },
            )
            .await??;
        store_pending_login(
            key,
            PendingLogin {
                bundle,
                phase: PendingPhase::Code(token),
                started: Instant::now(),
            },
        );
        Err(message_error(
            "Telegram login code sent. Enter it in Login code, then submit again",
        ))
    }

    async fn continue_login(
        attempt: LoginAttempt,
        pending: PendingLogin,
    ) -> Result<Owner<dyn Messenger>, BoxError> {
        let LoginAttempt {
            key,
            api_id,
            api_hash,
            phone,
            session_path,
            code,
            password,
        } = attempt;
        let PendingLogin {
            bundle,
            phase,
            started,
        } = pending;

        match phase {
            PendingPhase::Code(token) => {
                let Some(code) = code else {
                    let client = bundle.client.clone();
                    let login_hash = api_hash.clone();
                    let token = telegram_task(async move {
                        client.request_login_code(&phone, &login_hash).await
                    })
                    .await??;
                    store_pending_login(
                        key,
                        PendingLogin {
                            bundle,
                            phase: PendingPhase::Code(token),
                            started: Instant::now(),
                        },
                    );
                    return Err(message_error(
                        "New Telegram login code sent. Enter it, then submit again",
                    ));
                };

                let client = bundle.client.clone();
                let (result, token) = telegram_task(async move {
                    let result = client.sign_in(&token, &code).await;
                    (result, token)
                })
                .await?;

                match result {
                    Ok(_) => Self::finish_login(api_id, api_hash, session_path, bundle).await,
                    Err(SignInError::PasswordRequired(password_token)) => {
                        let hint = password_token
                            .hint()
                            .filter(|hint| !hint.is_empty())
                            .map(|hint| format!(" Hint: {hint}"))
                            .unwrap_or_default();
                        store_pending_login(
                            key,
                            PendingLogin {
                                bundle,
                                phase: PendingPhase::Password(Box::new(password_token)),
                                started,
                            },
                        );
                        Err(message_error(format!(
                            "Telegram two-factor password required.{hint} Enter it, then submit again"
                        )))
                    }
                    Err(SignInError::InvalidCode) => {
                        store_pending_login(
                            key,
                            PendingLogin {
                                bundle,
                                phase: PendingPhase::Code(token),
                                started,
                            },
                        );
                        Err(message_error(
                            "Telegram login code is invalid or expired; check it and submit again",
                        ))
                    }
                    Err(SignInError::InvalidPassword(password_token)) => {
                        store_pending_login(
                            key,
                            PendingLogin {
                                bundle,
                                phase: PendingPhase::Password(Box::new(password_token)),
                                started,
                            },
                        );
                        Err(message_error(
                            "Telegram two-factor password is invalid; check it and submit again",
                        ))
                    }
                    Err(SignInError::SignUpRequired) => Err(message_error(
                        "Telegram account does not exist; create it with an official Telegram client first",
                    )),
                    Err(SignInError::Other(error)) => Err(message_error(format!(
                        "Telegram sign-in failed: {error}. Submit again to restart login"
                    ))),
                }
            }
            PendingPhase::Password(password_token) => {
                let Some(password) = password else {
                    let hint = password_token
                        .hint()
                        .filter(|hint| !hint.is_empty())
                        .map(|hint| format!(" Hint: {hint}"))
                        .unwrap_or_default();
                    store_pending_login(
                        key,
                        PendingLogin {
                            bundle,
                            phase: PendingPhase::Password(password_token),
                            started,
                        },
                    );
                    return Err(message_error(format!(
                        "Telegram two-factor password required.{hint} Enter it, then submit again"
                    )));
                };

                let client = bundle.client.clone();
                let result =
                    telegram_task(
                        async move { client.check_password(*password_token, password).await },
                    )
                    .await?;
                match result {
                    Ok(_) => Self::finish_login(api_id, api_hash, session_path, bundle).await,
                    Err(SignInError::InvalidPassword(password_token)) => {
                        store_pending_login(
                            key,
                            PendingLogin {
                                bundle,
                                phase: PendingPhase::Password(Box::new(password_token)),
                                started,
                            },
                        );
                        Err(message_error(
                            "Telegram two-factor password is invalid; check it and submit again",
                        ))
                    }
                    Err(error) => Err(message_error(format!(
                        "Telegram two-factor sign-in failed: {error}. Submit again to restart login"
                    ))),
                }
            }
        }
    }

    pub(crate) fn room_target(&self, room_id: ID) -> Result<ChatTarget, BoxError> {
        self.state
            .lock()
            .unwrap()
            .room_targets
            .get(&room_id)
            .copied()
            .ok_or_else(|| message_error("No Telegram peer mapping for this room"))
    }

    pub(crate) fn room_ref(&self, target: ChatTarget) -> Result<PeerRef, BoxError> {
        self.state
            .lock()
            .unwrap()
            .peer_refs
            .get(&target)
            .copied()
            .ok_or_else(|| message_error("No Telegram peer reference for this room"))
    }
}

#[async_trait]
impl Messenger for TelegramMessenger {
    async fn create_messenger(cred: Credential) -> Result<Owner<dyn Messenger>, BoxError> {
        match TelegramAuth::try_from(cred)? {
            TelegramAuth::UserLogin {
                api_id,
                api_hash,
                phone,
                code,
                password,
            } => Self::user_login(api_id, api_hash, phone, code, password).await,
            TelegramAuth::Session {
                api_id,
                api_hash,
                session,
            } => Self::restore(api_id, api_hash, session).await,
        }
    }

    fn auth_methods() -> &'static [AuthMethod] {
        TelegramAuth::auth_methods()
    }

    fn name(&self) -> &'static str {
        INTERFACE_NAME
    }

    fn id(&self) -> String {
        format!(
            "{INTERFACE_NAME}{}",
            stable_id("account", &self.account_id.to_string())
        )
    }

    fn auth(&self) -> Credential {
        Credential(vec![
            CredentialField::new("api_id", self.api_id.to_string()),
            CredentialField::new("api_hash", self.api_hash.clone()),
            CredentialField::new("session", self.session_path.to_string_lossy().into_owned()),
        ])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ChatTarget(i64);

impl ChatTarget {
    pub(crate) fn from_peer_id(peer_id: PeerId) -> Option<Self> {
        peer_id.bot_api_dialog_id().map(Self)
    }

    pub(crate) fn from_channel_id(channel_id: i64) -> Option<Self> {
        PeerId::channel(channel_id).and_then(Self::from_peer_id)
    }

    pub(crate) fn room_id(self) -> ID {
        stable_id("room", &self.0.to_string())
    }

    pub(crate) fn message_id(self, message_id: i32) -> ID {
        stable_id("message", &format!("{}:{message_id}", self.0))
    }

    pub(crate) fn is_channel(self) -> bool {
        PeerId::from_bot_api_dialog_id(self.0)
            .is_some_and(|peer_id| peer_id.kind() == PeerKind::Channel)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TelegramUser {
    pub(crate) external_id: i64,
    pub(crate) name: String,
    pub(crate) username: Option<String>,
    pub(crate) phone: Option<String>,
    pub(crate) is_bot: bool,
}

impl TelegramUser {
    fn from_user(user: &GrammersUser) -> Self {
        let external_id = user.id().bare_id().unwrap_or_default();
        let username = user.username().map(str::to_owned);
        let name = {
            let full_name = user.full_name();
            if !full_name.trim().is_empty() {
                full_name
            } else {
                username.clone().unwrap_or_else(|| external_id.to_string())
            }
        };
        Self {
            external_id,
            name,
            username,
            phone: user.phone().map(str::to_owned),
            is_bot: user.is_bot(),
        }
    }

    pub(crate) fn from_peer(peer: &Peer) -> Self {
        match peer {
            Peer::User(user) => Self::from_user(user),
            Peer::Group(_) | Peer::Channel(_) => {
                let external_id = peer.id().bot_api_dialog_id().unwrap_or_default();
                Self {
                    external_id,
                    name: peer
                        .name()
                        .filter(|name| !name.trim().is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| external_id.to_string()),
                    username: peer.username().map(str::to_owned),
                    phone: None,
                    is_bot: false,
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TelegramPeer {
    pub(crate) target: ChatTarget,
    pub(crate) name: String,
    pub(crate) user: Option<TelegramUser>,
}

impl TelegramPeer {
    pub(crate) fn from_peer(peer: &Peer) -> Self {
        let target = ChatTarget::from_peer_id(peer.id())
            .expect("concrete Telegram peers always have an external ID");
        let (user, preferred_name) = match peer {
            Peer::User(user) => {
                let user = TelegramUser::from_user(user);
                (Some(user.clone()), user.name)
            }
            Peer::Group(_) | Peer::Channel(_) => (None, peer.name().unwrap_or_default().to_owned()),
        };
        let name = if preferred_name.trim().is_empty() {
            target.0.to_string()
        } else {
            preferred_name
        };
        Self { target, name, user }
    }

    pub(crate) fn unknown(peer_id: PeerId) -> Result<Self, BoxError> {
        let target = ChatTarget::from_peer_id(peer_id)
            .ok_or_else(|| message_error("Telegram update used unresolved self peer"))?;
        let is_user = peer_id.kind() == PeerKind::User;
        let name = target.0.to_string();
        let user = is_user.then(|| TelegramUser {
            external_id: peer_id.bare_id().unwrap_or_default(),
            name: name.clone(),
            username: None,
            phone: None,
            is_bot: false,
        });
        Ok(Self { target, name, user })
    }
}

#[derive(Clone)]
pub(crate) struct TelegramRoom {
    pub(crate) target: ChatTarget,
    pub(crate) peer: TelegramPeer,
    pub(crate) last_activity: i64,
}

#[derive(Clone)]
pub(crate) struct WireMessage {
    pub(crate) target: ChatTarget,
    pub(crate) peer_ref: PeerRef,
    pub(crate) peer: TelegramPeer,
    pub(crate) message_id: i32,
    pub(crate) author: Option<TelegramUser>,
    pub(crate) date: DateTime<Utc>,
    pub(crate) edit_date: Option<DateTime<Utc>>,
    pub(crate) text: String,
}

#[derive(Clone)]
pub(crate) struct CachedMessage {
    pub(crate) wire: WireMessage,
    pub(crate) history: Vec<Revision>,
}

#[derive(Default)]
pub(crate) struct TelegramState {
    pub(crate) dialogs_loaded: bool,
    pub(crate) rooms: HashMap<ChatTarget, TelegramRoom>,
    pub(crate) room_targets: HashMap<ID, ChatTarget>,
    pub(crate) peer_refs: HashMap<ChatTarget, PeerRef>,
    pub(crate) users: HashMap<i64, TelegramUser>,
    pub(crate) messages: HashMap<ChatTarget, BTreeMap<i32, CachedMessage>>,
    pub(crate) message_targets: HashMap<ID, (ChatTarget, i32)>,
    pub(crate) query_events: VecDeque<QueryEvent>,
    pub(crate) events: VecDeque<TextEvent>,
}

pub(crate) struct TelegramUpdates {
    pub(crate) raw: Option<UnboundedReceiver<UpdatesLike>>,
    pub(crate) stream: Option<UpdateStream>,
}

pub(crate) fn stable_id(kind: &str, external: &str) -> ID {
    let mut hash = Fnv1a64::default();
    hash.write(b"Telegram");
    hash.write(kind.as_bytes());
    hash.write(external.as_bytes());
    hash.finish()
}

struct Fnv1a64(u64);

impl Default for Fnv1a64 {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl std::hash::Hasher for Fnv1a64 {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn exposes_user_login_and_hides_session_restore() {
        let methods = Telegram::auth_methods();
        assert_eq!(methods.len(), 2);
        assert_eq!(methods[0].label, "User account");
        assert!(!methods[0].hidden);
        assert_eq!(
            methods[0]
                .fields
                .iter()
                .map(|field| (field.key, field.optional))
                .collect::<Vec<_>>(),
            vec![
                ("api_id", false),
                ("api_hash", false),
                ("phone", false),
                ("code", true),
                ("password", true),
            ]
        );
        assert!(methods[1].hidden);
    }

    #[test]
    fn rejects_session_paths_outside_private_directory() {
        assert!(valid_session_path(Path::new(
            "cache/Telegram/account.session"
        )));
        assert!(valid_session_path(Path::new(
            "./cache/Telegram/account.session"
        )));
        assert!(!valid_session_path(Path::new("../account.session")));
        assert!(!valid_session_path(Path::new(
            "cache/Telegram/nested/account.session"
        )));
    }
}
