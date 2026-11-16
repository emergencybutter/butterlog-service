//! Deleting a flight: one its pilot asks to delete from its page, and those the anonymous
//! sweep deletes past retention (`anonymous::sweep`).

use crate::anonymous::ObjectStore;
use crate::error::AppError;
use sqlx::PgPool;

/// Deletes a flight's objects in R2, then the flight. The objects are its screenshots,
/// its archived track and an old share's upload. A failed object delete stops it before
/// any row goes, so it can be tried again rather than leave an orphan. Its points,
/// screenshots' rows, Discord message records and FlySto state go with the row.
///
/// With `remember`, a tombstone in `deleted_flights` keeps the analysis from making it
/// again (`analysis::ingest::link`).
pub async fn erase(db: &PgPool, store: &impl ObjectStore, flight_id: i64, remember: bool) -> Result<(), AppError> {
    let screenshots: Vec<(String, String, Option<uuid::Uuid>)> =
        sqlx::query_as("SELECT hash, url, session_id FROM screenshots WHERE flight_id = $1")
            .bind(flight_id)
            .fetch_all(db)
            .await?;
    let shares: Vec<String> = sqlx::query_scalar(
        "SELECT r2_key FROM flight_shares \
          WHERE remote_flight_id = $1 OR id = (SELECT share_id FROM flights WHERE id = $1)",
    )
    .bind(flight_id)
    .fetch_all(db)
    .await?;
    let track: Option<String> = sqlx::query_scalar("SELECT track_key FROM flights WHERE id = $1")
        .bind(flight_id)
        .fetch_optional(db)
        .await?
        .flatten();
    let keys = screenshots
        .iter()
        .map(|(hash, url, session)| crate::handlers::screenshot_key(Some(flight_id), *session, hash, url))
        .chain(shares)
        .chain(track);
    for key in keys {
        store
            .delete_public(&key)
            .await
            .map_err(|e| AppError::Storage(format!("flight {flight_id}: could not delete {key}: {e}")))?;
    }

    let mut tx = db.begin().await?;
    if remember {
        sqlx::query(
            "INSERT INTO deleted_flights (flight_id, user_id, statistics, service_owned, created_at) \
             SELECT id, user_id, statistics, service_owned, created_at FROM flights WHERE id = $1 \
             ON CONFLICT (flight_id) DO NOTHING",
        )
        .bind(flight_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "DELETE FROM flight_shares \
          WHERE remote_flight_id = $1 OR id = (SELECT share_id FROM flights WHERE id = $1)",
    )
    .bind(flight_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM flights WHERE id = $1")
        .bind(flight_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// What stands in the way of a pilot deleting a flight.
#[derive(Debug, PartialEq)]
pub enum Refusal {
    /// No such flight, or not theirs: the same answer, so as not to tell which.
    NotTheirs,
    /// Still flying. The analysis and the app are still writing to it.
    Flying,
}

/// Whether a pilot may delete a flight: their own, and over.
pub async fn may_delete(db: &PgPool, user_id: i64, flight_id: i64) -> Result<Result<(), Refusal>, AppError> {
    let row: Option<(i64, String)> = sqlx::query_as("SELECT user_id, status FROM flights WHERE id = $1")
        .bind(flight_id)
        .fetch_optional(db)
        .await?;
    Ok(match row {
        Some((owner, _)) if owner != user_id => Err(Refusal::NotTheirs),
        None => Err(Refusal::NotTheirs),
        Some((_, status)) if status == "active" => Err(Refusal::Flying),
        Some(_) => Ok(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Store {
        fail: bool,
        deleted: Mutex<Vec<String>>,
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

    async fn test_db() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let db = PgPool::connect(&url).await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        Some(db)
    }

    async fn pilot(db: &PgPool) -> i64 {
        sqlx::query_scalar("INSERT INTO users (discord_id, username) VALUES ($1, 'pilot') RETURNING id")
            .bind(uuid::Uuid::new_v4().to_string())
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn flight(db: &PgPool, user: i64, status: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO flights (user_id, departure, arrival, statistics, status) \
             VALUES ($1, 'KAST', 'KSEA', '{\"takeoff_time\": \"2026-11-15 12:00:00.000\"}', $2) RETURNING id",
        )
        .bind(user)
        .bind(status)
        .fetch_one(db)
        .await
        .unwrap()
    }

    async fn exists(db: &PgPool, id: i64) -> bool {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flights WHERE id = $1)")
            .bind(id)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn only_its_pilot_may_delete_a_flight_and_only_once_it_is_over() {
        let Some(db) = test_db().await else { return };
        let (pilot_id, other) = (pilot(&db).await, pilot(&db).await);
        let (ended, flying) = (flight(&db, pilot_id, "ended").await, flight(&db, pilot_id, "active").await);
        assert_eq!(may_delete(&db, pilot_id, ended).await.unwrap(), Ok(()));
        assert_eq!(may_delete(&db, other, ended).await.unwrap(), Err(Refusal::NotTheirs));
        assert_eq!(may_delete(&db, pilot_id, flying).await.unwrap(), Err(Refusal::Flying));
        assert_eq!(may_delete(&db, pilot_id, i64::MAX).await.unwrap(), Err(Refusal::NotTheirs));
    }

    #[tokio::test]
    async fn a_flight_goes_with_its_objects_and_leaves_a_tombstone() {
        let Some(db) = test_db().await else { return };
        let user = pilot(&db).await;
        let id = flight(&db, user, "ended").await;
        sqlx::query("UPDATE flights SET track_key = $2 WHERE id = $1")
            .bind(id)
            .bind(format!("tracks/{id}.json.gz"))
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO screenshots (flight_id, hash, url) VALUES ($1, 'abc', 'https://x/abc.webp')")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO flight_discord_messages (flight_id, discord_message_id, discord_channel_id) VALUES ($1, '1', '2')")
            .bind(id)
            .execute(&db)
            .await
            .unwrap();

        // An object that cannot be deleted keeps the flight, to be tried again.
        assert!(erase(&db, &Store { fail: true, ..Default::default() }, id, true).await.is_err());
        assert!(exists(&db, id).await);

        let store = Store::default();
        erase(&db, &store, id, true).await.unwrap();
        assert!(!exists(&db, id).await);
        let mut deleted = store.deleted.lock().unwrap().clone();
        deleted.sort();
        assert_eq!(deleted, vec![format!("screenshots/{id}/abc.webp"), format!("tracks/{id}.json.gz")]);
        let left: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM screenshots WHERE flight_id = $1), \
                    (SELECT COUNT(*) FROM flight_discord_messages WHERE flight_id = $1)",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(left, (0, 0));
        let tombstone: (i64, serde_json::Value) =
            sqlx::query_as("SELECT user_id, statistics FROM deleted_flights WHERE flight_id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(tombstone.0, user);
        assert_eq!(tombstone.1["takeoff_time"], "2026-11-15 12:00:00.000");
    }

    #[tokio::test]
    async fn the_sweep_leaves_no_tombstone() {
        let Some(db) = test_db().await else { return };
        let id = flight(&db, pilot(&db).await, "ended").await;
        erase(&db, &Store::default(), id, false).await.unwrap();
        let kept: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM deleted_flights WHERE flight_id = $1)")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert!(!kept);
    }
}
