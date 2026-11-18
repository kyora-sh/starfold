//! `--base-path`: serves everything under a path prefix, for a reverse proxy that forwards
//! `/PREFIX/...` unchanged. The page's own URLs are relative, so they resolve under it.

use axum::extract::{Request, State};
use axum::http::{StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Router;
use std::sync::Arc;

/// The prefix `--base-path` gives: `/` and path parts of letters, digits, `.`, `_`, `~` or
/// `-`, none empty, `.` or `..`. Trailing slashes are dropped; nothing, `""` or `/` is none.
pub fn parse(value: Option<&str>) -> Result<Option<String>, String> {
    let Some(v) = value
        .map(|v| v.trim_end_matches('/'))
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let Some(rest) = v.strip_prefix('/') else {
        return Err(format!("{v:?} has to start with /"));
    };
    let part_ok = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
    };
    match rest.split('/').all(part_ok) {
        true => Ok(Some(v.to_string())),
        false => Err(format!(
            "{v:?}: each part between slashes has to be letters, digits, '.', '_', '~' or '-', \
             and not empty, '.' or '..'"
        )),
    }
}

/// `app` (routes and layers as they are without a prefix) served under `prefix`: a request
/// for `PREFIX/rest` reaches `app` as `/rest`, `PREFIX` itself is sent to `PREFIX/` (so the
/// page's relative URLs resolve under it), and anything else is not found.
pub fn under(prefix: &str, app: Router) -> Router {
    Router::new()
        .fallback_service(app)
        .layer(axum::middleware::from_fn_with_state(
            Arc::<str>::from(prefix),
            strip,
        ))
}

async fn strip(State(prefix): State<Arc<str>>, mut req: Request, next: Next) -> Response {
    let uri = req.uri();
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    if uri.path() == &*prefix {
        return Redirect::permanent(&format!("{prefix}/{query}")).into_response();
    }
    let Some(rest) = uri
        .path()
        .strip_prefix(&*prefix)
        .filter(|r| r.starts_with('/'))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = format!("{rest}{query}").parse().ok();
    match Uri::from_parts(parts) {
        Ok(stripped) => *req.uri_mut() = stripped,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    }
    next.run(req).await
}

#[cfg(test)]
mod tests;
