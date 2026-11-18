use super::*;
use axum::routing::{on, MethodFilter};
use axum::Router;
use std::collections::BTreeSet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SERVER: &str = include_str!("../server.rs");

/// The method-routing functions a route can be registered with, and the method each means.
const METHODS: &[(&str, &str)] = &[
    ("routing::post(", "POST"),
    ("routing::put(", "PUT"),
    ("routing::patch(", "PATCH"),
    ("routing::delete(", "DELETE"),
    ("routing::any(", "ANY"),
    ("routing::on(", "ANY"),
];

/// Every `.route(` in server.rs, however its arguments are laid out over lines: its path
/// (the first string literal after it) and its method (from the method-routing call before
/// the next `.route(`; get, the bundled! and asset! routes are GET).
fn routes_in_server() -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let mut rest = SERVER;
    while let Some(at) = rest.find(".route(") {
        rest = &rest[at + ".route(".len()..];
        let open = rest.find('"').expect("a path literal");
        let (path, after) = rest[open + 1..].split_once('"').expect("its end");
        let call = &after[..after.find(".route(").unwrap_or(after.len())];
        let call = &call[..call.find("\n    }").unwrap_or(call.len())];
        let method = METHODS
            .iter()
            .find(|(f, _)| call.contains(f))
            .map_or("GET", |m| m.1);
        out.insert((method.to_string(), path.to_string()));
    }
    out
}

/// Every command `handle_text` matches on: each literal of an arm's pattern (`"a" =>`,
/// `"a" | "b" =>`, `"a" if ... =>`).
fn commands_in_server() -> BTreeSet<String> {
    let start = SERVER.find("fn handle_text(").expect("handle_text");
    let body = &SERVER[start..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    let mut out = BTreeSet::new();
    for l in body.lines().map(str::trim) {
        if !l.starts_with('"') {
            continue;
        }
        let Some((pattern, _)) = l.split_once("=>") else {
            continue;
        };
        let pattern = pattern.split(" if ").next().unwrap_or(pattern);
        for alt in pattern.split('|') {
            if let Some(cmd) = alt
                .trim()
                .strip_prefix('"')
                .and_then(|a| a.strip_suffix('"'))
            {
                out.insert(cmd.to_string());
            }
        }
    }
    out
}

/// Every source file of the crate but its tests, as (path, text).
fn sources() -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let mut dirs = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("tests.rs") {
                out.push((p.clone(), std::fs::read_to_string(&p).unwrap()));
            }
        }
    }
    out
}

#[test]
fn routes_are_only_registered_where_the_table_is_checked() {
    for (path, text) in sources() {
        if path.ends_with("server.rs") {
            for f in [
                ".fallback(",
                ".fallback_service(",
                ".nest(",
                ".nest_service(",
                ".route_service(",
            ] {
                assert!(
                    !text.contains(f),
                    "server.rs uses {f}: decide what it serves under --read-only in readonly.rs"
                );
            }
            continue;
        }
        assert!(
            !text.contains(".route(\"") && !text.contains("Router::new()"),
            "{} builds routes; they need decisions in readonly.rs",
            path.display()
        );
    }
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
    r.fallback(|| async { "fallback ran" })
        .layer(axum::middleware::from_fn_with_state(read_only, check))
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
    for method in ["GET", "POST"] {
        let (status, text) = ask(port, method, "/no-such-route").await;
        assert_eq!(status, 404, "{method}: {text}");
        assert!(!text.contains("fallback ran"), "{method}: the fallback ran");
    }
}

#[tokio::test]
async fn a_server_that_is_not_read_only_refuses_nothing() {
    let port = serve(stub(false)).await;
    for (m, p, _) in ROUTES {
        let (_, text) = ask(port, m, p).await;
        assert!(text.ends_with("ran"), "{m} {p}: {text}");
    }
}
