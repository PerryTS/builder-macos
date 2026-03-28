use crate::build::assets::{
    compile_ios_icon_asset_catalog, generate_android_icons, generate_icns, generate_ios_icons,
};
use crate::build::cleanup::{cleanup_tmpdir, create_build_tmpdir};
use crate::build::compiler;
use crate::build::validate;
use crate::build::verify;
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
    // Check if this is a sign-only job (precompiled by Linux worker)
    let target = determine_target(&request.manifest.targets);
    if matches!(target, BuildTarget::MacOsSign | BuildTarget::IosSign) {
        return run_sign_only_pipeline(request, config, &cancelled, &progress, &target).await;
    }

    // Validate manifest fields before any filesystem or subprocess operations
    validate::validate_manifest(&request.manifest)?;

    // If Tart VM isolation is enabled, delegate the entire build to a fresh VM
    if config.tart_enabled() {
        return super::tart::execute_build_in_vm(request, config, cancelled, progress).await;
    }

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
        BuildTarget::MacOsSign | BuildTarget::IosSign => unreachable!("sign-only handled earlier"),
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
        // Use only the filename component of app_name to prevent path traversal
        // (e.g. app_name = "../../malicious" would escape the .app directory)
        let safe_name = std::path::Path::new(&request.manifest.app_name)
            .file_name()
            .ok_or_else(|| "app_name is not a valid filename".to_string())?;
        let inner_binary = compiler_app.join(safe_name);
        if inner_binary.exists() {
            let extracted = tmpdir
                .join("output")
                .join(format!("{}_ios", safe_name.to_string_lossy()));
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
        BuildTarget::MacOsSign | BuildTarget::IosSign => unreachable!("sign-only handled earlier"),
    }
}

async fn run_macos_pipeline(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    binary_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    let distribute = request.manifest.macos_distribute.as_deref().unwrap_or("notarize");
    let is_appstore = distribute == "appstore" || distribute == "testflight";
    let is_both = distribute == "both";

    let mac_sdk_info = query_macos_sdk_info().await;

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
    let icns_opt = if icns_path.exists() { Some(icns_path.as_path()) } else { None };
    macos::create_app_bundle(&request.manifest, binary_path, icns_opt, &app_path, Some(&mac_sdk_info), None)?;
    send_progress(progress, StageName::Bundling, 100, None);

    if is_both {
        // --- "both" mode: two passes ---
        // Pass 1: Sign with Developer ID → create DMG → notarize
        // Pass 2: Re-sign with Apple Distribution → create .pkg → upload to App Store Connect

        // -- Pass 1: Notarize DMG --
        send_stage(progress, StageName::Signing, "Signing with Developer ID (for notarization)");
        check_cancelled(cancelled)?;

        let notarize_kc = if let (Some(p12_b64), Some(p12_pass)) = (
            request.credentials.apple_notarize_certificate_p12_base64.as_deref(),
            request.credentials.apple_notarize_certificate_password.as_deref(),
        ) {
            apple::TempKeychain::create(
                &format!("{}-notarize", request.job_id),
                p12_b64, p12_pass, tmpdir,
                request.credentials.apple_notarize_signing_identity.as_deref(),
            ).await.ok()
        } else {
            None
        };
        let notarize_identity = notarize_kc.as_ref()
            .map(|kc| kc.identity.as_str())
            .or_else(|| request.credentials.apple_notarize_signing_identity.as_deref());

        if let Some(identity) = notarize_identity {
            let entitlements_path = if request.manifest.entitlements.is_some() {
                let p = tmpdir.join("entitlements.plist");
                macos::write_entitlements_plist(&request.manifest, &p)?;
                Some(p)
            } else {
                None
            };
            let notarize_p12 = request.credentials.apple_notarize_certificate_p12_base64.as_deref()
                .map(|b64| write_p12_temp(tmpdir, b64, "notarize")).transpose()?;
            apple::codesign_app(identity, entitlements_path.as_deref(), &app_path, true, None,
                notarize_p12.as_deref(), request.credentials.apple_notarize_certificate_password.as_deref(),
            ).await?;
        }
        send_progress(progress, StageName::Signing, 50, Some("Developer ID signed"));

        // Create DMG
        let dmg_path = tmpdir.join(format!("{}.dmg", request.manifest.app_name));
        send_stage(progress, StageName::Packaging, "Creating DMG");
        check_cancelled(cancelled)?;
        macos::create_dmg(&request.manifest.app_name, &app_path, &dmg_path).await?;

        // Notarize DMG
        send_stage(progress, StageName::Notarizing, "Notarizing DMG with Apple");
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

        // Clean up notarize keychain
        if let Some(ref kc) = notarize_kc {
            kc.remove_from_search_list();
        }

        // Save DMG artifact
        let dmg_artifact = copy_artifact(&dmg_path, &request.manifest.app_name, &request.job_id, "dmg")?;

        // -- Pass 2: App Store upload --
        // Re-create .app bundle (codesign modifies in-place, so start fresh)
        send_stage(progress, StageName::Signing, "Re-signing with Apple Distribution (for App Store)");
        check_cancelled(cancelled)?;

        // Decode provisioning profile for App Store / TestFlight
        let macos_profile_path = if let Some(ref b64) = request.credentials.provisioning_profile_base64 {
            let decoded = base64_decode(b64)?;
            let p = tmpdir.join("embedded.provisionprofile");
            std::fs::write(&p, decoded)
                .map_err(|e| format!("Failed to write provisioning profile: {e}"))?;
            Some(p)
        } else {
            None
        };

        let app_path_appstore = tmpdir.join(format!("{}-appstore.app", request.manifest.app_name));
        macos::create_app_bundle(&request.manifest, binary_path, icns_opt, &app_path_appstore, Some(&mac_sdk_info),
            macos_profile_path.as_deref())?;

        let appstore_kc = if let (Some(p12_b64), Some(p12_pass)) = (
            request.credentials.apple_certificate_p12_base64.as_deref(),
            request.credentials.apple_certificate_password.as_deref(),
        ) {
            match apple::TempKeychain::create(
                &format!("{}-appstore", request.job_id),
                p12_b64, p12_pass, tmpdir,
                request.credentials.apple_signing_identity.as_deref(),
            ).await {
                Ok(kc) => {
                    if let (Some(inst_b64), Some(inst_pass)) = (
                        request.credentials.apple_installer_certificate_p12_base64.as_deref(),
                        request.credentials.apple_installer_certificate_password.as_deref(),
                    ) {
                        let _ = kc.import_additional_p12(inst_b64, inst_pass, tmpdir);
                    }
                    Some(kc)
                }
                Err(e) => { tracing::warn!("TempKeychain failed: {e}"); None }
            }
        } else {
            None
        };
        let appstore_identity = appstore_kc.as_ref()
            .map(|kc| kc.identity.as_str())
            .or_else(|| request.credentials.apple_signing_identity.as_deref());

        if let Some(identity) = appstore_identity {
            let entitlements_path = if request.manifest.entitlements.is_some() {
                let p = tmpdir.join("entitlements-appstore.plist");
                macos::write_entitlements_plist(&request.manifest, &p)?;
                Some(p)
            } else {
                None
            };
            let appstore_p12 = request.credentials.apple_certificate_p12_base64.as_deref()
                .map(|b64| write_p12_temp(tmpdir, b64, "appstore")).transpose()?;
            apple::codesign_app(identity, entitlements_path.as_deref(), &app_path_appstore, false, None,
                appstore_p12.as_deref(), request.credentials.apple_certificate_password.as_deref(),
            ).await?;
        }
        send_progress(progress, StageName::Signing, 100, Some("Apple Distribution signed"));

        // Create .pkg
        send_stage(progress, StageName::Packaging, "Creating installer package (.pkg)");
        check_cancelled(cancelled)?;
        let pkg_path = tmpdir.join(format!("{}.pkg", request.manifest.app_name));
        let installer_identity = appstore_kc.as_ref()
            .and_then(|kc| apple::find_installer_identity(&kc.path))
            .unwrap_or_default();
        if installer_identity.is_empty() {
            return Err("No installer signing identity found. Ensure a Mac Installer Distribution certificate is available.".to_string());
        }
        if let Some(ref kc) = appstore_kc {
            let _ = kc.add_to_search_list();
        }
        macos::create_pkg(&app_path_appstore, &pkg_path, &installer_identity).await?;
        if let Some(ref kc) = appstore_kc {
            kc.remove_from_search_list();
        }
        send_progress(progress, StageName::Packaging, 100, None);

        // Verify binary before publishing
        run_verification(config, progress, cancelled, &dmg_artifact, "macos-arm64", "gui").await?;

        // Upload to App Store Connect
        if !has_notarization {
            return Err(
                "macos.distribute = \"both\" requires App Store Connect API credentials. \
                 Run `perry setup macos` or pass --apple-key-id / --apple-issuer-id / --apple-p8-key."
                    .to_string(),
            );
        }
        send_stage(progress, StageName::Publishing, "Uploading to App Store Connect");
        check_cancelled(cancelled)?;
        let result = appstore::upload_macos_to_appstore(
            &pkg_path,
            request.credentials.apple_p8_key.as_deref().unwrap(),
            request.credentials.apple_key_id.as_deref().unwrap(),
            request.credentials.apple_issuer_id.as_deref().unwrap(),
            tmpdir,
        )
        .await?;
        let _ = progress.send(ServerMessage::Published {
            platform: "macos".into(),
            message: format!("{} (DMG also available)", result.message),
            url: None,
        });
        send_progress(progress, StageName::Publishing, 100, None);

        // Return the DMG as the primary artifact (App Store upload is done via altool)
        Ok(dmg_artifact)
    } else {
        // --- Single-mode: appstore OR notarize ---

        // Embed provisioning profile for App Store / TestFlight (single-mode)
        if is_appstore {
            if let Some(ref b64) = request.credentials.provisioning_profile_base64 {
                let decoded = base64_decode(b64)?;
                let profile_dest = app_path.join("Contents/embedded.provisionprofile");
                std::fs::write(&profile_dest, decoded)
                    .map_err(|e| format!("Failed to embed provisioning profile: {e}"))?;
            }
        }

        // Stage 5: Code sign
        send_stage(progress, StageName::Signing, "Signing application");
        check_cancelled(cancelled)?;

        // Try creating TempKeychain for legacy codesign fallback, but don't fail if it errors
        // (rcodesign doesn't need it — it reads the p12 directly)
        let temp_kc = if let (Some(p12_b64), Some(p12_pass)) = (
            request.credentials.apple_certificate_p12_base64.as_deref(),
            request.credentials.apple_certificate_password.as_deref(),
        ) {
            match apple::TempKeychain::create(&request.job_id, p12_b64, p12_pass, tmpdir, request.credentials.apple_signing_identity.as_deref()).await {
                Ok(kc) => {
                    if let (Some(inst_b64), Some(inst_pass)) = (
                        request.credentials.apple_installer_certificate_p12_base64.as_deref(),
                        request.credentials.apple_installer_certificate_password.as_deref(),
                    ) {
                        let _ = kc.import_additional_p12(inst_b64, inst_pass, tmpdir);
                    }
                    Some(kc)
                }
                Err(e) => {
                    tracing::warn!("TempKeychain creation failed (will use rcodesign): {e}");
                    None
                }
            }
        } else {
            None
        };
        let effective_identity = temp_kc.as_ref()
            .map(|kc| kc.identity.as_str())
            .or_else(|| request.credentials.apple_signing_identity.as_deref());

        if let Some(identity) = effective_identity {
            let entitlements_path = if request.manifest.entitlements.is_some() {
                let p = tmpdir.join("entitlements.plist");
                macos::write_entitlements_plist(&request.manifest, &p)?;
                Some(p)
            } else {
                None
            };
            let single_p12 = request.credentials.apple_certificate_p12_base64.as_deref()
                .map(|b64| write_p12_temp(tmpdir, b64, "single")).transpose()?;
            apple::codesign_app(
                identity,
                entitlements_path.as_deref(),
                &app_path,
                !is_appstore, // hardened runtime for notarization, not needed for App Store
                None,
                single_p12.as_deref(), request.credentials.apple_certificate_password.as_deref(),
            )
            .await?;
        }
        send_progress(progress, StageName::Signing, 100, None);

        if is_appstore {
            // App Store path: create .pkg and upload to App Store Connect

            // Stage 6: Package .pkg
            send_stage(progress, StageName::Packaging, "Creating installer package (.pkg)");
            check_cancelled(cancelled)?;
            let pkg_path = tmpdir.join(format!("{}.pkg", request.manifest.app_name));
            let installer_identity = temp_kc.as_ref()
                .and_then(|kc| apple::find_installer_identity(&kc.path))
                .unwrap_or_default();
            if let Some(ref kc) = temp_kc {
                let _ = kc.add_to_search_list();
            }
            macos::create_pkg(&app_path, &pkg_path, &installer_identity).await?;
            if let Some(ref kc) = temp_kc {
                kc.remove_from_search_list();
            }
            send_progress(progress, StageName::Packaging, 100, None);

            // Verify binary before publishing
            run_verification(config, progress, cancelled, &pkg_path, "macos-arm64", "gui").await?;

            // Stage 7: Upload to App Store Connect
            let has_creds = request.credentials.apple_key_id.is_some()
                && request.credentials.apple_issuer_id.is_some()
                && request.credentials.apple_p8_key.is_some();
            if !has_creds {
                return Err(
                    "macos.distribute = \"appstore\" requires App Store Connect API credentials. \
                     Run `perry setup macos` or pass --apple-key-id / --apple-issuer-id / --apple-p8-key."
                        .to_string(),
                );
            }
            send_stage(progress, StageName::Publishing, "Uploading to App Store Connect");
            check_cancelled(cancelled)?;
            let result = appstore::upload_macos_to_appstore(
                &pkg_path,
                request.credentials.apple_p8_key.as_deref().unwrap(),
                request.credentials.apple_key_id.as_deref().unwrap(),
                request.credentials.apple_issuer_id.as_deref().unwrap(),
                tmpdir,
            )
            .await?;
            let _ = progress.send(ServerMessage::Published {
                platform: "macos".into(),
                message: result.message,
                url: None,
            });
            send_progress(progress, StageName::Publishing, 100, None);

            let artifact_path = copy_artifact(&pkg_path, &request.manifest.app_name, &request.job_id, "pkg")?;
            Ok(artifact_path)
        } else {
            // Notarize path: create .dmg, notarize, return DMG

            // Stage 6: Package + Notarize
            let has_notarization = request.credentials.apple_key_id.is_some()
                && request.credentials.apple_issuer_id.is_some()
                && request.credentials.apple_p8_key.is_some();

            let dmg_path = tmpdir.join(format!("{}.dmg", request.manifest.app_name));

            if has_notarization {
                // Create initial DMG for notarization submission
                send_stage(progress, StageName::Packaging, "Creating DMG for notarization");
                check_cancelled(cancelled)?;
                macos::create_dmg(&request.manifest.app_name, &app_path, &dmg_path).await?;
                send_progress(progress, StageName::Packaging, 50, None);

                // Notarize the DMG
                send_stage(progress, StageName::Notarizing, "Submitting to Apple for notarization");
                check_cancelled(cancelled)?;
                apple::notarize_dmg(
                    &dmg_path,
                    request.credentials.apple_p8_key.as_deref().unwrap(),
                    request.credentials.apple_key_id.as_deref().unwrap(),
                    request.credentials.apple_issuer_id.as_deref().unwrap(),
                    tmpdir,
                )
                .await?;

                // Staple the notarization ticket to the .app
                send_stage(progress, StageName::Notarizing, "Stapling notarization ticket");
                let _ = tokio::process::Command::new("xcrun")
                    .args(["stapler", "staple", app_path.to_str().unwrap_or("")])
                    .output()
                    .await;

                // Recreate DMG with the stapled .app
                send_stage(progress, StageName::Packaging, "Recreating DMG with stapled app");
                let _ = std::fs::remove_file(&dmg_path);
                macos::create_dmg(&request.manifest.app_name, &app_path, &dmg_path).await?;

                // Sign the DMG itself
                let sign_identity = temp_kc.as_ref()
                    .map(|kc| kc.identity.as_str())
                    .unwrap_or("Developer ID Application");
                let kc_path = temp_kc.as_ref().map(|kc| kc.path.as_str());
                let mut sign_cmd = tokio::process::Command::new("codesign");
                sign_cmd.arg("--force").arg("--sign").arg(sign_identity);
                if let Some(kc) = kc_path {
                    sign_cmd.arg("--keychain").arg(kc);
                }
                sign_cmd.arg(&dmg_path);
                let sign_out = sign_cmd.output().await;
                if let Ok(ref o) = sign_out {
                    if !o.status.success() {
                        tracing::warn!("DMG signing failed (non-fatal): {}", String::from_utf8_lossy(&o.stderr));
                    }
                }

                // Notarize the final signed DMG
                send_stage(progress, StageName::Notarizing, "Notarizing signed DMG");
                apple::notarize_dmg(
                    &dmg_path,
                    request.credentials.apple_p8_key.as_deref().unwrap(),
                    request.credentials.apple_key_id.as_deref().unwrap(),
                    request.credentials.apple_issuer_id.as_deref().unwrap(),
                    tmpdir,
                )
                .await?;

                // Staple the DMG
                let _ = tokio::process::Command::new("xcrun")
                    .args(["stapler", "staple", dmg_path.to_str().unwrap_or("")])
                    .output()
                    .await;

                send_progress(progress, StageName::Packaging, 100, None);
                send_progress(progress, StageName::Notarizing, 100, None);
            } else {
                // No notarization credentials — just create unsigned DMG
                send_stage(progress, StageName::Packaging, "Creating DMG");
                check_cancelled(cancelled)?;
                macos::create_dmg(&request.manifest.app_name, &app_path, &dmg_path).await?;
                send_progress(progress, StageName::Packaging, 100, None);
                send_progress(progress, StageName::Notarizing, 100, None);
            }

            // Verify binary before returning
            run_verification(config, progress, cancelled, &dmg_path, "macos-arm64", "gui").await?;

            let artifact_path = copy_artifact(&dmg_path, &request.manifest.app_name, &request.job_id, "dmg")?;
            Ok(artifact_path)
        }
    }
}

async fn run_ios_pipeline(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    tmpdir: &std::path::Path,
    binary_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> Result<PathBuf, String> {
    // Query Xcode/SDK info for Info.plist DT* keys
    let sdk_info = query_sdk_info().await;
    let team_id = request.credentials.apple_team_id.as_deref().unwrap_or("");

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
        Some(&sdk_info),
    )?;

    if icons_dir.exists() {
        // Copy individual PNGs into bundle (required by altool validation for all iOS versions)
        for entry in std::fs::read_dir(&icons_dir).map_err(|e| format!("Read icons dir: {e}"))? {
            let entry = entry.map_err(|e| format!("Icon entry: {e}"))?;
            std::fs::copy(entry.path(), app_path.join(entry.file_name()))
                .map_err(|e| format!("Copy icon: {e}"))?;
        }
        // Compile icon asset catalog → Assets.car (required for iOS 11+ App Store)
        let deployment_target = request
            .manifest
            .ios_deployment_target
            .as_deref()
            .unwrap_or("17.0");
        match compile_ios_icon_asset_catalog(&icons_dir, deployment_target, tmpdir).await {
            Ok(assets_car) => {
                std::fs::copy(&assets_car, app_path.join("Assets.car"))
                    .map_err(|e| format!("Failed to copy Assets.car into bundle: {e}"))?;
            }
            Err(e) => {
                // Log but don't fail — individual PNGs are still in the bundle
                let _ = progress.send(crate::ws::messages::ServerMessage::Log {
                    stage: crate::ws::messages::StageName::Bundling,
                    line: format!("Warning: asset catalog compilation failed: {e}"),
                    stream: crate::ws::messages::LogStream::Stderr,
                });
            }
        }
    }
    send_progress(progress, StageName::Bundling, 100, None);

    // Stage 5: Code sign iOS app
    send_stage(progress, StageName::Signing, "Signing iOS application");
    check_cancelled(cancelled)?;
    let temp_kc = if let (Some(p12_b64), Some(p12_pass)) = (
        request.credentials.apple_certificate_p12_base64.as_deref(),
        request.credentials.apple_certificate_password.as_deref(),
    ) {
        apple::TempKeychain::create(&request.job_id, p12_b64, p12_pass, tmpdir, request.credentials.apple_signing_identity.as_deref()).await.ok()
    } else {
        None
    };
    let effective_identity = temp_kc.as_ref()
        .map(|kc| kc.identity.as_str())
        .or_else(|| request.credentials.apple_signing_identity.as_deref());

    if let Some(identity) = effective_identity {
        let entitlements_path = {
            let p = tmpdir.join("entitlements.plist");
            ios::write_ios_entitlements_plist(&request.manifest, team_id, &p)?;
            p
        };
        // Add temp keychain to search list so codesign can find the private key
        if let Some(ref kc) = temp_kc {
            kc.add_to_search_list().map_err(|e| format!("Failed to add keychain to search list: {e}"))?;
        }
        let ios_p12 = request.credentials.apple_certificate_p12_base64.as_deref()
            .map(|b64| write_p12_temp(tmpdir, b64, "ios")).transpose()?;
        apple::codesign_app(
            identity,
            Some(&entitlements_path),
            &app_path,
            false, // iOS: no hardened runtime flag
            None,
            ios_p12.as_deref(), request.credentials.apple_certificate_password.as_deref(),
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

    // Verify binary before publishing
    run_verification(config, progress, cancelled, &ipa_path, "ios-arm64", "gui").await?;

    // Stage 7: Upload to App Store Connect (if configured)
    let has_appstore_creds = request.credentials.apple_key_id.is_some()
        && request.credentials.apple_issuer_id.is_some()
        && request.credentials.apple_p8_key.is_some();
    let ios_distribute = request.manifest.ios_distribute.as_deref();
    let wants_upload = ios_distribute
        .map(|d| d == "appstore" || d == "testflight")
        .unwrap_or(false);

    if wants_upload {
        if !has_appstore_creds {
            return Err(format!(
                "ios.distribute = \"{}\" requires App Store Connect API credentials. \
                 Run `perry setup ios` or pass --apple-key-id / --apple-issuer-id / --apple-p8-key.",
                ios_distribute.unwrap_or("")
            ));
        }
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
            "Skipping App Store upload (distribute not set)",
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

    let is_playstore = request
        .manifest
        .android_distribute
        .as_deref()
        .map(|d| d == "playstore" || d.starts_with("playstore:"))
        .unwrap_or(false);

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

    // Verify binary before publishing
    run_verification(config, progress, cancelled, &final_artifact, "android-arm64", "gui").await?;

    // Stage 7: Publishing
    if is_playstore {
        send_stage(progress, StageName::Publishing, "Uploading to Google Play");
        check_cancelled(cancelled)?;

        let distribute_str = request.manifest.android_distribute.as_deref().unwrap_or("playstore");
        let play_track = parse_playstore_track(request.manifest.android_distribute.as_deref())
            .ok_or_else(|| {
                format!(
                    "Invalid Play Store track in distribute = \"{distribute_str}\". \
                     Valid: playstore, playstore:internal, playstore:alpha, playstore:beta, playstore:production"
                )
            })?;

        let play_result = playstore::upload_to_playstore(
            &final_artifact,
            &request.manifest.bundle_id,
            request.credentials.google_play_service_account_json.as_deref(),
            play_track,
        )
        .await
        .map_err(|e| format!("Google Play upload failed: {e}"))?;

        let _ = progress.send(ServerMessage::Published {
            platform: "android".into(),
            message: play_result.message,
            url: None,
        });
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

/// Run verification if a verify URL is configured.
/// Sends the artifact to perry-verify and blocks until the result is known.
/// Verification failure aborts the build (prevents publishing broken binaries).
async fn run_verification(
    config: &WorkerConfig,
    progress: &ProgressSender,
    cancelled: &Arc<AtomicBool>,
    artifact_path: &std::path::Path,
    target: &str,
    app_type: &str,
) -> Result<(), String> {
    let verify_url = match config.verify_url.as_deref() {
        Some(url) => url,
        None => return Ok(()), // no verify URL configured — skip
    };

    send_stage(progress, StageName::Verifying, "Verifying binary");
    check_cancelled(cancelled)?;

    verify::verify_binary(artifact_path, verify_url, target, app_type, progress).await?;
    send_progress(progress, StageName::Verifying, 100, None);

    Ok(())
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

/// Sign-only pipeline for precompiled bundles from Linux cross-compilation.
/// The tarball contains a precompiled .ipa (iOS) or .tar.gz with .app (macOS),
/// not source code. We extract, sign, and package/upload.
async fn run_sign_only_pipeline(
    request: &BuildRequest,
    config: &WorkerConfig,
    cancelled: &Arc<AtomicBool>,
    progress: &ProgressSender,
    target: &BuildTarget,
) -> Result<PathBuf, String> {
    let tmpdir = create_build_tmpdir().map_err(|e| format!("Failed to create tmpdir: {e}"))?;

    let result = async {
        // Stage 1: Extract precompiled bundle
        send_stage(progress, StageName::Extracting, "Extracting precompiled bundle");
        check_cancelled(cancelled)?;

        let extract_dir = tmpdir.join("precompiled");
        std::fs::create_dir_all(&extract_dir)
            .map_err(|e| format!("Failed to create extract dir: {e}"))?;
        extract_archive(&request.tarball_path, &extract_dir)?;
        send_progress(progress, StageName::Extracting, 100, None);

        // Find the .app bundle in the extracted content
        let app_path = find_app_bundle(&extract_dir)?;
        tracing::info!("Found precompiled .app: {}", app_path.display());
        // Log .app contents for debugging
        if let Ok(entries) = std::fs::read_dir(&app_path) {
            let files: Vec<String> = entries.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
            tracing::info!(".app contents: {:?}", files);
        }
        let plist_path = app_path.join("Info.plist");
        if plist_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&plist_path) {
                let has_icon_name = content.contains("CFBundleIconName");
                let has_dt_platform = content.contains("DTPlatformName");
                tracing::info!("Info.plist: CFBundleIconName={has_icon_name} DTPlatformName={has_dt_platform} size={}", content.len());
            }
        } else {
            tracing::warn!("No Info.plist found in .app!");
        }

        // Stage 2: Sign with rcodesign
        send_stage(progress, StageName::Signing, "Signing precompiled bundle");
        check_cancelled(cancelled)?;

        let p12_path = if let Some(ref b64) = request.credentials.apple_certificate_p12_base64 {
            let decoded = base64_decode(b64)?;
            let p = tmpdir.join("signing.p12");
            std::fs::write(&p, decoded)
                .map_err(|e| format!("Failed to write p12: {e}"))?;
            Some(p)
        } else {
            None
        };

        let identity = request.credentials.apple_signing_identity.as_deref();

        // Compile icon asset catalog on macOS (xcrun actool is macOS-only)
        if matches!(target, BuildTarget::IosSign) {
            // Find a source icon — check multiple names the Linux worker may have used
            let icon_src = ["Icon-1024.png", "AppIcon.png", "icon.png"]
                .iter()
                .map(|n| app_path.join(n))
                .find(|p| p.exists());

            if let Some(icon_path) = icon_src {
                let assets_dir = tmpdir.join("Assets.xcassets");
                let iconset = assets_dir.join("AppIcon.appiconset");
                std::fs::create_dir_all(&iconset).ok();

                // Generate all required icon sizes using sips (macOS native)
                let icon_sizes: &[(u32, &str, &str, &str)] = &[
                    (1024, "icon_1024.png", "ios-marketing", "1x"),
                    (180, "icon_60_3x.png", "iphone", "3x"),
                    (120, "icon_60_2x.png", "iphone", "2x"),
                    (167, "icon_83.5_2x.png", "ipad", "2x"),
                    (152, "icon_76_2x.png", "ipad", "2x"),
                    (76, "icon_76_1x.png", "ipad", "1x"),
                ];
                for (size, name, _, _) in icon_sizes {
                    let dest = iconset.join(name);
                    std::fs::copy(&icon_path, &dest).ok();
                    if *size != 1024 {
                        let _ = std::process::Command::new("sips")
                            .args(["-z", &size.to_string(), &size.to_string()])
                            .arg(&dest)
                            .output();
                    }
                }

                let contents_json = r#"{"images":[
                    {"filename":"icon_1024.png","idiom":"ios-marketing","scale":"1x","size":"1024x1024"},
                    {"filename":"icon_60_3x.png","idiom":"iphone","scale":"3x","size":"60x60"},
                    {"filename":"icon_60_2x.png","idiom":"iphone","scale":"2x","size":"60x60"},
                    {"filename":"icon_83.5_2x.png","idiom":"ipad","scale":"2x","size":"83.5x83.5"},
                    {"filename":"icon_76_2x.png","idiom":"ipad","scale":"2x","size":"76x76"},
                    {"filename":"icon_76_1x.png","idiom":"ipad","scale":"1x","size":"76x76"}
                ],"info":{"author":"xcode","version":1}}"#;
                std::fs::write(iconset.join("Contents.json"), contents_json).ok();
                std::fs::write(assets_dir.join("Contents.json"), r#"{"info":{"author":"perry","version":1}}"#).ok();

                // Compile asset catalog using xcrun actool
                let partial_plist = tmpdir.join("partial-info.plist");
                let actool_result = tokio::process::Command::new("xcrun")
                    .args(["actool", "--compile", app_path.to_str().unwrap_or(""),
                           "--platform", "iphoneos", "--minimum-deployment-target", "17.0",
                           "--app-icon", "AppIcon",
                           "--output-partial-info-plist", partial_plist.to_str().unwrap_or("")])
                    .arg(assets_dir.to_str().unwrap_or(""))
                    .output()
                    .await;
                match actool_result {
                    Ok(o) if o.status.success() => {
                        tracing::info!("Compiled iOS asset catalog (Assets.car)");
                        // Add CFBundleIconName to Info.plist (required for asset catalog icons)
                        let plist_path = app_path.join("Info.plist");
                        let _ = tokio::process::Command::new("/usr/libexec/PlistBuddy")
                            .args(["-c", "Add :CFBundleIconName string AppIcon",
                                   plist_path.to_str().unwrap_or("")])
                            .output()
                            .await;
                        tracing::info!("Added CFBundleIconName to Info.plist");
                    }
                    Ok(o) => {
                        tracing::warn!("actool failed (non-fatal): {}", String::from_utf8_lossy(&o.stderr));
                    }
                    Err(e) => {
                        tracing::warn!("actool not available (non-fatal): {e}");
                    }
                }
            }
        }

        // Embed provisioning profile for iOS (required for App Store / TestFlight)
        if matches!(target, BuildTarget::IosSign) {
            if let Some(ref b64) = request.credentials.provisioning_profile_base64 {
                let decoded = base64_decode(b64)?;
                let profile_dest = app_path.join("embedded.mobileprovision");
                std::fs::write(&profile_dest, decoded)
                    .map_err(|e| format!("Failed to embed provisioning profile: {e}"))?;
                tracing::info!("Embedded provisioning profile into .app");
            }
        }
        // Embed provisioning profile for macOS App Store
        if matches!(target, BuildTarget::MacOsSign) {
            if let Some(ref b64) = request.credentials.provisioning_profile_base64 {
                let decoded = base64_decode(b64)?;
                let profile_dest = app_path.join("Contents/embedded.provisionprofile");
                if let Some(parent) = profile_dest.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                std::fs::write(&profile_dest, decoded)
                    .map_err(|e| format!("Failed to embed provisioning profile: {e}"))?;
                tracing::info!("Embedded provisioning profile into .app");
            }
        }

        if let Some(ref p12) = p12_path {
            // Generate platform-appropriate entitlements
            let entitlements_path = if matches!(target, BuildTarget::IosSign) {
                // iOS needs minimal entitlements — just get-task-allow (for dev) and app identifier
                // The provisioning profile handles most entitlements on iOS
                let bundle_id = if request.manifest.bundle_id.is_empty() {
                    "com.example.app"
                } else {
                    &request.manifest.bundle_id
                };
                let p = tmpdir.join("entitlements.plist");
                let plist = format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>application-identifier</key>
    <string>{team_id}.{bundle_id}</string>
    <key>com.apple.developer.team-identifier</key>
    <string>{team_id}</string>
</dict>
</plist>"#,
                    team_id = request.credentials.apple_team_id.as_deref().unwrap_or("TEAMID"),
                    bundle_id = bundle_id,
                );
                std::fs::write(&p, plist)
                    .map_err(|e| format!("Failed to write iOS entitlements: {e}"))?;
                Some(p)
            } else if matches!(target, BuildTarget::MacOsSign) && request.manifest.entitlements.is_some() {
                let p = tmpdir.join("entitlements.plist");
                macos::write_entitlements_plist(&request.manifest, &p)?;
                Some(p)
            } else {
                None
            };

            apple::codesign_app(
                identity.unwrap_or(""),
                entitlements_path.as_deref(),
                &app_path,
                matches!(target, BuildTarget::MacOsSign), // hardened runtime for macOS
                None,
                Some(p12.as_path()),
                request.credentials.apple_certificate_password.as_deref(),
            ).await?;
        }
        send_progress(progress, StageName::Signing, 100, None);

        match target {
            BuildTarget::IosSign => {
                // Log .app contents before .ipa creation
                if let Ok(entries) = std::fs::read_dir(&app_path) {
                    let files: Vec<String> = entries.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
                    tracing::info!("Pre-ipa .app contents: {:?}", files);
                }

                // Stage 3: Create signed .ipa
                send_stage(progress, StageName::Packaging, "Creating signed .ipa");
                check_cancelled(cancelled)?;

                let ipa_path = tmpdir.join(format!("{}.ipa", request.manifest.app_name));
                ios::create_ipa(&request.manifest.app_name, &app_path, &ipa_path).await?;
                send_progress(progress, StageName::Packaging, 100, None);

                // Stage 4: Upload to App Store Connect
                let has_creds = request.credentials.apple_key_id.is_some()
                    && request.credentials.apple_issuer_id.is_some()
                    && request.credentials.apple_p8_key.is_some();

                if has_creds {
                    send_stage(progress, StageName::Publishing, "Uploading to App Store Connect");
                    check_cancelled(cancelled)?;

                    let result = appstore::upload_to_appstore(
                        &ipa_path,
                        request.credentials.apple_p8_key.as_deref().unwrap(),
                        request.credentials.apple_key_id.as_deref().unwrap(),
                        request.credentials.apple_issuer_id.as_deref().unwrap(),
                        &tmpdir,
                    ).await;
                    match result {
                        Ok(r) => tracing::info!("App Store upload: {}", r.message),
                        Err(e) => return Err(format!("App Store upload failed:\n{e}")),
                    }
                    send_progress(progress, StageName::Publishing, 100, None);
                }

                let artifact = copy_artifact(&ipa_path, &request.manifest.app_name, &request.job_id, "ipa")?;
                Ok(artifact)
            }
            BuildTarget::MacOsSign => {
                // Stage 3: Create DMG
                send_stage(progress, StageName::Packaging, "Creating DMG");
                check_cancelled(cancelled)?;

                let dmg_path = tmpdir.join(format!("{}.dmg", request.manifest.app_name));
                macos::create_dmg(&request.manifest.app_name, &app_path, &dmg_path).await?;
                send_progress(progress, StageName::Packaging, 100, None);

                // Stage 4: Notarize if credentials available
                let has_notarize_creds = request.credentials.apple_key_id.is_some()
                    && request.credentials.apple_issuer_id.is_some()
                    && request.credentials.apple_p8_key.is_some();

                if has_notarize_creds {
                    send_stage(progress, StageName::Notarizing, "Notarizing DMG");
                    check_cancelled(cancelled)?;

                    let notarize_result = apple::notarize_dmg(
                        &dmg_path,
                        request.credentials.apple_p8_key.as_deref().unwrap(),
                        request.credentials.apple_key_id.as_deref().unwrap(),
                        request.credentials.apple_issuer_id.as_deref().unwrap(),
                        &tmpdir,
                    ).await;
                    match notarize_result {
                        Ok(()) => tracing::info!("Notarization succeeded"),
                        Err(e) => tracing::warn!("Notarization failed (non-fatal): {e}"),
                    }
                    send_progress(progress, StageName::Notarizing, 100, None);
                }

                let artifact = copy_artifact(&dmg_path, &request.manifest.app_name, &request.job_id, "dmg")?;
                Ok(artifact)
            }
            _ => Err("Unexpected target in sign-only pipeline".into()),
        }
    }.await;

    cleanup_tmpdir(&tmpdir);
    result
}

/// Extract an archive — auto-detects zip (.ipa) vs tar.gz format
fn extract_archive(archive_path: &std::path::Path, dest: &std::path::Path) -> Result<(), String> {
    let data = std::fs::read(archive_path)
        .map_err(|e| format!("Failed to read archive: {e}"))?;

    // Detect format by magic bytes: zip starts with PK (0x50 0x4B), gzip with 0x1F 0x8B
    if data.len() >= 2 && data[0] == 0x50 && data[1] == 0x4B {
        // ZIP format (.ipa)
        let cursor = std::io::Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor)
            .map_err(|e| format!("Failed to open zip: {e}"))?;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i)
                .map_err(|e| format!("Failed to read zip entry: {e}"))?;
            let outpath = dest.join(file.mangled_name());
            if file.is_dir() {
                std::fs::create_dir_all(&outpath).ok();
            } else {
                if let Some(parent) = outpath.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                let mut outfile = std::fs::File::create(&outpath)
                    .map_err(|e| format!("Failed to create file: {e}"))?;
                std::io::copy(&mut file, &mut outfile)
                    .map_err(|e| format!("Failed to extract file: {e}"))?;
            }
        }
        Ok(())
    } else {
        // Assume tar.gz
        extract_tarball(archive_path, dest)
    }
}

/// Find a .app bundle in the extracted directory (searches recursively)
fn find_app_bundle(dir: &std::path::Path) -> Result<PathBuf, String> {
    for entry in walkdir::WalkDir::new(dir).max_depth(3) {
        if let Ok(e) = entry {
            if e.path().extension().map_or(false, |ext| ext == "app") && e.path().is_dir() {
                return Ok(e.path().to_path_buf());
            }
        }
    }
    Err(format!("No .app bundle found in precompiled archive at {}", dir.display()))
}

#[derive(PartialEq)]
enum BuildTarget {
    MacOs,
    Ios,
    Android,
    MacOsSign,
    IosSign,
}

fn determine_target(targets: &[String]) -> BuildTarget {
    for t in targets {
        match t.to_lowercase().as_str() {
            "ios" => return BuildTarget::Ios,
            "ios-sign" => return BuildTarget::IosSign,
            "macos-sign" => return BuildTarget::MacOsSign,
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

    // Manually iterate entries to prevent path traversal attacks.
    // archive.unpack() does NOT validate paths — a malicious tarball could
    // write files outside the destination via ".." components or absolute paths.
    for entry in archive
        .entries()
        .map_err(|e| format!("Failed to read tarball entries: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("Failed to read tarball entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("Failed to read entry path: {e}"))?
            .into_owned();

        // Reject absolute paths and any ".." path components
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!(
                "Tarball contains unsafe path (path traversal rejected): {}",
                path.display()
            ));
        }

        entry
            .unpack_in(dest)
            .map_err(|e| format!("Failed to extract {}: {e}", path.display()))?;
    }

    Ok(())
}

/// Query Xcode version info, with env var overrides.
/// Set PERRY_DT_XCODE, PERRY_DT_XCODE_BUILD to override the Xcode version reported in Info.plist.
async fn query_xcode_info() -> (String, String) {
    // Allow env var overrides for when the installed Xcode is behind Apple's requirement
    if let (Ok(xc), Ok(xcb)) = (
        std::env::var("PERRY_DT_XCODE"),
        std::env::var("PERRY_DT_XCODE_BUILD"),
    ) {
        return (xc, xcb);
    }

    let xcode_out = tokio::process::Command::new("xcodebuild")
        .arg("-version")
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();

    let mut dt_xcode = "2630".to_string();
    let mut dt_xcode_build = "17C529".to_string();
    for line in xcode_out.lines() {
        if let Some(ver) = line.strip_prefix("Xcode ") {
            let parts: Vec<&str> = ver.trim().split('.').collect();
            let major: u32 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(26);
            let minor: u32 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            // DTXcode is a 4-digit code: MMmP (major 2 digits, minor 1, patch 1)
            // e.g. Xcode 16.2 → 1620, Xcode 26.3 → 2630
            dt_xcode = format!("{:02}{}{}", major, minor, 0);
        } else if let Some(build) = line.strip_prefix("Build version ") {
            dt_xcode_build = build.trim().to_string();
        }
    }

    (dt_xcode, dt_xcode_build)
}

/// Query SDK version for a given sdk name (e.g. "iphoneos", "macosx").
async fn query_sdk_version(sdk: &str, default_ver: &str, default_build: &str) -> (String, String) {
    let sdk_version = tokio::process::Command::new("xcrun")
        .args(["--sdk", sdk, "--show-sdk-version"])
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_ver.to_string());

    let sdk_build = tokio::process::Command::new("xcrun")
        .args(["--sdk", sdk, "--show-sdk-build-version"])
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_build.to_string());

    (sdk_version, sdk_build)
}

/// Query the local Xcode installation for iOS SDK/version info to embed in Info.plist.
/// When PERRY_DT_XCODE is set (override mode), uses the SDK values that ship with
/// Xcode 26.3 GM instead of querying the local (potentially outdated) Xcode.
/// Note: Xcode 26.3 ships with iOS SDK 26.2, not 26.3.
async fn query_sdk_info() -> ios::SdkInfo {
    let (xcode, xcode_build) = query_xcode_info().await;

    // If we're overriding Xcode version, also override SDK to stay consistent.
    // Xcode 26.3 (17C529) ships with iOS SDK 26.2 (build 23C57).
    let (sdk_version, sdk_build) = if std::env::var("PERRY_DT_XCODE").is_ok() {
        ("26.2".to_string(), "23C57".to_string())
    } else {
        query_sdk_version("iphoneos", "26.2", "23C57").await
    };

    ios::SdkInfo {
        platform_version: sdk_version.clone(),
        sdk_name: format!("iphoneos{sdk_version}"),
        sdk_build,
        xcode,
        xcode_build,
    }
}

/// Query the local Xcode installation for macOS SDK info.
/// When PERRY_DT_XCODE is set (override mode), uses the SDK values that ship with
/// Xcode 26.3 GM. Note: Xcode 26.3 ships with macOS SDK 26.2, not 26.3.
async fn query_macos_sdk_info() -> macos::MacSdkInfo {
    let (xcode, xcode_build) = query_xcode_info().await;

    // Xcode 26.3 (17C529) ships with macOS SDK 26.2 (build 25C58).
    let (sdk_version, sdk_build) = if std::env::var("PERRY_DT_XCODE").is_ok() {
        ("26.2".to_string(), "25C58".to_string())
    } else {
        query_sdk_version("macosx", "26.2", "25C58").await
    };

    macos::MacSdkInfo {
        platform_version: sdk_version.clone(),
        sdk_name: format!("macosx{sdk_version}"),
        sdk_build,
        xcode,
        xcode_build,
    }
}

/// Parse the Play Store track from a `distribute` field value.
///
/// - `"playstore"` → `Some("internal")` (default track)
/// - `"playstore:beta"` → `Some("beta")`
/// - anything else → `None`
fn parse_playstore_track(distribute: Option<&str>) -> Option<&'static str> {
    let d = distribute?;
    if d == "playstore" {
        return Some("internal");
    }
    if let Some(track) = d.strip_prefix("playstore:") {
        return match track {
            "internal" => Some("internal"),
            "alpha" => Some("alpha"),
            "beta" => Some("beta"),
            "production" => Some("production"),
            _ => None, // invalid track — caught by pre-flight validation in perry CLI
        };
    }
    None
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| format!("Invalid base64: {e}"))
}

/// Write base64-encoded p12 data to a temp file and return the path.
fn write_p12_temp(tmpdir: &std::path::Path, p12_b64: &str, suffix: &str) -> Result<std::path::PathBuf, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(p12_b64.trim())
        .map_err(|e| format!("Failed to decode p12: {e}"))?;
    let path = tmpdir.join(format!("signing-{suffix}.p12"));
    std::fs::write(&path, &bytes)
        .map_err(|e| format!("Failed to write p12 temp file: {e}"))?;
    Ok(path)
}
