/// Unified error type for all handler functions.
/// `thiserror::Error` generates the `Display` impl from the `#[error("...")]` attributes.
/// `AppResult<T>` is just a convenience alias so handlers can write `AppResult<Html<String>>`
/// instead of the full `Result<Html<String>, AppError>`.
use axum::{
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    /// Wraps a SQLx error — the `#[from]` attribute lets `?` convert automatically.
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Wraps a Tera template rendering error.
    #[error("Template error: {0}")]
    Template(#[from] tera::Error),
    /// Wraps a filesystem I/O error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Missing or invalid `user_id` cookie — the user is not logged in.
    #[error("Unauthorized")]
    Unauthorized,
    /// Catch-all for domain errors (bank mismatch, duplicate file, etc.).
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Other(e.to_string())
    }
}

/// Axum calls `into_response()` automatically when a handler returns `Err(AppError)`.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            // Redirect rather than 401 — this is an HTML app, not a JSON API.
            // The `?unauthorized=1` query param lets the login page show an explanatory banner.
            AppError::Unauthorized => Redirect::to("/login?unauthorized=1").into_response(),
            e => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;
