# Sim-Native Telemetry — rollout plan

**Status:** proposed. Companion to `docs/sim-native-telemetry.md`, which says *what* to build;
this says in what order to ship it. Reorders §8 of the design and adds what the current code
requires first.

## 1. What the code requires first

1. **The shared crate lives inside the service repo.** The workspace is four separate git
   repos, and the service `Dockerfile` builds with `COPY . .` from `butterlog-service/` only,
   so a sibling crate would break the image build. App and service share only the wire format
   and `TrafficState` (design §6), so `butterlog-wire` goes in `butterlog-service/wire/`:
   already in the Docker build context, and reached by the app and plugin by path, as the app
   reaches `simplesimconnect` today.
2. **The app only builds for Windows.** `simplesimconnect` is an unconditional dependency and
   `latest.json` lists only `windows-x86_64`. Plugin capture on macOS and Linux needs an app
   there to receive the TCP stream and upload it: SimConnect gated behind `cfg(windows)`, plus
   signing and notarisation for both app and plugin. That is its own phase (§2, phase 9), not
   part of plugin capture.
3. **There is no CI in any repo.** The design assumes CI builds of `win.xpl`, `mac.xpl` and
   `lin.xpl`. That needs GitHub Actions, and an Apple Developer account for notarisation.
4. **The client does more than display flights.** It creates flights with their summaries
   (`POST /api/v0/flights`), and formats shares and webhooks (`webhook_manager.rs`), which
   drive the Discord posts. All of this moves to the service before the client is stripped,
   or Discord posts and shares stop.
5. **Traffic peers exchange `FlightMetrics`.** `multiplayer.rs` sends its own aircraft as
   `FlightMetrics` JSON, which the client stops building when its monitors are stripped. The
   packet becomes a versioned `TrafficState` (design §6.1), and old and new clients fly
   together during the rollout.
6. **Every user is a Discord user.** `users.discord_id` is `NOT NULL` and 44 places in the
   service read it. Anonymous accounts (design §6.2) need it nullable and every reader
   checked.
7. **Old clients stay around.** The updater spreads new versions over time, so the v0 track
   and flight endpoints keep working until `client_version` shows old clients are gone.
8. **Hot chunks live in Postgres (`bytea`), not on local disk**, so the Cloud Run standby can
   take over a live session. Compacted segments get their own R2 bucket or prefix; today's
   bucket is `butterlog-screenshots`.

## 2. Phases

Each phase ships on its own. Nothing user-visible changes until phase 5.

| Phase | Ships | Rollout | Exit criterion |
|---|---|---|---|
| 0. Foundations | `butterlog-service/wire/` crate; GitHub Actions building the plugin for Windows | Internal | Service image and app both build against `butterlog-wire` |
| 1. Measure | Encoder in `butterlog-wire`; replay of local SQLite `.db` logs; plugin sampler spike logging frame timing beside Web API rate and jitter | Internal | Real bytes per flight-hour; agreed quanta table; design §4.1 "verify" items settled |
| 2. Ingest, shadow mode (MSFS) | Service: session and chunk endpoints, schemas by hash, hot chunks in `bytea`, compaction to R2. App: SimConnect `CHANGED\|TAGGED` sampler, chunker, bounded spool, uploader, running beside `live_track.rs` | Hidden setting, on for the author; then an opt-in "beta telemetry" toggle | A week of flights with no `seq` gaps; spool replay proven by killing the app mid-flight and taking the service down |
| 3. Server analysis with parity check | Normalisers and `flight_analyzer.rs` moved into the service; derived products cached by `(flight, analyser_version)`; diff of service vs client summary for every flight uploaded both ways; web reads derived products, falls back to `flight_track_points`. Session-open ack returns the resolved ICAO type and airline; peers switch to `TrafficState`, new clients still sending and accepting `FlightMetrics` for old peers | Beta users | Parity within agreed tolerances (times, airports, runway, landing rate, distance), differences due to higher sample rate written down; old and new clients see each other as traffic, MSFS ↔ X-Plane, with the right models |
| 4. X-Plane plugin capture (Windows) | Flight-loop sampler, SPSC ring, TCP link with handshake, `catch_unwind` on every entry point; app-managed install via `x-plane_install_12.txt`; Web API kept as `xp_webapi` fallback; sim commands over TCP; UDP RREF dropped (UDP stays for traffic only) | Beta toggle; manual plugin zip releases stop | Beta X-Plane flights pass the phase 3 parity check |
| 5. Service owns side effects, then default on | Flight creation, Discord posts, shares, the ended-flights feed and the FlySto export (design §5.3) come from the service; then the new pipeline is on for every logged-in user and the old track upload is off | App 0.4.0, the first release users notice | Discord posts, shares and exports unchanged for new-pipeline flights |
| 6. Live | In-memory buffer per session; SSE with catch-up from the last keyframe; visibility per flight | All users | Viewer joining mid-flight catches up within one keyframe |
| 7. Anonymous accounts | Design §6.2: anonymous accounts, secret link, 14-day sweeper, claim on sign-up, rate limits and quotas | First-run screen; existing logged-out users see it once on update, before anything is sent | Anonymous flight visible through its link; swept after 14 days; kept after claim, both into a new and an existing Discord account |
| 8. History import, then strip the client | First the bulk importer uploads local `.db` logs as `capture=legacy_1hz` sessions, prompting users who aren't signed up to sign up first; one or two releases later the flight UI, analyser, indexes, client export and local-only recording are removed | Announced on Discord: the logbook moves to the web, through the secret link for users who aren't signed up | Imported history visible on the web before the strip release |
| 9. macOS and Linux | SimConnect behind `cfg(windows)`; app and plugin built, signed and notarised; platforms added to `latest.json` | All users | X-Plane flights recorded from macOS and Linux |
| 10. Retire the old path | `flight_track_points` and v0 track endpoints removed; `FlightMetrics` traffic packet dropped; Web API fallback kept or dropped based on `xp_webapi` share | — | No clients older than 0.4.x in `client_version` |

Phase 6 can move later if phase 5 runs long: live already works through `/live` today.
Phase 7 must ship before phase 8: once the client is stripped, the secret link is the only
logbook a user who hasn't signed up has.

The riskiest phases are 3 (the parity check is what makes server analysis trustworthy) and 9
(the most net-new work).

## 3. Anonymous accounts: rollout notes

The design is in design §6.2. What matters for shipping it:

- **Nothing changes for logged-out users until phase 7.** Before it, the app records locally
  as today; the first-run screen and the move to uploading arrive together, and the screen
  comes before the first upload.
- **The importer prompts for sign-up.** Imported into an anonymous account, flights older than
  14 days would be swept at once, so phase 8 asks users who aren't signed up to sign up
  before importing, and imports only the last 14 days if they decline.
- **Swept flights are gone.** The app deletes acked chunks (design §3.5), so the "Kept until"
  date on the link page is the real deadline; the copy says so.

## 4. Proposed answer to design §9

- **Screenshots:** keep the current upload path for now and add `sim_time` to each one, so
  they can be attached to sessions later without a protocol change.
