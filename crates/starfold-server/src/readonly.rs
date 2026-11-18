//! What `--read-only` lets through, decided in one place for every route and every
//! message a viewer sends. Watching (the page, the stream and its state, the screen) is
//! allowed; acting (the desktop, files, recordings, other viewers' streams) is refused.
//! Anything not in these tables is refused under `--read-only`, and the tests read the
//! router and the command handler so a new route or command without a decision fails.

use crate::negotiate::ClientMsg;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Watch,
    Act,
}

use Kind::{Act, Watch};

/// Every route the server has, by method and path. HEAD is GET.
pub const ROUTES: &[(&str, &str, Kind)] = &[
    ("GET", "/", Watch),
    ("GET", "/index.html", Watch),
    ("GET", "/client.js", Watch),
    ("GET", "/worker.js", Watch),
    ("GET", "/dav1d.js", Watch),
    ("GET", "/dav1d.wasm", Watch),
    ("GET", "/audio.js", Watch),
    ("GET", "/mic.js", Watch),
    ("GET", "/favicon.ico", Watch),
    ("GET", "/inter.woff2", Watch),
    ("GET", "/healthz", Watch),
    ("GET", "/stats", Watch),
    ("GET", "/ws", Watch),
    ("POST", "/input", Act),
    // Reading files: under --read-only only when --file-transfers names download
    // (files.rs), so they can stay watching here.
    ("GET", "/files/list", Watch),
    ("GET", "/files/download", Watch),
    ("PUT", "/files/upload", Act),
    ("GET", "/screenshot", Watch),
    ("GET", "/record", Act),
    ("POST", "/record/start", Act),
    ("POST", "/record/stop", Act),
    ("GET", "/wait", Watch),
    // Only when the server starts with STARFOLD_TEST_HOOKS set (server.rs).
    ("POST", "/debug/renegotiate", Act),
    ("POST", "/debug/fail-h264", Act),
];

/// A route's kind; None for one not in [`ROUTES`].
pub fn route(method: &Method, path: &str) -> Option<Kind> {
    let m = if method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    ROUTES
        .iter()
        .find(|(rm, rp, _)| *rm == m && *rp == path)
        .map(|r| r.2)
}

/// Every text command on /ws (`cmd,args`).
pub const COMMANDS: &[(&str, Kind)] = &[
    ("a", Watch),
    ("v", Watch),
    ("au", Watch),
    ("kf", Watch),
    ("po", Watch),
    ("m", Act),
    ("b", Act),
    ("s", Act),
    ("sp", Act),
    ("k", Act),
    ("r", Act),
    ("c", Act),
    ("mi", Act),
    ("sz", Act),
];

/// A text command's kind; None for one not in [`COMMANDS`].
pub fn command(cmd: &str) -> Option<Kind> {
    COMMANDS.iter().find(|(c, _)| *c == cmd).map(|c| c.1)
}

/// A JSON message's kind (protocol 2). All of them are about the viewer's own stream:
/// capabilities, decoder state, acks, keyframe requests, its display rate.
pub fn json(m: &ClientMsg) -> Kind {
    match m {
        ClientMsg::Hello { .. }
        | ClientMsg::Ready { .. }
        | ClientMsg::Error { .. }
        | ClientMsg::Ack { .. }
        | ClientMsg::Keyframe { .. }
        | ClientMsg::Slow
        | ClientMsg::Hz { .. } => Watch,
    }
}

/// A binary message's kind: clipboard images and microphone audio, all on the desktop.
pub fn binary(_first: Option<u8>) -> Kind {
    Act
}

/// The layer on every route and on the fallback: under `--read-only`, a route that acts,
/// or one with no decision, is refused before its handler runs, and a request no route
/// matched gets a plain 404 here, so no fallback handler runs for it either.
pub async fn check(State(read_only): State<bool>, req: Request, next: Next) -> Response {
    if read_only {
        let Some(path) = req.extensions().get::<MatchedPath>() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if route(req.method(), path.as_str()) != Some(Watch) {
            return refused(req.method(), path.as_str());
        }
    }
    next.run(req).await
}

fn refused(method: &Method, path: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({
            "error": format!("this server is read-only (--read-only): {method} {path} is refused"),
            "read_only": true,
        })
        .to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
