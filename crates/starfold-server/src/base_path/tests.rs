use super::*;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::Uri;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "pw";

/// Routes standing in for the server's, each answering the path and query it was asked for.
fn routes() -> Router {
    let echo = |uri: Uri| async move {
        format!("path {}", uri.path_and_query().map_or("", |p| p.as_str()))
    };
    Router::new()
        .route("/", get(echo))
        .route("/healthz", get(echo))
        .route("/client.js", get(echo))
        .route("/stats", get(echo))
        .route(
            "/ws",
            get(|ws: WebSocketUpgrade| async move { ws.on_upgrade(|_| async {}) }),
        )
}

/// What `serve` serves: the routes with the login on and the host check, under `base`.
fn app(base: Option<&str>) -> Router {
    let login = Arc::new(crate::login::Login::new(None, PASSWORD.into()));
    let access = Arc::new(crate::access::Access::new(None, None));
    let base = base.map(|p| BasePath::parse(p).expect("a valid prefix"));
    compose(routes(), base.as_ref(), Some(login), access)
}

fn prefix(p: &str) -> BasePath {
    BasePath::parse(p).expect("a valid prefix")
}

/// Serves `router` on a loopback port and returns the port.
async fn serve(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    port
}

/// A request's extra headers.
#[derive(Clone, Copy, Debug)]
enum Asking {
    /// The password, from this machine.
    Signed,
    /// No password.
    Anonymous,
    /// The password, for a host name starfold doesn't answer to.
    OtherHost,
    /// The password, from another site's page.
    OtherSite,
}

const ASKING: [Asking; 4] = [
    Asking::Signed,
    Asking::Anonymous,
    Asking::OtherHost,
    Asking::OtherSite,
];

/// The status and the whole response to a GET of `target`.
async fn get_from(port: u16, target: &str, asking: Asking, extra: &str) -> (u16, String) {
    ask(port, "GET", target, asking, extra).await
}

/// The status and the whole response to a `method` request for `target`.
async fn ask(port: u16, method: &str, target: &str, asking: Asking, extra: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let own = format!("127.0.0.1:{port}");
    let (host, more) = match asking {
        Asking::Signed => (
            own.as_str(),
            format!("Authorization: Bearer {PASSWORD}\r\n"),
        ),
        Asking::Anonymous => (own.as_str(), String::new()),
        Asking::OtherHost => (
            "starfold.example",
            format!("Authorization: Bearer {PASSWORD}\r\n"),
        ),
        Asking::OtherSite => (
            own.as_str(),
            format!("Authorization: Bearer {PASSWORD}\r\nOrigin: https://other.example\r\n"),
        ),
    };
    let close = match extra.contains("Connection:") {
        true => "",
        false => "Connection: close\r\n",
    };
    let req = format!(
        "{method} {target} HTTP/1.1\r\nHost: {host}\r\n{more}{close}Content-Length: 0\r\n{extra}\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(2), s.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[test]
fn every_prefix_shape_has_one_normal_form_or_is_refused() {
    for (given, normal) in [
        ("/desk", "/desk"),
        ("/desk/", "/desk"),
        ("/a/b-c_d.e~f/9", "/a/b-c_d.e~f/9"),
        ("/a/b/", "/a/b"),
    ] {
        assert_eq!(
            BasePath::parse(given).map(|b| b.as_str().to_string()),
            Ok(normal.to_string()),
            "{given:?}"
        );
    }
    let bytes_1024 = format!("/{}", "a".repeat(1023));
    assert_eq!(bytes_1024.len(), 1024);
    assert_eq!(prefix(&bytes_1024).as_str(), bytes_1024);
    assert_eq!(prefix(&format!("{bytes_1024}/")).as_str(), bytes_1024);
    assert!(
        BasePath::parse(&format!("{bytes_1024}a")).is_err(),
        "1025 bytes"
    );
    assert!(BasePath::parse(&format!("/{}", "a".repeat(65_533))).is_err());
    for refused in [
        "", "/", "desk", "desk/", "//", "///", "/desk//", "//desk", "/a//b", "/a/b//", "/%2F",
        "/a%2Fb", "/%2e", "/.", "/..", "/a/./b", "/a/../b", "/a/.", "/a b", "/a?b", "/a#b", "/ä",
        "/a\\b", "-h", "--help",
    ] {
        assert!(
            BasePath::parse(refused).is_err(),
            "{refused:?}: {:?}",
            BasePath::parse(refused)
        );
    }
}

#[test]
fn the_command_line_gives_one_prefix_or_is_refused() {
    let read = |v: &[&str]| {
        from_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .map(|b| b.map(|b| b.as_str().to_string()))
    };
    for (args, want) in [
        (&[][..], None),
        (&["--port", "6901", "--read-only"], None),
        (&["--title=--base-path=/x"], None),
        (&["--title", "base-path"], None),
        (&["--base-path", "/d"], Some("/d")),
        (&["--base-path=/d/"], Some("/d")),
        (&["--base_path", "/d"], Some("/d")),
        (&["--base_path=/d"], Some("/d")),
        (
            &["--port", "6901", "--base-path", "/a/b", "--read-only"],
            Some("/a/b"),
        ),
    ] {
        assert_eq!(read(args), Ok(want.map(String::from)), "{args:?}");
    }
    for refused in [
        &["--base-path"][..],
        &["--base-path="],
        &["--base-path", ""],
        &["--base-path", "/"],
        &["--base-path=/"],
        &["--base-path=//"],
        &["--base-path", "-h"],
        &["--base-path", "--help"],
        &["--base-path", "desk"],
        &["--base-path=/d", "--base-path=/d"],
        &["--base-path=/d", "--base_path", "/e"],
        &["--base-path=/desk", "--base-path="],
        &["--base-path=//", "--base_path", "/desk"],
        &["--base-path", "--base-path=/d"],
        &["--title", "--base-path"],
    ] {
        assert!(read(refused).is_err(), "{refused:?}: {:?}", read(refused));
    }
}

#[tokio::test]
async fn a_prefix_serves_nothing_outside_itself() {
    for prefix in ["/desk", "/a/b-c_d.e~f/9"] {
        let port = serve(app(Some(prefix))).await;
        let inside = format!("{prefix}/healthz");
        assert_eq!(
            get_from(port, &inside, Asking::Signed, "").await.0,
            200,
            "{inside}"
        );
        let longer = format!("{prefix}top/healthz");
        for outside in [
            "/healthz",
            "/",
            "/stats",
            longer.as_str(),
            "/desk/../healthz",
        ] {
            let (status, body) = get_from(port, outside, Asking::Signed, "").await;
            assert_eq!(status, 404, "{prefix}: {outside}: {body}");
        }
    }
}

#[tokio::test]
async fn under_the_prefix_every_route_answers_as_without_one() {
    let plain = serve(app(None)).await;
    let prefixed = serve(app(Some("/p"))).await;
    for path in [
        "/",
        "/healthz",
        "/client.js",
        "/stats",
        "/stats?x=1",
        "/nothing",
    ] {
        for asking in ASKING {
            let (want, want_body) = get_from(plain, path, asking, "").await;
            let target = format!("/p{path}");
            let (got, got_body) = get_from(prefixed, &target, asking, "").await;
            assert_eq!(
                got, want,
                "{target} {asking:?}: {got_body}\nwithout: {want_body}"
            );
            if want == 200 {
                assert!(got_body.ends_with(&format!("path {path}")), "{got_body}");
            }
        }
    }
}

#[tokio::test]
async fn the_bare_prefix_is_checked_before_it_is_sent_on() {
    let port = serve(app(Some("/p"))).await;
    for asking in [Asking::Anonymous, Asking::OtherHost, Asking::OtherSite] {
        let (want, want_body) = get_from(port, "/p/", asking, "").await;
        let (got, got_body) = get_from(port, "/p", asking, "").await;
        assert_ne!(want, 200, "{asking:?}: {want_body}");
        assert_eq!(got, want, "{asking:?}: {got_body}\n/p/: {want_body}");
    }
    let (status, _) = get_from(port, "/p", Asking::Anonymous, "").await;
    assert_eq!(status, 401);
    let (status, head) = get_from(port, "/p?token=pw", Asking::Anonymous, "").await;
    assert_eq!(status, 308, "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("location: /p/?token=pw\r\n"),
        "{head}"
    );
}

#[tokio::test]
async fn the_websocket_upgrades_under_the_prefix() {
    let port = serve(app(Some("/p"))).await;
    let upgrade = "Upgrade: websocket\r\nConnection: Upgrade\r\n\
                   Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n";
    let (status, head) = get_from(port, "/p/ws", Asking::Signed, upgrade).await;
    assert_eq!(status, 101, "{head}");
}

#[tokio::test]
async fn without_a_prefix_nothing_changes() {
    let port = serve(app(None)).await;
    let (status, body) = get_from(port, "/healthz", Asking::Anonymous, "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.ends_with("path /healthz"), "{body}");
    assert_eq!(
        get_from(port, "/p/healthz", Asking::Signed, "").await.0,
        404
    );
}

#[tokio::test]
async fn outside_paths_get_the_checks_first() {
    let plain = serve(app(None)).await;
    let prefixed = serve(app(Some("/p"))).await;
    for path in [
        "/",
        "/healthz",
        "/client.js",
        "/stats",
        "/nothing",
        "/pq/stats",
    ] {
        for asking in ASKING {
            let (without, _) = get_from(plain, path, asking, "").await;
            let (got, body) = get_from(prefixed, path, asking, "").await;
            let want = match without {
                401 | 403 => without,
                _ => 404,
            };
            assert_eq!(got, want, "{path} {asking:?}: {body}");
        }
    }
}

#[tokio::test]
async fn an_outside_path_is_a_plain_not_found_whatever_the_method() {
    let plain = serve(app(None)).await;
    let prefixed = serve(app(Some("/p"))).await;
    for (method, path) in [
        ("POST", "/healthz"),
        ("PUT", "/stats"),
        ("DELETE", "/"),
        ("POST", "/nothing"),
    ] {
        let (status, head) = ask(prefixed, method, path, Asking::Signed, "").await;
        assert_eq!(status, 404, "{method} {path}: {head}");
        assert!(
            !head.to_ascii_lowercase().contains("\r\nallow:"),
            "{method} {path}: {head}"
        );
        let (want, want_head) = ask(plain, method, path, Asking::Signed, "").await;
        let inside = format!("/p{path}");
        let (got, got_head) = ask(prefixed, method, &inside, Asking::Signed, "").await;
        assert_eq!(got, want, "{method} {inside}: {got_head}");
        assert_eq!(
            got_head.to_ascii_lowercase().contains("\r\nallow:"),
            want_head.to_ascii_lowercase().contains("\r\nallow:"),
            "{method} {inside}: {got_head}\nwithout: {want_head}"
        );
    }
}

#[tokio::test]
async fn the_query_reaches_the_route_under_the_prefix() {
    let port = serve(app(Some("/p"))).await;
    let (status, body) = get_from(port, "/p/stats?token=pw", Asking::Anonymous, "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.ends_with("path /stats?token=pw"), "{body}");
    let (status, body) = get_from(port, "/p/stats?x=1&y=%2F", Asking::Signed, "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.ends_with("path /stats?x=1&y=%2F"), "{body}");
    let upgrade = "Upgrade: websocket\r\nConnection: Upgrade\r\n\
                   Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n";
    let (status, head) = get_from(port, "/p/ws?token=pw", Asking::Anonymous, upgrade).await;
    assert_eq!(status, 101, "{head}");
}

#[cfg(unix)]
#[test]
fn an_argument_that_is_not_utf8_is_refused_not_a_panic() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let bad = vec![
        OsString::from("--base-path"),
        OsString::from_vec(b"/d\xff".to_vec()),
    ];
    let got = std::panic::catch_unwind(|| crate::utf8_args(bad));
    let err = got.expect("no panic").expect_err("refused");
    assert!(err.contains("argument 2"), "{err}");
    assert_eq!(
        crate::utf8_args(vec![OsString::from("--base-path"), OsString::from("/d")]),
        Ok(vec!["--base-path".to_string(), "/d".to_string()])
    );
}

#[test]
fn the_startup_lines_carry_the_prefix_and_are_unchanged_without_one() {
    assert_eq!(
        listening("0.0.0.0", 6901, None),
        "listening on http://0.0.0.0:6901"
    );
    assert_eq!(listening("::1", 80, None), "listening on http://::1:80");
    let d = prefix("/d");
    assert_eq!(
        listening("0.0.0.0", 6901, Some(&d)),
        "listening on http://0.0.0.0:6901/d/"
    );
    assert_eq!(page("0.0.0.0", 6901, None), "http://localhost:6901/");
    assert_eq!(page("::", 6901, None), "http://localhost:6901/");
    assert_eq!(page("::1", 6901, None), "http://[::1]:6901/");
    assert_eq!(page("10.0.0.2", 6901, None), "http://10.0.0.2:6901/");
    assert_eq!(page("0.0.0.0", 6901, Some(&d)), "http://localhost:6901/d/");
    assert_eq!(page("::1", 6901, Some(&d)), "http://[::1]:6901/d/");
}
