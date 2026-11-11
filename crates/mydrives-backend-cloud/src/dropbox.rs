//! Dropbox, through the HTTP API v2 (dropbox.com/developers/documentation/
//! http, read 2026-11-11).
//!
//! Dropbox addresses things by **path**, not id, so here an item's id *is*
//! its path inside the account (`/Work/2026/todo.txt`; the root is the empty
//! string). Paths compare without case, as Dropbox does. Listings come from
//! `files/list_folder` and its `continue` cursor; downloads from the content
//! host with the arguments in a `Dropbox-API-Arg` header and `Range` honoured;
//! uploads with one `files/upload` up to 150 MB and an upload session above
//! it. Deleting sends to Dropbox's deleted-files history, as the web site
//! does.
//!
//! Signed in with OAuth as a PKCE app made in the Dropbox App Console with
//! the redirect URI `http://localhost:53682/` - Dropbox matches it port for
//! port, so the port is fixed (and shared with rclone's convention).

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use mydrives_core::backend::Backend;
use mydrives_core::error::{Error, Result};
use mydrives_core::oauth::Session;
use mydrives_core::types::{FileKind, FsStat};

use crate::{CloudApi, CloudBackend, CloudCaps, Item, ItemId};

const API: &str = "https://api.dropboxapi.com";
const CONTENT: &str = "https://content.dropboxapi.com";
/// Above this an upload goes through a session; the single call takes 150 MB.
const SIMPLE_UPLOAD_MAX: u64 = 100 * 1024 * 1024;
/// Session fragments: a multiple of 4 MiB, as Dropbox asks.
const FRAGMENT: u64 = 8 * 1024 * 1024;

pub struct DropboxApi {
    http: reqwest::Client,
    api: String,
    content: String,
    session: Arc<Session>,
    subpath: Option<String>,
}

impl DropboxApi {
    pub fn new(session: Arc<Session>, subpath: Option<String>) -> DropboxApi {
        DropboxApi::with_bases(API, CONTENT, session, subpath)
    }

    pub fn with_bases(
        api: &str,
        content: &str,
        session: Arc<Session>,
        subpath: Option<String>,
    ) -> DropboxApi {
        let http = reqwest::Client::builder()
            .user_agent(concat!("MyDrives/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        DropboxApi {
            http,
            api: api.trim_end_matches('/').to_string(),
            content: content.trim_end_matches('/').to_string(),
            session,
            subpath,
        }
    }

    async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
        what: &str,
    ) -> Result<reqwest::Response> {
        let token = self.session.bearer().await?;
        let response = build()
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| transport_error(e, what))?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return check(response, what).await;
        }
        let token = self.session.force_refresh().await?;
        let response = build()
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| transport_error(e, what))?;
        check(response, what).await
    }

    /// An RPC call: JSON in, JSON out.
    async fn rpc<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        body: serde_json::Value,
        what: &str,
    ) -> Result<T> {
        let url = format!("{}/2/{endpoint}", self.api);
        self.send(|| self.http.post(&url).json(&body), what)
            .await?
            .json::<T>()
            .await
            .map_err(|e| Error::Backend(format!("Dropbox {what}: 응답을 읽을 수 없습니다: {e}")))
    }

    /// A content call: the arguments go in a header, the bytes in the body.
    fn content_request(&self, endpoint: &str, arg: &serde_json::Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}/2/{endpoint}", self.content))
            .header("Dropbox-API-Arg", escape_header(&arg.to_string()))
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
    }
}

/// `Dropbox-API-Arg` must be ASCII; Dropbox reads `\uXXXX` escapes for the
/// rest, which a Korean file name is.
fn escape_header(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() && c != '\u{7f}' {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

async fn check(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(status_error(status, &body, what))
}

fn transport_error(e: reqwest::Error, what: &str) -> Error {
    if e.is_timeout() || e.is_connect() {
        Error::Offline(format!("Dropbox {what}: 서버에 닿을 수 없습니다: {e}"))
    } else {
        Error::Backend(format!("Dropbox {what}: {e}"))
    }
}

#[derive(Deserialize, Default)]
struct ApiError {
    #[serde(default)]
    error_summary: String,
}

fn status_error(status: reqwest::StatusCode, body: &str, what: &str) -> Error {
    let summary = serde_json::from_str::<ApiError>(body)
        .map(|e| e.error_summary)
        .unwrap_or_else(|_| body.chars().take(200).collect());
    match status.as_u16() {
        401 => Error::AuthFailed(
            "Dropbox 로그인이 더 이상 유효하지 않습니다 - 드라이브 편집에서 다시 로그인하세요".into(),
        ),
        403 => Error::PermissionDenied(format!("Dropbox {what}: {summary}")),
        409 if summary.contains("not_found") => Error::NotFound(format!("Dropbox {what}: {summary}")),
        409 if summary.contains("conflict") => Error::AlreadyExists(format!("Dropbox {what}: {summary}")),
        409 if summary.contains("insufficient_space") => {
            Error::Backend(format!("Dropbox {what}: 저장 공간이 부족합니다"))
        }
        409 => Error::Backend(format!("Dropbox {what}: {summary}")),
        429 => Error::Timeout(format!("Dropbox {what}: 요청 한도를 넘었습니다, 잠시 뒤 다시 시도됩니다")),
        500..=599 => Error::Offline(format!("Dropbox {what}: 서비스 오류 {status}")),
        _ => Error::Backend(format!("Dropbox {what}: {status} {summary}")),
    }
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = ".tag", default)]
    tag: String,
    name: String,
    #[serde(default)]
    path_display: Option<String>,
    #[serde(default)]
    path_lower: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    client_modified: String,
    #[serde(default)]
    server_modified: String,
}

impl Metadata {
    fn into_item(self) -> Option<Item> {
        let kind = match self.tag.as_str() {
            "file" => FileKind::File,
            "folder" => FileKind::Dir,
            // A "deleted" entry, or something new; not a thing to list.
            _ => return None,
        };
        let path = self
            .path_display
            .or(self.path_lower)
            .unwrap_or_else(|| format!("/{}", self.name));
        let modified = crate::rfc3339::parse(&self.client_modified)
            .or_else(|| crate::rfc3339::parse(&self.server_modified))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        Some(Item {
            id: path,
            name: self.name,
            kind,
            size: self.size,
            created: modified,
            modified,
            hidden: false,
        })
    }
}

#[derive(Deserialize)]
struct ListPage {
    #[serde(default)]
    entries: Vec<Metadata>,
    #[serde(default)]
    cursor: String,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct FolderCreated {
    metadata: Metadata,
}

#[derive(Deserialize)]
struct SessionStarted {
    session_id: String,
}

#[derive(Deserialize)]
struct SpaceUsage {
    #[serde(default)]
    used: u64,
    #[serde(default)]
    allocation: Allocation,
}

#[derive(Deserialize, Default)]
struct Allocation {
    #[serde(default)]
    allocated: u64,
}

/// The names Dropbox refuses to store, as its documentation lists them.
/// Checking here turns a doomed upload at close into a refusal the moment
/// the file is made, which is what a filesystem is supposed to say.
fn disallowed_name(name: &str) -> bool {
    const NEVER: [&str; 5] = [".ds_store", "desktop.ini", "thumbs.db", ".dropbox", ".dropbox.attr"];
    let lower = name.to_ascii_lowercase();
    NEVER.contains(&lower.as_str()) || lower.ends_with('~') || lower.starts_with("icon\r")
}

fn join(parent: &str, name: &str) -> String {
    format!("{}/{}", parent.trim_end_matches('/'), name)
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "",
        Some(i) => &path[..i],
    }
}

fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[async_trait]
impl CloudApi for DropboxApi {
    fn describe(&self) -> String {
        format!("dropbox://{}", self.subpath.as_deref().unwrap_or("/"))
    }

    fn caps(&self) -> CloudCaps {
        CloudCaps {
            range_reads: true,
            case_sensitive: false,
        }
    }

    fn root_id(&self) -> ItemId {
        String::new()
    }

    async fn connect(&self) -> Result<()> {
        self.quota().await.map(|_| ())
    }

    async fn list(&self, folder: &str) -> Result<Vec<Item>> {
        let mut page: ListPage = self
            .rpc(
                "files/list_folder",
                serde_json::json!({
                    "path": folder,
                    "recursive": false,
                    "include_deleted": false,
                    "include_non_downloadable_files": false,
                    "limit": 2000,
                }),
                "목록",
            )
            .await?;
        let mut out: Vec<Item> = page.entries.drain(..).filter_map(Metadata::into_item).collect();
        while page.has_more {
            page = self
                .rpc(
                    "files/list_folder/continue",
                    serde_json::json!({ "cursor": page.cursor }),
                    "목록",
                )
                .await?;
            out.extend(page.entries.drain(..).filter_map(Metadata::into_item));
        }
        Ok(out)
    }

    async fn mkdir(&self, parent: &str, name: &str) -> Result<Item> {
        let created: FolderCreated = self
            .rpc(
                "files/create_folder_v2",
                serde_json::json!({ "path": join(parent, name), "autorename": false }),
                "폴더 생성",
            )
            .await?;
        // Outside a listing the metadata comes without a `.tag`; it is a
        // folder by construction.
        let mut metadata = created.metadata;
        metadata.tag = "folder".into();
        metadata
            .into_item()
            .ok_or_else(|| Error::Backend("Dropbox 폴더 생성: 응답이 폴더가 아닙니다".into()))
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.rpc::<serde_json::Value>(
            "files/delete_v2",
            serde_json::json!({ "path": id }),
            "삭제",
        )
        .await
        .map(|_| ())
    }

    async fn rename(&self, id: &str, name: &str) -> Result<()> {
        self.rpc::<serde_json::Value>(
            "files/move_v2",
            serde_json::json!({
                "from_path": id,
                "to_path": join(parent_of(id), name),
                "autorename": false,
            }),
            "이름 변경",
        )
        .await
        .map(|_| ())
    }

    async fn move_to(&self, id: &str, parent: &str) -> Result<()> {
        self.rpc::<serde_json::Value>(
            "files/move_v2",
            serde_json::json!({
                "from_path": id,
                "to_path": join(parent, name_of(id)),
                "autorename": false,
            }),
            "이동",
        )
        .await
        .map(|_| ())
    }

    async fn download(&self, id: &str, into: &mut tokio::fs::File) -> Result<u64> {
        use tokio::io::AsyncWriteExt;
        let arg = serde_json::json!({ "path": id });
        let mut response = self
            .send(|| self.content_request("files/download", &arg), "다운로드")
            .await?;
        let mut total = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| transport_error(e, "다운로드"))?
        {
            into.write_all(&chunk).await.map_err(Error::Io)?;
            total += chunk.len() as u64;
        }
        into.flush().await.map_err(Error::Io)?;
        Ok(total)
    }

    async fn download_range(&self, id: &str, offset: u64, len: u32) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let arg = serde_json::json!({ "path": id });
        let range = format!("bytes={}-{}", offset, offset + len as u64 - 1);
        let response = self
            .send(
                || {
                    self.content_request("files/download", &arg)
                        .header(reqwest::header::RANGE, range.as_str())
                },
                "읽기",
            )
            .await?;
        let whole = response.status() == reqwest::StatusCode::OK;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| transport_error(e, "읽기"))?;
        if whole {
            let start = (offset as usize).min(bytes.len());
            let end = (start + len as usize).min(bytes.len());
            return Ok(bytes[start..end].to_vec());
        }
        Ok(bytes.to_vec())
    }

    async fn upload(
        &self,
        parent: &str,
        name: &str,
        mut from: tokio::fs::File,
        len: u64,
        _modified: Option<SystemTime>,
    ) -> Result<()> {
        // Dropbox rejects a handful of names outright. Finder drops a
        // .DS_Store into every folder it opens, so without this every
        // browse ended in a failed upload the user could see.
        if disallowed_name(name) {
            return Err(Error::PermissionDenied(format!(
                "Dropbox는 '{name}'이라는 이름을 받지 않습니다"
            )));
        }
        let commit = serde_json::json!({
            "path": join(parent, name),
            "mode": "overwrite",
            "autorename": false,
            "mute": true,
        });
        if len <= SIMPLE_UPLOAD_MAX {
            let mut data = Vec::with_capacity(len as usize);
            from.read_to_end(&mut data).await.map_err(Error::Io)?;
            self.send(
                || self.content_request("files/upload", &commit).body(data.clone()),
                "업로드",
            )
            .await?;
            return Ok(());
        }
        // Session: start with the first fragment, append the middle ones,
        // finish with the last and the commit.
        let first = read_fragment(&mut from, FRAGMENT as usize).await?;
        let started: SessionStarted = self
            .send(
                || {
                    self.content_request(
                        "files/upload_session/start",
                        &serde_json::json!({ "close": false }),
                    )
                    .body(first.clone())
                },
                "업로드 시작",
            )
            .await?
            .json()
            .await
            .map_err(|e| Error::Backend(format!("Dropbox 업로드 시작: 응답을 읽을 수 없습니다: {e}")))?;
        let mut offset = first.len() as u64;
        while offset < len {
            let remaining = len - offset;
            let chunk = read_fragment(&mut from, remaining.min(FRAGMENT) as usize).await?;
            if chunk.is_empty() {
                return Err(Error::Backend(format!(
                    "Dropbox 업로드: 파일이 {len} 바이트라고 했는데 {offset} 바이트에서 끝났습니다"
                )));
            }
            let cursor = serde_json::json!({ "session_id": started.session_id, "offset": offset });
            let last = offset + chunk.len() as u64 >= len;
            if last {
                let arg = serde_json::json!({ "cursor": cursor, "commit": commit });
                self.send(
                    || self.content_request("files/upload_session/finish", &arg).body(chunk.clone()),
                    "업로드 마무리",
                )
                .await?;
            } else {
                let arg = serde_json::json!({ "cursor": cursor, "close": false });
                self.send(
                    || self.content_request("files/upload_session/append_v2", &arg).body(chunk.clone()),
                    "업로드",
                )
                .await?;
            }
            offset += chunk.len() as u64;
        }
        if offset == len && len == first.len() as u64 {
            // The whole file fit in the first fragment: finish with nothing.
            let cursor = serde_json::json!({ "session_id": started.session_id, "offset": offset });
            let arg = serde_json::json!({ "cursor": cursor, "commit": commit });
            self.send(
                || self.content_request("files/upload_session/finish", &arg).body(Vec::new()),
                "업로드 마무리",
            )
            .await?;
        }
        Ok(())
    }

    async fn quota(&self) -> Result<FsStat> {
        let usage: SpaceUsage = self
            .rpc("users/get_space_usage", serde_json::Value::Null, "용량 조회")
            .await?;
        let total = if usage.allocation.allocated == 0 {
            usage.used + (1u64 << 40)
        } else {
            usage.allocation.allocated
        };
        Ok(FsStat {
            total_bytes: total,
            free_bytes: total.saturating_sub(usage.used),
            block_size: 4096,
            total_files: 0,
            free_files: 0,
        })
    }
}

/// Up to `want` bytes from `from` (fewer only at the end of the file).
async fn read_fragment(from: &mut tokio::fs::File, want: usize) -> Result<Vec<u8>> {
    let want = want.min(FRAGMENT as usize);
    let mut chunk = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        let n = from.read(&mut chunk[filled..]).await.map_err(Error::Io)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    chunk.truncate(filled);
    Ok(chunk)
}

/// A Dropbox: the account, or one folder of it.
pub fn backend(id: String, session: Arc<Session>, subpath: Option<String>) -> Arc<dyn Backend> {
    let api = DropboxApi::new(session, subpath.clone());
    Arc::new(CloudBackend::new(id, api, subpath.as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_joined_and_split_like_dropbox_wants() {
        assert_eq!(join("", "a.txt"), "/a.txt");
        assert_eq!(join("/Work", "2026"), "/Work/2026");
        assert_eq!(parent_of("/Work/2026/x.txt"), "/Work/2026");
        assert_eq!(parent_of("/x.txt"), "");
        assert_eq!(name_of("/Work/2026/x.txt"), "x.txt");
    }

    #[test]
    fn the_argument_header_stays_ascii() {
        let h = escape_header(r#"{"path":"/회의록.pdf"}"#);
        assert!(h.is_ascii());
        assert!(h.contains("\\ud68c"), "{h}");
    }
}
