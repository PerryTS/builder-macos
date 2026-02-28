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

    let config = WorkerConfig::from_env();

    tracing::info!(
        hub = %config.hub_ws_url,
        perry = %config.perry_binary,
        "Perry-ship worker starting"
    );

    worker::run_worker(config).await;
}
