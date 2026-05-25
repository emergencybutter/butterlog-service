use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post, put, delete},
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
        .route("/settings", get(settings_handler))
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
            "/discord-notification-channels",
            get(handlers::get_discord_channels_handler).post(handlers::add_discord_channel_handler),
        )
        .route(
            "/discord-notification-channels/:channel_id",
            delete(handlers::delete_discord_channel_handler),
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

    let mut response = Redirect::temporary("/settings").into_response();
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

    let channels = match discord::get_bot_channels(&state.discord_http, state.config.predetermined_channels.as_deref()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to fetch bot channels: {}", e);
            vec![("1462209019740426452".to_string(), "Default Voyager Channel".to_string())]
        }
    };

    let enabled_channels: Vec<String> = sqlx::query_scalar(
        "SELECT channel_id FROM discord_notification_channels WHERE user_id = $1"
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut predetermined_html = String::new();
    for (id, name) in &channels {
        let is_checked = enabled_channels.contains(id);
        let checked_attr = if is_checked { "checked" } else { "" };
        predetermined_html.push_str(&format!(
            r#"
            <div class="channel-row">
                <div class="channel-info">
                    <span class="channel-name">{}</span>
                    <span class="channel-id">ID: {}</span>
                </div>
                <label class="switch">
                    <input type="checkbox" id="switch-{}" onclick="toggleChannel('{}', this.checked)" {}>
                    <span class="slider"></span>
                </label>
            </div>
            "#,
            name, id, id, id, checked_attr
        ));
    }

    let mut custom_html = String::new();
    for id in &enabled_channels {
        if !channels.iter().any(|(p_id, _)| p_id == id) {
            custom_html.push_str(&format!(
                r#"
                <div class="channel-row custom-row">
                    <div class="channel-info">
                        <span class="channel-name">Custom Channel</span>
                        <span class="channel-id">ID: {}</span>
                    </div>
                    <button class="btn-delete" onclick="deleteChannel('{}')">
                        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"></polyline><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path><line x1="10" y1="11" x2="10" y2="17"></line><line x1="14" y1="11" x2="14" y2="17"></line></svg>
                        Remove
                    </button>
                </div>
                "#,
                id, id
            ));
        }
    }
    
    let custom_section = if custom_html.is_empty() {
        "".to_string()
    } else {
        format!(
            r#"
            <div class="section-title">Registered Custom Channels</div>
            <div class="channel-list">
                {}
            </div>
            "#,
            custom_html
        )
    };

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
                @import url('https://fonts.googleapis.com/css2?family=Outfit:wght@300;400;600;700&display=swap');
                body {{
                    font-family: 'Outfit', sans-serif;
                    background: radial-gradient(circle at top left, #1e1e38, #11111b);
                    color: #cdd6f4;
                    min-height: 100vh;
                    margin: 0;
                    display: flex;
                    flex-direction: column;
                    align-items: center;
                    justify-content: flex-start;
                    padding: 2rem 1rem;
                    box-sizing: border-box;
                }}
                .settings-container {{
                    max-width: 600px;
                    width: 100%;
                    background: rgba(30, 30, 46, 0.45);
                    backdrop-filter: blur(16px);
                    border: 1px solid rgba(255, 255, 255, 0.08);
                    border-radius: 24px;
                    padding: 2.5rem;
                    box-shadow: 0 20px 40px rgba(0, 0, 0, 0.4);
                    margin-top: 1.5rem;
                    box-sizing: border-box;
                }}
                .header {{
                    text-align: center;
                    margin-bottom: 2rem;
                }}
                h1 {{
                    font-size: 2.2rem;
                    font-weight: 700;
                    margin: 0 0 0.5rem 0;
                    background: linear-gradient(90deg, #cba6f7, #b4befe);
                    -webkit-background-clip: text;
                    -webkit-text-fill-color: transparent;
                }}
                .subtitle {{
                    color: #a6adc8;
                    font-size: 1rem;
                    margin: 0;
                }}
                .section-title {{
                    font-size: 0.95rem;
                    font-weight: 600;
                    color: #f5c2e7;
                    margin: 2rem 0 1rem 0;
                    text-transform: uppercase;
                    letter-spacing: 1px;
                }}
                .channel-list {{
                    background: rgba(17, 17, 27, 0.35);
                    border: 1px solid rgba(255, 255, 255, 0.04);
                    border-radius: 16px;
                    padding: 0.5rem;
                    margin-bottom: 1.5rem;
                }}
                .channel-row {{
                    display: flex;
                    justify-content: space-between;
                    align-items: center;
                    padding: 1rem;
                    border-bottom: 1px solid rgba(255, 255, 255, 0.04);
                    transition: background-color 0.2s;
                }}
                .channel-row:last-child {{
                    border-bottom: none;
                }}
                .channel-row:hover {{
                    background-color: rgba(255, 255, 255, 0.02);
                    border-radius: 12px;
                }}
                .channel-info {{
                    display: flex;
                    flex-direction: column;
                }}
                .channel-name {{
                    font-weight: 600;
                    color: #cdd6f4;
                    font-size: 1rem;
                }}
                .channel-id {{
                    font-size: 0.8rem;
                    color: #6c7086;
                    margin-top: 0.2rem;
                }}
                /* Toggle switch */
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
                    background-color: #a6adc8;
                    transition: 0.3s cubic-bezier(0.4, 0, 0.2, 1);
                    border-radius: 50%;
                }}
                input:checked + .slider {{
                    background-color: rgba(166, 227, 161, 0.2);
                    border-color: rgba(166, 227, 161, 0.4);
                }}
                input:checked + .slider:before {{
                    transform: translateX(24px);
                    background-color: #a6e3a1;
                }}
                /* Buttons and input */
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
                    color: #cdd6f4;
                    font-family: inherit;
                    font-size: 0.95rem;
                    outline: none;
                    transition: border-color 0.2s, box-shadow 0.2s;
                }}
                input[type="text"]:focus {{
                    border-color: #cba6f7;
                    box-shadow: 0 0 0 3px rgba(203, 166, 247, 0.15);
                }}
                button {{
                    background: #cba6f7;
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
                    background: #b4befe;
                    transform: translateY(-1px);
                }}
                .btn-delete {{
                    background: rgba(243, 139, 168, 0.1);
                    color: #f38ba8;
                    border: 1px solid rgba(243, 139, 168, 0.2);
                    padding: 0.5rem 1rem;
                    font-size: 0.85rem;
                    display: flex;
                    align-items: center;
                    gap: 0.5rem;
                    border-radius: 8px;
                    cursor: pointer;
                    font-family: inherit;
                    transition: background-color 0.2s;
                }}
                .btn-delete:hover {{
                    background: rgba(243, 139, 168, 0.2);
                }}
                /* Toast Notification */
                .toast-container {{
                    position: fixed;
                    bottom: 2rem;
                    right: 2rem;
                    z-index: 1000;
                }}
                .toast {{
                    background: rgba(30, 30, 46, 0.95);
                    border: 1px solid rgba(255, 255, 255, 0.1);
                    padding: 1rem 1.5rem;
                    border-radius: 12px;
                    box-shadow: 0 10px 30px rgba(0,0,0,0.5);
                    display: flex;
                    align-items: center;
                    gap: 0.75rem;
                    margin-top: 0.5rem;
                    animation: slideIn 0.3s ease, fadeOut 0.3s ease 2.7s forwards;
                    color: #cdd6f4;
                }}
                .toast.success {{
                    border-left: 4px solid #a6e3a1;
                }}
                .toast.error {{
                    border-left: 4px solid #f38ba8;
                }}
                @keyframes slideIn {{
                    from {{ transform: translateX(100%); opacity: 0; }}
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

                <section>
                    <div class="section-title">Available Channels</div>
                    <div class="channel-list">
                        {}
                    </div>
                </section>

                {}

                <section>
                    <div class="section-title">Register Custom Channel</div>
                    <div class="input-group">
                        <input type="text" id="custom-channel-id" placeholder="Enter custom Discord Channel ID..." />
                        <button onclick="addCustomChannel()">Add Channel</button>
                    </div>
                </section>
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

                async function toggleChannel(channelId, checked) {{
                    try {{
                        let response;
                        if (checked) {{
                            response = await fetch('/discord-notification-channels', {{
                                method: 'POST',
                                headers: {{
                                    'Content-Type': 'application/json'
                                }},
                                body: JSON.stringify({{ channelId: channelId }})
                            }});
                        }} else {{
                            response = await fetch(`/discord-notification-channels/${{channelId}}`, {{
                                method: 'DELETE'
                            }});
                        }}

                        if (response.ok) {{
                            showToast(checked ? 'Channel enabled successfully!' : 'Channel disabled successfully!', 'success');
                            setTimeout(() => window.location.reload(), 1000);
                        }} else {{
                            const data = await response.json().catch(() => ({{}}));
                            const errMsg = data.error || 'Request failed';
                            showToast(errMsg, 'error');
                            document.getElementById(`switch-${{channelId}}`).checked = !checked;
                        }}
                    }} catch (err) {{
                        showToast('Network error occurred', 'error');
                        document.getElementById(`switch-${{channelId}}`).checked = !checked;
                    }}
                }}

                async function deleteChannel(channelId) {{
                    try {{
                        const response = await fetch(`/discord-notification-channels/${{channelId}}`, {{
                            method: 'DELETE'
                        }});
                        if (response.ok) {{
                            showToast('Custom channel removed successfully!', 'success');
                            setTimeout(() => window.location.reload(), 1000);
                        }} else {{
                            const data = await response.json().catch(() => ({{}}));
                            const errMsg = data.error || 'Failed to remove channel';
                            showToast(errMsg, 'error');
                        }}
                    }} catch (err) {{
                        showToast('Network error occurred', 'error');
                    }}
                }}

                async function addCustomChannel() {{
                    const input = document.getElementById('custom-channel-id');
                    const channelId = input.value.trim();
                    if (!channelId) {{
                        showToast('Please enter a valid Channel ID', 'error');
                        return;
                    }}

                    try {{
                        const response = await fetch('/discord-notification-channels', {{
                            method: 'POST',
                            headers: {{
                                'Content-Type': 'application/json'
                            }},
                            body: JSON.stringify({{ channelId: channelId }})
                        }});

                        if (response.ok) {{
                            showToast('Custom channel successfully added!', 'success');
                            input.value = '';
                            setTimeout(() => window.location.reload(), 1000);
                        }} else {{
                            const data = await response.json().catch(() => ({{}}));
                            const errMsg = data.error || 'Failed to add custom channel';
                            showToast(errMsg, 'error');
                        }}
                    }} catch (err) {{
                        showToast('Network error occurred', 'error');
                    }}
                }}
            </script>
        </body>
        </html>
        "#,
        predetermined_html,
        custom_section
    ))
    .into_response()
}
