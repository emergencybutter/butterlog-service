use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post, put, delete},
    Router,
};
use serde::Deserialize;
use std::net::SocketAddr;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod config;
mod db;
mod error;
mod auth;
mod r2;
mod handlers;
mod discord;

use crate::config::Config;
use crate::error::AppError;

#[derive(Clone)]
pub struct AppState {
    db: sqlx::PgPool,
    config: Config,
    http_client: reqwest::Client,
    r2: r2::R2Client,
    discord_http: std::sync::Arc<serenity::http::Http>,
}

#[derive(Deserialize)]
struct LoginQuery {
    port: Option<u16>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    error: Option<String>,
    state: Option<String>,
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

    let r2_client = r2::R2Client::new(&config);

    // Initialize Discord Bot
    let discord_http = discord::start_discord_bot(&config.discord_bot_token).await?;

    let state = AppState {
        db: db_pool,
        config: config.clone(),
        http_client: reqwest::Client::new(),
        r2: r2_client,
        discord_http,
    };

    // Build the router with trace logging
    let app = Router::new()
        .route("/", get(home_handler))
        .route("/content/settings", get(settings_handler))
        .route("/api/v0/auth/login", get(login_handler))
        .route("/api/v0/auth/discord/callback", get(callback_handler))
        .route(
            "/api/v0/discord-notification-channels",
            get(handlers::get_discord_channels_handler).post(handlers::add_discord_channel_handler),
        )
        .route(
            "/api/v0/discord-notification-channels/:channel_id",
            delete(handlers::delete_discord_channel_handler),
        )
        .route(
            "/api/v0/admin/allowlist-channel",
            post(handlers::add_allowlist_channel_handler),
        )
        .route(
            "/api/v0/admin/allowlist-channel/:channel_id",
            delete(handlers::delete_allowlist_channel_handler),
        )
        .route("/api/v0/users/:webhook_token/flights", post(handlers::create_flight_handler))
        .route(
            "/api/v0/users/:webhook_token/flights/:id",
            put(handlers::update_flight_handler).get(handlers::get_flight_handler),
        )
        .route(
            "/api/v0/users/:webhook_token/flights/:id/screenshots",
            post(handlers::upload_screenshot_handler),
        )
        .route(
            "/api/v0/users/:webhook_token/flights/:id/screenshots/:hash",
            delete(handlers::delete_screenshot_handler),
        )
        .layer(axum::middleware::from_fn(log_requests))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("ButterLog service starting on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn log_requests(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    
    tracing::info!("[Incoming Request] {} {}", method, uri);
    
    let start = std::time::Instant::now();
    let response = next.run(req).await;
    let latency = start.elapsed();
    
    tracing::info!(
        "[Incoming Response] {} {} -> Status: {} (took {:?})",
        method,
        uri,
        response.status(),
        latency
    );
    
    response
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
                <a href="/api/v0/auth/login" class="btn">Log In with Discord</a>
            </div>
        </body>
        </html>
    "#)
}

async fn login_handler(
    State(state): State<AppState>,
    Query(query): Query<LoginQuery>,
) -> impl IntoResponse {
    let state_param = query.port.map(|p| p.to_string());
    let auth_url = auth::get_login_url(
        &state.config.discord_client_id,
        &state.config.discord_redirect_uri,
        state_param.as_deref(),
    );
    Redirect::temporary(&auth_url)
}

async fn callback_handler(
    State(state): State<AppState>,
    Query(params): Query<CallbackQuery>,
) -> Result<Response, AppError> {
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

    // Insert or update user info in DB and get api_token
    let api_token = auth::save_or_update_user(&state.db, &discord_user).await?;

    // Check if we need to redirect back to the local app's loopback listener
    if let Some(ref state_val) = params.state {
        if let Ok(port) = state_val.parse::<u16>() {
            let redirect_url = format!("http://127.0.0.1:{}?token={}", port, api_token);
            let mut response = Redirect::temporary(&redirect_url).into_response();
            let cookie_val = format!("token={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000", api_token);
            response.headers_mut().insert(
                axum::http::header::SET_COOKIE,
                axum::http::HeaderValue::from_str(&cookie_val).unwrap()
            );
            return Ok(response);
        }
    }

    let mut response = Redirect::temporary("/content/settings").into_response();
    let cookie_val = format!("token={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000", api_token);
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&cookie_val).unwrap()
    );
    Ok(response)
}

async fn settings_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let user_id = match handlers::get_user_id_from_session(&state.db, &headers).await {
        Ok(id) => id,
        Err(_) => {
            return Redirect::temporary("/api/v0/auth/login").into_response();
        }
    };

    // Fetch user's Discord ID from the database
    let user_discord_id_str: String = match sqlx::query_scalar("SELECT discord_id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Failed to fetch user Discord ID: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to load user profile. Please try logging in again."
            ).into_response();
        }
    };

    let user_discord_id = user_discord_id_str.parse::<u64>().ok();

    // Fetch all guilds the bot is in and details about the user's admin status
    let guilds_info = match discord::get_bot_guilds_and_channels(&state.discord_http, user_discord_id).await {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("Failed to fetch bot guilds and channels: {}", e);
            vec![]
        }
    };

    // Fetch allowlisted channels from the database
    let allowlisted_channels: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT channel_id, channel_name, guild_id FROM allowlisted_channels"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let allowlisted_ids: std::collections::HashSet<String> = allowlisted_channels
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect();

    // Fetch channels the current user has enabled for notifications
    let enabled_channels: Vec<String> = sqlx::query_scalar(
        "SELECT channel_id FROM discord_notification_channels WHERE user_id = $1"
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    // Group allowlisted channels by guild name
    let guild_names: std::collections::HashMap<String, String> = guilds_info
        .iter()
        .map(|g| (g.id.clone(), g.name.clone()))
        .collect();

    // 1. Build Admin Controls HTML
    let mut admin_html = String::new();
    let mut is_any_admin = false;

    for guild in &guilds_info {
        if guild.is_user_admin {
            is_any_admin = true;
            let mut channels_html = String::new();

            if guild.channels.is_empty() {
                channels_html.push_str(r#"<div class="no-channels">No text channels found in this server.</div>"#);
            } else {
                for (chan_id, chan_name) in &guild.channels {
                    let is_checked = allowlisted_ids.contains(chan_id);
                    let checked_attr = if is_checked { "checked" } else { "" };
                    
                    // Escape channel name for javascript string parameters and HTML safety
                    let escaped_name = chan_name
                        .replace('\\', "\\\\")
                        .replace('\'', "\\'")
                        .replace('"', "&quot;");

                    channels_html.push_str(&format!(
                        r#"
                        <div class="channel-row">
                            <div class="channel-info">
                                <span class="channel-name">#{}</span>
                                <span class="channel-id">ID: {}</span>
                            </div>
                            <label class="switch">
                                <input type="checkbox" id="allowlist-{}" onclick="toggleAllowlist('{}', '{}', '{}', this.checked)" {}>
                                <span class="slider"></span>
                            </label>
                        </div>
                        "#,
                        chan_name, chan_id, chan_id, chan_id, guild.id, escaped_name, checked_attr
                    ));
                }
            }

            admin_html.push_str(&format!(
                r#"
                <div class="guild-card">
                    <div class="guild-header">
                        <svg class="guild-icon-svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"></path></svg>
                        <span class="guild-title">{}</span>
                        <span class="badge admin-badge">Server Admin</span>
                    </div>
                    <div class="channel-list">
                        {}
                    </div>
                </div>
                "#,
                guild.name, channels_html
            ));
        }
    }

    let admin_section = if is_any_admin {
        format!(
            r#"
            <section class="settings-section">
                <div class="section-title-container">
                    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="11" width="18" height="11" rx="2" ry="2"></rect><path d="M7 11V7a5 5 0 0 1 10 0v4"></path></svg>
                    <span class="section-title">Admin Controls</span>
                </div>
                <p class="section-desc">As a Discord administrator, allowlist channels to let server members subscribe to flight notifications there.</p>
                <div class="guilds-container">
                    {}
                </div>
            </section>
            "#,
            admin_html
        )
    } else {
        "".to_string()
    };

    // 2. Build User Available Channels HTML
    let mut available_html = String::new();
    
    // Group enabled channels by guild
    let mut enabled_by_guild: std::collections::HashMap<String, Vec<(&String, &String)>> = std::collections::HashMap::new();
    for (chan_id, chan_name, guild_id) in &allowlisted_channels {
        if enabled_channels.contains(chan_id) {
            enabled_by_guild.entry(guild_id.clone()).or_default().push((chan_id, chan_name));
        }
    }

    if enabled_by_guild.is_empty() {
        available_html.push_str(
            r#"
            <div class="no-channels-fallback">
                <svg width="40" height="40" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"></path><line x1="12" y1="9" x2="12" y2="13"></line><line x1="12" y1="17" x2="12.01" y2="17"></line></svg>
                <p>No active notification channels found.</p>
                <p class="small-desc">Announcement channels are automatically mapped based on the Discord servers you belong to.</p>
            </div>
            "#
        );
    } else {
        for (guild_id, chans) in &enabled_by_guild {
            let guild_name = guild_names.get(guild_id).cloned().unwrap_or_else(|| format!("Server ({})", guild_id));
            let mut chans_html = String::new();

            for (chan_id, chan_name) in chans {
                chans_html.push_str(&format!(
                    r#"
                    <div class="channel-row">
                        <div class="channel-info">
                            <span class="channel-name">#{}</span>
                            <span class="channel-id">ID: {}</span>
                        </div>
                        <span class="badge notified-badge">Notified</span>
                    </div>
                    "#,
                    chan_name, chan_id
                ));
            }

            available_html.push_str(&format!(
                r#"
                <div class="guild-card">
                    <div class="guild-header">
                        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"></path></svg>
                        <span class="guild-title">{}</span>
                    </div>
                    <div class="channel-list">
                        {}
                    </div>
                </div>
                "#,
                guild_name, chans_html
            ));
        }
    }

    let user_section = format!(
        r#"
        <section class="settings-section">
            <div class="section-title-container">
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18 8A6 6 0 0 0 6 8c0 7-3 9-3 9h18s-3-2-3-9"></path><path d="M13.73 21a2 2 0 0 1-3.46 0"></path></svg>
                <span class="section-title">Notification Channels</span>
            </div>
            <p class="section-desc">The channels currently receiving your ButterLog flight telemetry embeds. Announcement channels are managed automatically.</p>
            <div class="guilds-container">
                {}
            </div>
        </section>
        "#,
        available_html
    );

    // Render the layout
    Html(format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>ButterLog Notification Settings</title>
            <meta name="description" content="Manage your Discord notification channels for ButterLog flight telemetry.">
            <style>
                @import url('https://fonts.googleapis.com/css2?family=Outfit:wght@300;400;500;600;700&display=swap');
                
                :root {{
                    --bg-gradient: radial-gradient(circle at 50% 0%, #1e1e3f 0%, #0d0d16 100%);
                    --panel-bg: rgba(30, 30, 46, 0.45);
                    --border-color: rgba(255, 255, 255, 0.08);
                    --text-primary: #cdd6f4;
                    --text-secondary: #a6adc8;
                    --text-muted: #6c7086;
                    --primary-color: #cba6f7;
                    --primary-hover: #b4befe;
                    --accent-pink: #f5c2e7;
                    --accent-green: #a6e3a1;
                    --accent-red: #f38ba8;
                    --card-bg: rgba(17, 17, 27, 0.35);
                }}

                body {{
                    font-family: 'Outfit', sans-serif;
                    background: var(--bg-gradient);
                    color: var(--text-primary);
                    min-height: 100vh;
                    margin: 0;
                    display: flex;
                    flex-direction: column;
                    align-items: center;
                    justify-content: flex-start;
                    padding: 3rem 1rem;
                    box-sizing: border-box;
                }}

                .settings-container {{
                    max-width: 700px;
                    width: 100%;
                    background: var(--panel-bg);
                    backdrop-filter: blur(20px);
                    -webkit-backdrop-filter: blur(20px);
                    border: 1px solid var(--border-color);
                    border-radius: 28px;
                    padding: 3rem;
                    box-shadow: 0 30px 60px rgba(0, 0, 0, 0.5), inset 0 1px 0 rgba(255, 255, 255, 0.1);
                    box-sizing: border-box;
                    animation: fadeIn 0.6s cubic-bezier(0.16, 1, 0.3, 1);
                }}

                @keyframes fadeIn {{
                    from {{ opacity: 0; transform: translateY(15px); }}
                    to {{ opacity: 1; transform: translateY(0); }}
                }}

                .header {{
                    text-align: center;
                    margin-bottom: 3rem;
                }}

                h1 {{
                    font-size: 2.5rem;
                    font-weight: 700;
                    margin: 0 0 0.5rem 0;
                    background: linear-gradient(90deg, var(--primary-color), var(--primary-hover));
                    -webkit-background-clip: text;
                    -webkit-text-fill-color: transparent;
                    letter-spacing: -0.5px;
                }}

                .subtitle {{
                    color: var(--text-secondary);
                    font-size: 1.1rem;
                    margin: 0;
                    font-weight: 300;
                }}

                .settings-section {{
                    margin-bottom: 3.5rem;
                }}

                .settings-section:last-of-type {{
                    margin-bottom: 1rem;
                }}

                .section-title-container {{
                    display: flex;
                    align-items: center;
                    gap: 0.75rem;
                    margin-bottom: 0.5rem;
                    color: var(--accent-pink);
                }}

                .section-title {{
                    font-size: 1.2rem;
                    font-weight: 600;
                    text-transform: uppercase;
                    letter-spacing: 1.5px;
                }}

                .section-desc {{
                    font-size: 0.95rem;
                    color: var(--text-secondary);
                    margin: 0 0 1.5rem 0;
                    line-height: 1.5;
                }}

                .guilds-container {{
                    display: flex;
                    flex-direction: column;
                    gap: 1.5rem;
                }}

                .guild-card {{
                    background: var(--card-bg);
                    border: 1px solid rgba(255, 255, 255, 0.04);
                    border-radius: 20px;
                    padding: 1.5rem;
                    box-shadow: 0 8px 16px rgba(0, 0, 0, 0.15);
                    transition: border-color 0.3s, transform 0.3s;
                }}

                .guild-card:hover {{
                    border-color: rgba(255, 255, 255, 0.08);
                }}

                .guild-header {{
                    display: flex;
                    align-items: center;
                    gap: 0.75rem;
                    margin-bottom: 1.25rem;
                    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
                    padding-bottom: 0.75rem;
                }}

                .guild-title {{
                    font-weight: 600;
                    font-size: 1.1rem;
                    color: var(--text-primary);
                }}

                .badge {{
                    font-size: 0.75rem;
                    font-weight: 600;
                    padding: 0.25rem 0.5rem;
                    border-radius: 6px;
                    text-transform: uppercase;
                    letter-spacing: 0.5px;
                }}

                .admin-badge {{
                    background: rgba(203, 166, 247, 0.15);
                    color: var(--primary-color);
                    border: 1px solid rgba(203, 166, 247, 0.3);
                }}

                .notified-badge {{
                    background: rgba(166, 227, 161, 0.15);
                    color: var(--accent-green);
                    border: 1px solid rgba(166, 227, 161, 0.3);
                }}

                .channel-list {{
                    display: flex;
                    flex-direction: column;
                    gap: 0.5rem;
                }}

                .channel-row {{
                    display: flex;
                    justify-content: space-between;
                    align-items: center;
                    padding: 0.75rem 1rem;
                    border-radius: 12px;
                    background: rgba(255, 255, 255, 0.01);
                    border: 1px solid transparent;
                    transition: background-color 0.2s, border-color 0.2s;
                }}

                .channel-row:hover {{
                    background-color: rgba(255, 255, 255, 0.03);
                    border-color: rgba(255, 255, 255, 0.03);
                }}

                .channel-info {{
                    display: flex;
                    flex-direction: column;
                }}

                .channel-name {{
                    font-weight: 500;
                    color: var(--text-primary);
                    font-size: 1rem;
                }}

                .channel-id {{
                    font-size: 0.8rem;
                    color: var(--text-muted);
                    margin-top: 0.15rem;
                }}

                .no-channels {{
                    color: var(--text-muted);
                    font-size: 0.9rem;
                    text-align: center;
                    padding: 1rem 0;
                    font-style: italic;
                }}

                .no-channels-fallback {{
                    text-align: center;
                    padding: 3rem 2rem;
                    background: var(--card-bg);
                    border-radius: 20px;
                    border: 1px dashed rgba(255, 255, 255, 0.1);
                    color: var(--text-secondary);
                }}

                .no-channels-fallback svg {{
                    margin-bottom: 1rem;
                    color: var(--accent-pink);
                }}

                .no-channels-fallback p {{
                    margin: 0.25rem 0;
                    font-size: 1rem;
                }}

                .no-channels-fallback .small-desc {{
                    font-size: 0.85rem;
                    color: var(--text-muted);
                }}

                /* Toggle Switch */
                .switch {{
                    position: relative;
                    display: inline-block;
                    width: 48px;
                    height: 24px;
                }}

                .switch input {{
                    opacity: 0;
                    width: 0;
                    height: 0;
                }}

                .slider {{
                    position: absolute;
                    cursor: pointer;
                    top: 0; left: 0; right: 0; bottom: 0;
                    background-color: rgba(255, 255, 255, 0.08);
                    transition: 0.3s cubic-bezier(0.4, 0, 0.2, 1);
                    border-radius: 24px;
                    border: 1px solid rgba(255, 255, 255, 0.05);
                }}

                .slider:before {{
                    position: absolute;
                    content: "";
                    height: 16px;
                    width: 16px;
                    left: 3px;
                    bottom: 3px;
                    background-color: var(--text-secondary);
                    transition: 0.3s cubic-bezier(0.4, 0, 0.2, 1);
                    border-radius: 50%;
                }}

                input:checked + .slider {{
                    background-color: rgba(166, 227, 161, 0.2);
                    border-color: rgba(166, 227, 161, 0.4);
                }}

                input:checked + .slider:before {{
                    transform: translateX(24px);
                    background-color: var(--accent-green);
                }}

                /* Input and Custom Channels */
                .input-group {{
                    display: flex;
                    gap: 0.75rem;
                    margin-top: 1rem;
                }}

                input[type="text"] {{
                    flex: 1;
                    background: rgba(17, 17, 27, 0.6);
                    border: 1px solid rgba(255, 255, 255, 0.08);
                    border-radius: 12px;
                    padding: 0.75rem 1rem;
                    color: var(--text-primary);
                    font-family: inherit;
                    font-size: 0.95rem;
                    outline: none;
                    transition: border-color 0.2s, box-shadow 0.2s;
                }}

                input[type="text"]:focus {{
                    border-color: var(--primary-color);
                    box-shadow: 0 0 0 3px rgba(203, 166, 247, 0.15);
                }}

                button {{
                    background: var(--primary-color);
                    color: #11111b;
                    border: none;
                    border-radius: 12px;
                    padding: 0.75rem 1.5rem;
                    font-weight: 600;
                    cursor: pointer;
                    font-family: inherit;
                    font-size: 0.95rem;
                    transition: transform 0.2s, background-color 0.2s;
                }}

                button:hover {{
                    background: var(--primary-hover);
                    transform: translateY(-1px);
                }}

                /* Toast Notifications */
                .toast-container {{
                    position: fixed;
                    bottom: 2rem;
                    right: 2rem;
                    z-index: 1000;
                    display: flex;
                    flex-direction: column;
                    gap: 0.75rem;
                }}

                .toast {{
                    background: rgba(30, 30, 46, 0.95);
                    border: 1px solid rgba(255, 255, 255, 0.1);
                    padding: 1rem 1.5rem;
                    border-radius: 14px;
                    box-shadow: 0 15px 30px rgba(0,0,0,0.5);
                    display: flex;
                    align-items: center;
                    gap: 0.75rem;
                    animation: slideIn 0.3s cubic-bezier(0.16, 1, 0.3, 1), fadeOut 0.3s ease 2.7s forwards;
                    color: var(--text-primary);
                    font-weight: 500;
                    font-size: 0.95rem;
                }}

                .toast.success {{
                    border-left: 4px solid var(--accent-green);
                }}

                .toast.error {{
                    border-left: 4px solid var(--accent-red);
                }}

                @keyframes slideIn {{
                    from {{ transform: translateX(120%); opacity: 0; }}
                    to {{ transform: translateX(0); opacity: 1; }}
                }}

                @keyframes fadeOut {{
                    to {{ opacity: 0; transform: translateY(10px); }}
                }}
            </style>
        </head>
        <body>
            <main class="settings-container">
                <header class="header">
                    <h1>ButterLog Notifications</h1>
                    <p class="subtitle">Select the channels you wish to receive flight telemetry embeds</p>
                </header>

                {}

                {}

            </main>

            <div class="toast-container" id="toast-container"></div>

            <script>
                function showToast(message, type) {{
                    const container = document.getElementById('toast-container');
                    const toast = document.createElement('div');
                    toast.className = `toast ${{type}}`;
                    toast.innerText = message;
                    container.appendChild(toast);
                    setTimeout(() => toast.remove(), 3000);
                }}

                async function toggleAllowlist(channelId, guildId, channelName, checked) {{
                    try {{
                        let response;
                        if (checked) {{
                            response = await fetch('/api/v0/admin/allowlist-channel', {{
                                method: 'POST',
                                headers: {{
                                    'Content-Type': 'application/json'
                                }},
                                body: JSON.stringify({{ 
                                    channelId: channelId,
                                    guildId: guildId,
                                    channelName: channelName
                                }})
                            }});
                        }} else {{
                            response = await fetch(`/api/v0/admin/allowlist-channel/${{channelId}}`, {{
                                method: 'DELETE'
                            }});
                        }}

                        if (response.ok) {{
                            showToast(checked ? 'Channel added to allowlist!' : 'Channel removed from allowlist!', 'success');
                            // Reload after a short delay to update the user notifications section
                            setTimeout(() => window.location.reload(), 1000);
                        }} else {{
                            const data = await response.json().catch(() => ({{}}));
                            const errMsg = data.error || 'Request failed';
                            showToast(errMsg, 'error');
                            document.getElementById(`allowlist-${{channelId}}`).checked = !checked;
                        }}
                    }} catch (err) {{
                        showToast('Network error occurred', 'error');
                        document.getElementById(`allowlist-${{channelId}}`).checked = !checked;
                    }}
                }}


            </script>
        </body>
        </html>
        "#,
        admin_section,
        user_section
    ))
    .into_response()
}
