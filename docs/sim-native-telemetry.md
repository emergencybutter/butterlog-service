# Sim-Native Telemetry — design

**Status:** proposed. Nothing here is built yet. Supersedes the track channel of
`docs/live-flight-protocol.md` once implemented; that document stays the record of the
current system.
**Goal:** the server becomes the place a flight lives. The client stops displaying flights
and instead streams nearly everything the simulator exposes, in the simulator's own terms,
over a protocol compact enough for the network and for long-term storage. The web gets
richer visualisation, including watching a flight as it happens. Normalisation and analysis
move server-side, so every past flight can be re-analysed when the analyser improves.

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
  columns: per channel → (tick indices, values)
}
```

- Values are quantised to integers, delta (position/attitude: delta-of-delta) encoded,
  zigzag-varint packed, column by column.
- The whole chunk is zstd-compressed.
- **Keyframes:** every ~60 s a chunk carries the absolute value of *every* channel, so any
  range is decodable with bounded look-back, and a viewer joining mid-flight needs at most
  one keyframe.
- Discrete happenings the sim reports as events (aircraft loaded, crash, sim pause, replay
  entered) travel as timestamped records in the same chunk.

**Size (estimate, to be measured — §8 step 1):** in cruise most channels are absent or zero
deltas. Expect roughly 0.5–1 MB for a 2-hour flight including high-rate phases, against
~4 MB for raw f64 at 1 Hz and ~80 MB at 20 Hz.

### 3.5 Transport

- App → service: WebSocket, or HTTP POST per chunk; each chunk acked by `seq`.
- The app appends every chunk to a local **spool** before sending and deletes it only once
  acked. Offline, app crash, and service downtime all reduce to "send the spool later".
- Idempotent by `(session_id, seq)`, as `flight_track_points` is today by sample epoch.
- Authenticated with the existing Bearer API token.

## 4. X-Plane: the plugin is the primary capture path

X-Plane is a first-class citizen. Supported: **X-Plane 12 only**, on **Windows, macOS and
Linux**.

### 4.1 Options considered

| | Legacy UDP (RREF) | Web API WebSocket (today) | Plugin |
|---|---|---|---|
| Rate | Per dataref, set by us | Fixed by X-Plane (believed ~10 Hz — verify) | Every frame or any divisor |
| Sample timestamp | None — arrival time | None — arrival time, with jitter | Sim time, exact per frame |
| Change filtering | None, resends all every tick | Exact-equality only (verify) | Ours: quantise-then-compare |
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

### 4.2 Plugin architecture

```
flight loop (sim thread): read datarefs → phase → quantise → compare → push tick to SPSC ring
plugin bg thread:         drain ring → length-prefixed frames → localhost TCP → app
```

- **The plugin only samples.** It does not chunk, compress, spool or talk to the service.
  The app remains the single uploader for both sims (token, spool, retries, FlySto export).
- **TCP, not UDP, to the app.** A dropped datagram in a recording that is kept for ever is a
  silent hole. UDP stays for traffic injection, where only the newest value matters.
- **Safety in the sim process.** No allocation and no I/O on the flight loop. Every
  `extern "C"` entry point is wrapped in `catch_unwind` — a panic there takes down X-Plane.
  If the ring is full (app gone), ticks are dropped and counted, never blocked on.
- **Handshake.** On connect the plugin sends its protocol version and capabilities. The app
  rejects an incompatible plugin and falls back to the Web API instead of recording garbage.
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
- Derived products — map track, chart series, events, landing scorecard, stats — are cached
  per `(flight, analyser_version)` and rebuilt on version bump.

### 5.3 Live

- Each active session keeps its last N minutes decoded in memory.
- Viewers subscribe over SSE or WebSocket: catch-up snapshot (from the last keyframe), then
  normalised deltas as chunks arrive.
- The VPS is a single instance: in-process broadcast suffices. If the Cloud Run standby ever
  serves traffic, fan-out goes through Postgres `LISTEN/NOTIFY`.
- Visibility per flight: private, link-only, public; optional broadcast delay.

## 6. Client after the change

| Keeps | Loses |
|---|---|
| Sim adapters (SimConnect, X-Plane plugin link, Web API fallback) | Flight list, flight details, charts, map |
| Encoder, spool, uploader | Screenshot gallery (capture and upload stay) |
| FlySto export — works without login | Aircraft stats, `flight_analyzer.rs` |
| SimVar / dataref introspection, debugging | Airport, runway, ICAO, airline indexes |
| Traffic injection (`multiplayer.rs`), sim commands | Share and webhook formatting |

**Not logged in:** the app records locally and offers FlySto export; nothing is sent.
**Logged in:** everything streams to the service.

FlySto export needs flight boundaries and normalised fields locally. Rather than keep a
second implementation, the decoders, normalisers and segmentation live in a shared crate,
**`butterlog-core`**, used by both the service and the app. The wire stays sim-native; the
FlySto CSV and the web never disagree about what a field means.

## 7. Web UX this enables

- Replay with a scrubber driving map, 3D cockpit view, attitude and charts in sync.
- Landing analysis from 20+ Hz data around touchdown.
- Flight comparison (same approach flown twice).
- Engine monitor view.
- Raw channel explorer for any recorded SimVar or dataref.
- Live follow, with the visibility settings above.

## 8. Plan

1. **Measure.** Write the encoder in `butterlog-core`; replay existing SQLite logs through
   it to measure sizes and tune quanta. In parallel, a sampler spike in the plugin logging
   frame timing and payload per flight, side by side with Web API message rate and jitter
   (settles the "verify" items in §4.1).
2. **Ingest.** Service accepts sessions and chunks and stores raw, alongside the current
   upload path.
3. **Plugin capture.** Flight-loop sampler, TCP link, handshake, CI builds for all three
   platforms, app-managed install.
4. **Normalise and analyse server-side**; web views read derived products.
5. **Live** over SSE/WS from the hot buffer.
6. **Strip the client UI.**
7. **Migrate history:** bulk importer converting local `.db` logs into sessions and
   uploading them (flagged `capture=legacy_1hz`).

## 9. Open questions

- After the service acks a session, does the client keep a local copy (N days) or delete it?
- When client segmentation (for FlySto) and server segmentation disagree, the server is the
  authority — is that acceptable for users who export and also upload?
- Screenshots: keep the current upload path, or attach them to sessions by `sim_time`?
