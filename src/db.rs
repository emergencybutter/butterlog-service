use sqlx::{postgres::PgPoolOptions, PgPool};
use std::time::Duration;
use crate::error::AppError;

pub async fn init_db(database_url: &str) -> Result<PgPool, AppError> {
    tracing::info!("Connecting to PostgreSQL database...");

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await?;

    tracing::info!("Database connection established. Running migrations...");

    // Runs migrations embedded in the binary during compile-time
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await?;

    tracing::info!("Database migrations executed successfully.");
    Ok(pool)
}
