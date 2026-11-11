//! The desktop OAuth flow end to end, with the "browser" played by a request
//! to the loopback redirect and the service by a mock token endpoint.

use std::time::Duration;

use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use mydrives_core::error::Error;
use mydrives_core::oauth::{authorize, Provider, Session, TokenSet};

fn provider(server: &MockServer) -> Provider {
    Provider {
        name: "Test",
        auth_url: format!("{}/auth", server.uri()),
        token_url: format!("{}/token", server.uri()),
        client_id: "client-123".into(),
        client_secret: None,
        scopes: vec!["files.read".into(), "files.write".into()],
        extra_auth_params: vec![("access_type".into(), "offline".into())],
    }
}

/// Plays the browser: reads the sign-in URL the app built, checks it, and
/// "comes back" to the loopback address with a code and the same state.
fn fake_browser(url: &str) -> mydrives_core::error::Result<()> {
    let (_, query) = url.split_once('?').expect("a query string");
    let get = |k: &str| {
        query
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{k}=")))
            .map(str::to_string)
    };
    assert_eq!(get("client_id").as_deref(), Some("client-123"));
    assert_eq!(get("response_type").as_deref(), Some("code"));
    assert_eq!(get("code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(get("access_type").as_deref(), Some("offline"));
    assert_eq!(get("scope").as_deref(), Some("files.read%20files.write"));
    let redirect = get("redirect_uri").unwrap().replace("%3A", ":").replace("%2F", "/");
    let state = get("state").unwrap();
    assert!(get("code_challenge").map_or(false, |c| c.len() >= 43));
    std::thread::spawn(move || {
        // The redirect, as a browser would make it.
        let page = http_get(&format!("{redirect}/?code=auth-code-xyz&state={state}"));
        assert!(page.contains("로그인 완료"), "{page}");
    });
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signing_in_goes_round_the_loop_and_comes_back_with_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=auth-code-xyz"))
        .and(body_string_contains("code_verifier="))
        .and(body_string_contains("client_id=client-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600,
            "scope": "files.read files.write", "token_type": "Bearer"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tokens = authorize(&provider(&server), Duration::from_secs(10), fake_browser)
        .await
        .unwrap();
    assert_eq!(tokens.access_token, "at-1");
    assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
    assert!(!tokens.expires_within(Duration::from_secs(3000)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_in_the_browser_is_an_auth_error_not_a_hang() {
    let server = MockServer::start().await;
    let refuse = |url: &str| -> mydrives_core::error::Result<()> {
        let (_, query) = url.split_once('?').unwrap();
        let redirect = query
            .split('&')
            .find_map(|p| p.strip_prefix("redirect_uri="))
            .unwrap()
            .replace("%3A", ":")
            .replace("%2F", "/");
        std::thread::spawn(move || {
            let _ = http_get(&format!("{redirect}/?error=access_denied"));
        });
        Ok(())
    };
    match authorize(&provider(&server), Duration::from_secs(10), refuse).await {
        Err(Error::AuthFailed(msg)) => assert!(msg.contains("access_denied"), "{msg}"),
        other => panic!("expected refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_session_refreshes_a_token_about_to_expire_and_keeps_the_new_set() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=rt-old"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "at-new", "expires_in": 3600, "token_type": "Bearer"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let stale = TokenSet {
        access_token: "at-old".into(),
        refresh_token: Some("rt-old".into()),
        expires_at: 1, // long gone
        scope: None,
    };
    let kept = std::sync::Arc::new(parking_lot::Mutex::new(None));
    let kept2 = kept.clone();
    let session = Session::new(provider(&server), stale, move |t| {
        *kept2.lock() = Some(t.clone());
    });
    assert_eq!(session.bearer().await.unwrap(), "at-new");
    // The old refresh token survives a refresh answer that omits it, and the
    // new set was handed over for storing.
    let stored = kept.lock().clone().unwrap();
    assert_eq!(stored.access_token, "at-new");
    assert_eq!(stored.refresh_token.as_deref(), Some("rt-old"));
    // A second call within the hour does not refresh again (expect(1)).
    assert_eq!(session.bearer().await.unwrap(), "at-new");
}

#[tokio::test]
async fn a_revoked_refresh_token_says_to_sign_in_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant", "error_description": "Token has been expired or revoked."
        })))
        .mount(&server)
        .await;
    let session = Session::new(
        provider(&server),
        TokenSet {
            access_token: "x".into(),
            refresh_token: Some("rt".into()),
            expires_at: 1,
            scope: None,
        },
        |_| {},
    );
    match session.bearer().await {
        Err(Error::AuthFailed(msg)) => {
            assert!(msg.contains("invalid_grant"), "{msg}");
            assert!(msg.contains("다시 로그인"), "{msg}");
        }
        other => panic!("expected auth failure, got {other:?}"),
    }
}

/// A one-line HTTP/1.1 GET over a plain socket: enough to play a browser
/// following a redirect to 127.0.0.1, without pulling a blocking client in.
fn http_get(url: &str) -> String {
    use std::io::{Read, Write};
    let rest = url.strip_prefix("http://").expect("http url");
    let (host, target) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = std::net::TcpStream::connect(host).expect("connect to the loopback listener");
    write!(stream, "GET /{target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}
