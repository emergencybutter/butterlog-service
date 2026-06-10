# Butterlog WebService API Documentation

This document describes the Web API for Butterlog, designed for consumption by both humans and agents.

## Rediretions

You can assume these base urls are proxied to the webservice:
`https://butterlog.flyvoyager.net/api/v0`

## Authentication

In a client app, the user will click on "login with discord" or equivalent and the client will launch the browser to `https://butterlog.flyvoyager.net/api/v0/auth/login`. At this point the use would follow the auth workflow normally and the client gets back some authentication token. Each login issues a fresh token; previously issued tokens stay valid (tokens idle for 180 days are pruned). Only a SHA-256 hash of the token is stored server-side.

**Preferred: header authentication.** Send the token in the `Authorization` header and use the base url `https://butterlog.flyvoyager.net/api/v0`:

```
Authorization: Bearer <token>
```

**Legacy: path-token authentication (deprecated).** Old clients embed the token in the URL and use the base url `https://butterlog.flyvoyager.net/api/v0/users/:webhookToken`. These routes keep working, but tokens in URLs end up in proxy/infrastructure logs -- new integrations should use the header form.

All endpoints below exist under both base urls with identical request/response shapes.


## Endpoints

### Flight Management

#### Create a Flight
`POST /flights`

Creates a new flight entry.

*   **Path Parameters:**
    *   `webhookToken`: Your unique authentication token.
*   **Request Body (JSON):**
    *   `departure` (string): ICAO code of the departure airport (e.g., "KLAX").
    *   `statistics` (object): A `FlightSummary` object (see [Data Structures](#data-structures)).
*   **Response:**
    *   `201 Created`: Returns the created `Flight` object.
    *   `400 Bad Request`: Missing required fields.
    *   `401 Unauthorized`: Invalid or missing authentication.

#### Update a Flight
`PUT /flights/:id`

Updates an existing flight (e.g., when it lands or progress is made).

*   **Path Parameters:**
    *   `webhookToken`: Your unique authentication token.
    *   `id` (number): The database ID of the flight.
*   **Request Body (JSON):**
    *   `arrival` (string, optional): ICAO code of the arrival airport.
    *   `statistics` (object): An updated `FlightSummary` object.
*   **Response:**
    *   `200 OK`: Returns the updated `Flight` object.
    *   `404 Not Found`: Flight ID does not exist for this user.

#### Get Flight Details
`GET /flights/:id`

Retrieves a specific flight's data.

*   **Path Parameters:**
    *   `webhookToken`: Your unique authentication token.
    *   `id` (number): The database ID of the flight.
*   **Response:**
    *   `200 OK`: Returns the `Flight` object.
    *   `404 Not Found`: Flight ID does not exist for this user.

---

### Screenshot Management

#### Upload a Screenshot
`POST /flights/:id/screenshots`

Uploads an image for a specific flight. Images are automatically resized to 1600px width and compressed as optimized webp format.

*   **Path Parameters:**
    *   `webhookToken`: Your unique authentication token.
    *   `id` (number): The database ID of the flight.
*   **Request Body (Multipart/Form-Data):**
    *   `screenshot` (file): The image file to upload.
*   **Response:**
    *   `201 Created`: Returns `{ "hash": "sha256-hash-of-processed-image" }`.
    *   `400 Bad Request`: No file uploaded.
    *   `404 Not Found`: Flight not found.

#### Delete a Screenshot
`DELETE /flights/:id/screenshots/:hash`

Removes a screenshot from a flight.

*   **Path Parameters:**
    *   `webhookToken`: Your unique authentication token.
    *   `id` (number): The database ID of the flight.
    *   `hash` (string): The SHA-256 hash of the screenshot.
*   **Response:**
    *   `204 No Content`: Successfully deleted.
    *   `404 Not Found`: Flight or screenshot not found.

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
  "screenshots": ["hash1", "hash2"]
}
```

### FlightSummary Object
The `statistics` field contains detailed flight data:
```json
{
  "log_path": "string",
  "airframe_name": "string",
  "departure": { "icao": "KLAX", "name": "Los Angeles Intl" },
  "arrival": { "icao": "KSFO", "name": "San Francisco Intl" },
  "takeoff_time": "ISO8601 Date String or null",
  "landing_time": "ISO8601 Date String or null",
  "start_time": "ISO8601 Date String or null",
  "end_time": "ISO8601 Date String or null",
  "takeoff_snapshot": object | null,
  "landing_snapshot": object | null,
  "current_snapshot": object | null,
  "max_entries": object | null,
  "landing_scorecard": object | null
}
```
