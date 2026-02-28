use std::path::Path;
use tokio::process::Command;
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct AppleCredentials {
    pub team_id: String,
    pub signing_identity: String,
    pub key_id: String,
    pub issuer_id: String,
    pub p8_key: String,
}

impl std::fmt::Debug for AppleCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppleCredentials")
            .field("team_id", &self.team_id)
            .field("signing_identity", &"[REDACTED]")
            .field("key_id", &self.key_id)
            .field("issuer_id", &"[REDACTED]")
            .field("p8_key", &"[REDACTED]")
            .finish()
    }
}

pub async fn codesign_app(
    identity: &str,
    entitlements: Option<&Path>,
    app_path: &Path,
) -> Result<(), String> {
    let mut cmd = Command::new("codesign");
    cmd.arg("--force")
        .arg("--options")
        .arg("runtime")
        .arg("--sign")
        .arg(identity);

    if let Some(ent) = entitlements {
        cmd.arg("--entitlements").arg(ent);
    }

    cmd.arg(app_path);

    let output = cmd
        .output()
        .await
        .map_err(|e| format!("Failed to run codesign: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("codesign failed: {stderr}"));
    }

    Ok(())
}

pub async fn notarize_dmg(
    dmg_path: &Path,
    p8_key: &str,
    key_id: &str,
    issuer_id: &str,
    tmpdir: &Path,
) -> Result<(), String> {
    // Write .p8 key to a temporary file
    let p8_path = tmpdir.join("AuthKey.p8");
    std::fs::write(&p8_path, p8_key)
        .map_err(|e| format!("Failed to write .p8 key: {e}"))?;

    // Submit for notarization
    let output = Command::new("xcrun")
        .arg("notarytool")
        .arg("submit")
        .arg(dmg_path)
        .arg("--key")
        .arg(&p8_path)
        .arg("--key-id")
        .arg(key_id)
        .arg("--issuer")
        .arg(issuer_id)
        .arg("--wait")
        .output()
        .await
        .map_err(|e| format!("Failed to run notarytool: {e}"))?;

    // Immediately delete the .p8 key file
    if let Err(e) = std::fs::remove_file(&p8_path) {
        tracing::warn!(error = %e, "Failed to delete .p8 key file");
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("notarytool failed: {stderr}"));
    }

    // Staple the notarization ticket
    let staple_output = Command::new("xcrun")
        .arg("stapler")
        .arg("staple")
        .arg(dmg_path)
        .output()
        .await
        .map_err(|e| format!("Failed to run stapler: {e}"))?;

    if !staple_output.status.success() {
        let stderr = String::from_utf8_lossy(&staple_output.stderr);
        return Err(format!("stapler failed: {stderr}"));
    }

    Ok(())
}
