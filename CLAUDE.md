# Perry Builder (macOS)

Rust-based **sign-only** worker for the Perry ecosystem. Handles `macos-sign`
and `ios-sign` jobs — receives precompiled .app bundles from the Linux worker
(via hub re-queue), performs code signing, packaging, and App Store upload.
Does NOT compile — all compilation happens on the Linux worker.

Runs on oakhost-tart (Hetzner, macOS via Tart VMs).

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
    pipeline.rs        # Build orchestration (sign-only pipeline)
    compiler.rs        # Invokes perry compiler (unused in sign-only mode)
    assets.rs          # Icon/asset processing
    cleanup.rs         # Post-build cleanup
  package/
    macos.rs           # .app bundle → .dmg packaging
    ios.rs             # .app bundle → .ipa packaging
  signing/
    apple.rs           # macOS/iOS code signing + notarization
  publish/
    appstore.rs        # App Store Connect upload
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

## Worker Capabilities
Advertises `["macos-sign", "ios-sign"]` to the hub. Only handles sign-only jobs
dispatched by the hub after Linux worker cross-compiles the app.

## Sign-Only Pipeline
1. Extract precompiled .app bundle from tarball
2. Generate icons: strip alpha channel, generate all required sizes, compile Assets.car with `actool`
3. Merge plist: apply actool's partial plist into Info.plist
4. Embed provisioning profile (App Store builds)
5. Code sign with `rcodesign` (Rust-based, no Keychain needed)
6. Package: create .ipa (iOS) or .dmg (macOS)
7. Upload to App Store Connect (if publish target)

## Icon Pipeline
- Strip alpha channel from source icon (Apple requires no transparency)
- Generate all required sizes (e.g., 60x60@2x, 60x60@3x for iOS)
- Compile asset catalog with `actool` to produce Assets.car
- Merge actool's partial Info.plist into the app's Info.plist

## Concurrent Builds
Supports running multiple builds in parallel (default 2, configurable
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
1. Worker connects to hub WebSocket, sends `worker_hello` with capabilities (`macos-sign`, `ios-sign`) + `max_concurrent`
2. Hub re-queues precompiled bundles from Linux worker as sign-only jobs
3. Worker receives `job_assign`, spawns build as async task
4. Each build: clone golden VM → boot → SCP precompiled bundle → sign → package → upload artifact
5. Progress/logs streamed back to hub in real-time via shared WS channel
6. Multiple builds run concurrently in separate VMs
7. VM cleaned up after each build

## Related Repos
- [hub](https://github.com/PerryTS/hub) — the hub server this worker connects to
- [builder-linux](https://github.com/PerryTS/builder-linux) — Linux worker (handles ALL compilation)
- [perry](https://github.com/PerryTS/perry) — compiler + CLI
