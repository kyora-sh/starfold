use super::*;
use axum::routing::{on, MethodFilter};
use axum::Router;
use std::collections::BTreeSet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SERVER: &str = include_str!("../server.rs");

fn ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `text` without its comments; string, raw string and char literals are kept whole.
fn code(text: &str) -> String {
    let t: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < t.len() {
        let c = t[i];
        if c == 'r' && (i == 0 || !ident(t[i - 1])) {
            let mut j = i + 1;
            while t.get(j) == Some(&'#') {
                j += 1;
            }
            if t.get(j) == Some(&'"') {
                let hashes = j - i - 1;
                let closes = |k: usize| {
                    t[k] == '"' && t[k + 1..].iter().take(hashes).filter(|h| **h == '#').count() == hashes
                };
                let mut k = j + 1;
                while k < t.len() && !closes(k) {
                    k += 1;
                }
                let end = (k + 1 + hashes).min(t.len());
                out.extend(&t[i..end]);
                i = end;
                continue;
            }
        }
        let end = match c {
            '"' => {
                let mut k = i + 1;
                while k < t.len() && t[k] != '"' {
                    k += if t[k] == '\\' { 2 } else { 1 };
                }
                k + 1
            }
            '\'' if t.get(i + 1) == Some(&'"') => i + 2,
            '\'' if t.get(i + 1) == Some(&'\\') => {
                let mut k = i + 3;
                while k < t.len() && t[k] != '\'' {
                    k += 1;
                }
                k + 1
            }
            '/' if t.get(i + 1) == Some(&'/') => {
                while i < t.len() && t[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if t.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < t.len() && !(t[i] == '*' && t[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
                continue;
            }
            _ => i + 1,
        };
        let end = end.min(t.len());
        out.extend(&t[i..end]);
        i = end;
    }
    out
}

/// Each `.name(` in `code`: where `name` starts and the text after its `(`. Whitespace may
/// sit around the dot and before the parenthesis.
fn method_calls<'a>(code: &'a str, name: &str) -> Vec<(usize, &'a str)> {
    let mut out = Vec::new();
    for (at, _) in code.match_indices(name) {
        let before = &code[..at];
        let after = &code[at + name.len()..];
        if before.ends_with(ident) || after.starts_with(ident) || !before.trim_end().ends_with('.') {
            continue;
        }
        if let Some(args) = after.trim_start().strip_prefix('(') {
            out.push((at, args));
        }
    }
    out
}

/// The arguments of a call: `rest` from just after its `(` up to the `)` that closes it.
fn arguments(rest: &str) -> &str {
    let mut depth = 1;
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => {
                while let Some((_, s)) = chars.next() {
                    match s {
                        '\\' => {
                            chars.next();
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..i];
                }
            }
            _ => {}
        }
    }
    panic!("a call that is never closed: {rest}")
}

/// Every name called in `args` (`name(` or `name!(`), string literals left out.
fn called(args: &str) -> Vec<String> {
    let chars: Vec<char> = args.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                i += if chars[i] == '\\' { 2 } else { 1 };
            }
            i += 1;
        } else if ident(chars[i]) {
            let start = i;
            while i < chars.len() && ident(chars[i]) {
                i += 1;
            }
            let mut name: String = chars[start..i].iter().collect();
            let mut j = i;
            if chars.get(j) == Some(&'!') {
                name.push('!');
                j += 1;
            }
            while chars.get(j).is_some_and(|c| c.is_whitespace()) {
                j += 1;
            }
            if chars.get(j) == Some(&'(') {
                out.push(name);
            }
        } else {
            i += 1;
        }
    }
    out
}

/// The method a method-routing function (or its `_service` form) registers; `any` and `on`
/// take every method. The bundled! and asset! macros are get.
fn method_of(f: &str) -> Option<&'static str> {
    Some(match f.strip_suffix("_service").unwrap_or(f) {
        "get" | "bundled!" | "asset!" => "GET",
        "head" => "HEAD",
        "post" => "POST",
        "put" => "PUT",
        "patch" => "PATCH",
        "delete" => "DELETE",
        "options" => "OPTIONS",
        "trace" => "TRACE",
        "connect" => "CONNECT",
        "any" | "on" => "ANY",
        _ => return None,
    })
}

/// Every `.route(` in `code`: its path (the first string literal of its arguments) with
/// each method its arguments register, chained ones (`get(a).post(b)`) included. One whose
/// arguments call no method-routing function is `?`, which no table entry matches.
fn routes_in(code: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for (_, rest) in method_calls(code, "route") {
        let args = arguments(rest);
        let (_, path) = args.split_once('"').expect("a path literal");
        let (path, _) = path.split_once('"').expect("its end");
        let methods: Vec<_> = called(args).iter().filter_map(|f| method_of(f)).collect();
        if methods.is_empty() {
            out.insert(("?".to_string(), path.to_string()));
        }
        for m in methods {
            out.insert((m.to_string(), path.to_string()));
        }
    }
    out
}

fn routes_in_server() -> BTreeSet<(String, String)> {
    routes_in(&code(SERVER))
}

/// Every command a match takes: each literal of an arm's pattern (`"a" =>`, `"a" | "b" =>`,
/// `"a" if ... =>`), alternatives on earlier lines included.
fn commands_in(body: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut pattern = String::new();
    for l in body.lines().map(str::trim) {
        let continues = !pattern.is_empty() && l.starts_with('|');
        if !l.starts_with('"') && !continues {
            pattern.clear();
            continue;
        }
        pattern.push(' ');
        pattern.push_str(l);
        let Some((p, _)) = pattern.split_once("=>") else {
            continue;
        };
        let p = p.split(" if ").next().unwrap_or(p);
        for alt in p.split('|') {
            if let Some(cmd) = alt
                .trim()
                .strip_prefix('"')
                .and_then(|a| a.strip_suffix('"'))
            {
                out.insert(cmd.to_string());
            }
        }
        pattern.clear();
    }
    out
}

fn commands_in_server() -> BTreeSet<String> {
    let server = code(SERVER);
    let start = server.find("fn handle_text(").expect("handle_text");
    let body = &server[start..];
    commands_in(&body[..body.find("\n}\n").expect("its end")])
}

/// Calls that add routes, fallbacks or other routers to a router.
const REGISTERING: &[&str] = &[
    "route",
    "route_service",
    "merge",
    "nest",
    "nest_service",
    "fallback",
    "fallback_service",
    "method_not_allowed_fallback",
];

#[test]
fn the_scan_sees_the_shapes_a_route_or_command_can_take() {
    let routes = routes_in(&code(
        r#"Router::new()
            .route ( "/a", get(a).post(b) )
            .route("/b",
                axum::routing::head(h))
            // .route("/commented", post(x))
            /* .route("/block", post(x)) */
            .route("/c", on(MethodFilter::PUT, c))
            .route("/d", helper())
            .route("/e", bundled!("e.js"))"#,
    ));
    let want = [
        ("GET", "/a"),
        ("POST", "/a"),
        ("HEAD", "/b"),
        ("ANY", "/c"),
        ("?", "/d"),
        ("GET", "/e"),
    ]
    .map(|(m, p)| (m.to_string(), p.to_string()));
    assert_eq!(routes, BTreeSet::from(want));
    let commands = commands_in(
        r#"
        "a" => x(),
        "b"
        | "c" => y(),
        "d" | "e"
            | "f" if z => w(),
        other => {}
        "#,
    );
    let want = ["a", "b", "c", "d", "e", "f"].map(String::from);
    assert_eq!(commands, BTreeSet::from(want));
    for (text, name) in [
        ("r . method_not_allowed_fallback (h)", "method_not_allowed_fallback"),
        ("r.fallback\n(h)", "fallback"),
        ("r\n    .route(\n\"/x\", get(h))", "route"),
        ("let s = r#\"// \"#; r.merge(x)", "merge"),
    ] {
        assert_eq!(method_calls(&code(text), name).len(), 1, "{text}");
    }
    for (text, name) in [
        ("c.route.lock()", "route"),
        ("// r.route(\"/x\", get(h))", "route"),
        ("r.route_service(\"/x\", s)", "route"),
        ("r.nest_service(\"/x\", s)", "nest"),
    ] {
        assert!(method_calls(&code(text), name).is_empty(), "{text}");
    }
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
    let mut seen_server = false;
    for (path, text) in sources() {
        let text = code(&text);
        if path.ends_with("server.rs") {
            seen_server = true;
            for f in REGISTERING.iter().filter(|f| !["route", "merge"].contains(f)) {
                assert!(
                    method_calls(&text, f).is_empty(),
                    "server.rs calls .{f}(: decide what it serves under --read-only in readonly.rs"
                );
            }
            // Every route and merged router is added inside router(), before its one
            // read-only layer, which Router::layer applies only to what is there already.
            let start = text.find("pub fn router(").expect("router()");
            let layers: Vec<_> = text.match_indices("crate::readonly::check").collect();
            assert_eq!(layers.len(), 1, "one read-only layer in server.rs");
            let layer = layers[0].0;
            for f in ["route", "merge"] {
                for (at, rest) in method_calls(&text, f) {
                    assert!(
                        start < at && at < layer,
                        ".{f}({} is not inside router() before the read-only layer",
                        &rest[..rest.len().min(40)]
                    );
                }
            }
            continue;
        }
        for f in REGISTERING {
            assert!(
                method_calls(&text, f).is_empty(),
                "{} calls .{f}(: routes are added in server.rs's router(), before the read-only layer, with decisions in readonly.rs",
                path.display()
            );
        }
        assert!(
            !text
                .split(|c: char| !ident(c))
                .any(|w| w == "Router" || w == "MethodRouter"),
            "{} names a router type: routes are added in server.rs's router()",
            path.display()
        );
    }
    assert!(seen_server);
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
