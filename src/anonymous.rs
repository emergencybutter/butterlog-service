//! Anonymous accounts (docs/sim-native-telemetry.md §6.2).
//!
//! A user who hasn't signed in with Discord still streams. The app creates an anonymous
//! account on first run and gets an ordinary API token, so every write path works
//! unchanged, plus a separate link secret that only reads. Their flights are reachable
//! through the link, never through the public pages, and kept 14 days; signing in with
//! Discord claims them for good.

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{generate_token, hash_token};
use crate::error::AppError;
use crate::handlers::get_user_id_from_session;
use crate::r2::R2Client;
use crate::AppState;

/// How long an anonymous user's flight or session is kept after it ends.
pub const RETENTION_DAYS: i32 = 14;

/// Accounts one client address may create per hour.
const SIGNUPS_PER_ADDRESS_PER_HOUR: i64 = 5;

/// Accounts created per hour across every address, a backstop for address spoofing.
const SIGNUPS_PER_HOUR: i64 = 500;

/// The cookie the link page sets, so the pages it leads to can be read.
const LINK_COOKIE: &str = "link";

/// Flights or sessions removed per sweep, so one sweep stays short.
const SWEEP_BATCH: i64 = 200;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnonymousAccount {
    pub user_id: i64,
    /// API token: the Bearer for every write.
    pub token: String,
    /// Read-only secret; the link is `/content/l/<linkSecret>`.
    pub link_secret: String,
    pub username: String,
    pub retention_days: i32,
}

/// 122 random bits, as hex.
fn generate_link_secret() -> String {
    Uuid::new_v4().simple().to_string()
}

/// The client address, as the proxies in front of the service report it.
fn client_address(headers: &HeaderMap) -> String {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header("cf-connecting-ip")
        .or_else(|| header("x-real-ip"))
        // nginx appends the address it saw, so the last entry is the one it vouches for.
        .or_else(|| header("x-forwarded-for").and_then(|v| v.rsplit(',').next()))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

pub async fn create(db: &PgPool, address: &str) -> Result<AnonymousAccount, AppError> {
    let mut tx = db.begin().await?;
    // Serialises signups, so concurrent ones cannot both pass the limit.
    sqlx::query("LOCK TABLE anonymous_signups IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let (from_address, total): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE address = $1), COUNT(*) \
           FROM anonymous_signups WHERE created_at > NOW() - INTERVAL '1 hour'",
    )
    .bind(address)
    .fetch_one(&mut *tx)
    .await?;
    if from_address >= SIGNUPS_PER_ADDRESS_PER_HOUR || total >= SIGNUPS_PER_HOUR {
        return Err(AppError::TooManyRequests(
            "Too many anonymous accounts created; try again later".to_string(),
        ));
    }
    sqlx::query("INSERT INTO anonymous_signups (address) VALUES ($1)")
        .bind(address)
        .execute(&mut *tx)
        .await?;

    let link_secret = generate_link_secret();
    let username = format!("Pilot {}", &Uuid::new_v4().simple().to_string()[..4].to_uppercase());
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO users (username, anonymous_since, link_secret_hash) \
         VALUES ($1, NOW(), $2) RETURNING id",
    )
    .bind(&username)
    .bind(hash_token(&link_secret))
    .fetch_one(&mut *tx)
    .await?;

    let token = generate_token();
    sqlx::query("INSERT INTO api_tokens (token_hash, user_id) VALUES ($1, $2)")
        .bind(hash_token(&token))
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(AnonymousAccount { user_id, token, link_secret, username, retention_days: RETENTION_DAYS })
}

/// Replaces the user's link secret; the old link stops working.
pub async fn rotate_link(db: &PgPool, user_id: i64) -> Result<String, AppError> {
    let secret = generate_link_secret();
    let updated = sqlx::query(
        "UPDATE users SET link_secret_hash = $1 WHERE id = $2 AND anonymous_since IS NOT NULL",
    )
    .bind(hash_token(&secret))
    .bind(user_id)
    .execute(db)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(AppError::Forbidden("Only anonymous accounts have a link".to_string()));
    }
    Ok(secret)
}

/// The anonymous user a link secret reads.
pub async fn user_for_link(db: &PgPool, secret: &str) -> Result<Option<i64>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM users WHERE link_secret_hash = $1 AND anonymous_since IS NOT NULL",
    )
    .bind(hash_token(secret))
    .fetch_optional(db)
    .await?)
}

/// Whether the request may see what `owner_id` flew: anyone for a signed-in user; for an
/// anonymous one, only its own token or a link cookie for it.
pub async fn can_view(db: &PgPool, headers: &HeaderMap, owner_id: i64) -> Result<bool, AppError> {
    let anonymous: Option<bool> =
        sqlx::query_scalar("SELECT anonymous_since IS NOT NULL FROM users WHERE id = $1")
            .bind(owner_id)
            .fetch_optional(db)
            .await?;
    match anonymous {
        None => Ok(false),
        Some(false) => Ok(true),
        Some(true) => {
            if let Some(secret) = cookie(headers, LINK_COOKIE)
                && user_for_link(db, secret).await? == Some(owner_id)
            {
                return Ok(true);
            }
            Ok(get_user_id_from_session(db, headers).await.ok() == Some(owner_id))
        }
    }
}

/// Whether `user_id` is anonymous, and if so, the headers its pages carry: kept out of
/// search engines, and no link secret leaked through a Referer.
pub async fn private_headers(db: &PgPool, user_id: i64) -> Result<Vec<(header::HeaderName, HeaderValue)>, AppError> {
    let anonymous: Option<bool> =
        sqlx::query_scalar("SELECT anonymous_since IS NOT NULL FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(db)
            .await?;
    Ok(if anonymous == Some(true) {
        vec![
            (header::HeaderName::from_static("x-robots-tag"), HeaderValue::from_static("noindex")),
            (header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
        ]
    } else {
        Vec::new()
    })
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Claimed {
    pub flights: i64,
    pub sessions: i64,
}

/// Moves everything an anonymous account holds to the signed-in `user_id` and deletes the
/// anonymous account, so its token stops working. A Discord login always creates or
/// updates its user first, so claiming into a new or an existing account is the same move.
pub async fn claim(db: &PgPool, user_id: i64, anonymous_token: &str) -> Result<Claimed, AppError> {
    let mut tx = db.begin().await?;
    let claimer_anonymous: bool =
        sqlx::query_scalar("SELECT anonymous_since IS NOT NULL FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
    if claimer_anonymous {
        return Err(AppError::Forbidden("Sign in before claiming flights".to_string()));
    }
    let anonymous_id: Option<i64> = sqlx::query_scalar(
        "SELECT u.id FROM api_tokens t JOIN users u ON u.id = t.user_id \
          WHERE t.token_hash = $1 AND u.anonymous_since IS NOT NULL \
          FOR UPDATE OF u",
    )
    .bind(hash_token(anonymous_token))
    .fetch_optional(&mut *tx)
    .await?;
    let Some(anonymous_id) = anonymous_id else {
        return Err(AppError::NotFound("No anonymous account for this token".to_string()));
    };

    let flights = sqlx::query("UPDATE flights SET user_id = $1 WHERE user_id = $2")
        .bind(user_id)
        .bind(anonymous_id)
        .execute(&mut *tx)
        .await?
        .rows_affected() as i64;
    let sessions = sqlx::query("UPDATE telemetry_sessions SET user_id = $1 WHERE user_id = $2")
        .bind(user_id)
        .bind(anonymous_id)
        .execute(&mut *tx)
        .await?
        .rows_affected() as i64;
    sqlx::query("UPDATE flight_shares SET user_id = $1 WHERE user_id = $2")
        .bind(user_id)
        .bind(anonymous_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE flight_commands SET issued_by = $1 WHERE issued_by = $2")
        .bind(user_id)
        .bind(anonymous_id)
        .execute(&mut *tx)
        .await?;
    // Tokens, peer presence and notification channels go with the row.
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(anonymous_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Claimed { flights, sessions })
}

/// Deletes objects in R2. A trait so the sweeper can be tested without R2.
pub trait ObjectStore {
    /// An object in the public bucket (screenshots, shares).
    async fn delete_public(&self, key: &str) -> Result<(), String>;
    /// A compacted session segment in the telemetry bucket.
    async fn delete_segment(&self, key: &str) -> Result<(), String>;
}

pub struct R2Objects {
    pub r2: R2Client,
    pub telemetry_bucket: Option<String>,
}

impl ObjectStore for R2Objects {
    async fn delete_public(&self, key: &str) -> Result<(), String> {
        self.r2.delete_object(key).await
    }

    async fn delete_segment(&self, key: &str) -> Result<(), String> {
        match &self.telemetry_bucket {
            Some(bucket) => self.r2.delete_private(bucket, key).await,
            None => Err("R2_TELEMETRY_BUCKET is not set".to_string()),
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Swept {
    pub flights: usize,
    pub sessions: usize,
    pub users: u64,
}

/// The R2 key of a screenshot, as `delete_screenshot_core` derives it.
fn screenshot_key(flight_id: i64, hash: &str, url: &str) -> String {
    let ext = if url.ends_with(".webp") { "webp" } else { "jpg" };
    format!("screenshots/{flight_id}/{hash}.{ext}")
}

/// Deletes what anonymous users have kept past retention: flights with their screenshots
/// and shares, sessions with their segments, then accounts left with nothing and unused.
/// Objects go first, so a failure leaves the row for the next sweep, never an orphan.
pub async fn sweep(db: &PgPool, store: &impl ObjectStore) -> Result<Swept, AppError> {
    let mut swept = Swept::default();

    let flights: Vec<i64> = sqlx::query_scalar(
        "SELECT f.id FROM flights f JOIN users u ON u.id = f.user_id \
          WHERE u.anonymous_since IS NOT NULL \
            AND COALESCE(f.ended_at, f.updated_at) < NOW() - make_interval(days => $1) \
          ORDER BY f.id LIMIT $2",
    )
    .bind(RETENTION_DAYS)
    .bind(SWEEP_BATCH)
    .fetch_all(db)
    .await?;
    'flights: for flight_id in flights {
        let screenshots: Vec<(String, String)> =
            sqlx::query_as("SELECT hash, url FROM screenshots WHERE flight_id = $1")
                .bind(flight_id)
                .fetch_all(db)
                .await?;
        let shares: Vec<String> =
            sqlx::query_scalar(
                "SELECT r2_key FROM flight_shares                   WHERE remote_flight_id = $1 OR id = (SELECT share_id FROM flights WHERE id = $1)",
            )
                .bind(flight_id)
                .fetch_all(db)
                .await?;
        let keys = screenshots
            .iter()
            .map(|(hash, url)| screenshot_key(flight_id, hash, url))
            .chain(shares);
        for key in keys {
            if let Err(e) = store.delete_public(&key).await {
                tracing::error!(flight_id, key, "anonymous sweep could not delete object: {e}");
                continue 'flights;
            }
        }
        let mut tx = db.begin().await?;
        sqlx::query(
            "DELETE FROM flight_shares               WHERE remote_flight_id = $1 OR id = (SELECT share_id FROM flights WHERE id = $1)",
        )
            .bind(flight_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM flights WHERE id = $1")
            .bind(flight_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        swept.flights += 1;
    }

    let sessions: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT s.id, s.segment_key FROM telemetry_sessions s JOIN users u ON u.id = s.user_id \
          WHERE u.anonymous_since IS NOT NULL \
            AND COALESCE(s.ended_at, s.last_chunk_at, s.started_at) \
                < NOW() - make_interval(days => $1) \
          ORDER BY s.started_at LIMIT $2",
    )
    .bind(RETENTION_DAYS)
    .bind(SWEEP_BATCH)
    .fetch_all(db)
    .await?;
    for (id, segment_key) in sessions {
        if let Some(key) = segment_key
            && let Err(e) = store.delete_segment(&key).await
        {
            tracing::error!(%id, key, "anonymous sweep could not delete segment: {e}");
            continue;
        }
        sqlx::query("DELETE FROM telemetry_sessions WHERE id = $1")
            .bind(id)
            .execute(db)
            .await?;
        swept.sessions += 1;
    }

    swept.users = sqlx::query(
        "DELETE FROM users u \
          WHERE u.anonymous_since < NOW() - make_interval(days => $1) \
            AND NOT EXISTS (SELECT 1 FROM flights f WHERE f.user_id = u.id) \
            AND NOT EXISTS (SELECT 1 FROM telemetry_sessions s WHERE s.user_id = u.id) \
            AND NOT EXISTS (SELECT 1 FROM api_tokens t WHERE t.user_id = u.id \
                   AND COALESCE(t.last_used_at, t.created_at) > NOW() - make_interval(days => $1))",
    )
    .bind(RETENTION_DAYS)
    .execute(db)
    .await?
    .rows_affected();

    sqlx::query("DELETE FROM anonymous_signups WHERE created_at < NOW() - INTERVAL '1 day'")
        .execute(db)
        .await?;
    Ok(swept)
}

/// Sweeps every hour.
pub fn spawn_sweeper(db: PgPool, store: R2Objects) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            ticker.tick().await;
            match sweep(&db, &store).await {
                Ok(s) if s != Swept::default() => tracing::info!(
                    flights = s.flights,
                    sessions = s.sessions,
                    users = s.users,
                    "swept expired anonymous data"
                ),
                Err(e) => tracing::error!("anonymous sweep failed: {e:?}"),
                _ => {}
            }
        }
    });
}

pub async fn create_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account = create(&state.db, &client_address(&headers)).await?;
    Ok((StatusCode::CREATED, Json(account)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub link_secret: String,
}

pub async fn rotate_link_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Link>, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    Ok(Json(Link { link_secret: rotate_link(&state.db, user_id).await? }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRequest {
    pub anonymous_token: String,
}

pub async fn claim_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ClaimRequest>,
) -> Result<Json<Claimed>, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    claim(&state.db, user_id, &body.anonymous_token).await.map(Json)
}

/// `/content/l/<secret>`: remembers the link in a cookie and opens the user's flights, so every
/// page and live update after it can be read without the secret in the URL.
pub async fn link_handler(
    State(state): State<AppState>,
    Path(secret): Path<String>,
) -> Result<Response, AppError> {
    let private = [
        (header::HeaderName::from_static("x-robots-tag"), HeaderValue::from_static("noindex")),
        (header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
    ];
    let Some(user_id) = user_for_link(&state.db, &secret).await? else {
        return Ok((StatusCode::NOT_FOUND, private, "This link is not valid, or has been replaced.")
            .into_response());
    };
    let cookie = format!(
        "{LINK_COOKIE}={secret}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={}",
        RETENTION_DAYS as i64 * 86_400
    );
    let mut response = Redirect::to(&format!("/content/flight/user/{user_id}")).into_response();
    let headers = response.headers_mut();
    for (name, value) in private {
        headers.insert(name, value);
    }
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| AppError::NotFound("No such link".to_string()))?,
    );
    Ok(response)
}

/// Against a real Postgres, given by `TEST_DATABASE_URL` (skipped without it), as in
/// `sessions.rs`.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Sweeps act on every anonymous user, so tests that sweep run one at a time.
    static SWEEPING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn db() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let pool = PgPool::connect(&url).await.expect("TEST_DATABASE_URL reachable");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations");
        Some(pool)
    }

    fn address() -> String {
        format!("test-{}", Uuid::new_v4())
    }

    async fn discord_user(db: &PgPool) -> (i64, String) {
        let user = crate::auth::DiscordUser {
            id: Uuid::new_v4().as_u128().to_string(),
            username: "pilot".into(),
            global_name: None,
            avatar: None,
        };
        let token = crate::auth::save_or_update_user(db, &user).await.unwrap();
        let id = sqlx::query_scalar("SELECT id FROM users WHERE discord_id = $1")
            .bind(&user.id)
            .fetch_one(db)
            .await
            .unwrap();
        (id, token)
    }

    async fn flight(db: &PgPool, user_id: i64, ended_days_ago: i32) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO flights (user_id, departure, statistics, status, ended_at) \
             VALUES ($1, 'KTYS', '{}', 'ended', NOW() - make_interval(days => $2)) RETURNING id",
        )
        .bind(user_id)
        .bind(ended_days_ago)
        .fetch_one(db)
        .await
        .unwrap()
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        h
    }

    fn link_cookie(secret: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, format!("theme=dark; link={secret}").parse().unwrap());
        h
    }

    #[derive(Default)]
    struct Store {
        deleted: Mutex<Vec<String>>,
        fail: bool,
    }

    impl ObjectStore for Store {
        async fn delete_public(&self, key: &str) -> Result<(), String> {
            if self.fail {
                return Err("down".into());
            }
            self.deleted.lock().unwrap().push(key.to_string());
            Ok(())
        }

        async fn delete_segment(&self, key: &str) -> Result<(), String> {
            self.delete_public(key).await
        }
    }

    #[tokio::test]
    async fn the_token_writes_and_the_link_only_reads() {
        let Some(db) = db().await else { return };
        let account = create(&db, &address()).await.unwrap();
        assert!(account.username.starts_with("Pilot "));
        assert_eq!(
            get_user_id_from_session(&db, &bearer(&account.token)).await.unwrap(),
            account.user_id
        );
        assert!(get_user_id_from_session(&db, &bearer(&account.link_secret)).await.is_err());
        assert_eq!(user_for_link(&db, &account.link_secret).await.unwrap(), Some(account.user_id));

        let (other, other_token) = discord_user(&db).await;
        assert!(can_view(&db, &HeaderMap::new(), other).await.unwrap(), "signed-in users are public");
        assert!(!can_view(&db, &HeaderMap::new(), account.user_id).await.unwrap());
        assert!(!can_view(&db, &bearer(&other_token), account.user_id).await.unwrap());
        assert!(can_view(&db, &bearer(&account.token), account.user_id).await.unwrap());
        assert!(can_view(&db, &link_cookie(&account.link_secret), account.user_id).await.unwrap());

        let rotated = rotate_link(&db, account.user_id).await.unwrap();
        assert!(!can_view(&db, &link_cookie(&account.link_secret), account.user_id).await.unwrap());
        assert!(can_view(&db, &link_cookie(&rotated), account.user_id).await.unwrap());
        assert!(matches!(rotate_link(&db, other).await, Err(AppError::Forbidden(_))));
    }

    #[tokio::test]
    async fn signups_are_limited_per_address() {
        let Some(db) = db().await else { return };
        let address = address();
        for _ in 0..SIGNUPS_PER_ADDRESS_PER_HOUR {
            create(&db, &address).await.unwrap();
        }
        assert!(matches!(create(&db, &address).await, Err(AppError::TooManyRequests(_))));
        create(&db, &self::address()).await.unwrap();
    }

    #[tokio::test]
    async fn claiming_moves_everything_and_retires_the_anonymous_account() {
        let Some(db) = db().await else { return };
        let _sweeping = SWEEPING.lock().await;
        let account = create(&db, &address()).await.unwrap();
        let old = flight(&db, account.user_id, 13).await;
        let (user, user_token) = discord_user(&db).await;
        let existing = flight(&db, user, 1).await;

        assert!(matches!(
            claim(&db, account.user_id, &account.token).await,
            Err(AppError::Forbidden(_))
        ));
        let claimed = claim(&db, user, &account.token).await.unwrap();
        assert_eq!(claimed, Claimed { flights: 1, sessions: 0 });

        let owners: Vec<i64> = sqlx::query_scalar("SELECT user_id FROM flights WHERE id = ANY($1)")
            .bind(vec![old, existing])
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(owners, vec![user, user]);
        assert!(get_user_id_from_session(&db, &bearer(&account.token)).await.is_err());
        assert_eq!(user_for_link(&db, &account.link_secret).await.unwrap(), None);
        assert!(get_user_id_from_session(&db, &bearer(&user_token)).await.is_ok());
        assert!(matches!(claim(&db, user, &account.token).await, Err(AppError::NotFound(_))));

        // Claimed, the 13-day-old flight is no longer anonymous, so it is never swept.
        sqlx::query("UPDATE flights SET ended_at = NOW() - INTERVAL '30 days' WHERE id = $1")
            .bind(old)
            .execute(&db)
            .await
            .unwrap();
        sweep(&db, &Store::default()).await.unwrap();
        let kept: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM flights WHERE id = $1)")
            .bind(old)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(kept);
    }

    #[tokio::test]
    async fn the_sweep_deletes_expired_anonymous_flights_with_their_objects() {
        let Some(db) = db().await else { return };
        let _sweeping = SWEEPING.lock().await;
        let account = create(&db, &address()).await.unwrap();
        let expired = flight(&db, account.user_id, 15).await;
        let recent = flight(&db, account.user_id, 13).await;
        let (user, _) = discord_user(&db).await;
        let signed_in = flight(&db, user, 400).await;
        sqlx::query("INSERT INTO screenshots (flight_id, hash, url) VALUES ($1, 'abc', 'https://x/abc.webp')")
            .bind(expired)
            .execute(&db)
            .await
            .unwrap();
        let share = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO flight_shares (id, user_id, remote_flight_id, r2_key) VALUES ($1, $2, $3, $4)")
            .bind(&share)
            .bind(account.user_id)
            .bind(expired)
            .bind(format!("shares/{share}.json.gz"))
            .execute(&db)
            .await
            .unwrap();

        // An object that cannot be deleted keeps the flight for the next sweep.
        sweep(&db, &Store { fail: true, ..Default::default() }).await.unwrap();
        let exists = |id: i64| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM flights WHERE id = $1)")
                    .bind(id)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        assert!(exists(expired).await);

        let store = Store::default();
        sweep(&db, &store).await.unwrap();
        assert!(!exists(expired).await);
        assert!(exists(recent).await);
        assert!(exists(signed_in).await);
        let mut deleted = store.deleted.lock().unwrap().clone();
        deleted.sort();
        assert_eq!(
            deleted,
            vec![format!("screenshots/{expired}/abc.webp"), format!("shares/{share}.json.gz")]
        );
        let share_left: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM flight_shares WHERE id = $1)")
            .bind(&share)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(!share_left);
    }

    #[tokio::test]
    async fn the_sweep_retires_idle_empty_accounts_only() {
        let Some(db) = db().await else { return };
        let _sweeping = SWEEPING.lock().await;
        let idle = create(&db, &address()).await.unwrap();
        let active = create(&db, &address()).await.unwrap();
        let with_flight = create(&db, &address()).await.unwrap();
        flight(&db, with_flight.user_id, 1).await;
        for id in [idle.user_id, active.user_id, with_flight.user_id] {
            sqlx::query("UPDATE users SET anonymous_since = NOW() - INTERVAL '20 days' WHERE id = $1")
                .bind(id)
                .execute(&db)
                .await
                .unwrap();
            sqlx::query("UPDATE api_tokens SET created_at = NOW() - INTERVAL '20 days' WHERE user_id = $1")
                .bind(id)
                .execute(&db)
                .await
                .unwrap();
        }
        // Using the token marks it used.
        get_user_id_from_session(&db, &bearer(&active.token)).await.unwrap();

        sweep(&db, &Store::default()).await.unwrap();
        let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM users WHERE id = ANY($1) ORDER BY id")
            .bind(vec![idle.user_id, active.user_id, with_flight.user_id])
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(left, vec![active.user_id, with_flight.user_id]);
    }
}
