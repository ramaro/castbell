# Implementation Plan

## Goal

Binary = webhook server + Chromecast client. Receives HTTP POST with payload, drives Chromecast devices per CLI config. Minimal, clean Rust. Workspace with a server binary (`castbell`) and a test client binary (`castbell-client`).

## Crate choices (researched)

- **cast-sender 0.3.0** — only modern async CASTV2 client on crates.io. Built on **smol** (`async-net`, `async-native-tls`, `smol::lock`, `smol::spawn`). Provides `Receiver`, `App`, `AppId`, `MediaController`, `LoadRequestData`, `MediaInformation`, `StreamType`, `namespace::media::Media`, `namespace::connection::Connection`, `namespace::receiver::Status`.
- `rust_cast` / `chromecast` — blocking `std::net::TcpStream`. Rejected.
- `oxicast 0.0.3` — tokio, but alpha. Rejected.

=> Cast work runs on smol. Webhook runs on tokio. Bridge via `spawn_blocking(smol::block_on(...))`.

## Runtimes

- Webhook: **tokio** (rt-multi-thread) + **axum**.
- Cast: **smol**, booted per-call inside `smol::block_on`. `Receiver::connect` spawns a detached smol receiver task; `disconnect()` at end tears it down. Background tasks (image display window, resume) also use `smol::spawn` on the same global executor.
- No long-lived cast connections: one connect/launch/load/disconnect per webhook hit. Simple, stateless, matches "doorbell" usage (infrequent).

## CLI (clap derive, repeatable flags)

```
castbell \
  --listen 127.0.0.1:8080 \
  --device livingroom:192.168.1.50 \
  --device kitchen:192.168.1.51 \
  --endpoint /doorbell-lr:livingroom \
  --endpoint /alert-all:livingroom,kitchen \
  --action livingroom:doorbell \
  --action kitchen:doorbell --action kitchen:sleep:2s --action kitchen:thumbnail:30s --action kitchen:resume \
  --doorbell-file ./doorbell.mp3 \
  --advertise-url http://192.168.1.10:8080
```

- `--device <alias>:<ip>` — device registry. Port 8009 (cast-sender hardcodes it).
- `--endpoint <path>:<alias,alias>` — route. **Path must be >= 8 chars**, validated at startup (exit non-zero otherwise). Unknown/unconfigured paths -> 404.
- `--action <alias>:<doorbell|thumbnail|thumbnail:<dur>|livestream|sleep:<dur>|resume>` — per-device action subset, in order. Order is preserved as given on the CLI.
  - `doorbell` — play the doorbell sound (mp3).
  - `thumbnail` — display the snapshot persistently (until device idle or the next load).
  - `thumbnail:<dur>` (e.g. `image:30s`) — display the snapshot then auto-clear after the duration by stopping the receiver app (device returns to idle/ambient).
  - `livestream` — play a live video stream (HLS/MP4).
  - `sleep:<dur>` (e.g. `sleep:2s`, `sleep:500ms`) — wait between surrounding loads.
  - `resume` — re-launch the app that was playing before the doorbell and best-effort resume the captured media at its last position. **Only valid as the last action** for a device (enforced at startup).
- `--doorbell-url <url>` / `--doorbell-file <path>` — doorbell sound source. **Mutually exclusive.** `--doorbell-file` loads a local mp3 at startup and serves it at `GET /doorbell.mp3` via `--advertise-url` (so `--advertise-url` is required when using the file form). `--doorbell-url` passes an external URL straight to the cast device (no advertise needed).
- `--livestream-url` — external media URL for the `livestream` action.
- `--advertise-url <base>` — externally-reachable base URL of this server (Chromecast fetches served images / doorbell mp3 from here). Required if any device uses `thumbnail`/`thumbnail:<dur>` or if `--doorbell-file` is set. **Must be reachable from the Chromecast** — `http://127.0.0.1:...` triggers a startup WARN (the device would fetch from its own loopback).
- Every request logged (path, body, status). Default log level `info` (no `RUST_LOG` needed). Startup dumps the full resolved config (devices, actions, routes, urls).

## Payload

UniFi Protect alarm webhook. Relevant structure:

```json
{
  "alarm": {
    "name": "Ring - webhook",
    "triggers": [{
      "key": "ring",
      "eventId": "0a6b57ab-...",
      "sourceEvent": { "id": "0a6b57ab-...", "type": "ring", ... }
    }],
    "thumbnail": "data:image/jpeg;base64,/9j/...=",   // embedded JPEG, NOT a URL (optional — some events omit it)
  },
  "timestamp": 1787498999168
}
```

Key fields:
- `alarm.thumbnail` — **data URI** (`data:image/jpeg;base64,...`), doorbell snapshot. **Optional** — some UniFi Protect event types omit it. Deserialization succeeds regardless; validation only fails (400) when an `thumbnail`/`thumbnail:<dur>` action is configured for the route but the payload has no valid thumbnail.
- `alarm.triggers[0].eventId` (fallback `sourceEvent.id`) — unique event id, used to key the served image.

Problem: Chromecast default media receiver cannot load a `data:` URI as `content_id` — it needs an HTTP URL it can fetch. Solution: decode the base64 JPEG, serve it from this server at `GET /media/<event_id>`, and pass `<advertise_url>/media/<event_id>` to the Chromecast.

## Data model (`config.rs`)

```rust
enum Action {
    Doorbell,
    Thumbnail,
    TimedThumbnail(Duration),   // auto-clears after duration
    Livestream,
    Sleep(Duration),        // wait between loads
    Resume,                 // resume prior media (last action only)
}

struct Config {
    listen: SocketAddr,
    advertise_url: Option<Url>,
    devices: HashMap<Alias, String>,            // alias -> ip
    routes: HashMap<String /*path*/, Vec<Alias>>,
    actions: HashMap<Alias, Vec<Action>>,
    doorbell_url: Option<String>,               // effective URL passed to cast
    doorbell_bytes: Option<Vec<u8>>,            // present when served locally (--doorbell-file)
    livestream_url: Option<Url>,
    images: ThumbnailCache,                         // in-memory: event_id -> JPEG bytes (60s TTL)
}
```

Built once at startup; wrapped in `Arc<Config>`; shared with handlers via axum state.

Startup validation:
- endpoint path length >= 8
- every alias referenced by a route or action is a known `--device`
- `--doorbell-url` and `--doorbell-file` mutually exclusive
- `resume` only valid as the last action for a device
- one doorbell source present if any action uses `doorbell`; livestream url present if any action uses `livestream`
- advertise_url present if any device uses `thumbnail`/`thumbnail:<dur>` **or** `--doorbell-file` is set
- doorbell file is read at startup; read failure is a fatal config error
- advertise_url not localhost (WARN, not fatal — operator may have a valid reason)

Pure helpers (tested): `parse_device`, `parse_endpoint`, `parse_action`, `parse_action_kind`, `parse_duration`, `choose_doorbell`, `doorbell_cast_url`, `advertise_is_local`.

## Cast action (`cast.rs`)

### Planned loads

Actions are converted to a plan of `PlannedLoad` items, preserving CLI order:

```rust
enum PlannedLoad {
    Doorbell { audio_url: String },
    Thumbnail { url: String },
    TimedThumbnail { url: String, display: Duration },
    Livestream { url: String },
    Sleep(Duration),
    Resume,
}
```

`plan_loads(actions, doorbell_url, livestream_url, thumbnail_url) -> Vec<PlannedLoad>` — pure, tested. `Action::Thumbnail` without an image URL is dropped (no-op).

### Execution (`run_device`)

Per device, per hit, inside `tokio::task::spawn_blocking(|| smol::block_on(async { ... }))`:

1. **Capture** (if plan contains `Resume`): before launching our app, read `Receiver::status()` → `find_media_app(apps)` (any running app with Media namespace, prefers Default Media Receiver) → `Connection::Connect` to it → `Media::GetStatus` → capture `(AppId, MediaInformation, current_time)`. Best-effort: logs and continues if nothing found.
2. **Launch** `DefaultMediaReceiver`, create `MediaController`.
3. **Execute plan in order**:
   - `Sleep(d)` → `smol::Timer::after(d)`
   - `Doorbell` / `Image` / `Livestream` → `mc.load(load_request_for(load))` with `autoplay: true` (no explicit `start()` — the receiver rejects a follow-up Play as "Invalid Request")
   - `TimedThumbnail` → load the image, then **spawn a detached background task**: sleep `display` → `stop_app` (clears image) → run any trailing loads (e.g. `Resume`) → `disconnect`. Foreground returns Ok immediately so the webhook responds 200 fast and the 20s webhook timeout doesn't trip for >20s displays.
   - `Resume` (inline, when no `TimedThumbnail` precedes it) → `do_resume(rx, &resume_media, ip)`: re-launch the original `AppId`, `load(captured_media, current_time)`. Non-fatal if the load is rejected (custom receivers like YouTube ignore generic `Media.Load`).

`load_request_for(load) -> LoadRequestData` — pure, tested. Builds `MediaInformation` with `content_id`, `content_type`, `stream_type`, `autoplay: true`.

### Resume support matrix

| App | Re-launch | Media resume | Why |
|-----|-----------|-------------|-----|
| Default Media Receiver (HLS/MP4/MP3) | ✓ | ✓ at `current_time` | Generic `Media.Load` works |
| BBC Sounds | ✓ | ✓ (station resumes) | BBC receiver accepts generic `Media.Load` |
| YouTube | ✓ | ✗ (lands in app, not specific video) | YouTube uses proprietary MDX protocol; `Media.Load` rejected but app re-launches |
| Other proprietary apps | ✓ | ✗ (likely) | Same — proprietary protocols |

Resume capture requires `Connection::Connect` to the foreign app before querying it (cast-sender's `launch_app` does this automatically for apps it launches, but not for already-running foreign apps).

Pure helpers (tested): `find_media_app`, `build_resume_load`.

## Web (`web.rs`)

- Build `Router` with one `POST` route per configured path, `with_state(Arc<Config>)`.
- Fallback handler -> 404 (covers unknown + missing endpoints).
- `GET /media/<id>` — serves cached JPEG bytes from `Config.thumbnails`. Content-Type `image/jpeg`. 404 if unknown/expired id.
- `GET /doorbell.mp3` — serves `Config.doorbell_bytes` (loaded from `--doorbell-file` at startup). Content-Type `audio/mpeg`. 404 if doorbell not configured via file.
- Handler: parse JSON (UniFi Protect alarm), extract `alarm.thumbnail` (data URI, optional) + event id, decode base64 if any device needs image, store bytes in `Config.thumbnails`, `tracing::info!` request + dispatch plan, fan out devices concurrently (`futures::join_all` over one `spawn_blocking` per device), aggregate per-device results -> 200 if all ok, 500 with error list. Image eviction: delayed — a background `tokio::spawn` removes the image after the 60s TTL (not immediately after cast tasks complete, since the Chromecast fetches the poster lazily after the LOAD acks).

Pure helpers (tested): `needs_thumbnail` (recognizes `Image` + `TimedThumbnail`), `thumbnail_url`, `collect_errors`.

## Logging

- Default level `info` (falls back to `EnvFilter::new("info")` when `RUST_LOG` unset). `debug` shows full payload body. `warn` shows only failures.
- Startup: dumps resolved config (listen, advertise_url, doorbell_url, doorbell_file_loaded, livestream_url, every device with actions, every route with devices). WARN if advertise_url is localhost.
- Per request: `webhook request` (path, len) → `dispatching` (path, devices, need_thumbnail, thumbnail_url, doorbell_url, livestream_url) → per-device `cast connected` / `cast load` (ip, action, content_id) / `cast sleep` / `cast image display window` / `cast resume` / `cast done` → `device ok` or `device failed` (alias, ip, err) → `ok` (200) or `cast errors` (500, error list).
- Per cast stage: `cast connect failed` / `cast launch failed` / `cast load failed` / `cast resume load` / `cast resume: media load rejected` (non-fatal for YouTube etc.)

## File layout

Cargo workspace at the repo root:

```
Cargo.toml          # workspace root
shell.nix           # OpenSSL + pkg-config + toolchain
crates/
  castbell/      # the webhook + chromecast server binary
    Cargo.toml
    src/
      main.rs      # clap parse -> Config -> tracing init -> startup log -> start axum
      config.rs    # CLI -> Config + validation (pure: parse_device/endpoint/action, parse_duration, choose_doorbell, doorbell_cast_url, advertise_is_local)
      payload.rs   # UniFi Protect alarm struct + data URI parse/decode (pure: parse_data_uri, resolve_event_id; thumbnail optional)
      media.rs     # ThumbnailCache + pure expired(at, now, ttl); pub TTL=60s
      cast.rs      # run_device (I/O) + pure: PlannedLoad, plan_loads, load_request_for, find_media_app, build_resume_load
                    #   I/O helpers: capture_resume_media, do_resume
      web.rs       # router + handler + logging + pure: needs_thumbnail, thumbnail_url, collect_errors
  castbell-client/   # tiny CLI that mimics a UniFi Protect alarm payload, for manual testing
    Cargo.toml
    src/main.rs
```

~6 small files in the server crate. Pure logic extracted into tested functions; I/O (`run_device`, `webhook`, `capture_resume_media`, `do_resume`, `build` file read, `main`) kept separate. **86 tests** inline (`#[cfg(test)]`) + router integration via `tower::oneshot` (`tower` dev-dep). The client is intentionally minimal (clap + ureq + base64 + serde_json), no tests.

## Dependencies

```toml
# castbell
tokio        { features = ["rt-multi-thread", "macros", "net"] }
axum
clap         { features = ["derive"] }
serde        { features = ["derive"] }
serde_json
url          { features = ["serde"] }
tracing
tracing-subscriber { features = ["env-filter"] }
futures
base64
cast-sender
smol
smol-timeout

[dev-dependencies]
tower = { version = "0.5", features = ["util"] }

# castbell-client
clap         { features = ["derive"] }
ureq         { default-features = false, features = ["tls"] }
base64
serde_json
```

(cast-sender pulls async-net, async-native-tls → openssl-sys, prost, etc.)

## Notes / risks

- **Build needs OpenSSL + pkg-config** (for `async-native-tls` via cast-sender). `shell.nix` provides them: `nix-shell --command "cargo build"` (non-pure for network access on first build; `--pure` works once crates are cached).
- **TLS**: cast-sender sets `danger_accept_invalid_certs` internally — no cert config needed.
- **Livestream**: default media receiver supports HLS (`application/x-mpegurl`) and MP4. RTSP unsupported. Livestream URL should be HLS/MP4.
- **No persistence, no mDNS discovery** — devices specified by address per spec.
- **`--advertise-url` must be reachable from the Chromecast**, not from the server. `http://127.0.0.1:...` triggers a startup WARN (the device would fetch from its own loopback). Use the server's LAN IP.
- **No explicit `start()` after load.** `autoplay = true` starts playback on LOAD; a follow-up `Media::Play` is rejected as "Invalid Request".
- **Single media session.** Default receiver holds one session; each `load` replaces the prior. cast-sender 0.3.0 serializes `MetadataType` with `#[serde(untagged)]` so the `metadataType` discriminator is missing → audio artwork not rendered. Therefore `doorbell` + `thumbnail` on the same device can't show both simultaneously — the operator sequences them via `sleep:<dur>` between loads. The SPEC's per-device action subsets let operators dedicate display devices vs speaker devices.
- **20s webhook timeout** (`smol-timeout`) covers only the load phase. A final `thumbnail:<dur>` display window + any trailing `resume` run in a detached smol background task so a >20s display does not trip the timeout or block the webhook response.
- **Image eviction is delayed.** The cached JPEG is removed after the 60s TTL via a background `tokio::spawn` (not immediately after cast tasks complete), because the Chromecast fetches the image lazily after the LOAD acks.
- **`resume` is best-effort.** Captures from any Media-namespace app (prefers Default Media Receiver, falls back to custom apps like BBC Sounds / YouTube). Re-launches the original app and attempts `Media.Load` at the captured `current_time`. Works fully for Default Media Receiver + BBC Sounds. YouTube rejects the load (proprietary protocol) but the app is still re-launched — user lands in the right app. Load failure is non-fatal (logged as info, not error).

## Manual testing with `castbell-client`

`castbell-client` builds a UniFi Protect alarm payload and POSTs it.

```
# start the server
nix-shell --command "cargo run -p castbell -- \
  --listen 0.0.0.0:8080 \
  --device hall:192.168.1.201 \
  --device kitchen:192.168.1.202 \
  --endpoint /doorbell:hall,kitchen \
  --action hall:doorbell \
  --action kitchen:doorbell --action kitchen:sleep:3s --action kitchen:thumbnail:20s --action kitchen:resume \
  --doorbell-file ./doorbell.mp3 \
  --advertise-url http://192.168.1.2:8080"

# print the payload without sending
nix-shell --command "cargo run -p castbell-client -- --print"

# POST to the server with a real JPEG thumbnail
nix-shell --command "cargo run -p castbell-client -- \
  --url http://192.168.1.2:8080/doorbell \
  --event-id test-1 \
  --thumbnail ~/Downloads/dogs.jpg"
```

Flags: `--url` (target endpoint, required unless `--print`), `--event-id` (default `test-event-<ms>`), `--thumbnail <jpeg-file>` (default: small synthetic stand-in, not a valid JPEG — pass a real JPEG for end-to-end Chromecast tests), `--name`, `--key`, `--print` (print payload, don't send).

### Verified scenarios

- **Doorbell only** (`--action hall:doorbell`): ring plays, webhook 200 in ~2s.
- **Doorbell + thumbnail + resume** (`--action kitchen:doorbell --action kitchen:sleep:3s --action kitchen:thumbnail:20s --action kitchen:resume`): ring → 3s wait → snapshot displays 20s → auto-clears → prior app re-launched (BBC Sounds resumes station; YouTube re-opens but video doesn't resume). Webhook 200 in ~7s (load phase only; display + resume run in background).
- **Multi-device** (`--endpoint /doorbell:hall,kitchen`): both devices dispatched concurrently via `join_all`.
- **Unknown endpoint** → 404. **Bad payload** → 400. **Cast failure** → 500 with per-device error list.
- **Localhost advertise-url** → startup WARN.
