use crate::build::assets::{generate_android_icons, generate_icns, generate_ios_icons};
use crate::build::cleanup::{cleanup_tmpdir, create_build_tmpdir};
use crate::build::compiler;
use crate::config::WorkerConfig;
use crate::package::{android, ios, macos};
use crate::publish::{appstore, playstore};
use crate::queue::job::{BuildCredentials, BuildManifest};
use crate::signing::{android as android_signing, apple};
use crate::ws::messages::{ServerMessage, StageName};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

/// Simplified build request for the worker (no queue/broadcast internals)
pub struct BuildRequest {
    pub manifest: BuildManifest,
    pub credentials: BuildCredentials,
    pub tarball_path: PathBuf,
    pub job_id: String,
}

/// Progress sender type alias
type ProgressSender = UnboundedSender<ServerMessage>;

pub async fn execute_build(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: Arc<AtomicBool>,
    progress: ProgressSender,
) -> Result<PathBuf, String> {
    let tmpdir = create_build_tmpdir().map_err(|e| format!("Failed to create tmpdir: {e}"))?;

    let result = run_pipeline(request, config, &cancelled, &progress, &tmpdir).await;

    // Always clean up build tmpdir
    cleanup_tmpdir(&tmpdir);

    result
}

async fn run_pipeline(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
) -> Result<PathBuf, String> {
    let target = determine_target(&request.manifest.targets);

    // Stage 1: Extract tarball
    send_stage(progress, StageName::Extracting, "Extracting project archive");
    check_cancelled(cancelled)?;
    let project_dir = tmpdir.join("project");
    std::fs::create_dir_all(&project_dir)
        .map_err(|e| format!("Failed to create project dir: {e}"))?;
    extract_tarball(&request.tarball_path, &project_dir)?;
    send_progress(progress, StageName::Extracting, 100, None);

    // Stage 2: Compile
    send_stage(progress, StageName::Compiling, "Compiling TypeScript to native");
    check_cancelled(cancelled)?;
    let binary_path = tmpdir.join("output").join(&request.manifest.app_name);
    std::fs::create_dir_all(binary_path.parent().unwrap())
        .map_err(|e| format!("Failed to create output dir: {e}"))?;

    let compiler_target = match target {
        BuildTarget::Ios => Some("ios"),
        BuildTarget::Android => Some("android"),
        BuildTarget::MacOs => None,
    };
    compiler::compile(
        &request.manifest,
        progress,
        cancelled,
        &config.perry_binary,
        &project_dir,
        &binary_path,
        compiler_target,
    )
    .await?;

    let actual_binary = if target == BuildTarget::Android {
        if !binary_path.exists() {
            return Err("Compiler produced no output .so library".into());
        }
        binary_path.clone()
    } else if target == BuildTarget::Ios {
        let compiler_app = binary_path.with_extension("app");
        let inner_binary = compiler_app.join(&request.manifest.app_name);
        if inner_binary.exists() {
            let extracted = tmpdir
                .join("output")
                .join(format!("{}_ios", request.manifest.app_name));
            std::fs::copy(&inner_binary, &extracted)
                .map_err(|e| format!("Failed to extract iOS binary from compiler .app: {e}"))?;
            extracted
        } else if binary_path.exists() {
            binary_path.clone()
        } else {
            return Err(format!(
                "Compiler produced no output binary (expected {} or {})",
                binary_path.display(),
                inner_binary.display()
            ));
        }
    } else {
        if !binary_path.exists() {
            return Err("Compiler produced no output binary".into());
        }
        binary_path.clone()
    };
    send_progress(progress, StageName::Compiling, 100, None);

    match target {
        BuildTarget::MacOs => {
            run_macos_pipeline(request, config, cancelled, progress, tmpdir, &actual_binary, &project_dir)
                .await
        }
        BuildTarget::Ios => {
            run_ios_pipeline(request, config, cancelled, progress, tmpdir, &actual_binary, &project_dir)
                .await
        }
        BuildTarget::Android => {
            run_android_pipeline(request, config, cancelled, progress, tmpdir, &actual_binary, &project_dir)
                .await
        }
    }
}

async fn run_macos_pipeline(
    request: &BuildRequest,
    _config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    binary_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // Stage 3: Generate assets (icons)
    send_stage(progress, StageName::GeneratingAssets, "Generating app icons");
    check_cancelled(cancelled)?;
    let icns_path = tmpdir.join("AppIcon.icns");
    if let Some(ref icon_name) = request.manifest.icon {
        let icon_src = project_dir.join(icon_name);
        if icon_src.exists() {
            generate_icns(&icon_src, &icns_path)?;
        }
    }
    send_progress(progress, StageName::GeneratingAssets, 100, None);

    // Stage 4: Bundle .app
    send_stage(progress, StageName::Bundling, "Creating macOS .app bundle");
    check_cancelled(cancelled)?;
    let app_path = tmpdir.join(format!("{}.app", request.manifest.app_name));
    let icns_opt = if icns_path.exists() {
        Some(icns_path.as_path())
    } else {
        None
    };
    macos::create_app_bundle(&request.manifest, binary_path, icns_opt, &app_path)?;
    send_progress(progress, StageName::Bundling, 100, None);

    // Stage 5: Code sign
    send_stage(progress, StageName::Signing, "Signing application");
    check_cancelled(cancelled)?;
    let has_signing = request.credentials.apple_signing_identity.is_some();
    if has_signing {
        let entitlements_path = if request.manifest.entitlements.is_some() {
            let p = tmpdir.join("entitlements.plist");
            macos::write_entitlements_plist(&request.manifest, &p)?;
            Some(p)
        } else {
            None
        };
        apple::codesign_app(
            request.credentials.apple_signing_identity.as_deref().unwrap(),
            entitlements_path.as_deref(),
            &app_path,
        )
        .await?;
    }
    send_progress(progress, StageName::Signing, 100, None);

    // Stage 6: Package DMG
    send_stage(progress, StageName::Packaging, "Creating DMG");
    check_cancelled(cancelled)?;
    let dmg_path = tmpdir.join(format!("{}.dmg", request.manifest.app_name));
    macos::create_dmg(&request.manifest.app_name, &app_path, &dmg_path).await?;
    send_progress(progress, StageName::Packaging, 100, None);

    // Stage 7: Notarize
    send_stage(progress, StageName::Notarizing, "Notarizing with Apple");
    check_cancelled(cancelled)?;
    let has_notarization = request.credentials.apple_key_id.is_some()
        && request.credentials.apple_issuer_id.is_some()
        && request.credentials.apple_p8_key.is_some();
    if has_notarization {
        apple::notarize_dmg(
            &dmg_path,
            request.credentials.apple_p8_key.as_deref().unwrap(),
            request.credentials.apple_key_id.as_deref().unwrap(),
            request.credentials.apple_issuer_id.as_deref().unwrap(),
            tmpdir,
        )
        .await?;
    }
    send_progress(progress, StageName::Notarizing, 100, None);

    // Copy artifact to stable location
    let artifact_path = copy_artifact(&dmg_path, &request.manifest.app_name, &request.job_id, "dmg")?;
    Ok(artifact_path)
}

async fn run_ios_pipeline(
    request: &BuildRequest,
    _config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    binary_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // Stage 3: Generate iOS assets (icons)
    send_stage(progress, StageName::GeneratingAssets, "Generating iOS app icons");
    check_cancelled(cancelled)?;
    let icons_dir = tmpdir.join("ios_icons");
    if let Some(ref icon_name) = request.manifest.icon {
        let icon_src = project_dir.join(icon_name);
        if icon_src.exists() {
            generate_ios_icons(&icon_src, &icons_dir)?;
        }
    }
    send_progress(progress, StageName::GeneratingAssets, 100, None);

    // Stage 4: Bundle iOS .app
    send_stage(progress, StageName::Bundling, "Creating iOS .app bundle");
    check_cancelled(cancelled)?;
    let app_path = tmpdir.join(format!("{}.app", request.manifest.app_name));

    let profile_path = if let Some(ref b64) = request.credentials.provisioning_profile_base64 {
        let decoded = base64_decode(b64)?;
        let p = tmpdir.join("embedded.mobileprovision");
        std::fs::write(&p, decoded)
            .map_err(|e| format!("Failed to write provisioning profile: {e}"))?;
        Some(p)
    } else {
        None
    };

    let icon_png = if icons_dir.join("Icon-1024.png").exists() {
        Some(icons_dir.join("Icon-1024.png"))
    } else {
        None
    };

    ios::create_ios_app_bundle(
        &request.manifest,
        binary_path,
        icon_png.as_deref(),
        profile_path.as_deref(),
        &app_path,
    )?;

    if icons_dir.exists() {
        for entry in std::fs::read_dir(&icons_dir).map_err(|e| format!("Read icons dir: {e}"))? {
            let entry = entry.map_err(|e| format!("Icon entry: {e}"))?;
            std::fs::copy(entry.path(), app_path.join(entry.file_name()))
                .map_err(|e| format!("Copy icon: {e}"))?;
        }
    }
    send_progress(progress, StageName::Bundling, 100, None);

    // Stage 5: Code sign iOS app
    send_stage(progress, StageName::Signing, "Signing iOS application");
    check_cancelled(cancelled)?;
    let has_signing = request.credentials.apple_signing_identity.is_some();
    if has_signing {
        let entitlements_path = {
            let p = tmpdir.join("entitlements.plist");
            ios::write_ios_entitlements_plist(&request.manifest, &p)?;
            p
        };
        apple::codesign_app(
            request.credentials.apple_signing_identity.as_deref().unwrap(),
            Some(&entitlements_path),
            &app_path,
        )
        .await?;
    }
    send_progress(progress, StageName::Signing, 100, None);

    // Stage 6: Package .ipa
    send_stage(progress, StageName::Packaging, "Creating .ipa");
    check_cancelled(cancelled)?;
    let ipa_path = tmpdir.join(format!("{}.ipa", request.manifest.app_name));
    ios::create_ipa(&request.manifest.app_name, &app_path, &ipa_path).await?;
    send_progress(progress, StageName::Packaging, 100, None);

    // Stage 7: Upload to App Store Connect (if configured)
    let has_appstore_creds = request.credentials.apple_key_id.is_some()
        && request.credentials.apple_issuer_id.is_some()
        && request.credentials.apple_p8_key.is_some();
    let should_upload = has_appstore_creds
        && request
            .manifest
            .ios_distribute
            .as_deref()
            .map(|d| d == "appstore" || d == "testflight")
            .unwrap_or(false);

    if should_upload {
        send_stage(progress, StageName::Publishing, "Uploading to App Store Connect");
        check_cancelled(cancelled)?;

        let result = appstore::upload_to_appstore(
            &ipa_path,
            request.credentials.apple_p8_key.as_deref().unwrap(),
            request.credentials.apple_key_id.as_deref().unwrap(),
            request.credentials.apple_issuer_id.as_deref().unwrap(),
            tmpdir,
        )
        .await?;

        let _ = progress.send(ServerMessage::Published {
            platform: "ios".into(),
            message: result.message,
            url: None,
        });
        send_progress(progress, StageName::Publishing, 100, None);
    } else {
        send_stage(
            progress,
            StageName::Publishing,
            "Skipping App Store upload (no credentials or distribute not set)",
        );
        send_progress(progress, StageName::Publishing, 100, None);
    }

    let artifact_path = copy_artifact(&ipa_path, &request.manifest.app_name, &request.job_id, "ipa")?;
    Ok(artifact_path)
}

async fn run_android_pipeline(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    so_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // Stage 3: Generate Android assets (icons)
    send_stage(
        progress,
        StageName::GeneratingAssets,
        "Generating Android app icons",
    );
    check_cancelled(cancelled)?;
    let icons_dir = tmpdir.join("android_icons");
    if let Some(ref icon_name) = request.manifest.icon {
        let icon_src = project_dir.join(icon_name);
        if icon_src.exists() {
            generate_android_icons(&icon_src, &icons_dir)?;
        }
    }
    send_progress(progress, StageName::GeneratingAssets, 100, None);

    // Stage 4: Bundle — Create Android Gradle project and build APK
    send_stage(
        progress,
        StageName::Bundling,
        "Creating Android project and building APK",
    );
    check_cancelled(cancelled)?;

    let keystore_path = if let Some(ref b64) = request.credentials.android_keystore_base64 {
        let decoded = base64_decode(b64)?;
        let p = tmpdir.join("release.keystore");
        std::fs::write(&p, decoded)
            .map_err(|e| format!("Failed to write keystore: {e}"))?;
        Some(p)
    } else {
        None
    };

    let icons_opt = if icons_dir.exists() {
        Some(icons_dir.as_path())
    } else {
        None
    };

    let android_project = android::create_android_project(
        &request.manifest,
        &config.perry_binary,
        so_path,
        icons_opt,
        tmpdir,
    )?;

    let is_playstore = request.manifest.android_distribute.as_deref() == Some("playstore");

    // Create a broadcast sender for the android build (Gradle streaming)
    let (gradle_tx, _) = tokio::sync::broadcast::channel(256);
    let artifact_path = if is_playstore {
        android::build_aab(&android_project, Some(&gradle_tx)).await?
    } else {
        android::build_apk(&android_project, true, Some(&gradle_tx)).await?
    };
    send_progress(progress, StageName::Bundling, 100, None);

    // Stage 5: Sign
    send_stage(progress, StageName::Signing, "Signing Android artifact");
    check_cancelled(cancelled)?;

    let final_artifact = if let Some(ref ks_path) = keystore_path {
        let ks_pass = request
            .credentials
            .android_keystore_password
            .as_deref()
            .unwrap_or("");
        let key_alias = request
            .credentials
            .android_key_alias
            .as_deref()
            .unwrap_or("key0");
        let key_pass = request
            .credentials
            .android_key_password
            .as_deref()
            .unwrap_or(ks_pass);

        if is_playstore {
            android_signing::sign_aab(&artifact_path, ks_path, ks_pass, key_alias, key_pass)
                .await?;
            artifact_path.clone()
        } else {
            android_signing::sign_apk(&artifact_path, ks_path, ks_pass, key_alias, key_pass)
                .await?
        }
    } else {
        artifact_path.clone()
    };

    if let Some(ref ks_path) = keystore_path {
        std::fs::remove_file(ks_path).ok();
    }
    send_progress(progress, StageName::Signing, 100, None);

    // Stage 6: Packaging
    send_stage(progress, StageName::Packaging, "Finalizing Android package");
    send_progress(progress, StageName::Packaging, 100, None);

    // Stage 7: Publishing
    if is_playstore {
        send_stage(progress, StageName::Publishing, "Uploading to Google Play");
        check_cancelled(cancelled)?;

        let play_track = request
            .manifest
            .android_distribute
            .as_deref()
            .and_then(|d| {
                // If distribute is "playstore", default track is "internal"
                // Could be extended to parse "playstore:beta" etc.
                if d == "playstore" { Some("internal") } else { None }
            })
            .unwrap_or("internal");

        match playstore::upload_to_playstore(
            &final_artifact,
            &request.manifest.bundle_id,
            request.credentials.google_play_service_account_json.as_deref(),
            play_track,
        ).await {
            Ok(result) => {
                let _ = progress.send(ServerMessage::Published {
                    platform: "android".into(),
                    message: result.message,
                    url: None,
                });
            }
            Err(e) => {
                let _ = progress.send(ServerMessage::Log {
                    stage: StageName::Publishing,
                    line: format!("Play Store upload skipped: {e}"),
                    stream: crate::ws::messages::LogStream::Stderr,
                });
            }
        }
        send_progress(progress, StageName::Publishing, 100, None);
    } else {
        send_stage(
            progress,
            StageName::Publishing,
            "Skipping store upload (distribute not set to playstore)",
        );
        send_progress(progress, StageName::Publishing, 100, None);
    }

    let ext = if is_playstore { "aab" } else { "apk" };
    let artifact_path =
        copy_artifact(&final_artifact, &request.manifest.app_name, &request.job_id, ext)?;
    Ok(artifact_path)
}

/// Copy artifact to a stable location (outside the build tmpdir that gets cleaned up)
fn copy_artifact(
    source: &std::path::Path,
    app_name: &str,
    job_id: &str,
    ext: &str,
) -> Result<PathBuf, String> {
    let artifact_dir = std::env::temp_dir().join("perry-artifacts");
    std::fs::create_dir_all(&artifact_dir)
        .map_err(|e| format!("Failed to create artifact dir: {e}"))?;

    let dest = artifact_dir.join(format!("{app_name}-{job_id}.{ext}"));
    std::fs::copy(source, &dest).map_err(|e| format!("Failed to copy artifact: {e}"))?;
    Ok(dest)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildTarget {
    MacOs,
    Ios,
    Android,
}

fn determine_target(targets: &[String]) -> BuildTarget {
    for t in targets {
        match t.to_lowercase().as_str() {
            "ios" => return BuildTarget::Ios,
            "android" => return BuildTarget::Android,
            _ => {}
        }
    }
    BuildTarget::MacOs
}

fn check_cancelled(cancelled: &Arc<AtomicBool>) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("Build cancelled".into())
    } else {
        Ok(())
    }
}

fn send_stage(progress: &ProgressSender, stage: StageName, message: &str) {
    let _ = progress.send(ServerMessage::Stage {
        stage,
        message: message.to_string(),
    });
}

fn send_progress(progress: &ProgressSender, stage: StageName, percent: u8, message: Option<&str>) {
    let _ = progress.send(ServerMessage::Progress {
        stage,
        percent,
        message: message.map(String::from),
    });
}

fn extract_tarball(tarball_path: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    let file =
        std::fs::File::open(tarball_path).map_err(|e| format!("Failed to open tarball: {e}"))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(dest)
        .map_err(|e| format!("Failed to extract tarball: {e}"))?;
    Ok(())
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| format!("Invalid base64: {e}"))
}
