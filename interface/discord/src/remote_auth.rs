//! Discord desktop remote authentication (QR-code login).
//!
//! <https://docs.discord.food/remote-authentication/desktop>

use std::{
    error::Error,
    future::Future,
    io,
    time::{Duration, Instant},
};

use async_tungstenite::{
    WebSocketReceiver, WebSocketSender,
    smol::{ConnectStream, connect_async},
    tungstenite::{
        Message as WebsocketMessage,
        client::IntoClientRequest,
        http::{HeaderValue, header::ORIGIN},
    },
};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use facet::Facet;
use futures::{
    FutureExt, StreamExt,
    future::{Either, Fuse, select},
};
use futures_timer::Delay;
use messenger_interface::auth::Credential;
use rsa::{
    Oaep, RsaPrivateKey, RsaPublicKey,
    pkcs8::EncodePublicKey,
    sha2::{Digest, Sha256},
};
use secure_string::SecureString;
use tracing::info;

use crate::{
    DISCORD_API, api,
    auth::{DiscordAuth, login_headers},
    downloaders::{Body as _, Fetch, Fresh},
    random::SystemRandom,
};

type BoxError = Box<dyn Error + Send + Sync>;

const REMOTE_AUTH_GATEWAY: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
const DISCORD_ORIGIN: &str = "https://discord.com";
const QR_URL_PREFIX: &str = "https://discord.com/ra/";
const RSA_BITS: usize = 2048;
const REMOTE_AUTH_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REMOTE_AUTH_SESSION_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Facet)]
struct RemoteAuthEvent {
    op: String,
    timeout_ms: Option<u64>,
    heartbeat_interval: Option<u64>,
    encrypted_nonce: Option<String>,
    fingerprint: Option<String>,
    encrypted_user_payload: Option<String>,
    ticket: Option<String>,
}

#[derive(Facet)]
struct InitCommand {
    op: String,
    encoded_public_key: String,
}

#[derive(Facet)]
struct NonceProofCommand {
    op: String,
    nonce: String,
}

#[derive(Facet)]
struct HeartbeatCommand {
    op: String,
}

enum SocketWake {
    Message(Option<Result<WebsocketMessage, async_tungstenite::tungstenite::Error>>),
    Heartbeat,
    SessionExpired,
}

struct RemoteAuthSocket {
    sender: WebSocketSender<ConnectStream>,
    receiver: WebSocketReceiver<ConnectStream>,
    heartbeat_interval: Duration,
    heartbeat: Fuse<Delay>,
    session_timeout: Fuse<Delay>,
    heartbeat_acked: bool,
}

impl RemoteAuthSocket {
    fn new(
        sender: WebSocketSender<ConnectStream>,
        receiver: WebSocketReceiver<ConnectStream>,
        heartbeat_interval: Duration,
        session_timeout: Duration,
    ) -> Self {
        Self {
            sender,
            receiver,
            heartbeat_interval,
            heartbeat: Delay::new(heartbeat_interval).fuse(),
            session_timeout: Delay::new(session_timeout).fuse(),
            heartbeat_acked: true,
        }
    }

    async fn send<T>(&mut self, payload: &T) -> Result<(), BoxError>
    where
        T: for<'facet> Facet<'facet>,
    {
        let json = String::from_utf8(facet_json::to_vec(payload)?)?;
        remote_auth_timeout(
            self.sender.send(WebsocketMessage::text(json)),
            REMOTE_AUTH_OPERATION_TIMEOUT,
            "Discord remote-auth gateway send",
        )
        .await
    }

    async fn next_event(&mut self) -> Result<RemoteAuthEvent, BoxError> {
        loop {
            let wake = {
                let incoming = self.receiver.next();
                futures::pin_mut!(incoming);
                let activity = select(incoming, &mut self.heartbeat);
                futures::pin_mut!(activity);
                match select(activity, &mut self.session_timeout).await {
                    Either::Left((Either::Left((message, _heartbeat)), _timeout)) => {
                        SocketWake::Message(message)
                    }
                    Either::Left((Either::Right(((), _incoming)), _timeout)) => {
                        SocketWake::Heartbeat
                    }
                    Either::Right(((), _activity)) => SocketWake::SessionExpired,
                }
            };

            match wake {
                SocketWake::Message(Some(Ok(
                    WebsocketMessage::Ping(_) | WebsocketMessage::Pong(_),
                ))) => continue,
                SocketWake::Message(message) => {
                    let event = decode_event(message)?;
                    if event.op == "heartbeat_ack" {
                        self.heartbeat_acked = true;
                        continue;
                    }
                    return Ok(event);
                }
                SocketWake::Heartbeat => {
                    if !self.heartbeat_acked {
                        return Err(invalid_data(
                            "Discord remote-auth gateway stopped acknowledging heartbeats",
                        ));
                    }
                    self.send(&HeartbeatCommand {
                        op: "heartbeat".to_owned(),
                    })
                    .await?;
                    self.heartbeat_acked = false;
                    self.heartbeat = Delay::new(self.heartbeat_interval).fuse();
                }
                SocketWake::SessionExpired => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Discord QR login session expired",
                    )
                    .into());
                }
            }
        }
    }
}

/// Live Discord QR-login session. QR payload is presented through shared auth;
/// RSA private key and token remain backend-owned and are never formatted or
/// logged.
pub(crate) struct QrLoginSession {
    qr_url: String,
    private_key: RsaPrivateKey,
    socket: RemoteAuthSocket,
}

impl QrLoginSession {
    pub(crate) async fn start() -> Result<Self, BoxError> {
        let mut request = REMOTE_AUTH_GATEWAY.into_client_request()?;
        request
            .headers_mut()
            .insert(ORIGIN, HeaderValue::from_static(DISCORD_ORIGIN));
        let (websocket, _) = remote_auth_timeout(
            connect_async(request),
            REMOTE_AUTH_OPERATION_TIMEOUT,
            "Discord remote-auth gateway connect",
        )
        .await?;
        let (mut sender, mut receiver) = websocket.split();

        let hello = remote_auth_timeout(
            next_event_without_heartbeat(&mut receiver),
            REMOTE_AUTH_OPERATION_TIMEOUT,
            "Discord remote-auth gateway Hello",
        )
        .await?;
        if hello.op != "hello" {
            return Err(invalid_data(format!(
                "Discord remote-auth gateway sent '{}' before hello",
                hello.op
            )));
        }
        let heartbeat_interval = hello
            .heartbeat_interval
            .filter(|interval| *interval > 0)
            .map(Duration::from_millis)
            .ok_or_else(|| invalid_data("Discord remote-auth hello omitted heartbeat interval"))?;
        let session_timeout = hello
            .timeout_ms
            .filter(|timeout| *timeout > 0)
            .map(Duration::from_millis)
            .map(|timeout| timeout.min(MAX_REMOTE_AUTH_SESSION_TIMEOUT))
            .ok_or_else(|| invalid_data("Discord remote-auth hello omitted session timeout"))?;
        let session_deadline = Instant::now()
            .checked_add(session_timeout)
            .ok_or_else(|| invalid_data("Discord remote-auth session timeout is invalid"))?;

        let private_key = smol::unblock(|| {
            let mut random = SystemRandom::default();
            RsaPrivateKey::new(&mut random, RSA_BITS)
        })
        .await?;
        let public_key = RsaPublicKey::from(&private_key);
        let public_key_der = public_key.to_public_key_der()?;
        let encoded_public_key = STANDARD.encode(public_key_der.as_ref());
        let expected_fingerprint = fingerprint(public_key_der.as_ref());

        send_payload(
            &mut sender,
            &InitCommand {
                op: "init".to_owned(),
                encoded_public_key,
            },
        )
        .await?;

        let remaining = session_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Discord QR login session expired during setup",
            )
            .into());
        }
        let mut socket = RemoteAuthSocket::new(sender, receiver, heartbeat_interval, remaining);
        loop {
            let event = socket.next_event().await?;
            match event.op.as_str() {
                "nonce_proof" => {
                    let encrypted_nonce = event.encrypted_nonce.ok_or_else(|| {
                        invalid_data("Discord nonce-proof event omitted encrypted nonce")
                    })?;
                    let encrypted_nonce = STANDARD.decode(encrypted_nonce)?;
                    let nonce = private_key.decrypt(Oaep::new::<Sha256>(), &encrypted_nonce)?;
                    socket
                        .send(&NonceProofCommand {
                            op: "nonce_proof".to_owned(),
                            nonce: URL_SAFE_NO_PAD.encode(nonce),
                        })
                        .await?;
                }
                "pending_remote_init" => {
                    let received_fingerprint = event.fingerprint.ok_or_else(|| {
                        invalid_data("Discord remote-auth init omitted fingerprint")
                    })?;
                    if received_fingerprint != expected_fingerprint {
                        return Err(invalid_data(
                            "Discord remote-auth fingerprint did not match generated public key",
                        ));
                    }
                    info!("Discord: QR login session ready");
                    return Ok(Self {
                        qr_url: format!("{QR_URL_PREFIX}{received_fingerprint}"),
                        private_key,
                        socket,
                    });
                }
                op => {
                    return Err(invalid_data(format!(
                        "Discord remote-auth gateway sent unexpected '{op}' during handshake"
                    )));
                }
            }
        }
    }

    /// URL encoded by QR shown to user.
    #[must_use]
    pub(crate) fn qr_url(&self) -> &str {
        &self.qr_url
    }

    /// Wait for mobile approval, exchange ticket, decrypt token, and return
    /// standard persistable Discord token credential.
    ///
    /// # Errors
    ///
    /// Returns an error when gateway closes, protocol validation fails, mobile
    /// approval is canceled, or ticket exchange/decryption fails.
    pub(crate) async fn finish(mut self) -> Result<Credential, BoxError> {
        loop {
            let event = self.socket.next_event().await?;
            match event.op.as_str() {
                "pending_ticket" => {
                    let encrypted_payload = event.encrypted_user_payload.ok_or_else(|| {
                        invalid_data("Discord pending-ticket event omitted encrypted user payload")
                    })?;
                    let encrypted_payload = STANDARD.decode(encrypted_payload)?;
                    let payload = self
                        .private_key
                        .decrypt(Oaep::new::<Sha256>(), &encrypted_payload)?;
                    let payload = std::str::from_utf8(&payload)?;
                    if payload.split(':').count() != 4 {
                        return Err(invalid_data(
                            "Discord pending-ticket event contained malformed user payload",
                        ));
                    }
                }
                "pending_login" => {
                    let ticket = event.ticket.ok_or_else(|| {
                        invalid_data("Discord pending-login event omitted ticket")
                    })?;
                    return self.exchange_ticket(ticket).await;
                }
                "cancel" => {
                    return Err(invalid_data("Discord QR login was canceled on mobile"));
                }
                op => {
                    return Err(invalid_data(format!(
                        "Discord remote-auth gateway sent unexpected '{op}' while awaiting approval"
                    )));
                }
            }
        }
    }

    async fn exchange_ticket(self, ticket: String) -> Result<Credential, BoxError> {
        let body = facet_json::to_vec(&api::RemoteAuthLoginRequest { ticket })?;
        let response = Fetch::<Fresh>::fetch(
            || {
                surf::post(format!("{DISCORD_API}/users/@me/remote-auth/login"))
                    .body(body.clone())
                    .content_type("application/json")
            },
            login_headers(),
        )
        .await?
        .json::<api::RemoteAuthLoginResponse>()
        .await?;

        let encrypted_token = STANDARD.decode(response.encrypted_token)?;
        let token = self
            .private_key
            .decrypt(Oaep::new::<Sha256>(), &encrypted_token)?;
        let token = SecureString::from(String::from_utf8(token)?);
        info!("Discord: QR login succeeded");
        Ok(DiscordAuth::Token { token }.into())
    }
}

async fn remote_auth_timeout<F, T, E>(
    future: F,
    timeout: Duration,
    operation: &str,
) -> Result<T, BoxError>
where
    F: Future<Output = Result<T, E>>,
    E: Into<BoxError>,
{
    match select(Box::pin(future), Box::pin(Delay::new(timeout))).await {
        Either::Left((result, _timeout)) => result.map_err(Into::into),
        Either::Right(((), _operation)) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{operation} exceeded {} second deadline", timeout.as_secs()),
        )
        .into()),
    }
}

async fn send_payload<T>(
    sender: &mut WebSocketSender<ConnectStream>,
    payload: &T,
) -> Result<(), BoxError>
where
    T: for<'facet> Facet<'facet>,
{
    let json = String::from_utf8(facet_json::to_vec(payload)?)?;
    remote_auth_timeout(
        sender.send(WebsocketMessage::text(json)),
        REMOTE_AUTH_OPERATION_TIMEOUT,
        "Discord remote-auth gateway initialization send",
    )
    .await
}

async fn next_event_without_heartbeat(
    receiver: &mut WebSocketReceiver<ConnectStream>,
) -> Result<RemoteAuthEvent, BoxError> {
    loop {
        let message = receiver.next().await;
        match message {
            Some(Ok(WebsocketMessage::Ping(_) | WebsocketMessage::Pong(_))) => continue,
            message => return decode_event(message),
        }
    }
}

fn decode_event(
    message: Option<Result<WebsocketMessage, async_tungstenite::tungstenite::Error>>,
) -> Result<RemoteAuthEvent, BoxError> {
    match message {
        Some(Ok(WebsocketMessage::Text(text))) => {
            facet_json::from_str(text.as_str()).map_err(Into::into)
        }
        Some(Ok(WebsocketMessage::Close(frame))) => {
            let detail = frame.map_or_else(
                || "without a close frame".to_owned(),
                |frame| format!("with code {}: {}", frame.code, frame.reason),
            );
            Err(invalid_data(format!(
                "Discord remote-auth gateway closed {detail}"
            )))
        }
        Some(Ok(WebsocketMessage::Ping(_) | WebsocketMessage::Pong(_))) => Err(invalid_data(
            "Discord remote-auth control frame interrupted event wait",
        )),
        Some(Ok(_)) => Err(invalid_data(
            "Discord remote-auth gateway sent a non-text payload",
        )),
        Some(Err(error)) => Err(error.into()),
        None => Err(invalid_data(
            "Discord remote-auth gateway closed before login completed",
        )),
    }
}

fn fingerprint(public_key_der: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(public_key_der))
}

fn invalid_data(message: impl Into<String>) -> BoxError {
    io::Error::new(io::ErrorKind::InvalidData, message.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_auth_hello() {
        let event = facet_json::from_str::<RemoteAuthEvent>(
            r#"{"op":"hello","timeout_ms":142637,"heartbeat_interval":41250}"#,
        )
        .unwrap();

        assert_eq!(event.op, "hello");
        assert_eq!(event.timeout_ms, Some(142_637));
        assert_eq!(event.heartbeat_interval, Some(41_250));
    }

    #[test]
    fn remote_auth_operation_timeout_is_bounded() {
        let error = futures::executor::block_on(remote_auth_timeout(
            futures::future::pending::<Result<(), io::Error>>(),
            Duration::from_millis(1),
            "test remote auth operation",
        ))
        .unwrap_err();

        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::TimedOut)
        );
        assert!(error.to_string().contains("test remote auth operation"));
    }

    #[test]
    fn fingerprint_is_sha256_base64url_without_padding() {
        assert_eq!(
            fingerprint(b"discord remote auth"),
            "FrmXL_BYyHeD0YLVTAnUcC2FXNgyS1Qp5t9LaBruzaU"
        );
    }
}
