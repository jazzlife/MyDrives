//! Google Drive, through the Drive API v3 (developers.google.com/workspace/
//! drive/api, read 2026-11-11).
//!
//! Signed in with OAuth as a desktop app (see `mydrives_core::oauth`); the
//! user brings the OAuth client id from their own Google Cloud project,
//! because one app's id cannot be shipped inside an open-source binary
//! without inviting everyone to pose as it.
//!
//! Items are listed by a query against their parent (`'<id>' in parents and
//! trashed = false`), pages of 1,000. Downloads are `files.get?alt=media`,
//! which honours `Range`, so reads need no local copy. Uploads use the
//! resumable protocol in one shot: a POST (new file) or PATCH (existing)
//! that returns a session URI, then one PUT of the bytes. Deleting puts a
//! file in the bin (`trashed = true`), as the web site does.
//!
//! Google's own document types (Docs, Sheets, Slides) have no bytes to hand
//! out - they can only be exported - so they are left out of listings.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use serde::Deserialize;

use mydrives_core::backend::Backend;
use mydrives_core::error::{Error, Result};
use mydrives_core::oauth::{Provider, Session};
use mydrives_core::types::{FileKind, FsStat};

use crate::{CloudApi, CloudBackend, CloudCaps, Item, ItemId};

const API: &str = "https://www.googleapis.com";
const FOLDER: &str = "application/vnd.google-apps.folder";
const FIELDS: &str = "id,name,mimeType,size,modifiedTime,createdTime";

/// The sign-in parameters for Google, given the user's OAuth client.
pub fn provider(client_id: &str, client_secret: Option<&str>) -> Provider {
    mydrives_core::oauth::google_provider(client_id, client_secret)
}

pub struct GdriveApi {
    http: reqwest::Client,
    base: String,
    session: Arc<Session>,
    subpath: Option<String>,
}

impl GdriveApi {
    pub fn new(session: Arc<Session>, subpath: Option<String>) -> GdriveApi {
        GdriveApi::with_base(API, session, subpath)
    }

    pub fn with_base(base: &str, session: Arc<Session>, subpath: Option<String>) -> GdriveApi {
        let http = reqwest::Client::builder()
            .user_agent(concat!("MyDrives/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        GdriveApi {
            http,
            base: base.trim_end_matches('/').to_string(),
            session,
            subpath,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Sends `build()` with a bearer token, once more with a fresh token if
    /// the first answer was 401.
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
            .map_err(|e| Error::Backend(format!("Google Drive {what}: 응답을 읽을 수 없습니다: {e}")))
    }

    /// The file called `name` directly inside `parent`, if any.
    async fn find(&self, parent: &str, name: &str) -> Result<Option<Item>> {
        let q = format!(
            "'{}' in parents and name = '{}' and trashed = false",
            parent,
            name.replace('\\', "\\\\").replace('\'', "\\'")
        );
        let page: ListPage = self
            .json(
                || {
                    self.http.get(self.url("/drive/v3/files")).query(&[
                        ("q", q.as_str()),
                        ("fields", &format!("files({FIELDS})")),
                        ("pageSize", "10"),
                    ])
                },
                "찾기",
            )
            .await?;
        Ok(page
            .files
            .into_iter()
            .filter_map(Item::from_file)
            .find(|i| i.name == name))
    }

    async fn parents_of(&self, id: &str) -> Result<Vec<String>> {
        let f: Parents = self
            .json(
                || {
                    self.http
                        .get(self.url(&format!("/drive/v3/files/{id}")))
                        .query(&[("fields", "parents")])
                },
                "상위 폴더",
            )
            .await?;
        Ok(f.parents)
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
        Error::Offline(format!("Google Drive {what}: 서버에 닿을 수 없습니다: {e}"))
    } else {
        Error::Backend(format!("Google Drive {what}: {e}"))
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
    message: String,
    #[serde(default)]
    errors: Vec<ApiErrorItem>,
}

#[derive(Deserialize, Default)]
struct ApiErrorItem {
    #[serde(default)]
    reason: String,
}

fn status_error(status: reqwest::StatusCode, body: &str, what: &str) -> Error {
    let err: ApiError = serde_json::from_str(body).unwrap_or_default();
    let reason = err
        .error
        .errors
        .first()
        .map(|e| e.reason.clone())
        .unwrap_or_default();
    let message = if err.error.message.is_empty() {
        status.to_string()
    } else {
        err.error.message
    };
    match status.as_u16() {
        401 => Error::AuthFailed(
            "Google 로그인이 더 이상 유효하지 않습니다 - 드라이브 편집에서 다시 로그인하세요".into(),
        ),
        403 if reason.contains("RateLimit") || reason == "rateLimitExceeded" => {
            Error::Timeout(format!("Google Drive {what}: 요청 한도를 넘었습니다, 잠시 뒤 다시 시도됩니다"))
        }
        403 if reason == "storageQuotaExceeded" => {
            Error::Backend(format!("Google Drive {what}: 저장 공간이 부족합니다"))
        }
        403 => Error::PermissionDenied(format!("Google Drive {what}: {message}")),
        404 => Error::NotFound(format!("Google Drive {what}: {message}")),
        429 => Error::Timeout(format!("Google Drive {what}: 요청 한도를 넘었습니다, 잠시 뒤 다시 시도됩니다")),
        500..=599 => Error::Offline(format!("Google Drive {what}: 서비스 오류 {status}")),
        _ => Error::Backend(format!("Google Drive {what}: {message}")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    id: String,
    name: String,
    #[serde(default)]
    mime_type: String,
    /// Google sends int64 as a string.
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    modified_time: String,
    #[serde(default)]
    created_time: String,
}

impl Item {
    fn from_file(f: File) -> Option<Item> {
        let kind = if f.mime_type == FOLDER {
            FileKind::Dir
        } else if f.mime_type.starts_with("application/vnd.google-apps.") {
            // A Docs/Sheets/Slides document: no bytes to serve.
            return None;
        } else {
            FileKind::File
        };
        let modified = crate::rfc3339::parse(&f.modified_time).unwrap_or(SystemTime::UNIX_EPOCH);
        Some(Item {
            id: f.id,
            name: f.name,
            kind,
            size: f.size.and_then(|s| s.parse().ok()).unwrap_or(0),
            created: crate::rfc3339::parse(&f.created_time).unwrap_or(modified),
            modified,
            hidden: false,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPage {
    #[serde(default)]
    files: Vec<File>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct Parents {
    #[serde(default)]
    parents: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct About {
    #[serde(default)]
    storage_quota: Quota,
}

#[derive(Deserialize, Default)]
struct Quota {
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    usage: Option<String>,
}

#[async_trait]
impl CloudApi for GdriveApi {
    fn describe(&self) -> String {
        format!("gdrive://{}", self.subpath.as_deref().unwrap_or("/"))
    }

    fn caps(&self) -> CloudCaps {
        CloudCaps {
            range_reads: true,
            case_sensitive: true,
        }
    }

    fn root_id(&self) -> ItemId {
        "root".into()
    }

    async fn connect(&self) -> Result<()> {
        self.quota().await.map(|_| ())
    }

    async fn list(&self, folder: &str) -> Result<Vec<Item>> {
        let q = format!("'{folder}' in parents and trashed = false");
        let fields = format!("nextPageToken,files({FIELDS})");
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page: ListPage = self
                .json(
                    || {
                        let mut req = self.http.get(self.url("/drive/v3/files")).query(&[
                            ("q", q.as_str()),
                            ("fields", fields.as_str()),
                            ("pageSize", "1000"),
                            ("orderBy", "name"),
                        ]);
                        if let Some(t) = &token {
                            req = req.query(&[("pageToken", t)]);
                        }
                        req
                    },
                    "목록",
                )
                .await?;
            out.extend(page.files.into_iter().filter_map(Item::from_file));
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }

    async fn mkdir(&self, parent: &str, name: &str) -> Result<Item> {
        let body = serde_json::json!({
            "name": name,
            "mimeType": FOLDER,
            "parents": [parent],
        });
        let file: File = self
            .json(
                || {
                    self.http
                        .post(self.url("/drive/v3/files"))
                        .query(&[("fields", FIELDS)])
                        .json(&body)
                },
                "폴더 생성",
            )
            .await?;
        Item::from_file(file).ok_or_else(|| Error::Backend("폴더 생성 응답이 폴더가 아닙니다".into()))
    }

    async fn delete(&self, id: &str) -> Result<()> {
        // Into the bin, not gone: a mistaken delete on a mounted drive is
        // recoverable the same way it is on the web site.
        let body = serde_json::json!({ "trashed": true });
        self.send(
            || self.http.patch(self.url(&format!("/drive/v3/files/{id}"))).json(&body),
            "삭제",
        )
        .await
        .map(|_| ())
    }

    async fn rename(&self, id: &str, name: &str) -> Result<()> {
        let body = serde_json::json!({ "name": name });
        self.send(
            || self.http.patch(self.url(&format!("/drive/v3/files/{id}"))).json(&body),
            "이름 변경",
        )
        .await
        .map(|_| ())
    }

    async fn move_to(&self, id: &str, parent: &str) -> Result<()> {
        let remove = self.parents_of(id).await?.join(",");
        self.send(
            || {
                self.http
                    .patch(self.url(&format!("/drive/v3/files/{id}")))
                    .query(&[("addParents", parent), ("removeParents", remove.as_str())])
                    .json(&serde_json::json!({}))
            },
            "이동",
        )
        .await
        .map(|_| ())
    }

    async fn download(&self, id: &str, into: &mut tokio::fs::File) -> Result<u64> {
        use tokio::io::AsyncWriteExt;
        let mut response = self
            .send(
                || {
                    self.http
                        .get(self.url(&format!("/drive/v3/files/{id}")))
                        .query(&[("alt", "media")])
                },
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
        let range = format!("bytes={}-{}", offset, offset + len as u64 - 1);
        let response = self
            .send(
                || {
                    self.http
                        .get(self.url(&format!("/drive/v3/files/{id}")))
                        .query(&[("alt", "media")])
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
            // The server ignored the range and sent everything.
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
        from: tokio::fs::File,
        len: u64,
        _modified: Option<SystemTime>,
    ) -> Result<()> {
        // One resumable session, one PUT. New file or new content for an
        // existing one: the only difference is the first request.
        let existing = self.find(parent, name).await?;
        let session = match &existing {
            Some(item) => {
                let url = self.url(&format!("/upload/drive/v3/files/{}", item.id));
                self.send(
                    || {
                        self.http
                            .patch(&url)
                            .query(&[("uploadType", "resumable")])
                            .header("X-Upload-Content-Length", len.to_string())
                            .json(&serde_json::json!({}))
                    },
                    "업로드 시작",
                )
                .await?
            }
            None => {
                let url = self.url("/upload/drive/v3/files");
                let body = serde_json::json!({ "name": name, "parents": [parent] });
                self.send(
                    || {
                        self.http
                            .post(&url)
                            .query(&[("uploadType", "resumable")])
                            .header("X-Upload-Content-Length", len.to_string())
                            .json(&body)
                    },
                    "업로드 시작",
                )
                .await?
            }
        };
        let location = session
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| Error::Backend("Google Drive 업로드 시작: 세션 주소가 없습니다".into()))?;
        let stream = tokio_util::io::ReaderStream::new(from);
        let response = self
            .http
            .put(&location)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|e| transport_error(e, "업로드"))?;
        check(response, "업로드").await.map(|_| ())
    }

    async fn quota(&self) -> Result<FsStat> {
        let about: About = self
            .json(
                || {
                    self.http
                        .get(self.url("/drive/v3/about"))
                        .query(&[("fields", "storageQuota")])
                },
                "용량 조회",
            )
            .await?;
        let usage: u64 = about.storage_quota.usage.and_then(|s| s.parse().ok()).unwrap_or(0);
        // No limit means unlimited storage (a Workspace plan): report plenty
        // of room rather than a fake number near zero.
        let total: u64 = about
            .storage_quota
            .limit
            .and_then(|s| s.parse().ok())
            .unwrap_or(usage + (1u64 << 40));
        Ok(FsStat {
            total_bytes: total,
            free_bytes: total.saturating_sub(usage),
            block_size: 4096,
            total_files: 0,
            free_files: 0,
        })
    }
}

/// A Google Drive: the account, or one folder of it.
pub fn backend(id: String, session: Arc<Session>, subpath: Option<String>) -> Arc<dyn Backend> {
    let api = GdriveApi::new(session, subpath.clone());
    Arc::new(CloudBackend::new(id, api, subpath.as_deref()))
}
