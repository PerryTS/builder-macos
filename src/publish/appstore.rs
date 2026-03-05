//! App Store Connect upload via xcrun altool
//!
//! Uses Apple's altool with API key authentication to upload .ipa files
//! to App Store Connect for TestFlight or App Store distribution.

use std::path::Path;
use tokio::process::Command;

/// Upload an .ipa to App Store Connect using xcrun altool.
///
/// The .p8 key is temporarily written to disk for altool, then immediately deleted.
///
/// `distribute` controls whether this goes to "appstore" or "testflight" —
/// altool uploads to App Store Connect regardless, and distribution is
/// controlled by the App Store Connect portal settings.
pub async fn upload_to_appstore(
    ipa_path: &Path,
    p8_key: &str,
    key_id: &str,
    issuer_id: &str,
    tmpdir: &Path,
) -> Result<UploadResult, String> {
    // altool looks for the key in specific directories.
    // Write it to a temporary location and pass via --apiKey flag.
    let private_keys_dir = tmpdir.join("private_keys");
    std::fs::create_dir_all(&private_keys_dir)
        .map_err(|e| format!("Failed to create private_keys dir: {e}"))?;

    let p8_path = private_keys_dir.join(format!("AuthKey_{key_id}.p8"));
    std::fs::write(&p8_path, p8_key)
        .map_err(|e| format!("Failed to write .p8 key: {e}"))?;

    // Validate the IPA first
    let validate_output = Command::new("xcrun")
        .arg("altool")
        .arg("--validate-app")
        .arg("-f")
        .arg(ipa_path)
        .arg("--type")
        .arg("ios")
        .arg("--apiKey")
        .arg(key_id)
        .arg("--apiIssuer")
        .arg(issuer_id)
        .env("API_PRIVATE_KEYS_DIR", &private_keys_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to run altool validate: {e}"))?;

    if !validate_output.status.success() {
        // Clean up key before returning error
        let _ = std::fs::remove_file(&p8_path);
        let _ = std::fs::remove_dir_all(&private_keys_dir);
        let stderr = String::from_utf8_lossy(&validate_output.stderr);
        let stdout = String::from_utf8_lossy(&validate_output.stdout);
        return Err(format!(
            "App validation failed:\n{}\n{}",
            stderr.trim(),
            stdout.trim()
        ));
    }

    // Upload the IPA
    let upload_output = Command::new("xcrun")
        .arg("altool")
        .arg("--upload-app")
        .arg("-f")
        .arg(ipa_path)
        .arg("--type")
        .arg("ios")
        .arg("--apiKey")
        .arg(key_id)
        .arg("--apiIssuer")
        .arg(issuer_id)
        .arg("--output-format")
        .arg("normal")
        .env("API_PRIVATE_KEYS_DIR", &private_keys_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to run altool upload: {e}"))?;

    // Immediately clean up the key
    if let Err(e) = std::fs::remove_file(&p8_path) {
        tracing::warn!(error = %e, "Failed to delete .p8 key file");
    }
    let _ = std::fs::remove_dir_all(&private_keys_dir);

    let upload_stdout = String::from_utf8_lossy(&upload_output.stdout);
    let upload_stderr = String::from_utf8_lossy(&upload_output.stderr);

    // altool sometimes exits 0 but writes failure to stdout — check both
    if !upload_output.status.success()
        || upload_stdout.contains("Failed")
        || upload_stdout.contains("ERROR ITMS")
        || upload_stderr.contains("Failed")
        || upload_stderr.contains("ERROR ITMS")
    {
        return Err(format!(
            "App Store upload failed:\nstdout: {}\nstderr: {}",
            upload_stdout.trim(),
            upload_stderr.trim()
        ));
    }

    Ok(UploadResult {
        message: upload_stdout.trim().to_string(),
    })
}

pub struct UploadResult {
    pub message: String,
}

/// Upload a macOS .pkg to App Store Connect using xcrun altool.
pub async fn upload_macos_to_appstore(
    pkg_path: &std::path::Path,
    p8_key: &str,
    key_id: &str,
    issuer_id: &str,
    tmpdir: &std::path::Path,
) -> Result<UploadResult, String> {
    let private_keys_dir = tmpdir.join("private_keys");
    std::fs::create_dir_all(&private_keys_dir)
        .map_err(|e| format!("Failed to create private_keys dir: {e}"))?;

    let p8_path = private_keys_dir.join(format!("AuthKey_{key_id}.p8"));
    std::fs::write(&p8_path, p8_key)
        .map_err(|e| format!("Failed to write .p8 key: {e}"))?;

    // Validate first
    let validate_output = Command::new("xcrun")
        .arg("altool")
        .arg("--validate-app")
        .arg("-f")
        .arg(pkg_path)
        .arg("--type")
        .arg("osx")
        .arg("--apiKey")
        .arg(key_id)
        .arg("--apiIssuer")
        .arg(issuer_id)
        .env("API_PRIVATE_KEYS_DIR", &private_keys_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to run altool validate: {e}"))?;

    if !validate_output.status.success() {
        let _ = std::fs::remove_file(&p8_path);
        let _ = std::fs::remove_dir_all(&private_keys_dir);
        let stderr = String::from_utf8_lossy(&validate_output.stderr);
        let stdout = String::from_utf8_lossy(&validate_output.stdout);
        return Err(format!(
            "macOS app validation failed:\n{}\n{}",
            stderr.trim(),
            stdout.trim()
        ));
    }

    // Upload
    let upload_output = Command::new("xcrun")
        .arg("altool")
        .arg("--upload-app")
        .arg("-f")
        .arg(pkg_path)
        .arg("--type")
        .arg("osx")
        .arg("--apiKey")
        .arg(key_id)
        .arg("--apiIssuer")
        .arg(issuer_id)
        .arg("--output-format")
        .arg("normal")
        .env("API_PRIVATE_KEYS_DIR", &private_keys_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to run altool upload: {e}"))?;

    if let Err(e) = std::fs::remove_file(&p8_path) {
        tracing::warn!(error = %e, "Failed to delete .p8 key file");
    }
    let _ = std::fs::remove_dir_all(&private_keys_dir);

    let upload_stdout = String::from_utf8_lossy(&upload_output.stdout);
    let upload_stderr = String::from_utf8_lossy(&upload_output.stderr);

    if !upload_output.status.success()
        || upload_stdout.contains("Failed")
        || upload_stdout.contains("ERROR ITMS")
        || upload_stderr.contains("Failed")
        || upload_stderr.contains("ERROR ITMS")
    {
        return Err(format!(
            "macOS App Store upload failed:\nstdout: {}\nstderr: {}",
            upload_stdout.trim(),
            upload_stderr.trim()
        ));
    }

    Ok(UploadResult {
        message: upload_stdout.trim().to_string(),
    })
}
