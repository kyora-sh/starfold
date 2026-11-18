use super::*;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::Uri;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "pw";

/// Routes standing in for the server's, each answering the path it was asked for.
fn routes() -> Router {
    let echo = |uri: Uri| async move { format!("path {}", uri.path()) };
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
    compose(routes(), base, Some(login), access)
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
    let req = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n{more}{close}{extra}\r\n");
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
    let accepted: &[(Option<&str>, Option<&str>)] = &[
        (None, None),
        (Some(""), None),
        (Some("/"), None),
        (Some("/desk"), Some("/desk")),
        (Some("/desk/"), Some("/desk")),
        (Some("/a/b-c_d.e~f/9"), Some("/a/b-c_d.e~f/9")),
        (Some("/a/b/"), Some("/a/b")),
    ];
    for &(given, normal) in accepted {
        assert_eq!(parse(given), Ok(normal.map(String::from)), "{given:?}");
    }
    for refused in [
        "desk", "desk/", "//", "///", "/desk//", "//desk", "/a//b", "/a/b//", "/%2F", "/a%2Fb",
        "/%2e", "/.", "/..", "/a/./b", "/a/../b", "/a/.", "/a b", "/a?b", "/a#b", "/ä", "/a\\b",
    ] {
        assert!(
            parse(Some(refused)).is_err(),
            "{refused:?}: {:?}",
            parse(Some(refused))
        );
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
                assert!(got_body.ends_with(&format!("path {}", path.split('?').next().unwrap())));
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

#[test]
fn the_startup_lines_carry_the_prefix_and_are_unchanged_without_one() {
    assert_eq!(
        listening("0.0.0.0", 6901, None),
        "listening on http://0.0.0.0:6901"
    );
    assert_eq!(listening("::1", 80, None), "listening on http://::1:80");
    assert_eq!(
        listening("0.0.0.0", 6901, Some("/d")),
        "listening on http://0.0.0.0:6901/d/"
    );
    assert_eq!(page("0.0.0.0", 6901, None), "http://localhost:6901/");
    assert_eq!(page("::", 6901, None), "http://localhost:6901/");
    assert_eq!(page("::1", 6901, None), "http://[::1]:6901/");
    assert_eq!(page("10.0.0.2", 6901, None), "http://10.0.0.2:6901/");
    assert_eq!(
        page("0.0.0.0", 6901, Some("/d")),
        "http://localhost:6901/d/"
    );
    assert_eq!(page("::1", 6901, Some("/d")), "http://[::1]:6901/d/");
}
