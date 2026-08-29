//! SSE framing of the shared event bus.
//!
//! Each bus event becomes one SSE frame: the event name from
//! [`koharu_app::core::events::Event`]'s `Display` and the full tagged JSON
//! as the data payload, so clients can filter on `event:` while still
//! receiving the discriminated union verbatim.

use axum::extract::State;
use axum::response::Sse;
use axum::response::sse::{Event as SseFrame, KeepAlive};
use futures::{Stream, StreamExt as _};
use koharu_app::SharedApp;
use koharu_app::core::events::Event as AppEvent;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

/// `GET /api/v1/events` — the process-wide event stream.
///
/// Subscribes immediately (events published before the handler runs are not
/// replayed; clients resynchronize through the full-state endpoints). A
/// subscriber that falls behind the broadcast buffer receives a synthetic
/// `lagged` frame with the skipped count.
pub async fn stream(
    State(app): State<SharedApp>,
) -> Sse<impl Stream<Item = Result<SseFrame, std::convert::Infallible>>> {
    let frames = BroadcastStream::new(app.events().subscribe()).map(|item| match item {
        Ok(event) => Ok(frame(&event)),
        Err(BroadcastStreamRecvError::Lagged(skipped)) => Ok(SseFrame::default()
            .event("lagged")
            .data(skipped.to_string())),
    });
    Sse::new(frames).keep_alive(KeepAlive::default())
}

fn frame(event: &AppEvent) -> SseFrame {
    match serde_json::to_string(event) {
        Ok(data) => SseFrame::default().event(event.to_string()).data(data),
        Err(error) => SseFrame::default().event("error").data(error.to_string()),
    }
}
