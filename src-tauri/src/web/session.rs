//! Session endpoints: `GET /api/session` (last omp session of a project,
//! tail view) and `GET /api/session/stream` (SSE tail of new session events).

use super::{db_lock, join_result, ok_json, query_param, ApiError};
use crate::importers;
use crate::util::norm;
use axum::body::{Body, Bytes};
use axum::http::{header, StatusCode, Uri};
use axum::response::Response;
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};


// -------------------------------------------------------------- session ----


pub(crate) async fn api_session(uri: Uri) -> Result<Response, ApiError> {
    let project = query_param(&uri, "project")
        .ok_or_else(|| ApiError::unprocessable("query project required"))?;
    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        let name = norm(&project);
        let known: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM projects WHERE lower(replace(name,'ё','е')) = ?1",
                [&name],
                |r| r.get(0),
            )
            .ok();
        if known.is_none() {
            return Err(ApiError::not_found(format!(
                "unknown project '{}'",
                project
            )));
        }
        let f = crate::db::latest_session_file(&conn, &project);
        drop(conn);
        match f {
            Some(f) if Path::new(&f).exists() => {
                let events = importers::parse_session_events(&f, 400);
                let n = events.len();
                Ok(json!({"file": f, "events": events[n.saturating_sub(40)..]}))
            }
            _ => Ok(json!({"file": Value::Null, "events": []})),
        }
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}

pub(crate) async fn api_session_stream(uri: Uri) -> Result<Response, ApiError> {
    let project = query_param(&uri, "project")
        .ok_or_else(|| ApiError::unprocessable("query project required"))?;
    let project_for_lookup = project.clone();
    let f = tokio::task::spawn_blocking(move || -> Result<Option<String>, ApiError> {
        let conn = db_lock();
        Ok(crate::db::latest_session_file(&conn, &project_for_lookup))
    })
    .await
    .map_err(join_result)??;
    let Some(f) = f.filter(|f| Path::new(f).exists()) else {
        return Err(ApiError::not_found(format!(
            "no omp session for project '{}'",
            project
        )));
    };
    let stream = async_stream::stream! {
        let mut pos = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        let mut buf: Vec<u8> = Vec::new();
        let mut last_beat = Instant::now();
        loop {
            // stream only newly appended events (server.py gen())
            if let Ok(size) = std::fs::metadata(&f).map(|m| m.len()) {
                if size < pos {
                    pos = 0;
                    buf.clear();
                }
                if size > pos {
                    if let Ok(mut fh) = std::fs::File::open(&f) {
                        use std::io::{Read, Seek, SeekFrom};
                        if fh.seek(SeekFrom::Start(pos)).is_ok() {
                            let mut chunk = Vec::with_capacity((size - pos) as usize);
                            match fh.read_to_end(&mut chunk) {
                                Ok(n) => {
                                    pos += n as u64;
                                    buf.extend_from_slice(&chunk);
                                    let fallback = std::fs::metadata(&f).ok().and_then(|m| m.modified().ok())
                                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                        .map(|d| d.as_secs_f64()).unwrap_or(0.0);
                                    let mut start = 0usize;
                                    for i in 0..buf.len() {
                                        if buf[i] == b'\n' {
                                            let line = &buf[start..i];
                                            start = i + 1;
                                            let line = String::from_utf8_lossy(line);
                                            let line = line.trim();
                                            if line.is_empty() { continue; }
                                            let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
                                            for ev in importers::parse_transcript_record(&d, fallback) {
                                                let frame = format!("data: {ev}\n\n");
                                                yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(frame));
                                            }
                                        }
                                    }
                                    buf.drain(..start);
                                }
                                Err(_) => {}
                            }
                        }
                    }
                }
            }
            if last_beat.elapsed() >= Duration::from_secs(15) {
                yield Ok(Bytes::from_static(b": ping\n\n"));
                last_beat = Instant::now();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
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
