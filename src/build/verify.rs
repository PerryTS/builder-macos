//! Pre-publish runtime verification via perry-verify service.
//!
//! Sends the compiled binary to the perry-verify HTTP API and polls
//! for results. Called from the build pipeline after packaging/signing
//! but before uploading to any store.

use crate::ws::messages::{ServerMessage, StageName};
use std::path::Path;
use tokio::sync::mpsc::UnboundedSender;

/// Verify a binary against the perry-verify service.
///
/// Returns `Ok(true)` if passed, `Ok(false)` if failed/skipped (non-blocking),
/// or `Err` only on hard errors that should abort the build.
pub async fn verify_binary(
    binary_path: &Path,
    verify_url: &str,
    target: &str,
    app_type: &str,
    progress: &UnboundedSender<ServerMessage>,
) -> Result<bool, String> {
    let binary_data = std::fs::read(binary_path)
        .map_err(|e| format!("Failed to read binary for verification: {e}"))?;

    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&binary_data);

    let config_json = serde_json::json!({
        "auth": { "strategy": "none" }
    })
    .to_string();

    let manifest_json = serde_json::json!({
        "appType": app_type,
        "hasAuthGate": false,
        "entryScreen": "main"
    })
    .to_string();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(330))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let base_url = verify_url.trim_end_matches('/');
    let submit_url = format!("{base_url}/verify");

    let form = reqwest::multipart::Form::new()
        .text("binary_b64", b64)
        .text("target", target.to_string())
        .text("config", config_json)
        .text("manifest", manifest_json);

    let resp = match client.post(&submit_url).multipart(form).send().await {
        Ok(r) => r,
        Err(e) => {
            let _ = progress.send(ServerMessage::Log {
                stage: StageName::Verifying,
                line: format!("Verify service unreachable: {e}"),
                stream: crate::ws::messages::LogStream::Stderr,
            });
            return Ok(false); // non-blocking
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let _ = progress.send(ServerMessage::Log {
            stage: StageName::Verifying,
            line: format!("Verify service returned {status}: {body}"),
            stream: crate::ws::messages::LogStream::Stderr,
        });
        return Ok(false);
    }

    let body = resp.text().await.map_err(|e| format!("Failed to read verify response: {e}"))?;

    #[derive(serde::Deserialize)]
    struct SubmitResp {
        #[serde(rename = "jobId")]
        job_id: String,
    }

    let submit: SubmitResp =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse verify response: {e}"))?;

    let _ = progress.send(ServerMessage::Log {
        stage: StageName::Verifying,
        line: format!("Verification job: {}", submit.job_id),
        stream: crate::ws::messages::LogStream::Stdout,
    });

    // Poll for results
    let poll_url = format!("{base_url}/verify/{}", submit.job_id);
    let timeout = std::time::Duration::from_secs(300);
    let poll_interval = std::time::Duration::from_secs(3);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            let _ = progress.send(ServerMessage::Log {
                stage: StageName::Verifying,
                line: "Verification timed out after 300s".into(),
                stream: crate::ws::messages::LogStream::Stderr,
            });
            return Ok(false);
        }

        tokio::time::sleep(poll_interval).await;

        let resp = match client.get(&poll_url).send().await {
            Ok(r) => r,
            Err(_) => continue,
        };

        if !resp.status().is_success() {
            continue;
        }

        let body = match resp.text().await {
            Ok(b) => b,
            Err(_) => continue,
        };

        #[derive(serde::Deserialize)]
        struct StatusResp {
            status: String,
            steps: Option<Vec<StepInfo>>,
            error: Option<String>,
            #[serde(rename = "durationMs")]
            duration_ms: Option<u64>,
        }

        #[derive(serde::Deserialize)]
        struct StepInfo {
            name: String,
            status: String,
            #[serde(rename = "durationMs")]
            duration_ms: Option<u64>,
        }

        let status: StatusResp = match serde_json::from_str(&body) {
            Ok(s) => s,
            Err(_) => continue,
        };

        match status.status.as_str() {
            "passed" => {
                if let Some(ms) = status.duration_ms {
                    let _ = progress.send(ServerMessage::Log {
                        stage: StageName::Verifying,
                        line: format!("Verification passed ({:.1}s)", ms as f64 / 1000.0),
                        stream: crate::ws::messages::LogStream::Stdout,
                    });
                }
                return Ok(true);
            }
            "failed" | "error" => {
                let detail = status.error.unwrap_or_default();
                let _ = progress.send(ServerMessage::Log {
                    stage: StageName::Verifying,
                    line: format!("Verification {}: {detail}", status.status),
                    stream: crate::ws::messages::LogStream::Stderr,
                });
                if let Some(steps) = status.steps {
                    for step in steps {
                        let icon = if step.status == "passed" { "+" } else { "x" };
                        let _ = progress.send(ServerMessage::Log {
                            stage: StageName::Verifying,
                            line: format!(
                                "  [{icon}] {} ({}ms)",
                                step.name,
                                step.duration_ms.unwrap_or(0)
                            ),
                            stream: crate::ws::messages::LogStream::Stderr,
                        });
                    }
                }
                return Err(format!("Verification {}: {detail}", status.status));
            }
            "running" => {
                // Log step progress
                if let Some(ref steps) = status.steps {
                    for step in steps {
                        if step.status == "passed" {
                            let _ = progress.send(ServerMessage::Log {
                                stage: StageName::Verifying,
                                line: format!(
                                    "  [+] {} ({}ms)",
                                    step.name,
                                    step.duration_ms.unwrap_or(0)
                                ),
                                stream: crate::ws::messages::LogStream::Stdout,
                            });
                        }
                    }
                }
            }
            _ => {} // pending — keep polling
        }
    }
}
