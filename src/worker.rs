use crate::build::pipeline::{self, BuildRequest};
use crate::config::WorkerConfig;
use crate::ws::messages::{ErrorCode, ServerMessage};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// How often to send a WebSocket ping to the hub (seconds).
const PING_INTERVAL_SECS: u64 = 30;

/// If no message (including pong) is received for this long, consider the connection dead.
const STALE_TIMEOUT_SECS: u64 = 90;

/// Upload a built artifact to the hub via HTTP POST (base64-encoded body).
async fn upload_artifact(
    url: &str,
    artifact_path: &std::path::Path,
    artifact_name: &str,
    sha256: &str,
    target: &str,
    auth_token: Option<&str>,
) -> Result<serde_json::Value, String> {
    use base64::Engine;
    let data =
        std::fs::read(artifact_path).map_err(|e| format!("Failed to read artifact: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);

    let client = reqwest::Client::new();
    let mut req = client
        .post(url)
        .header("Content-Type", "text/plain")
        .header("x-artifact-name", artifact_name)
        .header("x-artifact-sha256", sha256)
        .header("x-artifact-target", target);
    if let Some(token) = auth_token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .body(b64)
        .send()
        .await
        .map_err(|e| format!("Artifact upload failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Hub returned HTTP {status} for artifact upload: {body}"));
    }

    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Failed to parse upload response: {e}"))
}

/// Download a base64-encoded tarball from the hub and write the decoded bytes to a temp file.
async fn download_tarball(url: &str, job_id: &str, auth_token: Option<&str>) -> Result<PathBuf, String> {
    tracing::info!(url = %url, "Downloading tarball");
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;
    let mut req = client.get(url);
    if let Some(token) = auth_token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e} (url={url}, is_builder={}, is_connect={}, is_timeout={})", e.is_builder(), e.is_connect(), e.is_timeout()))?;

    if !resp.status().is_success() {
        return Err(format!("Hub returned HTTP {}", resp.status()));
    }

    let b64_text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read tarball response body: {e}"))?;

    use base64::Engine;
    let tarball_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_text.trim())
        .map_err(|e| format!("Failed to base64-decode tarball: {e}"))?;

    let dl_dir = std::env::temp_dir().join("perry-worker-dl");
    std::fs::create_dir_all(&dl_dir)
        .map_err(|e| format!("Failed to create download dir: {e}"))?;

    let tarball_path = dl_dir.join(format!("{job_id}.tar.gz"));
    std::fs::write(&tarball_path, &tarball_bytes)
        .map_err(|e| format!("Failed to write tarball to disk: {e}"))?;

    Ok(tarball_path)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubMessage {
    JobAssign {
        job_id: String,
        manifest: serde_json::Value,
        credentials: serde_json::Value,
        tarball_url: String,
        #[serde(default)]
        artifact_upload_url: Option<String>,
        #[serde(default)]
        auth_token: Option<String>,
    },
    Cancel {
        job_id: String,
    },
    UpdatePerry {},
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerMessage {
    WorkerHello {
        capabilities: Vec<String>,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        perry_version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_concurrent: Option<usize>,
    },
    UpdateResult {
        success: bool,
        old_version: String,
        new_version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Get the perry compiler version.
/// First tries running the binary directly; if that fails (e.g. the binary is
/// inside a Tart VM), reads from ~/.perry-version which is written after each update.
fn get_perry_version(perry_binary: &str) -> Option<String> {
    // Try running the binary directly (works for Linux/Windows workers)
    if let Some(v) = std::process::Command::new(perry_binary)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            s.strip_prefix("perry ").map(|v| v.to_string()).or_else(|| {
                if s.is_empty() { None } else { Some(s) }
            })
        })
    {
        return Some(v);
    }

    // Fallback: read cached version file (written by golden VM update)
    let version_file = std::env::var("HOME")
        .map(|h| format!("{h}/.perry-version"))
        .unwrap_or_else(|_| "/tmp/.perry-version".into());
    std::fs::read_to_string(version_file).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Run the perry update process by updating the golden Tart VM image.
/// Boots the golden image, SSHes in, pulls + rebuilds, copies libs, then saves.
/// Returns (success, new_version_string, error_message).
async fn run_perry_update(perry_binary: &str) -> (bool, String, Option<String>) {
    // Prevent concurrent updates — use a simple file lock
    let lock_path = std::env::temp_dir().join("perry-update.lock");
    if lock_path.exists() {
        tracing::info!("Update already in progress (lock file exists), skipping");
        return (false, String::new(), Some("Update already in progress".into()));
    }
    let _ = std::fs::write(&lock_path, "");
    // Ensure lock is removed on exit
    struct LockGuard(std::path::PathBuf);
    impl Drop for LockGuard { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
    let _lock = LockGuard(lock_path);

    let golden_image = std::env::var("PERRY_TART_IMAGE")
        .unwrap_or_else(|_| "perry-builder-golden".into());
    let ssh_password = std::env::var("PERRY_TART_SSH_PASSWORD")
        .unwrap_or_else(|_| "admin".into());
    let sshpass = std::env::var("HOME")
        .map(|h| format!("{h}/bin/sshpass"))
        .unwrap_or_else(|_| "sshpass".into());

    tracing::info!(image = %golden_image, "Updating perry in golden Tart VM...");

    // Clone golden image to a temporary update VM
    let update_vm = "perry-update-tmp";
    let clone = run_tart_cmd(&["clone", &golden_image, update_vm]).await;
    if let Err(e) = clone {
        return (false, String::new(), Some(format!("Failed to clone golden image: {e}")));
    }

    // Boot the update VM
    let mut vm_child = match tokio::process::Command::new("tart")
        .args(["run", update_vm, "--no-graphics"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = run_tart_cmd(&["delete", update_vm]).await;
            return (false, String::new(), Some(format!("Failed to start update VM: {e}")));
        }
    };

    // Wait for IP
    let vm_ip = match wait_for_vm_ip(update_vm, 120).await {
        Some(ip) => ip,
        None => {
            let _ = vm_child.kill().await;
            let _ = run_tart_cmd(&["stop", update_vm]).await;
            let _ = run_tart_cmd(&["delete", update_vm]).await;
            return (false, String::new(), Some("Timed out waiting for update VM IP".into()));
        }
    };

    tracing::info!(ip = %vm_ip, "Update VM booted");

    let ssh_base = format!(
        "{} -p '{}' ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 admin@{}",
        sshpass, ssh_password, vm_ip
    );

    // Run the update script inside the VM
    // Build each target separately to keep memory usage reasonable.
    // Use cp to temp + mv to handle hardlinked same-file cases.
    let update_script = concat!(
        "set -e; cd ~/perry; find .git -name \"*.lock\" -delete 2>/dev/null || true; rm -f .git/packed-refs; rm -rf .git/refs/remotes; git checkout -- . 2>/dev/null || true; git clean -fd 2>/dev/null || true; git fetch origin; git checkout -B main origin/main; ",
        "cargo build --release -p perry; ",
        "cargo build --release -p perry-runtime -p perry-stdlib; ",
        "cargo build --release -p perry-ui-macos; ",
        "cargo build --release -p perry-ui-ios --target aarch64-apple-ios; ",
        "cargo build --release -p perry-runtime --target aarch64-apple-ios; ",
        "cargo build --release -p perry-stdlib --target aarch64-apple-ios; ",
        "cp target/release/perry ~/bin/perry.tmp && mv -f ~/bin/perry.tmp ~/bin/perry; ",
        "cp target/release/libperry_runtime.a ~/bin/libperry_runtime.a.tmp && mv -f ~/bin/libperry_runtime.a.tmp ~/bin/libperry_runtime.a; ",
        "cp target/release/libperry_stdlib.a ~/bin/libperry_stdlib.a.tmp && mv -f ~/bin/libperry_stdlib.a.tmp ~/bin/libperry_stdlib.a; ",
        "cp target/release/libperry_ui_macos.a ~/bin/libperry_ui_macos.a.tmp && mv -f ~/bin/libperry_ui_macos.a.tmp ~/bin/libperry_ui_macos.a; ",
        "cp target/aarch64-apple-ios/release/libperry_runtime.a ~/bin/libperry_runtime_ios.a.tmp && mv -f ~/bin/libperry_runtime_ios.a.tmp ~/bin/libperry_runtime_ios.a; ",
        "cp target/aarch64-apple-ios/release/libperry_stdlib.a ~/bin/libperry_stdlib_ios.a.tmp && mv -f ~/bin/libperry_stdlib_ios.a.tmp ~/bin/libperry_stdlib_ios.a; ",
        "cp target/aarch64-apple-ios/release/libperry_ui_ios.a ~/bin/libperry_ui_ios.a.tmp && mv -f ~/bin/libperry_ui_ios.a.tmp ~/bin/libperry_ui_ios.a; ",
        // Install Apple WWDR intermediate CAs (G3, G6) so codesign can validate cert chains.
        // These get lost when the golden image is rebuilt from a clone.
        "curl -sL https://www.apple.com/certificateauthority/AppleWWDRCAG3.cer -o /tmp/wwdr-g3.cer && sudo security add-certificates -k /Library/Keychains/System.keychain /tmp/wwdr-g3.cer 2>/dev/null || true; ",
        "curl -sL https://www.apple.com/certificateauthority/AppleWWDRCAG6.cer -o /tmp/wwdr-g6.cer && sudo security add-certificates -k /Library/Keychains/System.keychain /tmp/wwdr-g6.cer 2>/dev/null || true; ",
        "rm -f /tmp/wwdr-g3.cer /tmp/wwdr-g6.cer; ",
        "~/bin/perry --version"
    );

    let build_cmd = format!("{} '{}'", ssh_base, update_script);
    let build = tokio::process::Command::new("bash")
        .args(["-c", &build_cmd])
        .output()
        .await;

    let new_version;
    match &build {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            if !o.status.success() {
                tracing::error!("VM update failed:\nstdout: {stdout}\nstderr: {stderr}");
                // Cleanup: stop and delete the update VM (don't save)
                let _ = run_tart_cmd(&["stop", update_vm]).await;
                let _ = vm_child.kill().await;
                let _ = run_tart_cmd(&["delete", update_vm]).await;
                return (false, String::new(), Some(format!("Update build failed: {stderr}")));
            }
            // Extract version from the last line of stdout
            new_version = stdout.lines().last()
                .and_then(|l| l.strip_prefix("perry "))
                .unwrap_or("")
                .trim()
                .to_string();
            tracing::info!(version = %new_version, "Perry updated in VM");
        }
        Err(e) => {
            let _ = run_tart_cmd(&["stop", update_vm]).await;
            let _ = vm_child.kill().await;
            let _ = run_tart_cmd(&["delete", update_vm]).await;
            return (false, String::new(), Some(format!("SSH to update VM failed: {e}")));
        }
    }

    // Shut down VM gracefully to save state
    let shutdown_cmd = format!("{} 'sudo shutdown -h now'", ssh_base);
    let _ = tokio::process::Command::new("bash")
        .args(["-c", &shutdown_cmd])
        .output()
        .await;

    // Wait for VM to stop
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    let _ = run_tart_cmd(&["stop", update_vm]).await;
    let _ = vm_child.kill().await;

    // Replace golden image with the updated VM — SAFE: backup first, restore on failure
    tracing::info!("Replacing golden image with updated VM...");
    let backup_name = format!("{golden_image}-backup");
    // Step 1: Backup golden → golden-backup
    let _ = run_tart_cmd(&["delete", &backup_name]).await; // remove stale backup
    if let Err(e) = run_tart_cmd(&["clone", &golden_image, &backup_name]).await {
        tracing::warn!("Failed to backup golden image (continuing anyway): {e}");
    }
    // Step 2: Delete golden
    if let Err(e) = run_tart_cmd(&["delete", &golden_image]).await {
        tracing::error!("Failed to delete old golden image: {e}");
        let _ = run_tart_cmd(&["delete", update_vm]).await;
        let _ = run_tart_cmd(&["delete", &backup_name]).await;
        return (false, new_version.clone(), Some(format!("Failed to replace golden image: {e}")));
    }
    // Step 3: Clone update → golden
    if let Err(e) = run_tart_cmd(&["clone", update_vm, &golden_image]).await {
        tracing::error!("Clone failed, restoring from backup: {e}");
        // Restore golden from backup
        if let Err(e2) = run_tart_cmd(&["clone", &backup_name, &golden_image]).await {
            tracing::error!("CRITICAL: restore from backup also failed: {e2}");
        }
        let _ = run_tart_cmd(&["delete", update_vm]).await;
        let _ = run_tart_cmd(&["delete", &backup_name]).await;
        return (false, new_version.clone(), Some(format!("Failed to replace golden image: {e}")));
    }
    // Step 4: Cleanup
    let _ = run_tart_cmd(&["delete", update_vm]).await;
    let _ = run_tart_cmd(&["delete", &backup_name]).await;

    // Cache version on host so get_perry_version works without booting the VM
    let version_file = std::env::var("HOME")
        .map(|h| format!("{h}/.perry-version"))
        .unwrap_or_else(|_| "/tmp/.perry-version".into());
    let _ = std::fs::write(&version_file, &new_version);

    tracing::info!(version = %new_version, "Golden image updated successfully");
    (true, new_version, None)
}

async fn run_tart_cmd(args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("tart")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("tart command failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(format!("tart {} failed: {stderr}", args.join(" ")));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn wait_for_vm_ip(vm_name: &str, timeout_secs: u64) -> Option<String> {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed().as_secs() > timeout_secs {
            return None;
        }
        if let Ok(ip) = run_tart_cmd(&["ip", vm_name]).await {
            if !ip.is_empty() {
                return Some(ip);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

pub async fn run_worker(config: WorkerConfig) {
    tracing::info!("Perry-ship worker starting, connecting to hub: {}", config.hub_ws_url);

    let mut backoff_secs = 1u64;

    loop {
        match connect_and_run(&config).await {
            Ok(_) => {
                tracing::info!("Connection to hub closed, reconnecting in 5s...");
                backoff_secs = 1; // reset backoff on clean close
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            Err(e) => {
                tracing::error!("Connection error: {e}, reconnecting in {backoff_secs}s...");
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(60); // exponential backoff, max 60s
            }
        }
    }
}

async fn connect_and_run(config: &WorkerConfig) -> Result<(), String> {
    let (ws_stream, _) = connect_async(&config.hub_ws_url)
        .await
        .map_err(|e| format!("Failed to connect to hub: {e}"))?;

    let (mut write, mut read) = ws_stream.split();

    // Send worker_hello
    let perry_version = get_perry_version(&config.perry_binary);
    let hello = WorkerMessage::WorkerHello {
        capabilities: vec!["macos-sign".into(), "ios-sign".into()],
        name: config.worker_name.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "worker".into())
        }),
        secret: config.hub_secret.clone(),
        perry_version,
        max_concurrent: Some(config.max_concurrent_builds),
    };

    write
        .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .map_err(|e| format!("Failed to send worker_hello: {e}"))?;

    tracing::info!(max_concurrent = config.max_concurrent_builds, "Connected to hub, waiting for jobs...");

    // Shared WS write channel — build tasks send messages here
    let (ws_tx, mut ws_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

    // Spawn dedicated WS writer task — prevents WS backpressure from blocking main loop
    let ws_writer_error = Arc::new(std::sync::Mutex::new(None::<String>));
    let ws_writer_err_clone = ws_writer_error.clone();
    tokio::spawn(async move {
        while let Some(msg) = ws_rx.recv().await {
            if let Err(e) = write.send(msg).await {
                *ws_writer_err_clone.lock().unwrap() = Some(format!("WS write failed: {e}"));
                break;
            }
        }
    });

    // Per-job cancellation flags
    let cancel_flags: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let active_builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Channel for background update results
    let (update_tx, mut update_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerMessage>();

    // Heartbeat state
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(PING_INTERVAL_SECS));
    ping_interval.tick().await; // consume the immediate first tick
    let mut last_message_at = tokio::time::Instant::now();

    loop {
        // Check if WS writer died
        if let Some(err) = ws_writer_error.lock().unwrap().take() {
            return Err(err);
        }

        tokio::select! {
            biased;

            // Incoming WebSocket message
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => {
                        return Err(format!("WebSocket error: {e}"));
                    }
                    None => break, // stream ended
                };

                last_message_at = tokio::time::Instant::now();

                let text = match msg {
                    Message::Text(t) => t,
                    Message::Ping(data) => {
                        let _ = ws_tx.send(Message::Pong(data));
                        continue;
                    }
                    Message::Pong(_) => continue,
                    Message::Close(_) => break,
                    _ => continue,
                };

                let hub_msg: HubMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("Failed to parse hub message: {e}");
                        continue;
                    }
                };

                match hub_msg {
                    HubMessage::JobAssign {
                        job_id,
                        manifest,
                        credentials,
                        tarball_url,
                        artifact_upload_url,
                        auth_token,
                    } => {
                        let n = active_builds.load(Ordering::Relaxed);
                        tracing::info!(job_id = %job_id, active = n, "Received job assignment");

                        // Create per-job cancellation flag
                        let cancelled = Arc::new(AtomicBool::new(false));
                        cancel_flags.lock().unwrap().insert(job_id.clone(), cancelled.clone());

                        // Spawn build as a concurrent task
                        let build_config = config.clone();
                        let build_ws_tx = ws_tx.clone();
                        let build_active = active_builds.clone();
                        let build_cancel_flags = cancel_flags.clone();
                        build_active.fetch_add(1, Ordering::Relaxed);

                        tokio::spawn(async move {
                            handle_build(
                                &build_config,
                                &build_ws_tx,
                                &cancelled,
                                job_id.clone(),
                                manifest,
                                credentials,
                                tarball_url,
                                artifact_upload_url,
                                auth_token,
                            ).await;

                            build_active.fetch_sub(1, Ordering::Relaxed);
                            build_cancel_flags.lock().unwrap().remove(&job_id);
                        });
                    }

                    HubMessage::Cancel { job_id } => {
                        if let Some(flag) = cancel_flags.lock().unwrap().get(&job_id) {
                            tracing::info!(job_id = %job_id, "Cancelling build");
                            flag.store(true, Ordering::Relaxed);
                        } else {
                            tracing::info!(job_id = %job_id, "Cancel request (no active build for this job)");
                        }
                    }

                    HubMessage::UpdatePerry {} => {
                        let n = active_builds.load(Ordering::Relaxed);
                        if n > 0 {
                            tracing::info!("Deferring update_perry: {n} builds active");
                        } else {
                            tracing::info!("Received update_perry request from hub");
                            let old_version = get_perry_version(&config.perry_binary).unwrap_or_default();
                            let perry_bin = config.perry_binary.clone();
                            let update_result_tx = update_tx.clone();
                            tokio::spawn(async move {
                                let (success, new_version, error) = run_perry_update(&perry_bin).await;
                                let result = WorkerMessage::UpdateResult {
                                    success,
                                    old_version,
                                    new_version,
                                    error,
                                };
                                let _ = update_result_tx.send(result);
                            });
                        }
                    }
                }
            }

            // Handle background update results
            result = update_rx.recv() => {
                if let Some(result) = result {
                    let _ = ws_tx.send(Message::Text(serde_json::to_string(&result).unwrap().into()));
                }
            }

            // Send periodic ping to detect dead connections
            _ = ping_interval.tick() => {
                let stale_duration = last_message_at.elapsed();
                if stale_duration > std::time::Duration::from_secs(STALE_TIMEOUT_SECS) {
                    return Err(format!(
                        "Connection stale: no message received for {}s, reconnecting",
                        stale_duration.as_secs()
                    ));
                }

                if let Err(e) = ws_tx.send(Message::Ping(vec![].into())) {
                    return Err(format!("Failed to send ping: {e}"));
                }
            }
        }
    }

    Ok(())
}

/// Handle a single build job. Runs as a spawned task, sends all WS messages
/// through the shared `ws_tx` channel.
async fn handle_build(
    config: &WorkerConfig,
    ws_tx: &tokio::sync::mpsc::UnboundedSender<Message>,
    cancelled: &Arc<AtomicBool>,
    job_id: String,
    manifest: serde_json::Value,
    credentials: serde_json::Value,
    tarball_url: String,
    artifact_upload_url: Option<String>,
    auth_token: Option<String>,
) {
    // Parse manifest and credentials
    let manifest: crate::queue::job::BuildManifest =
        match serde_json::from_value(manifest) {
            Ok(m) => m,
            Err(e) => {
                let err_msg = format!("Invalid manifest: {e}");
                tracing::error!("{err_msg}");
                send_error(ws_tx, &job_id, &err_msg);
                send_complete(ws_tx, &job_id, false, 0.0);
                return;
            }
        };

    let credentials: crate::queue::job::BuildCredentials =
        match serde_json::from_value(credentials) {
            Ok(c) => c,
            Err(e) => {
                let err_msg = format!("Invalid credentials: {e}");
                tracing::error!("{err_msg}");
                send_error(ws_tx, &job_id, &err_msg);
                send_complete(ws_tx, &job_id, false, 0.0);
                return;
            }
        };

    // Download tarball from hub
    let tarball_path = match download_tarball(&tarball_url, &job_id, auth_token.as_deref()).await {
        Ok(p) => p,
        Err(e) => {
            let err_msg = format!("Failed to download tarball: {e}");
            tracing::error!(job_id = %job_id, "{err_msg}");
            send_error(ws_tx, &job_id, &err_msg);
            send_complete(ws_tx, &job_id, false, 0.0);
            return;
        }
    };

    let build_target = manifest.targets.first().cloned().unwrap_or_else(|| "macos".into());

    let request = BuildRequest {
        manifest,
        credentials,
        tarball_path,
        job_id: job_id.clone(),
    };

    // Create progress sender that forwards to hub WS
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();

    // Run build in a subtask
    let build_config = config.clone();
    let cancelled_for_build = cancelled.clone();
    let (build_result_tx, build_result_rx) =
        tokio::sync::oneshot::channel::<Result<PathBuf, String>>();

    tokio::spawn(async move {
        let result = pipeline::execute_build(
            &request,
            &build_config,
            cancelled_for_build,
            progress_tx,
        )
        .await;
        std::fs::remove_file(&request.tarball_path).ok();
        let _ = build_result_tx.send(result);
    });

    let start = std::time::Instant::now();
    let mut build_result: Option<Result<PathBuf, String>> = None;

    tokio::pin!(build_result_rx);
    let mut build_done = false;
    let mut progress_done = false;

    loop {
        tokio::select! {
            biased;

            result = &mut build_result_rx, if !build_done => {
                build_result = result.ok();
                build_done = true;
                if progress_done { break; }
            }

            progress = progress_rx.recv(), if !progress_done => {
                match progress {
                    Some(msg) => {
                        let mut json_val = serde_json::to_value(&msg).unwrap_or_default();
                        if let serde_json::Value::Object(ref mut map) = json_val {
                            map.insert("job_id".into(), serde_json::Value::String(job_id.clone()));
                        }
                        let json = serde_json::to_string(&json_val).unwrap();
                        let _ = ws_tx.send(Message::Text(json.into()));
                    }
                    None => {
                        progress_done = true;
                        if build_done { break; }
                    }
                }
            }
        }
    }

    // Drain remaining progress messages
    while let Ok(msg) = progress_rx.try_recv() {
        let mut json_val = serde_json::to_value(&msg).unwrap_or_default();
        if let serde_json::Value::Object(ref mut map) = json_val {
            map.insert("job_id".into(), serde_json::Value::String(job_id.clone()));
        }
        let json = serde_json::to_string(&json_val).unwrap();
        let _ = ws_tx.send(Message::Text(json.into()));
    }

    let duration_secs = start.elapsed().as_secs_f64();

    match build_result {
        Some(Ok(artifact_path)) => {
            let artifact_name = artifact_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("artifact")
                .to_string();
            let metadata = std::fs::metadata(&artifact_path).ok();
            let size = metadata.map(|m| m.len()).unwrap_or(0);
            let sha256 = compute_sha256(&artifact_path).unwrap_or_default();
            let target = build_target.as_str();

            if let Some(ref upload_url) = artifact_upload_url {
                match upload_artifact(upload_url, &artifact_path, &artifact_name, &sha256, target, auth_token.as_deref()).await {
                    Ok(resp) => {
                        tracing::info!(job_id = %job_id, "Artifact uploaded: {}", resp);
                    }
                    Err(e) => {
                        tracing::error!(job_id = %job_id, "Artifact upload failed: {e}");
                        let error_msg = serde_json::to_string(&serde_json::json!({
                            "type": "error",
                            "job_id": job_id,
                            "code": "INTERNAL_ERROR",
                            "message": format!("Artifact upload failed: {e}"),
                        }))
                        .unwrap();
                        let _ = ws_tx.send(Message::Text(error_msg.into()));
                    }
                }
            } else {
                let artifact_msg = serde_json::to_string(&serde_json::json!({
                    "type": "artifact_ready",
                    "job_id": job_id,
                    "target": target,
                    "path": artifact_path.to_string_lossy(),
                    "artifact_name": artifact_name,
                    "sha256": sha256,
                    "size": size,
                }))
                .unwrap();
                let _ = ws_tx.send(Message::Text(artifact_msg.into()));
            }

            std::fs::remove_file(&artifact_path).ok();

            let complete_msg = serde_json::to_string(&serde_json::json!({
                "type": "complete",
                "job_id": job_id,
                "success": true,
                "duration_secs": duration_secs,
                "artifacts": [{
                    "name": artifact_name,
                    "size": size,
                    "sha256": sha256,
                }]
            }))
            .unwrap();
            let _ = ws_tx.send(Message::Text(complete_msg.into()));

            tracing::info!(job_id = %job_id, "Build completed in {:.1}s", duration_secs);
        }
        Some(Err(err_msg)) => {
            tracing::error!(job_id = %job_id, error = %err_msg, "Build failed");
            send_error(ws_tx, &job_id, &err_msg);
            send_complete(ws_tx, &job_id, false, duration_secs);
        }
        None => {
            tracing::error!(job_id = %job_id, "Build task panicked");
            send_complete(ws_tx, &job_id, false, duration_secs);
        }
    }
}

fn send_error(
    ws_tx: &tokio::sync::mpsc::UnboundedSender<Message>,
    job_id: &str,
    message: &str,
) {
    let json = serde_json::to_string(&serde_json::json!({
        "type": "error",
        "job_id": job_id,
        "code": "INTERNAL_ERROR",
        "message": message,
    }))
    .unwrap();
    let _ = ws_tx.send(Message::Text(json.into()));
}

fn send_complete(
    ws_tx: &tokio::sync::mpsc::UnboundedSender<Message>,
    job_id: &str,
    success: bool,
    duration_secs: f64,
) {
    let json = serde_json::to_string(&serde_json::json!({
        "type": "complete",
        "job_id": job_id,
        "success": success,
        "duration_secs": duration_secs,
        "artifacts": []
    }))
    .unwrap();
    let _ = ws_tx.send(Message::Text(json.into()));
}

fn compute_sha256(path: &PathBuf) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path).map_err(|e| format!("Failed to read artifact: {e}"))?;
    Ok(hex::encode(Sha256::digest(&data)))
}
