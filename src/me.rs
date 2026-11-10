//! `GET /api/v0/me`: who the app is signed in as, and the flights it needs to know of.
//!
//! The app no longer logs flights itself: the service makes them from its telemetry. So
//! it asks here for the flight being flown (its web commands are that flight's) and the
//! last one finished (the tray offers its debrief), along with the account it shows in
//! its Service panel.

use axum::{extract::State, http::HeaderMap, Json};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    pub user_id: i64,
    pub username: String,
    pub display_name: String,
    /// The Discord avatar, when the user has one.
    pub avatar_url: Option<String>,
    /// The user's flights on the web.
    pub flights_url: String,
    pub settings_url: String,
    pub flysto_connected: bool,
    /// Whether ended flights go to FlySto by themselves.
    pub flysto_auto: bool,
    /// The flight being flown, if any.
    pub live: Option<FlightRef>,
    /// The last flight finished, if any.
    pub last_ended: Option<FlightRef>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FlightRef {
    pub id: i64,
    pub url: String,
    /// The page with its Debrief open.
    pub debrief_url: String,
    pub departure: String,
    pub arrival: Option<String>,
    pub aircraft: Option<String>,
    pub ended_at: Option<DateTime<Utc>>,
}

type Row = (i64, String, Option<String>, Option<String>, Option<DateTime<Utc>>);

fn flight(base: &str, (id, departure, arrival, aircraft, ended_at): Row) -> FlightRef {
    FlightRef {
        id,
        url: format!("{base}/content/flights/{id}"),
        debrief_url: format!("{base}/content/flights/{id}#debrief"),
        departure,
        arrival,
        aircraft,
        ended_at,
    }
}

pub async fn me(db: &PgPool, base: &str, user_id: i64) -> Result<Me, AppError> {
    let (username, global_name, discord_id, avatar, flysto, flysto_auto): (String, Option<String>, String, Option<String>, bool, bool) =
        sqlx::query_as(
            "SELECT username, global_name, discord_id, avatar, flysto_token IS NOT NULL, flysto_auto FROM users WHERE id = $1",
        )
        .bind(user_id)
        .fetch_one(db)
        .await?;
    let newest = |status: &'static str| {
        sqlx::query_as::<_, Row>(
            "SELECT id, departure, arrival, \
                    COALESCE(NULLIF(statistics->>'airframe_name', ''), NULLIF(statistics->>'resolved_icao', '')), \
                    ended_at \
               FROM flights WHERE user_id = $1 AND status = $2 \
              ORDER BY COALESCE(ended_at, updated_at) DESC, id DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(status)
        .fetch_optional(db)
    };
    let live = newest("active").await?.map(|r| flight(base, r));
    let last_ended = newest("ended").await?.map(|r| flight(base, r));
    Ok(Me {
        user_id,
        display_name: global_name.filter(|n| !n.is_empty()).unwrap_or_else(|| username.clone()),
        username,
        avatar_url: avatar
            .filter(|a| !a.is_empty())
            .map(|a| format!("https://cdn.discordapp.com/avatars/{discord_id}/{a}.png?size=64")),
        flights_url: format!("{base}/content"),
        settings_url: format!("{base}/content/settings"),
        flysto_connected: flysto,
        flysto_auto,
        live,
        last_ended,
    })
}

pub async fn me_handler(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Me>, AppError> {
    let user_id = crate::handlers::get_user_id_from_session(&state.db, &headers).await?;
    me(&state.db, &state.config.public_base_url, user_id).await.map(Json)
}

/// Against a real Postgres, given by `TEST_DATABASE_URL` (skipped without it).
#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let pool = PgPool::connect(&url).await.expect("TEST_DATABASE_URL reachable");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations");
        Some(pool)
    }

    #[tokio::test]
    async fn the_app_learns_the_live_flight_and_the_last_finished_one() {
        let Some(db) = db().await else { return };
        let discord = uuid::Uuid::new_v4().to_string();
        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (discord_id, username, global_name, avatar) VALUES ($1, 'pilot', 'The Pilot', 'abc') RETURNING id",
        )
        .bind(&discord)
        .fetch_one(&db)
        .await
        .unwrap();

        let empty = me(&db, "https://b", user).await.unwrap();
        assert_eq!((empty.live, empty.last_ended), (None, None));
        assert_eq!(empty.display_name, "The Pilot");
        assert_eq!(empty.avatar_url, Some(format!("https://cdn.discordapp.com/avatars/{discord}/abc.png?size=64")));

        let add = |status: &'static str, ended: Option<&'static str>, dep: &'static str| {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "INSERT INTO flights (user_id, departure, arrival, statistics, status, ended_at) \
                     VALUES ($1, $2, 'KSEA', '{\"airframe_name\":\"Cessna 172\"}', $3, $4::timestamptz) RETURNING id",
                )
                .bind(user)
                .bind(dep)
                .bind(status)
                .bind(ended)
                .fetch_one(&db)
                .await
                .unwrap()
            }
        };
        let _older = add("ended", Some("2026-11-01T10:00:00Z"), "KBFI").await;
        let newer = add("ended", Some("2026-11-02T10:00:00Z"), "KPAE").await;
        let live = add("active", None, "KSEA").await;

        let got = me(&db, "https://b", user).await.unwrap();
        let last = got.last_ended.unwrap();
        assert_eq!((last.id, last.departure.as_str()), (newer, "KPAE"));
        assert_eq!(last.debrief_url, format!("https://b/content/flights/{newer}#debrief"));
        assert_eq!(last.aircraft.as_deref(), Some("Cessna 172"));
        assert_eq!(got.live.unwrap().id, live);
    }
}
