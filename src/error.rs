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
        }
    }
}

impl std::error::Error for AppError {}

