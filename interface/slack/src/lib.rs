//! Minimal Slack messenger adapter.
//!
//! Covers token auth, workspace/channel/user queries, recent text history,
//! and sending plain text via Slack Web API. Realtime events, edit/delete,
//! reactions, files, blocks, and calls are intentionally left out for now.

use std::{
    collections::HashMap,
    error::Error,
    hash::Hasher as _,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use messenger_interface::{
    auth::{AuthMethod, Credential, CredentialField, CredentialSchema},
    interface::Messenger,
    types::ID,
};
use secure_string::SecureString;

mod api_types;
mod query;
mod rest_api;
mod text;

pub(crate) const INTERFACE_NAME: &str = "Slack";
pub(crate) const SLACK_API: &str = "https://slack.com/api";

/// Public entry point, mirroring `discord::Discord` and `steam::Steam`.
pub struct Slack;

impl Slack {
    pub async fn create_messenger(
        cred: Credential,
    ) -> Result<Arc<dyn Messenger>, Box<dyn Error + Send + Sync>> {
        <SlackMessenger as Messenger>::create_messenger(cred).await
    }

    pub fn auth_methods() -> &'static [AuthMethod] {
        <SlackMessenger as Messenger>::auth_methods()
    }
}

pub(crate) struct SlackMessenger {
    token: SecureString,
    auth_cache: Mutex<Option<api_types::AuthTestResponse>>,
    users: Mutex<HashMap<String, api_types::SlackUser>>,
    channel_ids: Mutex<HashMap<ID, String>>,
    message_ts: Mutex<HashMap<ID, String>>,
}

impl SlackMessenger {
    fn build(token: SecureString) -> Arc<dyn Messenger> {
        Arc::new(Self {
            token,
            auth_cache: Mutex::new(None),
            users: Mutex::new(HashMap::new()),
            channel_ids: Mutex::new(HashMap::new()),
            message_ts: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn auth_header(&self) -> Vec<(&'static str, String)> {
        vec![(
            "Authorization",
            format!("Bearer {}", self.token.unsecure().trim()),
        )]
    }

    pub(crate) fn remember_channel(&self, slack_id: &str) -> ID {
        let id = stable_id("channel", slack_id);
        self.channel_ids
            .lock()
            .unwrap()
            .insert(id, slack_id.to_owned());
        id
    }

    pub(crate) fn channel_id(&self, id: ID) -> Result<String, Box<dyn Error + Send + Sync>> {
        self.channel_ids
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| "No Slack channel id mapping for this room".into())
    }

    pub(crate) fn remember_message(&self, channel_id: &str, ts: &str) -> ID {
        let id = stable_id("message", &format!("{channel_id}:{ts}"));
        self.message_ts.lock().unwrap().insert(id, ts.to_owned());
        id
    }

    pub(crate) fn message_ts(&self, id: ID) -> Result<String, Box<dyn Error + Send + Sync>> {
        self.message_ts
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| "No Slack message timestamp mapping".into())
    }

    pub(crate) async fn auth_test(
        &self,
    ) -> Result<api_types::AuthTestResponse, Box<dyn Error + Send + Sync>> {
        if let Some(cached) = self.auth_cache.lock().unwrap().clone() {
            return Ok(cached);
        }

        let auth = self.rest_auth_test().await?;
        *self.auth_cache.lock().unwrap() = Some(auth.clone());
        Ok(auth)
    }

    pub(crate) async fn user_info_cached(
        &self,
        user_id: &str,
    ) -> Result<api_types::SlackUser, Box<dyn Error + Send + Sync>> {
        if let Some(user) = self.users.lock().unwrap().get(user_id).cloned() {
            return Ok(user);
        }

        let user = self.rest_user_info(user_id).await?;
        self.users
            .lock()
            .unwrap()
            .insert(user.id.clone(), user.clone());
        Ok(user)
    }

    pub(crate) fn cache_users(&self, users: &[api_types::SlackUser]) {
        let mut cache = self.users.lock().unwrap();
        for user in users {
            cache.insert(user.id.clone(), user.clone());
        }
    }
}

#[derive(CredentialSchema)]
enum SlackAuth {
    #[credential(method = "Token")]
    Token {
        #[credential(label = "Bot or user token", secret)]
        token: SecureString,
    },
}

#[async_trait]
impl Messenger for SlackMessenger {
    async fn create_messenger(
        cred: Credential,
    ) -> Result<Arc<dyn Messenger>, Box<dyn Error + Send + Sync>> {
        match SlackAuth::try_from(cred)? {
            SlackAuth::Token { token } => Ok(Self::build(token)),
        }
    }

    fn auth_methods() -> &'static [AuthMethod] {
        SlackAuth::auth_methods()
    }

    fn name(&self) -> &'static str {
        INTERFACE_NAME
    }

    fn id(&self) -> String {
        format!(
            "{INTERFACE_NAME}{}",
            stable_id("token", self.token.unsecure())
        )
    }

    fn auth(&self) -> Credential {
        Credential(vec![CredentialField::new("token", self.token.clone())])
    }
}

pub(crate) fn stable_id(kind: &str, external: &str) -> ID {
    // FNV-1a: deterministic across runs, unlike std's randomized maps.
    let mut hash = Fnv1a64::default();
    hash.write(b"Slack");
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
