//! Control channel: the tray UI and the `mydrives` CLI talk to the agent over it.
//!
//! A local socket rather than a TCP port, so the OS enforces who may talk to the
//! agent: a unix socket with 0600 in the user's runtime directory, and a named
//! pipe restricted to the current user on Windows. Protocol is newline-delimited
//! JSON, which keeps the agent debuggable with `nc`/`socat`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use mydrives_core::config::RemoteConfig;
use mydrives_core::manager::RemoteStatus;
use mydrives_core::update::UpdateStatus;

pub const PROTOCOL_VERSION: u32 = 2;

/// What `Request::Update` should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAction {
    /// Just report.
    Status,
    /// Ask the agent to look for a new release now.
    Check,
    /// Apply a staged update now, even if a drive is in use.
    Apply,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Providers,
    /// `replace` says this is an edit of the drive already called `remote.id`:
    /// its settings may point somewhere new, and a `None` secret keeps the
    /// stored one. Without it, an id already in use by a drive pointing
    /// elsewhere is refused, so a typo cannot swallow another drive.
    Add {
        remote: RemoteConfig,
        secret: Option<String>,
        #[serde(default)]
        replace: bool,
    },
    /// Try the settings without saving anything: connect, read the root, done.
    /// The UI calls this before `Add` so a typo in the host or password is
    /// reported immediately instead of becoming a failed mount later.
    Test { remote: RemoteConfig, secret: Option<String> },
    /// Ask the server which shares it offers (the `share` field is ignored).
    ListShares { remote: RemoteConfig, secret: Option<String> },
    Remove { id: String },
    /// A drive's saved settings, so the editor can open pre-filled. The reply
    /// carries no secret - its `SecretRef` only says where the password lives.
    Describe { id: String },
    /// Change what a drive is called (the label, not the id).
    Rename { id: String, label: String },
    Mount { id: String },
    Unmount { id: String },
    /// Update state, or ask for a check / an immediate apply.
    Update { action: UpdateAction },
    /// Store a GitHub token for update checks against a private repository.
    SetUpdateToken { token: String },
    /// macOS mount layer: report FUSE-T, or install it now.
    FuseT { install: bool },
    /// Login-item registration. `None` only reports; `Some` changes it.
    /// Handled by the agent, not the caller, because the login item has to
    /// point at the *agent* binary and only the agent knows where it lives.
    Autostart { enable: Option<bool> },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong { version: u32, agent: String },
    Status { remotes: Vec<RemoteStatus> },
    Providers { providers: Vec<ProviderInfo> },
    Mounted { id: String, mountpoint: String },
    Tested { summary: String },
    Shares { shares: Vec<String> },
    /// A drive's saved settings, for the editor. No secret is included.
    Described { remote: RemoteConfig },
    Update { status: UpdateStatus },
    FuseT { installed: bool, version: Option<String>, detail: String },
    /// `detail` is the registration itself (plist path / Run value), so the
    /// user can see what was written rather than trusting a boolean.
    Autostart { enabled: bool, detail: String },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    /// `None` means usable; otherwise why not.
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("the MyDrives agent is not running ({0})")]
    NotRunning(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
}

pub type IpcResult<T> = std::result::Result<T, IpcError>;

/// Endpoint address. Overridable for tests and for running several agents.
pub fn endpoint() -> String {
    if let Ok(v) = std::env::var("MYDRIVES_IPC") {
        return v;
    }
    #[cfg(windows)]
    {
        format!(
            r"\\.\pipe\mydrives-{}",
            std::env::var("USERNAME").unwrap_or_else(|_| "user".into())
        )
    }
    #[cfg(unix)]
    {
        socket_path().display().to_string()
    }
}

#[cfg(unix)]
pub fn socket_path() -> PathBuf {
    if let Ok(v) = std::env::var("MYDRIVES_IPC") {
        return PathBuf::from(v);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.join(format!("mydrives-{}.sock", libc_getuid()))
}

#[cfg(unix)]
fn libc_getuid() -> u32 {
    // Avoiding a libc dependency for one call.
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(not(unix))]
pub fn socket_path() -> PathBuf {
    PathBuf::from(endpoint())
}

// --------------------------------------------------------------------- client

/// One request, one response. Short-lived connections keep the agent simple and
/// make a crashed CLI impossible to notice.
pub async fn call(req: &Request) -> IpcResult<Response> {
    let payload = serde_json::to_string(req).map_err(|e| IpcError::Protocol(e.to_string()))?;
    let mut line = String::new();

    #[cfg(unix)]
    {
        let path = socket_path();
        let stream = tokio::net::UnixStream::connect(&path).await.map_err(|e| {
            IpcError::NotRunning(format!("{}: {e}", path.display()))
        })?;
        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(payload.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;
        let mut reader = BufReader::new(read_half);
        reader.read_line(&mut line).await?;
    }
    #[cfg(windows)]
    {
        let name = endpoint();
        let client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .map_err(|e| IpcError::NotRunning(format!("{name}: {e}")))?;
        let mut stream = client;
        stream.write_all(payload.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;
        let mut reader = BufReader::new(stream);
        reader.read_line(&mut line).await?;
    }

    if line.trim().is_empty() {
        return Err(IpcError::Protocol("empty response".into()));
    }
    serde_json::from_str(&line).map_err(|e| IpcError::Protocol(format!("{e}: {line}")))
}

// --------------------------------------------------------------------- server

/// Serves requests until the handler asks to stop (by returning `false`).
pub async fn serve<H, F>(handler: H) -> IpcResult<()>
where
    H: Fn(Request) -> F + Send + Sync + 'static,
    F: std::future::Future<Output = (Response, bool)> + Send + 'static,
{
    let handler = std::sync::Arc::new(handler);

    #[cfg(unix)]
    {
        let path = socket_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // A stale socket from a crashed agent would make bind() fail; the agent is
        // single-instance, so removing it is correct.
        if path.exists() {
            if tokio::net::UnixStream::connect(&path).await.is_ok() {
                return Err(IpcError::Protocol(format!(
                    "another MyDrives agent is already listening on {}",
                    path.display()
                )));
            }
            let _ = std::fs::remove_file(&path);
        }
        let listener = tokio::net::UnixListener::bind(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!(endpoint = %path.display(), "control socket ready");

        // Each connection is handled on its own task. A single request can
        // block for seconds - unmounting a busy FUSE-T volume waits on the
        // kernel - and handling connections inline would freeze every other
        // request behind it (a `status` call could not answer while one drive
        // was disconnecting). `shutdown` is a request like any other, so the
        // task that sees it signals the accept loop to stop.
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel::<()>(1);
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let handler = handler.clone();
                    let stop_tx = stop_tx.clone();
                    tokio::spawn(async move {
                        if handle_conn(stream, handler).await {
                            let _ = stop_tx.send(()).await;
                        }
                    });
                }
                _ = stop_rx.recv() => break,
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let name = endpoint();
        tracing::info!(endpoint = %name, "control pipe ready");
        let mut server = ServerOptions::new().first_pipe_instance(true).create(&name)?;
        loop {
            server.connect().await?;
            let connected = server;
            server = ServerOptions::new().create(&name)?;
            let handler = handler.clone();
            let stop = handle_conn(connected, handler).await;
            if stop {
                break;
            }
        }
    }

    Ok(())
}

async fn handle_conn<S, H, F>(stream: S, handler: std::sync::Arc<H>) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    H: Fn(Request) -> F + Send + Sync + 'static,
    F: std::future::Future<Output = (Response, bool)> + Send,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return false;
    }
    let (response, stop) = match serde_json::from_str::<Request>(&line) {
        Ok(req) => handler(req).await,
        Err(e) => (
            Response::Error {
                message: format!("bad request: {e}"),
            },
            false,
        ),
    };
    let mut out = serde_json::to_string(&response).unwrap_or_else(|e| {
        format!("{{\"result\":\"error\",\"message\":\"serialize: {e}\"}}")
    });
    out.push('\n');
    let stream = reader.into_inner();
    let mut stream = stream;
    if let Err(e) = stream.write_all(out.as_bytes()).await {
        tracing::debug!(error = %e, "client went away before reading the response");
    }
    let _ = stream.flush().await;
    stop
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_roundtrip_as_json() {
        let req = Request::Mount { id: "nas".into() };
        let text = serde_json::to_string(&req).unwrap();
        assert!(text.contains("\"op\":\"mount\""), "{text}");
        match serde_json::from_str::<Request>(&text).unwrap() {
            Request::Mount { id } => assert_eq!(id, "nas"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn responses_roundtrip_as_json() {
        let res = Response::Mounted {
            id: "nas".into(),
            mountpoint: "Z:".into(),
        };
        let text = serde_json::to_string(&res).unwrap();
        let back: Response = serde_json::from_str(&text).unwrap();
        assert!(matches!(back, Response::Mounted { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_reports_a_missing_agent_clearly() {
        std::env::set_var("MYDRIVES_IPC", "/tmp/mydrives-test-missing.sock");
        let err = call(&Request::Ping).await.unwrap_err();
        assert!(matches!(err, IpcError::NotRunning(_)), "{err}");
        std::env::remove_var("MYDRIVES_IPC");
    }
}
