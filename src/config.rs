use std::env;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub hub_ws_url: String,
    pub perry_binary: String,
    pub android_home: Option<String>,
    pub android_ndk_home: Option<String>,
    pub worker_name: Option<String>,
    pub hub_secret: Option<String>,
    pub verify_url: Option<String>,
    /// When set, builds run inside a fresh Tart VM clone for full isolation.
    /// Value is the golden image name (e.g. "perry-builder-golden").
    pub tart_image: Option<String>,
    /// SSH password for the Tart VM (user is always "admin").
    pub tart_ssh_password: Option<String>,
    /// Path to perry-ship binary inside the Tart VM.
    /// Defaults to "/Users/admin/bin/perry-ship".
    pub tart_perry_ship_path: Option<String>,
    /// Max concurrent builds (default 2). Each build runs in its own Tart VM.
    pub max_concurrent_builds: usize,
}

impl WorkerConfig {
    pub fn from_env() -> Self {
        Self {
            hub_ws_url: env::var("PERRY_HUB_URL")
                .unwrap_or_else(|_| "wss://hub.perryts.com/ws".into()),
            perry_binary: env::var("PERRY_BUILD_PERRY_BINARY")
                .unwrap_or_else(|_| "perry".into()),
            android_home: env::var("PERRY_BUILD_ANDROID_HOME")
                .or_else(|_| env::var("ANDROID_HOME"))
                .ok(),
            android_ndk_home: env::var("PERRY_BUILD_ANDROID_NDK_HOME")
                .or_else(|_| env::var("ANDROID_NDK_HOME"))
                .ok(),
            worker_name: env::var("PERRY_WORKER_NAME").ok(),
            hub_secret: env::var("PERRY_HUB_WORKER_SECRET").ok(),
            verify_url: env::var("PERRY_VERIFY_URL").ok(),
            tart_image: env::var("PERRY_TART_IMAGE").ok(),
            tart_ssh_password: env::var("PERRY_TART_SSH_PASSWORD").ok(),
            tart_perry_ship_path: env::var("PERRY_TART_PERRY_SHIP_PATH").ok(),
            max_concurrent_builds: env::var("PERRY_MAX_CONCURRENT_BUILDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2),
        }
    }

    /// Returns true if Tart VM isolation is enabled.
    pub fn tart_enabled(&self) -> bool {
        self.tart_image.is_some()
    }
}
