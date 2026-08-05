//! Discord user-authentication request and response shapes.
//!
//! <https://docs.discord.food/authentication>

use facet::Facet;

/// Body for `POST /auth/login`.
/// <https://docs.discord.food/authentication#login-account>
#[derive(Facet)]
pub(crate) struct LoginRequest {
    pub login: String,
    pub password: String,
    pub undelete: bool,
}

/// Response to `POST /auth/login`.
/// <https://docs.discord.food/authentication#login-account>
#[derive(Facet)]
pub(crate) struct LoginResponse {
    pub token: Option<String>,
    pub mfa: Option<bool>,
    pub ticket: Option<String>,
    pub totp: Option<bool>,
    pub login_instance_id: Option<String>,
}

/// Body for `POST /auth/mfa/totp`.
/// <https://docs.discord.food/authentication#verify-mfa>
#[derive(Facet)]
pub(crate) struct MfaTotpRequest {
    pub ticket: String,
    pub code: String,
    pub login_instance_id: Option<String>,
}

/// Response to `POST /auth/mfa/totp`.
/// <https://docs.discord.food/authentication#verify-mfa>
#[derive(Facet)]
pub(crate) struct MfaResponse {
    pub token: Option<String>,
}

/// Body for `POST /users/@me/remote-auth/login`.
/// <https://docs.discord.food/remote-authentication/desktop#exchange-remote-auth-ticket>
#[derive(Facet)]
pub(crate) struct RemoteAuthLoginRequest {
    pub ticket: String,
}

/// Response from `POST /users/@me/remote-auth/login`.
/// <https://docs.discord.food/remote-authentication/desktop#exchange-remote-auth-ticket>
#[derive(Facet)]
pub(crate) struct RemoteAuthLoginResponse {
    pub encrypted_token: String,
}
