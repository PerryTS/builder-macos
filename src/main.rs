use perry_ship::config::WorkerConfig;
use perry_ship::worker;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "perry_ship=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    // `perry-ship build-local` — run a single build locally (used inside Tart VMs)
    if args.get(1).map(|s| s.as_str()) == Some("build-local") {
        run_build_local(&args[2..]).await;
        return;
    }

    let config = WorkerConfig::from_env();

    tracing::info!(
        hub = %config.hub_ws_url,
        perry = %config.perry_binary,
        "Perry-ship worker starting"
    );

    worker::run_worker(config).await;
}

/// Run a single build locally from manifest/credentials/tarball files.
/// Used by the host perry-ship to execute builds inside Tart VMs.
///
/// Usage: perry-ship build-local --manifest <path> --credentials <path>
///        --tarball <path> --job-id <id> --perry-binary <path> --artifact-dir <path>
///
/// Outputs progress as JSON lines to stdout.
/// Final line: ARTIFACT:<path> on success.
async fn run_build_local(args: &[String]) {
    use perry_ship::build::pipeline::{self, BuildRequest};
    use perry_ship::queue::job::{BuildCredentials, BuildManifest};
    use perry_ship::ws::messages::ServerMessage;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let mut manifest_path = None;
    let mut credentials_path = None;
    let mut tarball_path = None;
    let mut job_id = None;
    let mut perry_binary = None;
    let mut artifact_dir = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                i += 1;
                manifest_path = args.get(i).cloned();
            }
            "--credentials" => {
                i += 1;
                credentials_path = args.get(i).cloned();
            }
            "--tarball" => {
                i += 1;
                tarball_path = args.get(i).cloned();
            }
            "--job-id" => {
                i += 1;
                job_id = args.get(i).cloned();
            }
            "--perry-binary" => {
                i += 1;
                perry_binary = args.get(i).cloned();
            }
            "--artifact-dir" => {
                i += 1;
                artifact_dir = args.get(i).cloned();
            }
            other => {
                eprintln!("Unknown argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let manifest_path = manifest_path.expect("--manifest required");
    let credentials_path = credentials_path.expect("--credentials required");
    let tarball_path = tarball_path.expect("--tarball required");
    let job_id = job_id.expect("--job-id required");
    let perry_binary = perry_binary.unwrap_or_else(|| "perry".into());
    let artifact_dir = artifact_dir.unwrap_or_else(|| "/tmp/perry-artifacts".into());

    let manifest_json = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("Failed to read manifest: {e}"));
    let credentials_json = std::fs::read_to_string(&credentials_path)
        .unwrap_or_else(|e| panic!("Failed to read credentials: {e}"));

    let manifest: BuildManifest = serde_json::from_str(&manifest_json)
        .unwrap_or_else(|e| panic!("Invalid manifest JSON: {e}"));
    let credentials: BuildCredentials = serde_json::from_str(&credentials_json)
        .unwrap_or_else(|e| panic!("Invalid credentials JSON: {e}"));

    // Delete credentials file immediately after reading
    let _ = std::fs::remove_file(&credentials_path);

    let config = WorkerConfig {
        hub_ws_url: String::new(),
        perry_binary,
        android_home: std::env::var("ANDROID_HOME").ok(),
        android_ndk_home: std::env::var("ANDROID_NDK_HOME").ok(),
        worker_name: None,
        hub_secret: None,
        verify_url: std::env::var("PERRY_VERIFY_URL").ok(),
        tart_image: None, // never nest Tart
        tart_ssh_password: None,
        tart_perry_ship_path: None,
    };

    let request = BuildRequest {
        manifest,
        credentials,
        tarball_path: PathBuf::from(tarball_path),
        job_id: job_id.clone(),
    };

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    let cancelled = Arc::new(AtomicBool::new(false));

    // Spawn progress printer — outputs JSON lines to stdout for the host to parse
    let printer = tokio::spawn(async move {
        while let Some(msg) = progress_rx.recv().await {
            if let Ok(json) = serde_json::to_string(&msg) {
                println!("{json}");
            }
        }
    });

    let result = pipeline::execute_build(&request, &config, cancelled, progress_tx).await;

    // Wait for all progress to be printed
    let _ = printer.await;

    match result {
        Ok(artifact_path) => {
            // Move artifact to the requested artifact dir
            let dest_dir = PathBuf::from(&artifact_dir);
            std::fs::create_dir_all(&dest_dir).ok();
            let artifact_name = artifact_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("artifact");
            let dest = dest_dir.join(artifact_name);
            if artifact_path != dest {
                std::fs::copy(&artifact_path, &dest).ok();
                std::fs::remove_file(&artifact_path).ok();
            }
            println!("ARTIFACT:{}", dest.display());
        }
        Err(e) => {
            eprintln!("BUILD_ERROR:{e}");
            std::process::exit(1);
        }
    }
}
