//! The OneDrive client against a server answering like Microsoft Graph.

use std::io::Write;
use std::sync::Arc;

use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use mydrives_backend_cloud::onedrive::OnedriveApi;
use mydrives_backend_cloud::CloudApi;
use mydrives_core::oauth::{Provider, Session, TokenSet};
use mydrives_core::types::FileKind;

fn session(server: &MockServer) -> Arc<Session> {
    Session::new(
        Provider {
            name: "Microsoft",
            auth_url: format!("{}/auth", server.uri()),
            token_url: format!("{}/token", server.uri()),
            client_id: "cid".into(),
            client_secret: None,
            scopes: vec![],
            extra_auth_params: vec![],
            loopback_host: "localhost",
            preferred_port: None,
        },
        TokenSet {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: u64::MAX / 2,
            scope: None,
        },
        |_| {},
    )
}

fn api(server: &MockServer) -> OnedriveApi {
    OnedriveApi::with_base(&server.uri(), session(server), None)
}

#[tokio::test]
async fn the_root_and_a_folder_are_listed_as_children_with_paging() {
    let server = MockServer::start().await;
    let next = format!("{}/me/drive/root/children?$skiptoken=abc", server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/root/children"))
        .and(header("authorization", "Bearer at"))
        .respond_with(move |req: &Request| {
            let skip = req.url.query_pairs().any(|(k, _)| k == "$skiptoken");
            if !skip {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "value": [
                        {"id": "F1", "name": "Docs", "folder": {"childCount": 2},
                         "lastModifiedDateTime": "2026-01-02T03:04:05Z", "createdDateTime": "2026-01-01T00:00:00Z"}
                    ],
                    "@odata.nextLink": next
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "value": [
                        {"id": "A1", "name": "photo.jpg", "size": 2048, "file": {"mimeType": "image/jpeg"},
                         "lastModifiedDateTime": "2026-01-02T03:04:05Z", "createdDateTime": "2026-01-01T00:00:00Z"}
                    ]
                }))
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/items/F1/children"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "value": [] })))
        .expect(1)
        .mount(&server)
        .await;

    let items = api(&server).list("root").await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].kind, FileKind::Dir);
    assert_eq!(items[1].name, "photo.jpg");
    assert_eq!(items[1].size, 2048);
    assert!(api(&server).list("F1").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_range_read_goes_to_the_download_url_without_the_token() {
    let server = MockServer::start().await;
    let dl = format!("{}/dl/A1", server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/items/A1"))
        // No $select: naming the download URL there makes Graph omit it.
        .and(move |req: &Request| !req.url.query_pairs().any(|(k, _)| k == "$select"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "A1", "@microsoft.graph.downloadUrl": dl
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dl/A1"))
        .and(header("range", "bytes=5-9"))
        .respond_with(|req: &Request| {
            assert!(req.headers.get("authorization").is_none());
            ResponseTemplate::new(206).set_body_bytes(b"56789".to_vec())
        })
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(api(&server).download_range("A1", 5, 5).await.unwrap(), b"56789");
}

#[tokio::test]
async fn a_small_file_is_one_put_and_a_folder_is_made_moved_renamed_and_deleted() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/me/drive/items/F1:/new.txt:/content"))
        .respond_with(|req: &Request| {
            assert_eq!(req.body, b"hello");
            ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": "N1", "name": "new.txt" }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/me/drive/items/F1/children"))
        .and(body_partial_json(serde_json::json!({ "name": "Sub", "folder": {} })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "F2", "name": "Sub", "folder": {}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "F2" })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/F2"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("src");
    std::fs::File::create(&p).unwrap().write_all(b"hello").unwrap();
    let f = tokio::fs::File::open(&p).await.unwrap();
    let a = api(&server);
    a.upload("F1", "new.txt", f, 5, None).await.unwrap();
    let made = a.mkdir("F1", "Sub").await.unwrap();
    assert_eq!(made.id, "F2");
    assert_eq!(made.kind, FileKind::Dir);
    a.rename("F2", "Sub2").await.unwrap();
    a.move_to("F2", "F9").await.unwrap();
    a.delete("F2").await.unwrap();
}

#[tokio::test]
async fn quota_comes_from_the_drive_and_a_401_says_to_sign_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "quota": { "total": 1099511627776u64, "used": 1073741824u64, "remaining": 1098437885952u64 }
        })))
        .mount(&server)
        .await;
    let st = api(&server).quota().await.unwrap();
    assert_eq!(st.total_bytes, 1_099_511_627_776);
    assert_eq!(st.free_bytes, 1_099_511_627_776 - 1_073_741_824);

    let expired = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant", "error_description": "AADSTS70000: expired"
        })))
        .mount(&expired)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": { "code": "InvalidAuthenticationToken", "message": "Access token has expired." }
        })))
        .mount(&expired)
        .await;
    match api(&expired).connect().await {
        Err(mydrives_core::error::Error::AuthFailed(msg)) => assert!(msg.contains("로그인"), "{msg}"),
        other => panic!("expected an auth failure, got {other:?}"),
    }
}
