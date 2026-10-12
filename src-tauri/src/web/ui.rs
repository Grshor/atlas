//! UI state: `GET /api/ui` — panels, host window, live windows, minimised
//! tabs, GUI presence and port ownership (the frontend's bootstrap view).

use super::{db_lock, join_result, ok_json, PORT_SELF, ApiError};
use crate::gui;
use crate::settings;
use axum::response::Response;
use serde_json::json;

pub(crate) async fn api_ui() -> Result<Response, ApiError> {
    let tabs = tokio::task::spawn_blocking(|| settings::tabs(&db_lock()))
        .await
        .map_err(join_result)?;
    Ok(ok_json(json!({
        "panels": settings::PANELS,
        "host": settings::HOST,
        "windows": gui::live(),
        "tabs": tabs,
        "gui": gui::app().is_some(),
        "port_owner": if PORT_SELF.load(std::sync::atomic::Ordering::Relaxed) {
            "self"
        } else {
            "other"
        },
    })))
}
