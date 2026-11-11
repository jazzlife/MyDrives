//! 네이버 MYBOX, through its Open API (developers.mybox.naver.com, read
//! 2026-11-11).
//!
//! Authentication is a **personal access token** the user makes on the MYBOX
//! web site (설정 → 계정 및 개인 액세스 토큰 관리), valid 30 to 180 days, sent
//! as `Authorization: Bearer mbx_pat_…`. There is no OAuth and no refresh:
//! when the token expires every call answers 401, and the only fix is a new
//! token typed into the drive's settings - so that case gets its own message.
//!
//! Files move in two steps. A download is `GET /drive/files/{id}/download`
//! for a one-shot URL (ten minutes), then a plain GET of that URL. An upload
//! is `POST /drive/files` with the name and size for a URL (48 hours, single
//! use), then a `multipart/form-data` POST to it with the bytes in a part
//! called `Filedata`. Neither step takes a byte range, which is why the
//! backend above keeps a local copy of an open file.
//!
//! Limits the service publishes: 50,000 downloads a day, 240 deletes a
//! minute, 30 searches a minute; listings page at 1,000 items.

use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use serde::Deserialize;

use mydrives_core::backend::Backend;
use mydrives_core::error::{Error, Result};
use mydrives_core::types::{FileKind, FsStat};

use crate::{CloudApi, CloudBackend, CloudCaps, Item, ItemId};

const BASE: &str = "https://open-api.mybox.naver.com/v1";
/// The largest page the service allows.
const PAGE: u32 = 1000;

pub struct MyboxApi {
    http: reqwest::Client,
    base: String,
    token: String,
    subpath: Option<String>,
}

impl MyboxApi {
    pub fn new(token: impl Into<String>, subpath: Option<String>) -> MyboxApi {
        MyboxApi::with_base(BASE, token, subpath)
    }

    /// Against another host - a test server.
    pub fn with_base(base: &str, token: impl Into<String>, subpath: Option<String>) -> MyboxApi {
        let http = reqwest::Client::builder()
            .user_agent(concat!("MyDrives/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client");
        MyboxApi {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.into(),
            subpath,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.bearer_auth(&self.token)
    }

    /// Sends a request and turns a non-2xx answer into the error a filesystem
    /// can act on.
    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> Result<reqwest::Response> {
        let response = req.send().await.map_err(|e| transport_error(e, what))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        Err(status_error(status, &body, what))
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<T> {
        self.send(req, what)
            .await?
            .json::<T>()
            .await
            .map_err(|e| Error::Backend(format!("MYBOX {what}: 응답을 읽을 수 없습니다: {e}")))
    }
}

fn transport_error(e: reqwest::Error, what: &str) -> Error {
    if e.is_timeout() || e.is_connect() {
        Error::Offline(format!("MYBOX {what}: 서버에 닿을 수 없습니다: {e}"))
    } else {
        Error::Backend(format!("MYBOX {what}: {e}"))
    }
}

fn status_error(status: reqwest::StatusCode, body: &str, what: &str) -> Error {
    let code = serde_json::from_str::<ApiError>(body)
        .map(|e| e.code)
        .unwrap_or_default();
    let detail = if code.is_empty() {
        String::new()
    } else {
        format!(" ({code})")
    };
    match status.as_u16() {
        401 => Error::AuthFailed(
            "MYBOX 토큰이 만료되었거나 잘못되었습니다 - MYBOX 웹 설정에서 새 개인 액세스 토큰을 만들어 드라이브 편집에 넣으세요".into(),
        ),
        403 => Error::PermissionDenied(format!("MYBOX {what}: 허용되지 않습니다{detail}")),
        404 => Error::NotFound(format!("MYBOX {what}{detail}")),
        409 => Error::AlreadyExists(format!("MYBOX {what}{detail}")),
        423 => Error::SharingViolation(format!("MYBOX {what}: 잠겨 있습니다{detail}")),
        429 => Error::Timeout(format!("MYBOX {what}: 요청 한도를 넘었습니다, 잠시 뒤 다시 시도됩니다{detail}")),
        507 => Error::Backend(format!("MYBOX {what}: 저장 공간이 부족합니다{detail}")),
        500..=599 => Error::Offline(format!("MYBOX {what}: 서비스 오류 {status}{detail}")),
        _ => Error::Backend(format!("MYBOX {what}: {status}{detail}")),
    }
}

#[derive(Deserialize, Default)]
struct ApiError {
    #[serde(default)]
    code: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Resource {
    resource_id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    modified_at: String,
    #[serde(default)]
    is_hidden: bool,
}

impl From<Resource> for Item {
    fn from(r: Resource) -> Item {
        let modified = crate::rfc3339::parse(&r.modified_at).unwrap_or(SystemTime::UNIX_EPOCH);
        Item {
            id: r.resource_id,
            name: r.name,
            kind: if r.kind == "folder" {
                FileKind::Dir
            } else {
                FileKind::File
            },
            size: r.size,
            created: crate::rfc3339::parse(&r.created_at).unwrap_or(modified),
            modified,
            hidden: r.is_hidden,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPage {
    #[serde(default)]
    resources: Vec<Resource>,
    #[serde(default)]
    response_meta_data: Meta,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Meta {
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Storage {
    quota_bytes: u64,
    used_bytes: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadTicket {
    download_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadTicket {
    upload_url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Created {
    resource_id: String,
    name: String,
}

#[async_trait]
impl CloudApi for MyboxApi {
    fn describe(&self) -> String {
        format!("mybox://{}", self.subpath.as_deref().unwrap_or("/"))
    }

    fn caps(&self) -> CloudCaps {
        CloudCaps {
            // The signed download URL is not documented to take a Range.
            range_reads: false,
            case_sensitive: true,
        }
    }

    fn root_id(&self) -> ItemId {
        // The root has no id of its own: it is listed at a different path and
        // is where an upload or a folder goes when `parentId` is left out.
        String::new()
    }

    async fn connect(&self) -> Result<()> {
        self.quota().await.map(|_| ())
    }

    async fn list(&self, folder: &str) -> Result<Vec<Item>> {
        let url = if folder.is_empty() {
            self.url("/drive/resources")
        } else {
            self.url(&format!("/drive/folders/{folder}/resources"))
        };
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut req = self
                .auth(self.http.get(&url))
                .query(&[("count", PAGE.to_string()), ("sort", "name,asc".into())]);
            if let Some(c) = &cursor {
                req = req.query(&[("cursor", c)]);
            }
            let page: ListPage = self.json(req, "목록").await?;
            out.extend(page.resources.into_iter().map(Item::from));
            match page.response_meta_data.next_cursor {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => break,
            }
        }
        Ok(out)
    }

    async fn mkdir(&self, parent: &str, name: &str) -> Result<Item> {
        let mut body = serde_json::json!({ "folderName": name });
        if !parent.is_empty() {
            body["parentId"] = serde_json::Value::String(parent.to_string());
        }
        let created: Created = self
            .json(self.auth(self.http.post(self.url("/drive/folders"))).json(&body), "폴더 생성")
            .await?;
        Ok(Item::folder(created.resource_id, created.name))
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.send(
            self.auth(self.http.delete(self.url(&format!("/drive/resources/{id}")))),
            "삭제",
        )
        .await
        .map(|_| ())
    }

    async fn rename(&self, id: &str, name: &str) -> Result<()> {
        self.send(
            self.auth(self.http.post(self.url(&format!("/drive/resources/{id}/rename"))))
                .json(&serde_json::json!({ "name": name })),
            "이름 변경",
        )
        .await
        .map(|_| ())
    }

    async fn move_to(&self, id: &str, parent: &str) -> Result<()> {
        // The root cannot be named as a destination: `parentId` is required
        // here, and the root has no id. Moving to the top of the account is
        // rare enough to refuse clearly rather than guess.
        if parent.is_empty() {
            return Err(Error::unsupported(
                "MYBOX API는 최상위 폴더로의 이동을 받지 않습니다",
            ));
        }
        self.send(
            self.auth(self.http.post(self.url(&format!("/drive/resources/{id}/move"))))
                .json(&serde_json::json!({ "parentId": parent, "isOverwrite": false })),
            "이동",
        )
        .await
        .map(|_| ())
    }

    async fn download(&self, id: &str, into: &mut tokio::fs::File) -> Result<u64> {
        use tokio::io::AsyncWriteExt;
        let ticket: DownloadTicket = self
            .json(
                self.auth(self.http.get(self.url(&format!("/drive/files/{id}/download")))),
                "다운로드 URL",
            )
            .await?;
        // The URL carries its own credentials; the bearer token must not go
        // to the storage host.
        let mut response = self.send(self.http.get(&ticket.download_url), "다운로드").await?;
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

    async fn upload(
        &self,
        parent: &str,
        name: &str,
        from: tokio::fs::File,
        len: u64,
        _modified: Option<SystemTime>,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "fileName": name,
            "fileSize": len,
            "isOverwrite": true,
        });
        if !parent.is_empty() {
            body["parentId"] = serde_json::Value::String(parent.to_string());
        }
        let ticket: UploadTicket = self
            .json(
                self.auth(self.http.post(self.url("/drive/files"))).json(&body),
                "업로드 URL",
            )
            .await?;
        let stream = tokio_util::io::ReaderStream::new(from);
        let part = reqwest::multipart::Part::stream_with_length(
            reqwest::Body::wrap_stream(stream),
            len,
        )
        .file_name(name.to_string())
        .mime_str("application/octet-stream")
        .map_err(|e| Error::Backend(format!("MYBOX 업로드: {e}")))?;
        let form = reqwest::multipart::Form::new().part("Filedata", part);
        self.send(self.http.post(&ticket.upload_url).multipart(form), "업로드")
            .await
            .map(|_| ())
    }

    async fn quota(&self) -> Result<FsStat> {
        let s: Storage = self
            .json(self.auth(self.http.get(self.url("/drive/storage"))), "용량 조회")
            .await?;
        Ok(FsStat {
            total_bytes: s.quota_bytes,
            free_bytes: s.quota_bytes.saturating_sub(s.used_bytes),
            block_size: 4096,
            total_files: 0,
            free_files: 0,
        })
    }
}

/// A MYBOX drive: the account, or one folder of it, over a personal access
/// token.
pub fn backend(id: String, token: String, subpath: Option<String>) -> Arc<dyn Backend> {
    let api = MyboxApi::new(token, subpath.clone());
    Arc::new(CloudBackend::new(id, api, subpath.as_deref()))
}
