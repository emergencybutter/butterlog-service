//! The FlySto export (docs/sim-native-telemetry.md §5.3): a flight as the Garmin G1000 CSV
//! that FlySto and CloudAhoy read, the format the app's `export_flight_to_csv` writes,
//! built from the flight's telemetry, one row a second.
//!
//! The telemetry has fewer engine and avionics channels than a G1000 logs: what it lacks
//! is left empty, never made up.

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};

use crate::error::AppError;
use crate::normalise::{Field, Sample};
use crate::AppState;

const AIRFRAME: &str = "#airframe_info, log_version=\"1.00\", airframe_name=\"{}\", unit_software_part_number=\"006-BXXX9-DE\", unit_software_version=\"15.24\", system_software_part_number=\"006-BXXXX-37\", system_id=\"25XXXX67\", mode=NORMAL, simulator_id=\"ButterLogV2\",";
const UNITS: &str = "#yyy-mm-dd, hh:mm:ss,   hh:mm,  ident,      degrees,      degrees, ft Baro,  inch,  ft msl, deg C,     kt,     kt,     fpm,    deg,    deg,      G,      G,   deg,   deg, volts, volts,  amps,   gals,   gals,      gph,   deg F,     psi,     Hg,    rpm,       %,   deg F,   deg F,   deg F,   deg F,   deg F,   deg F,   deg F,   deg F,   deg F,   deg F,   deg F,   deg F,   deg F,   deg F,  ft wgs,  kt, enum,    deg,    MHz,    MHz,     MHz,     MHz,    fsd,    fsd,     kt,   deg,     nm,    deg,    deg,   bool,  enum,   enum,   deg,   deg,   fpm,   enum,   mt,    mt,     mt,    mt,     mt";
const COLUMNS: &str = "Lcl Date, Lcl Time, UTCOfst, AtvWpt,     Latitude,    Longitude,    AltB, BaroA,  AltMSL,   OAT,    IAS, GndSpd,    VSpd,  Pitch,   Roll,  LatAc, NormAc,   HDG,   TRK, volt1, volt2,  amp1,  FQtyL,  FQtyR, E1 FFlow, E1 OilT, E1 OilP, E1 MAP, E1 RPM, E1 %Pwr, E1 CHT1, E1 CHT2, E1 CHT3, E1 CHT4, E1 CHT5, E1 CHT6, E1 EGT1, E1 EGT2, E1 EGT3, E1 EGT4, E1 EGT5, E1 EGT6, E1 TIT1, E1 TIT2,  AltGPS, TAS, HSIS,    CRS,   NAV1,   NAV2,    COM1,    COM2,   HCDI,   VCDI,WndSpd,WndDr, WptDst, WptBrg, MagVar, AfcsOn, RollM, PitchM, RollC, PichC, VSpdG, GPSfix,  HAL,   VAL, HPLwas, HPLfd, VPLwas";

/// A field with the given decimals; empty when the telemetry never had it.
fn num(s: &Sample, field: Field, decimals: usize) -> String {
    s.get(field).map_or_else(String::new, |v| format!("{v:.decimals$}"))
}

/// A true direction made magnetic, as a G1000 shows its heading and track.
fn magnetic(s: &Sample, true_field: Field) -> String {
    let v = s.get(true_field).map(|t| (t - s.get(Field::MagneticVariation).unwrap_or(0.0)).rem_euclid(360.0));
    v.map_or_else(String::new, |v| format!("{v:.1}"))
}

fn row(s: &Sample) -> Option<String> {
    let at: DateTime<Utc> = DateTime::from_timestamp_millis(s.wall_time_ms)?;
    s.get(Field::Latitude)?;
    let heading = match s.get(Field::HeadingMagnetic) {
        Some(h) => format!("{h:.1}"),
        None => magnetic(s, Field::HeadingTrue),
    };
    let autopilot = s.get(Field::Autopilot).map_or("", |v| if v > 0.5 { "1" } else { "0" });
    let blank = String::new();
    let cols: Vec<String> = vec![
        at.format("%Y-%m-%d").to_string(),
        at.format("%H:%M:%S").to_string(),
        "+00:00".into(),
        blank.clone(),
        num(s, Field::Latitude, 6),
        num(s, Field::Longitude, 6),
        num(s, Field::AltitudeIndicated, 1),
        num(s, Field::Baro, 2),
        num(s, Field::AltitudeMsl, 1),
        num(s, Field::Oat, 1),
        num(s, Field::Ias, 1),
        num(s, Field::GroundSpeed, 1),
        num(s, Field::VerticalSpeed, 1),
        num(s, Field::Pitch, 1),
        num(s, Field::Roll, 1),
        num(s, Field::GLateral, 3),
        num(s, Field::GNormal, 3),
        heading,
        magnetic(s, Field::Track),
        num(s, Field::Volts, 1),
        blank.clone(),
        num(s, Field::Amps, 1),
        num(s, Field::FuelLeft, 2),
        num(s, Field::FuelRight, 2),
        num(s, Field::FuelFlow, 1),
        num(s, Field::OilTemp, 1),
        num(s, Field::OilPressure, 1),
        num(s, Field::Manifold, 2),
        num(s, Field::Rpm, 1),
        num(s, Field::PctRpm, 1),
    ]
    .into_iter()
    // CHT1-6, EGT1-6: cylinder 1 of each, the only one recorded.
    .chain([num(s, Field::Cht, 1)])
    .chain(std::iter::repeat_n(blank.clone(), 5))
    .chain([num(s, Field::Egt, 1)])
    .chain(std::iter::repeat_n(blank.clone(), 5))
    .chain([
        blank.clone(), // TIT1
        blank.clone(), // TIT2
        num(s, Field::AltitudeMsl, 1), // AltGPS
        num(s, Field::Tas, 0),
        "GPS".into(),
        blank.clone(), // CRS
        num(s, Field::Nav1, 3),
        num(s, Field::Nav2, 3),
        num(s, Field::Com1, 3),
        num(s, Field::Com2, 3),
        blank.clone(), // HCDI
        blank.clone(), // VCDI
        num(s, Field::WindSpeed, 1),
        num(s, Field::WindDirection, 1),
        blank.clone(), // WptDst
        blank.clone(), // WptBrg
        num(s, Field::MagneticVariation, 2),
        autopilot.into(),
        "NONE".into(),
        "NONE".into(),
        blank.clone(), // RollC
        blank.clone(), // PichC
        blank.clone(), // VSpdG
        "3DDiff".into(),
    ])
    .chain(std::iter::repeat_n(blank, 5))
    .collect();
    Some(cols.join(", "))
}

/// Seconds without a sample, the telemetry having had no change to send, carry the last
/// one on, as the G1000 logs every second; a longer silence, the app restarting, stays a gap.
const CARRY_SECS: i64 = 10;

/// The CSV of a flight's samples, a row a second from the first with a position: the
/// first sample of each second, or the last before it.
pub fn flysto_csv(airframe: &str, samples: &[Sample]) -> String {
    let airframe = airframe.replace('"', "'");
    let mut out = format!("{}\n{UNITS}\n{COLUMNS}\n", AIRFRAME.replace("{}", &airframe));
    let mut push = |s: &Sample, second: i64| {
        let mut at = s.clone();
        at.wall_time_ms = second * 1_000;
        if let Some(line) = row(&at) {
            out.push_str(&line);
            out.push('\n');
        }
    };
    let mut last: Option<(&Sample, i64)> = None;
    for s in samples.iter().filter(|s| s.get(Field::Latitude).is_some()) {
        let this = s.wall_time_ms.div_euclid(1_000);
        if let Some((previous, second)) = last {
            if this <= second {
                continue;
            }
            if this - second <= CARRY_SECS {
                (second + 1..this).for_each(|t| push(previous, t));
            }
        }
        push(s, this);
        last = Some((s, this));
    }
    out
}

/// The flight's FlySto CSV, for its signed-in owner only (`viewer`): the telemetry is
/// theirs, at a rate the public pages don't show. Not found for anyone else.
pub async fn owned_csv(state: &AppState, flight_id: i64, viewer: Option<i64>) -> Result<String, AppError> {
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM flights WHERE id = $1")
        .bind(flight_id)
        .fetch_optional(&state.db)
        .await?;
    if owner.is_none() || owner != viewer {
        return Err(AppError::NotFound("Flight not found".to_string()));
    }
    let Some(bucket) = state.config.r2_telemetry_bucket.clone() else {
        return Err(AppError::NotFound("No telemetry for this flight".to_string()));
    };
    let segments = crate::sessions::R2Segments { r2: state.r2.clone(), bucket };
    let (title, samples) = crate::analysis::ingest::flight_samples(&state.db, &segments, flight_id)
        .await?
        .ok_or_else(|| AppError::NotFound("No telemetry for this flight".to_string()))?;
    tokio::task::spawn_blocking(move || flysto_csv(&title, &samples))
        .await
        .map_err(|e| AppError::Storage(e.to_string()))
}

/// `GET /content/flights/:id/export/flysto.csv`, the signed-in owner's only.
pub async fn flysto_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let viewer = crate::handlers::get_user_id_from_session(&state.db, &headers).await.ok();
    let csv = owned_csv(&state, flight_id, viewer).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"butterlog-{flight_id}.csv\"")),
            (header::CACHE_CONTROL, "private, no-store".to_string()),
        ],
        csv,
    )
        .into_response())
}

/// Whether a flight can be exported: an analysis found it in the telemetry.
pub async fn exportable(db: &sqlx::PgPool, flight_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM telemetry_flights WHERE flight_id = $1)")
        .bind(flight_id)
        .fetch_one(db)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_row_a_second_in_the_apps_columns() {
        let mut a = Sample::empty();
        a.wall_time_ms = 1_762_311_704_000; // 2025-11-05 03:01:44 UTC
        a.set(Field::Latitude, 46.15);
        a.set(Field::Longitude, -123.87);
        a.set(Field::HeadingTrue, 334.0);
        a.set(Field::MagneticVariation, 15.0);
        a.set(Field::Pitch, 3.0);
        a.set(Field::Autopilot, 1.0);
        let mut same_second = a.clone();
        same_second.wall_time_ms += 400;
        let mut next = a.clone();
        next.wall_time_ms += 1_000;
        let mut no_position = Sample::empty();
        no_position.wall_time_ms = a.wall_time_ms - 1_000;

        let csv = flysto_csv("Cessna \"172\"", &[no_position, a, same_second, next]);
        let lines: Vec<&str> = csv.lines().collect();
        assert!(lines[0].contains("airframe_name=\"Cessna '172'\""));
        assert_eq!(lines.len(), 5, "{csv}");
        let columns = lines[2].split(',').count();
        let row: Vec<&str> = lines[3].split(", ").collect();
        assert_eq!(row.len(), columns);
        assert_eq!(&row[..2], ["2025-11-05", "03:01:44"]);
        assert_eq!(row[4], "46.150000");
        assert_eq!(row[13], "3.0", "pitch, nose up");
        assert_eq!(row[17], "319.0", "magnetic heading");
        assert_eq!(row[59], "1", "autopilot");
        assert_eq!(row[9], "", "no OAT recorded");
        assert!(lines[4].starts_with("2025-11-05, 03:01:45"));
    }
}
