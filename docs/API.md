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
      "updated_ago_secs": 12
    }
    ```

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
*   `/content/flights/:id` -- flight detail page.
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
