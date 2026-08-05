//! Discord credential parsing and login exchange.

use std::error::Error;

use credential_macro::base64_literal;
use messenger_interface::auth::{
    AuthFuture, Credential, CredentialField, CredentialSchema, InteractiveAuthMethod,
    InteractiveAuthPrompt, InteractiveAuthSession, QrPayload,
};
use secure_string::SecureString;
use tracing::info;

use crate::{
    DISCORD_API, api,
    downloaders::{Body as _, Fetch, Fresh},
    remote_auth::QrLoginSession,
};

const LOGIN_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0";

const LOGIN_SUPER_PROPERTIES: &str = base64_literal!(
    r#"{
    "os": "Linux",
    "browser": "Firefox",
    "device": "",
    "system_locale": "en-US",
    "browser_user_agent": "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0",
    "browser_version": "124.0",
    "os_version": "",
    "referrer": "",
    "referring_domain": "",
    "referrer_current": "",
    "referring_domain_current": "",
    "release_channel": "stable",
    "client_build_number": 9999,
    "client_event_source": null
}"#
);

pub(crate) const fn login_headers() -> [(&'static str, &'static str); 2] {
    [
        ("User-Agent", LOGIN_USER_AGENT),
        ("X-Super-Properties", LOGIN_SUPER_PROPERTIES),
    ]
}

/// Discord credential shapes. Password credentials resolve into a token before
/// messenger construction; only token credentials are persisted afterward.
#[derive(CredentialSchema)]
pub(crate) enum DiscordAuth {
    #[credential(method = "Token")]
    Token {
        #[credential(label = "Token")]
        token: SecureString,
    },
    #[credential(method = "Email & password")]
    Password {
        #[credential(label = "Email or phone")]
        username: SecureString,
        #[credential(label = "Password", secret)]
        password: SecureString,
        #[credential(label = "Two-factor code (if enabled)")]
        otp: Option<SecureString>,
    },
}

fn start_qr_login() -> AuthFuture<InteractiveAuthSession> {
    Box::pin(async {
        let session = QrLoginSession::start().await?;
        let payload = QrPayload::new(session.qr_url().to_owned());
        Ok(InteractiveAuthSession {
            prompt: InteractiveAuthPrompt::QrCode { payload },
            finish: Box::pin(session.finish()),
        })
    })
}

pub(crate) const INTERACTIVE_AUTH_METHODS: &[InteractiveAuthMethod] = &[InteractiveAuthMethod {
    label: "QR code",
    start: start_qr_login,
}];

impl From<DiscordAuth> for Credential {
    fn from(auth: DiscordAuth) -> Self {
        match auth {
            DiscordAuth::Token { token } => Self(vec![CredentialField::new("token", token)]),
            DiscordAuth::Password {
                username,
                password,
                otp,
            } => {
                let mut fields = vec![
                    CredentialField::new("username", username),
                    CredentialField::new("password", password),
                ];
                if let Some(otp) = otp {
                    fields.push(CredentialField::new("otp", otp));
                }
                Self(fields)
            }
        }
    }
}

/// Resolve Discord [`Credential`] into persistable token shape.
pub(crate) async fn login_with_credentials(
    credential: Credential,
) -> Result<Credential, Box<dyn Error + Sync + Send>> {
    let (username, password, otp) = match DiscordAuth::try_from(credential)? {
        DiscordAuth::Token { token } => return Ok(DiscordAuth::Token { token }.into()),
        DiscordAuth::Password {
            username,
            password,
            otp,
        } => (username, password, otp),
    };

    let login = username.unsecure().trim();
    let body = facet_json::to_vec(&api::LoginRequest {
        login: login.to_owned(),
        password: password.unsecure().to_owned(),
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
        .json::<api::LoginResponse>()
        .await
    }
    .await
    .map_err(|err| explain_login_error(err, login))?;

    if let Some(token) = response.token {
        info!("Discord: credential login succeeded");
        return Ok(DiscordAuth::Token {
            token: SecureString::from(token),
        }
        .into());
    }

    let ticket = response.ticket.ok_or(
        "Discord login failed: no token returned (invalid credentials, or a CAPTCHA was required)",
    )?;
    let code = otp
        .as_ref()
        .map(SecureString::unsecure)
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .ok_or(
            "Discord login requires a two-factor (TOTP) authenticator code, but none was provided",
        )?;

    info!("Discord: submitting two-factor (TOTP) code");
    let body = facet_json::to_vec(&api::MfaTotpRequest {
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
    .json::<api::MfaResponse>()
    .await?;
    match response.token {
        Some(token) => Ok(DiscordAuth::Token {
            token: SecureString::from(token),
        }
        .into()),
        None => Err("Discord two-factor verification returned no token".into()),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_qr_interactive_auth() {
        let [method] = INTERACTIVE_AUTH_METHODS else {
            panic!("Discord should expose exactly one interactive auth method")
        };

        assert_eq!(method.label, "QR code");
    }

    #[test]
    fn credential_round_trip() {
        let credential: Credential = DiscordAuth::Password {
            username: SecureString::from("user@example.com"),
            password: SecureString::from("password"),
            otp: Some(SecureString::from("123456")),
        }
        .into();
        let DiscordAuth::Password {
            username,
            password,
            otp,
        } = DiscordAuth::try_from(credential).unwrap()
        else {
            panic!("password credential changed shape")
        };
        assert_eq!(username.unsecure(), "user@example.com");
        assert_eq!(password.unsecure(), "password");
        assert_eq!(otp.unwrap().unsecure(), "123456");
    }
}
