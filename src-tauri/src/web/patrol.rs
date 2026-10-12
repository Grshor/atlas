//! Patrol digest: `GET /api/patrol` — schedule + last run digest.

use super::{join_result, ok_json, ApiError};
use crate::patrol;
use axum::response::Response;

// ---------------------------------------------------------------- patrol ---

/// GET /api/patrol — schedule + the last digest (`null` when none ran yet).
pub(crate) async fn api_patrol() -> Result<Response, ApiError> {
    let out = tokio::task::spawn_blocking(patrol::status)
        .await
        .map_err(join_result)?;
    Ok(ok_json(out))
}

