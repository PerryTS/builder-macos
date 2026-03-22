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
    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if let Some(token) = auth_token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {e}"))?;

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
    },
    UpdateResult {
        success: bool,
        old_version: String,
        new_version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Get the perry compiler version by running `perry --version`.
fn get_perry_version(perry_binary: &str) -> Option<String> {
    std::process::Command::new(perry_binary)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            // Output is like "perry 0.2.179" — extract version part
            s.strip_prefix("perry ").map(|v| v.to_string()).or_else(|| {
                if s.is_empty() { None } else { Some(s) }
            })
        })
}

/// Run the perry update process by updating the golden Tart VM image.
/// Boots the golden image, SSHes in, pulls + rebuilds, copies libs, then saves.
/// Returns (success, new_version_string, error_message).
async fn run_perry_update(perry_binary: &str) -> (bool, String, Option<String>) {
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
        "{} -p '{}' ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 admin@{}",
        sshpass, ssh_password, vm_ip
    );

    // Run the update script inside the VM
    // Build each target separately to keep memory usage reasonable.
    // Use cp to temp + mv to handle hardlinked same-file cases.
    let update_script = concat!(
        "set -e; cd ~/perry; rm -f .git/*.lock .git/packed-refs; rm -rf .git/refs/remotes; git fetch origin; git checkout -B main origin/main; ",
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

    // Replace golden image with the updated VM
    tracing::info!("Replacing golden image with updated VM...");
    if let Err(e) = run_tart_cmd(&["delete", &golden_image]).await {
        tracing::error!("Failed to delete old golden image: {e}");
        let _ = run_tart_cmd(&["delete", update_vm]).await;
        return (false, new_version.clone(), Some(format!("Failed to replace golden image: {e}")));
    }
    if let Err(e) = run_tart_cmd(&["clone", update_vm, &golden_image]).await {
        tracing::error!("Failed to clone update VM to golden: {e}");
        // Critical: golden image is deleted but clone failed!
        return (false, new_version.clone(), Some(format!("CRITICAL: golden image deleted but clone failed: {e}")));
    }
    let _ = run_tart_cmd(&["delete", update_vm]).await;

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
        capabilities: vec!["macos".into(), "ios".into()],
        name: config.worker_name.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "worker".into())
        }),
        secret: config.hub_secret.clone(),
        perry_version,
    };

    write
        .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
        .await
        .map_err(|e| format!("Failed to send worker_hello: {e}"))?;

    tracing::info!("Connected to hub, waiting for jobs...");

    // Track current cancellation flag
    let cancelled = Arc::new(AtomicBool::new(false));

    // Heartbeat state
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(PING_INTERVAL_SECS));
    ping_interval.tick().await; // consume the immediate first tick
    let mut last_message_at = tokio::time::Instant::now();

    // Channel for background update results
    let (update_tx, mut update_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerMessage>();

    loop {
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
                        let _ = write.send(Message::Pong(data)).await;
                        continue;
                    }
                    Message::Pong(_) => {
                        // Hub responded to our ping — connection is alive
                        continue;
                    }
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
                        tracing::info!(job_id = %job_id, "Received job assignment");

                        // Reset cancellation flag
                        cancelled.store(false, Ordering::Relaxed);

                        // Handle the build (this borrows read/write for progress forwarding)
                        handle_build(
                            config,
                            &mut read,
                            &mut write,
                            &cancelled,
                            job_id,
                            manifest,
                            credentials,
                            tarball_url,
                            artifact_upload_url,
                            auth_token,
                        ).await?;

                        // Reset heartbeat timer after build completes
                        last_message_at = tokio::time::Instant::now();
                    }

                    HubMessage::Cancel { job_id } => {
                        tracing::info!(job_id = %job_id, "Cancel request (no active build for this job)");
                    }

                    HubMessage::UpdatePerry {} => {
                        tracing::info!("Received update_perry request from hub");
                        let old_version = get_perry_version(&config.perry_binary).unwrap_or_default();
                        // Spawn update in background so the main loop continues
                        // processing pings and hub messages during the long build.
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

            // Handle background update results
            result = update_rx.recv() => {
                if let Some(result) = result {
                    let _ = write.send(Message::Text(serde_json::to_string(&result).unwrap().into())).await;
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

                if let Err(e) = write.send(Message::Ping(vec![].into())).await {
                    return Err(format!("Failed to send ping: {e}"));
                }
            }
        }
    }

    Ok(())
}

/// Handle a single build job, forwarding progress and listening for cancel messages.
async fn handle_build(
    config: &WorkerConfig,
    read: &mut futures::stream::SplitStream<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>,
    write: &mut futures::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>,
    cancelled: &Arc<AtomicBool>,
    job_id: String,
    manifest: serde_json::Value,
    credentials: serde_json::Value,
    tarball_url: String,
    artifact_upload_url: Option<String>,
    auth_token: Option<String>,
) -> Result<(), String> {
    // Parse manifest and credentials
    let manifest: crate::queue::job::BuildManifest =
        match serde_json::from_value(manifest) {
            Ok(m) => m,
            Err(e) => {
                let err_msg = format!("Invalid manifest: {e}");
                tracing::error!("{err_msg}");
                send_error(write, &job_id, &err_msg).await;
                send_complete(write, &job_id, false, 0.0).await;
                return Ok(());
            }
        };

    let credentials: crate::queue::job::BuildCredentials =
        match serde_json::from_value(credentials) {
            Ok(c) => c,
            Err(e) => {
                let err_msg = format!("Invalid credentials: {e}");
                tracing::error!("{err_msg}");
                send_error(write, &job_id, &err_msg).await;
                send_complete(write, &job_id, false, 0.0).await;
                return Ok(());
            }
        };

    // Download tarball from hub
    let tarball_path = match download_tarball(&tarball_url, &job_id, auth_token.as_deref()).await {
        Ok(p) => p,
        Err(e) => {
            let err_msg = format!("Failed to download tarball: {e}");
            tracing::error!(job_id = %job_id, "{err_msg}");
            send_error(write, &job_id, &err_msg).await;
            send_complete(write, &job_id, false, 0.0).await;
            return Ok(());
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

    // Run build and forward progress
    let build_config = config.clone();
    let cancelled_for_build = cancelled.clone();
    let (build_result_tx, build_result_rx) =
        tokio::sync::oneshot::channel::<Result<PathBuf, String>>();

    // Spawn build task
    tokio::spawn(async move {
        let result = pipeline::execute_build(
            &request,
            &build_config,
            cancelled_for_build,
            progress_tx,
        )
        .await;
        // Clean up downloaded tarball
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

            // Build completion
            result = &mut build_result_rx, if !build_done => {
                build_result = result.ok();
                build_done = true;
                if progress_done {
                    break;
                }
            }

            // Forward progress to hub
            progress = progress_rx.recv(), if !progress_done => {
                match progress {
                    Some(msg) => {
                        let mut json_val = serde_json::to_value(&msg).unwrap_or_default();
                        if let serde_json::Value::Object(ref mut map) = json_val {
                            map.insert("job_id".into(), serde_json::Value::String(job_id.clone()));
                        }
                        let json = serde_json::to_string(&json_val).unwrap();
                        let _ = write.send(Message::Text(json.into())).await;
                    }
                    None => {
                        progress_done = true;
                        if build_done {
                            break;
                        }
                    }
                }
            }

            // Check for hub messages (cancel, ping)
            ws_msg = read.next() => {
                match ws_msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(hub_msg) = serde_json::from_str::<HubMessage>(&text) {
                            if let HubMessage::Cancel { job_id: cancel_id } = hub_msg {
                                if cancel_id == job_id {
                                    tracing::info!(job_id = %job_id, "Cancelling build");
                                    cancelled.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return Err("Hub disconnected during build".into());
                    }
                    _ => {}
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
        let _ = write.send(Message::Text(json.into())).await;
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
                        let _ = write.send(Message::Text(error_msg.into())).await;
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
                let _ = write.send(Message::Text(artifact_msg.into())).await;
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
            let _ = write.send(Message::Text(complete_msg.into())).await;

            tracing::info!(job_id = %job_id, "Build completed in {:.1}s", duration_secs);
        }
        Some(Err(err_msg)) => {
            tracing::error!(job_id = %job_id, error = %err_msg, "Build failed");
            send_error(write, &job_id, &err_msg).await;
            send_complete(write, &job_id, false, duration_secs).await;
        }
        None => {
            tracing::error!(job_id = %job_id, "Build task panicked");
            send_complete(write, &job_id, false, duration_secs).await;
        }
    }

    Ok(())
}

async fn send_error(
    write: &mut futures::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>,
    job_id: &str,
    message: &str,
) {
    let error_json = serde_json::to_string(&ServerMessage::Error {
        code: ErrorCode::InternalError,
        message: message.to_string(),
        stage: None,
    })
    .unwrap();
    let _ = write.send(Message::Text(error_json.into())).await;
}

async fn send_complete(
    write: &mut futures::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>,
    job_id: &str,
    success: bool,
    duration_secs: f64,
) {
    let complete_json = serde_json::to_string(&serde_json::json!({
        "type": "complete",
        "job_id": job_id,
        "success": success,
        "duration_secs": duration_secs,
        "artifacts": []
    }))
    .unwrap();
    let _ = write.send(Message::Text(complete_json.into())).await;
}

fn compute_sha256(path: &PathBuf) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path).map_err(|e| format!("Failed to read artifact: {e}"))?;
    Ok(hex::encode(Sha256::digest(&data)))
}
