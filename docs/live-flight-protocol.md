# Live Flight Protocol — design

**Status:** implemented. See `docs/API.md` for the endpoint reference; this document
remains the record of *why* the shape is what it is. Deviations from the design as written
are noted inline as **[as built]**.
**Goal:** an ongoing flight is visible online with the same detail as a finished share —
track on a 2D map and in 3D, altitude/IAS/VS/pitch charts, flight events, screenshots —
updating as the aircraft flies. Plus a reverse channel (§6) letting the pilot act on their
own simulator from that page: pause it, and drive the autopilot.

## 1. Where we are today

| Concern | Today | Gap |
|---|---|---|
| Liveness | `PUT /flights/:id` every ~60s, `statistics.current_snapshot` holds one position | One point, no history |
| Track | Only in the share document, uploaded once after landing | Nothing while airborne |
| Events (takeoff/TOC/TOD/landing) | Only in `FlightSummary.events` inside the share doc | Not in `statistics`, so invisible live |
| Screenshots | Already uploaded live via `POST /flights/:id/screenshots` | Fine as-is |
| Rendering | `share_detail.html` renders the full experience from one static JSON | Needs a live feed of the same shape |
| Flight end | Implicit — "not updated in 5 minutes" | No explicit end, can't distinguish landed from crashed-app |

Two structural facts drive the design:

1. `statistics` is an opaque `JSONB` the service only cherry-picks from
   (`extract_live_position`, `discord.rs`). **Adding fields to it is backward compatible.**
2. `share_detail.html` already knows how to render the full picture from a
   `FlightDetailShare`-shaped document. **The live read model should emit that same shape**
   so we get one renderer, not two.

## 2. Shape of the solution

Add an **append-only track channel** alongside the existing flight channel.

```
          existing (unchanged cadence, still triggers Discord sync)
app ──► POST /flights            ─────────────────────────► flights row
    ──► PUT  /flights/:id        ─────────────────────────► statistics, current_snapshot

          new (frequent, cheap, NO Discord sync)
    ──► POST /flights/:id/track  ─────────────────────────► flight_track_points
    ──► POST /flights/:id/end    ─────────────────────────► status = ended

          new read side (public)
web ──► GET /api/v0/flights/:id/live?since=<epoch> ◄─────── assembled share-shaped doc

          new command channel (owner only, never public — §6)
web ──► POST /api/v0/flights/:id/commands ────────────────► flight_commands
app ──► GET  /api/v0/flights/:id/commands (long-poll) ◄──── pending commands
    ──► POST /api/v0/flights/:id/commands/:cid/ack ───────► result
```

Keeping track upload on its own endpoint is deliberate: `PUT /flights/:id` fires a Discord
notification sync, so its cadence cannot be raised. The track endpoint is a pure append
and also serves as the liveness heartbeat (it bumps `flights.updated_at`).

## 3. Write protocol

### 3.1 `POST /flights/:id/track` (authenticated, owner only)

Appends a batch of telemetry samples. Body is **gzip-compressed JSON**
(`Content-Type: application/octet-stream`), decompressed cap **256 KB**.

```jsonc
{
  "startEpoch": 1753900123,  // absolute unix seconds of the first sample
  "points": {                // same columnar layout as TransposedFlightData
    "timestamps": [0, 10, 10, 12],   // deltas after the first (which is 0)
    "latitudes":  [f32, ...],
    "longitudes": [f32, ...],
    "altitudes":  [f32, ...],
    "ias":        [f32, ...],
    "vspeed":     [f32, ...],
    "pitch":      [f32, ...],
    "roll":       [f32, ...]
  },
  "events": [ /* FlightEvent, cumulative, see 3.3 */ ]
}
```

Response `200`:

```json
{ "lastEpoch": 1753900155, "accepted": 3, "duplicates": 0, "totalPoints": 1204, "status": "active" }
```

The service explodes the batch into rows and writes them with
`INSERT ... ON CONFLICT (flight_id, sample_epoch) DO NOTHING`. **The sample timestamp is the
natural key, so the endpoint is idempotent by construction.** Re-sending a batch after a
timeout, an app resuming from a stale cursor, and two clients briefly racing on the same
flight are all no-ops rather than errors. There is no sequence number, no server cursor to
agree on, and no resync handshake. Batches may arrive out of order.

Errors:

* `413` — decompressed body over cap.
* `429` — rate limit (see §7).
* `410 Gone` — flight already ended; the client should stop the track loop.

Columnar layout is reused verbatim from `TransposedFlightData`, so the app's existing
transposition code and the share renderer's decoder both work unchanged. Note that the
columnar form is a *transport* choice — it matches what the app already builds and what the
renderer already reads — and is independent of how the service stores the samples (§4).

### 3.2 Client-side cadence and thinning

* Buffer samples in memory as the sim monitor already produces them (1 Hz).
* Apply the **existing `downsample()`** from `flight_log_manager.rs` — straight and level
  (instantaneous VS ≤ 100 fpm and bank ≤ 3°, with a drift backstop) 1/300s, everything else
  1 Hz, and the ±60 s window around takeoff/landing forced out of the steady tier. Same
  thinning as the share, so the live track and the eventual share are consistent.
* Flush every **20 s**, or immediately when `downsample` emits a near-event sample
  (takeoff, touchdown) so the interesting moments appear online without delay.
* A `live_last_epoch` cursor persists in the flight's SQLite `summary` table next to the
  existing `remote_id` / `share_url` keys, so a restarted app resumes where it left off.
  This is purely a bandwidth optimisation, not a correctness requirement — a client that
  loses its cursor can replay the whole flight and the server will dedup it. When the local
  cursor is missing, `GET /flights/:id/track/cursor` returns `{ lastEpoch, totalPoints }`
  so the app can avoid re-uploading hours of samples.

Bandwidth, at ~30 bytes/point gzipped: an active phase runs at 1 Hz → 3600 points/hour →
**~110 KB/hour**, while cruise runs at 12 points/hour → effectively free. A typical 3-hour
airliner flight (20 min climb, 25 min descent and approach, the rest cruise) lands around
**3000 points / ~90 KB** total. A batch during climb or descent is ~20 points (~600 bytes);
in cruise most flushes carry no new points at all and are sent anyway as the liveness
heartbeat.

Note this is roughly 4–5× the old policy's volume, concentrated entirely in the phases
worth watching. It is still small in absolute terms and well inside the share document's
32 MB cap.

### 3.3 Events in `statistics`

Add `events: Vec<FlightEvent>` to `WebhookFlightSummary` (app side). It rides in the
existing `statistics` blob on the next `PUT`, and is repeated in the track batch that first
observes a new event so it lands online promptly without an extra PUT. The service treats
the union, keyed by `(event_type, timestamp)`.

Also worth adding to `statistics` while we're there, since the live view wants them:
`flight_phase` (the analyzer's current `FlightPhase`) and `fuel_consumed`.

> **[as built]** Events reach `statistics.events` *only* via the track batch, which the
> service merges under a row lock (`track::merge_events`). `WebhookFlightSummary` was left
> alone: the batch already carries them promptly, and adding a field would have meant
> editing six struct literals across both sim monitors for no extra coverage — a flight not
> streaming a track has no live view to feed.
>
> `flight_phase` and `fuel_consumed` were **not** added, so the live page's phase label is
> absent and its Fuel Consumed tile reads 0 until the flight ends and the share is built.
> `flight_phase` is cheap (`analyzer.current_phase` is already to hand at each of those six
> sites); `fuel_consumed` is not — nothing derives it during flight, only `parse_db_file`
> does, afterwards. Both are cosmetic and neither blocks the feature.

### 3.4 `POST /flights/:id/end` (authenticated)

```json
{ "reason": "landed" | "sim_closed" | "abandoned", "shareId": "uuid or null" }
```

Sets `status = 'ended'`, `ended_at = now()`, and records `share_id` when the app has
already uploaded the permanent share. This lets the live page redirect to the share
instead of showing a frozen track, and lets the service prune track points (§7).

Absent this call (app killed, machine slept), the flight goes `stale` by timeout — see §5.

## 4. Storage

```sql
-- New: one row per telemetry sample. The sample timestamp is the natural key.
CREATE TABLE flight_track_points (
    flight_id    BIGINT NOT NULL REFERENCES flights(id) ON DELETE CASCADE,
    sample_epoch BIGINT NOT NULL,   -- absolute unix seconds
    latitude     REAL   NOT NULL,
    longitude    REAL   NOT NULL,
    altitude     REAL,
    ias          REAL,
    vspeed       REAL,
    pitch        REAL,
    roll         REAL,
    PRIMARY KEY (flight_id, sample_epoch)
);

-- New columns on flights.
ALTER TABLE flights ADD COLUMN status       TEXT NOT NULL DEFAULT 'active';  -- active | ended
ALTER TABLE flights ADD COLUMN ended_at     TIMESTAMPTZ;
ALTER TABLE flights ADD COLUMN end_reason   TEXT;
ALTER TABLE flights ADD COLUMN share_id     TEXT REFERENCES flight_shares(id) ON DELETE SET NULL;
ALTER TABLE flights ADD COLUMN track_points INTEGER NOT NULL DEFAULT 0;

CREATE INDEX idx_flights_status_updated ON flights (status, updated_at DESC);
```

The primary-key index is the whole access path: the delta read in §5.2 is one range scan
on `(flight_id, sample_epoch)`, and the service transposes the rows back into the columnar
wire format on the way out. R2 is not needed for the live phase; the finished share already
goes to R2 and remains the archival artifact.

**Why rows and not a compressed blob column.** The obvious alternative is one row per
uploaded batch with the gzipped payload in a `BYTEA`. It looks cheaper and isn't:

* **The compression saves nothing here.** At a 20 s flush a batch is 2–3 points, ~200 bytes
  of JSON. Gzip carries 18 bytes of fixed overhead and compresses short numeric JSON poorly.
  Worse, Postgres only TOASTs and compresses values above ~2 KB, so a batch that small is
  stored inline and uncompressed whichever type it lands in — the application-level gzip
  buys essentially nothing at rest.
* **"Store as received" doesn't survive the rest of the design.** The full read (§5.1) has
  to concatenate batches into a single `transposedData`, and independently gzipped JSON
  fragments don't concatenate into valid JSON — so every read decompresses everything
  anyway. The retention thinning in §7 also has to decompress and re-encode. The pass-through
  never actually happens.
* **A blob is opaque to SQL.** No server-side aggregation, no map-trail query, no `psql`
  debugging, no `max(altitude)` without an application round-trip.

The cost of rows is tuple overhead — a 23-byte header per ~40 bytes of payload. A typical
3-hour flight is ~3000 rows (~190 KB); the 50 000-point ceiling in §7 is ~3 MB. Irrelevant at
this scale, and it buys idempotent writes (§3.1) and cheap range reads.

If a payload ever genuinely wants to be an opaque blob, the right home is R2 alongside the
shares, not a Postgres column — a `BYTEA` is the worst of both, as opaque as a blob and as
expensive as a database.

## 5. Read protocol

### 5.1 `GET /api/v0/flights/:id/live` (public)

Returns a document deliberately **shaped like `FlightDetailShare`** plus a live envelope:

```jsonc
{
  "status": "active" | "stale" | "ended",
  "lastEpoch": 1753900155,        // newest sample included; feed back as ?since=
  "updatedAgoSecs": 6,
  "shareId": null,                // set once ended and shared → client can redirect
  "pilot": { "userId": 12, "name": "..." },
  "trackPoints": 1204,
  "current": {                    // from statistics.current_snapshot, same as /map/data
    "latitude": 34.05, "longitude": -118.24,
    "altitude": 12500.0, "heading": 271.3, "speed": 289.0,
    "phase": "Cruise"
  },
  "summary": { /* share-shaped FlightSummary, adapted server-side — see 5.3 */ },
  "transposedData": { /* every sample, transposed into the columnar wire format */ },
  "screenshots": [ { "timestamp": 1753900500, "url": "..." } ],
  "remoteFlightId": 123
}
```

* `status`: `active` if updated < 120 s ago, `stale` between 120 s and the 30-minute
  reaper, `ended` once `POST /end` landed or the reaper fired.
* Cache: `Cache-Control: no-store` for `active`, `public, max-age=86400` once `ended`.
* CORS `*`, matching the existing public endpoints.

### 5.2 `GET /api/v0/flights/:id/live?since=<epoch>` — delta

Same envelope, but `transposedData` carries only samples with `sample_epoch > since` —
a single range scan on the primary key — and `summary` / `screenshots` are included only
when they changed (`"summaryUnchanged": true`). The client passes back the `lastEpoch` it
last saw. The page appends to its arrays and pushes new points onto the Leaflet polyline,
the deck.gl path, and each Chart.js dataset. Typical delta at a 10 s poll is **a few
hundred bytes**.

`ETag` = `"<lastEpoch>-<statistics_updated_at>"`; a poll with no new data answers `304`.

Because `since` is a sample timestamp rather than a batch counter, a client can also ask
for any suffix of the track it likes — which is what makes the map trail in §5.4 a single
query with no extra endpoint.

### 5.3 Summary adaptation (server side)

The live flight carries `statistics` in `WebhookFlightSummary` shape; the share renderer
expects `FlightSummary` shape. One small pure function in the service maps between them:

| share `summary` field | source in `statistics` |
|---|---|
| `startIcao` / `startAirportName` | `departure.icao` / `departure.name` |
| `endIcao` / `endAirportName` | `arrival.icao` / `.name`, else `closest_airport` |
| `startTime` / `endTime` | `start_time` / `end_time` (null while airborne) |
| `durationMinutes` | now − `takeoff_time`, live |
| `aircraftTitle`, `livery`, `resolvedIcao`, `resolvedAirline`, `atcModel`, `atcId` | direct |
| `maxAltitude`, `maxGroundSpeed` | `max_entries.AltMSL`, `max_entries.GndSpd` |
| `fuelConsumed` | new `statistics.fuel_consumed` (§3.3) |
| `events` | new `statistics.events` ∪ batch events (§3.3) |
| `screenshotCount` | `screenshots` table count |

This is the only real translation layer, and it keeps a single client-side renderer.

### 5.4 Pages

* `/content/flights/:id` — when `status != 'ended'`, render the **live variant**: the same
  markup and JS as `share_detail.html`, bootstrapped from `/live` and polling
  `?since=` every 10 s, with a "LIVE" badge, current phase, and elapsed time. When ended
  with a `share_id`, link/redirect to the existing share page.
* `/map` — add `"live": true` to `/api/v0/map/data` entries that have track points, and
  make the popup link to the live detail page. The last N minutes of trail for the selected
  aircraft is just `/live?since=<now − N·60>` — no extra endpoint.

### 5.5 Transport choice

Polling, not SSE/WebSocket. Reasons: the map already polls at 5 s and the pattern is
proven behind the Cloudflare proxy; deltas plus `ETag`/`304` make a 10 s poll cost
~200 bytes; and long-lived connections are the one thing that behaves badly if the service
ever falls back to Cloud Run. SSE at `/api/v0/flights/:id/stream` is a drop-in later
upgrade — same delta payloads, different framing.

## 6. Command channel (web → sim)

Everything above is one-directional: the sim publishes, the web reads. This section adds the
reverse path, so a pilot watching their own flight from a phone or a second screen can act on
the simulator — pause it, and nudge the autopilot — without alt-tabbing.

This is the one part of the design that is **never public**. Reads are open to anyone;
commands are owner-only in both directions.

### 6.1 Delivery: long-poll

```
GET /api/v0/flights/:id/commands?wait=25     (authenticated, owner only — the app calls this)
```

Returns pending commands immediately if any exist, otherwise holds the connection up to
`wait` seconds and answers `204 No Content` on timeout. The app reconnects in a loop.

**Why not piggyback on the track POST.** The obvious move is to return commands in the
`POST /flights/:id/track` response, which the app already sends every 20 s — no new
connection. But pause is a reflex action: a 20 s worst case makes it unusable, and the
whole feature lives or dies on that one command feeling instant. Long-poll gives
sub-second delivery for one idle connection per flying user, which at this scale is
nothing. The piggyback remains a fallback if an intermediary won't hold the connection —
the app degrades to 20 s latency rather than losing the feature.

Long-poll is comfortable on the self-hosted VPS deployment. It is the one part of this
design that would want revisiting if the Cloud Run standby ever became primary.

### 6.2 Issuing a command

```
POST /api/v0/flights/:id/commands            (authenticated, owner only)
{ "type": "pause", "params": { "state": "on" }, "ttlSecs": 30 }
```

`202 Accepted` → `{ "commandId": "uuid", "status": "pending" }`.

Rejected with `409` when the flight is not `active`, `403` when the caller is not the flight
owner or the pilot has remote control switched off, `422` on an out-of-range parameter.

The issuing UI polls `GET /api/v0/flights/:id/commands/:cid` (owner only) at ~1 s until the
status is terminal, so the button can show pending → applied, or surface a failure.

### 6.3 Command catalogue

`pause` is **stable**. Everything else ships **beta** and is labelled as such in the UI.

| `type` | params | stability | MSFS | X-Plane |
|---|---|---|---|---|
| `pause` | `state: "on" \| "off" \| "toggle"` | **stable** | `PAUSE_SET` / `PAUSE_TOGGLE` | `sim/operation/pause_toggle` |
| `set_heading_bug` | `heading: 0–359` | beta | `HEADING_BUG_SET` | `sim/cockpit/autopilot/heading_mag` |
| `ap_heading_mode` | `enabled: bool` | beta | `AP_HDG_HOLD_ON` / `_OFF` | `sim/autopilot/heading` |
| `ap_nav_mode` | `enabled: bool` | beta | `AP_NAV1_HOLD_ON` / `_OFF` | `sim/autopilot/NAV` |
| `set_vertical_speed` | `fpm: −6000…6000` | beta | `AP_VS_VAR_SET_ENGLISH` | `sim/cockpit/autopilot/vertical_velocity` |
| `set_altitude` | `feet: 0…60000` | beta | `AP_ALT_VAR_SET_ENGLISH` | `sim/cockpit/autopilot/altitude` |

The sim bindings in that table are the conventional ones and should be treated as a
starting point to verify during implementation, not as settled fact.

**Why the autopilot commands are beta and pause is not.** Pause is a simulator-level
operation: it works on every aircraft in both sims, deterministically. The autopilot
commands write to the *default* autopilot, and study-level add-ons — PMDG, Fenix, and most
of what serious users fly — implement their own autopilot logic against internal LVARs and
simply ignore the stock SimConnect events. The command will be delivered, the app will
report it applied, and nothing will move. That failure is invisible from the service's side
and unfixable without per-aircraft support, so the honest thing is to ship the capability
labelled beta rather than imply a guarantee we cannot make.

### 6.4 Applying, and acknowledging

```
POST /api/v0/flights/:id/commands/:cid/ack
{ "result": "applied" | "unsupported" | "rejected" | "expired", "detail": "..." }
```

`unsupported` is a first-class outcome, not an error: it is what the app returns when the
connected sim has no binding for that command, and it is what lets the UI stop offering it.

**Sim-side plumbing.** The two sims are not symmetric here:

* **X-Plane** needs no new transport. The app already holds a Web API connection at
  `localhost:8086` (`/api/v3/datarefs` plus a WebSocket) that it reads through; the same API
  writes datarefs and activates commands. The X-Plane plugin is *not* required for this
  feature — worth stating plainly, since it is required for traffic injection.
* **MSFS needs new FFI surface.** `simplesimconnect` is read-only today: it wraps open,
  data definitions, data requests, and AI object creation, and has no
  `TransmitClientEvent` / `MapClientEventToSimEvent`. The raw `simplesimconnect-sys` crate
  is bindgen-generated from the full SimConnect header at build time, so the symbols are
  already there and nothing needs regenerating — the work is two new safe wrappers in
  `simplesimconnect`. This is the largest single piece of implementation in the feature.

X-Plane SDK and SimConnect calls must both be made on the thread that owns the sim
connection, so commands are queued from the long-poll task and drained on the existing
monitor loop rather than applied inline.

### 6.5 Safety

Remote control of a running simulator deserves more caution than the rest of this design.

* **Opt-in, off by default.** App config `allow_remote_commands`, default **off**, with a
  separate `allow_beta_commands` sub-toggle also default off. Nobody gets their sim poked
  because they logged in.
* **Owner only.** Both the issue and the poll are authenticated and checked against
  `flights.user_id`. There is no delegation in v1 — see §11.
* **TTL, default 30 s, max 120 s.** A command issued while the app was offline must not fire
  when it reconnects ten minutes later. Expired commands are never delivered.
* **At-most-once.** `delivered_at` is stamped when the long-poll hands a command over; a
  command already delivered is not redelivered on reconnect, even without an ack. A dropped
  ack loses the status, not the guarantee.
* **Validated twice**, server-side on issue and app-side on apply, so a compromised or
  buggy client cannot drive an out-of-range value into the sim.
* **Only while `active`.** Commands against a `stale` or `ended` flight are refused.
* **Rate limited** to 30/minute per flight.
* Every command is retained with its issuer and outcome — this is an audit trail, and it is
  also the data that tells us which beta commands actually work in the wild.

### 6.6 Capability advertisement

The app reports which command types the connected sim can service, in
`statistics.capabilities`:

```json
{ "commands": ["pause", "set_heading_bug", "ap_heading_mode"] }
```

The live page renders only advertised commands, so an X-Plane pilot and an MSFS pilot see
different control strips, and a pilot with remote control switched off sees none. Combined
with the `unsupported` ack result, a control that turns out not to work on the current
aircraft can be greyed out after its first attempt.

### 6.7 Storage

```sql
CREATE TABLE flight_commands (
    id           UUID PRIMARY KEY,
    flight_id    BIGINT NOT NULL REFERENCES flights(id) ON DELETE CASCADE,
    issued_by    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    type         TEXT NOT NULL,
    params       JSONB NOT NULL,
    stability    TEXT NOT NULL,          -- stable | beta
    issued_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at   TIMESTAMPTZ NOT NULL,
    delivered_at TIMESTAMPTZ,
    acked_at     TIMESTAMPTZ,
    result       TEXT,
    detail       TEXT
);

-- The long-poll's hot query: undelivered, unexpired commands for one flight.
CREATE INDEX idx_flight_commands_pending
    ON flight_commands (flight_id, issued_at)
    WHERE delivered_at IS NULL;
```

### 6.8 Web UI

An owner-only control strip on the live flight page. Pause is a single prominent button
reflecting current sim state. The five autopilot controls sit behind a collapsed **Beta**
disclosure, each carrying a beta chip, with copy stating plainly that they drive the default
autopilot and may do nothing on add-on aircraft. Each control shows pending → applied →
unsupported inline, so a command that silently no-ops is visible rather than mysterious.

## 7. Limits, retention, abuse

* **Per batch:** 256 KB decompressed, 2000 points.
* **Per flight:** 50 000 track points — about 14 hours of continuously hand-flown 1 Hz
  samples, so in practice only a marathon flight reaches it. Past the ceiling the service
  still accepts the batch, then
  thins the oldest hour to one sample per minute outside a ±60 s window around any flight
  event — a single `DELETE`, no decompress/re-encode cycle:

  ```sql
  DELETE FROM flight_track_points p
   WHERE p.flight_id = $1
     AND p.sample_epoch < $2                       -- older than the cutoff
     AND p.sample_epoch % 60 <> 0
     AND NOT EXISTS (SELECT 1 FROM unnest($3::bigint[]) e
                      WHERE abs(p.sample_epoch - e) <= 60);
  ```

  This keeps takeoff and touchdown at full resolution and the cruise legible, so a
  12-hour flight stays visually complete without unbounded growth.
* **Rate:** 10 track POSTs per flight per minute, 429 beyond. Normal cadence is 3/min.
  Idempotent writes mean a client that retries into the limit loses nothing.
* **Reaper:** a background sweep marks `active` flights with no update for 30 minutes as
  `ended`, `end_reason = 'abandoned'`.
* **Retention:** track points are deleted 7 days after `ended_at` when a `share_id` exists
  (the share is the durable copy), 30 days otherwise. Cascade already handles flight deletion.
* Ownership is checked on every write; the read side is public, consistent with the
  existing public map and flight history pages.

## 8. Privacy

Live visibility follows an explicit setting rather than being implied by logging in.

* New app config `share_live_flights: bool`, defaulting to the existing
  `auto_share_flights` value so behaviour matches user expectation on upgrade.
* When off, the app simply never posts track batches — the flight still logs, still syncs,
  still appears on the map exactly as today. No server-side flag needed for v1.
* If we later want per-flight control, `POST /flights` grows a `live: bool` and the service
  rejects batches for `live = false` flights with `403`.

## 9. Compatibility

* Every new field is additive and optional. Old app versions keep working unchanged: no
  track points, `status` defaults to `active`, the reaper ends them, and
  `/content/flights/:id` renders exactly as it does today.
* Old clients reading `/api/v0/map/data` ignore the new `live` field.
* No change to `PUT /flights/:id` semantics, so Discord sync throttling is untouched.
* The share upload path is entirely unchanged — a live flight that ends still produces the
  same share document, and that remains the permanent artifact.
* The command channel is inert for old clients: they never poll, so commands simply expire
  on their TTL. The web UI only offers controls a client has advertised (§6.6), so an
  un-upgraded app shows no control strip rather than a set of dead buttons.

## 10. Phasing

1. **Protocol + storage.** Migration, `POST /flights/:id/track`, `/track/cursor`,
   `POST /flights/:id/end`, reaper. Service-side only; verifiable with `curl`.
2. **App uploader.** Track buffer, reuse `downsample()`, SQLite cursor, `end` call on
   flight finalize, `share_live_flights` setting. Retry on failure is just "send it again" —
   the server dedups.
3. **Read API.** `/api/v0/flights/:id/live` full + delta, summary adapter, ETag.
4. **Live page.** Factor the `share_detail.html` renderer so it takes a document plus an
   "append delta" entry point; wire the live variant of `/content/flights/:id`.
5. **Map integration.** `live` flag, popup link, optional selected-aircraft trail.
6. **Command channel, pause only.** `flight_commands` migration, issue/long-poll/ack
   endpoints, `allow_remote_commands` setting, and the MSFS `TransmitClientEvent` /
   `MapClientEventToSimEvent` wrappers in `simplesimconnect` plus the X-Plane Web API
   write path. Ships one stable command end to end.
7. **Beta autopilot commands.** The five remaining types, `allow_beta_commands`, capability
   advertisement, and the beta disclosure in the UI.

Phases 1–2 are independently shippable: once they land, a flight's full track exists
server-side even before any UI reads it, which de-risks phase 4.

Phases 6–7 depend only on phase 4 for a place to put the controls, and are otherwise
independent of the track work — the command channel could be built first if remote pause
turns out to be the more wanted feature. Splitting them so that phase 6 carries the whole
transport and exactly one command means the risky part (new SimConnect FFI, long-poll
behaviour through the proxy) is proven against the command that is guaranteed to work,
before adding five that may not.

> **[as built]** Phases 6 and 7 landed together. The split was a de-risking measure for the
> sim bindings, and the mapping tables turned out to be pure functions (`msfs_event`,
> `xplane_action`) that are unit-tested without a simulator — so the five beta commands
> carried none of the risk the split was protecting against. The *gating* still honours the
> split: `allow_beta_commands` is a separate opt-in, and with it off the app advertises
> only `pause` and refuses the rest.

## 11. Open questions

* ~~Should the live page expose the 3D replay while airborne?~~ **[as built]** Resolved
  conservatively: it appears only once the flight ends. The camera fit and the playback
  scrub both span the whole track, so rendering it mid-flight would re-frame the view under
  the viewer on every poll.
* Do we want the track channel to carry a second, coarser column set (fuel, engine) for
  a live "systems" panel, or keep the eight columns the share already defines?
* Should `end_reason = 'sim_closed'` flights (no landing) still be promoted to a share
  automatically, or stay live-only until the pilot shares them manually?
* Should command control ever be delegable — a shared-cockpit or instructor case where a
  named Discord user can pause your sim? It is a real use case and a real hazard, and it
  needs a consent flow rather than a config flag, so v1 is owner-only.
* Can the app detect that a stock autopilot event was ignored — by reading back the
  autopilot datarefs/SimVars a second later and comparing — and turn that into a truthful
  `unsupported` ack instead of a hopeful `applied`? That would make the beta controls far
  less mysterious on add-on aircraft, and it is the main thing that would let them graduate.
* Does pause deserve a confirmation when the flight is being watched by others, or is the
  owner-only restriction sufficient?
