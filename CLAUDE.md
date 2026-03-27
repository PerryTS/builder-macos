# Perry Builder (macOS)

Rust-based build worker for the Perry ecosystem. Connects to perry-hub via
WebSocket, receives build jobs, compiles perry projects into native macOS/iOS/Android
apps, handles code signing, and reports artifacts back.

## Tech Stack
- **Rust** (tokio async runtime)
- WebSocket client: tokio-tungstenite
- HTTP client: reqwest

## Project Structure
```
src/
  main.rs              # Entry point, CLI args
  worker.rs            # WebSocket connection to hub, job dispatch loop
  config.rs            # Configuration
  lib.rs               # Library root
  build/
    pipeline.rs        # Build orchestration
    compiler.rs        # Invokes perry compiler
    assets.rs          # Icon/asset processing
    cleanup.rs         # Post-build cleanup
  package/
    macos.rs           # .app bundle creation
    ios.rs             # .ipa packaging
    android.rs         # .apk/.aab packaging
  signing/
    apple.rs           # macOS/iOS code signing + notarization
    android.rs         # Android keystore signing
  publish/
    appstore.rs        # App Store Connect upload
    playstore.rs       # Google Play upload
  queue/
    job.rs             # Job types and state
  ws/
    messages.rs        # WebSocket protocol message types
```

## Build & Run
```sh
# Build
cargo build --release

# Run (connects to hub at default ws://localhost:3457)
./target/release/perry-ship
```

## Concurrent Builds
The worker supports running multiple builds in parallel (default 2, configurable
via `PERRY_MAX_CONCURRENT_BUILDS`). Each build runs in its own Tart VM clone.
Builds are spawned as tokio tasks with a shared WS write channel. The
`worker_hello` message advertises `max_concurrent` to the hub for slot-based dispatch.

## Code Signing
- **macOS/iOS**: Uses `rcodesign` (Rust-based, no macOS Security.framework dependency).
  Accepts `.p12` directly — no Keychain import needed. Falls back to Apple's `codesign`
  for ad-hoc signing.
- **macOS App Store**: Embeds provisioning profile as `Contents/embedded.provisionprofile`
  (required for TestFlight). Profile passed via `provisioning_profile_base64` in credentials.
- **Notarization**: Sign app → notarize DMG → staple → recreate DMG → sign DMG → notarize → staple.

## Environment Variables
- `PERRY_HUB_URL` — Hub WebSocket URL (default: `wss://hub.perryts.com/ws`)
- `PERRY_HUB_SECRET` — Auth secret for hub connection
- `PERRY_WORKER_NAME` — Worker name (default: hostname)
- `PERRY_MAX_CONCURRENT_BUILDS` — Max parallel builds (default: 2)
- `PERRY_TART_IMAGE` — Golden Tart VM image name
- `PERRY_TART_SSH_PASSWORD` — SSH password for Tart VMs

## How It Works
1. Worker connects to hub WebSocket, sends `worker_hello` with capabilities + `max_concurrent`
2. Hub assigns jobs → worker receives `job_assign`, spawns build as async task
3. Each build: clone golden VM → boot → SCP tarball → compile → package → sign → upload artifact
4. Progress/logs streamed back to hub in real-time via shared WS channel
5. Multiple builds run concurrently in separate VMs
6. VM cleaned up after each build

## Related Repos
- [hub](https://github.com/PerryTS/hub) — the hub server this worker connects to
- [perry](https://github.com/PerryTS/perry) — compiler + CLI
