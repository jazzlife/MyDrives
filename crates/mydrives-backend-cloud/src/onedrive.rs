//! OneDrive, personal or work, through Microsoft Graph v1.0 (learn.microsoft.
//! com/graph, read 2026-11-11).
//!
//! Signed in with OAuth as a public client registered in the user's own
//! Entra tenant ("Mobile and desktop applications", redirect
//! `http://localhost`). Items are listed as a folder's `children`, pages
//! following `@odata.nextLink`. A download is the item's pre-authenticated
//! `@microsoft.graph.downloadUrl`, which honours `Range` - so reads need no
//! local copy. Small files are uploaded with one PUT to `.../content`; larger
//! ones through an upload session in 10 MiB fragments (a multiple of the
//! 320 KiB Graph requires). Deleting sends to the recycle bin.

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

const API: &str = "https://graph.microsoft.com/v1.0";
const SELECT: &str = "id,name,size,file,folder,lastModifiedDateTime,createdDateTime";
/// Up to this size a file goes in one request; above it, an upload session.
const SIMPLE_UPLOAD_MAX: u64 = 4 * 1024 * 1024;
/// 32 × 320 KiB, inside Graph's 60 MiB ceiling for one fragment.
const FRAGMENT: u64 = 10 * 1024 * 1024;

pub struct OnedriveApi {
    http: reqwest::Client,
    base: String,
    session: Arc<Session>,
    subpath: Option<String>,
}

impl OnedriveApi {
    pub fn new(session: Arc<Session>, subpath: Option<String>) -> OnedriveApi {
        OnedriveApi::with_base(API, session, subpath)
    }

    pub fn with_base(base: &str, session: Arc<Session>, subpath: Option<String>) -> OnedriveApi {
        let http = reqwest::Client::builder()
            .user_agent(concat!("MyDrives/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        OnedriveApi {
            http,
            base: base.trim_end_matches('/').to_string(),
            session,
            subpath,
        }
    }

    /// `/me/drive/root` for the root, `/me/drive/items/{id}` otherwise.
    fn item_url(&self, id: &str, tail: &str) -> String {
        if id == "root" {
            format!("{}/me/drive/root{tail}", self.base)
        } else {
            format!("{}/me/drive/items/{id}{tail}", self.base)
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

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
        what: &str,
    ) -> Result<T> {
        self.send(build, what)
            .await?
            .json::<T>()
            .await
            .map_err(|e| Error::Backend(format!("OneDrive {what}: 응답을 읽을 수 없습니다: {e}")))
    }

    /// The pre-authenticated URL for a file's bytes, good for a few minutes.
    async fn download_url(&self, id: &str) -> Result<String> {
        let item: DownloadInfo = self
            .json(
                || {
                    self.http
                        .get(self.item_url(id, ""))
                        .query(&[("$select", "id,@microsoft.graph.downloadUrl")])
                },
                "다운로드 URL",
            )
            .await?;
        item.download_url
            .ok_or_else(|| Error::Backend("OneDrive 다운로드 URL: 파일이 아니거나 내려받을 수 없습니다".into()))
    }
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
        Error::Offline(format!("OneDrive {what}: 서버에 닿을 수 없습니다: {e}"))
    } else {
        Error::Backend(format!("OneDrive {what}: {e}"))
    }
}

#[derive(Deserialize, Default)]
struct ApiError {
    #[serde(default)]
    error: ApiErrorBody,
}

#[derive(Deserialize, Default)]
struct ApiErrorBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

fn status_error(status: reqwest::StatusCode, body: &str, what: &str) -> Error {
    let err: ApiError = serde_json::from_str(body).unwrap_or_default();
    let code = err.error.code;
    let message = if err.error.message.is_empty() {
        status.to_string()
    } else {
        err.error.message
    };
    match status.as_u16() {
        401 => Error::AuthFailed(
            "Microsoft 로그인이 더 이상 유효하지 않습니다 - 드라이브 편집에서 다시 로그인하세요".into(),
        ),
        403 => Error::PermissionDenied(format!("OneDrive {what}: {message}")),
        404 => Error::NotFound(format!("OneDrive {what}: {message}")),
        409 => Error::AlreadyExists(format!("OneDrive {what}: {message}")),
        423 => Error::SharingViolation(format!("OneDrive {what}: {message}")),
        429 | 503 if code == "activityLimitReached" || status.as_u16() == 429 => {
            Error::Timeout(format!("OneDrive {what}: 요청 한도를 넘었습니다, 잠시 뒤 다시 시도됩니다"))
        }
        507 => Error::Backend(format!("OneDrive {what}: 저장 공간이 부족합니다")),
        500..=599 => Error::Offline(format!("OneDrive {what}: 서비스 오류 {status}")),
        _ => Error::Backend(format!("OneDrive {what}: {message} ({code})")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DriveItem {
    id: String,
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    folder: Option<serde_json::Value>,
    #[serde(default)]
    last_modified_date_time: String,
    #[serde(default)]
    created_date_time: String,
}

impl From<DriveItem> for Item {
    fn from(d: DriveItem) -> Item {
        let modified =
            crate::rfc3339::parse(&d.last_modified_date_time).unwrap_or(SystemTime::UNIX_EPOCH);
        Item {
            id: d.id,
            name: d.name,
            kind: if d.folder.is_some() {
                FileKind::Dir
            } else {
                FileKind::File
            },
            size: d.size,
            created: crate::rfc3339::parse(&d.created_date_time).unwrap_or(modified),
            modified,
            hidden: false,
        }
    }
}

#[derive(Deserialize)]
struct Children {
    #[serde(default)]
    value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
}

#[derive(Deserialize)]
struct DownloadInfo {
    #[serde(rename = "@microsoft.graph.downloadUrl", default)]
    download_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadSession {
    upload_url: String,
}

#[derive(Deserialize)]
struct DriveInfo {
    #[serde(default)]
    quota: Quota,
}

#[derive(Deserialize, Default)]
struct Quota {
    #[serde(default)]
    total: u64,
    #[serde(default)]
    used: u64,
}

#[async_trait]
impl CloudApi for OnedriveApi {
    fn describe(&self) -> String {
        format!("onedrive://{}", self.subpath.as_deref().unwrap_or("/"))
    }

    fn caps(&self) -> CloudCaps {
        CloudCaps {
            range_reads: true,
            // OneDrive, like Windows, does not tell names apart by case.
            case_sensitive: false,
        }
    }

    fn root_id(&self) -> ItemId {
        "root".into()
    }

    async fn connect(&self) -> Result<()> {
        self.quota().await.map(|_| ())
    }

    async fn list(&self, folder: &str) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let page: Children = self
                .json(
                    || match &next {
                        // The continuation link carries its own query.
                        Some(url) => self.http.get(url),
                        None => self
                            .http
                            .get(self.item_url(folder, "/children"))
                            .query(&[("$select", SELECT), ("$top", "200")]),
                    },
                    "목록",
                )
                .await?;
            out.extend(page.value.into_iter().map(Item::from));
            match page.next_link {
                Some(url) if !url.is_empty() => next = Some(url),
                _ => break,
            }
        }
        Ok(out)
    }

    async fn mkdir(&self, parent: &str, name: &str) -> Result<Item> {
        let body = serde_json::json!({
            "name": name,
            "folder": {},
            "@microsoft.graph.conflictBehavior": "fail",
        });
        let item: DriveItem = self
            .json(
                || self.http.post(self.item_url(parent, "/children")).json(&body),
                "폴더 생성",
            )
            .await?;
        Ok(item.into())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.send(|| self.http.delete(self.item_url(id, "")), "삭제")
            .await
            .map(|_| ())
    }

    async fn rename(&self, id: &str, name: &str) -> Result<()> {
        let body = serde_json::json!({ "name": name });
        self.send(|| self.http.patch(self.item_url(id, "")).json(&body), "이름 변경")
            .await
            .map(|_| ())
    }

    async fn move_to(&self, id: &str, parent: &str) -> Result<()> {
        let reference = if parent == "root" {
            serde_json::json!({ "path": "/drive/root" })
        } else {
            serde_json::json!({ "id": parent })
        };
        let body = serde_json::json!({ "parentReference": reference });
        self.send(|| self.http.patch(self.item_url(id, "")).json(&body), "이동")
            .await
            .map(|_| ())
    }

    async fn download(&self, id: &str, into: &mut tokio::fs::File) -> Result<u64> {
        use tokio::io::AsyncWriteExt;
        let url = self.download_url(id).await?;
        // The URL is the credential; no bearer token goes with it.
        let mut response = check(
            self.http
                .get(&url)
                .send()
                .await
                .map_err(|e| transport_error(e, "다운로드"))?,
            "다운로드",
        )
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
        let url = self.download_url(id).await?;
        let range = format!("bytes={}-{}", offset, offset + len as u64 - 1);
        let response = check(
            self.http
                .get(&url)
                .header(reqwest::header::RANGE, range)
                .send()
                .await
                .map_err(|e| transport_error(e, "읽기"))?,
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
        let encoded = percent_encode_segment(name);
        if len <= SIMPLE_UPLOAD_MAX {
            // One PUT; replaces an existing file of that name.
            let mut data = Vec::with_capacity(len as usize);
            from.read_to_end(&mut data).await.map_err(Error::Io)?;
            let url = self.item_url(parent, &format!(":/{encoded}:/content"));
            self.send(
                || {
                    self.http
                        .put(&url)
                        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                        .body(data.clone())
                },
                "업로드",
            )
            .await?;
            return Ok(());
        }
        // An upload session, fragments in order, the last one commits.
        let body = serde_json::json!({
            "item": { "@microsoft.graph.conflictBehavior": "replace", "name": name }
        });
        let url = self.item_url(parent, &format!(":/{encoded}:/createUploadSession"));
        let session: UploadSession = self
            .json(|| self.http.post(&url).json(&body), "업로드 시작")
            .await?;
        let mut sent = 0u64;
        let mut buf = vec![0u8; FRAGMENT as usize];
        while sent < len {
            let want = ((len - sent).min(FRAGMENT)) as usize;
            let mut filled = 0;
            while filled < want {
                let n = from.read(&mut buf[filled..want]).await.map_err(Error::Io)?;
                if n == 0 {
                    return Err(Error::Backend(format!(
                        "OneDrive 업로드: 파일이 {len} 바이트라고 했는데 {} 바이트에서 끝났습니다",
                        sent + filled as u64
                    )));
                }
                filled += n;
            }
            let range = format!("bytes {}-{}/{}", sent, sent + want as u64 - 1, len);
            // No bearer token on the session URL: Graph answers 401 if one
            // is sent.
            let response = self
                .http
                .put(&session.upload_url)
                .header(reqwest::header::CONTENT_LENGTH, want)
                .header(reqwest::header::CONTENT_RANGE, range)
                .body(buf[..want].to_vec())
                .send()
                .await
                .map_err(|e| transport_error(e, "업로드"))?;
            check(response, "업로드").await?;
            sent += want as u64;
        }
        Ok(())
    }

    async fn quota(&self) -> Result<FsStat> {
        let drive: DriveInfo = self
            .json(
                || {
                    self.http
                        .get(format!("{}/me/drive", self.base))
                        .query(&[("$select", "quota")])
                },
                "용량 조회",
            )
            .await?;
        let total = if drive.quota.total == 0 {
            drive.quota.used + (1u64 << 40)
        } else {
            drive.quota.total
        };
        Ok(FsStat {
            total_bytes: total,
            free_bytes: total.saturating_sub(drive.quota.used),
            block_size: 4096,
            total_files: 0,
            free_files: 0,
        })
    }
}

/// A file name as one path segment of a Graph URL.
fn percent_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A OneDrive: the account, or one folder of it.
pub fn backend(id: String, session: Arc<Session>, subpath: Option<String>) -> Arc<dyn Backend> {
    let api = OnedriveApi::new(session, subpath.clone());
    Arc::new(CloudBackend::new(id, api, subpath.as_deref()))
}
