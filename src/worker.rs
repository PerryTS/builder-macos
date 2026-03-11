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
) -> Result<serde_json::Value, String> {
    use base64::Engine;
    let data =
        std::fs::read(artifact_path).map_err(|e| format!("Failed to read artifact: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);

    let client = reqwest::Client::new();
    let resp = client
        .post(url)
        .header("Content-Type", "text/plain")
        .header("x-artifact-name", artifact_name)
        .header("x-artifact-sha256", sha256)
        .header("x-artifact-target", target)
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
async fn download_tarball(url: &str, job_id: &str) -> Result<PathBuf, String> {
    let resp = reqwest::get(url)
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
    },
    Cancel {
        job_id: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkerMessage {
    WorkerHello {
        capabilities: Vec<String>,
        name: String,
    },
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
    let hello = WorkerMessage::WorkerHello {
        capabilities: vec!["macos".into(), "ios".into(), "android".into()],
        name: config.worker_name.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "worker".into())
        }),
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
                        ).await?;

                        // Reset heartbeat timer after build completes
                        last_message_at = tokio::time::Instant::now();
                    }

                    HubMessage::Cancel { job_id } => {
                        tracing::info!(job_id = %job_id, "Cancel request (no active build for this job)");
                    }
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
    let tarball_path = match download_tarball(&tarball_url, &job_id).await {
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
                match upload_artifact(upload_url, &artifact_path, &artifact_name, &sha256, target).await {
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
