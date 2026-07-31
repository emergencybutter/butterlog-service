//! Commands from the web to a running simulator.
//!
//! The rest of the live-flight feature is public to read. This part is not:
//! remote control of somebody's simulator is owner-only in both directions, and
//! the app opts in explicitly before it will poll at all.
//!
//! Delivery is a long-poll rather than a ride-along on the 20s track upload,
//! because pause is a reflex action and a 20s worst case makes it unusable.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::handlers::get_user_id_from_session;
use crate::AppState;

/// Longest a client may ask us to hold a long-poll open.
const MAX_WAIT_SECS: u64 = 25;

/// How often the long-poll checks for work while parked.
const POLL_TICK_MS: u64 = 500;

pub const DEFAULT_TTL_SECS: i64 = 30;
pub const MAX_TTL_SECS: i64 = 120;

/// Commands per flight per minute. Normal use is a handful.
const RATE_LIMIT_PER_MIN: i64 = 30;

/// How sure we are that a command does what it says.
///
/// `pause` is a simulator-level operation: it works on every aircraft in both
/// sims. The autopilot commands drive the *default* autopilot, and study-level
/// add-ons (PMDG, Fenix, and most of what serious users fly) run their own
/// autopilot logic against internal LVARs and ignore the stock events. Such a
/// command is delivered, reported applied, and does nothing — a failure the
/// service cannot see. Hence the label rather than a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stability {
    Stable,
    Beta,
}

impl Stability {
    pub fn as_str(self) -> &'static str {
        match self {
            Stability::Stable => "stable",
            Stability::Beta => "beta",
        }
    }
}

/// Validate a command type and its parameters, returning its stability.
///
/// Parameters are checked here *and* again in the app before they reach the
/// sim, so neither a buggy client nor a compromised one can drive an
/// out-of-range value into a running simulator.
pub fn validate(kind: &str, params: &serde_json::Value) -> Result<Stability, String> {
    let f = |key: &str| params.get(key).and_then(|v| v.as_f64());
    let b = |key: &str| params.get(key).and_then(|v| v.as_bool());
    let s = |key: &str| params.get(key).and_then(|v| v.as_str());

    match kind {
        "pause" => match s("state") {
            Some("on") | Some("off") | Some("toggle") => Ok(Stability::Stable),
            _ => Err("pause requires state of on, off or toggle".to_string()),
        },
        "set_heading_bug" => match f("heading") {
            Some(h) if (0.0..360.0).contains(&h) => Ok(Stability::Beta),
            _ => Err("heading must be 0-359".to_string()),
        },
        "ap_heading_mode" | "ap_nav_mode" => match b("enabled") {
            Some(_) => Ok(Stability::Beta),
            None => Err(format!("{} requires an `enabled` boolean", kind)),
        },
        "set_vertical_speed" => match f("fpm") {
            Some(v) if (-6000.0..=6000.0).contains(&v) => Ok(Stability::Beta),
            _ => Err("fpm must be between -6000 and 6000".to_string()),
        },
        "set_altitude" => match f("feet") {
            Some(v) if (0.0..=60000.0).contains(&v) => Ok(Stability::Beta),
            _ => Err("feet must be between 0 and 60000".to_string()),
        },
        other => Err(format!("unknown command `{}`", other)),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueCommandRequest {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedCommand {
    pub command_id: uuid::Uuid,
    pub status: &'static str,
    pub stability: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingCommand {
    pub id: uuid::Uuid,
    #[serde(rename = "type")]
    pub kind: String,
    pub params: serde_json::Value,
    pub stability: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandStatus {
    pub id: uuid::Uuid,
    #[serde(rename = "type")]
    pub kind: String,
    pub stability: String,
    /// `pending`, `delivered`, or the terminal ack result.
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WaitQuery {
    pub wait: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AckRequest {
    pub result: String,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Owner check plus lifecycle. Commanding a flight that is not running is a
/// conflict, not a not-found: the flight exists, it just cannot act.
async fn owned_active_flight(
    state: &AppState,
    flight_id: i64,
    user_id: i64,
) -> Result<(), AppError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT status FROM flights WHERE id = $1 AND user_id = $2")
            .bind(flight_id)
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;

    match row {
        None => Err(AppError::NotFound("Flight not found".to_string())),
        Some((status,)) if status == "ended" => Err(AppError::Conflict(
            "Flight is no longer active".to_string(),
        )),
        Some(_) => Ok(()),
    }
}

pub async fn issue_command_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
    Json(payload): Json<IssueCommandRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    owned_active_flight(&state, flight_id, user_id).await?;

    let stability = validate(&payload.kind, &payload.params).map_err(AppError::Unprocessable)?;

    let ttl = payload
        .ttl_secs
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(1, MAX_TTL_SECS);

    let recent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM flight_commands \
          WHERE flight_id = $1 AND issued_at > NOW() - INTERVAL '1 minute'",
    )
    .bind(flight_id)
    .fetch_one(&state.db)
    .await?;
    if recent >= RATE_LIMIT_PER_MIN {
        return Err(AppError::TooManyRequests(
            "Too many commands for this flight; slow down".to_string(),
        ));
    }

    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO flight_commands (id, flight_id, issued_by, type, params, stability, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, NOW() + make_interval(secs => $7))",
    )
    .bind(id)
    .bind(flight_id)
    .bind(user_id)
    .bind(&payload.kind)
    .bind(&payload.params)
    .bind(stability.as_str())
    .bind(ttl as f64)
    .execute(&state.db)
    .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(IssuedCommand {
            command_id: id,
            status: "pending",
            stability: stability.as_str(),
        }),
    ))
}

/// Atomically claim any pending commands for this flight.
///
/// The `delivered_at` stamp is set in the same statement that reads them, so two
/// app instances racing on one flight cannot both execute the same command.
async fn claim_pending(
    state: &AppState,
    flight_id: i64,
) -> Result<Vec<PendingCommand>, AppError> {
    let rows: Vec<(uuid::Uuid, String, serde_json::Value, String)> = sqlx::query_as(
        "UPDATE flight_commands SET delivered_at = NOW() \
          WHERE id IN ( \
              SELECT id FROM flight_commands \
               WHERE flight_id = $1 AND delivered_at IS NULL AND expires_at > NOW() \
               ORDER BY issued_at \
               FOR UPDATE SKIP LOCKED \
          ) \
          RETURNING id, type, params, stability",
    )
    .bind(flight_id)
    .fetch_all(&state.db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, kind, params, stability)| PendingCommand {
            id,
            kind,
            params,
            stability,
        })
        .collect())
}

/// Long-poll for pending commands. Returns immediately when work exists,
/// otherwise parks until `wait` elapses and answers `204`.
pub async fn poll_commands_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    Query(q): Query<WaitQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    owned_active_flight(&state, flight_id, user_id).await?;

    let wait = q.wait.unwrap_or(MAX_WAIT_SECS).min(MAX_WAIT_SECS);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);

    loop {
        let pending = claim_pending(&state, flight_id).await?;
        if !pending.is_empty() {
            return Ok((StatusCode::OK, Json(pending)).into_response());
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        tokio::time::sleep(std::time::Duration::from_millis(POLL_TICK_MS)).await;
    }
}

/// Mark expired commands so the issuing UI stops waiting on them.
async fn expire_stale(state: &AppState, flight_id: i64) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE flight_commands \
            SET acked_at = NOW(), result = 'expired', \
                detail = COALESCE(detail, 'Not collected before the deadline') \
          WHERE flight_id = $1 AND acked_at IS NULL AND expires_at < NOW()",
    )
    .bind(flight_id)
    .execute(&state.db)
    .await?;
    Ok(())
}

pub async fn command_status_handler(
    State(state): State<AppState>,
    Path((flight_id, command_id)): Path<(i64, uuid::Uuid)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    // Deliberately not owned_active_flight: a command's outcome stays readable
    // after the flight ends, which is when a late ack often lands.
    let owns: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM flights WHERE id = $1 AND user_id = $2)")
        .bind(flight_id)
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;
    if !owns {
        return Err(AppError::NotFound("Flight not found".to_string()));
    }

    expire_stale(&state, flight_id).await?;

    let row: Option<(uuid::Uuid, String, String, Option<String>, Option<String>, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as(
            "SELECT id, type, stability, result, detail, delivered_at \
               FROM flight_commands WHERE id = $1 AND flight_id = $2",
        )
        .bind(command_id)
        .bind(flight_id)
        .fetch_optional(&state.db)
        .await?;

    let (id, kind, stability, result, detail, delivered_at) =
        row.ok_or_else(|| AppError::NotFound("Command not found".to_string()))?;

    let status = result.unwrap_or_else(|| {
        if delivered_at.is_some() {
            "delivered".to_string()
        } else {
            "pending".to_string()
        }
    });

    Ok((
        StatusCode::OK,
        Json(CommandStatus {
            id,
            kind,
            stability,
            status,
            detail,
        }),
    ))
}

pub async fn ack_command_handler(
    State(state): State<AppState>,
    Path((flight_id, command_id)): Path<(i64, uuid::Uuid)>,
    headers: HeaderMap,
    Json(payload): Json<AckRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    if !matches!(
        payload.result.as_str(),
        "applied" | "unsupported" | "rejected" | "expired"
    ) {
        return Err(AppError::Unprocessable(format!(
            "Unknown ack result `{}`",
            payload.result
        )));
    }

    let updated = sqlx::query(
        "UPDATE flight_commands c \
            SET acked_at = NOW(), result = $1, detail = $2 \
           FROM flights f \
          WHERE c.id = $3 AND c.flight_id = $4 AND c.flight_id = f.id \
            AND f.user_id = $5 AND c.acked_at IS NULL",
    )
    .bind(&payload.result)
    .bind(&payload.detail)
    .bind(command_id)
    .bind(flight_id)
    .bind(user_id)
    .execute(&state.db)
    .await?;

    if updated.rows_affected() == 0 {
        // Already acked, or not ours. Either way there is nothing to change,
        // and a retrying client should not be told it failed.
        return Ok(StatusCode::NO_CONTENT);
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Which command types the connected sim can currently service, as last
/// reported by the app in `statistics.capabilities.commands`. The live page
/// renders only these, so an X-Plane pilot and an MSFS pilot see different
/// control strips and a pilot with remote control off sees none.
pub fn advertised_commands(statistics: &serde_json::Value) -> Vec<String> {
    statistics
        .get("capabilities")
        .and_then(|c| c.get("commands"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pause_is_the_only_stable_command() {
        assert_eq!(
            validate("pause", &json!({"state": "on"})).unwrap(),
            Stability::Stable
        );
        for (kind, params) in [
            ("set_heading_bug", json!({"heading": 270.0})),
            ("ap_heading_mode", json!({"enabled": true})),
            ("ap_nav_mode", json!({"enabled": false})),
            ("set_vertical_speed", json!({"fpm": -800.0})),
            ("set_altitude", json!({"feet": 12000.0})),
        ] {
            assert_eq!(
                validate(kind, &params).unwrap(),
                Stability::Beta,
                "{} should be beta",
                kind
            );
        }
    }

    #[test]
    fn pause_state_must_be_one_of_three_words() {
        assert!(validate("pause", &json!({"state": "toggle"})).is_ok());
        assert!(validate("pause", &json!({"state": "sideways"})).is_err());
        assert!(validate("pause", &json!({})).is_err());
    }

    #[test]
    fn out_of_range_parameters_are_refused() {
        assert!(validate("set_heading_bug", &json!({"heading": 360.0})).is_err());
        assert!(validate("set_heading_bug", &json!({"heading": -1.0})).is_err());
        assert!(validate("set_heading_bug", &json!({"heading": 359.0})).is_ok());
        assert!(validate("set_vertical_speed", &json!({"fpm": 7000.0})).is_err());
        assert!(validate("set_altitude", &json!({"feet": 60001.0})).is_err());
        assert!(validate("set_altitude", &json!({"feet": 0.0})).is_ok());
    }

    #[test]
    fn unknown_commands_are_refused_rather_than_forwarded() {
        // The sim executor must never see a type the service does not know.
        assert!(validate("format_c_drive", &json!({})).is_err());
    }

    #[test]
    fn booleans_are_required_not_coerced() {
        assert!(validate("ap_nav_mode", &json!({"enabled": "yes"})).is_err());
        assert!(validate("ap_nav_mode", &json!({"enabled": 1})).is_err());
    }

    #[test]
    fn capabilities_default_to_none_when_unreported() {
        assert!(advertised_commands(&json!({})).is_empty());
        assert!(advertised_commands(&json!({"capabilities": {}})).is_empty());
        assert_eq!(
            advertised_commands(&json!({"capabilities": {"commands": ["pause"]}})),
            vec!["pause".to_string()]
        );
    }
}
