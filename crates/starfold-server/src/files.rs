//! File transfer over plain HTTP, next to the stream: list a directory, download a
//! file or a folder (zipped on the fly), upload files, folders included. Everything is
//! confined to the folder given with `--files-root` (no folder, no file transfer), and
//! symlinks are never followed (see [`beneath`]). Transfers use their own connections, so
//! a large upload or download never queues behind (or in front of) video frames.

pub mod beneath;
#[cfg(all(test, unix))]
mod tests;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use beneath::{Dir, Kind, Opened, Refused, Root};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Files {
    /// None: no `--files-root`, so no file transfer.
    pub root: Option<Root>,
    pub upload: bool,
    pub download: bool,
    /// Directory (relative to root) the client opens first and uploads into by default.
    /// Created on the first upload if it does not exist yet.
    pub default_dir: String,
    /// 0 = unlimited.
    pub max_upload_bytes: u64,
}

/// Uploads must carry this header. Browsers only send custom headers cross-origin
/// after a CORS preflight, which we never answer, so other sites can't upload.
pub const UPLOAD_HEADER: &str = "x-voxl-upload";

type Q = Query<HashMap<String, String>>;

fn err(code: StatusCode, msg: &str) -> Response {
    (
        code,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "error": msg }).to_string(),
    )
        .into_response()
}

/// A client path ("Downloads/report.pdf") as plain names under the root: absolute paths,
/// `..` and NULs are rejected.
// The error is the HTTP response to send, as is usual with axum.
#[allow(clippy::result_large_err)]
fn parts(rel: &str) -> Result<Vec<&str>, Response> {
    let bad = || err(StatusCode::BAD_REQUEST, "invalid path");
    if rel.contains('\0') {
        return Err(bad());
    }
    let mut v = Vec::new();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => v.push(s.to_str().ok_or_else(bad)?),
            Component::CurDir => {}
            _ => return Err(bad()),
        }
    }
    Ok(v)
}

fn refused(r: Refused) -> Response {
    match r {
        Refused::Missing => err(StatusCode::NOT_FOUND, "not found"),
        Refused::NotFolder => err(StatusCode::NOT_FOUND, "not a folder"),
        Refused::Link => err(StatusCode::FORBIDDEN, "symlinks are not followed"),
        Refused::Special => err(StatusCode::BAD_REQUEST, "not a file"),
        Refused::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            err(StatusCode::FORBIDDEN, "permission denied")
        }
        Refused::Io(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Runs blocking filesystem work off the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Response> + Send + 'static,
) -> Result<T, Response> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|_| Err(err(StatusCode::INTERNAL_SERVER_ERROR, "file task failed")))
}

pub async fn list(State(f): State<Arc<Files>>, Query(q): Q) -> Response {
    if !f.download && !f.upload || f.root.is_none() {
        return err(StatusCode::FORBIDDEN, "file transfer is disabled");
    }
    let rel = q.get("path").cloned().unwrap_or_default();
    let listed = blocking(move || {
        let root = f.root.as_ref().expect("checked");
        let parts = parts(&rel)?;
        let dir = match root.dir(&parts, false) {
            Ok(d) => d,
            // The default upload folder may not exist until something is uploaded.
            Err(Refused::Missing) if rel == f.default_dir => return Ok((parts.join("/"), vec![])),
            Err(r) => return Err(refused(r)),
        };
        let mut entries = Vec::new();
        // Symlinks are left out: they are never followed.
        for e in dir.entries().map_err(|e| refused(Refused::Io(e)))? {
            let name = e.name.to_string_lossy().into_owned();
            if name.starts_with('.') || !matches!(e.kind, Kind::Dir | Kind::File) {
                continue;
            }
            let mtime = e
                .modified
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            entries.push(serde_json::json!({ "name": name, "dir": e.kind == Kind::Dir, "size": e.len, "mtime": mtime }));
            if entries.len() >= 5000 {
                break;
            }
        }
        Ok((parts.join("/"), entries))
    })
    .await;
    let (path, entries) = match listed {
        Ok(l) => l,
        Err(e) => return e,
    };
    let body = serde_json::json!({ "path": path, "entries": entries });
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body.to_string(),
    )
        .into_response()
}

pub async fn download(State(f): State<Arc<Files>>, Query(q): Q) -> Response {
    if !f.download || f.root.is_none() {
        return err(StatusCode::FORBIDDEN, "downloads are disabled");
    }
    let rel = q.get("path").cloned().unwrap_or_default();
    let opened = blocking(move || {
        let root = f.root.as_ref().expect("checked");
        let parts = parts(&rel)?;
        let Some((last, parent)) = parts.split_last() else {
            let top = root.dir(&[], false).map_err(refused)?;
            return Ok(("Home".to_string(), Opened::Dir(top)));
        };
        let opened = root
            .dir(parent, false)
            .and_then(|d| d.open(OsStr::new(last)))
            .map_err(refused)?;
        Ok((last.to_string(), opened))
    })
    .await;
    let (name, file) = match opened {
        Ok((name, Opened::Dir(d))) => return zip_folder(d, name),
        Ok((name, Opened::File(file))) => (name, tokio::fs::File::from_std(file)),
        Err(e) => return e,
    };
    let Ok(md) = file.metadata().await else {
        return err(StatusCode::NOT_FOUND, "not found");
    };
    let stream = futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buf = Vec::with_capacity(256 << 10);
        match file.read_buf(&mut buf).await {
            Ok(0) => None,
            Ok(_) => Some((Ok::<_, std::io::Error>(bytes::Bytes::from(buf)), Some(file))),
            Err(e) => Some((Err(e), None)),
        }
    });
    let mut h = attachment(&name, "application/octet-stream");
    h.insert(header::CONTENT_LENGTH, md.len().into());
    (h, Body::from_stream(stream)).into_response()
}

/// Headers for a download saved as `name`.
fn attachment(name: &str, content_type: &'static str) -> HeaderMap {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let disposition = format!(
        "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
        pct_encode(name)
    );
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    if let Ok(v) = disposition.parse() {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h
}

/// Streams a folder as a zip, built while it downloads: nothing is staged on disk and
/// memory stays at a few chunks however big the folder is. Symlinks are left out (they
/// could lead outside the shared folder, or loop).
fn zip_folder(dir: Dir, name: String) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(4);
    let top = name.clone();
    tokio::task::spawn_blocking(move || {
        let out = ChunkWriter {
            tx: tx.clone(),
            buf: Vec::with_capacity(CHUNK),
        };
        if let Err(e) = write_zip(dir, &top, out) {
            log::warn!("zipping {top}: {e}");
            // Ends the response with an error, so the browser marks the download failed.
            let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
        }
    });
    let stream =
        futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|c| (c, rx)) });
    (
        attachment(&format!("{name}.zip"), "application/zip"),
        Body::from_stream(stream),
    )
        .into_response()
}

const CHUNK: usize = 256 << 10;

/// Hands the zip to the HTTP response in chunks; blocks while the client catches up.
struct ChunkWriter {
    tx: tokio::sync::mpsc::Sender<std::io::Result<bytes::Bytes>>,
    buf: Vec<u8>,
}

impl ChunkWriter {
    fn send(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK));
        self.tx
            .blocking_send(Ok(chunk.into()))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "download cancelled"))
    }
}

impl std::io::Write for ChunkWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.send()?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.send()
    }
}

impl Drop for ChunkWriter {
    fn drop(&mut self) {
        let _ = self.send();
    }
}

/// Formats that are already compressed: stored as they are rather than deflated again.
const COMPRESSED: &[&str] = &[
    "zip", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "br", "jar", "whl", "apk", "dmg", "iso",
    "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "mp3", "m4a", "aac", "ogg", "opus",
    "flac", "mp4", "m4v", "mov", "mkv", "webm", "avi", "woff", "woff2", "pdf", "docx", "xlsx",
    "pptx",
];

/// Zip time (local fields, no zone) for a modification time, read as UTC.
fn zip_time(t: std::time::SystemTime) -> Option<zip::DateTime> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as u16;
    zip::DateTime::from_date_and_time(
        year,
        month,
        day,
        (rem / 3600) as u8,
        (rem % 3600 / 60) as u8,
        (rem % 60) as u8,
    )
    .ok()
}

fn write_zip(dir: Dir, top: &str, out: ChunkWriter) -> zip::result::ZipResult<()> {
    use zip::write::SimpleFileOptions;
    let mut z = zip::ZipWriter::new_stream(out);
    let base = SimpleFileOptions::default();
    let mut todo = vec![(dir, top.to_string())];
    while let Some((dir, rel)) = todo.pop() {
        z.add_directory(format!("{rel}/"), base)?;
        let mut entries = match dir.entries() {
            Ok(v) => v,
            // Unreadable folders are left empty rather than failing the whole download.
            Err(_) => continue,
        };
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        // Pushed in reverse so folders come out in name order.
        let mut dirs = Vec::new();
        for e in entries {
            let name = e.name.to_string_lossy().into_owned();
            let rel = format!("{rel}/{name}");
            let mut file = match e.kind {
                Kind::Dir | Kind::File => match dir.open(&e.name) {
                    Ok(Opened::Dir(d)) => {
                        dirs.push((d, rel));
                        continue;
                    }
                    Ok(Opened::File(f)) => f,
                    Err(_) => continue,
                },
                Kind::Link | Kind::Other => continue,
            };
            let Ok(md) = file.metadata() else {
                continue;
            };
            let stored = Path::new(&name)
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| COMPRESSED.contains(&x.to_ascii_lowercase().as_str()));
            let mut opts = base.large_file(md.len() >= u32::MAX as u64);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                opts = opts.unix_permissions(md.permissions().mode() & 0o777);
            }
            opts = if stored {
                opts.compression_method(zip::CompressionMethod::Stored)
            } else {
                // Fastest level: the VM's CPU is busy streaming the desktop.
                opts.compression_method(zip::CompressionMethod::Deflated)
                    .compression_level(Some(1))
            };
            if let Some(t) = md.modified().ok().and_then(zip_time) {
                opts = opts.last_modified_time(t);
            }
            z.start_file(rel, opts)?;
            std::io::copy(&mut file, &mut z)?;
        }
        todo.extend(dirs.into_iter().rev());
    }
    z.finish()?;
    Ok(())
}

fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A plain file name: no separators, no `.`/`..`, no control characters.
fn clean_name(name: &str) -> Option<&str> {
    let name = name.trim();
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control());
    ok.then_some(name)
}

/// `name`, or `name (1).ext`, `name (2).ext`, ... if taken.
fn unique(dir: &Dir, name: &str) -> String {
    if !dir.exists(OsStr::new(name)) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| format!("{stem} ({n}){ext}"))
        .find(|p| !dir.exists(OsStr::new(p)))
        .unwrap()
}

static UPLOADS: AtomicU64 = AtomicU64::new(0);

pub async fn upload(
    State(f): State<Arc<Files>>,
    Query(q): Q,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !f.upload || f.root.is_none() {
        return err(StatusCode::FORBIDDEN, "uploads are disabled");
    }
    if !headers.contains_key(UPLOAD_HEADER) {
        return err(StatusCode::FORBIDDEN, "missing upload header");
    }
    let Some(name) = q.get("name").and_then(|n| clean_name(n)) else {
        return err(StatusCode::BAD_REQUEST, "invalid file name");
    };
    let rel = q
        .get("path")
        .cloned()
        .unwrap_or_else(|| f.default_dir.clone());
    let sub = q.get("sub").cloned().unwrap_or_default();
    let name = name.to_string();
    // Write to a hidden temporary file and rename once complete, so nothing in the
    // VM sees (or opens) a half-uploaded file.
    let part: OsString = format!(
        ".{name}.{}-{}.part",
        std::process::id(),
        UPLOADS.fetch_add(1, Ordering::Relaxed)
    )
    .into();
    let fs = f.clone();
    let part2 = part.clone();
    let opened = blocking(move || {
        let root = fs.root.as_ref().expect("checked");
        let mut path = parts(&rel)?;
        // The default upload folder is made on the first upload.
        let mut dir = root.dir(&path, rel == fs.default_dir).map_err(refused)?;
        // Folder uploads: `sub` is the file's folder inside the upload ("Photos/2024"),
        // created as needed. Folders that already exist are merged into.
        for p in sub.split('/').filter(|p| !p.is_empty()) {
            let Some(p) = clean_name(p) else {
                return Err(err(StatusCode::BAD_REQUEST, "invalid folder name"));
            };
            dir = match dir.child(OsStr::new(p), true) {
                Err(Refused::NotFolder) => {
                    return Err(err(
                        StatusCode::CONFLICT,
                        "a file is in the way of the folder",
                    ))
                }
                d => d.map_err(refused)?,
            };
            path.push(p);
        }
        let out = dir.create_new(&part2).map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("cannot write: {e}"),
            )
        })?;
        let path: Vec<String> = path.into_iter().map(String::from).collect();
        Ok((dir, path, out))
    })
    .await;
    let (dir, path, out) = match opened {
        Ok(o) => o,
        Err(e) => return e,
    };
    let mut out = tokio::fs::File::from_std(out);
    let mut written: u64 = 0;
    let mut stream = body.into_data_stream();
    let result: Result<(), Response> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| err(StatusCode::BAD_REQUEST, "upload interrupted"))?;
            written += chunk.len() as u64;
            if f.max_upload_bytes > 0 && written > f.max_upload_bytes {
                return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "file is too large"));
            }
            out.write_all(&chunk).await.map_err(|e| {
                err(
                    StatusCode::INSUFFICIENT_STORAGE,
                    &format!("cannot write: {e}"),
                )
            })?;
        }
        out.flush().await.map_err(|e| {
            err(
                StatusCode::INSUFFICIENT_STORAGE,
                &format!("cannot write: {e}"),
            )
        })?;
        Ok(())
    }
    .await;
    drop(out);
    let saved = blocking(move || {
        if let Err(e) = result {
            let _ = dir.remove(&part);
            return Err(e);
        }
        let dest = unique(&dir, &name);
        if let Err(e) = dir.rename(&part, OsStr::new(&dest)) {
            let _ = dir.remove(&part);
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("cannot save: {e}"),
            ));
        }
        Ok(path.into_iter().chain([dest]).collect::<Vec<_>>().join("/"))
    })
    .await;
    let dest = match saved {
        Ok(d) => d,
        Err(e) => return e,
    };
    log::info!("uploaded {dest} ({written} bytes)");
    let body = serde_json::json!({ "path": dest, "size": written });
    (
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}
