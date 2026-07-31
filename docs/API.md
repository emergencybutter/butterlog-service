# Butterlog WebService API Documentation

This document describes the Web API for Butterlog, designed for consumption by both humans and agents.

## Base URL

You can assume this base url is proxied to the webservice:
`https://butterlog.flyvoyager.net/api/v0`

## Authentication

In a client app, the user will click on "login with discord" or equivalent and the client will launch the browser to `https://butterlog.flyvoyager.net/api/v0/auth/login` (append `?port=<port>` to have the callback redirect to a loopback listener at `http://127.0.0.1:<port>?token=<token>`). The user follows the Discord OAuth flow and the client receives an authentication token.

Each login issues a fresh token; previously issued tokens stay valid, so a web login does not invalidate a desktop app's saved token. Tokens idle for 180 days are pruned. Only a SHA-256 hash of the token is stored server-side.

**Preferred: header authentication.** Send the token in the `Authorization` header and use the base url `https://butterlog.flyvoyager.net/api/v0`:

```
Authorization: Bearer <token>
```

**Legacy: path-token authentication (deprecated).** Old clients embed the token in the URL and use the base url `https://butterlog.flyvoyager.net/api/v0/users/:webhookToken`. These routes keep working, but tokens in URLs end up in proxy/infrastructure logs -- new integrations should use the header form.

The authenticated endpoints below exist under both base urls with identical request/response shapes. Endpoints marked **public** require no authentication and exist only under `/api/v0`.

## Endpoints

### Flight Management

#### Create a Flight
`POST /flights`

Creates a new flight entry. Triggers a Discord notification sync in the background.

*   **Request Body (JSON):**
    *   `departure` (string): ICAO code of the departure airport (e.g., "KLAX").
    *   `statistics` (object): A `FlightSummary` object (see [Data Structures](#data-structures)).
    *   `notes` (string, optional): Pilot notes, max 500 characters.
    *   `multiplayer_enabled` (bool, optional): Register this client for peer discovery.
    *   `udp_address` (string, optional): The client's public `ip:port` for UDP multiplayer.
*   **Response:**
    *   `201 Created`: Returns the created `Flight` object (includes `peers` when multiplayer is enabled).
    *   `400 Bad Request`: Missing required fields or notes too long.
    *   `401 Unauthorized`: Invalid or missing authentication.

#### Update a Flight
`PUT /flights/:id`

Updates an existing flight (e.g., when it lands or progress is made). Triggers a Discord notification sync (throttled to once per minute for telemetry-only changes).

*   **Path Parameters:**
    *   `id` (number): The database ID of the flight.
*   **Request Body (JSON):**
    *   `arrival` (string, optional): ICAO code of the arrival airport. Omitting keeps the current value.
    *   `statistics` (object): An updated `FlightSummary` object (replaces the stored one).
    *   `notes` (string, optional): Omitting keeps the current value.
    *   `multiplayer_enabled` / `udp_address`: as in Create.
*   **Response:**
    *   `200 OK`: Returns the updated `Flight` object.
    *   `404 Not Found`: Flight ID does not exist for this user.

#### Get Flight Details
`GET /flights/:id`

Retrieves a specific flight's data.

*   **Path Parameters:**
    *   `id` (number): The database ID of the flight.
*   **Response:**
    *   `200 OK`: Returns the `Flight` object.
    *   `404 Not Found`: Flight ID does not exist for this user.

#### Update Flight Notes
`PUT /flights/:id/notes`

Updates only the pilot notes for a flight.

*   **Path Parameters:**
    *   `id` (number): The database ID of the flight.
*   **Request Body (JSON):**
    *   `notes` (string): Max 500 characters.
*   **Response:**
    *   `204 No Content`: Successfully updated.
    *   `400 Bad Request`: Notes too long.
    *   `404 Not Found`: Flight ID does not exist for this user.

#### Upload a Track Batch
`POST /flights/:id/track`

Appends telemetry samples to an in-progress flight, so it can be watched live on the web. Kept separate from `PUT /flights/:id` because that endpoint triggers a Discord notification sync and so cannot be sent frequently; this one is a pure append and also acts as the flight's liveness heartbeat (it bumps `updated_at`).

*   **Request Body:** gzip-compressed JSON (`Content-Type: application/octet-stream`), at most **256KB decompressed**, at most **2000 samples**:
    ```jsonc
    {
      "startEpoch": 1753900123,   // absolute unix seconds of the first sample
      "points": {                 // columnar, same layout as a share's transposedData
        "timestamps": [0, 10, 10],  // deltas; the first is 0
        "latitudes": [], "longitudes": [],
        "altitudes": [], "ias": [], "vspeed": [], "pitch": [], "roll": []
      },
      "events": []                // FlightEvent objects, merged into statistics.events
    }
    ```
    `latitudes`/`longitudes` must match the timestamp count. The other columns may be omitted entirely, but if present must also match — a short column would misalign every later sample. Samples without a usable fix are dropped.
*   **Response:**
    *   `200 OK`: `{ "lastEpoch", "accepted", "duplicates", "totalPoints", "status" }`.
    *   `400 Bad Request`: malformed batch or mismatched columns.
    *   `410 Gone`: the flight has ended; stop sending.
    *   `413 Payload Too Large` / `429 Too Many Requests`.

Writes are `ON CONFLICT DO NOTHING` on `(flight_id, sample_epoch)`, so **the endpoint is idempotent**: the sample timestamp is the natural key. Re-sending a batch after a timeout, resuming from a stale cursor, or two clients briefly racing are all no-ops. There is no sequence number and no resync handshake, and batches may arrive out of order.

#### Track Cursor
`GET /flights/:id/track/cursor`

Where the server thinks the track ends: `{ "lastEpoch", "totalPoints", "status" }`. A bandwidth optimisation for a client that lost its local cursor — replaying the whole flight would also be correct, just wasteful.

#### End a Flight
`POST /flights/:id/end`

*   **Request Body (JSON, optional):** `reason` (`landed` | `sim_closed` | `abandoned`, default `landed`), `shareId` (string, optional).
*   **Response:** `204 No Content`. Calling it twice is not an error.

Sets `status = 'ended'`, so the live page can hand off to the permanent share rather than showing a frozen track. Without this call a flight is ended by a reaper 30 minutes after its last update.

---

### Screenshot Management

#### Upload a Screenshot
`POST /flights/:id/screenshots`

Uploads an image for a specific flight. The client is expected to resize and encode before uploading: the service only accepts **WebP** images with width and height of at most **1600px**, and the request body is capped at **15MB**. The image is stored in object storage keyed by its SHA-256 hash; re-uploading the same image is a no-op.

*   **Path Parameters:**
    *   `id` (number): The database ID of the flight.
*   **Request Body (Multipart/Form-Data):**
    *   `screenshot` (file): The WebP image file to upload.
*   **Response:**
    *   `201 Created`: Returns `{ "hash": "<sha256 of the upload>", "url": "<public image url>" }`.
    *   `400 Bad Request`: No file uploaded, not WebP, or dimensions exceed 1600px.
    *   `404 Not Found`: Flight not found.

#### Delete a Screenshot
`DELETE /flights/:id/screenshots/:hash`

Removes a screenshot from a flight (database record and stored object).

*   **Path Parameters:**
    *   `id` (number): The database ID of the flight.
    *   `hash` (string): The SHA-256 hash of the screenshot.
*   **Response:**
    *   `204 No Content`: Successfully deleted.
    *   `404 Not Found`: Flight not found.

---

### Flight Shares

A share is a self-contained, gzip-compressed JSON document (track, summary, screenshot URLs) rendered by the public share page at `/content/flights/share/:share_id`.

#### Upload a Share
`POST /flights/share`

*   **Request Body:** gzip-compressed JSON (`Content-Type: application/octet-stream`). The decompressed document may be at most 32MB. When the document contains a `remoteFlightId` (or `remote_flight_id`) field, the flight's Discord notification is updated with the share link.
*   **Response:**
    *   `201 Created`: Returns `{ "url": "<public share page url>", "id": "<share uuid>" }`.
    *   `400 Bad Request`: Empty body, invalid gzip/JSON, or decompressed size over the limit.

#### Delete a Share
`DELETE /flights/share/:share_id`

Deletes a share you own (database record and stored object).

*   **Response:**
    *   `204 No Content`: Successfully deleted.
    *   `404 Not Found`: Share not found or not owned by you.

#### Fetch Share Data (public)
`GET /api/v0/flights/share/:share_id`

Returns the decompressed share JSON. No authentication; served with `Access-Control-Allow-Origin: *` and `Cache-Control: public, max-age=86400`.

---

### Multiplayer

#### Ping
`POST /multiplayer/ping`

Registers this client's UDP endpoint for peer discovery and returns the other active peers. Presence expires 120 seconds after the last ping; sending a null or empty `udp_address` unregisters the client.

*   **Request Body (JSON):**
    *   `udp_address` (string or null): The client's public `ip:port` discovered via STUN.
*   **Response:**
    *   `200 OK`: Returns `{ "peers": ["ip:port", ...] }` (empty when unregistering).

---

### Public Data

#### Live Map Data (public)
`GET /api/v0/map/data`

Returns every flight updated in the last 5 minutes, for the live map. Intentionally unauthenticated, mirroring the public flight history pages.

*   **Response:** `200 OK` with an array of:
    ```json
    {
      "flight_id": 123,
      "pilot_name": "string",
      "departure": "KLAX",
      "arrival": "KSFO",
      "aircraft_type": "string",
      "latitude": 0.0,
      "longitude": 0.0,
      "altitude": 0.0,
      "heading": 0.0,
      "speed": 0.0,
      "updated_ago_secs": 12,
      "live": true
    }
    ```
    `live` marks a flight that is streaming a track, so a client can link to the live detail page.

#### Aircraft Usage Stats (public)
`GET /api/v0/stats/aircraft`

Aircraft-usage leaderboard aggregated across all flights, grouped by resolved ICAO type designator (e.g. `A320`). The same set of aircraft is returned ranked three ways; every entry carries all three metrics so a client can sort by whichever it likes. Derived entirely from each flight's `statistics`: flight count, total flown time (`landing_time − takeoff_time`, falling back to `end_time − start_time`), and total great-circle distance between the takeoff and landing snapshot positions. Flights missing a timestamp or snapshot still count toward `flights`; they simply add no time or distance. Intentionally unauthenticated, like the live map.

*   **Response:** `200 OK`
    ```json
    {
      "by_flights":  [ { "icao": "A320", "flights": 42, "total_seconds": 151200, "total_hours": 42.0, "total_distance_nm": 9876.5 } ],
      "by_time":     [ { "icao": "A320", "flights": 42, "total_seconds": 151200, "total_hours": 42.0, "total_distance_nm": 9876.5 } ],
      "by_distance": [ { "icao": "A320", "flights": 42, "total_seconds": 151200, "total_hours": 42.0, "total_distance_nm": 9876.5 } ]
    }
    ```
    `by_flights` is sorted by `flights` desc, `by_time` by `total_seconds` desc, `by_distance` by `total_distance_nm` desc (ties broken by ICAO).

#### User Current Flight Telemetry (public)
`GET /api/v0/user/:user_id/current`

Exposes a clean, typed snapshot of the user's active flight (updated in the last 5 minutes), or `null` if they are not currently flying. The live position is extracted from the flight's `current_snapshot` server-side, so clients don't parse the raw statistics blob.

*   **Path Parameters:**
    *   `user_id` (number): The database ID of the user.
*   **Response:** `200 OK` with either:
    *   `null` if not currently flying.
    *   A JSON object containing:
        ```json
        {
          "flight_id": 123,
          "departure": "KLAX",
          "arrival": "KSFO",
          "aircraft_type": "Boeing 737-800",
          "position": {
            "latitude": 34.05,
            "longitude": -118.24,
            "altitude": 12500.0,
            "heading": 271.3,
            "speed": 289.0
          },
          "updated_ago_secs": 3,
          "updated_at": "ISO8601 Date String"
        }
        ```
    *   `arrival` is `null` while still en route; `position` is `null` until the flight reports a usable location. `altitude` is MSL feet, `heading` is degrees true, `speed` is ground speed in knots. `updated_ago_secs` lets a client fade a stale position rather than show it as live.

#### User Current Flight Telemetry by Discord ID (public)
`GET /api/v0/user/by-discord/:discord_id/current`

Identical response to the numeric-id endpoint above, but keyed by the pilot's Discord user id (`users.discord_id`) instead of the internal numeric id. Lets a client that already knows who's signed in (e.g. freeflight, which authenticates with Discord) show a pilot's live flight without them looking up a Butterlog id.

*   **Path Parameters:**
    *   `discord_id` (string): The pilot's Discord user id (snowflake).
*   **Response:** `200 OK` with the same object as above, or `null` if that Discord user has no active flight (or isn't a known Butterlog user).

#### Live Flight Document (public)
`GET /api/v0/flights/:id/live`

The in-progress equivalent of a share: the same document shape (`summary`, `transposedData`, `screenshots`), so one client-side renderer draws both. Public, mirroring the live map and the flight history pages.

*   **Query:** `since=<epoch>` returns only samples newer than that timestamp, and omits `summary`/`screenshots` (setting `summaryUnchanged: true`) — a typical delta at a 10s poll is a few hundred bytes. Omit it for the whole track.
*   **Response:** `200 OK`
    ```jsonc
    {
      "status": "active" | "stale" | "ended",
      "lastEpoch": 1753900155,       // feed back as ?since=
      "updatedAgoSecs": 6,
      "shareId": null,               // set once ended and shared; the page links to it
      "trackPoints": 1204,
      "commands": ["pause"],         // what the pilot's client will accept, may be empty
      "pilot": { "userId": 12, "name": "..." },
      "current": { "latitude", "longitude", "altitude", "heading", "speed", "phase" },
      "summary": { /* share-shaped FlightSummary */ },
      "transposedData": { /* columnar samples */ },
      "screenshots": [ { "timestamp", "url" } ],
      "remoteFlightId": 123
    }
    ```
    `active` means reporting; `stale` means no update for over 120s (a frozen track, not a stopped aeroplane); `ended` means the app said so or the reaper gave up. Sends an `ETag`; an unchanged poll answers `304`.

---

### Sim Commands

Commands sent from the web to a running simulator. **Owner-only in both directions** — this is the one non-public part of the live-flight feature. The desktop app also has to opt in (`Allow remote commands`, off by default) before it will poll at all.

Command types, with stability. `pause` is **stable**; the rest are **beta** because they drive the aircraft's *default* autopilot, and study-level add-ons implement their own and ignore the stock events — the command is delivered, reported applied, and nothing moves.

| `type` | params | stability |
|---|---|---|
| `pause` | `state`: `on` \| `off` \| `toggle` | stable |
| `set_heading_bug` | `heading`: 0–359 | beta |
| `ap_heading_mode` | `enabled`: bool | beta |
| `ap_nav_mode` | `enabled`: bool | beta |
| `set_vertical_speed` | `fpm`: −6000…6000 | beta |
| `set_altitude` | `feet`: 0…60000 | beta |

#### Issue a Command
`POST /api/v0/flights/:id/commands` — body `{ "type", "params", "ttlSecs" }` (TTL default 30s, max 120s, so a command issued while the app was offline cannot fire on reconnect).

*   `202 Accepted`: `{ "commandId", "status": "pending", "stability" }`.
*   `403` not the owner · `409` flight not active · `422` bad parameter · `429` over 30/min.

#### Poll for Commands (the app)
`GET /api/v0/flights/:id/commands?wait=25` — long-poll. Returns pending commands immediately if any exist, otherwise holds up to `wait` seconds (max 25) and answers `204`. Delivery is stamped as the commands are claimed, so a command is handed out **at most once** even if the ack never arrives.

#### Command Status
`GET /api/v0/flights/:id/commands/:cid` — `{ "id", "type", "stability", "status", "detail" }`, where status is `pending`, `delivered`, or a terminal `applied` / `unsupported` / `rejected` / `expired`. `unsupported` is a first-class outcome: it is what the app reports when the connected sim has no binding for the command.

#### Acknowledge a Command (the app)
`POST /api/v0/flights/:id/commands/:cid/ack` — body `{ "result", "detail" }`. Always `204`, including for an already-acked command, so a retrying client is never told it failed.

---

### Discord Notification Settings

These endpoints use the web session (the `token` cookie set by the OAuth callback) or a Bearer token.

*   `GET /api/v0/discord-notification-channels` -- channel IDs currently receiving your flight notifications. Channels are managed automatically from the allowlist and your guild memberships; direct mutation endpoints respond `403`.
*   `POST /api/v0/admin/allowlist-channel` -- body `{ "channelId": "...", "guildId": "...", "channelName": "..." }`. Allowlists a channel for notifications. Requires the caller to be a Discord administrator of the guild; the bot must be able to post in the channel.
*   `DELETE /api/v0/admin/allowlist-channel/:channel_id` -- removes a channel from the allowlist (admin only).

---

### Web Pages (HTML, public)

*   `/` -- landing page with Discord login.
*   `/content` -- latest flights from every pilot.
*   `/content/flight/user/:user_id` -- one pilot's flights.
*   `/content/flights/:id` -- flight detail page. While the flight is in progress and streaming a track, this serves the **live** variant instead: the same detail, polled from `/api/v0/flights/:id/live`, with a LIVE badge and (for the pilot) the sim control strip.
*   `/content/flights/share/:share_id` -- shared flight page (map, charts, screenshots).
*   `/content/settings` -- Discord notification settings (requires login).
*   `/map` -- live traffic map.

---

## Data Structures

### Flight Object
```json
{
  "id": 123,
  "user_id": 1,
  "departure": "KLAX",
  "arrival": "KSFO",
  "statistics": { ... },
  "screenshots": ["hash1", "hash2"],
  "notes": "optional, omitted when null",
  "peers": ["ip:port"]
}
```
`peers` is only present on create/update responses when `multiplayer_enabled` is true.

### FlightSummary Object
The `statistics` field is a client-defined JSON document. Fields the service and Discord embeds read:
```json
{
  "airframe_name": "string",
  "simulator": "string",
  "simulator_version": "string",
  "departure": { "icao": "KLAX", "name": "Los Angeles Intl" },
  "arrival": { "icao": "KSFO", "name": "San Francisco Intl" },
  "takeoff_time": "ISO8601 Date String or null",
  "landing_time": "ISO8601 Date String or null",
  "start_time": "ISO8601 Date String or null",
  "end_time": "ISO8601 Date String or null",
  "takeoff_snapshot": "object | null",
  "landing_snapshot": "object | null  (VSpd and NormAc drive the landing badge)",
  "current_snapshot": "object | null  (Latitude/Longitude/AltMSL/HDG/GndSpd drive the live map)",
  "max_entries": "object | null",
  "closest_airport": "{ icao, name, distance } | null"
}
```
