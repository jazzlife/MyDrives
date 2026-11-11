//! The Google Drive client against a server answering like Drive API v3.

use std::io::Write;
use std::sync::Arc;

use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use mydrives_backend_cloud::gdrive::GdriveApi;
use mydrives_backend_cloud::CloudApi;
use mydrives_core::oauth::{Provider, Session, TokenSet};
use mydrives_core::types::FileKind;

fn session(server: &MockServer, access: &str) -> Arc<Session> {
    Session::new(
        Provider {
            name: "Google",
            auth_url: format!("{}/auth", server.uri()),
            token_url: format!("{}/token", server.uri()),
            client_id: "cid".into(),
            client_secret: None,
            scopes: vec![],
            extra_auth_params: vec![],
        },
        TokenSet {
            access_token: access.into(),
            refresh_token: Some("rt".into()),
            expires_at: u64::MAX / 2,
            scope: None,
        },
        |_| {},
    )
}

fn api(server: &MockServer) -> GdriveApi {
    GdriveApi::with_base(&server.uri(), session(server, "at"), None)
}

#[tokio::test]
async fn a_folder_is_listed_with_a_parents_query_and_docs_are_left_out() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(header("authorization", "Bearer at"))
        .and(query_param("q", "'f1' in parents and trashed = false"))
        .and(query_param("pageSize", "1000"))
        .respond_with(|req: &Request| {
            let token = req.url.query_pairs().find(|(k, _)| k == "pageToken").map(|(_, v)| v.to_string());
            match token.as_deref() {
                None => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "nextPageToken": "p2",
                    "files": [
                        {"id": "d1", "name": "Sub", "mimeType": "application/vnd.google-apps.folder",
                         "modifiedTime": "2026-01-02T03:04:05.000Z", "createdTime": "2026-01-01T00:00:00Z"},
                        {"id": "g1", "name": "Notes", "mimeType": "application/vnd.google-apps.document",
                         "modifiedTime": "2026-01-02T03:04:05.000Z"}
                    ]
                })),
                Some("p2") => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "files": [
                        {"id": "a1", "name": "photo.jpg", "mimeType": "image/jpeg", "size": "2048",
                         "modifiedTime": "2026-01-02T03:04:05Z", "createdTime": "2026-01-01T00:00:00Z"}
                    ]
                })),
                other => panic!("unexpected page token {other:?}"),
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let items = api(&server).list("f1").await.unwrap();
    assert_eq!(items.len(), 2, "the Google Doc is not a file");
    assert_eq!(items[0].name, "Sub");
    assert_eq!(items[0].kind, FileKind::Dir);
    assert_eq!(items[1].name, "photo.jpg");
    assert_eq!(items[1].size, 2048);
}

#[tokio::test]
async fn a_range_read_asks_for_the_bytes_and_takes_what_comes_back() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/a1"))
        .and(query_param("alt", "media"))
        .and(header("range", "bytes=10-19"))
        .respond_with(ResponseTemplate::new(206).set_body_bytes(b"0123456789".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let got = api(&server).download_range("a1", 10, 10).await.unwrap();
    assert_eq!(got, b"0123456789");
}

#[tokio::test]
async fn an_upload_is_a_resumable_session_then_one_put() {
    let server = MockServer::start().await;
    // No file of that name yet.
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("pageSize", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "files": [] })))
        .expect(1)
        .mount(&server)
        .await;
    let put_url = format!("{}/upload/session/abc", server.uri());
    Mock::given(method("POST"))
        .and(path("/upload/drive/v3/files"))
        .and(query_param("uploadType", "resumable"))
        .and(header("x-upload-content-length", "5"))
        .and(body_partial_json(serde_json::json!({ "name": "new.txt", "parents": ["f1"] })))
        .respond_with(ResponseTemplate::new(200).insert_header("Location", put_url.as_str()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload/session/abc"))
        .and(header("content-length", "5"))
        .respond_with(|req: &Request| {
            assert_eq!(req.body, b"hello");
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "n1" }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("src");
    std::fs::File::create(&p).unwrap().write_all(b"hello").unwrap();
    let f = tokio::fs::File::open(&p).await.unwrap();
    api(&server).upload("f1", "new.txt", f, 5, None).await.unwrap();
}

#[tokio::test]
async fn an_existing_file_is_replaced_in_place_with_patch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files"))
        .and(query_param("pageSize", "10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "files": [{"id": "old1", "name": "same.txt", "mimeType": "text/plain", "size": "1"}]
        })))
        .mount(&server)
        .await;
    let put_url = format!("{}/upload/session/x", server.uri());
    Mock::given(method("PATCH"))
        .and(path("/upload/drive/v3/files/old1"))
        .and(query_param("uploadType", "resumable"))
        .respond_with(ResponseTemplate::new(200).insert_header("Location", put_url.as_str()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/upload/session/x"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "old1" })))
        .expect(1)
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("src");
    std::fs::File::create(&p).unwrap().write_all(b"x").unwrap();
    let f = tokio::fs::File::open(&p).await.unwrap();
    api(&server).upload("f1", "same.txt", f, 1, None).await.unwrap();
}

#[tokio::test]
async fn moving_swaps_parents_and_deleting_puts_in_the_bin() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/files/a1"))
        .and(query_param("fields", "parents"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "parents": ["f1"] })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/drive/v3/files/a1"))
        .and(query_param("addParents", "f2"))
        .and(query_param("removeParents", "f1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "a1" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/drive/v3/files/b1"))
        .and(body_partial_json(serde_json::json!({ "trashed": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "b1" })))
        .expect(1)
        .mount(&server)
        .await;
    let a = api(&server);
    a.move_to("a1", "f2").await.unwrap();
    a.delete("b1").await.unwrap();
}

#[tokio::test]
async fn a_401_is_retried_once_with_a_fresh_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "at-2", "expires_in": 3600
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/about"))
        .and(header("authorization", "Bearer at"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": { "message": "Invalid Credentials", "errors": [{ "reason": "authError" }] }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/drive/v3/about"))
        .and(header("authorization", "Bearer at-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "storageQuota": { "limit": "16106127360", "usage": "1073741824" }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let st = api(&server).quota().await.unwrap();
    assert_eq!(st.total_bytes, 16_106_127_360);
    assert_eq!(st.free_bytes, 16_106_127_360 - 1_073_741_824);
}
