# Sim-Native Telemetry — design

**Status:** proposed. Nothing here is built yet. Supersedes the track channel of
`docs/live-flight-protocol.md` once implemented; that document stays the record of the
current system.
**Goal:** the server becomes the place a flight lives. The client stops displaying flights
and instead streams nearly everything the simulator exposes, in the simulator's own terms,
over a protocol compact enough for the network and for long-term storage. The web gets
richer visualisation, including watching a flight as it happens. Normalisation and analysis
move server-side, so every past flight can be re-analysed when the analyser improves.
**Scope:** the app requires an account and has no offline mode; a user who hasn't signed up
gets an anonymous one (§6.2). Every product of a flight — web views, analysis and the
FlySto/CloudAhoy export — is built by the service.

## 1. Where we are today

| Concern | Today | Problem |
|---|---|---|
| Capture | ~70 MSFS SimVars / X-Plane datarefs at 1 Hz into a per-flight SQLite file | 1 Hz is too coarse for touchdown analysis |
| Normalisation | In each client monitor (`sim_monitor/msfs.rs`, `xplane.rs`) into `FlightMetrics` | Sim quirks leak anyway (`live.rs` re-converts MSFS attitude; `"G FORCE"` stands in for `xp_prop_rpm`) |
| Analysis | `flight_analyzer.rs` on the client, results frozen into the summary | A better analyser never reaches old flights |
| Upload | `live_track.rs` thins with `Downsampler`, posts transposed f32 JSON every 20 s | Lossy, verbose, and the only copy of the full data stays on one PC |
| Storage | `flight_track_points`, one Postgres row per sample | Won't scale to 20+ Hz |
| X-Plane | Web API WebSocket (`dataref_subscribe_values`), arrival-time stamped; UDP only for traffic → plugin | No sample timestamps, fixed rate, no replay/pause awareness |

## 2. Shape of the solution

```
 MSFS ── SimConnect (CHANGED|TAGGED, SIM_FRAME) ──┐
                                                  ├─► app: frame → chunk → zstd → spool ──► service ingest
 X-Plane 12 ── butterlog_xp_plugin ── TCP ────────┘                                           │
   (fallback: Web API WebSocket)                                                              ▼
                                               raw chunks (source of truth) ── normalisers ── derived products
                                                                                  │              │
                                                                         live fan-out (SSE/WS)  web UX
```

Principles:

1. **The wire is sim-native.** A channel is a SimVar (`PLANE BANK DEGREES`) or a dataref
   path (`sim/flightmodel/position/phi`) with its native unit. The client does not unify
   sims.
2. **Raw is the source of truth.** The service stores what it received, unmodified.
   Everything the web shows is derived and can be rebuilt.
3. **Quantisation is the only lossy step.** No time-thinning on the client.
4. **One pipeline below the sampler.** MSFS and X-Plane differ only in how a frame is
   sampled; chunking, compression, spooling and upload are shared.
5. **The client only captures and uploads.** Decoding, normalisation, segmentation, analysis
   and export exist in one place, the service. No logic is shared across the two repos
   except the wire format. The one exception is the small traffic model peers exchange for
   traffic injection (§6.1), which is a live-display concern, not analysis.

## 3. Wire format

### 3.1 Session and schema

A *session* is one continuous connection to one sim with one aircraft loaded. It opens with
a header and a channel table:

```
SessionHeader {
  protocol_version, sim ("msfs2020" | "msfs2024" | "xplane12"), sim_version,
  capture ("simconnect" | "xp_plugin" | "xp_webapi"), client_version,
  aircraft identity (title, ICAO, tail, livery, engine type/count — as the sim reports them)
}
Channel {
  id: u16,
  name: string,            -- SimVar or dataref path, plus index for arrays
  unit: string,            -- sim-native
  kind: f64 | i64 | bool | enum | string,
  quantum: f64,            -- quantisation step, e.g. 1e-7 for lat/lon, 0.1 for feet
  rate_class: frame | fast | slow | on_change
}
```

The service stores the table keyed by its hash, so a client can add channels without a
service deploy. Unknown channels are stored and browsable; normalisers ignore them until
taught.

### 3.2 Ticks

Each tick carries a common header, independent of sim:

- `sim_time` — sim clock (X-Plane `sim/time/total_running_time_sec`; MSFS `SIMULATION TIME`)
- `wall_time` — UTC, as a delta from the chunk's base
- `state` flags — paused, replay, slew, sim rate ≠ 1

Then a presence bitmap over the channel table, and values only for present channels.

**Change-only is the general rule.** A channel is present in a tick iff its *quantised*
value differs from the last one sent. Exact float equality filters almost nothing (position
and attitude move in the 6th decimal every frame); quantise-then-compare is the deadband that
makes change-only pay off. It runs at the sampler, where the data is.

MSFS gets the same semantics natively from SimConnect `SIMCONNECT_DATA_REQUEST_FLAG_CHANGED |
SIMCONNECT_DATA_REQUEST_FLAG_TAGGED` at `SIMCONNECT_PERIOD_SIM_FRAME`; quantisation is then
applied by the app before encoding.

### 3.3 Sampling rates

Per channel, varying with flight phase:

| Phase | Position, attitude, VS, G, AGL | Engines, fuel, electrics | Radios, AP, lights, gear |
|---|---|---|---|
| Below ~1,000 ft AGL or on runway | every frame, capped at 30 Hz | 1 Hz | on change |
| Climb / descent / manoeuvring | 5 Hz | 1 Hz | on change |
| Steady cruise | 1 Hz | 0.2 Hz | on change |

The phase test runs in the sampler (cheap: AGL, on-ground, VS, bank). The exact thresholds
are tunables, not protocol.

### 3.4 Chunks

Ticks are grouped into chunks of 2–5 s (the live latency budget):

```
Chunk {
  session_id, seq: u32, base_sim_time, base_wall_time,
  keyframe: bool,
  columns: per channel → (presence, values)
}
```

The byte layout is specified in `butterlog-wire` (`wire/src/chunk.rs`).

- Values are quantised to integers, delta (position/attitude: delta-of-delta) encoded,
  zigzag-varint packed, column by column.
- **Presence** per column is a bitmap over the chunk's ticks or a list of tick-index gaps,
  whichever is smaller: a bitmap for a channel present at most ticks, gaps for one that
  rarely changes. At 1 Hz, gap lists alone cost about as much as the values.
- **Chunks continue the previous one:** a column's first value is a delta against the
  channel's last value in the session. Restarting from absolute values in every chunk cost
  more than the deltas at low rates. The service therefore decodes a session's chunks in
  order and holds back any that arrive early; a chunk whose predecessor is missing is
  refused, never decoded wrong.
- The whole chunk is zstd-compressed (level 3: levels 1–9 measured within a few percent).
- **Keyframes:** every ~60 s a chunk starts from absolute values and carries the value of
  *every* channel, so any range is decodable with bounded look-back, and a viewer joining
  mid-flight needs at most one keyframe. 120 s saves only ~4%.
- **Events:** discrete happenings the sim reports (sim start and stop, pause, crash and
  crash reset, flight or aircraft loaded with the file it names, position changed by hand,
  replay entered and left) travel in the chunk after the columns, each at the tick it
  arrived with: a kind code and a detail string. An event makes a tick due even when no
  value changed. Kind codes are fixed; a code the reader doesn't know decodes as "other",
  so a newer client doesn't fail an older service. Events that arrive between sessions
  (the flight loaded before the aircraft's title) go with the next session's first tick.
  The service copies them to `telemetry_events` as chunks decode, for analysis to query
  without reading segments.

**Size, measured at 1 Hz** (rollout phase 1: 428 local logs, 384 flight-hours, ~75
normalised channels, 5 s chunks): **~140 kB per flight-hour** as sent (median; p90 180 kB),
~70 kB per flight-hour compacted per session at zstd -19, against ~1.6 MB per flight-hour
for the SQLite logs today. 2 s chunks cost ~40% more than 5 s. Scaling every quantum 10×
coarser or finer moves size by ~30–40%; no single channel dominates.

**Size, measured at high rate** (rollout phase 1: an X-Plane 12 traffic pattern captured
every frame by the plugin, 128 dataref values, 8.2 min, replayed through the §3.3 sampler
by `measure_capture`): the whole pattern stays below 1,000 ft AGL, so it is the worst case,
sampled every frame capped at 30 Hz. **~600 kB per hour** as sent (83 kB for the pattern),
68 kB compacted; ~460 kB per hour with the low phase at 10 Hz. A real flight spends most of
its time in climb and cruise at 5 Hz and 1 Hz, so a 2-hour flight lands well inside the
0.5–1 MB estimate.

MSFS 2024, the same pattern through `msfs_capture` (every SimVar every sim frame, 65
values, 7.2 min, ~60 fps): **~900 kB per hour** as sent at 30 Hz near the ground (107 kB
for the pattern), 96 kB compacted; ~620 kB per hour at 10 Hz. Heavier than X-Plane with
half the channels: position and attitude change on 85% of frames at a higher frame rate.
Still inside the estimate; the low-phase rate is the main lever if it needs to shrink.

Seen in the MSFS capture, for the sampler and the normalisers:

- **Loading a flight** moves the aircraft through (0, 0) and another airport and resets
  `SIMULATION TIME` twice (jumps of −3 s and −9 s) in the first 17 s. The sampler must
  treat sim time going backwards as a discontinuity, and the flight starts after loading.
- **`SIM ON GROUND` flickers** at liftoff: ten transitions within a second. Takeoff and
  touchdown detection needs debouncing on the service.
- Pause arrives as the `Pause` system event, not a SimVar.

### 3.5 Transport

- App → service: WebSocket, or HTTP POST per chunk; each chunk acked by `seq`.
- The app appends every chunk to a local **spool** before sending and deletes it once
  acked. The spool is a crash-safe send buffer, not an archive and not an offline mode: it
  covers a dropped connection, an app crash or a service restart mid-flight, which all
  reduce to "send the spool later". It is bounded (a few hundred MB, tunable); the app warns
  when it is growing and, once full, stops recording rather than evicting unsent chunks.
- No token, no recording: a session only starts with a valid token. On first run the app
  gets one by creating an anonymous account (§6.2).
- Idempotent by `(session_id, seq)`, as `flight_track_points` is today by sample epoch.
- Authenticated with the existing Bearer API token.

## 4. X-Plane: the plugin is the primary capture path

X-Plane is a first-class citizen. Supported: **X-Plane 12 only**, on **Windows, macOS and
Linux**.

### 4.1 Options considered

| | Legacy UDP (RREF) | Web API WebSocket (today) | Plugin |
|---|---|---|---|
| Rate | Per dataref, set by us | Fixed by X-Plane: measured 8.1 Hz, 100 ms apart (p99 119 ms) | Every frame or any divisor: measured ~50 fps, frame p50 20 ms, p99 30 ms, none dropped |
| Sample timestamp | None — arrival time | None — arrival time; p99 2.2 s late; nothing at all while paused | Sim time, exact per frame |
| Change filtering | None, resends all every tick | Exact-equality only: unchanged values never resent | Ours: quantise-then-compare |
| Types | float32 only, one array element per subscription | All | All, plus the full XPLM API |
| Sim events (plane loaded, crash, replay, pause) | No | Poll datarefs | Plugin messages, directly |
| Delivery | Lossy | Reliable | Reliable (we choose) |
| Install | None | None | Plugin (already needed for traffic) |

**Decision:** drop UDP RREF entirely. Capture through `butterlog_xp_plugin`. Keep the Web
API as a fallback for users without the plugin, flagged `capture=xp_webapi` so the service
knows timing is arrival-based and can lower the landing scorecard's confidence. The fallback
is cheap because channels are named by dataref path in both cases — one normaliser serves
both. It can be retired later.

Deciding reasons:

- **Timing.** Touchdown analysis at 20+ Hz and synchronised web replay need sample-accurate
  timestamps. Only the plugin has them.
- **Sim state.** The plugin can tell real flight from replay, pause and time acceleration. A
  replayed landing must never be recorded as a second landing.
- **Phase-dependent rates and deadband filtering** require running in the flight loop.
- **One X-Plane component** for capture, commands (`sim_commands`), traffic injection and
  dataref introspection (XPLM 4 `XPLMCountDataRefs` / `XPLMGetDataRefsByIndex`).

Measured in rollout phase 1, on the same X-Plane 12 traffic pattern with the plugin's
capture spike and `xp_webapi_probe` running side by side: the plugin saw about six times as
many samples as the Web API. Notes for the real sampler:

- `sim/time/total_running_time_sec` is a float dataref; `XPLMGetDatad` reads it as 0. Read
  each dataref by its reported type.
- The flight-loop counter advances by 2 per callback, so it can't count dropped frames.
- Replay is visible and must be flagged: in the pattern, entering replay
  (`replay_mode` 9) moved the aircraft back to its start position, and the replayed takeoff
  showed on-ground going 1 → 0 again. Without the `REPLAY` flag the service would see a
  second takeoff. Pause (`sim/time/paused`, `sim_speed` 0) is visible the same way.
- `sim/cockpit2/engine/indicators/MP_in_hg` and `sim/flightmodel/engine/ENGHP` aren't found
  under those names in X-Plane 12; the app's Web API subscription has the same gap.

### 4.2 Plugin architecture

```
flight loop (sim thread): read the subscribed datarefs → fixed-size frame → bounded channel
plugin bg thread:         drain channel → length-prefixed frames → localhost TCP → app
app:                      frame → shared sampler (phase, quantise, compare) → chunks → spool
```

- **The plugin only reads.** It reads every frame the datarefs the app subscribed to and
  sends them raw, stamped with the sim clock. It does not quantise, sample, chunk, compress,
  spool or talk to the service. The app remains the single uploader for both sims (token,
  spool, retries).

  *Built (rollout phase 4):* sampling moved from the flight loop to the app. The first
  plan quantised and filtered in the plugin; instead the app runs the same `butterlog-wire`
  sampler it uses for SimConnect and the Web API, so rates and quanta live in one place, the
  plugin doesn't change when they're tuned, and the plugin (built on public CI) doesn't
  depend on the private `butterlog-wire`. Raw frames over localhost cost about 50 kB/s,
  which doesn't matter. The app sends its dataref list in the subscription, so the plugin
  holds no list either. Protocol: `butterlog_xp_plugin` `src/link.rs`, port 49021; the app
  side is `butterlog-app` `telemetry/xp_plugin.rs`, with the same golden messages tested on
  both sides.
- **TCP, not UDP, to the app.** A dropped datagram in a recording that is kept for ever is a
  silent hole. UDP stays for traffic injection, where only the newest value matters.
- **Safety in the sim process.** No allocation and no I/O on the flight loop. Every
  `extern "C"` entry point is wrapped in `catch_unwind` — a panic there takes down X-Plane.
  If the ring is full (app gone), ticks are dropped and counted, never blocked on.
- **Handshake.** On connect the plugin sends its protocol version, its own version and
  X-Plane's. The app rejects an incompatible plugin and falls back to the Web API instead of
  recording garbage. A session has one capture: if the link drops mid-session, the session
  ends and a new one goes on through the Web API.
- **Events.** The plugin sends `XPLM_MSG_PLANE_LOADED` (user aircraft, with its `.acf` path)
  and `XPLM_MSG_AIRPORT_LOADED` in order with the frames. Pause, replay and crash are read
  from their datarefs, as with the Web API.
- **Distribution.** The app installs and updates the plugin itself, locating X-Plane via
  `x-plane_install_12.txt` in the OS's per-user preferences/app-data directory. CI builds
  `win.xpl`, `mac.xpl` (universal x86_64 + arm64, signed and notarised) and `lin.xpl`. The
  hand-built `butterlog_xp_plugin-0.2.x.zip` releases go away.
- **Commands.** Web-issued commands (pause, autopilot) move from the Web API to the plugin
  over the same TCP link.

## 5. Service

### 5.1 Ingest and storage

- While a session is live, chunks are appended to local disk (or `bytea`) and indexed in
  Postgres by `(session_id, seq)`.
- At session end, chunks are compacted into one segment object in R2.
- Postgres keeps metadata only: sessions, schemas by hash, flights (segments of sessions),
  events, derived summaries. `flight_track_points` is retired once the web reads derived
  products.

### 5.2 Normalisers and analysis

- Per-sim normalisers (the logic now in `sim_monitor/msfs.rs` and `xplane.rs`) decode raw
  chunks into a canonical model: units, sign and angle conventions, `capture` quality.
- Flight segmentation, landing analysis (`flight_analyzer.rs`), airport/runway resolution and
  ICAO/airline identification move server-side. The airports CSV dependency on the client
  disappears.
- Derived products — map track, chart series, events, landing scorecard, stats, exports —
  are cached per `(flight, analyser_version)` and rebuilt on version bump.
- **Built (rollout phase 3):** `src/normalise.rs` maps both sims to one sample stream;
  `src/analysis/` is the app's analyser on it, with sessions split into flights at
  teleports and from the first move, ground contact held 0.5 s, and paused, replay and slew
  samples skipped. Each ended session is analysed before compaction into
  `telemetry_flights`, once per analyser version, beside the old-path flight with the same
  takeoff and a `{service, app}` diff of endpoints, times and landing figures. The pattern
  fixtures hold it to the app's own summaries of the same flights.

### 5.3 Export (FlySto, CloudAhoy)

- The Garmin-style CSV export that `export_flight_to_csv` (app,
  `flight_log_manager.rs`) writes today becomes a derived product:
  `GET /flights/{id}/export/flysto.csv`, with an Export button on the flight page.
- Built from the canonical model, so the export and the web never disagree about what a
  field means, and old flights get better exports when the normalisers improve.
- Garmin logs are 1 Hz; the exporter resamples the high-rate data down to that, so nothing
  is lost against today's 1 Hz export. Migrated `capture=legacy_1hz` flights export too.
- The CSV writer ports almost unchanged (both sides are Rust); it reads the canonical model
  instead of the per-flight SQLite file.
- Later, if FlySto offers an upload API, the service can push flights directly.

### 5.4 Live

- Each active session keeps its last N minutes decoded in memory.
- Viewers subscribe over SSE or WebSocket: catch-up snapshot (from the last keyframe), then
  normalised deltas as chunks arrive.
- The VPS is a single instance: in-process broadcast suffices. If the Cloud Run standby ever
  serves traffic, fan-out goes through Postgres `LISTEN/NOTIFY`.
- Visibility per flight: private, link-only, public; optional broadcast delay.

### 5.5 Screenshots

Today the app attributes a screenshot to the flight it is recording and looks up its position
in the per-flight SQLite file (`find_closest_metrics` in `screenshot_manager.rs`), then posts
it to `POST /flights/:id/screenshots`. After the change the client knows neither: flights are
segmented on the service, and the local metrics file is gone.

- The app records every screenshot it sees during a session in its local `screenshots.db`,
  whether or not it is uploaded: file path, `session_id`, and wall time (UTC, the file's
  creation time). The table exists today; `flight_id`, latitude and longitude give way to
  `session_id` and wall time. Nothing about a screenshot reaches the service until it is
  uploaded, so a screenshot the user keeps private stays private, path and all.
- An upload, automatic or chosen later, is `POST /sessions/{id}/screenshots` with the image
  and the wall time from `screenshots.db`. Wall time, not sim time: ticks carry it, the
  screenshot file has it, and the history importer can match old screenshots to
  `legacy_1hz` sessions by file time.
- With auto-upload off, the app lists the screenshots it has recorded but not uploaded, to
  upload or forget. It is the only screenshot view left in the app (§6).
- If the session no longer exists on the service (an anonymous session swept after 14 days,
  §6.2), the upload is refused and the app says so.
- The service places it: the flight is the segment containing that time, and the position
  comes from the recorded track. Like any derived product, the placement is redone when
  segmentation changes, so a re-analysed session never leaves a screenshot on the wrong
  flight.
- A screenshot taken outside any session (sim menus, no aircraft loaded) is not uploaded.
- Anonymous retention (§6.2) covers screenshots with their flight.
- The auto-upload setting and its filename filter stay on the client, unchanged.
- `POST /flights/:id/screenshots` keeps serving old clients until they are gone; those
  screenshots keep their flight link.

## 6. Client after the change

| Keeps | Loses |
|---|---|
| Sim adapters (SimConnect, X-Plane plugin link, Web API fallback) | Flight list, flight details, charts, map |
| Encoder, spool, uploader | Screenshot gallery and position lookup (§5.5) |
| `screenshots.db`, screenshot upload, list of screenshots not yet uploaded (§5.5) | |
| SimVar / dataref introspection, debugging | FlySto export (moves to the service, §5.3) |
| Traffic injection (`multiplayer.rs`), sim commands | Aircraft stats, `flight_analyzer.rs` |
| `TrafficState` conversion per sim (§6.1) | Airport, runway, ICAO, airline indexes |
| Local model matching (`aircraft-characteristics.csv`) | ICAO type / airline resolution (moves to the service) |
| | Share and webhook formatting |
| | Offline / local-only recording |

The app requires an account, signed in or anonymous (§6.2). With a token, everything streams
to the service; without one, it records nothing.

The app carries no decoder, normaliser or segmentation: it only needs the encoder. The wire
format (schema, chunk encoding) lives in a small crate, **`butterlog-wire`**, used by the
app to encode and by the service to decode. Normalisers and analysis are service-only, so a
fix to them ships with a service deploy, never an app release, and the two never need to
agree on anything beyond `protocol_version`.

Trade-offs accepted:

- Users who only wanted a local logger plus FlySto now have their telemetry stored on the
  service, if only for 14 days anonymously.
- A long service outage eventually fills the bounded spool and stops recording.
- Export adds a little service load; it is cheap and cached.

### 6.1 Traffic injection

Peers exchange state directly over UDP (`multiplayer.rs`); the service is only the
matchmaker. An MSFS pilot sees an X-Plane pilot, so peers need a sim-neutral model. Today
that is the full `FlightMetrics`; it becomes a small **`TrafficState`** in `butterlog-wire`:

- Position, MSL altitude, heading, pitch, bank, ground speed, on-ground, and for animation
  gear, flaps, lights and engines running. Versioned with `protocol_version`.
- Each sim adapter converts its own values into `TrafficState` to send, and back out to
  inject. That is a few lines per sim, not the analysis normalisers.
- It is deliberately separate from the service normalisers (calibration, capture quality,
  segmentation). If the two drift, a peer may look slightly different in your sim, but no
  recorded data changes — an acceptable cost for not sharing a crate.
- Traffic stays peer-to-peer, not relayed through the service: relaying adds latency and
  load, and the direct path with STUN/NAT traversal is deliberate
  (`docs/multiplayer-same-nat.md`, `docs/multiplayer-proximity-peers.md` in the app).

**Identity comes from the service.** The sender no longer resolves its own ICAO type and
airline. The service already does that from the session header (§5.2); it returns the
result in the session-open ack — ICAO type, airline ICAO and airline name — and the app
forwards it to peers. The receiver matches local models with `aircraft-characteristics.csv`
and local livery titles with the airline name it was sent, so the client drops the airport,
runway, ICAO and airline tables.

### 6.2 Anonymous accounts

A user who hasn't signed up still streams. Their flights are reachable through a secret link
shown in the app and kept 14 days; signing up keeps them for good.

**Identity.**

- An anonymous user is an ordinary `users` row with no Discord identity: `discord_id`
  becomes nullable (`UNIQUE` is unaffected by NULLs), plus `anonymous_since TIMESTAMPTZ
  NULL`. Display name is generated (`Pilot 4F2A`).
- On first run the app calls `POST /api/v0/anonymous` (unauthenticated) and receives an API
  token and a link secret. From then on it uses the same Bearer path as a signed-up user, so
  ingest, analysis, export and live need no anonymous special case.
- **The token writes, the link only reads.** They are separate secrets. The link secret is
  122 random bits (a v4 UUID), stored hashed like `api_tokens`; the app keeps the plaintext
  so it can show the link any time, and can rotate it, which invalidates the old one.

**The link.**

- `/content/l/<secret>` (under `/content`, which the proxy already sends to the service)
  sets a `link` cookie and opens the user's flights, so the pages and live updates behind
  it need no secret in their URLs: the flight list, each flight page, replay, export and
  live follow, read-only. The list says how long flights are kept and to sign in to keep
  them; a per-flight "Kept until <date>" is still to come.
- `noindex`, `Referrer-Policy: no-referrer`, and excluded from the public map, the
  ended-flights feed, aircraft stats and any Discord posting.
- Share links (`flight_shares`) still work for individual flights, and expire with them.

**Retention.**

- A flight of an anonymous user expires 14 days after it ends (`ended_at`, else its last
  update); a session, 14 days after it ends or goes silent. Expiry is computed, not stored,
  so a claim needs to change nothing but the owner. A flight takes its screenshots and
  shares with it; a session, its chunks and R2 segment; derived products will follow theirs.
- Deletion is a sweeper next to `track::spawn_reaper`, not an R2 lifecycle rule, so a claim
  can cancel it without moving objects.
- An anonymous user with no flights and a token unused for 14 days is deleted.
- The app keeps no copy (§3.5), so a swept flight is gone for good, even after sign-up.

**Sign-up (claim).**

- The Discord login flow is unchanged. Once the app holds the new token it calls `POST
  /api/v0/account/claim` with the new token as Bearer and the anonymous token in the body.
- The anonymous user's flights (with their screenshots), sessions and shares are reassigned
  to the Discord account and the anonymous row is deleted, taking its token and link with
  it. The Discord login has always created or updated its user by then, so a new and an
  existing account are the same move.

**Consent and abuse.**

- Uploading without signing in is a change from "nothing is sent unless logged in", so it is
  never silent. First run says flights are uploaded to a private link and kept 14 days, and
  offers *Continue* or *Sign in with Discord*. Existing logged-out users see the same screen
  once, before anything is sent.
- An anonymous user can delete their flights from the app or the link page at any time.
- `POST /api/v0/anonymous` is rate-limited per IP. Anonymous users get an ingest quota (bytes
  per day) and a cap on concurrent live sessions. Storage cost is bounded by the 14 days.

## 7. Web UX this enables

- Replay with a scrubber driving map, 3D cockpit view, attitude and charts in sync.
- Landing analysis from 20+ Hz data around touchdown.
- Flight comparison (same approach flown twice).
- Engine monitor view.
- Raw channel explorer for any recorded SimVar or dataref.
- Live follow, with the visibility settings above.

## 8. Plan

1. **Measure.** Write the encoder in `butterlog-wire`; replay existing SQLite logs through
   it to measure sizes and tune quanta. In parallel, a sampler spike in the plugin logging
   frame timing and payload per flight, side by side with Web API message rate and jitter
   (settles the "verify" items in §4.1).
2. **Ingest.** Service accepts sessions and chunks and stores raw, alongside the current
   upload path.
3. **Plugin capture.** Flight-loop sampler, TCP link, handshake, CI builds for all three
   platforms, app-managed install.
4. **Normalise and analyse server-side**; web views read derived products. The session-open
   ack returns the resolved identity, and peers switch to `TrafficState` (§6.1); both land
   before the client's indexes are removed.
5. **Export on the service** (§5.3): FlySto CSV endpoint and web button. Must ship before the
   client export is removed, so users never lose it in between.
6. **Live** over SSE/WS from the hot buffer.
7. **Anonymous accounts** (§6.2): must ship before the strip, when the secret link becomes
   the only logbook of a user who hasn't signed up.
8. **Migrate history:** bulk importer converting local `.db` logs into sessions and
   uploading them (flagged `capture=legacy_1hz`). For users who never logged in this is the
   only way to keep their flights, so it ships before the client loses its flight list, and
   the app prompts them to sign up and import — imported into an anonymous account, flights
   older than 14 days would be swept at once.
9. **Strip the client:** flight UI, analyser, indexes, FlySto export, local-only recording.

## 9. Open questions

None at the moment.

Settled:

- The client deletes spooled chunks once acked; the service is the only archive, including
  for anonymous users (§6.2).
- Segmentation exists only on the service, so client and server cannot disagree.
- Screenshots attach to sessions by wall time and the service places them (§5.5). The app
  remembers session and time locally in `screenshots.db`, so nothing about a screenshot is
  sent before it is uploaded.
