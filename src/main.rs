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
mod telemetry;
mod commands;
mod live;
mod templates;
mod track;

use crate::config::Config;
use crate::error::AppError;
use askama::Template;

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

    // Ends flights that stopped reporting and prunes expired track points.
    track::spawn_reaper(state.db.clone());

    // Build the router with trace logging
    let app = Router::new()
        .route("/", get(home_handler))
        .route("/content", get(content_handler))
        .route("/content/flight/user/:user_id", get(content_user_handler))
        .route("/content/settings", get(settings_handler))
        .route("/content/stats", get(stats_handler))
        .route("/map", get(map_handler))
        .route("/api/v0/map/data", get(map_data_handler))
        .route("/api/v0/stats/aircraft", get(aircraft_stats_handler))
        .route("/api/v0/user/:user_id/current", get(user_current_flight_handler))
        .route("/api/v0/user/by-discord/:discord_id/current", get(user_current_flight_by_discord_handler))
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
        // Header-authenticated API (Authorization: Bearer <token>). The
        // /users/:webhook_token/... routes below are the legacy path-token
        // form, kept for old clients.
        .route("/api/v0/flights", post(handlers::create_flight_bearer_handler))
        .route(
            "/api/v0/flights/:id",
            put(handlers::update_flight_bearer_handler).get(handlers::get_flight_bearer_handler),
        )
        .route(
            "/api/v0/flights/:id/notes",
            put(handlers::update_flight_notes_bearer_handler),
        )
        .route(
            "/api/v0/flights/:id/screenshots",
            post(handlers::upload_screenshot_bearer_handler)
                .layer(axum::extract::DefaultBodyLimit::max(handlers::MAX_SCREENSHOT_UPLOAD)),
        )
        .route(
            "/api/v0/flights/:id/screenshots/:hash",
            delete(handlers::delete_screenshot_bearer_handler),
        )
        .route(
            "/api/v0/flights/share",
            post(handlers::upload_flight_share_bearer_handler),
        )
        .route(
            "/api/v0/multiplayer/ping",
            post(handlers::multiplayer_ping_bearer_handler),
        )
        // Live flight track: a pure append that doubles as the liveness
        // heartbeat, kept off PUT /flights/:id so it never triggers a Discord sync.
        .route(
            "/api/v0/flights/:id/track",
            post(track::upload_track_handler)
                .layer(axum::extract::DefaultBodyLimit::max(track::MAX_TRACK_DECOMPRESSED as usize)),
        )
        .route("/api/v0/flights/:id/track/cursor", get(track::track_cursor_handler))
        .route("/api/v0/flights/:id/end", post(track::end_flight_handler))
        // Public, like the map and the flight history pages it mirrors.
        .route("/api/v0/flights/:id/live", get(live::live_flight_handler))
        // Owner-only in both directions — the one non-public part of the
        // live-flight feature.
        .route(
            "/api/v0/flights/:id/commands",
            get(commands::poll_commands_handler).post(commands::issue_command_handler),
        )
        .route(
            "/api/v0/flights/:id/commands/:cid",
            get(commands::command_status_handler),
        )
        .route(
            "/api/v0/flights/:id/commands/:cid/ack",
            post(commands::ack_command_handler),
        )
        .route("/api/v0/users/:webhook_token/flights", post(handlers::create_flight_handler))
        .route(
            "/api/v0/users/:webhook_token/flights/:id",
            put(handlers::update_flight_handler).get(handlers::get_flight_handler),
        )
        .route(
            "/api/v0/users/:webhook_token/flights/:id/notes",
            put(handlers::update_flight_notes_handler),
        )
        .route(
            "/api/v0/users/:webhook_token/flights/:id/screenshots",
            post(handlers::upload_screenshot_handler)
                // Screenshot uploads may exceed axum's 2MB default body limit;
                // this is the real (intentional) upload cap.
                .layer(axum::extract::DefaultBodyLimit::max(handlers::MAX_SCREENSHOT_UPLOAD)),
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
        .route("/content/flights/:id", get(flight_detail_handler))
        .route(
            "/api/v0/users/:webhook_token/multiplayer/ping",
            post(handlers::multiplayer_ping_handler),
        )
        .layer(axum::middleware::from_fn(log_requests))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    tracing::info!("ButterLog service starting on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    // Cloud Run delivers SIGTERM before stopping an instance; finish in-flight
    // requests instead of dropping them.
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("Shutdown signal received, draining connections...");
}

/// Redacts the secret webhook-token segment from `/api/v0/users/<token>/...` paths so it
/// never lands in logs. Other path segments (share ids, channel ids) are not secrets.
fn redact_path(path: &str) -> String {
    const PREFIX: &str = "/api/v0/users/";
    if let Some(rest) = path.strip_prefix(PREFIX) {
        let tail = rest.find('/').map(|i| &rest[i..]).unwrap_or("");
        format!("{}***{}", PREFIX, tail)
    } else {
        path.to_string()
    }
}

async fn log_requests(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = redact_path(req.uri().path());

    tracing::info!("[Incoming Request] {} {}", method, path);

    let start = std::time::Instant::now();
    let response = next.run(req).await;
    let latency = start.elapsed();

    tracing::info!(
        "[Incoming Response] {} {} -> Status: {} (took {:?})",
        method,
        path,
        response.status(),
        latency
    );

    response
}

async fn home_handler() -> Result<Response, AppError> {
    Ok(Html(templates::HomePage.render()?).into_response())
}

/// OAuth `state` is "{nonce}" or "{nonce}.{port}" — the nonce ties the callback
/// to the browser that started the flow (CSRF protection); the optional port is
/// the desktop app's loopback listener.
async fn login_handler(
    State(state): State<AppState>,
    Query(query): Query<LoginQuery>,
) -> impl IntoResponse {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let state_param = match query.port {
        Some(p) => format!("{}.{}", nonce, p),
        None => nonce.clone(),
    };
    let auth_url = auth::get_login_url(
        &state.config.discord_client_id,
        &state.config.discord_redirect_uri,
        Some(&state_param),
    );
    let mut response = Redirect::temporary(&auth_url).into_response();
    let cookie_val = format!(
        "oauth_state={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=600",
        nonce
    );
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&cookie_val).unwrap(),
    );
    response
}

/// Reads a cookie value from the request headers.
fn get_cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let cookie_header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for cookie in cookie_header.split(';') {
        let mut parts = cookie.trim().splitn(2, '=');
        if parts.next() == Some(name) {
            return parts.next().map(|v| v.to_string());
        }
    }
    None
}

fn session_cookie(api_token: &str) -> axum::http::HeaderValue {
    let cookie_val = format!(
        "token={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=31536000",
        api_token
    );
    axum::http::HeaderValue::from_str(&cookie_val).unwrap()
}

async fn callback_handler(
    State(state): State<AppState>,
    Query(params): Query<CallbackQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    if let Some(err) = params.error {
        return Err(AppError::Auth(format!("Discord OAuth error: {}", err)));
    }

    let code = params.code.ok_or_else(|| {
        AppError::Auth("Missing code parameter in OAuth callback".to_string())
    })?;

    // Verify the OAuth state nonce against the cookie set at login (CSRF check),
    // and extract the optional loopback port suffix.
    let state_val = params.state.as_deref().unwrap_or("");
    let (nonce, port) = match state_val.split_once('.') {
        Some((n, p)) => (n, p.parse::<u16>().ok()),
        None => (state_val, None),
    };
    let cookie_nonce = get_cookie(&headers, "oauth_state");
    if nonce.is_empty() || cookie_nonce.as_deref() != Some(nonce) {
        return Err(AppError::Auth(
            "OAuth state mismatch. Please restart the login flow.".to_string(),
        ));
    }

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

    // Redirect back to the local app's loopback listener when a port was given
    let mut response = match port {
        Some(p) => {
            let redirect_url = format!("http://127.0.0.1:{}?token={}", p, api_token);
            Redirect::temporary(&redirect_url).into_response()
        }
        None => Redirect::temporary("/content").into_response(),
    };
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        session_cookie(&api_token),
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

    // Guilds where the user is an admin, with allowlist toggles per channel
    let admin_guilds: Vec<templates::AdminGuild> = guilds_info
        .iter()
        .filter(|g| g.is_user_admin)
        .map(|guild| templates::AdminGuild {
            name: guild.name.clone(),
            channels: guild
                .channels
                .iter()
                .map(|(chan_id, chan_name)| {
                    // Escaped for the single-quoted JS string in the onclick
                    // attribute: HTML-escape first, then backslash-escape.
                    let js_name = esc(chan_name).replace('\\', "\\\\").replace('\'', "\\'");
                    templates::AdminChannel {
                        id: chan_id.clone(),
                        name: chan_name.clone(),
                        js_name,
                        guild_id: guild.id.clone(),
                        checked: allowlisted_ids.contains(chan_id),
                    }
                })
                .collect(),
        })
        .collect();

    // Group the user's enabled notification channels by guild
    let mut enabled_by_guild: std::collections::HashMap<String, Vec<templates::NotifiedChannel>> =
        std::collections::HashMap::new();
    for (chan_id, chan_name, guild_id) in &allowlisted_channels {
        if enabled_channels.contains(chan_id) {
            enabled_by_guild
                .entry(guild_id.clone())
                .or_default()
                .push(templates::NotifiedChannel {
                    id: chan_id.clone(),
                    name: chan_name.clone(),
                });
        }
    }
    let notified_guilds: Vec<templates::NotifiedGuild> = enabled_by_guild
        .into_iter()
        .map(|(guild_id, channels)| templates::NotifiedGuild {
            name: guild_names
                .get(&guild_id)
                .cloned()
                .unwrap_or_else(|| format!("Server ({})", guild_id)),
            channels,
        })
        .collect();

    let page = templates::SettingsPage { admin_guilds, notified_guilds };
    match page.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("Failed to render settings page: {:?}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Page rendering failed",
            )
                .into_response()
        }
    }
}

async fn flight_detail_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(flight_id): axum::extract::Path<i64>,
) -> Result<Response, AppError> {
    // A flight still in progress gets the live page instead: same detail, but
    // fetched from /api/v0/flights/:id/live and kept current. Requires a track
    // to draw — a flight from a client too old to stream one falls through to
    // the static page, which is exactly what it rendered before.
    let live: Option<(String, i32, i64)> =
        sqlx::query_as("SELECT status, track_points, user_id FROM flights WHERE id = $1")
            .bind(flight_id)
            .fetch_optional(&state.db)
            .await?;
    if let Some((status, track_points, owner_id)) = live {
        if status != "ended" && track_points > 0 {
            let viewer = handlers::get_user_id_from_session(&state.db, &headers)
                .await
                .ok();
            let page = templates::LiveDetailPage {
                flight_id,
                is_owner: viewer == Some(owner_id),
            };
            return Ok(Html(page.render()?).into_response());
        }
    }

    let row: Option<(String, Option<String>, serde_json::Value, chrono::DateTime<chrono::Utc>, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT f.departure, f.arrival, f.statistics, f.created_at, u.username, u.global_name, f.notes \
         FROM flights f JOIN users u ON f.user_id = u.id WHERE f.id = $1"
    )
    .bind(flight_id)
    .fetch_optional(&state.db)
    .await?;

    let (dep, arr, stats, created_at, username, global_name, notes) = match row {
        Some(r) => r,
        None => return Ok((axum::http::StatusCode::NOT_FOUND, Html("<h1>Flight not found</h1>".to_string())).into_response()),
    };

    let screenshots: Vec<String> = sqlx::query_scalar(
        "SELECT url FROM screenshots WHERE flight_id = $1 ORDER BY created_at"
    )
    .bind(flight_id)
    .fetch_all(&state.db)
    .await?;

    // Touchdown + peak telemetry from the same JSON the Discord embed reads.
    // Touchdown shows the whole landing category; Peak is curated to the headline
    // figures (the full max set reads as a wall of numbers on the web layout).
    let to_items = |pairs: Vec<(&'static str, String)>| -> Vec<templates::StatItem> {
        pairs
            .into_iter()
            .map(|(label, value)| templates::StatItem { label: label.to_string(), value })
            .collect()
    };
    const PEAK_KEYS: &[&str] = &["AltB", "IAS", "GndSpd", "VSpd", "NormAc"];
    let touchdown_stats = to_items(
        stats
            .get("landing_snapshot")
            .map(|s| telemetry::labeled_values(s, "landing"))
            .unwrap_or_default(),
    );
    let peak_stats = to_items(
        stats
            .get("max_entries")
            .map(|s| telemetry::labeled_values_for_keys(s, PEAK_KEYS))
            .unwrap_or_default(),
    );

    // Link to the public 3D share page when this flight has been shared. The
    // `remote_flight_id` column stores the local flights.id (see discord sync).
    let share_id: Option<String> = sqlx::query_scalar(
        "SELECT id FROM flight_shares WHERE remote_flight_id = $1 ORDER BY created_at DESC LIMIT 1"
    )
    .bind(flight_id)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);
    let share_href = share_id
        .map(|id| format!("/content/flights/share/{}", id))
        .unwrap_or_default();

    let page = templates::FlightDetailPage {
        dep,
        arr_display: arr.unwrap_or_else(|| "In Flight".to_string()),
        pilot: global_name.unwrap_or(username),
        airframe: stats
            .get("airframe_name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Aircraft")
            .to_string(),
        resolved_icao: stats.get("resolved_icao").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        resolved_airline: stats.get("resolved_airline").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        simulator: stats.get("simulator").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        date_str: created_at.format("%B %d, %Y, %H:%M UTC").to_string(),
        landing_badge: landing_badge_html(&stats).unwrap_or_default(),
        notes: notes.as_deref().map(str::trim).unwrap_or("").to_string(),
        urls_json: serde_json::to_string(&screenshots).unwrap_or_default(),
        screenshots,
        touchdown_stats,
        peak_stats,
        share_href,
    };

    Ok(Html(page.render()?).into_response())
}

/// (id, departure, arrival, statistics, created_at, user_id, username, global_name, avatar, discord_id)
type FlightListRow = (
    i64,
    String,
    Option<String>,
    serde_json::Value,
    chrono::DateTime<chrono::Utc>,
    i64,
    String,
    Option<String>,
    Option<String>,
    String,
    // status
    String,
    // track_points
    i32,
);

/// Minimal HTML escaping for user-controlled text rendered into attributes/markup.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Classify a touchdown vertical speed (fpm, negative = descending) into a
/// badge CSS class and label.
fn landing_rating(vspd: f64) -> (&'static str, &'static str) {
    if vspd >= -150.0 {
        ("butter", "BUTTER")
    } else if vspd >= -250.0 {
        ("smooth", "SMOOTH")
    } else if vspd >= -350.0 {
        ("firm", "FIRM")
    } else {
        ("hard", "HARD")
    }
}

/// Landing badge HTML from a flight's statistics JSON; None when the flight
/// has no landing snapshot (still airborne).
fn landing_badge_html(stats: &serde_json::Value) -> Option<String> {
    let landing = stats.get("landing_snapshot")?;
    let vspd = landing.get("VSpd").and_then(|v| v.as_f64())?;
    let gforce_str = landing.get("NormAc").and_then(|v| v.as_f64())
        .map(|g| format!(" / {:.2}G", g)).unwrap_or_default();
    let (class, label) = landing_rating(vspd);
    Some(format!(
        r#"<div class="badge badge-{}">{}<br><span class="badge-detail">{:.0} fpm{}</span></div>"#,
        class, label, vspd.abs(), gforce_str
    ))
}

async fn content_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let logged_in_user_id = handlers::get_user_id_from_session(&state.db, &headers).await.ok();

    // Latest flights across every pilot
    let flights: Vec<FlightListRow> = sqlx::query_as(
        "SELECT f.id, f.departure, f.arrival, f.statistics, f.created_at, \
                u.id, u.username, u.global_name, u.avatar, u.discord_id, f.status, f.track_points \
         FROM flights f JOIN users u ON f.user_id = u.id \
         ORDER BY f.created_at DESC LIMIT 50"
    )
    .fetch_all(&state.db)
    .await?;

    render_flights_page(&state, flights, logged_in_user_id, None, "Telemetry records from every pilot").await
}

async fn content_user_handler(
    State(state): State<AppState>,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    let logged_in_user_id = handlers::get_user_id_from_session(&state.db, &headers).await.ok();

    // Latest flights for a single pilot
    let flights: Vec<FlightListRow> = sqlx::query_as(
        "SELECT f.id, f.departure, f.arrival, f.statistics, f.created_at, \
                u.id, u.username, u.global_name, u.avatar, u.discord_id, f.status, f.track_points \
         FROM flights f JOIN users u ON f.user_id = u.id \
         WHERE f.user_id = $1 ORDER BY f.created_at DESC LIMIT 50"
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;

    render_flights_page(&state, flights, logged_in_user_id, Some(user_id), "Telemetry records and landing reports").await
}

async fn render_flights_page(
    state: &AppState,
    flights: Vec<FlightListRow>,
    logged_in_user_id: Option<i64>,
    filter_user_id: Option<i64>,
    subtitle: &str,
) -> Result<Response, AppError> {
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

    let cards: Vec<templates::FlightCard> = flights
        .into_iter()
        .map(|flight| {
            let flight_id = flight.0;
            let stats = flight.3;
            let avatar_url = match flight.8.as_deref() {
                Some(hash) if !hash.is_empty() => {
                    format!("https://cdn.discordapp.com/avatars/{}/{}.png", flight.9, hash)
                }
                _ => "https://cdn.discordapp.com/embed/avatars/0.png".to_string(),
            };
            let screenshots = screenshots_by_flight.remove(&flight_id).unwrap_or_default();
            // A flight that is still running and has a track to draw gets the
            // live page; the badge says LIVE rather than ONGOING so the card
            // reads as something worth clicking.
            let is_live = flight.10 != "ended" && flight.11 > 0;
            let landing_badge = if stats.get("landing_snapshot").is_some() {
                landing_badge_html(&stats).unwrap_or_default()
            } else if is_live {
                r#"<div class="badge badge-live">LIVE</div>"#.to_string()
            } else {
                r#"<div class="badge badge-ongoing">ONGOING</div>"#.to_string()
            };
            templates::FlightCard {
                share_href: share_by_flight
                    .get(&flight_id)
                    .map(|sid| format!("/content/flights/share/{}", sid))
                    .unwrap_or_default(),
                live_href: if is_live {
                    format!("/content/flights/{}", flight_id)
                } else {
                    String::new()
                },
                avatar_url,
                pilot: flight.7.unwrap_or(flight.6),
                dep: flight.1,
                arr: flight.2.unwrap_or_else(|| "In Flight".to_string()),
                airframe: stats
                    .get("airframe_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Aircraft")
                    .to_string(),
                resolved_icao: stats.get("resolved_icao").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                resolved_airline: stats.get("resolved_airline").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                simulator: stats
                    .get("simulator")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Simulator")
                    .to_string(),
                date_str: flight.4.format("%B %d, %Y, %H:%M UTC").to_string(),
                landing_badge,
                urls_json: serde_json::to_string(&screenshots).unwrap_or_default(),
                screenshots,
            }
        })
        .collect();

    let page = templates::FlightsPage {
        subtitle: subtitle.to_string(),
        history_active: filter_user_id.is_none(),
        show_my_flights: logged_in_user_id.is_some(),
        my_flights_href: logged_in_user_id
            .map(|uid| format!("/content/flight/user/{}", uid))
            .unwrap_or_default(),
        my_flights_active: logged_in_user_id.is_some() && filter_user_id == logged_in_user_id,
        flights: cards,
    };

    Ok(Html(page.render()?).into_response())
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
    /// This flight is streaming a track, so the popup can link to the live
    /// detail page. Read from the denormalised counter on `flights` rather than
    /// counting track rows, keeping the map query a single pass.
    live: bool,
}

/// Live position pulled out of a flight's `statistics.current_snapshot`.
/// The sim plugin sends capitalized keys (`Latitude`, `Longitude`, `HDG`,
/// `GndSpd`, `AltMSL`); older clients sent lowercase ones, so both are
/// accepted. `None` means the flight has no usable position yet (no
/// snapshot, or a snapshot missing lat/lon).
#[derive(Serialize, Clone, Copy)]
struct LivePosition {
    latitude: f64,
    longitude: f64,
    /// MSL altitude, feet.
    altitude: f64,
    /// True heading, degrees.
    heading: f64,
    /// Ground speed, knots.
    speed: f64,
}

fn extract_live_position(statistics: &serde_json::Value) -> Option<LivePosition> {
    let snapshot = statistics.get("current_snapshot")?;
    let num = |keys: &[&str]| keys.iter().find_map(|k| snapshot.get(*k).and_then(|v| v.as_f64()));
    Some(LivePosition {
        latitude: num(&["Latitude", "latitude"])?,
        longitude: num(&["Longitude", "longitude"])?,
        altitude: num(&["AltMSL", "gps_altitude_msl", "AltB"]).unwrap_or(0.0),
        heading: num(&["HDG", "heading"]).unwrap_or(0.0),
        speed: num(&["GndSpd", "ground_speed"]).unwrap_or(0.0),
    })
}

fn aircraft_type_of(statistics: &serde_json::Value) -> String {
    statistics
        .get("airframe_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string()
}

/// Intentionally public (no auth): the live map shows every active flight's
/// position to anyone, mirroring the public /content flight history.
async fn map_data_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    let active_flights: Vec<(i64, String, Option<String>, serde_json::Value, chrono::DateTime<chrono::Utc>, String, Option<String>, i32)> = sqlx::query_as(
        "SELECT f.id, f.departure, f.arrival, f.statistics, f.updated_at, u.username, u.global_name, f.track_points \
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
        if let Some(pos) = extract_live_position(&statistics) {
            aircrafts.push(MapAircraft {
                flight_id: flight.0,
                pilot_name: flight.6.unwrap_or(flight.5),
                departure: flight.1,
                arrival: flight.2.unwrap_or_else(|| "In Flight".to_string()),
                aircraft_type: aircraft_type_of(&statistics),
                latitude: pos.latitude,
                longitude: pos.longitude,
                altitude: pos.altitude,
                heading: pos.heading,
                speed: pos.speed,
                updated_ago_secs: now.signed_duration_since(flight.4).num_seconds(),
                live: flight.7 > 0,
            });
        }
    }

    Ok(axum::Json(aircrafts))
}

/// Great-circle distance between two lat/lon points, in nautical miles.
fn haversine_nm(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_NM: f64 = 3440.065;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_NM * a.sqrt().asin()
}

/// One row of the aircraft leaderboard: aggregate totals for a single ICAO
/// type designator (e.g. `A320`). All three headline metrics ride along on
/// every entry so a client can rank by whichever it wants.
#[derive(Serialize, Clone)]
struct AircraftStat {
    icao: String,
    /// Number of logged flights in this type.
    flights: i64,
    /// Total flown time, seconds (sum over flights where it could be derived).
    total_seconds: i64,
    /// Same, expressed as hours (one decimal) for convenience.
    total_hours: f64,
    /// Total great-circle distance takeoff→landing, nautical miles.
    total_distance_nm: f64,
}

/// The aircraft-usage leaderboard: the same aircraft ranked three ways.
#[derive(Serialize)]
struct AircraftStatsResponse {
    by_flights: Vec<AircraftStat>,
    by_time: Vec<AircraftStat>,
    by_distance: Vec<AircraftStat>,
}

/// Per-ICAO accumulator; finalised into `AircraftStat`.
#[derive(Default)]
struct AircraftAgg {
    flights: i64,
    total_seconds: i64,
    total_distance_nm: f64,
}

/// Public aircraft-usage stats, aggregated by resolved ICAO type designator.
/// Reads only fields the service already stores in each flight's `statistics`:
/// `resolved_icao`, the takeoff/landing timestamps (duration), and the
/// takeoff/landing snapshot positions (great-circle distance). Flights missing
/// a piece still count toward the flight tally; they just don't add time or
/// distance. Public like the live map — no per-user data is exposed.
async fn aircraft_stats_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    Ok(axum::Json(aggregate_aircraft_stats(&state.db).await?))
}

/// Shared aggregation behind both the JSON endpoint and the `/content/stats`
/// page. See `aircraft_stats_handler` for the field semantics.
async fn aggregate_aircraft_stats(
    db: &sqlx::PgPool,
) -> Result<AircraftStatsResponse, AppError> {
    // Project just the sub-fields we need out of the JSONB blob so we never
    // haul whole snapshot arrays across the wire. Coordinates come back as text
    // and are parsed in Rust to sidestep casting non-numeric JSON in SQL.
    let rows: Vec<(
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT \
            TRIM(statistics->>'resolved_icao') AS icao, \
            statistics->>'takeoff_time' AS takeoff_time, \
            statistics->>'landing_time' AS landing_time, \
            statistics->>'start_time' AS start_time, \
            statistics->>'end_time' AS end_time, \
            statistics->'takeoff_snapshot'->>'Latitude' AS to_lat, \
            statistics->'takeoff_snapshot'->>'Longitude' AS to_lon, \
            statistics->'landing_snapshot'->>'Latitude' AS ld_lat, \
            statistics->'landing_snapshot'->>'Longitude' AS ld_lon \
         FROM flights \
         WHERE statistics->>'resolved_icao' IS NOT NULL \
           AND TRIM(statistics->>'resolved_icao') <> ''",
    )
    .fetch_all(db)
    .await?;

    let parse_time = |s: &Option<String>| -> Option<chrono::DateTime<chrono::FixedOffset>> {
        s.as_deref()
            .filter(|v| !v.is_empty())
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
    };
    let parse_coord = |s: &Option<String>| -> Option<f64> {
        s.as_deref().and_then(|v| v.parse::<f64>().ok())
    };

    let mut aggs: std::collections::HashMap<String, AircraftAgg> = std::collections::HashMap::new();
    for (icao, takeoff, landing, start, end, to_lat, to_lon, ld_lat, ld_lon) in rows {
        let agg = aggs.entry(icao).or_default();
        agg.flights += 1;

        // Duration: prefer takeoff→landing, fall back to start→end. Only
        // positive spans count (clock skew / bad rows shouldn't subtract).
        let span = parse_time(&landing)
            .zip(parse_time(&takeoff))
            .or_else(|| parse_time(&end).zip(parse_time(&start)))
            .map(|(b, a)| (b - a).num_seconds())
            .filter(|&s| s > 0);
        if let Some(secs) = span {
            agg.total_seconds += secs;
        }

        // Distance: great-circle between the two snapshot fixes when we have
        // both. Missing/partial snapshots simply add nothing.
        if let (Some(la1), Some(lo1), Some(la2), Some(lo2)) = (
            parse_coord(&to_lat),
            parse_coord(&to_lon),
            parse_coord(&ld_lat),
            parse_coord(&ld_lon),
        ) {
            agg.total_distance_nm += haversine_nm(la1, lo1, la2, lo2);
        }
    }

    let mut stats: Vec<AircraftStat> = aggs
        .into_iter()
        .map(|(icao, a)| AircraftStat {
            icao,
            flights: a.flights,
            total_seconds: a.total_seconds,
            total_hours: (a.total_seconds as f64 / 360.0).round() / 10.0,
            total_distance_nm: (a.total_distance_nm * 10.0).round() / 10.0,
        })
        .collect();

    let mut by_flights = stats.clone();
    by_flights.sort_by(|a, b| b.flights.cmp(&a.flights).then_with(|| a.icao.cmp(&b.icao)));
    let mut by_time = stats.clone();
    by_time.sort_by(|a, b| b.total_seconds.cmp(&a.total_seconds).then_with(|| a.icao.cmp(&b.icao)));
    stats.sort_by(|a, b| {
        b.total_distance_nm
            .partial_cmp(&a.total_distance_nm)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.icao.cmp(&b.icao))
    });

    Ok(AircraftStatsResponse {
        by_flights,
        by_time,
        by_distance: stats,
    })
}

/// Whole number with thousands separators, e.g. `9877` -> `9,877`.
fn group_thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let first = s.len() % 3;
    for (i, ch) in s.chars().enumerate() {
        if i != 0 && (i - first) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// Turn a ranked `AircraftStat` list into display rows: bars are sized against
/// the leader's `metric`, and each row's caption carries the two off-axis
/// figures. `metric` selects which value drives the bar for this list.
fn to_stat_rows(
    stats: &[AircraftStat],
    metric: impl Fn(&AircraftStat) -> f64,
    value: impl Fn(&AircraftStat) -> String,
    sub: impl Fn(&AircraftStat) -> String,
) -> Vec<templates::StatRow> {
    let top = stats.first().map(&metric).filter(|&m| m > 0.0);
    stats
        .iter()
        .enumerate()
        .map(|(i, s)| templates::StatRow {
            rank: i + 1,
            icao: s.icao.clone(),
            value: value(s),
            sub: sub(s),
            pct: top
                .map(|t| ((metric(s) / t) * 100.0).round().clamp(0.0, 100.0) as u32)
                .unwrap_or(0),
        })
        .collect()
}

/// Public aircraft-usage leaderboard page (`/content/stats`): the same three
/// rankings as `GET /api/v0/stats/aircraft`, rendered as bar charts.
async fn stats_handler(State(state): State<AppState>) -> Result<Response, AppError> {
    let s = aggregate_aircraft_stats(&state.db).await?;

    let hours = |a: &AircraftStat| format!("{:.1} h", a.total_hours);
    let dist = |a: &AircraftStat| format!("{} nm", group_thousands(a.total_distance_nm.round() as i64));
    let count = |a: &AircraftStat| {
        format!("{} flight{}", a.flights, if a.flights == 1 { "" } else { "s" })
    };

    let page = templates::StatsPage {
        by_flights: to_stat_rows(
            &s.by_flights,
            |a| a.flights as f64,
            &count,
            |a| format!("{} · {}", hours(a), dist(a)),
        ),
        by_time: to_stat_rows(
            &s.by_time,
            |a| a.total_seconds as f64,
            &hours,
            |a| format!("{} · {}", count(a), dist(a)),
        ),
        by_distance: to_stat_rows(
            &s.by_distance,
            |a| a.total_distance_nm,
            &dist,
            |a| format!("{} · {}", count(a), hours(a)),
        ),
    };
    Ok(Html(page.render()?).into_response())
}

/// The current-flight telemetry contract consumed by freeflight's live map
/// (services/ff-api proxies this per-user). A clean, typed extraction of the
/// live position rather than the raw flight row — see `docs/API.md`.
#[derive(Serialize)]
struct CurrentFlight {
    flight_id: i64,
    departure: String,
    /// `None` while still en route (no arrival filed yet).
    arrival: Option<String>,
    aircraft_type: String,
    /// `None` until the flight reports a usable position.
    position: Option<LivePosition>,
    /// Seconds since the flight last reported — clients fade a stale
    /// position rather than showing it as live.
    updated_ago_secs: i64,
    updated_at: chrono::DateTime<chrono::Utc>,
}

/// The columns both current-flight lookups select, in order.
type CurrentFlightRow = (i64, String, Option<String>, serde_json::Value, chrono::DateTime<chrono::Utc>);

fn build_current_flight(row: CurrentFlightRow, now: chrono::DateTime<chrono::Utc>) -> CurrentFlight {
    let (id, departure, arrival, statistics, updated_at) = row;
    CurrentFlight {
        flight_id: id,
        departure,
        arrival,
        aircraft_type: aircraft_type_of(&statistics),
        position: extract_live_position(&statistics),
        updated_ago_secs: now.signed_duration_since(updated_at).num_seconds(),
        updated_at,
    }
}

/// Current flight by Butterlog's internal numeric user id.
async fn user_current_flight_handler(
    State(state): State<AppState>,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let flight_row: Option<CurrentFlightRow> = sqlx::query_as(
        "SELECT id, departure, arrival, statistics, updated_at \
         FROM flights \
         WHERE user_id = $1 AND updated_at > NOW() - INTERVAL '5 minutes' \
         ORDER BY updated_at DESC \
         LIMIT 1"
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    let now = chrono::Utc::now();
    // `None` (JSON `null`) when the user isn't currently flying.
    Ok(axum::Json(flight_row.map(|row| build_current_flight(row, now))))
}

/// Current flight by the pilot's Discord id — lets a client that already
/// knows who's signed in (freeflight authenticates with Discord, and
/// `users.discord_id` is that same id) show a pilot's live flight without
/// them looking up their numeric Butterlog id.
async fn user_current_flight_by_discord_handler(
    State(state): State<AppState>,
    axum::extract::Path(discord_id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let flight_row: Option<CurrentFlightRow> = sqlx::query_as(
        "SELECT f.id, f.departure, f.arrival, f.statistics, f.updated_at \
         FROM flights f \
         JOIN users u ON f.user_id = u.id \
         WHERE u.discord_id = $1 AND f.updated_at > NOW() - INTERVAL '5 minutes' \
         ORDER BY f.updated_at DESC \
         LIMIT 1"
    )
    .bind(discord_id)
    .fetch_optional(&state.db)
    .await?;

    let now = chrono::Utc::now();
    Ok(axum::Json(flight_row.map(|row| build_current_flight(row, now))))
}

async fn map_handler() -> Result<Response, AppError> {
    Ok(Html(templates::MapPage.render()?).into_response())
}

async fn flight_share_detail_handler(
    State(state): State<AppState>,
    axum::extract::Path(share_id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
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

    let json_str = match handlers::decompress_gzip_capped(compressed.as_slice(), handlers::MAX_SHARE_DECOMPRESSED) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to decompress share {}: {}", share_id, e);
            return Ok((axum::http::StatusCode::INTERNAL_SERVER_ERROR, Html("<h1>Failed to decompress share data</h1>".to_string())).into_response());
        }
    };

    let page = templates::ShareDetailPage {
        share_id,
        is_owner,
        json_escaped: json_str.replace('\\', "\\\\").replace("</", "<\\/"),
    };

    let mut response = Html(page.render()?).into_response();
    response.headers_mut().insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_live_position_reads_capitalized_snapshot_keys() {
        let stats = serde_json::json!({
            "current_snapshot": {
                "Latitude": 34.05, "Longitude": -118.24,
                "AltMSL": 12500.0, "HDG": 271.3, "GndSpd": 289.0
            }
        });
        let p = extract_live_position(&stats).expect("position");
        assert_eq!(p.latitude, 34.05);
        assert_eq!(p.longitude, -118.24);
        assert_eq!(p.altitude, 12500.0);
        assert_eq!(p.heading, 271.3);
        assert_eq!(p.speed, 289.0);
    }

    #[test]
    fn extract_live_position_accepts_lowercase_and_defaults_missing() {
        // Older clients sent lowercase lat/lon; a snapshot with only a fix
        // still yields a position, with the other fields defaulted to 0.
        let stats = serde_json::json!({ "current_snapshot": { "latitude": 1.0, "longitude": 2.0 } });
        let p = extract_live_position(&stats).expect("position");
        assert_eq!((p.latitude, p.longitude), (1.0, 2.0));
        assert_eq!((p.altitude, p.heading, p.speed), (0.0, 0.0, 0.0));
    }

    #[test]
    fn extract_live_position_is_none_without_snapshot_or_latlon() {
        assert!(extract_live_position(&serde_json::json!({})).is_none());
        assert!(
            extract_live_position(&serde_json::json!({ "current_snapshot": { "AltMSL": 100.0 } }))
                .is_none()
        );
    }

    #[test]
    fn redact_path_hides_webhook_token() {
        assert_eq!(
            redact_path("/api/v0/users/abc123def/flights/42"),
            "/api/v0/users/***/flights/42"
        );
        assert_eq!(redact_path("/api/v0/users/abc123def"), "/api/v0/users/***");
        assert_eq!(redact_path("/content/flights/7"), "/content/flights/7");
        assert_eq!(redact_path("/"), "/");
    }

    #[test]
    fn esc_escapes_html_metacharacters() {
        assert_eq!(
            esc(r#"<script>alert("x") & 'y'</script>"#),
            "&lt;script&gt;alert(&quot;x&quot;) &amp; 'y'&lt;/script&gt;"
        );
        assert_eq!(esc("plain text"), "plain text");
    }

    #[test]
    fn landing_rating_thresholds() {
        assert_eq!(landing_rating(-50.0).1, "BUTTER");
        assert_eq!(landing_rating(-150.0).1, "BUTTER");
        assert_eq!(landing_rating(-151.0).1, "SMOOTH");
        assert_eq!(landing_rating(-250.0).1, "SMOOTH");
        assert_eq!(landing_rating(-300.0).1, "FIRM");
        assert_eq!(landing_rating(-350.0).1, "FIRM");
        assert_eq!(landing_rating(-500.0).1, "HARD");
    }

    #[test]
    fn landing_badge_html_renders_or_skips() {
        let landed = serde_json::json!({
            "landing_snapshot": { "VSpd": -121.0, "NormAc": 1.25 }
        });
        let html = landing_badge_html(&landed).expect("badge for landed flight");
        assert!(html.contains("badge-butter"));
        assert!(html.contains("121 fpm"));
        assert!(html.contains("1.25G"));

        let airborne = serde_json::json!({ "current_snapshot": {} });
        assert!(landing_badge_html(&airborne).is_none());
    }

    #[test]
    fn get_cookie_parses_header() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("a=1; oauth_state=n0nce; token=t=with=equals"),
        );
        assert_eq!(get_cookie(&headers, "oauth_state").as_deref(), Some("n0nce"));
        assert_eq!(get_cookie(&headers, "token").as_deref(), Some("t=with=equals"));
        assert_eq!(get_cookie(&headers, "missing"), None);
    }

    /// The header-auth routes share the /api/v0/flights prefix with static
    /// segments (share) next to :id params; axum panics at router build time
    /// on conflicts, so building the same shape here catches that in CI.
    #[test]
    fn flight_route_shapes_do_not_conflict() {
        use axum::routing::{delete, get, post, put};
        let _router: axum::Router = axum::Router::new()
            .route("/api/v0/flights", post(|| async {}))
            .route("/api/v0/flights/:id", put(|| async {}).get(|| async {}))
            .route("/api/v0/flights/:id/notes", put(|| async {}))
            .route("/api/v0/flights/:id/screenshots", post(|| async {}))
            .route("/api/v0/flights/:id/screenshots/:hash", delete(|| async {}))
            .route("/api/v0/flights/share", post(|| async {}))
            .route("/api/v0/flights/share/:share_id", get(|| async {}).delete(|| async {}))
            .route("/api/v0/flights/:id/track", post(|| async {}))
            .route("/api/v0/flights/:id/track/cursor", get(|| async {}))
            .route("/api/v0/flights/:id/end", post(|| async {}))
            .route("/api/v0/flights/:id/live", get(|| async {}))
            .route("/api/v0/flights/:id/commands", get(|| async {}).post(|| async {}))
            .route("/api/v0/flights/:id/commands/:cid", get(|| async {}))
            .route("/api/v0/flights/:id/commands/:cid/ack", post(|| async {}))
            .route("/api/v0/multiplayer/ping", post(|| async {}))
            .route("/api/v0/user/:user_id/current", get(|| async {}));
    }
}
