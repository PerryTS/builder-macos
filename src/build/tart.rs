//! Tart VM isolation for build jobs.
//!
//! When enabled, each build runs inside a fresh Tart VM clone:
//! 1. `tart clone <golden-image> job-<id>` — create ephemeral VM
//! 2. `tart run job-<id> --no-graphics` — boot VM in background
//! 3. Wait for IP via `tart ip job-<id>`
//! 4. SCP tarball into VM, run build via SSH, SCP artifact back
//! 5. `tart stop job-<id>` + `tart delete job-<id>` — always cleanup

use crate::config::WorkerConfig;
use crate::ws::messages::{LogStream, ServerMessage, StageName};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

type ProgressSender = UnboundedSender<ServerMessage>;

/// VM name derived from job ID.
fn vm_name(job_id: &str) -> String {
    format!("job-{job_id}")
}

/// Run a command and return stdout, or an error with stderr.
async fn run_cmd(cmd: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("Failed to run {cmd}: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{cmd} failed: {stderr}"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// SSH command builder for the VM.
struct VmSsh {
    ip: String,
    password: String,
}

impl VmSsh {
    fn new(ip: String, password: String) -> Self {
        Self { ip, password }
    }

    /// Common SSH options to force password auth via sshpass.
    /// Without these, the host's SSH keys get tried first and exhaust auth attempts.
    fn ssh_opts() -> Vec<&'static str> {
        vec![
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR",
            "-o", "PreferredAuthentications=password",
            "-o", "PubkeyAuthentication=no",
        ]
    }

    /// Run a command inside the VM via SSH. Returns (stdout, stderr).
    async fn exec(&self, remote_cmd: &str) -> Result<(String, String), String> {
        let user_host = format!("admin@{}", self.ip);
        let mut args = vec!["-p", &self.password, "ssh"];
        args.extend(Self::ssh_opts());
        args.extend(["-o", "ConnectTimeout=10"]);
        args.push(&user_host);
        args.push(remote_cmd);

        let output = Command::new("sshpass")
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("SSH exec failed: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            return Err(format!("SSH command failed (exit {}): {stderr}", output.status));
        }

        Ok((stdout, stderr))
    }

    /// SCP a file to the VM.
    async fn upload(&self, local: &Path, remote: &str) -> Result<(), String> {
        let mut args = vec!["-p", &self.password, "scp"];
        args.extend(Self::ssh_opts());
        let local_str = local.to_string_lossy();
        args.push(&local_str);
        let dest = format!("admin@{}:{remote}", self.ip);
        args.push(&dest);

        let output = Command::new("sshpass")
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("SCP upload failed: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("SCP upload failed: {stderr}"));
        }
        Ok(())
    }

    /// SCP a file from the VM to the host.
    async fn download(&self, remote: &str, local: &Path) -> Result<(), String> {
        let mut args = vec!["-p", &self.password, "scp"];
        args.extend(Self::ssh_opts());
        let src = format!("admin@{}:{remote}", self.ip);
        args.push(&src);
        let local_str = local.to_string_lossy();
        args.push(&local_str);

        let output = Command::new("sshpass")
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("SCP download failed: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("SCP download failed: {stderr}"));
        }
        Ok(())
    }

    /// Wait until SSH is reachable (VM may take a few seconds after boot).
    async fn wait_ready(&self, timeout_secs: u64) -> Result<(), String> {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(timeout_secs);

        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(format!(
                    "VM SSH not reachable after {timeout_secs}s at {}",
                    self.ip
                ));
            }

            match self.exec("echo ok").await {
                Ok((stdout, _)) if stdout.trim() == "ok" => return Ok(()),
                _ => {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    }
}

/// Clone and boot the VM. Returns the VM name (for cleanup).
async fn start_vm(golden_image: &str, job_id: &str) -> Result<String, String> {
    let name = vm_name(job_id);

    // Clone golden image
    run_cmd("tart", &["clone", golden_image, &name]).await?;

    // Start VM in background (--no-graphics = headless)
    let mut child = Command::new("tart")
        .args(["run", &name, "--no-graphics"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start VM: {e}"))?;

    // Detach the child so it runs in the background
    // (we'll stop it explicitly later)
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    Ok(name)
}

/// Wait for the VM to get an IP address.
async fn wait_for_ip(vm_name: &str, timeout_secs: u64) -> Result<String, String> {
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(timeout_secs);

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(format!("VM {vm_name} did not get an IP after {timeout_secs}s"));
        }

        match run_cmd("tart", &["ip", vm_name]).await {
            Ok(ip) if !ip.is_empty() => return Ok(ip),
            _ => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

/// Stop and delete the VM. Always runs, even on error.
async fn cleanup_vm(vm_name: &str) {
    // Stop VM (ignore errors — it may already be stopped)
    let _ = run_cmd("tart", &["stop", vm_name]).await;
    // Delete VM clone
    if let Err(e) = run_cmd("tart", &["delete", vm_name]).await {
        tracing::warn!(vm = %vm_name, "Failed to delete VM: {e}");
    }
}

fn send_log(progress: &ProgressSender, stage: StageName, line: &str, stream: LogStream) {
    let _ = progress.send(ServerMessage::Log {
        stage,
        line: line.to_string(),
        stream,
    });
}

/// Execute a build inside a Tart VM.
///
/// This is the Tart equivalent of `pipeline::execute_build`. The host perry-ship
/// orchestrates the build by:
/// 1. Cloning + booting a VM
/// 2. Uploading the tarball
/// 3. Running perry-ship's build command inside the VM via SSH
/// 4. Downloading the resulting artifact
/// 5. Destroying the VM
pub async fn execute_build_in_vm(
    request: &super::pipeline::BuildRequest,
    config: &WorkerConfig,
    cancelled: Arc<AtomicBool>,
    progress: ProgressSender,
) -> Result<PathBuf, String> {
    let golden_image = config
        .tart_image
        .as_deref()
        .ok_or("PERRY_TART_IMAGE not set")?;
    let ssh_password = config
        .tart_ssh_password
        .as_deref()
        .unwrap_or("admin");

    let vm_name = vm_name(&request.job_id);

    // Run the build in a closure so we can always clean up the VM
    let result = run_vm_build(
        request,
        config,
        &cancelled,
        &progress,
        golden_image,
        ssh_password,
    )
    .await;

    // Always clean up VM, regardless of success/failure
    send_log(
        &progress,
        StageName::Extracting,
        &format!("Destroying VM {vm_name}..."),
        LogStream::Stdout,
    );
    cleanup_vm(&vm_name).await;

    result
}

async fn run_vm_build(
    request: &super::pipeline::BuildRequest,
    config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    golden_image: &str,
    ssh_password: &str,
) -> Result<PathBuf, String> {
    let job_id = &request.job_id;

    // Stage: Boot VM
    let _ = progress.send(ServerMessage::Stage {
        stage: StageName::Extracting,
        message: "Booting isolated VM...".into(),
    });

    if cancelled.load(Ordering::Relaxed) {
        return Err("Build cancelled".into());
    }

    send_log(
        progress,
        StageName::Extracting,
        &format!("Cloning golden image '{golden_image}' → job-{job_id}"),
        LogStream::Stdout,
    );

    let vm = start_vm(golden_image, job_id).await?;
    send_log(progress, StageName::Extracting, "VM started, waiting for IP...", LogStream::Stdout);

    let ip = wait_for_ip(&vm, 120).await?;
    send_log(
        progress,
        StageName::Extracting,
        &format!("VM IP: {ip}"),
        LogStream::Stdout,
    );

    let ssh = VmSsh::new(ip, ssh_password.to_string());

    // Wait for SSH to be ready
    send_log(progress, StageName::Extracting, "Waiting for SSH...", LogStream::Stdout);
    ssh.wait_ready(60).await?;
    send_log(progress, StageName::Extracting, "SSH connected", LogStream::Stdout);

    if cancelled.load(Ordering::Relaxed) {
        return Err("Build cancelled".into());
    }

    // Upload tarball to VM
    send_log(progress, StageName::Extracting, "Uploading tarball to VM...", LogStream::Stdout);
    let remote_tarball = format!("/tmp/{job_id}.tar.gz");
    ssh.upload(&request.tarball_path, &remote_tarball).await?;

    // Serialize manifest and credentials to JSON files and upload them
    let tmpdir = std::env::temp_dir().join(format!("perry-tart-{job_id}"));
    std::fs::create_dir_all(&tmpdir)
        .map_err(|e| format!("Failed to create temp dir: {e}"))?;

    let manifest_path = tmpdir.join("manifest.json");
    let creds_path = tmpdir.join("credentials.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string(&request.manifest).map_err(|e| format!("Serialize manifest: {e}"))?,
    )
    .map_err(|e| format!("Write manifest: {e}"))?;

    // Credentials: we need to re-serialize since BuildCredentials uses ZeroizeOnDrop
    // and doesn't impl Serialize. We'll use the original JSON from the hub instead.
    // For now, write an empty JSON and pass creds via env vars in the SSH command.
    // Actually — let's upload the credentials file too. We'll serialize it manually.
    let creds_json = serialize_credentials(&request.credentials);
    std::fs::write(&creds_path, &creds_json)
        .map_err(|e| format!("Write credentials: {e}"))?;

    ssh.upload(&manifest_path, "/tmp/manifest.json").await?;
    ssh.upload(&creds_path, "/tmp/credentials.json").await?;

    // Clean up local temp files (creds should not linger on host)
    let _ = std::fs::remove_dir_all(&tmpdir);

    if cancelled.load(Ordering::Relaxed) {
        return Err("Build cancelled".into());
    }

    // Run the build inside the VM
    // The VM has perry-ship installed; we invoke it in "local build" mode
    let perry_ship_path = config
        .tart_perry_ship_path
        .as_deref()
        .unwrap_or("/Users/admin/bin/perry-ship");

    let perry_binary = "/Users/admin/bin/perry";
    let remote_artifact_dir = "/tmp/perry-artifacts";

    // Build command: run perry-ship in single-job mode inside the VM
    // Source cargo env to ensure Rust tools are available for native lib builds
    // Propagate select env vars into the VM.
    // NOTE: Do NOT propagate PERRY_DT_XCODE/PERRY_DT_XCODE_BUILD — the VM has
    // the correct Xcode installed and should use its own real SDK values.
    // Those overrides are only for hosts with outdated Xcode (e.g. MacinCloud).
    let mut env_exports = String::new();
    for var in ["PERRY_VERIFY_URL"] {
        if let Ok(val) = std::env::var(var) {
            env_exports.push_str(&format!("export {var}='{val}'; "));
        }
    }
    let build_cmd = format!(
        "source ~/.cargo/env 2>/dev/null; {env_exports}\
         mkdir -p {remote_artifact_dir} && \
         {perry_ship_path} build-local \
         --manifest /tmp/manifest.json \
         --credentials /tmp/credentials.json \
         --tarball {remote_tarball} \
         --job-id {job_id} \
         --perry-binary {perry_binary} \
         --artifact-dir {remote_artifact_dir}"
    );

    let _ = progress.send(ServerMessage::Stage {
        stage: StageName::Compiling,
        message: "Building inside VM...".into(),
    });

    // Execute the build via SSH and stream output
    let build_result = ssh_exec_streaming(&ssh, &build_cmd, progress, cancelled).await;

    match build_result {
        Ok(output) => {
            // Parse the artifact path from the last line of stdout
            let artifact_line = output
                .lines()
                .rev()
                .find(|l| l.starts_with("ARTIFACT:"))
                .ok_or("Build completed but no ARTIFACT: line in output")?;
            let remote_artifact_path = artifact_line
                .strip_prefix("ARTIFACT:")
                .unwrap()
                .trim();

            // Download artifact from VM
            let _ = progress.send(ServerMessage::Stage {
                stage: StageName::Packaging,
                message: "Downloading artifact from VM...".into(),
            });

            let artifact_name = Path::new(remote_artifact_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("artifact");

            let local_artifact_dir = std::env::temp_dir().join("perry-artifacts");
            std::fs::create_dir_all(&local_artifact_dir)
                .map_err(|e| format!("Failed to create artifact dir: {e}"))?;
            let local_artifact = local_artifact_dir.join(artifact_name);

            ssh.download(remote_artifact_path, &local_artifact).await?;

            Ok(local_artifact)
        }
        Err(e) => Err(format!("VM build failed: {e}")),
    }
}

/// Execute SSH command and stream stdout/stderr lines as log messages.
async fn ssh_exec_streaming(
    ssh: &VmSsh,
    cmd: &str,
    progress: &ProgressSender,
    cancelled: &Arc<AtomicBool>,
) -> Result<String, String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let user_host = format!("admin@{}", ssh.ip);
    let mut child = Command::new("sshpass")
        .args([
            "-p", &ssh.password, "ssh",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR",
            "-o", "PreferredAuthentications=password",
            "-o", "PubkeyAuthentication=no",
            &user_host,
            cmd,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn SSH: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();

    let mut full_stdout = String::new();
    let mut full_stderr = String::new();

    loop {
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill().await;
            return Err("Build cancelled".into());
        }

        tokio::select! {
            line = stdout_reader.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        // Forward progress messages from VM's perry-ship
                        if l.starts_with('{') {
                            // JSON progress message from perry-ship — forward as-is
                            // (the VM perry-ship outputs JSON progress to stdout)
                            if let Ok(msg) = serde_json::from_str::<ServerMessage>(&l) {
                                let _ = progress.send(msg);
                            }
                        } else {
                            send_log(progress, StageName::Compiling, &l, LogStream::Stdout);
                        }
                        full_stdout.push_str(&l);
                        full_stdout.push('\n');
                    }
                    Ok(None) => break,
                    Err(e) => {
                        send_log(progress, StageName::Compiling, &format!("stdout read error: {e}"), LogStream::Stderr);
                        break;
                    }
                }
            }
            line = stderr_reader.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        send_log(progress, StageName::Compiling, &l, LogStream::Stderr);
                        full_stderr.push_str(&l);
                        full_stderr.push('\n');
                    }
                    Ok(None) => {}
                    Err(e) => {
                        send_log(progress, StageName::Compiling, &format!("stderr read error: {e}"), LogStream::Stderr);
                    }
                }
            }
        }
    }

    // Drain remaining stderr after stdout closes
    while let Ok(Some(l)) = stderr_reader.next_line().await {
        send_log(progress, StageName::Compiling, &l, LogStream::Stderr);
        full_stderr.push_str(&l);
        full_stderr.push('\n');
    }

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Failed to wait for SSH process: {e}"))?;

    if !status.success() {
        // Extract BUILD_ERROR from stderr if present
        let build_error = full_stderr
            .lines()
            .find(|l| l.starts_with("BUILD_ERROR:"))
            .map(|l| l.strip_prefix("BUILD_ERROR:").unwrap().trim().to_string());
        let err_msg = build_error.unwrap_or_else(|| {
            if full_stderr.trim().is_empty() {
                format!("Build exited with {status}")
            } else {
                // Use last non-empty line of stderr
                full_stderr
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("unknown error")
                    .to_string()
            }
        });
        return Err(err_msg);
    }

    Ok(full_stdout)
}

/// Manually serialize BuildCredentials to JSON.
/// BuildCredentials has ZeroizeOnDrop but no Serialize — we reconstruct it field-by-field.
fn serialize_credentials(creds: &crate::queue::job::BuildCredentials) -> String {
    let mut map = serde_json::Map::new();

    macro_rules! insert_opt {
        ($field:ident) => {
            if let Some(ref v) = creds.$field {
                map.insert(
                    stringify!($field).to_string(),
                    serde_json::Value::String(v.clone()),
                );
            }
        };
    }

    insert_opt!(apple_team_id);
    insert_opt!(apple_signing_identity);
    insert_opt!(apple_key_id);
    insert_opt!(apple_issuer_id);
    insert_opt!(apple_p8_key);
    insert_opt!(provisioning_profile_base64);
    insert_opt!(apple_certificate_p12_base64);
    insert_opt!(apple_certificate_password);
    insert_opt!(apple_notarize_certificate_p12_base64);
    insert_opt!(apple_notarize_certificate_password);
    insert_opt!(apple_notarize_signing_identity);
    insert_opt!(apple_installer_certificate_p12_base64);
    insert_opt!(apple_installer_certificate_password);
    insert_opt!(android_keystore_base64);
    insert_opt!(android_keystore_password);
    insert_opt!(android_key_alias);
    insert_opt!(android_key_password);
    insert_opt!(google_play_service_account_json);

    serde_json::Value::Object(map).to_string()
}
