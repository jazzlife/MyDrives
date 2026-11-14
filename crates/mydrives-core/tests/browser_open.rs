//! The sign-in URL has to reach the browser whole.
//!
//! It is easy to lose half of it on the way: Windows' `cmd` ends a command at
//! `&`, so a URL handed to `cmd /C start` without quotes arrives truncated at
//! its first parameter and the service answers "the request body must contain
//! the following parameter: 'scope'". Only a real browser launch shows that,
//! which is why this test opens one - and why it is ignored by default:
//!
//!     cargo test -p mydrives-core --test browser_open -- --ignored --nocapture

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

#[test]
#[ignore = "opens a browser window"]
fn the_browser_asks_for_every_parameter() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    // The shape of a real authorize URL: several `&`, an encoded redirect and
    // a `scope` that is last in line and so the first thing to go missing.
    let path = "/authorize?client_id=abc-123\
                &redirect_uri=http%3A%2F%2Flocalhost%3A1234%2F\
                &response_type=code\
                &scope=Files.ReadWrite.All%20offline_access\
                &state=xyz";
    mydrives_core::oauth::open_in_browser(&format!("http://127.0.0.1:{port}{path}"))
        .expect("open a browser");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let Ok((stream, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let mut stream = stream;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut line = String::new();
        let _ = BufReader::new(stream.try_clone().expect("clone")).read_line(&mut line);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        let line = line.trim().to_string();
        println!("request: {line}");
        // A browser asks for other things too (favicon, a probe connection
        // with no request at all); the authorize request is the one to judge.
        if line.starts_with("GET /authorize") {
            assert_eq!(
                line,
                format!("GET {path} HTTP/1.1"),
                "the browser was handed a truncated URL"
            );
            return;
        }
        seen.push(line);
    }
    panic!("no request for the URL arrived within 60s; saw: {seen:?}");
}
