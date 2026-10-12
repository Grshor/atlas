//! Theme endpoints: `GET /api/theme` (current Omarchy palette) and the
//! `GET /api/theme/stream` SSE push, plus the background palette watcher.

use super::{join_result, ok_json, ApiError};
use crate::cfg;
use axum::body::{Body, Bytes};
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

// ----------------------------------------------------------------- theme ----

fn read_theme() -> Result<Value, String> {
    read_theme_at(&cfg::theme_file())
}

/// The live theme shared by the watcher and the SSE stream: the last
/// successfully parsed palette (same shape as [`read_theme`] — the watchers
/// only ever replace it wholesale).
static THEME: LazyLock<parking_lot::RwLock<Option<Value>>> =
    LazyLock::new(|| parking_lot::RwLock::new(None));
/// Bumps on every observed change — the SSE push signal.
static THEME_VERSION: AtomicU64 = AtomicU64::new(0);

/// One watcher step, stat-first: reload + bump the version only when the
/// file's mtime moved past `seen` (cheap `stat` per tick; the file is read
/// and parsed on change only). `seen` is updated in place. `Some(version)` =
/// the shared state now carries a fresh palette; a missing/unreadable file
/// keeps the previous one (honest degrade). The mtime IS the clock — tests
/// feed a temp file through this very function.
fn theme_step(path: &Path, seen: &mut f64) -> Option<u64> {
    let mtime = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())?;
    if mtime <= *seen {
        return None;
    }
    let body = read_theme_at(path).ok()?;
    *seen = mtime;
    *THEME.write() = Some(body);
    Some(THEME_VERSION.fetch_add(1, Ordering::SeqCst) + 1)
}

/// Background watcher: the omarchy theme file's mtime every
/// [`THEME_POLL_SECS`]; a change reloads the palette into the shared state
/// and bumps the version the SSE stream pushes on.
pub const THEME_POLL_SECS: u64 = 2;

/// Spawn the theme watcher (once per process, next to `patrol::spawn`).
pub fn spawn_theme_watcher() {
    tokio::spawn(async move {
        let path = cfg::theme_file();
        let mut seen = 0.0f64;
        loop {
            let _ = theme_step(&path, &mut seen);
            tokio::time::sleep(Duration::from_secs(THEME_POLL_SECS)).await;
        }
    });
}

fn read_theme_at(path: &Path) -> Result<Value, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("theme file unavailable: {e}"))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let bytes = std::fs::read(&path).map_err(|e| format!("theme file unavailable: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut palette = Map::new();
    let mut mode: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let mut val = v.trim();
        if let Some(stripped) = val.strip_prefix('"') {
            val = stripped;
        }
        val = val.trim_end();
        if let Some(stripped) = val.strip_suffix('"') {
            val = stripped;
        }
        if key == "mode" {
            mode = Some(val.to_string());
        } else {
            palette.insert(key.to_string(), Value::String(val.to_string()));
        }
    }
    Ok(json!({"mode": mode, "mtime": mtime, "palette": palette}))
}

/// GET /api/theme/stream (SSE): the current palette once on connect
/// (event `theme`, the exact `GET /api/theme` shape `{"mode","mtime",
/// "palette"}`), then the full palette again on every watcher-observed
/// change; a `: keep-alive` comment every [`THEME_HEARTBEAT_SECS`] keeps
/// intermediaries quiet. `GET /api/theme` stays the polling fallback.
pub(crate) async fn api_theme_stream() -> Result<Response, ApiError> {
    let stream = async_stream::stream! {
        let mut seen = 0.0f64;
        let mut last_sent = 0u64;
        let mut beat = Instant::now();
        loop {
            if THEME_VERSION.load(Ordering::SeqCst) == 0 {
                // fresh boot, the watcher's first tick has not landed yet:
                // load inline so connect never waits for it
                let _ = theme_step(&cfg::theme_file(), &mut seen);
            }
            let v = THEME_VERSION.load(Ordering::SeqCst);
            if v != last_sent {
                let body = THEME.read().clone();
                if let Some(body) = body {
                    let frame = format!("event: theme\ndata: {body}\n\n");
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(frame));
                    last_sent = v;
                }
            } else if beat.elapsed() >= Duration::from_secs(THEME_HEARTBEAT_SECS) {
                yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(": keep-alive\n\n"));
                beat = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    };
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap())
}

/// Heartbeat cadence of the theme SSE stream.
pub const THEME_HEARTBEAT_SECS: u64 = 20;

pub(crate) async fn api_theme() -> Result<Response, ApiError> {
    let res = tokio::task::spawn_blocking(read_theme)
        .await
        .map_err(join_result)?
        .map_err(|e| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(ok_json(res))
}


#[cfg(test)]
mod tests {
    use super::*;
    /// The theme watcher's change detection over a temp theme file: first
    /// sight loads, an unchanged mtime does not bump, an overwrite does and
    /// the shared palette carries the new values; a vanished file keeps the
    /// last good palette.
    #[test]
    fn theme_step_bumps_only_on_change() {
        let dir = std::env::temp_dir().join(format!("atlas-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("colors.toml");
        std::fs::write(&path, "mode = \"dark\"\nbackground = \"#111111\"\n").expect("write");

        let mut seen = 0.0f64;
        let v1 = theme_step(&path, &mut seen).expect("first sight loads");
        assert!(v1 >= 1);
        assert!(seen > 0.0, "seen carries the file mtime");
        {
            let t = THEME.read().clone().expect("state loaded");
            assert_eq!(t["palette"]["background"], "#111111");
            assert_eq!(t["mode"], "dark");
        }

        // same mtime → no bump
        assert!(theme_step(&path, &mut seen).is_none(), "unchanged file is a no-op");

        // overwrite → mtime moves → bump; the new palette is live
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "mode = \"light\"\nbackground = \"#eeeeee\"\n").expect("rewrite");
        let v2 = theme_step(&path, &mut seen).expect("bump after overwrite");
        assert!(v2 > v1);
        let t = THEME.read().clone().expect("state loaded");
        assert_eq!(t["palette"]["background"], "#eeeeee");
        assert_eq!(t["mode"], "light");

        // vanished file → honest no-op, the last good palette stays
        std::fs::remove_file(&path).expect("remove");
        assert!(theme_step(&path, &mut seen).is_none());
        assert_eq!(THEME.read().as_ref().unwrap()["mode"], "light");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
