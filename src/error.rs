use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    Database(sqlx::Error),
    Network(reqwest::Error),
    Auth(String),
    Migration(sqlx::migrate::MigrateError),
    NotFound(String),
    BadRequest(String),
    Forbidden(String),
    Storage(String),
    Template(askama::Error),
    /// The request is well-formed but conflicts with the resource's current
    /// state (e.g. commanding a flight that is no longer active).
    Conflict(String),
    /// The resource existed but is finished and will not accept more writes.
    Gone(String),
    /// Body over the configured cap, checked after decompression.
    PayloadTooLarge(String),
    /// Semantically invalid parameter (e.g. a heading outside 0-359).
    Unprocessable(String),
    /// Rate limit exceeded.
    TooManyRequests(String),
}

impl From<askama::Error> for AppError {
    fn from(err: askama::Error) -> Self {
        Self::Template(err)
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        Self::Network(err)
    }
}

impl From<sqlx::migrate::MigrateError> for AppError {
    fn from(err: sqlx::migrate::MigrateError) -> Self {
        Self::Migration(err)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::Database(ref err) => {
                tracing::error!("Database error: {:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database operation failed".to_string(),
                )
            }
            AppError::Network(ref err) => {
                tracing::error!("Network error: {:?}", err);
                (
                    StatusCode::BAD_GATEWAY,
                    "External API request failed".to_string(),
                )
            }
            AppError::Auth(ref msg) => {
                tracing::warn!("Authentication error: {}", msg);
                (StatusCode::UNAUTHORIZED, msg.clone())
            }
            AppError::Migration(ref err) => {
                tracing::error!("Migration error: {:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Database migration failed".to_string(),
                )
            }
            AppError::Storage(ref msg) => {
                tracing::error!("Storage error: {}", msg);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Storage operation failed".to_string(),
                )
            }
            AppError::Template(ref err) => {
                tracing::error!("Template rendering error: {:?}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Page rendering failed".to_string(),
                )
            }
            AppError::NotFound(ref msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::BadRequest(ref msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Forbidden(ref msg) => (StatusCode::FORBIDDEN, msg.clone()),
            AppError::Conflict(ref msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::Gone(ref msg) => (StatusCode::GONE, msg.clone()),
            AppError::PayloadTooLarge(ref msg) => (StatusCode::PAYLOAD_TOO_LARGE, msg.clone()),
            AppError::Unprocessable(ref msg) => {
                (StatusCode::UNPROCESSABLE_ENTITY, msg.clone())
            }
            AppError::TooManyRequests(ref msg) => (StatusCode::TOO_MANY_REQUESTS, msg.clone()),
        };

        let body = Json(json!({
            "error": error_message,
        }));

        (status, body).into_response()
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Database(err) => write!(f, "Database error: {}", err),
            AppError::Network(err) => write!(f, "Network error: {}", err),
            AppError::Auth(msg) => write!(f, "Authentication error: {}", msg),
            AppError::Migration(err) => write!(f, "Migration error: {}", err),
            AppError::NotFound(msg) => write!(f, "Not found: {}", msg),
            AppError::BadRequest(msg) => write!(f, "Bad request: {}", msg),
            AppError::Forbidden(msg) => write!(f, "Forbidden: {}", msg),
            AppError::Storage(msg) => write!(f, "Storage error: {}", msg),
            AppError::Template(err) => write!(f, "Template error: {}", err),
            AppError::Conflict(msg) => write!(f, "Conflict: {}", msg),
            AppError::Gone(msg) => write!(f, "Gone: {}", msg),
            AppError::PayloadTooLarge(msg) => write!(f, "Payload too large: {}", msg),
            AppError::Unprocessable(msg) => write!(f, "Unprocessable: {}", msg),
            AppError::TooManyRequests(msg) => write!(f, "Too many requests: {}", msg),
        }
    }
}

impl std::error::Error for AppError {}

