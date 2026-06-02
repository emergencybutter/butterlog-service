use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post, put, delete},
    Router,
};
use serde::{Deserialize, Serialize};
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
    pub peers: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<i64, (String, std::time::Instant)>>>,
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
        peers: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };

    // Build the router with trace logging
    let app = Router::new()
        .route("/", get(home_handler))
        .route("/content", get(content_handler))
        .route("/content/settings", get(settings_handler))
        .route("/map", get(map_handler))
        .route("/api/v0/map/data", get(map_data_handler))
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
        .route(
            "/api/v0/users/:webhook_token/flights/share",
            post(handlers::upload_flight_share_handler),
        )
        .route(
            "/api/v0/users/:webhook_token/flights/share/:share_id",
            delete(handlers::delete_flight_share_handler),
        )
        .route(
            "/api/v0/flights/share/:share_id",
            get(handlers::get_flight_share_json_handler).delete(handlers::delete_flight_share_session_handler),
        )
        .route("/content/flights/share/:share_id", get(flight_share_detail_handler))
        .route(
            "/api/v0/users/:webhook_token/multiplayer/ping",
            post(handlers::multiplayer_ping_handler),
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

    let mut response = Redirect::temporary("/content").into_response();
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

                .nav-tabs {{
                    display: flex;
                    justify-content: center;
                    gap: 1rem;
                    margin-bottom: 3rem;
                    border-bottom: 1px solid var(--border-color);
                    padding-bottom: 1rem;
                }}

                .nav-tab {{
                    color: var(--text-muted);
                    text-decoration: none;
                    font-weight: 500;
                    font-size: 1rem;
                    padding: 0.5rem 1rem;
                    border-radius: 8px;
                    transition: all 0.2s;
                }}

                .nav-tab:hover {{
                    color: var(--text-primary);
                    background: rgba(255, 255, 255, 0.05);
                }}

                .nav-tab.active {{
                    color: var(--primary-color);
                    background: rgba(203, 166, 247, 0.1);
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

                <div class="nav-tabs">
                    <a href="/content" class="nav-tab">Flight History</a>
                    <a href="/content/settings" class="nav-tab active">Notification Settings</a>
                    <a href="/map" class="nav-tab">Live Map</a>
                </div>

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

async fn flight_detail_handler(
    State(state): State<AppState>,
    axum::extract::Path(flight_id): axum::extract::Path<i64>,
) -> Result<Response, AppError> {
    let row: Option<(String, Option<String>, serde_json::Value, chrono::DateTime<chrono::Utc>, String, Option<String>)> = sqlx::query_as(
        "SELECT f.departure, f.arrival, f.statistics, f.created_at, u.username, u.global_name \
         FROM flights f JOIN users u ON f.user_id = u.id WHERE f.id = $1"
    )
    .bind(flight_id)
    .fetch_optional(&state.db)
    .await?;

    let (dep, arr, stats, created_at, username, global_name) = match row {
        Some(r) => r,
        None => return Ok((axum::http::StatusCode::NOT_FOUND, Html("<h1>Flight not found</h1>".to_string())).into_response()),
    };

    let screenshots: Vec<String> = sqlx::query_scalar(
        "SELECT url FROM screenshots WHERE flight_id = $1 ORDER BY created_at"
    )
    .bind(flight_id)
    .fetch_all(&state.db)
    .await?;

    let arr_display = arr.as_deref().unwrap_or("In Flight");
    let pilot = global_name.as_deref().unwrap_or(&username);
    let airframe = stats.get("airframe_name").and_then(|v| v.as_str()).unwrap_or("Unknown Aircraft");
    let simulator = stats.get("simulator").and_then(|v| v.as_str()).unwrap_or("");
    let date_str = created_at.format("%B %d, %Y, %H:%M UTC").to_string();

    let mut landing_badge = String::new();
    if let Some(landing) = stats.get("landing_snapshot") {
        if let Some(vspd) = landing.get("VSpd").and_then(|v| v.as_f64()) {
            let gforce_str = landing.get("NormAc").and_then(|v| v.as_f64())
                .map(|g| format!(" / {:.2}G", g)).unwrap_or_default();
            let (class, label) = if vspd >= -150.0 { ("butter", "BUTTER") }
                else if vspd >= -250.0 { ("smooth", "SMOOTH") }
                else if vspd >= -350.0 { ("firm", "FIRM") }
                else { ("hard", "HARD") };
            landing_badge = format!(
                r#"<div class="badge badge-{}">{}<br><span class="badge-detail">{:.0} fpm{}</span></div>"#,
                class, label, vspd.abs(), gforce_str
            );
        }
    }

    let gallery_html = if !screenshots.is_empty() {
        let urls_json = serde_json::to_string(&screenshots).unwrap_or_default();
        let thumbs: String = screenshots.iter().enumerate().map(|(i, url)| {
            format!(
                r#"<img class="screenshot-thumb" src="{}" alt="Screenshot {}" loading="lazy" onclick='openLightbox({}, {})'>"#,
                url, i + 1, urls_json, i
            )
        }).collect();
        format!(r#"<div class="screenshot-gallery">{}</div>"#, thumbs)
    } else {
        String::new()
    };

    let html = format!(r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{dep} → {arr_display} — ButterLog</title>
    <meta property="og:title" content="{dep} → {arr_display}">
    <meta property="og:description" content="{airframe} • {pilot} • {date_str}">
    <meta property="og:site_name" content="ButterLog">
    <style>
        @import url('https://fonts.googleapis.com/css2?family=Outfit:wght@300;400;500;600;700&display=swap');
        :root {{
            --bg-gradient: radial-gradient(circle at 50% 0%, #1e1e3f 0%, #0d0d16 100%);
            --panel-bg: rgba(30, 30, 46, 0.45);
            --border-color: rgba(255, 255, 255, 0.08);
            --text-primary: #cdd6f4; --text-secondary: #a6adc8; --text-muted: #6c7086;
            --primary-color: #cba6f7; --accent-pink: #f5c2e7; --card-bg: rgba(17, 17, 27, 0.35);
        }}
        body {{ font-family: 'Outfit', sans-serif; background: var(--bg-gradient); color: var(--text-primary);
                min-height: 100vh; margin: 0; display: flex; flex-direction: column; align-items: center;
                justify-content: flex-start; padding: 3rem 1rem; box-sizing: border-box; }}
        .container {{ max-width: 750px; width: 100%; background: var(--panel-bg); backdrop-filter: blur(20px);
                      border: 1px solid var(--border-color); border-radius: 28px; padding: 3rem;
                      box-shadow: 0 30px 60px rgba(0,0,0,0.5); box-sizing: border-box; }}
        .back {{ color: var(--text-muted); text-decoration: none; font-size: 0.9rem; display: inline-flex;
                 align-items: center; gap: 0.4rem; margin-bottom: 2rem; transition: color 0.2s; }}
        .back:hover {{ color: var(--primary-color); }}
        .route {{ display: flex; align-items: center; gap: 1rem; margin-bottom: 0.5rem; }}
        .icao {{ font-size: 2.5rem; font-weight: 700; }}
        .arrow {{ font-size: 2rem; color: var(--primary-color); }}
        .airframe {{ font-size: 1.2rem; color: var(--accent-pink); font-weight: 500; margin-bottom: 0.3rem; }}
        .meta {{ color: var(--text-secondary); font-size: 0.9rem; margin-bottom: 0.25rem; }}
        .pilot {{ color: var(--text-muted); font-size: 0.85rem; margin-bottom: 1.5rem; }}
        .badge {{ display: inline-block; padding: 0.5rem 1rem; border-radius: 10px; font-weight: 700;
                  font-size: 0.85rem; text-align: center; margin-bottom: 1.5rem; }}
        .badge-detail {{ font-size: 0.75rem; font-weight: 400; opacity: 0.9; }}
        .badge-butter {{ background: linear-gradient(135deg, #a6e3a1, #89b4fa); color: #11111b; }}
        .badge-smooth {{ background: linear-gradient(135deg, #94e2d5, #a6e3a1); color: #11111b; }}
        .badge-firm {{ background: linear-gradient(135deg, #fab387, #f9e2af); color: #11111b; }}
        .badge-hard {{ background: linear-gradient(135deg, #f38ba8, #eba0ac); color: #11111b; }}
        .screenshot-gallery {{ display: flex; gap: 8px; overflow-x: auto; padding-top: 1rem;
                                border-top: 1px solid var(--border-color); scrollbar-width: thin; }}
        .screenshot-thumb {{ width: 140px; height: 88px; object-fit: cover; border-radius: 8px; cursor: pointer;
                              flex-shrink: 0; border: 1px solid var(--border-color); transition: transform 0.2s; }}
        .screenshot-thumb:hover {{ transform: scale(1.04); }}
        .lightbox {{ display: none; position: fixed; inset: 0; background: rgba(0,0,0,0.92); z-index: 1000;
                     align-items: center; justify-content: center; flex-direction: column; gap: 1rem; }}
        .lightbox.open {{ display: flex; }}
        .lightbox-img {{ max-width: 90vw; max-height: 82vh; object-fit: contain; border-radius: 10px; }}
        .lightbox-counter {{ color: rgba(255,255,255,0.6); font-size: 0.9rem; }}
        .lb-btn {{ position: absolute; top: 50%; transform: translateY(-50%); background: rgba(255,255,255,0.08);
                   border: 1px solid rgba(255,255,255,0.12); color: white; font-size: 1.8rem; width: 3rem;
                   height: 3rem; border-radius: 50%; cursor: pointer; display: flex; align-items: center;
                   justify-content: center; transition: background 0.2s; }}
        .lb-btn:hover {{ background: rgba(255,255,255,0.15); }}
        .lb-prev {{ left: 1.5rem; }} .lb-next {{ right: 1.5rem; }}
        .lb-close {{ position: absolute; top: 1rem; right: 1rem; background: none; border: none;
                     color: rgba(255,255,255,0.6); font-size: 1.5rem; cursor: pointer; padding: 0.5rem; }}
        .lb-close:hover {{ color: white; }}
    </style>
</head>
<body>
    <div class="container">
        <a href="/content" class="back">← Flight History</a>
        <div class="route">
            <span class="icao">{dep}</span>
            <span class="arrow">→</span>
            <span class="icao">{arr_display}</span>
        </div>
        <div class="airframe">{airframe}</div>
        <div class="meta">{simulator} • {date_str}</div>
        <div class="pilot">Flown by {pilot}</div>
        {landing_badge}
        {gallery_html}
    </div>

    <div id="lightbox" class="lightbox" onclick="lbBackdropClick(event)">
        <button class="lb-close" onclick="closeLightbox()">✕</button>
        <button class="lb-btn lb-prev" id="lb-prev" onclick="lbNav(-1)">‹</button>
        <img id="lb-img" class="lightbox-img" src="" alt="">
        <button class="lb-btn lb-next" id="lb-next" onclick="lbNav(1)">›</button>
        <div id="lb-counter" class="lightbox-counter"></div>
    </div>
    <script>
        var lbImages = [], lbIdx = 0;
        function openLightbox(urls, idx) {{ lbImages = urls; lbIdx = idx; updateLb(); document.getElementById('lightbox').classList.add('open'); document.body.style.overflow = 'hidden'; }}
        function closeLightbox() {{ document.getElementById('lightbox').classList.remove('open'); document.body.style.overflow = ''; }}
        function lbNav(dir) {{ lbIdx = (lbIdx + dir + lbImages.length) % lbImages.length; updateLb(); }}
        function lbBackdropClick(e) {{ if (e.target === document.getElementById('lightbox')) closeLightbox(); }}
        function updateLb() {{
            document.getElementById('lb-img').src = lbImages[lbIdx];
            var multi = lbImages.length > 1;
            document.getElementById('lb-prev').style.display = multi ? '' : 'none';
            document.getElementById('lb-next').style.display = multi ? '' : 'none';
            document.getElementById('lb-counter').textContent = multi ? (lbIdx + 1) + ' / ' + lbImages.length : '';
        }}
        document.addEventListener('keydown', function(e) {{
            if (!document.getElementById('lightbox').classList.contains('open')) return;
            if (e.key === 'ArrowLeft') lbNav(-1); else if (e.key === 'ArrowRight') lbNav(1); else if (e.key === 'Escape') closeLightbox();
        }});
    </script>
</body>
</html>"#);

    Ok(Html(html).into_response())
}

async fn content_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let user_id = handlers::get_user_id_from_session(&state.db, &headers).await.ok();

    // Query 10 latest flights for this user (empty when not logged in)
    let flights: Vec<(i64, String, Option<String>, serde_json::Value, chrono::DateTime<chrono::Utc>)> = if let Some(uid) = user_id {
        sqlx::query_as(
            "SELECT id, departure, arrival, statistics, created_at \
             FROM flights WHERE user_id = $1 ORDER BY created_at DESC LIMIT 10"
        )
        .bind(uid)
        .fetch_all(&state.db)
        .await?
    } else {
        vec![]
    };

    let flight_ids: Vec<i64> = flights.iter().map(|f| f.0).collect();

    // Bulk-fetch share IDs for these flights
    let raw_shares: Vec<(i64, String)> = if !flight_ids.is_empty() {
        sqlx::query_as(
            "SELECT remote_flight_id, id FROM flight_shares \
             WHERE remote_flight_id = ANY($1) ORDER BY created_at DESC"
        )
        .bind(&flight_ids)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    } else {
        vec![]
    };
    let mut share_by_flight: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for (flight_id, share_id) in raw_shares {
        share_by_flight.entry(flight_id).or_insert(share_id);
    }

    // Bulk-fetch all screenshots for these flights in one query
    let raw_screenshots: Vec<(i64, String)> = if !flight_ids.is_empty() {
        sqlx::query_as(
            "SELECT flight_id, url FROM screenshots WHERE flight_id = ANY($1) ORDER BY flight_id, created_at"
        )
        .bind(&flight_ids)
        .fetch_all(&state.db)
        .await?
    } else {
        vec![]
    };

    let mut screenshots_by_flight: std::collections::HashMap<i64, Vec<String>> = std::collections::HashMap::new();
    for (flight_id, url) in raw_screenshots {
        screenshots_by_flight.entry(flight_id).or_default().push(url);
    }

    let mut flights_html = String::new();
    if flights.is_empty() {
        flights_html = r#"
            <div class="no-flights">
                <svg viewBox="0 0 24 24" width="48" height="48" stroke="currentColor" stroke-width="1.5" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M17.8 19.2L16 11l3.5-3.5A2.13 2.13 0 1 0 16.5 4L13 7.5l-8.2-1.8L3 7l7.5 4L7 15l-3-1L3 15l3 2 2 3 1-1-1-3 4-3.5 4 7.5z"></path></svg>
                <p>No flights logged yet. Start the ButterLog client and take off!</p>
            </div>
        "#.to_string();
    } else {
        flights_html.push_str(r#"<div class="flight-list">"#);
        for flight in flights {
            let flight_id = flight.0;
            let dep = flight.1;
            let arr = flight.2.as_deref().unwrap_or("In Flight");
            let stats = flight.3;
            let date_str = flight.4.format("%B %d, %Y, %H:%M UTC").to_string();
            let flight_screenshots = screenshots_by_flight.get(&flight_id).map(|v| v.as_slice()).unwrap_or_default();

            let airframe = stats.get("airframe_name")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Aircraft");

            let simulator = stats.get("simulator")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Simulator");

            let mut landing_badge = String::new();
            if let Some(landing) = stats.get("landing_snapshot") {
                if let Some(vspd) = landing.get("VSpd").and_then(|v| v.as_f64()) {
                    let gforce_str = landing.get("NormAc")
                        .and_then(|v| v.as_f64())
                        .map(|g| format!(" / {:.2}G", g))
                        .unwrap_or_default();

                    let (class, label) = if vspd >= -150.0 {
                        ("butter", "BUTTER")
                    } else if vspd >= -250.0 {
                        ("smooth", "SMOOTH")
                    } else if vspd >= -350.0 {
                        ("firm", "FIRM")
                    } else {
                        ("hard", "HARD")
                    };

                    landing_badge = format!(
                        r#"<div class="badge badge-{}">{}<br><span class="badge-detail">{:.0} fpm{}</span></div>"#,
                        class, label, vspd.abs(), gforce_str
                    );
                }
            } else {
                landing_badge = r#"<div class="badge badge-ongoing">ONGOING</div>"#.to_string();
            }

            // Build screenshot gallery HTML
            let gallery_html = if !flight_screenshots.is_empty() {
                let urls_json = serde_json::to_string(flight_screenshots).unwrap_or_default();
                let thumbs: String = flight_screenshots.iter().enumerate().map(|(i, url)| {
                    format!(
                        r#"<img class="screenshot-thumb" src="{}" alt="Screenshot {}" loading="lazy" onclick='openLightbox({}, {})'>"#,
                        url, i + 1, urls_json, i
                    )
                }).collect();
                format!(r#"<div class="screenshot-gallery">{}</div>"#, thumbs)
            } else {
                String::new()
            };

            let share_href = share_by_flight.get(&flight_id)
                .map(|sid| format!("/content/flights/share/{}", sid));
            let card_open = match &share_href {
                Some(href) => format!(r#"<a href="{}" class="flight-card-link">"#, href),
                None => r#"<div class="flight-card-link" style="cursor:default">"#.to_string(),
            };
            let card_close = if share_href.is_some() { "</a>" } else { "</div>" };

            flights_html.push_str(&format!(
                r#"
                {card_open}
                <div class="flight-card">
                    <div class="flight-top">
                        <div class="flight-main">
                            <div class="flight-route">
                                <span class="route-icao">{dep}</span>
                                <span class="route-arrow">→</span>
                                <span class="route-icao">{arr}</span>
                            </div>
                            <div class="flight-meta">
                                <div class="airframe">{airframe}</div>
                                <div class="details">{simulator}</div>
                                <div class="date">{date_str}</div>
                            </div>
                        </div>
                        <div class="flight-right">
                            {landing_badge}
                        </div>
                    </div>
                    {gallery_html}
                </div>
                {card_close}
                "#
            ));
        }
        flights_html.push_str("</div>");
    }

    let html_content = format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta name="viewport" content="width=device-width, initial-scale=1.0">
            <title>ButterLog Flight Log</title>
            <meta name="description" content="View your latest telemetry logs on ButterLog.">
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
                    --accent-orange: #fab387;
                    --accent-yellow: #f9e2af;
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

                .container {{
                    max-width: 750px;
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
                    margin-bottom: 2.5rem;
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

                .nav-tabs {{
                    display: flex;
                    justify-content: center;
                    gap: 1rem;
                    margin-bottom: 3rem;
                    border-bottom: 1px solid var(--border-color);
                    padding-bottom: 1rem;
                }}

                .nav-tab {{
                    color: var(--text-muted);
                    text-decoration: none;
                    font-weight: 500;
                    font-size: 1rem;
                    padding: 0.5rem 1rem;
                    border-radius: 8px;
                    transition: all 0.2s;
                }}

                .nav-tab:hover {{
                    color: var(--text-primary);
                    background: rgba(255, 255, 255, 0.05);
                }}

                .nav-tab.active {{
                    color: var(--primary-color);
                    background: rgba(203, 166, 247, 0.1);
                }}

                .flight-list {{
                    display: flex;
                    flex-direction: column;
                    gap: 1.5rem;
                }}

                .flight-card-link {{
                    text-decoration: none;
                    color: inherit;
                    display: block;
                }}

                .flight-card {{
                    background: var(--card-bg);
                    border: 1px solid var(--border-color);
                    border-radius: 20px;
                    padding: 1.5rem 2rem;
                    display: flex;
                    flex-direction: column;
                    gap: 0;
                    transition: transform 0.3s cubic-bezier(0.16, 1, 0.3, 1), border-color 0.3s;
                }}

                .flight-card:hover {{
                    transform: translateY(-4px);
                    border-color: rgba(203, 166, 247, 0.3);
                }}

                .flight-top {{
                    display: flex;
                    justify-content: space-between;
                    align-items: center;
                }}

                .screenshot-gallery {{
                    display: flex;
                    gap: 8px;
                    overflow-x: auto;
                    padding-top: 1rem;
                    margin-top: 1rem;
                    border-top: 1px solid var(--border-color);
                    scrollbar-width: thin;
                    scrollbar-color: var(--border-color) transparent;
                }}

                .screenshot-gallery::-webkit-scrollbar {{
                    height: 4px;
                }}

                .screenshot-gallery::-webkit-scrollbar-thumb {{
                    background: var(--border-color);
                    border-radius: 2px;
                }}

                .screenshot-thumb {{
                    width: 140px;
                    height: 88px;
                    object-fit: cover;
                    border-radius: 8px;
                    cursor: pointer;
                    flex-shrink: 0;
                    border: 1px solid var(--border-color);
                    transition: transform 0.2s, border-color 0.2s;
                }}

                .screenshot-thumb:hover {{
                    transform: scale(1.04);
                    border-color: rgba(203, 166, 247, 0.5);
                }}

                .lightbox {{
                    display: none;
                    position: fixed;
                    inset: 0;
                    background: rgba(0, 0, 0, 0.92);
                    z-index: 1000;
                    align-items: center;
                    justify-content: center;
                    flex-direction: column;
                    gap: 1rem;
                }}

                .lightbox.open {{
                    display: flex;
                }}

                .lightbox-img {{
                    max-width: 90vw;
                    max-height: 82vh;
                    object-fit: contain;
                    border-radius: 10px;
                    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.6);
                }}

                .lightbox-counter {{
                    color: rgba(255,255,255,0.6);
                    font-size: 0.9rem;
                }}

                .lb-btn {{
                    position: absolute;
                    top: 50%;
                    transform: translateY(-50%);
                    background: rgba(255,255,255,0.08);
                    border: 1px solid rgba(255,255,255,0.12);
                    color: white;
                    font-size: 1.8rem;
                    width: 3rem;
                    height: 3rem;
                    border-radius: 50%;
                    cursor: pointer;
                    display: flex;
                    align-items: center;
                    justify-content: center;
                    transition: background 0.2s;
                    line-height: 1;
                }}

                .lb-btn:hover {{ background: rgba(255,255,255,0.15); }}
                .lb-prev {{ left: 1.5rem; }}
                .lb-next {{ right: 1.5rem; }}

                .lb-close {{
                    position: absolute;
                    top: 1rem;
                    right: 1rem;
                    background: none;
                    border: none;
                    color: rgba(255,255,255,0.6);
                    font-size: 1.5rem;
                    cursor: pointer;
                    padding: 0.5rem;
                    transition: color 0.2s;
                }}

                .lb-close:hover {{ color: white; }}

                .flight-main {{
                    display: flex;
                    flex-direction: column;
                    gap: 0.5rem;
                }}

                .flight-route {{
                    display: flex;
                    align-items: center;
                    gap: 0.75rem;
                }}

                .route-icao {{
                    font-size: 1.6rem;
                    font-weight: 700;
                    letter-spacing: -0.5px;
                    color: var(--text-primary);
                }}

                .route-arrow {{
                    font-size: 1.4rem;
                    color: var(--primary-color);
                }}

                .flight-meta {{
                    display: flex;
                    flex-direction: column;
                    gap: 0.25rem;
                }}

                .airframe {{
                    font-size: 1.1rem;
                    font-weight: 500;
                    color: var(--accent-pink);
                }}

                .details {{
                    font-size: 0.9rem;
                    color: var(--text-secondary);
                }}

                .date {{
                    font-size: 0.85rem;
                    color: var(--text-muted);
                }}

                .flight-right {{
                    display: flex;
                    flex-direction: column;
                    align-items: flex-end;
                    gap: 0.75rem;
                }}

                .badge {{
                    padding: 0.6rem 1.2rem;
                    border-radius: 12px;
                    font-weight: 700;
                    font-size: 0.85rem;
                    text-align: center;
                    box-shadow: 0 4px 12px rgba(0,0,0,0.15);
                    line-height: 1.2;
                }}

                .badge-detail {{
                    font-size: 0.75rem;
                    font-weight: 400;
                    opacity: 0.9;
                }}

                .badge-butter {{
                    background: linear-gradient(135deg, #a6e3a1, #89b4fa);
                    color: #11111b;
                }}

                .badge-smooth {{
                    background: linear-gradient(135deg, #94e2d5, #a6e3a1);
                    color: #11111b;
                }}

                .badge-firm {{
                    background: linear-gradient(135deg, #fab387, #f9e2af);
                    color: #11111b;
                }}

                .badge-hard {{
                    background: linear-gradient(135deg, #f38ba8, #eba0ac);
                    color: #11111b;
                }}

                .badge-ongoing {{
                    background: rgba(255, 255, 255, 0.05);
                    border: 1px solid var(--border-color);
                    color: var(--text-secondary);
                }}

                .no-flights {{
                    text-align: center;
                    padding: 4rem 2rem;
                    color: var(--text-muted);
                }}

                .no-flights svg {{
                    margin-bottom: 1.5rem;
                    opacity: 0.5;
                }}
            </style>
        </head>
        <body>
            <div class="container">
                <div class="header">
                    <h1>ButterLog history</h1>
                    <p class="subtitle">Telemetry records and landing reports</p>
                </div>
                
                <div class="nav-tabs">
                    <a href="/content" class="nav-tab active">Flight History</a>
                    <a href="/content/settings" class="nav-tab">Notification Settings</a>
                    <a href="/map" class="nav-tab">Live Map</a>
                </div>

                {}
            </div>

            <div id="lightbox" class="lightbox" onclick="lbBackdropClick(event)">
                <button class="lb-close" onclick="closeLightbox()" aria-label="Close">✕</button>
                <button class="lb-btn lb-prev" id="lb-prev" onclick="lbNav(-1)" aria-label="Previous">‹</button>
                <img id="lb-img" class="lightbox-img" src="" alt="">
                <button class="lb-btn lb-next" id="lb-next" onclick="lbNav(1)" aria-label="Next">›</button>
                <div id="lb-counter" class="lightbox-counter"></div>
            </div>
            <script>
                var lbImages = [], lbIdx = 0;
                function openLightbox(urls, idx) {{
                    lbImages = urls; lbIdx = idx;
                    updateLb();
                    document.getElementById('lightbox').classList.add('open');
                    document.body.style.overflow = 'hidden';
                }}
                function closeLightbox() {{
                    document.getElementById('lightbox').classList.remove('open');
                    document.body.style.overflow = '';
                }}
                function lbNav(dir) {{
                    lbIdx = (lbIdx + dir + lbImages.length) % lbImages.length;
                    updateLb();
                }}
                function lbBackdropClick(e) {{
                    if (e.target === document.getElementById('lightbox')) closeLightbox();
                }}
                function updateLb() {{
                    document.getElementById('lb-img').src = lbImages[lbIdx];
                    var multi = lbImages.length > 1;
                    document.getElementById('lb-prev').style.display = multi ? '' : 'none';
                    document.getElementById('lb-next').style.display = multi ? '' : 'none';
                    document.getElementById('lb-counter').textContent = multi ? (lbIdx + 1) + ' / ' + lbImages.length : '';
                }}
                document.addEventListener('keydown', function(e) {{
                    if (!document.getElementById('lightbox').classList.contains('open')) return;
                    if (e.key === 'ArrowLeft') lbNav(-1);
                    else if (e.key === 'ArrowRight') lbNav(1);
                    else if (e.key === 'Escape') closeLightbox();
                }});
            </script>
        </body>
        </html>
        "#,
        flights_html
    );

    Ok(Html(html_content).into_response())
}

#[derive(Serialize)]
struct MapAircraft {
    flight_id: i64,
    pilot_name: String,
    departure: String,
    arrival: String,
    aircraft_type: String,
    latitude: f64,
    longitude: f64,
    altitude: f64,
    heading: f64,
    speed: f64,
    updated_ago_secs: i64,
}

async fn map_data_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let active_flights: Vec<(i64, String, Option<String>, serde_json::Value, chrono::DateTime<chrono::Utc>, String, Option<String>)> = sqlx::query_as(
        "SELECT f.id, f.departure, f.arrival, f.statistics, f.updated_at, u.username, u.global_name \
         FROM flights f \
         JOIN users u ON f.user_id = u.id \
         WHERE f.updated_at > NOW() - INTERVAL '5 minutes'"
    )
    .fetch_all(&state.db)
    .await?;

    let mut aircrafts = Vec::new();
    let now = chrono::Utc::now();

    for flight in active_flights {
        let statistics = flight.3;
        if let Some(snapshot) = statistics.get("current_snapshot") {
            let lat = snapshot.get("Latitude").and_then(|v| v.as_f64())
                .or_else(|| snapshot.get("latitude").and_then(|v| v.as_f64()));
            let lon = snapshot.get("Longitude").and_then(|v| v.as_f64())
                .or_else(|| snapshot.get("longitude").and_then(|v| v.as_f64()));

            if let (Some(latitude), Some(longitude)) = (lat, lon) {
                let altitude = snapshot.get("AltMSL").and_then(|v| v.as_f64())
                    .or_else(|| snapshot.get("gps_altitude_msl").and_then(|v| v.as_f64()))
                    .or_else(|| snapshot.get("AltB").and_then(|v| v.as_f64()))
                    .unwrap_or(0.0);

                let heading = snapshot.get("HDG").and_then(|v| v.as_f64())
                    .or_else(|| snapshot.get("heading").and_then(|v| v.as_f64()))
                    .unwrap_or(0.0);

                let speed = snapshot.get("GndSpd").and_then(|v| v.as_f64())
                    .or_else(|| snapshot.get("ground_speed").and_then(|v| v.as_f64()))
                    .unwrap_or(0.0);

                let pilot_name = flight.6.unwrap_or(flight.5);
                let departure = flight.1;
                let arrival = flight.2.unwrap_or_else(|| "In Flight".to_string());
                let aircraft_type = statistics.get("airframe_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string();

                let updated_ago_secs = now.signed_duration_since(flight.4).num_seconds();

                aircrafts.push(MapAircraft {
                    flight_id: flight.0,
                    pilot_name,
                    departure,
                    arrival,
                    aircraft_type,
                    latitude,
                    longitude,
                    altitude,
                    heading,
                    speed,
                    updated_ago_secs,
                });
            }
        }
    }

    Ok(axum::Json(aircrafts))
}

async fn map_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let html_content = r##"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>ButterLog Live Traffic Map</title>
    <meta name="description" content="View live flight telemetry and coordinates mapping in real-time.">
    
    <!-- CSS & Fonts -->
    <link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" integrity="sha256-p4NxAoJBhIIN+hmNHrzRCf9tD/miZyoHS5obTRR9BMY=" crossorigin=""/>
    <script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js" integrity="sha256-20nQCchB9co0qIjJZRGuk2/Z9VM+kNiyxNV1lvTlZBo=" crossorigin=""></script>
    <link href="https://fonts.googleapis.com/css2?family=Outfit:wght@300;400;500;600;700&display=swap" rel="stylesheet">
    
    <style>
        :root {
            --bg-gradient: radial-gradient(circle at 50% 0%, #1e1e3f 0%, #0d0d16 100%);
            --panel-bg: rgba(17, 17, 27, 0.7);
            --border-color: rgba(255, 255, 255, 0.08);
            --text-primary: #cdd6f4;
            --text-secondary: #a6adc8;
            --text-muted: #6c7086;
            --primary-color: #cba6f7;
            --primary-hover: #b4befe;
            --accent-pink: #f5c2e7;
            --accent-green: #a6e3a1;
            --accent-red: #f38ba8;
            --accent-orange: #fab387;
            --card-bg: rgba(30, 30, 46, 0.5);
        }

        body {
            font-family: 'Outfit', sans-serif;
            margin: 0;
            padding: 0;
            background: #0d0d16;
            color: var(--text-primary);
            height: 100vh;
            display: flex;
            flex-direction: column;
            overflow: hidden;
        }

        #map {
            width: 100%;
            height: 100%;
            position: absolute;
            top: 0;
            left: 0;
            z-index: 1;
        }

        /* Glassmorphic Navbar Overlay */
        .top-navbar {
            position: absolute;
            top: 1.5rem;
            left: 50%;
            transform: translateX(-50%);
            z-index: 10;
            display: flex;
            align-items: center;
            gap: 2rem;
            padding: 0.75rem 2rem;
            background: var(--panel-bg);
            backdrop-filter: blur(16px);
            -webkit-backdrop-filter: blur(16px);
            border: 1px solid var(--border-color);
            border-radius: 20px;
            box-shadow: 0 10px 30px rgba(0, 0, 0, 0.4);
            transition: all 0.3s cubic-bezier(0.16, 1, 0.3, 1);
        }

        .navbar-brand {
            font-size: 1.25rem;
            font-weight: 700;
            background: linear-gradient(90deg, var(--primary-color), var(--accent-pink));
            -webkit-background-clip: text;
            -webkit-text-fill-color: transparent;
            letter-spacing: -0.5px;
            text-decoration: none;
        }

        .nav-links {
            display: flex;
            gap: 0.75rem;
        }

        .nav-link {
            color: var(--text-muted);
            text-decoration: none;
            font-weight: 500;
            font-size: 0.9rem;
            padding: 0.4rem 0.8rem;
            border-radius: 8px;
            transition: all 0.2s;
        }

        .nav-link:hover {
            color: var(--text-primary);
            background: rgba(255, 255, 255, 0.05);
        }

        .nav-link.active {
            color: var(--primary-color);
            background: rgba(203, 166, 247, 0.12);
        }

        /* Glassmorphic Sidebar Overlay */
        .sidebar {
            position: absolute;
            top: 6rem;
            right: 1.5rem;
            bottom: 1.5rem;
            width: 380px;
            z-index: 10;
            background: var(--panel-bg);
            backdrop-filter: blur(20px);
            -webkit-backdrop-filter: blur(20px);
            border: 1px solid var(--border-color);
            border-radius: 24px;
            box-shadow: 0 20px 50px rgba(0, 0, 0, 0.5);
            display: flex;
            flex-direction: column;
            overflow: hidden;
            box-sizing: border-box;
            transition: transform 0.3s ease;
        }

        .sidebar-header {
            padding: 1.5rem;
            border-bottom: 1px solid rgba(255, 255, 255, 0.06);
            display: flex;
            justify-content: space-between;
            align-items: center;
        }

        .sidebar-title {
            font-size: 1.2rem;
            font-weight: 600;
            color: var(--text-primary);
            display: flex;
            align-items: center;
            gap: 0.5rem;
        }

        .pulse-dot {
            width: 8px;
            height: 8px;
            background-color: var(--accent-green);
            border-radius: 50%;
            box-shadow: 0 0 0 0 rgba(166, 227, 161, 0.7);
            animation: pulse 1.6s infinite;
        }

        @keyframes pulse {
            0% {
                transform: scale(0.95);
                box-shadow: 0 0 0 0 rgba(166, 227, 161, 0.7);
            }
            70% {
                transform: scale(1);
                box-shadow: 0 0 0 8px rgba(166, 227, 161, 0);
            }
            100% {
                transform: scale(0.95);
                box-shadow: 0 0 0 0 rgba(166, 227, 161, 0);
            }
        }

        .active-count {
            font-size: 0.8rem;
            background: rgba(255, 255, 255, 0.06);
            padding: 0.25rem 0.6rem;
            border-radius: 12px;
            color: var(--text-secondary);
            font-weight: 500;
        }

        .aircraft-list {
            flex: 1;
            overflow-y: auto;
            padding: 1rem;
            display: flex;
            flex-direction: column;
            gap: 0.75rem;
        }

        .aircraft-list::-webkit-scrollbar {
            width: 6px;
        }

        .aircraft-list::-webkit-scrollbar-thumb {
            background: rgba(255, 255, 255, 0.08);
            border-radius: 3px;
        }

        .aircraft-card {
            background: var(--card-bg);
            border: 1px solid rgba(255, 255, 255, 0.03);
            border-radius: 16px;
            padding: 1rem;
            cursor: pointer;
            transition: all 0.2s cubic-bezier(0.16, 1, 0.3, 1);
            display: flex;
            flex-direction: column;
            gap: 0.5rem;
        }

        .aircraft-card:hover {
            border-color: rgba(203, 166, 247, 0.3);
            transform: translateY(-2px);
            background: rgba(30, 30, 46, 0.75);
        }

        .aircraft-card-header {
            display: flex;
            justify-content: space-between;
            align-items: center;
        }

        .pilot-name {
            font-weight: 600;
            color: var(--text-primary);
            font-size: 0.95rem;
        }

        .aircraft-type {
            font-size: 0.8rem;
            font-weight: 600;
            background: rgba(203, 166, 247, 0.15);
            color: var(--primary-color);
            padding: 0.15rem 0.5rem;
            border-radius: 6px;
            text-transform: uppercase;
        }

        .aircraft-route {
            display: flex;
            align-items: center;
            gap: 0.5rem;
            font-size: 1.1rem;
            font-weight: 700;
            color: var(--accent-pink);
        }

        .route-arrow {
            color: var(--text-muted);
            font-size: 0.9rem;
        }

        .aircraft-stats {
            display: grid;
            grid-template-columns: 1fr 1fr;
            gap: 0.5rem;
            font-size: 0.8rem;
            color: var(--text-secondary);
            border-top: 1px solid rgba(255, 255, 255, 0.04);
            padding-top: 0.5rem;
            margin-top: 0.25rem;
        }

        .stat-item {
            display: flex;
            align-items: center;
            gap: 0.35rem;
        }

        .stat-value {
            font-weight: 600;
            color: var(--text-primary);
        }

        .update-time {
            font-size: 0.75rem;
            color: var(--text-muted);
            align-self: flex-end;
        }

        .no-aircraft {
            text-align: center;
            padding: 3rem 1.5rem;
            color: var(--text-muted);
            display: flex;
            flex-direction: column;
            align-items: center;
            gap: 1rem;
            justify-content: center;
            height: 100%;
        }

        .no-aircraft svg {
            opacity: 0.3;
            animation: float 3s ease-in-out infinite;
        }

        @keyframes float {
            0% { transform: translateY(0px); }
            50% { transform: translateY(-10px); }
            100% { transform: translateY(0px); }
        }

        /* Leaflet Overrides */
        .leaflet-bar {
            border: 1px solid var(--border-color) !important;
            border-radius: 12px !important;
            overflow: hidden;
            box-shadow: 0 10px 30px rgba(0, 0, 0, 0.3) !important;
        }

        .leaflet-bar a {
            background: rgba(30, 30, 46, 0.85) !important;
            backdrop-filter: blur(8px);
            color: var(--text-primary) !important;
            border-bottom: 1px solid var(--border-color) !important;
        }

        .leaflet-bar a:hover {
            background: rgba(30, 30, 46, 0.95) !important;
            color: var(--primary-color) !important;
        }

        .leaflet-container {
            background: #09090e !important;
        }

        /* Plane Marker Icon Class */
        .plane-marker {
            transition: transform 0.5s cubic-bezier(0.16, 1, 0.3, 1);
            filter: drop-shadow(0 4px 6px rgba(0, 0, 0, 0.5));
        }

        .plane-marker svg {
            display: block;
        }

        /* Leaflet Popup customization */
        .leaflet-popup-content-wrapper {
            background: rgba(17, 17, 27, 0.9) !important;
            backdrop-filter: blur(10px);
            border: 1px solid var(--border-color) !important;
            border-radius: 16px !important;
            color: var(--text-primary) !important;
            box-shadow: 0 15px 30px rgba(0, 0, 0, 0.4) !important;
            padding: 0.25rem;
        }

        .leaflet-popup-tip {
            background: rgba(17, 17, 27, 0.9) !important;
            border: 1px solid var(--border-color) !important;
        }

        .popup-title {
            font-weight: 700;
            font-size: 1rem;
            color: var(--primary-color);
            margin-bottom: 0.25rem;
        }

        .popup-subtitle {
            font-size: 0.8rem;
            color: var(--text-secondary);
            margin-bottom: 0.5rem;
            border-bottom: 1px solid rgba(255, 255, 255, 0.08);
            padding-bottom: 0.25rem;
        }

        .popup-detail {
            font-size: 0.8rem;
            margin-bottom: 0.15rem;
        }

        .popup-detail span {
            font-weight: 600;
            color: var(--text-primary);
        }

        /* Responsive Layout */
        @media (max-width: 768px) {
            .sidebar {
                width: calc(100% - 2rem);
                height: 250px;
                top: auto;
                left: 1rem;
                right: 1rem;
                bottom: 1rem;
            }

            .top-navbar {
                width: calc(100% - 2rem);
                padding: 0.5rem 1rem;
                gap: 0.5rem;
                justify-content: space-between;
            }

            .nav-links {
                gap: 0.25rem;
            }

            .nav-link {
                font-size: 0.8rem;
                padding: 0.3rem 0.5rem;
            }
        }
    </style>
</head>
<body>

    <nav class="top-navbar">
        <a href="/content" class="navbar-brand">ButterLog</a>
        <div class="nav-links">
            <a href="/content" class="nav-link">Flight History</a>
            <a href="/content/settings" class="nav-link">Notification Settings</a>
            <a href="/map" class="nav-link active">Live Map</a>
        </div>
    </nav>

    <div id="map"></div>

    <aside class="sidebar">
        <header class="sidebar-header">
            <div class="sidebar-title">
                <span class="pulse-dot"></span>
                <span>Live Traffic</span>
            </div>
            <span class="active-count" id="active-count">0 pilots online</span>
        </header>
        
        <div class="aircraft-list" id="aircraft-list">
            <div class="no-aircraft">
                <svg viewBox="0 0 24 24" width="48" height="48" stroke="currentColor" stroke-width="1.5" fill="none" stroke-linecap="round" stroke-linejoin="round">
                    <path d="M17.8 19.2L16 11l3.5-3.5A2.13 2.13 0 1 0 16.5 4L13 7.5l-8.2-1.8L3 7l7.5 4L7 15l-3-1L3 15l3 2 2 3 1-1-1-3 4-3.5 4 7.5z"></path>
                </svg>
                <p style="margin: 0;">No active aircraft reporting.</p>
                <span style="font-size: 0.8rem; color: var(--text-muted);">Start the client and fly to see yourself on the map!</span>
            </div>
        </div>
    </aside>

    <script>
        // Init map
        const map = L.map('map', {
            zoomControl: false,
            attributionControl: false
        }).setView([20, 0], 2);

        // Add zoom control top left
        L.control.zoom({ position: 'topleft' }).addTo(map);

        // Dark Matter tiles
        L.tileLayer('https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png', {
            maxZoom: 20
        }).addTo(map);

        // Store active markers
        const markers = new Map();
        let isFirstLoad = true;

        async function updateMap() {
            try {
                const response = await fetch('/api/v0/map/data');
                if (!response.ok) throw new Error('Failed to fetch traffic data');
                const aircrafts = await response.json();
                
                // Track IDs present in this update
                const currentIds = new Set();
                
                // Update header count
                document.getElementById('active-count').innerText = `${aircrafts.length} pilot${aircrafts.length === 1 ? '' : 's'} online`;
                
                const listContainer = document.getElementById('aircraft-list');
                
                if (aircrafts.length === 0) {
                    listContainer.innerHTML = `
                        <div class="no-aircraft">
                            <svg viewBox="0 0 24 24" width="48" height="48" stroke="currentColor" stroke-width="1.5" fill="none" stroke-linecap="round" stroke-linejoin="round">
                                <path d="M17.8 19.2L16 11l3.5-3.5A2.13 2.13 0 1 0 16.5 4L13 7.5l-8.2-1.8L3 7l7.5 4L7 15l-3-1L3 15l3 2 2 3 1-1-1-3 4-3.5 4 7.5z"></path>
                            </svg>
                            <p style="margin: 0;">No active aircraft reporting.</p>
                            <span style="font-size: 0.8rem; color: var(--text-muted);">Start the client and fly to see yourself on the map!</span>
                        </div>
                    `;
                    
                    // Clear all markers
                    markers.forEach(({ marker }) => map.removeLayer(marker));
                    markers.clear();
                    return;
                }
                
                // Build sidebar HTML
                let listHtml = '';
                
                aircrafts.forEach(ac => {
                    currentIds.add(ac.flight_id);
                    
                    // Check if marker exists, update or create
                    const position = [ac.latitude, ac.longitude];
                    
                    // Create Custom Plane Icon
                    const planeIcon = L.divIcon({
                        html: `<div class="plane-marker" style="transform: rotate(${ac.heading}deg);">
                                 <svg viewBox="0 0 24 24" width="32" height="32" fill="#cba6f7" stroke="#11111b" stroke-width="1.2">
                                   <path d="M21 16v-2l-8-5V3.5c0-.83-.67-1.5-1.5-1.5S10 2.67 10 3.5V9l-8 5v2l8-2.5V19l-2 1.5V22l3.5-1 3.5 1v-1.5L14 19v-5.5l8 2.5z"/>
                                 </svg>
                               </div>`,
                        className: 'custom-div-icon',
                        iconSize: [32, 32],
                        iconAnchor: [16, 16]
                    });
                    
                    const popupContent = `
                        <div class="popup-title">${ac.pilot_name}</div>
                        <div class="popup-subtitle">${ac.aircraft_type}</div>
                        <div class="popup-detail">Route: <span>${ac.departure} ➔ ${ac.arrival}</span></div>
                        <div class="popup-detail">Altitude: <span>${Math.round(ac.altitude).toLocaleString()} ft</span></div>
                        <div class="popup-detail">Speed: <span>${Math.round(ac.speed)} kts</span></div>
                        <div class="popup-detail">Heading: <span>${Math.round(ac.heading)}°</span></div>
                    `;
                    
                    if (markers.has(ac.flight_id)) {
                        const { marker } = markers.get(ac.flight_id);
                        marker.setLatLng(position);
                        marker.setIcon(planeIcon);
                        marker.setPopupContent(popupContent);
                    } else {
                        const marker = L.marker(position, { icon: planeIcon }).addTo(map);
                        marker.bindPopup(popupContent);
                        markers.set(ac.flight_id, { marker, data: ac });
                    }
                    
                    // Add card to sidebar list
                    listHtml += `
                        <div class="aircraft-card" onclick="focusAircraft(${ac.latitude}, ${ac.longitude}, ${ac.flight_id})">
                            <div class="aircraft-card-header">
                                <span class="pilot-name">${ac.pilot_name}</span>
                                <span class="aircraft-type">${ac.aircraft_type}</span>
                            </div>
                            <div class="aircraft-route">
                                <span>${ac.departure}</span>
                                <span class="route-arrow">➔</span>
                                <span>${ac.arrival}</span>
                            </div>
                            <div class="aircraft-stats">
                                <div class="stat-item">
                                    <svg viewBox="0 0 24 24" width="12" height="12" stroke="currentColor" stroke-width="2" fill="none"><path d="M12 2v20M17 5H9.5a3.5 3.5 0 0 0 0 7h5a3.5 3.5 0 0 1 0 7H6"></path></svg>
                                    <span>Alt: <span class="stat-value">${Math.round(ac.altitude).toLocaleString()} ft</span></span>
                                </div>
                                <div class="stat-item">
                                    <svg viewBox="0 0 24 24" width="12" height="12" stroke="currentColor" stroke-width="2" fill="none"><path d="M13 2L3 14h9l-1 8 10-12h-9l1-8z"></path></svg>
                                    <span>Spd: <span class="stat-value">${Math.round(ac.speed)} kts</span></span>
                                </div>
                            </div>
                            <div class="update-time">Updated ${ac.updated_ago_secs}s ago</div>
                        </div>
                    `;
                });
                
                listContainer.innerHTML = listHtml;
                
                // Remove markers that are no longer active
                markers.forEach((val, id) => {
                    if (!currentIds.has(id)) {
                        map.removeLayer(val.marker);
                        markers.delete(id);
                    }
                });
                
                // Adjust map view on first load to encompass active planes
                if (isFirstLoad && markers.size > 0) {
                    const group = new L.featureGroup(Array.from(markers.values()).map(m => m.marker));
                    map.fitBounds(group.getBounds().pad(0.3));
                    isFirstLoad = false;
                }
            } catch (err) {
                console.error(err);
            }
        }

        function focusAircraft(lat, lon, flightId) {
            map.setView([lat, lon], 10);
            const entry = markers.get(flightId);
            if (entry) {
                entry.marker.openPopup();
            }
        }

        // Run updates
        updateMap();
        setInterval(updateMap, 5000);
    </script>
</body>
</html>"##;

    Ok(Html(html_content).into_response())
}

async fn flight_share_detail_handler(
    State(state): State<AppState>,
    axum::extract::Path(share_id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT r2_key, user_id FROM flight_shares WHERE id = $1"
    )
    .bind(&share_id)
    .fetch_optional(&state.db)
    .await?;

    let (key, share_owner_id) = match row {
        Some(r) => r,
        None => return Ok((axum::http::StatusCode::NOT_FOUND, Html("<h1>Share not found</h1>".to_string())).into_response()),
    };

    // Check if the logged-in user owns this share
    let logged_in_user_id = handlers::get_user_id_from_session(&state.db, &headers).await.ok();
    let is_owner = logged_in_user_id.is_some() && logged_in_user_id == share_owner_id;

    let compressed = match state.r2.download_object(&key).await {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Failed to download share {}: {}", share_id, e);
            return Ok((axum::http::StatusCode::INTERNAL_SERVER_ERROR, Html("<h1>Failed to load share data</h1>".to_string())).into_response());
        }
    };

    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut json_str = String::new();
    if let Err(e) = decoder.read_to_string(&mut json_str) {
        tracing::error!("Failed to decompress share {}: {}", share_id, e);
        return Ok((axum::http::StatusCode::INTERNAL_SERVER_ERROR, Html("<h1>Failed to decompress share data</h1>".to_string())).into_response());
    }

    let json_escaped = json_str.replace('\\', "\\\\").replace("</", "<\\/");

    let delete_button = if is_owner {
        format!(
            r#"<button onclick="deleteShare()" style="background:rgba(243,139,168,0.12);color:#f38ba8;border:1px solid rgba(243,139,168,0.3);border-radius:8px;padding:0.5rem 1rem;cursor:pointer;font-family:inherit;font-size:0.9rem" id="delete-btn">Delete Share</button>
<script>
async function deleteShare(){{
    if(!confirm('Delete this shared flight? This cannot be undone.'))return;
    document.getElementById('delete-btn').textContent='Deleting...';
    document.getElementById('delete-btn').disabled=true;
    const res=await fetch('/api/v0/flights/share/{share_id}',{{method:'DELETE',credentials:'include'}});
    if(res.ok||res.status===204){{window.location.href='/content';}}
    else{{document.getElementById('delete-btn').textContent='Delete Share';document.getElementById('delete-btn').disabled=false;alert('Delete failed');}}
}}
</script>"#,
            share_id = share_id
        )
    } else {
        String::new()
    };

    let html = format!(r##"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>ButterLog Flight Share</title>
    <meta property="og:site_name" content="ButterLog">
    <link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" crossorigin=""/>
    <script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js" crossorigin=""></script>
    <script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.0/dist/chart.umd.min.js"></script>
    <link href="https://fonts.googleapis.com/css2?family=Outfit:wght@300;400;500;600;700&display=swap" rel="stylesheet">
    <style>
        :root {{
            --bg: #0d0d16; --panel: rgba(30,30,46,0.5); --border: rgba(255,255,255,0.08);
            --text: #cdd6f4; --muted: #a6adc8; --dim: #6c7086;
            --purple: #cba6f7; --pink: #f5c2e7; --green: #a6e3a1; --blue: #89b4fa; --red: #f38ba8; --yellow: #f9e2af;
        }}
        * {{ box-sizing: border-box; margin: 0; padding: 0; }}
        body {{ font-family: 'Outfit', sans-serif; background: var(--bg); color: var(--text); min-height: 100vh; }}
        .page {{ max-width: 900px; margin: 0 auto; padding: 2rem 1rem 4rem; }}
        .back {{ color: var(--dim); text-decoration: none; font-size: 0.9rem; display: inline-flex; align-items: center; gap: 0.4rem; margin-bottom: 1.5rem; }}
        .back:hover {{ color: var(--purple); }}
        .route {{ display: flex; align-items: center; gap: 1rem; margin-bottom: 0.4rem; }}
        .icao {{ font-size: 2.2rem; font-weight: 700; }}
        .arrow {{ font-size: 2rem; color: var(--purple); }}
        .aircraft {{ font-size: 1.1rem; color: var(--pink); font-weight: 500; margin-bottom: 0.25rem; }}
        .meta {{ color: var(--muted); font-size: 0.9rem; margin-bottom: 1rem; }}
        .badge {{ display: inline-block; padding: 0.4rem 0.9rem; border-radius: 8px; font-weight: 700; font-size: 0.8rem; margin-bottom: 1.5rem; }}
        .badge-butter {{ background: linear-gradient(135deg, #a6e3a1, #89b4fa); color: #11111b; }}
        .badge-smooth {{ background: linear-gradient(135deg, #94e2d5, #a6e3a1); color: #11111b; }}
        .badge-firm {{ background: linear-gradient(135deg, #fab387, #f9e2af); color: #11111b; }}
        .badge-hard {{ background: linear-gradient(135deg, #f38ba8, #eba0ac); color: #11111b; }}
        .stats-grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 1rem; margin-bottom: 2rem; }}
        .stat-card {{ background: var(--panel); border: 1px solid var(--border); border-radius: 12px; padding: 1.2rem; text-align: center; }}
        .stat-label {{ color: var(--muted); font-size: 0.78rem; text-transform: uppercase; letter-spacing: 1px; margin-bottom: 0.4rem; }}
        .stat-value {{ font-size: 1.4rem; font-weight: 700; }}
        .landing-card {{ background: var(--panel); border: 1px solid var(--border); border-radius: 12px; padding: 1.5rem; margin-bottom: 2rem; }}
        .section-title {{ font-size: 0.8rem; text-transform: uppercase; letter-spacing: 1.5px; color: var(--pink); font-weight: 600; margin-bottom: 0.75rem; }}
        .landing-grid {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(130px, 1fr)); gap: 1rem; }}
        .li-label {{ color: var(--dim); font-size: 0.72rem; font-weight: 700; text-transform: uppercase; margin-bottom: 0.2rem; }}
        .li-val {{ font-size: 1.3rem; font-weight: 700; }}
        #map {{ height: 400px; border-radius: 12px; margin-bottom: 2rem; border: 1px solid var(--border); }}
        .charts {{ display: grid; grid-template-columns: 1fr 1fr; gap: 1rem; margin-bottom: 2rem; }}
        @media (max-width: 600px) {{ .charts {{ grid-template-columns: 1fr; }} }}
        .chart-card {{ background: var(--panel); border: 1px solid var(--border); border-radius: 12px; padding: 1rem; }}
        .chart-card canvas {{ max-height: 200px; }}
        .gallery {{ display: flex; gap: 8px; overflow-x: auto; padding-bottom: 0.5rem; scrollbar-width: thin; }}
        .gallery img {{ width: 160px; height: 100px; object-fit: cover; border-radius: 8px; flex-shrink: 0; border: 1px solid var(--border); cursor: pointer; transition: transform 0.2s; }}
        .gallery img:hover {{ transform: scale(1.04); }}
        .gallery-section {{ margin-bottom: 2rem; }}
        .lightbox {{ display: none; position: fixed; inset: 0; background: rgba(0,0,0,0.92); z-index: 1000; align-items: center; justify-content: center; }}
        .lightbox.open {{ display: flex; }}
        .lightbox img {{ max-width: 90vw; max-height: 85vh; object-fit: contain; border-radius: 10px; }}
        .lb-close {{ position: absolute; top: 1rem; right: 1rem; background: none; border: none; color: rgba(255,255,255,0.6); font-size: 1.5rem; cursor: pointer; padding: 0.5rem; }}
    </style>
</head>
<body>
<div class="page">
    <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:1.5rem">
        <a href="/content" class="back" style="margin-bottom:0">← Flight History</a>
        {delete_button}
    </div>
    <div id="header-mount"></div>
    <div id="gallery-mount"></div>
    <div id="map"></div>
    <div class="charts" id="charts-mount"></div>
</div>
<div id="lightbox" class="lightbox" onclick="if(event.target===this)closeLb()">
    <button class="lb-close" onclick="closeLb()">✕</button>
    <img id="lb-img" src="" alt="">
</div>
<script>
const SHARE_DATA = {json_escaped};
function reconstructTs(deltas) {{
    if (!deltas || !deltas.length) return [];
    const ts = [deltas[0]];
    for (let i = 1; i < deltas.length; i++) ts.push(ts[i-1] + deltas[i]);
    return ts;
}}
function fmtDur(m) {{ const h = Math.floor(m/60); return h > 0 ? h+'h '+(m%60)+'m' : m+'m'; }}
function fmtDate(s) {{
    if (!s) return '';
    try {{ return new Date(s.replace(' ','T')+'Z').toLocaleString(undefined,{{dateStyle:'medium',timeStyle:'short',timeZone:'UTC'}}); }} catch {{ return s; }}
}}
(function() {{
    const d = SHARE_DATA;
    const td = d.transposedData || d.transposed_data || {{}};
    const sum = d.summary || {{}};
    const scrns = d.screenshots || [];
    const timestamps = reconstructTs(td.timestamps);
    const lats = td.latitudes || [], lons = td.longitudes || [], alts = td.altitudes || [];
    const ias = td.ias || [], vs = td.vspeed || [], pitch = td.pitch || [];
    const dep = sum.startIcao || sum.departure || '?';
    const arr = sum.endIcao || sum.arrival || '?';
    const ac = sum.aircraftTitle || sum.airframe_name || '';
    const dur = sum.durationMinutes || sum.duration_minutes || 0;
    const maxAlt = (sum.maxAltitude || sum.max_altitude || 0).toFixed(0);
    const maxGs = (sum.maxGroundSpeed || sum.max_ground_speed || 0).toFixed(0);
    const fuel = (sum.fuelConsumed || sum.fuel_consumed || 0).toFixed(1);
    const events = sum.events || [];
    const lEvt = events.filter(e => (e.eventType||e.event_type) === 'landing').pop();
    const lvs = lEvt && (lEvt.touchdownFpm || lEvt.touchdown_fpm);
    let badgeHtml = '';
    if (lvs != null) {{
        const a = Math.abs(lvs);
        const [cls, lbl] = a < 150 ? ['butter','BUTTER'] : a < 250 ? ['smooth','SMOOTH'] : a < 350 ? ['firm','FIRM'] : ['hard','HARD'];
        badgeHtml = `<div class="badge badge-${{cls}}">${{lbl}} — ${{Math.round(a)}} fpm</div>`;
    }}
    document.getElementById('header-mount').innerHTML = `
        <div style="margin-bottom:1.5rem">
            <div class="route"><span class="icao">${{dep}}</span><span class="arrow">→</span><span class="icao">${{arr}}</span></div>
            <div class="aircraft">${{ac}}</div>
            <div class="meta">${{fmtDate(sum.startTime||sum.start_time)}} · ${{fmtDur(dur)}}</div>
            ${{badgeHtml}}
        </div>
        <div class="stats-grid">
            <div class="stat-card"><div class="stat-label">Max Altitude</div><div class="stat-value">${{maxAlt}} ft</div></div>
            <div class="stat-card"><div class="stat-label">Max Speed (GS)</div><div class="stat-value">${{maxGs}} kt</div></div>
            <div class="stat-card"><div class="stat-label">Fuel Consumed</div><div class="stat-value">${{fuel}} gal</div></div>
            <div class="stat-card"><div class="stat-label">Duration</div><div class="stat-value">${{fmtDur(dur)}}</div></div>
        </div>
        ${{lEvt ? `<div class="landing-card"><div class="section-title">Landing Performance</div><div class="landing-grid">
            ${{lvs != null ? `<div><div class="li-label">Touchdown VS</div><div class="li-val">${{Math.round(lvs)}} fpm</div></div>` : ''}}
            ${{(lEvt.landingG||lEvt.landing_g) != null ? `<div><div class="li-label">Landing G</div><div class="li-val">${{(lEvt.landingG||lEvt.landing_g).toFixed(2)}} G</div></div>` : ''}}
            ${{(lEvt.offsetPercent||lEvt.offset_percent) != null ? `<div><div class="li-label">Offset</div><div class="li-val">${{(lEvt.offsetPercent||lEvt.offset_percent).toFixed(1)}}%</div></div>` : ''}}
            ${{(lEvt.thresholdDistFt||lEvt.threshold_dist_ft) != null ? `<div><div class="li-label">Threshold</div><div class="li-val">${{Math.round(lEvt.thresholdDistFt||lEvt.threshold_dist_ft)}} ft</div></div>` : ''}}
        </div></div>` : ''}}
    `;
    if (lats.length > 0) {{
        const map = L.map('map');
        L.tileLayer('https://{{s}}.basemaps.cartocdn.com/dark_all/{{z}}/{{x}}/{{y}}{{r}}.png',{{attribution:'© OSM © CARTO',subdomains:'abcd',maxZoom:19}}).addTo(map);
        const coords = lats.map((la, i) => [la, lons[i]]);
        const path = L.polyline(coords,{{color:'#cba6f7',weight:2.5,opacity:0.9}}).addTo(map);
        map.fitBounds(path.getBounds().pad(0.12));
        events.forEach(e => {{
            const type = e.eventType||e.event_type;
            if (!e.latitude || !e.longitude) return;
            const color = type==='takeoff'?'#a6e3a1':type==='landing'?'#f38ba8':'#89b4fa';
            const label = type==='takeoff'?'Takeoff':type==='landing'?'Landing':type==='top_of_climb'?'TOC':'TOD';
            L.circleMarker([e.latitude,e.longitude],{{radius:7,color,fillColor:color,fillOpacity:0.9,weight:2}}).bindTooltip(label).addTo(map);
        }});
    }} else {{ document.getElementById('map').style.display = 'none'; }}
    if (timestamps.length > 0) {{
        const step = Math.max(1, Math.floor(timestamps.length / 300));
        const labels=[], ad=[], id=[], vd=[], pd=[];
        for (let i=0; i<timestamps.length; i+=step) {{
            labels.push(new Date(timestamps[i]*1000).toISOString().slice(11,16));
            ad.push(alts[i]||0); id.push(ias[i]||0); vd.push(vs[i]||0); pd.push(pitch[i]||0);
        }}
        const opts = () => ({{
            responsive:true, maintainAspectRatio:true,
            plugins:{{legend:{{display:false}},tooltip:{{mode:'index',intersect:false}}}},
            scales:{{
                x:{{ticks:{{color:'#6c7086',maxTicksLimit:6}},grid:{{color:'rgba(255,255,255,0.04)'}}}},
                y:{{ticks:{{color:'#6c7086'}},grid:{{color:'rgba(255,255,255,0.04)'}}}},
            }}
        }});
        const mk = (id2, lbl, data, color) => {{
            const el = document.createElement('div'); el.className='chart-card';
            el.innerHTML=`<div class="section-title">${{lbl}}</div><canvas id="${{id2}}"></canvas>`;
            document.getElementById('charts-mount').appendChild(el);
            new Chart(document.getElementById(id2),{{type:'line',data:{{labels,datasets:[{{label:lbl,data,borderColor:color,backgroundColor:color+'22',borderWidth:1.5,pointRadius:0,fill:true,tension:0.2}}]}},options:opts()}});
        }};
        mk('ca','Altitude (ft)',ad,'#89b4fa');
        mk('ci','Airspeed (kt)',id,'#a6e3a1');
        mk('cv','Vert Speed (fpm)',vd,'#f38ba8');
        mk('cp','Pitch (°)',pd,'#f9e2af');
    }}
    if (scrns.length > 0) {{
        const urls = scrns.map(s=>s.url);
        window._lbUrls = urls;
        const thumbs = urls.map((u,i)=>`<img src="${{u}}" loading="lazy" onclick="openLb(${{i}})" alt="">`).join('');
        document.getElementById('gallery-mount').innerHTML=`<div class="gallery-section"><div class="section-title">Screenshots</div><div class="gallery">${{thumbs}}</div></div>`;
    }}
}})();
function openLb(i){{ document.getElementById('lb-img').src=window._lbUrls[i]; document.getElementById('lightbox').classList.add('open'); document.body.style.overflow='hidden'; }}
function closeLb(){{ document.getElementById('lightbox').classList.remove('open'); document.body.style.overflow=''; }}
document.addEventListener('keydown',e=>{{if(e.key==='Escape')closeLb();}});
</script>
</body>
</html>"##, json_escaped = json_escaped, delete_button = delete_button);

    let mut response = Html(html).into_response();
    response.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    Ok(response)
}
