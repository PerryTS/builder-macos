//! App Store Connect upload via xcrun altool
//!
//! Uses Apple's altool with API key authentication to upload .ipa files
//! to App Store Connect for TestFlight or App Store distribution.

use std::collections::HashMap;
use std::path::Path;
use tokio::process::Command;

/// Maps known altool ITMS error codes and patterns to friendly, actionable messages.
///
/// Scans combined stdout+stderr for known patterns and returns a human-readable
/// error message with the raw output appended for debugging.
pub fn translate_altool_error(stdout: &str, stderr: &str) -> String {
    let combined = format!("{stdout}\n{stderr}");

    let guidance = if combined.contains("ITMS-90062") {
        Some(
            "Invalid provisioning profile — regenerate an App Store profile at \
             https://developer.apple.com/account/resources/profiles/list",
        )
    } else if combined.contains("ITMS-90161") {
        Some("Missing app icon — a 1024×1024 PNG with no alpha channel is required")
    } else if combined.contains("ITMS-90096") {
        Some(
            "App not found in App Store Connect — create an app record first at \
             https://appstoreconnect.apple.com/apps",
        )
    } else if combined.contains("ITMS-90174") {
        Some(
            "Invalid signing — use an Apple Distribution certificate and check its expiry at \
             https://developer.apple.com",
        )
    } else if combined.contains("ITMS-90165") {
        Some(
            "Invalid API key — verify Key ID, Issuer ID, and App Manager role at \
             https://appstoreconnect.apple.com/access/integrations/api",
        )
    } else if combined.contains("ITMS-90034") {
        Some("Invalid version format — use a format like 1.0.0")
    } else if combined.contains("ITMS-90060") {
        Some(
            "Build number must be higher than the previous upload — \
             increment build_number in perry.toml",
        )
    } else if combined.contains("No suitable application records") {
        Some(
            "No App Store Connect app found for this bundle ID — \
             create the app record first at https://appstoreconnect.apple.com/apps",
        )
    } else if combined.contains("401 Unauthorized")
        || combined.contains("Error: 401")
        || combined.contains("HTTP 401")
        || combined.contains("status 401")
    {
        Some("API authentication failed — check .p8 key, Key ID, and Issuer ID")
    } else {
        None
    };

    match guidance {
        Some(msg) => format!(
            "App Store upload failed: {msg}\n\nRaw output:\nstdout: {}\nstderr: {}",
            stdout.trim(),
            stderr.trim()
        ),
        None => format!(
            "App Store upload failed:\nstdout: {}\nstderr: {}",
            stdout.trim(),
            stderr.trim()
        ),
    }
}

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
    platform: &str,
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
        .arg(platform)
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
        return Err(translate_altool_error(&stdout, &stderr));
    }

    // Upload the IPA
    let upload_output = Command::new("xcrun")
        .arg("altool")
        .arg("--upload-app")
        .arg("-f")
        .arg(ipa_path)
        .arg("--type")
        .arg(platform)
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
        return Err(translate_altool_error(&upload_stdout, &upload_stderr));
    }

    Ok(UploadResult {
        message: upload_stdout.trim().to_string(),
    })
}

pub struct UploadResult {
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_translate_itms_90062() {
        let msg = translate_altool_error("ERROR ITMS-90062: invalid profile", "");
        assert!(msg.contains("provisioning profile"), "{msg}");
        assert!(msg.contains("developer.apple.com"), "{msg}");
        assert!(msg.contains("Raw output"), "{msg}");
    }

    #[test]
    fn test_translate_itms_90096() {
        let msg = translate_altool_error("", "ERROR ITMS-90096: app not found");
        assert!(msg.contains("App not found"), "{msg}");
        assert!(msg.contains("appstoreconnect.apple.com/apps"), "{msg}");
    }

    #[test]
    fn test_translate_itms_90165() {
        let msg = translate_altool_error("ERROR ITMS-90165: invalid key", "");
        assert!(msg.contains("Invalid API key"), "{msg}");
        assert!(msg.contains("access/integrations/api"), "{msg}");
    }

    #[test]
    fn test_translate_itms_90060() {
        let msg = translate_altool_error("ERROR ITMS-90060: build number too low", "");
        assert!(msg.contains("Build number"), "{msg}");
        assert!(msg.contains("perry.toml"), "{msg}");
    }

    #[test]
    fn test_translate_no_suitable_application_records() {
        let msg = translate_altool_error("No suitable application records were found", "");
        assert!(msg.contains("bundle ID"), "{msg}");
    }

    #[test]
    fn test_translate_401() {
        let msg = translate_altool_error("", "Error: 401 Unauthorized");
        assert!(msg.contains("authentication failed"), "{msg}");
    }

    #[test]
    fn test_translate_unknown_error_includes_raw() {
        let msg = translate_altool_error("some unknown stdout error", "some stderr detail");
        assert!(msg.contains("App Store upload failed"), "{msg}");
        assert!(msg.contains("some unknown stdout error"), "{msg}");
        assert!(msg.contains("some stderr detail"), "{msg}");
    }

    #[test]
    fn test_translate_itms_90161() {
        let msg = translate_altool_error("ERROR ITMS-90161: missing icon", "");
        assert!(msg.contains("app icon"), "{msg}");
        assert!(msg.contains("1024"), "{msg}");
    }

    #[test]
    fn test_translate_itms_90174() {
        let msg = translate_altool_error("", "ERROR ITMS-90174: invalid signing");
        assert!(msg.contains("signing"), "{msg}");
        assert!(msg.contains("Apple Distribution"), "{msg}");
    }
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
        return Err(translate_altool_error(&stdout, &stderr));
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
        return Err(translate_altool_error(&upload_stdout, &upload_stderr));
    }

    Ok(UploadResult {
        message: upload_stdout.trim().to_string(),
    })
}

// ---------------------------------------------------------------------------
// App Store Connect REST API — set "What's New" release notes after upload
// ---------------------------------------------------------------------------

fn generate_asc_jwt(key_id: &str, issuer_id: &str, p8_key: &str) -> Result<String, String> {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde::Serialize;
    #[derive(Serialize)]
    struct Claims { iss: String, iat: u64, exp: u64, aud: String }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("Clock error: {e}"))?.as_secs();
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(key_id.to_string());
    encode(&header, &Claims { iss: issuer_id.to_string(), iat: now, exp: now + 1200,
        aud: "appstoreconnect-v1".to_string() },
        &EncodingKey::from_ec_pem(p8_key.as_bytes()).map_err(|e| format!("Invalid .p8: {e}"))?)
        .map_err(|e| format!("JWT failed: {e}"))
}

/// Set "What's New" release notes on the latest App Store version via ASC REST API.
pub async fn set_release_notes(
    bundle_id: &str, release_notes: &HashMap<String, String>,
    p8_key: &str, key_id: &str, issuer_id: &str,
) -> Result<(), String> {
    if release_notes.is_empty() { return Ok(()); }
    let token = generate_asc_jwt(key_id, issuer_id, p8_key)?;
    let client = reqwest::Client::new();
    let base = "https://api.appstoreconnect.apple.com/v1";

    let apps: serde_json::Value = client.get(format!("{base}/apps?filter[bundleId]={bundle_id}"))
        .bearer_auth(&token).send().await.map_err(|e| format!("ASC: {e}"))?
        .json().await.map_err(|e| format!("ASC parse: {e}"))?;
    let app_id = apps["data"].as_array().and_then(|a| a.first())
        .and_then(|a| a["id"].as_str()).ok_or_else(|| format!("App not found: {bundle_id}"))?.to_string();

    let vers: serde_json::Value = client.get(format!("{base}/apps/{app_id}/appStoreVersions?filter[appStoreState]=PREPARE_FOR_SUBMISSION,READY_FOR_REVIEW&limit=1"))
        .bearer_auth(&token).send().await.map_err(|e| format!("ASC: {e}"))?
        .json().await.map_err(|e| format!("ASC parse: {e}"))?;
    let vid = vers["data"].as_array().and_then(|a| a.first())
        .and_then(|v| v["id"].as_str()).ok_or("No editable App Store version found")?.to_string();

    let locs: serde_json::Value = client.get(format!("{base}/appStoreVersions/{vid}/appStoreVersionLocalizations"))
        .bearer_auth(&token).send().await.map_err(|e| format!("ASC: {e}"))?
        .json().await.map_err(|e| format!("ASC parse: {e}"))?;
    let existing: HashMap<String, String> = locs["data"].as_array()
        .map(|a| a.iter().filter_map(|l| Some((l["attributes"]["locale"].as_str()?.to_string(), l["id"].as_str()?.to_string()))).collect())
        .unwrap_or_default();

    let mut ok = 0u32;
    for (locale, text) in release_notes {
        let r = if let Some(lid) = existing.get(locale) {
            client.patch(format!("{base}/appStoreVersionLocalizations/{lid}")).bearer_auth(&token)
                .json(&serde_json::json!({"data":{"type":"appStoreVersionLocalizations","id":lid,"attributes":{"whatsNew":text}}}))
                .send().await
        } else {
            client.post(format!("{base}/appStoreVersionLocalizations")).bearer_auth(&token)
                .json(&serde_json::json!({"data":{"type":"appStoreVersionLocalizations",
                    "relationships":{"appStoreVersion":{"data":{"type":"appStoreVersions","id":&vid}}},
                    "attributes":{"locale":locale,"whatsNew":text}}}))
                .send().await
        };
        match r {
            Ok(r) if r.status().is_success() => ok += 1,
            Ok(r) => tracing::warn!(locale, status = %r.status(), "release notes failed"),
            Err(e) => tracing::warn!(locale, "ASC: {e}"),
        }
    }
    tracing::info!("Set release notes for {ok}/{} locale(s)", release_notes.len());
    Ok(())
}
