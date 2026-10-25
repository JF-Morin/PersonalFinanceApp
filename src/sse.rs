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

#[derive(Clone, Debug, Serialize)]
pub struct AppEvent {
    pub kind: &'static str,
    pub actor_id: i64,
}

pub struct EventBus {
    tx: broadcast::Sender<AppEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(128);
        Self { tx }
    }

    pub fn send(&self, event: AppEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent> {
        self.tx.subscribe()
    }
}

pub async fn get_events(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<impl IntoResponse> {
    crate::handlers::get_user_id(&jar)?;

    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| {
        result.ok().map(|event| {
            let data = serde_json::to_string(&event).unwrap_or_default();
            Ok::<Event, Infallible>(Event::default().data(data))
        })
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
