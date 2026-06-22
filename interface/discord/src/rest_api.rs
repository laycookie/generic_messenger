//! Thin wrappers around Discord REST API endpoints.
//!
//! Each function here performs a single HTTP request and returns the decoded
//! raw `api_types::*` payload (or `()` for endpoints that have no response
//! body). Anything stateful — cache lookups, mapping inserts, conversion to
//! `messenger_interface` types — belongs in `query.rs`.

use std::error::Error;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use tracing::info;

use crate::{
    DISCORD_API, InnerDiscord, Owned,
    api_types::{self, SNOWFLAKE},
    downloaders::{Body as _, Fetch, Fresh},
};

/// `User-Agent` advertised on the login calls; must match `browser_user_agent`
/// inside the `X-Super-Properties` blob below so Discord sees a consistent
/// client.
const LOGIN_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0";

/// Headers sent on the unauthenticated `auth/*` calls.
///
/// The endpoint docs (<https://docs.discord.food/authentication>) do not list
/// any required headers, but Discord's web login in practice expects a
/// browser-like client: a real `User-Agent` and an `X-Super-Properties` blob
/// (base64-encoded client metadata). Without them logins are more likely to be
/// rejected or forced through a CAPTCHA, surfacing as a spurious
/// `INVALID_LOGIN`.
fn login_headers() -> Vec<(&'static str, String)> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use surf::http::convert::json;

    let super_properties = json!({
        "os": "Linux",
        "browser": "Firefox",
        "device": "",
        "system_locale": "en-US",
        "browser_user_agent": LOGIN_USER_AGENT,
        "browser_version": "124.0",
        "os_version": "",
        "referrer": "",
        "referring_domain": "",
        "referrer_current": "",
        "referring_domain_current": "",
        "release_channel": "stable",
        "client_build_number": 9999,
        "client_event_source": null,
    })
    .to_string();

    vec![
        ("User-Agent", LOGIN_USER_AGENT.to_string()),
        (
            "X-Super-Properties",
            STANDARD.encode(super_properties.as_bytes()),
        ),
    ]
}

/// Exchange account credentials (email/phone + password) for a user token.
///
/// Discord's `POST /auth/login` either returns a token directly or, when
/// two-factor is enabled, an MFA `ticket` that must be redeemed with a TOTP
/// code via `POST /auth/mfa/totp`. CAPTCHA-gated logins cannot be solved here
/// and surface as an error. No token is required for these endpoints.
///
/// Flow and field shapes per <https://docs.discord.food/authentication>.
pub(crate) async fn login_with_credentials(
    login: &str,
    password: &str,
    mfa_code: Option<&str>,
) -> Result<String, Box<dyn Error + Sync + Send>> {
    // Trim only the identifier: stray whitespace from copy/paste is a common
    // cause of a spurious rejection, and Discord trims emails anyway. The
    // password is sent verbatim — leading/trailing spaces can be significant.
    let login = login.trim();
    // https://docs.discord.food/authentication#login-account
    let body = facet_json::to_vec(&api_types::LoginRequest {
        login: login.to_owned(),
        password: password.to_owned(),
        undelete: false,
    })?;
    let response = async {
        Fetch::<Fresh>::fetch(
            || {
                surf::post(format!("{DISCORD_API}/auth/login"))
                    .body(body.clone())
                    .content_type("application/json")
            },
            login_headers(),
        )
        .await?
        .json::<api_types::LoginResponse>()
        .await
    }
    .await
    .map_err(|err| explain_login_error(err, login))?;

    if let Some(token) = response.token {
        info!("Discord: credential login succeeded");
        return Ok(token);
    }

    // No token means either an MFA challenge (a `ticket` to redeem) or a
    // CAPTCHA / invalid-credentials response (neither token nor ticket).
    let ticket = response.ticket.ok_or(
        "Discord login failed: no token returned (invalid credentials, or a CAPTCHA was required)",
    )?;
    let code = mfa_code
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .ok_or(
            "Discord login requires a two-factor (TOTP) authenticator code, but none was provided",
        )?;

    // https://docs.discord.food/authentication#verify-mfa
    info!("Discord: submitting two-factor (TOTP) code");
    let body = facet_json::to_vec(&api_types::MfaTotpRequest {
        ticket,
        code: code.to_owned(),
        login_instance_id: response.login_instance_id,
    })?;
    let response = Fetch::<Fresh>::fetch(
        || {
            surf::post(format!("{DISCORD_API}/auth/mfa/totp"))
                .body(body.clone())
                .content_type("application/json")
        },
        login_headers(),
    )
    .await?
    .json::<api_types::MfaResponse>()
    .await?;
    match response.token {
        Some(token) => Ok(token),
        None => Err("Discord two-factor verification returned no token".into()),
    }
}

/// Turn a raw `auth/login` HTTP failure into an actionable message. Discord
/// reports a bad email/password as `INVALID_LOGIN` on *both* fields (it won't
/// say which is wrong), and gates suspicious logins behind a CAPTCHA we cannot
/// solve. The serialized request body is known-correct, so these are about the
/// credential values or Discord's anti-automation, not our encoding.
fn explain_login_error(
    err: Box<dyn Error + Sync + Send>,
    login: &str,
) -> Box<dyn Error + Sync + Send> {
    let body = err.to_string();
    if body.contains("captcha") {
        return "Discord requires a CAPTCHA for this login, which this app cannot solve. \
             Log in once in the official Discord client or browser (same network if possible) \
             to clear the challenge, then try again."
            .into();
    }
    if body.contains("INVALID_LOGIN") {
        // A username (the @handle) is the single most common wrong value here:
        // the endpoint wants the account email or phone number.
        let hint = if login.contains('@') {
            "Discord rejected the email/password. Double-check the password, and note that \
             accounts using Google/Apple/passkey sign-in have no password to use here."
        } else {
            "Discord rejected the login. The username field must be your account's \
             email or phone number — not your Discord username/handle."
        };
        return hint.into();
    }
    err
}

impl InnerDiscord<Owned> {
    pub(crate) async fn get_auth_header(
        &self,
    ) -> Result<Vec<(&'static str, String)>, Box<dyn Error + Sync + Send>> {
        let token = self.ensure_token().await?;
        Ok(vec![("Authorization", token.unsecure().trim().to_string())])
    }

    pub(crate) async fn rest_get_profile(
        &self,
    ) -> Result<api_types::Profile, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/users/@me")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<api_types::Profile>()
        .await
    }

    pub(crate) async fn rest_get_contacts(
        &self,
    ) -> Result<Vec<api_types::Friend>, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/users/@me/relationships")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<Vec<api_types::Friend>>()
        .await
    }

    pub(crate) async fn rest_get_dms(
        &self,
    ) -> Result<Vec<api_types::Channel>, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/users/@me/channels")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<Vec<api_types::Channel>>()
        .await
    }

    pub(crate) async fn rest_get_guilds(
        &self,
    ) -> Result<Vec<api_types::Guild>, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/users/@me/guilds")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<Vec<api_types::Guild>>()
        .await
    }

    pub(crate) async fn rest_get_guild_channels(
        &self,
        guild_id: SNOWFLAKE,
    ) -> Result<Vec<api_types::Channel>, Box<dyn Error + Sync + Send>> {
        Fetch::<Fresh>::fetch(
            || surf::get(format!("{DISCORD_API}/guilds/{guild_id}/channels")),
            self.get_auth_header().await?,
        )
        .await?
        .json::<Vec<api_types::Channel>>()
        .await
    }

    pub(crate) async fn rest_get_messages(
        &self,
        channel_id: SNOWFLAKE,
        before: Option<SNOWFLAKE>,
    ) -> Result<Vec<api_types::Message>, Box<dyn Error + Sync + Send>> {
        let before_param = match before {
            Some(msg_id) => format!("?before={msg_id}"),
            None => String::new(),
        };
        let url = format!("{DISCORD_API}/channels/{channel_id}/messages{before_param}");
        Fetch::<Fresh>::fetch(|| surf::get(&url), self.get_auth_header().await?)
            .await?
            .json::<Vec<api_types::Message>>()
            .await
    }

    // Docs: https://discord.com/developers/docs/resources/message#create-reaction
    pub(crate) async fn rest_add_reaction(
        &self,
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let encoded_emoji = utf8_percent_encode(emoji, NON_ALPHANUMERIC);
        let url = format!(
            "{DISCORD_API}/channels/{channel_id}/messages/{message_id}/reactions/{encoded_emoji}/@me"
        );
        Fetch::<Fresh>::fetch(|| surf::put(&url), self.get_auth_header().await?).await?;
        Ok(())
    }

    // Docs: https://discord.com/developers/docs/resources/message#delete-own-reaction
    pub(crate) async fn rest_remove_reaction(
        &self,
        channel_id: SNOWFLAKE,
        message_id: SNOWFLAKE,
        emoji: &str,
    ) -> Result<(), Box<dyn Error + Sync + Send>> {
        let encoded_emoji = utf8_percent_encode(emoji, NON_ALPHANUMERIC);
        let url = format!(
            "{DISCORD_API}/channels/{channel_id}/messages/{message_id}/reactions/{encoded_emoji}/@me"
        );
        Fetch::<Fresh>::fetch(|| surf::delete(&url), self.get_auth_header().await?).await?;
        Ok(())
    }

    // Docs: https://discord.com/developers/docs/resources/message#create-message
    pub(crate) async fn rest_send_message(
        &self,
        channel_id: SNOWFLAKE,
        content: String,
    ) -> Result<api_types::Message, Box<dyn Error + Sync + Send>> {
        let message = api_types::CreateMessage {
            content: Some(content),
            nonce: None,
            enforce_nonce: None,
            tts: Some(false),
            flags: Some(0),
        };
        let msg_string = facet_json::to_vec(&message)?;
        Fetch::<Fresh>::fetch(
            || {
                surf::post(format!("{DISCORD_API}/channels/{channel_id}/messages"))
                    .body(msg_string.clone())
                    .content_type("application/json")
            },
            self.get_auth_header().await?,
        )
        .await?
        .json::<api_types::Message>()
        .await
    }
}
