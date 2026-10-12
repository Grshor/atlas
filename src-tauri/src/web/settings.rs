//! UI settings: `GET|PATCH /api/settings` — global defaults plus per-window
//! scopes over the frozen panel registry.

use super::{db_lock, join_result, ok_json, query_param, ApiError};
use crate::settings;
use axum::http::Uri;
use axum::response::Response;
use serde_json::Value;


/// Settings scope errors are 422 (the ticket's contract): the labels a caller
/// may scope to are the panels. `/api/windows/*` keeps its own 404.
fn win_unknown_settings(win: &str) -> ApiError {
    ApiError::unprocessable(format!(
        "неизвестная метка окна '{win}'; панели: {}",
        settings::PANELS.join(", ")
    ))
}

pub(crate) async fn api_settings_get(uri: Uri) -> Result<Response, ApiError> {
    let win = query_param(&uri, "win");
    if let Some(w) = &win {
        if !settings::PANELS.contains(&w.as_str()) {
            return Err(win_unknown_settings(w));
        }
    }
    let out = tokio::task::spawn_blocking(move || {
        settings::effective(&db_lock(), win.as_deref())
    })
    .await
    .map_err(join_result)?;
    Ok(ok_json(out))
}

pub(crate) async fn api_settings_patch(body: String) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| ApiError::unprocessable(format!("invalid JSON body: {e}")))?;
    let Some(obj) = v.as_object() else {
        return Err(ApiError::unprocessable(
            "body must be a flat JSON object of settings",
        ));
    };
    // `win` selects the scope: absent = global, a panel label = that window's
    // override (null value = clear the override so the global shows through).
    let win: Option<String> = match obj.get("win") {
        None => None,
        Some(Value::String(w)) => {
            if !settings::PANELS.contains(&w.as_str()) {
                return Err(win_unknown_settings(w));
            }
            Some(w.clone())
        }
        Some(_) => return Err(ApiError::unprocessable("win must be a string")),
    };
    // Validate everything first: a single bad key aborts the whole patch.
    let mut updates: Vec<(String, Option<Value>)> = Vec::new();
    for (k, val) in obj {
        if k == "win" {
            continue;
        }
        if val.is_null() {
            if win.is_none() {
                return Err(ApiError::unprocessable(format!(
                    "'{k}': null допустим только с \"win\" (сброс переопределения)"
                )));
            }
            updates.push((k.clone(), None));
            continue;
        }
        let normalized = settings::validate(k, val).map_err(ApiError::unprocessable)?;
        updates.push((k.clone(), Some(normalized)));
    }
    if updates.is_empty() {
        return Err(ApiError::unprocessable(
            "body must contain at least one setting key",
        ));
    }
    let out = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let conn = db_lock();
        for (k, val) in &updates {
            let res = match (val, win.as_deref()) {
                (Some(v), Some(w)) => settings::store_scoped(&conn, w, k, v),
                (Some(v), None) => settings::store(&conn, k, v),
                (None, Some(w)) => settings::clear_scoped(&conn, w, k),
                (None, None) => unreachable!("guarded above"),
            };
            res.map_err(|e| ApiError::bad(format!("{e}")))?;
        }
        Ok(settings::effective(&conn, win.as_deref()))
    })
    .await
    .map_err(join_result)??;
    Ok(ok_json(out))
}


#[cfg(test)]
mod tests {
    use crate::http::web::support::{call, test_router};
    use axum::http::StatusCode;
    use serde_json::json;
    /// The window-scope contract end to end: global write, per-window
    /// override, effective read, clear, and every rejection (422 names the
    /// offender; an unknown window label is 422 here, unlike /api/windows/*).
    #[tokio::test]
    async fn settings_scope_contract() {
        let r = test_router();
        // plumbing: the removed «Настройки» panel is gone from the registry
        let (st, ui) = call(&r, "GET", "/api/ui", None).await;
        assert_eq!(st, StatusCode::OK);
        let panels: Vec<String> = ui["panels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert_eq!(panels.len(), 9);
        assert!(!panels.contains(&"settings".to_string()));
        assert!(panels.contains(&"orchestrator".to_string()));

        // global write, no window scope
        let (st, body) = call(
            &r,
            "PATCH",
            "/api/settings",
            Some(r#"{"ui.statusbar":"hover"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["global"]["ui.statusbar"], "hover");
        assert_eq!(body["settings"]["ui.statusbar"], "hover");
        assert_eq!(body["windows"], json!({}));

        // per-window override: effective for that window, global untouched
        let (st, body) = call(
            &r,
            "PATCH",
            "/api/settings",
            Some(r#"{"ui.statusbar":"hidden","ui.search_k":7,"win":"board"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["settings"]["ui.statusbar"], "hidden");
        assert_eq!(body["settings"]["ui.search_k"], 7);
        assert_eq!(body["global"]["ui.statusbar"], "hover");
        assert_eq!(body["global"]["ui.search_k"], 20);
        assert_eq!(body["windows"]["board"]["ui.statusbar"], "hidden");

        // GET ?win=board merges; another window sees the global values
        let (st, body) = call(&r, "GET", "/api/settings?win=board", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body["settings"]["ui.statusbar"], "hidden");
        assert_eq!(body["global"]["ui.statusbar"], "hover");
        let (_, body) = call(&r, "GET", "/api/settings?win=search", None).await;
        assert_eq!(body["settings"]["ui.statusbar"], "hover");
        assert_eq!(body["settings"]["ui.search_k"], 20);

        // clearing the overrides gives the global values back, window drops out
        let (st, body) = call(
            &r,
            "PATCH",
            "/api/settings",
            Some(r#"{"ui.statusbar":null,"ui.search_k":null,"win":"board"}"#),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["settings"]["ui.statusbar"], "hover");
        assert_eq!(body["settings"]["ui.search_k"], 20);
        assert_eq!(body["windows"], json!({}));

        // rejections: unknown key, bad value, unknown/null-less win, empty body
        for (body, want) in [
            (r#"{"ui.nokey":1}"#, "ui.nokey"),
            (r#"{"ui.statusbar":"sometimes"}"#, "ui.statusbar"),
            (r#"{"ui.statusbar":"hidden","win":"settings"}"#, "settings"),
            (r#"{"ui.statusbar":"hidden","win":"nope"}"#, "nope"),
            (r#"{"ui.statusbar":null}"#, "ui.statusbar"),
            (r#"{"win":"board"}"#, "at least one setting key"),
        ] {
            let (st, out) = call(&r, "PATCH", "/api/settings", Some(body)).await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body} → {out}");
            let detail = out["detail"].as_str().unwrap_or_default();
            assert!(detail.contains(want), "{body} → {detail}");
        }
        let (st, out) = call(&r, "GET", "/api/settings?win=settings", None).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{out}");

        // ...while the window endpoints keep their own 404 contract
        let (st, out) = call(
            &r,
            "POST",
            "/api/windows/minimize",
            Some(r#"{"win":"settings","dry_run":true}"#),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{out}");
    }
}
