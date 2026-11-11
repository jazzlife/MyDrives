//! The MYBOX client against a server that answers like the documented API:
//! the paths, the bearer header, paging, and the two-step upload and
//! download are checked here, so a real token is only needed to confirm
//! the documentation, not to find out whether the client speaks it.

use std::io::Write;
use std::time::SystemTime;

use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use mydrives_backend_cloud::mybox::MyboxApi;
use mydrives_backend_cloud::CloudApi;
use mydrives_core::error::Error;
use mydrives_core::types::FileKind;

const TOKEN: &str = "mbx_pat_test";

fn api(server: &MockServer) -> MyboxApi {
    MyboxApi::with_base(&server.uri(), TOKEN, None)
}

#[tokio::test]
async fn the_root_is_listed_at_its_own_path_and_pages_are_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/resources"))
        .and(header("authorization", &format!("Bearer {TOKEN}")))
        .and(query_param("count", "1000"))
        .respond_with(move |req: &Request| {
            let cursor = req.url.query_pairs().find(|(k, _)| k == "cursor").map(|(_, v)| v.to_string());
            match cursor.as_deref() {
                None => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "fileCount": 1, "subFolderCount": 1,
                    "resources": [{
                        "resourceId": "f1", "parentId": "", "name": "Docs", "type": "folder",
                        "size": 0, "createdAt": "2026-08-11T09:00:00+09:00",
                        "modifiedAt": "2026-08-12T09:00:00+09:00", "accessedAt": "2026-08-12T09:00:00+09:00",
                        "isFavorite": false, "isHidden": false, "lastModifiedBy": "me"
                    }],
                    "responseMetaData": { "nextCursor": "MjA" }
                })),
                Some("MjA") => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "fileCount": 1, "subFolderCount": 1,
                    "resources": [{
                        "resourceId": "a1", "parentId": "", "name": "회의록.pdf", "type": "file",
                        "size": 1048576, "createdAt": "2026-08-11T09:00:00+09:00",
                        "modifiedAt": "2026-08-11T09:00:00+09:00", "accessedAt": "2026-08-11T09:00:00+09:00",
                        "isFavorite": false, "isHidden": true, "lastModifiedBy": "me"
                    }],
                    "responseMetaData": {}
                })),
                other => panic!("unexpected cursor {other:?}"),
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    let items = api(&server).list("").await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].name, "Docs");
    assert_eq!(items[0].kind, FileKind::Dir);
    assert_eq!(items[1].name, "회의록.pdf");
    assert_eq!(items[1].size, 1_048_576);
    assert!(items[1].hidden);
    // 2026-08-11 09:00 KST
    assert_eq!(
        items[1]
            .modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        1_786_406_400
    );
}

#[tokio::test]
async fn a_folder_is_listed_by_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/folders/f1/resources"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "fileCount": 0, "subFolderCount": 0, "resources": [], "responseMetaData": {}
        })))
        .expect(1)
        .mount(&server)
        .await;
    assert!(api(&server).list("f1").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_download_is_a_ticket_then_a_plain_get_without_the_token() {
    let server = MockServer::start().await;
    let file_url = format!("{}/storage/download?auth=3&atoken=xyz", server.uri());
    Mock::given(method("GET"))
        .and(path("/drive/files/a1/download"))
        .and(header("authorization", &format!("Bearer {TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "downloadUrl": file_url, "expiresIn": 600
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storage/download"))
        .respond_with(|req: &Request| {
            // The signed URL is the credential; the token must not leak to it.
            assert!(req.headers.get("authorization").is_none());
            ResponseTemplate::new(200).set_body_bytes(b"file bytes".to_vec())
        })
        .expect(1)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("got");
    let mut f = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&p)
        .await
        .unwrap();
    let n = api(&server).download("a1", &mut f).await.unwrap();
    assert_eq!(n, 10);
    assert_eq!(std::fs::read(&p).unwrap(), b"file bytes");
}

#[tokio::test]
async fn an_upload_is_a_ticket_then_a_multipart_post_of_filedata() {
    let server = MockServer::start().await;
    let up_url = format!("{}/storage/upload?stoken=abc", server.uri());
    Mock::given(method("POST"))
        .and(path("/drive/files"))
        .and(header("authorization", &format!("Bearer {TOKEN}")))
        .and(body_partial_json(serde_json::json!({
            "fileName": "new.txt", "fileSize": 5, "isOverwrite": true, "parentId": "f1"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "offset": 0, "uploadUrl": up_url
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/storage/upload"))
        .respond_with(|req: &Request| {
            let ct = req.headers.get("content-type").unwrap().to_str().unwrap();
            assert!(ct.starts_with("multipart/form-data"), "{ct}");
            let body = String::from_utf8_lossy(&req.body);
            assert!(body.contains("name=\"Filedata\""), "{body}");
            assert!(body.contains("filename=\"new.txt\""), "{body}");
            assert!(body.contains("hello"), "{body}");
            ResponseTemplate::new(200)
        })
        .expect(1)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("src");
    std::fs::File::create(&p).unwrap().write_all(b"hello").unwrap();
    let f = tokio::fs::File::open(&p).await.unwrap();
    api(&server)
        .upload("f1", "new.txt", f, 5, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn folders_are_made_and_things_are_renamed_moved_and_deleted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/drive/folders"))
        .and(body_partial_json(serde_json::json!({ "folderName": "New", "parentId": "f1" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "name": "New", "resourceId": "f9"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/drive/resources/f9/rename"))
        .and(body_partial_json(serde_json::json!({ "name": "Newer" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "name": "Newer" })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/drive/resources/f9/move"))
        .and(body_partial_json(serde_json::json!({ "parentId": "f2" })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/drive/resources/f9"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let a = api(&server);
    let made = a.mkdir("f1", "New").await.unwrap();
    assert_eq!(made.id, "f9");
    a.rename("f9", "Newer").await.unwrap();
    a.move_to("f9", "f2").await.unwrap();
    a.delete("f9").await.unwrap();
}

#[tokio::test]
async fn quota_becomes_free_space_and_an_expired_token_says_so() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/storage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "quotaBytes": 32212254720u64, "usedBytes": 5368709120u64, "maxFileBytes": 1,
            "trashAutoDeleteDays": 5, "fileCounts": {}
        })))
        .mount(&server)
        .await;
    let st = api(&server).quota().await.unwrap();
    assert_eq!(st.total_bytes, 32_212_254_720);
    assert_eq!(st.free_bytes, 32_212_254_720 - 5_368_709_120);

    let expired = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/drive/storage"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "code": "PLAT-401", "message": "UNAUTHORIZED"
        })))
        .mount(&expired)
        .await;
    match api(&expired).connect().await {
        Err(Error::AuthFailed(msg)) => assert!(msg.contains("토큰"), "{msg}"),
        other => panic!("expected an auth failure, got {other:?}"),
    }
}
