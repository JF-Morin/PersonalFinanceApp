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

impl AppError {
    /// The message shown to users (French, no technical details). The technical text
    /// (`Display`) is logged here, so every place that shows an error to a user also
    /// leaves a trace for debugging.
    pub fn user_message(&self) -> String {
        if !matches!(self, AppError::Other(_) | AppError::Unauthorized) {
            tracing::error!(error = %self, "Request failed");
        }
        match self {
            AppError::Database(e) => database_message(e).to_string(),
            AppError::Template(_) => "Erreur d'affichage de la page. Réessayez.".to_string(),
            AppError::Io(_) => "Erreur de lecture ou d'écriture de fichier sur le serveur.".to_string(),
            AppError::Unauthorized => "Session expirée : reconnectez-vous.".to_string(),
            // Domain errors are already written for users.
            AppError::Other(msg) => msg.clone(),
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            AppError::Database(sqlx::Error::RowNotFound) => StatusCode::NOT_FOUND,
            AppError::Database(sqlx::Error::Database(db)) if matches!(
                db.kind(),
                sqlx::error::ErrorKind::ForeignKeyViolation | sqlx::error::ErrorKind::UniqueViolation
            ) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Map a database error to a message a user can act on.
fn database_message(e: &sqlx::Error) -> &'static str {
    use sqlx::error::ErrorKind;
    match e {
        sqlx::Error::RowNotFound => "Élément introuvable — il a peut-être été supprimé entre-temps.",
        sqlx::Error::PoolTimedOut => "La base de données est occupée. Réessayez dans un instant.",
        sqlx::Error::Database(db) => match db.kind() {
            ErrorKind::ForeignKeyViolation => "Opération impossible : cet élément est encore lié à d'autres données.",
            ErrorKind::UniqueViolation => "Cet élément existe déjà.",
            ErrorKind::NotNullViolation | ErrorKind::CheckViolation => "Une donnée obligatoire est manquante ou invalide.",
            _ if db.message().contains("locked") || db.message().contains("busy") => {
                "La base de données est occupée. Réessayez dans un instant."
            }
            _ => "Erreur de base de données. Réessayez.",
        },
        _ => "Erreur de base de données. Réessayez.",
    }
}

/// Axum calls `into_response()` automatically when a handler returns `Err(AppError)`.
/// Users get `user_message()` (which also logs the technical error).
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            // Redirect rather than 401 — this is an HTML app, not a JSON API.
            // The `?unauthorized=1` query param lets the login page show an explanatory banner.
            AppError::Unauthorized => Redirect::to("/login?unauthorized=1").into_response(),
            e => (e.status(), e.user_message()).into_response(),
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;
