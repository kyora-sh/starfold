//! `--base-path`: serves everything under a path prefix, for a reverse proxy that forwards
//! `/PREFIX/...` unchanged. The page's own URLs are relative, so they resolve under it.

use crate::access::{self, Access};
use crate::login::{self, Login};
use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri};
use axum::middleware::{from_fn, from_fn_with_state, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Router;
use std::sync::Arc;

/// The longest prefix taken, in bytes. A prefix is part of every URL the page asks for and
/// counts against what an HTTP/1.1 request line may hold (the server's parser takes 64 KiB;
/// proxies often less), so this keeps `PREFIX/` and what follows it reachable.
pub const MAX_LEN: usize = 1024;

/// A path prefix in its one normal form: `/` and parts of letters, digits, `.`, `_`, `~` or
/// `-` between single slashes, none `.` or `..`, no trailing slash, at most [`MAX_LEN`] bytes.
/// [`BasePath::parse`] is the only way to make one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BasePath(Arc<str>);

impl BasePath {
    /// The prefix a `--base-path` value asks for. One trailing slash is dropped (`/desk/` is
    /// `/desk`). An empty value or `/` (no prefix, though one was asked for) and every shape
    /// not in the normal form are refused.
    pub fn parse(v: &str) -> Result<BasePath, String> {
        if v.is_empty() || v == "/" {
            return Err(format!(
                "{v:?} is no prefix; leave the flag out to serve at the root"
            ));
        }
        let normal = v.strip_suffix('/').unwrap_or(v);
        if normal.len() > MAX_LEN {
            return Err(format!(
                "{} bytes is longer than the {MAX_LEN} a prefix may be",
                normal.len()
            ));
        }
        let part_ok = |p: &str| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
        };
        match normal.strip_prefix('/') {
            Some(rest) if rest.split('/').all(part_ok) => Ok(BasePath(normal.into())),
            _ => Err(format!(
                "{v:?}: has to be / and parts of letters, digits, '.', '_', '~' or '-' between \
                 single slashes, none '.' or '..' (such as /desktop)"
            )),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The prefix the command line asks for, read as the flag parser reads it: `--base-path=V`,
/// or `--base-path V` when V doesn't start with `--`, in either spelling (`--base_path`).
/// - Absent: none, served at the root.
/// - Once, with a value [`BasePath::parse`] takes: that prefix.
/// - Anything else is refused: given more than once (in any spelling or form), without a
///   value, with an empty value or `/`, or with a value not in the normal form.
pub fn from_args(args: &[String]) -> Result<Option<BasePath>, String> {
    let mut given = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(k) = args[i].strip_prefix("--") {
            let (key, value) = match k.split_once('=') {
                Some((key, v)) => (key, Some(v)),
                None if i + 1 < args.len() && !args[i + 1].starts_with("--") => {
                    i += 1;
                    (k, Some(args[i].as_str()))
                }
                None => (k, None),
            };
            if key.replace('_', "-") == "base-path" {
                given.push(value);
            }
        }
        i += 1;
    }
    match given[..] {
        [] => Ok(None),
        [Some(v)] => BasePath::parse(v).map(Some),
        [None] => Err("needs a value, such as /desktop".into()),
        _ => Err(format!(
            "given {} times; give it once (or leave it out to serve at the root)",
            given.len()
        )),
    }
}

/// What the server serves: `routes` behind the login (if on) and the host and origin check,
/// as without a prefix. Under `base`, `PREFIX/rest` reaches them as `/rest`; `PREFIX` itself
/// goes through the same checks as `PREFIX/` and is then sent on to it, so the page's
/// relative URLs resolve under the prefix; any other path goes through them and is then not
/// found, before any route sees it.
pub fn compose(
    routes: Router,
    base: Option<&BasePath>,
    login: Option<Arc<Login>>,
    access: Arc<Access>,
) -> Router {
    // Under a prefix the routes sit behind one service, so `settle` answers before routing
    // (a route would add its own headers, such as `Allow`, to an answer for an outside path).
    let mut app = match base {
        Some(_) => Router::new()
            .fallback_service(routes)
            .layer(from_fn(settle)),
        None => routes,
    };
    if let Some(l) = login {
        app = app.layer(from_fn_with_state(l, login::check));
    }
    // Added last, so it runs first: a request for the wrong host or from another site's
    // page is turned away before the login is asked for.
    let app = app.layer(from_fn_with_state(access, access::check));
    match base {
        None => app,
        Some(p) => app.layer(from_fn_with_state(p.0.clone(), mark)),
    }
}

/// A request the checks see under another path than it came with.
#[derive(Clone)]
enum Mark {
    /// For the prefix itself, seen as `/`: sent on to `PREFIX/` once the checks pass.
    Bare(Arc<str>),
    /// Outside the prefix, seen as it came: not found once the checks pass.
    Outside,
}

/// Strips the prefix, so the checks and routes see the paths they see without one, and
/// marks the requests that aren't for something under it.
async fn mark(State(prefix): State<Arc<str>>, mut req: Request, next: Next) -> Response {
    let uri = req.uri();
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let (path, mark) = if uri.path() == &*prefix {
        ("/".to_string(), Some(Mark::Bare(prefix.clone())))
    } else {
        match uri
            .path()
            .strip_prefix(&*prefix)
            .filter(|r| r.starts_with('/'))
        {
            Some(rest) => (rest.to_string(), None),
            None => (uri.path().to_string(), Some(Mark::Outside)),
        }
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = format!("{path}{query}").parse().ok();
    match Uri::from_parts(parts) {
        Ok(seen) => *req.uri_mut() = seen,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    }
    if let Some(m) = mark {
        req.extensions_mut().insert(m);
    }
    next.run(req).await
}

/// After the checks: the prefix itself is sent on to its slash, what is outside it is not
/// found, and the rest goes to its route.
async fn settle(req: Request, next: Next) -> Response {
    match req.extensions().get::<Mark>() {
        Some(Mark::Bare(prefix)) => {
            let query = req
                .uri()
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default();
            Redirect::permanent(&format!("{prefix}/{query}")).into_response()
        }
        Some(Mark::Outside) => StatusCode::NOT_FOUND.into_response(),
        None => next.run(req).await,
    }
}

/// The log line saying where the server listens; without a prefix as it always was.
pub fn listening(addr: &str, port: u16, base: Option<&BasePath>) -> String {
    match base {
        Some(p) => format!("listening on http://{addr}:{port}{}/", p.as_str()),
        None => format!("listening on http://{addr}:{port}"),
    }
}

/// Where to point a browser on this machine.
pub fn page(addr: &str, port: u16, base: Option<&BasePath>) -> String {
    let p = base.map_or("", |b| b.as_str());
    match addr {
        "0.0.0.0" | "::" => format!("http://localhost:{port}{p}/"),
        a if a.contains(':') => format!("http://[{a}]:{port}{p}/"),
        a => format!("http://{a}:{port}{p}/"),
    }
}

#[cfg(test)]
mod tests;
