//! OAuth 2.0 for a desktop app: Google Drive, OneDrive and Dropbox all sign
//! a user in the same way.
//!
//! The app is a *public client* - it runs on the user's machine and cannot
//! keep a secret - so the flow is the one RFC 8252 prescribes for native
//! apps: PKCE, and a redirect to a loopback address this process listens on
//! for the moment the browser comes back. No secret is required by any of
//! the three services for this kind of client (Google calls it a "Desktop
//! app", Microsoft a "Mobile and desktop application", Dropbox has PKCE on
//! every app), though Google hands one out and accepts it when sent.
//!
//! What comes out is a [`TokenSet`]: an access token that lasts an hour and
//! a refresh token that lasts until the user revokes it. The set is stored
//! as the drive's secret, where a password would be, and a [`Session`]
//! refreshes the access token when it is about to expire and hands the new
//! set back to be stored again.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{Error, Result};

/// Where a service's sign-in and token endpoints are, and who is asking.
#[derive(Clone, Debug)]
pub struct Provider {
    /// Shown in messages: "Google", "Microsoft", "Dropbox".
    pub name: &'static str,
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    /// Service-specific query parameters for the sign-in page, such as
    /// Google's `access_type=offline`.
    pub extra_auth_params: Vec<(String, String)>,
    /// The host name in the redirect URI. `127.0.0.1` is what RFC 8252
    /// recommends and Google accepts; Microsoft only matches `localhost`
    /// (and ignores the port for it); Dropbox wants the registered form.
    pub loopback_host: &'static str,
    /// A port to try first, for a service that matches the registered
    /// redirect URI port for port (Dropbox). Any free port otherwise.
    pub preferred_port: Option<u16>,
}

/// The OAuth client MyDrives itself is registered as with each service.
///
/// A sign-in has to happen under *some* registered application, and sending
/// every user off to make one in three developer consoles is not a sign-in,
/// it is homework. So the project registers one application per service and
/// the build compiles the identifiers in, the way every desktop client that
/// talks to these services does: RFC 8252 says a native app cannot keep a
/// secret, so these identifiers are not credentials, and PKCE is what
/// actually protects the exchange.
///
/// They are not written in the source, because this repository is public
/// and Google rotates a client secret it finds in one. The release build
/// passes them in as environment variables instead; a build without them
/// leaves the fields empty, and then the window asks the user for a client
/// of their own exactly as before.
pub mod builtin {
    /// What a service needs in order to recognise the app that is asking.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Client {
        pub client_id: &'static str,
        pub client_secret: Option<&'static str>,
    }

    /// A compiled-in value counts only if the build actually supplied one;
    /// an empty string is what an unset secret expands to in CI.
    fn set(value: Option<&'static str>) -> Option<&'static str> {
        match value {
            Some(v) if !v.trim().is_empty() => Some(v.trim()),
            _ => None,
        }
    }

    pub fn google() -> Option<Client> {
        set(option_env!("MYDRIVES_GOOGLE_CLIENT_ID")).map(|client_id| Client {
            client_id,
            client_secret: set(option_env!("MYDRIVES_GOOGLE_CLIENT_SECRET")),
        })
    }

    pub fn microsoft() -> Option<Client> {
        set(option_env!("MYDRIVES_MICROSOFT_CLIENT_ID")).map(|client_id| Client {
            client_id,
            client_secret: None,
        })
    }

    pub fn dropbox() -> Option<Client> {
        set(option_env!("MYDRIVES_DROPBOX_CLIENT_ID")).map(|client_id| Client {
            client_id,
            client_secret: None,
        })
    }

    /// By the backend kind as it is written in the configuration file.
    pub fn for_kind(kind: &str) -> Option<Client> {
        match kind {
            "gdrive" => google(),
            "onedrive" => microsoft(),
            "dropbox" => dropbox(),
            _ => None,
        }
    }
}

/// Whether signing in to this kind of drive can go ahead with nothing but a
/// click, because the build knows which application to sign in as.
pub fn has_builtin_client(kind: &str) -> bool {
    builtin::for_kind(kind).is_some()
}

/// Which application to sign in as: the one this drive carries, if it was
/// given one, and otherwise the one compiled in. A drive that names its own
/// client keeps using it, so an existing drive does not change hands.
fn resolve_client(
    client_id: &str,
    client_secret: Option<&str>,
    built: Option<builtin::Client>,
) -> (String, Option<String>) {
    let given_secret = client_secret
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let given_id = client_id.trim();
    if !given_id.is_empty() {
        return (given_id.to_string(), given_secret);
    }
    match built {
        Some(c) => (
            c.client_id.to_string(),
            given_secret.or_else(|| c.client_secret.map(str::to_string)),
        ),
        None => (String::new(), given_secret),
    }
}

/// Google Drive, as the "Desktop app" OAuth client the build was given,
/// or as one the user registered themselves and typed into the drive.
pub fn google_provider(client_id: &str, client_secret: Option<&str>) -> Provider {
    let (client_id, client_secret) = resolve_client(client_id, client_secret, builtin::google());
    Provider {
        name: "Google",
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
        token_url: "https://oauth2.googleapis.com/token".into(),
        client_id,
        client_secret,
        // Everything in the drive, not only files this app made.
        scopes: vec!["https://www.googleapis.com/auth/drive".into()],
        // `offline` is what makes Google hand out a refresh token; `consent`
        // makes it do so again on a second sign-in instead of only the first.
        extra_auth_params: vec![
            ("access_type".into(), "offline".into()),
            ("prompt".into(), "consent".into()),
        ],
        loopback_host: "127.0.0.1",
        preferred_port: None,
    }
}

/// OneDrive (personal, and work or school) through Microsoft Graph, for an
/// app registered in the user's Entra tenant as "Mobile and desktop
/// applications" with the redirect URI `http://localhost`. Microsoft ignores
/// the port for localhost and does not support `[::1]`, so the redirect
/// names `localhost` and the listener takes care to be reachable there.
pub fn microsoft_provider(client_id: &str) -> Provider {
    let (client_id, _) = resolve_client(client_id, None, builtin::microsoft());
    Provider {
        name: "Microsoft",
        auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
        token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
        client_id,
        client_secret: None,
        // `offline_access` is what makes Microsoft hand out a refresh token.
        scopes: vec!["Files.ReadWrite.All".into(), "offline_access".into()],
        extra_auth_params: vec![("response_mode".into(), "query".into())],
        loopback_host: "localhost",
        preferred_port: None,
    }
}

/// The port the Dropbox app has to be registered with (`http://localhost:53682/`
/// - the same one rclone uses, so a user who has set that up already is
/// done), because Dropbox matches the redirect URI port for port.
pub const DROPBOX_PORT: u16 = 53682;

/// Dropbox, for an app made in the Dropbox App Console with the redirect
/// URI `http://localhost:53682/`. Dropbox apps have no secret for PKCE.
pub fn dropbox_provider(client_id: &str) -> Provider {
    let (client_id, _) = resolve_client(client_id, None, builtin::dropbox());
    Provider {
        name: "Dropbox",
        auth_url: "https://www.dropbox.com/oauth2/authorize".into(),
        token_url: "https://api.dropboxapi.com/oauth2/token".into(),
        client_id,
        client_secret: None,
        // Dropbox scopes are set on the app, not asked for here.
        scopes: vec![],
        // `offline` is what makes Dropbox hand out a refresh token.
        extra_auth_params: vec![("token_access_type".into(), "offline".into())],
        loopback_host: "localhost",
        preferred_port: Some(DROPBOX_PORT),
    }
}

/// What a sign-in leaves behind. Serialised as JSON into the keychain.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds. Zero means "unknown, treat as expired".
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub scope: Option<String>,
}

impl TokenSet {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Result<TokenSet> {
        serde_json::from_str(s.trim())
            .map_err(|e| Error::AuthFailed(format!("저장된 로그인 정보를 읽을 수 없습니다: {e}")))
    }

    /// Expired, or will be within `margin`.
    pub fn expires_within(&self, margin: Duration) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.expires_at <= now + margin.as_secs()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize, Default)]
struct TokenError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn token_set(r: TokenResponse, previous_refresh: Option<&str>) -> TokenSet {
    TokenSet {
        access_token: r.access_token,
        // A refresh answer usually omits the refresh token; the old one stays
        // valid.
        refresh_token: r
            .refresh_token
            .or_else(|| previous_refresh.map(str::to_string)),
        expires_at: now_secs() + r.expires_in.unwrap_or(3600),
        scope: r.scope,
    }
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("MyDrives/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| Error::Backend(format!("HTTP client: {e}")))
}

async fn token_request(
    provider: &Provider,
    form: &[(&str, &str)],
    previous_refresh: Option<&str>,
) -> Result<TokenSet> {
    let mut fields: Vec<(&str, &str)> = form.to_vec();
    fields.push(("client_id", &provider.client_id));
    if let Some(secret) = &provider.client_secret {
        fields.push(("client_secret", secret));
    }
    let response = http()?
        .post(&provider.token_url)
        .form(&fields)
        .send()
        .await
        .map_err(|e| Error::Offline(format!("{} 로그인 서버에 닿을 수 없습니다: {e}", provider.name)))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let err: TokenError = serde_json::from_str(&body).unwrap_or_default();
        let what = if err.error.is_empty() {
            status.to_string()
        } else if err.error_description.is_empty() {
            err.error
        } else {
            format!("{} ({})", err.error_description, err.error)
        };
        return Err(Error::AuthFailed(format!(
            "{} 로그인이 거부되었습니다: {what}",
            provider.name
        )));
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| Error::AuthFailed(format!("{} 토큰 응답을 읽을 수 없습니다: {e}", provider.name)))?;
    Ok(token_set(parsed, previous_refresh))
}

/// A new access token from the refresh token.
pub async fn refresh(provider: &Provider, refresh_token: &str) -> Result<TokenSet> {
    token_request(
        provider,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh_token)],
        Some(refresh_token),
    )
    .await
}

/// Signs the user in: opens the service's page in the browser (through
/// `open`), waits for it to come back to this process, and exchanges the
/// code for tokens. Gives up after `timeout`.
pub async fn authorize(
    provider: &Provider,
    timeout: Duration,
    open: impl FnOnce(&str) -> Result<()>,
) -> Result<TokenSet> {
    // Without an application to sign in as there is nothing to open: this
    // build shipped no client for the service and the drive brought none.
    if provider.client_id.trim().is_empty() {
        return Err(Error::InvalidArgument(format!(
            "{}에 로그인할 앱 정보가 없습니다 - 이 빌드에는 {} 클라이언트가 들어 있지 않으니 클라이언트 ID를 넣으세요",
            provider.name, provider.name
        )));
    }
    // The preferred port first (a service that matches the registered port
    // needs it), any free port otherwise.
    let listener = match provider.preferred_port {
        Some(p) => match tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
            Ok(l) => l,
            Err(_) => tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(Error::Io)?,
        },
        None => tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(Error::Io)?,
    };
    let port = listener.local_addr().map_err(Error::Io)?.port();
    // A redirect to `localhost` may resolve to `::1` first in the browser;
    // listening there too, on the same port, means it lands either way.
    let listener6 = if provider.loopback_host == "localhost" {
        tokio::net::TcpListener::bind(("::1", port)).await.ok()
    } else {
        None
    };
    let redirect_uri = format!("http://{}:{port}/", provider.loopback_host);

    let verifier = pkce_verifier();
    let challenge = base64url(&sha256(verifier.as_bytes()));
    let state = base64url(&random_bytes(16));

    let mut params: Vec<(String, String)> = vec![
        ("client_id".into(), provider.client_id.clone()),
        ("redirect_uri".into(), redirect_uri.clone()),
        ("response_type".into(), "code".into()),
        ("scope".into(), provider.scopes.join(" ")),
        ("code_challenge".into(), challenge),
        ("code_challenge_method".into(), "S256".into()),
        ("state".into(), state.clone()),
    ];
    params.extend(provider.extra_auth_params.iter().cloned());
    let url = format!("{}?{}", provider.auth_url, encode_query(&params));

    open(&url)?;

    let (code, returned_state) = tokio::time::timeout(timeout, wait_for_redirect(&listener, listener6.as_ref()))
        .await
        .map_err(|_| {
            Error::Timeout(format!(
                "{} 로그인을 기다리다 시간이 지났습니다 - 브라우저에서 로그인을 마치지 않았습니다",
                provider.name
            ))
        })??;
    if returned_state != state {
        return Err(Error::AuthFailed(
            "로그인 응답이 이 요청의 것이 아닙니다 (state 불일치)".into(),
        ));
    }

    token_request(
        provider,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
        ],
        None,
    )
    .await
}

/// Accepts the browser's return visit and takes `code` and `state` from it.
/// Answers with a small page so the tab does not sit on a blank screen.
async fn wait_for_redirect(
    listener: &tokio::net::TcpListener,
    listener6: Option<&tokio::net::TcpListener>,
) -> Result<(String, String)> {
    loop {
        let (mut stream, _) = match listener6 {
            Some(l6) => tokio::select! {
                a = listener.accept() => a,
                b = l6.accept() => b,
            },
            None => listener.accept().await,
        }
        .map_err(Error::Io)?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.map_err(Error::Io)?;
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let target = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();
        // Browsers ask for /favicon.ico too; only the redirect carries a
        // query string.
        let Some((_, query)) = target.split_once('?') else {
            let _ = stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
            continue;
        };
        let fields = decode_query(query);
        let get = |k: &str| fields.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        let (page, result) = match (get("code"), get("state"), get("error")) {
            (Some(code), Some(state), _) => (
                "<html><body style=\"font-family:sans-serif;margin:3em\"><h2>로그인 완료</h2><p>MyDrives로 돌아가세요. 이 창은 닫아도 됩니다.</p></body></html>",
                Ok((code, state)),
            ),
            (_, _, Some(error)) => (
                "<html><body style=\"font-family:sans-serif;margin:3em\"><h2>로그인이 취소되었습니다</h2><p>MyDrives로 돌아가 다시 시도하세요.</p></body></html>",
                Err(Error::AuthFailed(format!(
                    "로그인이 거부되었습니다: {}{}",
                    error,
                    get("error_description").map(|d| format!(" - {d}")).unwrap_or_default()
                ))),
            ),
            _ => {
                let _ = stream
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                continue;
            }
        };
        let body = page.as_bytes();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body).await;
        let _ = stream.flush().await;
        return result;
    }
}

/// Opens `url` in the user's browser.
pub fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("/usr/bin/open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::Backend(format!("브라우저를 열 수 없습니다: {e}")))
}

/// A signed-in service: hands out a usable access token, refreshing it when
/// it is about to run out and telling `persist` so the new set is kept.
pub struct Session {
    provider: Provider,
    tokens: tokio::sync::Mutex<TokenSet>,
    persist: Box<dyn Fn(&TokenSet) + Send + Sync>,
}

impl Session {
    pub fn new(
        provider: Provider,
        tokens: TokenSet,
        persist: impl Fn(&TokenSet) + Send + Sync + 'static,
    ) -> Arc<Session> {
        Arc::new(Session {
            provider,
            tokens: tokio::sync::Mutex::new(tokens),
            persist: Box::new(persist),
        })
    }

    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// A bearer token good for at least a minute.
    pub async fn bearer(&self) -> Result<String> {
        let mut tokens = self.tokens.lock().await;
        if tokens.expires_within(Duration::from_secs(60)) {
            self.renew(&mut tokens).await?;
        }
        Ok(tokens.access_token.clone())
    }

    /// Called after a request answered 401 with a token that looked fine:
    /// the service revoked it early, or the clock is off.
    pub async fn force_refresh(&self) -> Result<String> {
        let mut tokens = self.tokens.lock().await;
        self.renew(&mut tokens).await?;
        Ok(tokens.access_token.clone())
    }

    async fn renew(&self, tokens: &mut TokenSet) -> Result<()> {
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            return Err(Error::AuthFailed(format!(
                "{} 로그인이 만료되었고 갱신 토큰이 없습니다 - 드라이브 편집에서 다시 로그인하세요",
                self.provider.name
            )));
        };
        let fresh = refresh(&self.provider, &refresh_token).await.map_err(|e| match e {
            Error::AuthFailed(msg) => Error::AuthFailed(format!(
                "{msg} - 드라이브 편집에서 다시 로그인하세요"
            )),
            other => other,
        })?;
        *tokens = fresh;
        (self.persist)(tokens);
        Ok(())
    }
}

// ------------------------------------------------------------------ helpers

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    if getrandom::fill(&mut buf).is_err() {
        // No OS randomness is a broken machine; still, never hand out zeros.
        let seed = sha256(format!("{:?}{}", SystemTime::now(), std::process::id()).as_bytes());
        for (i, b) in buf.iter_mut().enumerate() {
            *b = seed[i % seed.len()];
        }
    }
    buf
}

fn pkce_verifier() -> String {
    // 32 random bytes → 43 URL-safe characters, inside RFC 7636's 43..128.
    base64url(&random_bytes(32))
}

fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).to_vec()
}

/// Base64 with the URL alphabet and no padding, as PKCE wants it.
pub fn base64url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn encode_query(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn decode_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_pieces_have_the_documented_shape() {
        let v = pkce_verifier();
        assert!(v.len() >= 43 && v.len() <= 128, "{}", v.len());
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        // RFC 7636 appendix B: the worked example.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url(&sha256(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn queries_round_trip() {
        let params = vec![
            ("scope".to_string(), "a b/c".to_string()),
            ("x".to_string(), "한글&=".to_string()),
        ];
        let encoded = encode_query(&params);
        assert_eq!(encoded, "scope=a%20b%2Fc&x=%ED%95%9C%EA%B8%80%26%3D");
        assert_eq!(decode_query(&encoded), params);
        assert_eq!(decode_query("code=4%2Fabc&state=s+t"), vec![
            ("code".to_string(), "4/abc".to_string()),
            ("state".to_string(), "s t".to_string()),
        ]);
    }

    #[test]
    fn a_token_set_knows_when_it_is_running_out() {
        let fresh = TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: now_secs() + 3600,
            scope: None,
        };
        assert!(!fresh.expires_within(Duration::from_secs(60)));
        let stale = TokenSet {
            expires_at: now_secs() + 30,
            ..fresh.clone()
        };
        assert!(stale.expires_within(Duration::from_secs(60)));
        let back = TokenSet::from_json(&fresh.to_json()).unwrap();
        assert_eq!(back, fresh);
    }
}

#[cfg(test)]
mod builtin_tests {
    use super::*;

    const SHIPPED: Option<builtin::Client> = Some(builtin::Client {
        client_id: "shipped.apps.googleusercontent.com",
        client_secret: Some("shipped-secret"),
    });

    #[test]
    fn a_drive_without_a_client_signs_in_as_the_one_the_build_shipped() {
        let (id, secret) = resolve_client("", None, SHIPPED);
        assert_eq!(id, "shipped.apps.googleusercontent.com");
        assert_eq!(secret.as_deref(), Some("shipped-secret"));
    }

    #[test]
    fn a_drive_that_brought_its_own_client_keeps_it() {
        let (id, secret) = resolve_client("  mine  ", Some(" mine-secret "), SHIPPED);
        assert_eq!(id, "mine");
        assert_eq!(secret.as_deref(), Some("mine-secret"));
    }

    #[test]
    fn nothing_shipped_and_nothing_given_leaves_the_client_empty() {
        let (id, secret) = resolve_client("   ", Some("   "), None);
        assert!(id.is_empty(), "{id}");
        assert_eq!(secret, None);
    }

    #[test]
    fn a_service_without_a_shipped_client_is_asked_for_one() {
        assert!(!has_builtin_client("smb"));
        assert_eq!(builtin::for_kind("ftp"), None);
    }
}
