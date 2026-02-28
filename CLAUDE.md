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

## How It Works
1. Worker connects to hub WebSocket, sends `worker_hello` with capabilities
2. Hub assigns jobs → worker receives `job_assign` with manifest + tarball path
3. Worker runs build pipeline: compile → package → sign → (optional) publish
4. Progress/logs streamed back to hub in real-time via WS
5. Finished artifacts registered with hub for CLI download

## Related Repos
- [hub](https://github.com/PerryTS/hub) — the hub server this worker connects to
- [perry](https://github.com/PerryTS/perry) — compiler + CLI
