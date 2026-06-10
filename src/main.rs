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
mod templates;

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

    // Build the router with trace logging
    let app = Router::new()
        .route("/", get(home_handler))
        .route("/content", get(content_handler))
        .route("/content/flight/user/:user_id", get(content_user_handler))
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
    axum::extract::Path(flight_id): axum::extract::Path<i64>,
) -> Result<Response, AppError> {
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

    let page = templates::FlightDetailPage {
        dep,
        arr_display: arr.unwrap_or_else(|| "In Flight".to_string()),
        pilot: global_name.unwrap_or(username),
        airframe: stats
            .get("airframe_name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown Aircraft")
            .to_string(),
        simulator: stats.get("simulator").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        date_str: created_at.format("%B %d, %Y, %H:%M UTC").to_string(),
        landing_badge: landing_badge_html(&stats).unwrap_or_default(),
        notes: notes.as_deref().map(str::trim).unwrap_or("").to_string(),
        urls_json: serde_json::to_string(&screenshots).unwrap_or_default(),
        screenshots,
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
                u.id, u.username, u.global_name, u.avatar, u.discord_id \
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
                u.id, u.username, u.global_name, u.avatar, u.discord_id \
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
            let landing_badge = if stats.get("landing_snapshot").is_some() {
                landing_badge_html(&stats).unwrap_or_default()
            } else {
                r#"<div class="badge badge-ongoing">ONGOING</div>"#.to_string()
            };
            templates::FlightCard {
                share_href: share_by_flight
                    .get(&flight_id)
                    .map(|sid| format!("/content/flights/share/{}", sid))
                    .unwrap_or_default(),
                avatar_url,
                pilot: flight.7.unwrap_or(flight.6),
                dep: flight.1,
                arr: flight.2.unwrap_or_else(|| "In Flight".to_string()),
                airframe: stats
                    .get("airframe_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Aircraft")
                    .to_string(),
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
}

/// Intentionally public (no auth): the live map shows every active flight's
/// position to anyone, mirroring the public /content flight history.
async fn map_data_handler(
    State(state): State<AppState>,
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
}
