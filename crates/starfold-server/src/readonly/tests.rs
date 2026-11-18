use super::*;
use axum::routing::{on, MethodFilter};
use axum::Router;
use std::collections::BTreeSet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SERVER: &str = include_str!("../server.rs");

/// Every `.route("PATH", ...)` in server.rs with its method: post( and put( as written,
/// everything else (get, the bundled! and asset! routes) GET.
fn routes_in_server() -> BTreeSet<(String, String)> {
    SERVER
        .lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix(".route(\"")?;
            let (path, tail) = rest.split_once('"')?;
            let method = if tail.contains("routing::post(") {
                "POST"
            } else if tail.contains("routing::put(") {
                "PUT"
            } else {
                "GET"
            };
            Some((method.to_string(), path.to_string()))
        })
        .collect()
}

/// Every command `handle_text` matches on (`"cmd" =>` or `"cmd" if ... =>`).
fn commands_in_server() -> BTreeSet<String> {
    let start = SERVER.find("fn handle_text(").expect("handle_text");
    let body = &SERVER[start..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    body.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix('"')?;
            let (cmd, tail) = rest.split_once('"')?;
            tail.contains("=>").then(|| cmd.to_string())
        })
        .collect()
}

#[test]
fn every_route_the_server_has_is_decided_once() {
    let table: Vec<(String, String)> = ROUTES
        .iter()
        .map(|(m, p, _)| (m.to_string(), p.to_string()))
        .collect();
    let decided: BTreeSet<_> = table.iter().cloned().collect();
    assert_eq!(decided.len(), table.len(), "a route decided twice");
    let routes = routes_in_server();
    assert!(routes.len() >= 20, "{routes:?}");
    assert_eq!(
        routes, decided,
        "routes in server.rs vs readonly::ROUTES: a new route needs a decision"
    );
}

#[test]
fn every_command_the_server_takes_is_decided_once() {
    let decided: BTreeSet<String> = COMMANDS.iter().map(|(c, _)| c.to_string()).collect();
    assert_eq!(decided.len(), COMMANDS.len(), "a command decided twice");
    let commands = commands_in_server();
    assert!(commands.len() >= 10, "{commands:?}");
    assert_eq!(
        commands, decided,
        "commands in handle_text vs readonly::COMMANDS: a new command needs a decision"
    );
}

#[test]
fn what_acts_is_what_changes_something() {
    let acting: BTreeSet<&str> = ROUTES.iter().filter(|r| r.2 == Act).map(|r| r.1).collect();
    assert_eq!(
        acting,
        BTreeSet::from([
            "/input",
            "/files/upload",
            "/record",
            "/record/start",
            "/record/stop",
            "/debug/renegotiate",
            "/debug/fail-h264",
        ])
    );
    for c in ["m", "b", "s", "sp", "k", "r", "c", "mi", "sz"] {
        assert_eq!(command(c), Some(Act), "{c}");
    }
    for c in ["a", "v", "au", "kf", "po"] {
        assert_eq!(command(c), Some(Watch), "{c}");
    }
    assert_eq!(
        command("new"),
        None,
        "no decision: refused under --read-only"
    );
    for first in [None, Some(0x11), Some(0x12), Some(0x13), Some(0xff)] {
        assert_eq!(binary(first), Act);
    }
    assert_eq!(route(&Method::HEAD, "/healthz"), Some(Watch));
    assert_eq!(route(&Method::POST, "/healthz"), None);
}

/// A router with every route in the table, each answering "ran", behind the layer.
fn stub(read_only: bool) -> Router {
    let mut r = Router::new();
    for (m, p, _) in ROUTES {
        let filter = MethodFilter::try_from(Method::from_bytes(m.as_bytes()).unwrap()).unwrap();
        r = r.route(p, on(filter, || async { "ran" }));
    }
    r.layer(axum::middleware::from_fn_with_state(read_only, check))
}

async fn serve(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    port
}

async fn ask(port: u16, method: &str, path: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[tokio::test]
async fn a_read_only_server_refuses_every_acting_route_and_no_watching_one() {
    let port = serve(stub(true)).await;
    for (m, p, kind) in ROUTES {
        let (status, text) = ask(port, m, p).await;
        match kind {
            Act => {
                assert_eq!(status, 403, "{m} {p}: {text}");
                assert!(text.contains("\"read_only\":true"), "{m} {p}: {text}");
                assert!(!text.ends_with("ran"), "{m} {p} ran");
            }
            Watch => assert!(text.ends_with("ran"), "{m} {p}: {text}"),
        }
    }
    let (status, _) = ask(port, "POST", "/healthz").await;
    assert_eq!(status, 403, "a method with no decision is refused too");
}

#[tokio::test]
async fn a_server_that_is_not_read_only_refuses_nothing() {
    let port = serve(stub(false)).await;
    for (m, p, _) in ROUTES {
        let (_, text) = ask(port, m, p).await;
        assert!(text.ends_with("ran"), "{m} {p}: {text}");
    }
}
