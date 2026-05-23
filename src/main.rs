use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect},
    routing::get,
    Router,
};
use serde::Deserialize;
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod db;
mod error;
mod auth;

use crate::config::Config;
use crate::error::AppError;

#[derive(Clone)]
struct AppState {
    db: sqlx::PgPool,
    config: Config,
    http_client: reqwest::Client,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    error: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize structured logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,butterlog_service=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configurations
    let config = Config::from_env();

    // Initialize database pool and run migrations
    let db_pool = db::init_db(&config.database_url).await?;

    let state = AppState {
        db: db_pool,
        config: config.clone(),
        http_client: reqwest::Client::new(),
    };

    // Build the router with trace logging
    let app = Router::new()
        .route("/", get(home_handler))
        .route("/login", get(login_handler))
        .route("/auth/callback", get(callback_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("ButterLog service starting on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn home_handler() -> impl IntoResponse {
    Html(r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>ButterLog Service</title>
            <style>
                body {
                    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
                    background: linear-gradient(135deg, #1e1e2e, #11111b);
                    color: #cdd6f4;
                    display: flex;
                    justify-content: center;
                    align-items: center;
                    height: 100vh;
                    margin: 0;
                }
                .container {
                    text-align: center;
                    background: rgba(255, 255, 255, 0.05);
                    padding: 3rem;
                    border-radius: 16px;
                    box-shadow: 0 8px 32px 0 rgba(0, 0, 0, 0.37);
                    backdrop-filter: blur(8px);
                    border: 1px solid rgba(255, 255, 255, 0.1);
                    max-width: 400px;
                    width: 90%;
                }
                h1 {
                    color: #f5c2e7;
                    margin-bottom: 1.5rem;
                    font-size: 2rem;
                }
                p {
                    color: #a6adc8;
                    margin-bottom: 2rem;
                    line-height: 1.5;
                }
                .btn {
                    display: inline-block;
                    background: #cba6f7;
                    color: #11111b;
                    padding: 0.75rem 1.5rem;
                    border-radius: 8px;
                    text-decoration: none;
                    font-weight: bold;
                    transition: transform 0.2s, background-color 0.2s;
                }
                .btn:hover {
                    background: #b4befe;
                    transform: translateY(-2px);
                }
            </style>
        </head>
        <body>
            <div class="container">
                <h1>ButterLog Backend</h1>
                <p>Welcome! Authenticate using your Discord account to get started.</p>
                <a href="/login" class="btn">Log In with Discord</a>
            </div>
        </body>
        </html>
    "#)
}

async fn login_handler(State(state): State<AppState>) -> impl IntoResponse {
    let auth_url = auth::get_login_url(&state.config.discord_client_id, &state.config.discord_redirect_uri);
    Redirect::temporary(&auth_url)
}

async fn callback_handler(
    State(state): State<AppState>,
    Query(params): Query<CallbackQuery>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(err) = params.error {
        return Err(AppError::Auth(format!("Discord OAuth error: {}", err)));
    }

    let code = params.code.ok_or_else(|| {
        AppError::Auth("Missing code parameter in OAuth callback".to_string())
    })?;

    // Exchange auth code for access token
    let access_token = auth::exchange_code(
        &state.http_client,
        &code,
        &state.config.discord_client_id,
        &state.config.discord_client_secret,
        &state.config.discord_redirect_uri,
    )
    .await?;

    // Fetch details of authenticating user from Discord
    let discord_user = auth::fetch_discord_user(&state.http_client, &access_token).await?;

    // Insert or update user info in DB
    auth::save_or_update_user(&state.db, &discord_user).await?;

    let display_name = discord_user.global_name.unwrap_or(discord_user.username);

    Ok(Html(format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>Welcome - ButterLog</title>
            <style>
                body {{
                    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
                    background: linear-gradient(135deg, #1e1e2e, #11111b);
                    color: #cdd6f4;
                    display: flex;
                    justify-content: center;
                    align-items: center;
                    height: 100vh;
                    margin: 0;
                }}
                .container {{
                    text-align: center;
                    background: rgba(255, 255, 255, 0.05);
                    padding: 3rem;
                    border-radius: 16px;
                    box-shadow: 0 8px 32px 0 rgba(0, 0, 0, 0.37);
                    backdrop-filter: blur(8px);
                    border: 1px solid rgba(255, 255, 255, 0.1);
                    max-width: 400px;
                    width: 90%;
                }}
                h1 {{
                    color: #a6e3a1;
                    margin-bottom: 1.5rem;
                    font-size: 2.2rem;
                }}
                p {{
                    color: #a6adc8;
                    font-size: 1.2rem;
                    line-height: 1.5;
                }}
            </style>
        </head>
        <body>
            <div class="container">
                <h1>Hello {}!</h1>
                <p>You have successfully logged in via Discord.</p>
            </div>
        </body>
        </html>
        "#,
        display_name
    )))
}
