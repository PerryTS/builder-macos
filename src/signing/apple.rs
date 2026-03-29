use std::path::Path;
use tokio::process::Command;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// A temporary macOS keychain scoped to a single build job.
/// Imports a .p12 cert on creation, exposes the detected signing identity,
/// and deletes the keychain on drop. Pass `keychain_path` to `codesign_app`
/// so codesign finds the cert without touching the system search list.
pub struct TempKeychain {
    /// Full path: ~/Library/Keychains/perry-kc-{job_id}.keychain-db
    pub path: String,
    /// Signing identity string parsed from the imported cert's CN.
    pub identity: String,
    /// Internal keychain password used for partition-list unlock.
    kc_password: String,
}

impl TempKeychain {
    pub async fn create(
        job_id: &str,
        p12_b64: &str,
        p12_password: &str,
        tmpdir: &Path,
        preferred_identity: Option<&str>,
    ) -> Result<Self, String> {
        use base64::Engine;
        let p12_bytes = base64::engine::general_purpose::STANDARD
            .decode(p12_b64.trim())
            .map_err(|e| format!("Invalid base64 for .p12: {e}"))?;

        // Write p12 to tmpdir (cleaned up by the build's cleanup_tmpdir)
        let p12_path = tmpdir.join(format!("cert-{job_id}.p12"));
        std::fs::write(&p12_path, &p12_bytes)
            .map_err(|e| format!("Failed to write .p12: {e}"))?;

        let kc_name = format!("perry-kc-{job_id}.keychain");
        let kc_password = format!("{job_id}-kc");

        // Helper: delete keychain on any error below
        let cleanup = |name: &str| {
            let _ = std::process::Command::new("security")
                .args(["delete-keychain", name])
                .status();
        };

        // 1. Create keychain (delete first in case a stale one exists from a failed build)
        cleanup(&kc_name);
        let out = std::process::Command::new("security")
            .args(["create-keychain", "-p", &kc_password, &kc_name])
            .output()
            .map_err(|e| format!("Failed to create keychain: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!("security create-keychain failed: {stderr}"));
        }

        // 2. Set long lock timeout
        let _ = std::process::Command::new("security")
            .args(["set-keychain-settings", "-lut", "7200", &kc_name])
            .status();

        // 3. Import .p12 (-T flags allow codesign, productsign, productbuild without UI prompt)
        let out = std::process::Command::new("security")
            .args([
                "import", p12_path.to_str().unwrap_or(""),
                "-k", &kc_name,
                "-P", p12_password,
                "-T", "/usr/bin/codesign",
                "-T", "/usr/bin/productsign",
                "-T", "/usr/bin/productbuild",
            ])
            .output()
            .map_err(|e| format!("Failed to run security import: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            cleanup(&kc_name);
            return Err(format!("security import failed: {stderr}"));
        }

        // Immediately remove p12 from disk
        let _ = std::fs::remove_file(&p12_path);

        // 4. Allow partition access (suppresses UI auth dialogs for codesign/productbuild)
        let out = std::process::Command::new("security")
            .args([
                "set-key-partition-list",
                "-S", "apple-tool:,apple:,codesign:,productbuild:",
                "-s",
                "-k", &kc_password,
                &kc_name,
            ])
            .output()
            .map_err(|e| format!("Failed to run set-key-partition-list: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            cleanup(&kc_name);
            return Err(format!("set-key-partition-list failed: {stderr}"));
        }

        // 4b. Import Apple WWDR intermediate CAs into temp keychain
        // so find-identity can validate the full certificate chain.
        // Download directly from Apple to avoid relying on system keychain state.
        for url in &[
            "https://www.apple.com/certificateauthority/AppleWWDRCAG3.cer",
            "https://www.apple.com/certificateauthority/AppleWWDRCAG6.cer",
            "https://www.apple.com/appleca/AppleIncRootCertificate.cer",
        ] {
            let cert_path = tmpdir.join(format!("wwdr-{job_id}.cer"));
            let dl = std::process::Command::new("curl")
                .args(["-sL", url, "-o", cert_path.to_str().unwrap_or("")])
                .status();
            if dl.map(|s| s.success()).unwrap_or(false) {
                let _ = std::process::Command::new("security")
                    .args(["import", cert_path.to_str().unwrap_or(""), "-k", &kc_name])
                    .status();
            }
            let _ = std::fs::remove_file(&cert_path);
        }
        // Also export any WWDR certs from system keychain (covers older cert versions)
        let export_out = std::process::Command::new("security")
            .args(["find-certificate", "-a", "-c", "Apple Worldwide Developer Relations", "-p", "/Library/Keychains/System.keychain"])
            .output();
        if let Ok(ref out) = export_out {
            if out.status.success() && !out.stdout.is_empty() {
                let pem_path = tmpdir.join(format!("wwdr-sys-{job_id}.pem"));
                let _ = std::fs::write(&pem_path, &out.stdout);
                let _ = std::process::Command::new("security")
                    .args(["import", pem_path.to_str().unwrap_or(""), "-k", &kc_name])
                    .status();
                let _ = std::fs::remove_file(&pem_path);
            }
        }

        // 5. Detect the signing identity from the keychain
        let out = std::process::Command::new("security")
            .args(["find-identity", "-v", "-p", "codesigning", &kc_name])
            .output()
            .map_err(|e| format!("Failed to query keychain identity: {e}"))?;
        let identity_output = String::from_utf8_lossy(&out.stdout);
        let identity = parse_identity_from_find_output(&identity_output, preferred_identity)
            .ok_or_else(|| {
                cleanup(&kc_name);
                format!("No valid signing identity found in .p12 certificate (looking for: {:?})", preferred_identity)
            })?;

        // Resolve full keychain path (security appends .keychain-db)
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let kc_path = format!("{home}/Library/Keychains/{kc_name}-db");

        Ok(TempKeychain { path: kc_path, identity, kc_password })
    }

    /// Import an additional .p12 certificate into this keychain.
    /// Used for importing the installer cert separately from the app cert.
    pub fn import_additional_p12(
        &self,
        p12_b64: &str,
        p12_password: &str,
        tmpdir: &Path,
    ) -> Result<(), String> {
        use base64::Engine;
        let p12_bytes = base64::engine::general_purpose::STANDARD
            .decode(p12_b64.trim())
            .map_err(|e| format!("Invalid base64 for additional .p12: {e}"))?;

        let p12_path = tmpdir.join("additional-cert.p12");
        std::fs::write(&p12_path, &p12_bytes)
            .map_err(|e| format!("Failed to write additional .p12: {e}"))?;

        // Derive the keychain name from path for security commands
        let kc_name = &self.path;

        let out = std::process::Command::new("security")
            .args([
                "import", p12_path.to_str().unwrap_or(""),
                "-k", kc_name,
                "-P", p12_password,
                "-T", "/usr/bin/codesign",
                "-T", "/usr/bin/productsign",
                "-T", "/usr/bin/productbuild",
            ])
            .output()
            .map_err(|e| format!("Failed to import additional .p12: {e}"))?;

        let _ = std::fs::remove_file(&p12_path);

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!("security import (additional .p12) failed: {stderr}"));
        }

        // Re-run partition list to cover the new key
        let _ = std::process::Command::new("security")
            .args([
                "set-key-partition-list",
                "-S", "apple-tool:,apple:,codesign:,productbuild:",
                "-s",
                "-k", &self.kc_password,
                kc_name,
            ])
            .status();

        Ok(())
    }

    /// Add this keychain to the user search list.
    /// Required for tools like productsign that don't support --keychain.
    pub fn add_to_search_list(&self) -> Result<(), String> {
        // Read current list
        let current = std::process::Command::new("security")
            .args(["list-keychains", "-d", "user"])
            .output()
            .map_err(|e| format!("list-keychains failed: {e}"))?;
        let current_str = String::from_utf8_lossy(&current.stdout);
        // Build new list: our keychain first, then existing ones (strip quotes/whitespace)
        let mut args = vec!["list-keychains".to_string(), "-d".to_string(), "user".to_string(), "-s".to_string()];
        args.push(self.path.clone());
        for line in current_str.lines() {
            let p = line.trim().trim_matches('"');
            if !p.is_empty() && p != self.path {
                args.push(p.to_string());
            }
        }
        std::process::Command::new("security")
            .args(&args)
            .status()
            .map_err(|e| format!("set list-keychains failed: {e}"))?;
        Ok(())
    }

    /// Remove this keychain from the user search list (without deleting it).
    pub fn remove_from_search_list(&self) {
        let current = std::process::Command::new("security")
            .args(["list-keychains", "-d", "user"])
            .output();
        if let Ok(out) = current {
            let current_str = String::from_utf8_lossy(&out.stdout);
            let mut args = vec!["list-keychains".to_string(), "-d".to_string(), "user".to_string(), "-s".to_string()];
            for line in current_str.lines() {
                let p = line.trim().trim_matches('"');
                if !p.is_empty() && p != self.path {
                    args.push(p.to_string());
                }
            }
            let _ = std::process::Command::new("security").args(&args).status();
        }
    }
}

impl Drop for TempKeychain {
    fn drop(&mut self) {
        self.remove_from_search_list();
        let _ = std::process::Command::new("security")
            .args(["delete-keychain", &self.path])
            .status();
        self.kc_password.zeroize();
        self.identity.zeroize();
    }
}

/// Find an installer signing identity from this keychain.
/// Looks for "3rd Party Mac Developer Installer" or "Mac Developer Installer" identities.
pub fn find_installer_identity(kc_name: &str) -> Option<String> {
    // Don't use -v (valid only) — installer certs may not pass the default
    // validity check without the full chain, but they still work for signing.
    let out = std::process::Command::new("security")
        .args(["find-identity", kc_name])
        .output()
        .ok()?;
    let output = String::from_utf8_lossy(&out.stdout);
    tracing::info!("find_installer_identity output for {kc_name}:\n{output}");
    for line in output.lines() {
        let line = line.trim();
        if let Some(start) = line.find('"') {
            if let Some(end) = line.rfind('"') {
                if end > start {
                    let identity = &line[start + 1..end];
                    if identity.contains("Installer") {
                        return Some(identity.to_string());
                    }
                }
            }
        }
    }
    None
}

fn parse_identity_from_find_output(output: &str, preferred: Option<&str>) -> Option<String> {
    // Lines look like:   1) DEADBEEF "iPhone Distribution: Foo Corp (TEAMID)"
    let mut all_identities = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some(start) = line.find('"') {
            if let Some(end) = line.rfind('"') {
                if end > start {
                    let identity = &line[start + 1..end];
                    if !identity.is_empty() {
                        all_identities.push(identity.to_string());
                    }
                }
            }
        }
    }

    // If a preferred identity is specified, find it (substring match)
    if let Some(pref) = preferred {
        if !pref.is_empty() {
            if let Some(found) = all_identities.iter().find(|id| id.contains(pref)) {
                return Some(found.clone());
            }
        }
    }

    // Fallback to first identity
    all_identities.into_iter().next()
}

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
    _identity: &str,
    entitlements: Option<&Path>,
    app_path: &Path,
    hardened_runtime: bool,
    _keychain: Option<&str>,
    p12_path: Option<&Path>,
    p12_password: Option<&str>,
) -> Result<(), String> {
    // Use rcodesign (Rust-based, no keychain/securityd dependency) if p12 is available.
    // Falls back to Apple's codesign for ad-hoc signing.
    if let (Some(p12), Some(pass)) = (p12_path, p12_password) {
        let mut cmd = Command::new("rcodesign");
        cmd.arg("sign");
        cmd.arg("--p12-file").arg(p12);
        cmd.arg("--p12-password").arg(pass);
        if hardened_runtime {
            cmd.arg("--code-signature-flags").arg("runtime");
        }
        if let Some(ent) = entitlements {
            cmd.arg("--entitlements-xml-path").arg(ent);
        }
        cmd.arg(app_path);

        let output = cmd
            .output()
            .await
            .map_err(|e| format!("Failed to run rcodesign: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(format!("rcodesign failed: {stderr}\n{stdout}"));
        }
        return Ok(());
    }

    // Fallback: Apple's codesign (for ad-hoc or when no p12 provided)
    let mut cmd = Command::new("codesign");
    cmd.arg("--force");
    if hardened_runtime {
        cmd.arg("--options").arg("runtime");
    }
    cmd.arg("--sign").arg(_identity);

    if let Some(kc) = _keychain {
        cmd.arg("--keychain").arg(kc);
    }

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
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!("notarytool failed:\nstdout: {stdout}\nstderr: {stderr}"));
    }

    // Staple the notarization ticket — retry up to 3 times with delays,
    // Apple's CDN can take a few seconds to propagate after notarytool returns.
    let mut stapled = false;
    for attempt in 1..=3 {
        let staple_output = Command::new("xcrun")
            .arg("stapler")
            .arg("staple")
            .arg(dmg_path)
            .output()
            .await
            .map_err(|e| format!("Failed to run stapler: {e}"))?;

        if staple_output.status.success() {
            stapled = true;
            break;
        }
        let stdout = String::from_utf8_lossy(&staple_output.stdout);
        if attempt < 3 {
            tracing::info!("Stapler attempt {attempt}/3 failed, retrying in 10s: {}", stdout.trim());
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        } else {
            tracing::warn!(
                "stapler failed after 3 attempts (non-fatal, app is still notarized): {}",
                stdout.trim()
            );
        }
    }
    if stapled {
        tracing::info!("Notarization ticket stapled to DMG");
    }

    Ok(())
}
