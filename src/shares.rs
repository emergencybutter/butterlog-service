//! Shares, from apps before 0.3.40. There are no shares any more: each flight's page shows
//! all of it (`flight_doc.rs`). An older app still uploads one when its pilot shares a
//! flight; it is kept so its link, the one the app shows, leads to the flight's page.
//!
//! A flight the service made has no id in the app, so its share arrives with none; such a
//! share takes the flight it covers most, by the time span of its track, once one exists.

use serde_json::Value;
use sqlx::PgPool;

use crate::error::AppError;
use crate::r2::R2Client;

/// How long after its upload a share without a flight is still matched to one.
const ADOPT_WITHIN: &str = "2 days";

/// The first and last epoch (seconds) of a share document's track: the first timestamp is
/// absolute, the rest deltas.
pub fn span(doc: &Value) -> Option<(i64, i64)> {
    let stamps = doc
        .get("transposedData")
        .or_else(|| doc.get("transposed_data"))?
        .get("timestamps")?
        .as_array()?;
    let first = stamps.first()?.as_i64()?;
    let last = stamps.iter().skip(1).filter_map(Value::as_i64).fold(first, |t, d| t + d);
    Some((first, last))
}

/// Reads the time span of shares uploaded without one, which only shares from before the
/// span was recorded lack, so they can be matched. Returns how many.
pub async fn fill_spans(db: &PgPool, r2: &R2Client, limit: i64) -> Result<usize, AppError> {
    let pending: Vec<(String, String)> = sqlx::query_as(&format!(
        "SELECT s.id, s.r2_key FROM flight_shares s JOIN users u ON u.id = s.user_id \
          WHERE s.remote_flight_id IS NULL AND s.starts_at IS NULL \
            AND u.service_flights_since IS NOT NULL \
            AND s.created_at > NOW() - INTERVAL '{ADOPT_WITHIN}' \
          LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(db)
    .await?;
    for (id, key) in &pending {
        let doc = r2
            .download_object(key)
            .await
            .ok()
            .and_then(|gz| crate::handlers::decompress_gzip_capped(gz.as_slice(), crate::handlers::MAX_SHARE_DECOMPRESSED).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        // One that can't be read is given an empty span at its upload, so it is not read
        // again each minute, and matches nothing.
        let (from, to) = doc.as_ref().and_then(span).map_or((None, None), |(a, b)| (Some(a), Some(b)));
        sqlx::query(
            "UPDATE flight_shares \
                SET starts_at = COALESCE(to_timestamp($2), created_at), \
                    ends_at = COALESCE(to_timestamp($3), created_at) \
              WHERE id = $1",
        )
        .bind(id)
        .bind(from.map(|t| t as f64))
        .bind(to.map(|t| t as f64))
        .execute(db)
        .await?;
    }
    Ok(pending.len())
}

/// Gives each recent share without a flight the user's flight the service owns that its
/// track overlaps most, if that flight has no share yet. Returns the flights given one,
/// whose shares are then to be written and Discord posts synced.
pub async fn adopt(db: &PgPool) -> Result<Vec<i64>, AppError> {
    let matched: Vec<(String, i64)> = sqlx::query_as(&format!(
        "SELECT DISTINCT ON (s.id) s.id, f.id \
           FROM flight_shares s \
           JOIN flights f ON f.user_id = s.user_id AND f.service_owned \
           CROSS JOIN LATERAL ( \
             SELECT to_timestamp(MIN(sample_epoch)) AS lo, to_timestamp(MAX(sample_epoch)) AS hi \
               FROM flight_track_points WHERE flight_id = f.id) t \
          WHERE s.remote_flight_id IS NULL AND s.starts_at IS NOT NULL \
            AND s.created_at > NOW() - INTERVAL '{ADOPT_WITHIN}' \
            AND f.created_at > s.starts_at - INTERVAL '1 day' \
            AND t.lo < s.ends_at AND t.hi > s.starts_at \
            AND NOT EXISTS (SELECT 1 FROM flight_shares o WHERE o.remote_flight_id = f.id) \
          ORDER BY s.id, LEAST(s.ends_at, t.hi) - GREATEST(s.starts_at, t.lo) DESC"
    ))
    .fetch_all(db)
    .await?;
    let mut adopted = Vec::new();
    for (share, flight) in matched {
        // Two shares overlapping one flight: the first takes it, the other stays as it is.
        if adopted.contains(&flight) {
            continue;
        }
        let mut tx = db.begin().await?;
        let taken = sqlx::query("UPDATE flight_shares SET remote_flight_id = $2 WHERE id = $1 AND remote_flight_id IS NULL")
            .bind(&share)
            .bind(flight)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if taken == 1 {
            sqlx::query("UPDATE flights SET share_id = COALESCE(share_id, $1) WHERE id = $2")
                .bind(&share)
                .bind(flight)
                .execute(&mut *tx)
                .await?;
            adopted.push(flight);
        }
        tx.commit().await?;
    }
    Ok(adopted)
}

/// Against a real Postgres, given by `TEST_DATABASE_URL` (skipped without it).
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_shares_span_adds_up_its_deltas() {
        let doc = json!({ "transposedData": { "timestamps": [1_000, 1, 1, 298] } });
        assert_eq!(span(&doc), Some((1_000, 1_300)));
        assert_eq!(span(&json!({ "summary": {} })), None);
    }

    async fn db() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let db = PgPool::connect(&url).await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        Some(db)
    }

    #[tokio::test]
    async fn a_share_without_a_flight_takes_the_service_flight_it_covers() {
        let Some(db) = db().await else { return };
        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (discord_id, username, service_flights_since) \
             VALUES ($1, 'test', NOW() - INTERVAL '1 day') RETURNING id",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .fetch_one(&db)
        .await
        .unwrap();
        let t0 = chrono::Utc::now().timestamp() - 3_600;
        // Two circuits, a minute apart; the share's track covers most of the second.
        let mut flights = Vec::new();
        for (from, to) in [(t0, t0 + 600), (t0 + 660, t0 + 1_260)] {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO flights (user_id, departure, statistics, created_at, service_owned, service_made, status) \
                 VALUES ($1, 'PHJH', $2, to_timestamp($3), TRUE, TRUE, 'ended') RETURNING id",
            )
            .bind(user)
            .bind(json!({ "start_time": "x", "simulator": "xplane" }))
            .bind(from as f64)
            .fetch_one(&db)
            .await
            .unwrap();
            for epoch in [from, (from + to) / 2, to] {
                sqlx::query("INSERT INTO flight_track_points (flight_id, sample_epoch, latitude, longitude) VALUES ($1, $2, 20.9, -156.6)")
                    .bind(id)
                    .bind(epoch)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            flights.push(id);
        }
        let share = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO flight_shares (id, user_id, r2_key, starts_at, ends_at) \
             VALUES ($1, $2, 'shares/x.json.gz', to_timestamp($3), to_timestamp($4))",
        )
        .bind(&share)
        .bind(user)
        .bind((t0 + 500) as f64)
        .bind((t0 + 1_300) as f64)
        .execute(&db)
        .await
        .unwrap();

        let adopted = adopt(&db).await.unwrap();
        assert!(adopted.contains(&flights[1]), "{adopted:?}");
        assert!(!adopted.contains(&flights[0]));
        let (linked, on_flight): (Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT s.remote_flight_id, f.share_id FROM flight_shares s, flights f WHERE s.id = $1 AND f.id = $2",
        )
        .bind(&share)
        .bind(flights[1])
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!((linked, on_flight.as_deref()), (Some(flights[1]), Some(share.as_str())));
        // Once taken, nothing more to adopt.
        assert!(!adopt(&db).await.unwrap().contains(&flights[1]));

    }
}
