-- Widen the per-sample telemetry the track carries.
--
-- Everything here is already captured by the client every tick; only a handful
-- of columns were ever uploaded. All are nullable: flights recorded before this
-- migration, and clients older than the release that fills them, simply leave
-- them empty, and the flight page renders a chart only for the series that have
-- data.
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS heading        REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS track          REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS ground_speed   REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS true_airspeed  REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS baro           REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS magvar         REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS g_load         REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS oat            REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS wind_speed     REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS wind_dir       REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS fuel_flow      REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS fuel_left      REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS fuel_right     REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS rpm            REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS pct_power      REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS manifold       REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS oil_temp       REAL;
ALTER TABLE flight_track_points ADD COLUMN IF NOT EXISTS oil_press      REAL;
