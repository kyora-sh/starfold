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

/// The prefix `--base-path` gives, in its one normal form: `/` and parts of letters, digits,
/// `.`, `_`, `~` or `-` between single slashes, none `.` or `..`, and no trailing slash (one
/// is dropped). Nothing, `""` or `/` is none; every other shape is refused.
pub fn parse(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(v) = value.filter(|v| !v.is_empty() && *v != "/") else {
        return Ok(None);
    };
    let normal = v.strip_suffix('/').unwrap_or(v);
    let part_ok = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
    };
    match normal.strip_prefix('/') {
        Some(rest) if rest.split('/').all(part_ok) => Ok(Some(normal.to_string())),
        _ => Err(format!(
            "{v:?}: has to be / and parts of letters, digits, '.', '_', '~' or '-' between \
             single slashes, none '.' or '..' (such as /desktop)"
        )),
    }
}

/// What the server serves: `routes` behind the login (if on) and the host and origin check,
/// as without a prefix. Under `base`, `PREFIX/rest` reaches them as `/rest`; `PREFIX` itself
/// goes through the same checks as `PREFIX/` and is then sent on to it, so the page's
/// relative URLs resolve under the prefix; any other path goes through them and is not found.
pub fn compose(
    routes: Router,
    base: Option<&str>,
    login: Option<Arc<Login>>,
    access: Arc<Access>,
) -> Router {
    let mut app = match base {
        Some(_) => routes.layer(from_fn(settle)),
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
        Some(p) => Router::new()
            .fallback_service(app)
            .layer(from_fn_with_state(Arc::<str>::from(p), mark)),
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
pub fn listening(addr: &str, port: u16, base: Option<&str>) -> String {
    match base {
        Some(p) => format!("listening on http://{addr}:{port}{p}/"),
        None => format!("listening on http://{addr}:{port}"),
    }
}

/// Where to point a browser on this machine.
pub fn page(addr: &str, port: u16, base: Option<&str>) -> String {
    let p = base.unwrap_or("");
    match addr {
        "0.0.0.0" | "::" => format!("http://localhost:{port}{p}/"),
        a if a.contains(':') => format!("http://[{a}]:{port}{p}/"),
        a => format!("http://{a}:{port}{p}/"),
    }
}

#[cfg(test)]
mod tests;
