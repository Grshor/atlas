//! Board event bus + `GET /api/events` SSE push. Anything that mutates a
//! board item or a delegation's evidence publishes here; the panel
//! subscribes once and re-reads the board (debounced) instead of waiting for
//! the poll tick — and an open review dialog refetches its run evidence the
//! moment the reviewer's verdict lands. Same wire discipline as the theme
//! stream (`text/event-stream`, `: ping` heartbeat, no buffering).

use super::ApiError;
use axum::body::{Body, Bytes};
use axum::http::{header, HeaderValue};
use std::sync::LazyLock;
use tokio::sync::broadcast;

/// Capacity of the broadcast bus: slow clients drop old events (they
/// re-read the board anyway — an event is a hint, never the payload).
const BUS_CAP: usize = 64;

/// The process-wide event bus. Senders ignore the no-receiver error —
/// publishing is fire-and-forget, nobody subscribed is fine.
static BUS: LazyLock<broadcast::Sender<String>> = LazyLock::new(|| {
    let (tx, _) = broadcast::channel(BUS_CAP);
    tx
});

/// Publish one board event (already a JSON value). Never blocks, never
/// fails the caller: a mutation must not care whether anyone watches.
pub fn publish(v: &serde_json::Value) {
    let _ = BUS.send(format!("data: {v}\n\n"));
}

/// The generic «доска изменилась» hint: any item mutation publishes it; the
/// panel debounces and re-reads the board.
pub fn board_changed() {
    publish(&serde_json::json!({"type": "board"}));
}

/// `GET /api/events` — the SSE stream: `{"type":"hello"}` at once, then
/// every published event verbatim; `: ping` keeps intermediaries quiet.
pub(crate) async fn api_events() -> Result<axum::response::Response, ApiError> {
    let mut rx = BUS.subscribe();
    let stream = async_stream::stream! {
        yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"data: {\"type\":\"hello\"}\n\n"));
        let mut ping = tokio::time::interval(std::time::Duration::from_secs(15));
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ping.tick() => yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b": ping\n\n")),
                msg = rx.recv() => match msg {
                    Ok(s) => yield Ok(Bytes::from(s)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue, // client re-reads the board anyway
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    };
    Ok(axum::http::Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"))
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bus: a published event reaches a subscriber as a ready SSE line;
    /// publishing with no subscribers is a no-op, not an error.
    #[tokio::test]
    async fn bus_delivers_and_never_blocks() {
        publish(&serde_json::json!({"type":"board"})); // no receivers — fine
        let mut rx = BUS.subscribe();
        publish(&serde_json::json!({"type":"run","id":7,"state":"review"}));
        let got = rx.recv().await.unwrap();
        assert!(got.contains("\"type\":\"run\""), "{got}");
        assert!(got.starts_with("data: ") && got.ends_with("\n\n"), "SSE-framed: {got}");
    }

    /// `GET /api/events` speaks SSE: text/event-stream, no-cache — the body
    /// is a live stream, the headers are the contract the panel checks.
    #[tokio::test]
    async fn events_endpoint_speaks_sse() {
        use super::super::support::test_router;
        use tower::ServiceExt;
        let r = test_router();
        let resp = r
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/events")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
    }
}
