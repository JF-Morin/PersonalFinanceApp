/// Server-Sent Events (SSE) for real-time cross-tab / cross-user updates.
///
/// How it works:
/// 1. `AppState` holds one `EventBus` (a Tokio broadcast channel).
/// 2. Any handler that mutates data calls `state.events.send(AppEvent { kind, actor_id })`.
/// 3. Every logged-in browser holds an open GET `/events` connection; each connection
///    subscribes to the broadcast channel and streams events to its client.
/// 4. The HTMX SSE extension (sse.js) on the page listens for named events and
///    triggers partial refreshes on elements that have `hx-trigger="sse:<kind> from:body"`.
///
/// The `actor_id` is embedded in the event data so the client can filter out events
/// it triggered itself if desired (optional — currently handled client-side).
use axum::{
    extract::State,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
};
use axum_extra::extract::CookieJar;
use serde::Serialize;
use std::convert::Infallible;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};

use crate::{error::AppResult, AppState};

/// A single broadcast message.  `kind` becomes the SSE event name so HTMX
/// can match it with `hx-trigger="sse:transactions from:body"` etc.
#[derive(Clone, Debug, Serialize)]
pub struct AppEvent {
    /// One of "transactions", "imports", "settings" — matches the SSE trigger names in templates.
    pub kind: &'static str,
    /// ID of the user who caused the change.
    pub actor_id: i64,
}

/// Shared event bus backed by a Tokio broadcast channel.
/// Cloning `AppState` clones the `Arc<EventBus>`, so all clones share the same sender.
pub struct EventBus {
    tx: broadcast::Sender<AppEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        // Channel capacity of 128: if a slow client falls 128 events behind, it misses
        // some — acceptable since it will reload on reconnect.
        let (tx, _) = broadcast::channel(128);
        Self { tx }
    }

    /// Broadcast an event to all connected SSE clients.  Errors are silently ignored
    /// (no receivers connected = nothing to do).
    pub fn send(&self, event: AppEvent) {
        let _ = self.tx.send(event);
    }

    /// Create a new receiver subscribed from this point forward.
    /// Called once per GET /events request.
    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.tx.subscribe()
    }
}

/// GET /events — streams SSE to the browser for the lifetime of the connection.
/// Requires authentication; unauthenticated requests return a redirect via AppError::Unauthorized.
pub async fn get_events(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<impl IntoResponse> {
    crate::handlers::get_user_id(&jar)?;

    let rx = state.events.subscribe();

    // Convert the broadcast receiver into an async stream, skip lagged-message errors.
    let stream = BroadcastStream::new(rx).filter_map(|result| {
        result.ok().map(|event| {
            // Embed the full AppEvent struct as JSON in the data field so the client
            // can read actor_id.  The event name is set separately so HTMX can match it.
            let data = serde_json::to_string(&event).unwrap_or_default();
            Ok::<Event, Infallible>(Event::default().event(event.kind).data(data))
        })
    });

    // KeepAlive sends a comment ping every ~15 s so proxies don't close the idle connection.
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
