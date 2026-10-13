//! Static frontend: the rust-embedded `../static` assets plus the
//! disk-first serving layer and the change watcher behind it. When the repo
//! checkout exists next to the binary (`cfg::atlas_dir()/static`), every file
//! is served FROM DISK — JS/CSS fixes reach the running panel without a
//! rebuild/restart; without a checkout (deployment) the handler falls back to
//! the embedded build-time copy. The watcher polls the static tree's max
//! mtime (same cadence as the theme watcher) and publishes an SSE `assets`
//! event on change so the panel can offer a manual reload.

use crate::cfg;
use axum::body::Bytes;
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rust_embed::RustEmbed;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Duration;


#[derive(RustEmbed)]
#[folder = "../static"]
struct Assets;

/// The source tree this binary was built from: `<repo>/static` (the parent
/// of src-tauri, compile-time). Missing in a bare deployment → embedded copy.
fn static_root() -> PathBuf {
    cfg::atlas_dir().join("static")
}

/// Per-request disk cache: resolved path → (mtime, bytes). A repeat request
/// pays one `stat` instead of a re-read while the file is unchanged. Paths
/// are bounded by the static tree itself; files here are small panel assets.
static DISK_CACHE: LazyLock<parking_lot::Mutex<HashMap<PathBuf, (f64, Bytes)>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Bumps on every watcher-observed change to the static tree — the SSE push
/// signal (the event itself carries no payload, same discipline as `board`).
static ASSETS_VERSION: AtomicU64 = AtomicU64::new(0);

fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

/// The disk copy of `rel` under `root`, or None when anything is off (root
/// missing, path escapes the tree, not a regular file, unreadable) — the
/// caller falls back to the embedded copy. `..` segments are rejected before
/// they ever reach the filesystem.
fn disk_file(root: &Path, rel: &str) -> Option<Bytes> {
    if rel.split('/').any(|c| c == "..") {
        return None;
    }
    let full = root.join(rel);
    let meta = std::fs::metadata(&full).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    {
        let cache = DISK_CACHE.lock();
        if let Some((seen, bytes)) = cache.get(&full) {
            if *seen == mtime {
                return Some(bytes.clone());
            }
        }
    }
    let bytes = Bytes::from(std::fs::read(&full).ok()?);
    DISK_CACHE.lock().insert(full, (mtime, bytes.clone()));
    Some(bytes)
}

pub(crate) async fn static_handler(method: Method, uri: Uri) -> Response {
    static_handler_in(&static_root(), method, uri)
}

/// The serving decision, with the disk root injected for tests: disk first,
/// then the build-time embed, then the frozen 404 (method check up front,
/// unchanged).
fn static_handler_in(root: &Path, method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed").into_response();
    }
    let path = uri.path();
    let path = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    let body = disk_file(root, path).or_else(|| {
        use std::borrow::Cow;
        Assets::get(path).map(|f| match f.data {
            Cow::Borrowed(b) => Bytes::from_static(b),
            Cow::Owned(v) => Bytes::from(v),
        })
    });
    match body {
        Some(data) => {
            let ct = content_type(path);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, HeaderValue::from_static(ct))],
                data,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response(),
    }
}

// --------------------------------------------------------------- watcher ----

/// Max mtime over the whole static tree (recursive). `None` = the tree is
/// missing/unreadable — the watcher keeps the previous `seen` (honest
/// degrade, same as the theme watcher on a vanished file).
fn walk_max_mtime(dir: &Path) -> Option<f64> {
    let mut max: Option<f64> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let ft = entry.file_type().ok()?;
        let m = if ft.is_dir() {
            walk_max_mtime(&entry.path())?
        } else if ft.is_file() {
            entry
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs_f64()
        } else {
            continue;
        };
        max = Some(max.map_or(m, |seen| seen.max(m)));
    }
    max
}

/// One watcher step over the static tree: bump + publish only when the max
/// mtime moved past `seen` (one recursive walk per tick — the tree is a few
/// dozen small files). `seen` is updated in place. `Some(version)` = the
/// `{"type":"assets"}` hint went out on the event bus.
fn assets_step(root: &Path, seen: &mut f64) -> Option<u64> {
    let max = walk_max_mtime(root)?;
    if max <= *seen {
        return None;
    }
    *seen = max;
    let v = ASSETS_VERSION.fetch_add(1, Ordering::SeqCst) + 1;
    log::info!("static assets changed on disk (v{v}) — panel may reload");
    super::events::publish(&json!({"type": "assets"}));
    Some(v)
}

/// Watcher cadence — same as the theme watcher.
pub const ASSETS_POLL_SECS: u64 = 2;

/// Spawn the assets watcher (once per process, next to `spawn_theme_watcher`).
/// The initial max mtime is recorded BEFORE the loop, so a quiet boot never
/// fires — only real edits after start do.
pub fn spawn_assets_watcher() {
    tokio::spawn(async move {
        let root = static_root();
        let mut seen = walk_max_mtime(&root).unwrap_or(0.0);
        loop {
            let _ = assets_step(&root, &mut seen);
            tokio::time::sleep(Duration::from_secs(ASSETS_POLL_SECS)).await;
        }
    });
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Throwaway static tree with one js file (stdlib only, like the theme
    /// tests); returns (root, file path).
    fn temp_static(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("atlas-assets-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("hot.js");
        std::fs::write(&file, "// disk copy\n").expect("write");
        (dir, file)
    }

    /// Change detection over the tree: the initial walk is the boot baseline
    /// (no fire), an unchanged tree is a no-op, a new/edited file bumps the
    /// version, a vanished tree is an honest no-op.
    #[test]
    fn assets_step_bumps_only_on_change() {
        let (root, file) = temp_static("step");

        let mut seen = walk_max_mtime(&root).expect("initial walk");
        assert!(seen > 0.0);
        assert!(assets_step(&root, &mut seen).is_none(), "boot baseline must not fire");

        assert!(assets_step(&root, &mut seen).is_none(), "unchanged tree is a no-op");

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, "// edited\n").expect("rewrite");
        let v = assets_step(&root, &mut seen).expect("bump after edit");
        assert!(v >= 1);
        assert!(assets_step(&root, &mut seen).is_none(), "same mtime again is a no-op");

        // nested file counts too (the tree is walked recursively)
        std::fs::create_dir_all(root.join("js")).expect("subdir");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("js/nested.js"), "// new\n").expect("write");
        assert!(assets_step(&root, &mut seen).is_some(), "nested edit must bump");

        // vanished tree → honest no-op, seen keeps the last value
        std::fs::remove_dir_all(&root).expect("remove");
        assert!(assets_step(&root, &mut seen).is_none());
    }

    /// The serving decision runs through a real Response — collect its
    /// in-memory body with axum's own helper.
    async fn body_bytes(res: Response) -> Vec<u8> {
        axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .expect("body")
            .to_vec()
    }

    /// Disk-first serving with an injected root: a file that exists on disk
    /// is served from there even when it differs from the embed; `..` never
    /// reaches the filesystem; a missing disk file falls through to the
    /// embedded copy; unknown paths stay 404; the method check is untouched.
    #[tokio::test]
    async fn static_handler_serves_disk_first() {
        let (root, _file) = temp_static("serve");
        std::fs::write(root.join("index.html"), "<html>disk</html>").expect("write");

        // differs from the embedded copy → the DISK bytes win
        let res = static_handler_in(&root, Method::GET, Uri::from_static("/hot.js"));
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_bytes(res).await, b"// disk copy\n".to_vec());

        // "/" maps to index.html (unchanged), served from disk
        let res = static_handler_in(&root, Method::GET, Uri::from_static("/"));
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_bytes(res).await, b"<html>disk</html>".to_vec());

        // traversal is rejected before the filesystem, embed misses it → 404
        let res = static_handler_in(&root, Method::GET, Uri::from_static("/../Cargo.toml"));
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // no such file on disk → embedded fallback still answers
        let res = static_handler_in(&root, Method::GET, Uri::from_static("/js/board.js"));
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!body_bytes(res).await.is_empty());

        // unknown path → frozen 404 shape
        let res = static_handler_in(&root, Method::GET, Uri::from_static("/nope.js"));
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // method check untouched
        let res = static_handler_in(&root, Method::POST, Uri::from_static("/hot.js"));
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);

        // empty root (deployment without a checkout): embed serves, unknown 404s
        let empty = root.join("absent");
        let res = static_handler_in(&empty, Method::GET, Uri::from_static("/js/board.js"));
        assert_eq!(res.status(), StatusCode::OK);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The disk cache answers repeat requests from the mtime-matched entry:
    /// editing the file moves its mtime and the NEXT request serves the new
    /// bytes (no stale copy — the whole point of hot reload).
    #[test]
    fn disk_cache_honors_mtime() {
        let (root, file) = temp_static("cache");
        let first = disk_file(&root, "hot.js").expect("first read");
        assert_eq!(first, Bytes::from_static(b"// disk copy\n"));
        assert_eq!(disk_file(&root, "hot.js").expect("cached"), first);

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, "// disk copy v2\n").expect("rewrite");
        let second = disk_file(&root, "hot.js").expect("re-read after edit");
        assert_eq!(second, Bytes::from_static(b"// disk copy v2\n"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
