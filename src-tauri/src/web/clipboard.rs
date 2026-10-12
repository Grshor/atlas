//! Clipboard handoff: `POST /api/clipboard/image` — raw image bytes (≤25 МБ)
//! land in `~/.local/share/atlas/paste/`; the terminal inserts the path.

use super::{join_result, ok_json, ApiError};
use axum::response::Response;
use serde_json::json;

/// POST /api/clipboard/image — RAW image bytes in the body (content-type
/// carries the type) → saved as `<data_dir>/paste/<millis>.<ext>`. Answer:
/// `{ok:true,path}` — the terminal panel pastes a screenshot as this PATH, so
/// an agent can open the file instead of a clipboard it cannot see.
pub(crate) async fn api_clipboard_image(
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    const MAX: usize = 25 * 1024 * 1024;
    if body.is_empty() {
        return Err(ApiError::unprocessable("пустое тело: жду байты изображения"));
    }
    if body.len() > MAX {
        return Err(ApiError::unprocessable("изображение больше 25 МБ"));
    }
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ext = if ct.contains("png") || body.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if ct.contains("jpeg") || ct.contains("jpg") || body.starts_with(b"\xff\xd8\xff") {
        "jpg"
    } else if ct.contains("webp") || body.starts_with(b"RIFF") {
        "webp"
    } else if ct.contains("gif") || body.starts_with(b"GIF8") {
        "gif"
    } else {
        return Err(ApiError::unprocessable(
            "жду изображение (png/jpeg/webp/gif) — по content-type или магическим байтам",
        ));
    };
    let dir = crate::cfg::data_dir().join("paste");
    let name = format!("{}.{ext}", (crate::util::now() * 1000.0) as u64);
    let len = body.len();
    let wrote = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(&name);
        std::fs::write(&path, &body)?;
        Ok::<std::path::PathBuf, std::io::Error>(path)
    })
    .await
    .map_err(join_result)?
    .map_err(|e| ApiError::bad(format!("не удалось сохранить: {e}")))?;
    log::info!("clipboard: image saved {}", wrote.display());
    Ok(ok_json(json!({
        "ok": true,
        "path": wrote.to_string_lossy(),
        "bytes": len,
    })))
}
