use super::*;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::Uri;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "pw";

/// What `serve` composes, with handlers that answer the path they were asked for: the
/// routes, then the login and the host check as layers.
fn app() -> Router {
    let echo = |uri: Uri| async move { format!("path {}", uri.path()) };
    let login = Arc::new(crate::login::Login::new(None, PASSWORD.into()));
    let access = Arc::new(crate::access::Access::new(None, None));
    Router::new()
        .route("/", get(echo))
        .route("/healthz", get(echo))
        .route("/client.js", get(echo))
        .route("/stats", get(echo))
        .route(
            "/ws",
            get(|ws: WebSocketUpgrade| async move { ws.on_upgrade(|_| async {}) }),
        )
        .layer(axum::middleware::from_fn_with_state(
            login,
            crate::login::check,
        ))
        .layer(axum::middleware::from_fn_with_state(
            access,
            crate::access::check,
        ))
}

/// Serves `router` on a loopback port and returns the port.
async fn serve(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    port
}

/// The status and the rest of the response to a GET of `target` with these extra headers.
async fn get_from(port: u16, target: &str, extra: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let close = match extra.contains("Connection:") {
        true => "",
        false => "Connection: close\r\n",
    };
    let req = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{close}{extra}\r\n");
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

fn bearer() -> String {
    format!("Authorization: Bearer {PASSWORD}\r\n")
}

#[test]
fn prefixes_are_parsed_and_checked() {
    assert_eq!(parse(None), Ok(None));
    assert_eq!(parse(Some("")), Ok(None));
    assert_eq!(parse(Some("/")), Ok(None));
    assert_eq!(parse(Some("/desk")), Ok(Some("/desk".into())));
    assert_eq!(parse(Some("/desk/")), Ok(Some("/desk".into())));
    assert_eq!(
        parse(Some("/a/b-c_d.e~f/9")),
        Ok(Some("/a/b-c_d.e~f/9".into()))
    );
    for bad in [
        "desk", "//desk", "/a//b", "/./a", "/a/..", "/a b", "/a?b", "/a%2F", "/ä", "/a#b",
    ] {
        assert!(parse(Some(bad)).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn requests_under_the_prefix_reach_the_routes_as_they_are_without_one() {
    let port = serve(under("/p/q", app())).await;
    let (status, body) = get_from(port, "/p/q/", &bearer()).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.ends_with("path /"), "{body}");
    let (status, body) = get_from(port, "/p/q/stats?x=1", &bearer()).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.ends_with("path /stats"), "{body}");
}

#[tokio::test]
async fn the_prefix_itself_is_sent_on_to_its_slash() {
    let port = serve(under("/p", app())).await;
    let (status, head) = get_from(port, "/p?token=pw", "").await;
    assert_eq!(status, 308, "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("location: /p/?token=pw\r\n"),
        "{head}"
    );
}

#[tokio::test]
async fn nothing_outside_the_prefix_is_served() {
    let port = serve(under("/p", app())).await;
    for target in ["/", "/healthz", "/stats", "/px/healthz", "/pp", "/ws"] {
        let (status, body) = get_from(port, target, &bearer()).await;
        assert_eq!(status, 404, "{target}: {body}");
    }
}

#[tokio::test]
async fn the_login_works_under_the_prefix_as_without_one() {
    let port = serve(under("/p", app())).await;
    for public in ["/p/healthz", "/p/client.js"] {
        let (status, body) = get_from(port, public, "").await;
        assert_eq!(status, 200, "{public}: {body}");
    }
    for private in ["/p/", "/p/stats", "/p/stats?token=wrong"] {
        let (status, body) = get_from(port, private, "").await;
        assert_eq!(status, 401, "{private}: {body}");
    }
    let (status, body) = get_from(port, "/p/stats?token=pw", "").await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn the_websocket_upgrades_under_the_prefix() {
    let port = serve(under("/p", app())).await;
    let upgrade = "Upgrade: websocket\r\nConnection: Upgrade\r\n\
                   Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n";
    let (status, head) = get_from(port, "/p/ws?token=pw", upgrade).await;
    assert_eq!(status, 101, "{head}");
}

#[tokio::test]
async fn without_a_prefix_nothing_changes() {
    let port = serve(app()).await;
    let (status, body) = get_from(port, "/healthz", "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.ends_with("path /healthz"), "{body}");
}
