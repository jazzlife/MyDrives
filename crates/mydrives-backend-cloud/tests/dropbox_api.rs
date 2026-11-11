//! The Dropbox client against a server answering like the HTTP API v2.

use std::io::Write;
use std::sync::Arc;

use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use mydrives_backend_cloud::dropbox::DropboxApi;
use mydrives_backend_cloud::CloudApi;
use mydrives_core::oauth::{Provider, Session, TokenSet};
use mydrives_core::types::FileKind;

fn session(server: &MockServer) -> Arc<Session> {
    Session::new(
        Provider {
            name: "Dropbox",
            auth_url: format!("{}/auth", server.uri()),
            token_url: format!("{}/token", server.uri()),
            client_id: "appkey".into(),
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

fn api(server: &MockServer) -> DropboxApi {
    DropboxApi::with_bases(&server.uri(), &server.uri(), session(server), None)
}

#[tokio::test]
async fn a_folder_is_listed_and_the_cursor_followed_and_ids_are_paths() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/2/files/list_folder"))
        .and(header("authorization", "Bearer at"))
        .and(body_partial_json(serde_json::json!({ "path": "", "recursive": false })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [
                {".tag": "folder", "name": "Work", "id": "id:f1", "path_lower": "/work", "path_display": "/Work"}
            ],
            "cursor": "c1", "has_more": true
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/2/files/list_folder/continue"))
        .and(body_partial_json(serde_json::json!({ "cursor": "c1" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "entries": [
                {".tag": "file", "name": "회의록.pdf", "id": "id:a1", "path_lower": "/회의록.pdf",
                 "path_display": "/회의록.pdf", "size": 1048576,
                 "client_modified": "2026-08-11T00:00:00Z", "server_modified": "2026-08-11T00:00:01Z"},
                {".tag": "deleted", "name": "gone.txt", "path_lower": "/gone.txt"}
            ],
            "cursor": "c2", "has_more": false
        })))
        .expect(1)
        .mount(&server)
        .await;

    let items = api(&server).list("").await.unwrap();
    assert_eq!(items.len(), 2, "the deleted entry is not listed");
    assert_eq!(items[0].id, "/Work");
    assert_eq!(items[0].kind, FileKind::Dir);
    assert_eq!(items[1].id, "/회의록.pdf");
    assert_eq!(items[1].size, 1_048_576);
}

#[tokio::test]
async fn a_range_read_puts_the_path_in_the_argument_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/2/files/download"))
        .and(header("range", "bytes=2-4"))
        .respond_with(|req: &Request| {
            let arg = req.headers.get("dropbox-api-arg").unwrap().to_str().unwrap();
            assert!(arg.contains("\"path\":\"/a.txt\""), "{arg}");
            ResponseTemplate::new(206).set_body_bytes(b"234".to_vec())
        })
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(api(&server).download_range("/a.txt", 2, 3).await.unwrap(), b"234");
}

#[tokio::test]
async fn a_small_upload_is_one_call_with_overwrite_and_an_ascii_argument() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/2/files/upload"))
        .respond_with(|req: &Request| {
            let arg = req.headers.get("dropbox-api-arg").unwrap().to_str().unwrap();
            assert!(arg.is_ascii(), "{arg}");
            assert!(arg.contains("\"mode\":\"overwrite\""), "{arg}");
            assert!(arg.contains("\\ud68c"), "the Korean name is escaped: {arg}");
            assert_eq!(req.body, b"hello");
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "name": "회의록.txt" }))
        })
        .expect(1)
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("src");
    std::fs::File::create(&p).unwrap().write_all(b"hello").unwrap();
    let f = tokio::fs::File::open(&p).await.unwrap();
    api(&server).upload("/Work", "회의록.txt", f, 5, None).await.unwrap();
}

#[tokio::test]
async fn folders_are_made_things_are_moved_by_path_and_deleted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/2/files/create_folder_v2"))
        .and(body_partial_json(serde_json::json!({ "path": "/Work/New" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "metadata": {"name": "New", "id": "id:n", "path_display": "/Work/New", "path_lower": "/work/new"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/2/files/move_v2"))
        .and(body_partial_json(serde_json::json!({ "from_path": "/Work/New", "to_path": "/Work/Newer" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "metadata": {} })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/2/files/move_v2"))
        .and(body_partial_json(serde_json::json!({ "from_path": "/Work/Newer", "to_path": "/Archive/Newer" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "metadata": {} })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/2/files/delete_v2"))
        .and(body_partial_json(serde_json::json!({ "path": "/Archive/Newer" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "metadata": {} })))
        .expect(1)
        .mount(&server)
        .await;

    let a = api(&server);
    let made = a.mkdir("/Work", "New").await.unwrap();
    assert_eq!(made.id, "/Work/New");
    assert_eq!(made.kind, FileKind::Dir);
    a.rename("/Work/New", "Newer").await.unwrap();
    a.move_to("/Work/Newer", "/Archive").await.unwrap();
    a.delete("/Archive/Newer").await.unwrap();
}

#[tokio::test]
async fn space_usage_and_a_missing_path_are_read_from_dropbox_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/2/users/get_space_usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "used": 1073741824u64,
            "allocation": { ".tag": "individual", "allocated": 2147483648u64 }
        })))
        .mount(&server)
        .await;
    let st = api(&server).quota().await.unwrap();
    assert_eq!(st.total_bytes, 2_147_483_648);
    assert_eq!(st.free_bytes, 1_073_741_824);

    Mock::given(method("POST"))
        .and(path("/2/files/list_folder"))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error_summary": "path/not_found/..",
            "error": { ".tag": "path", "path": { ".tag": "not_found" } }
        })))
        .mount(&server)
        .await;
    assert!(matches!(
        api(&server).list("/nope").await,
        Err(mydrives_core::error::Error::NotFound(_))
    ));
}
