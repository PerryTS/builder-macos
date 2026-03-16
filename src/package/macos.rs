use crate::build::validate::escape_xml;
use crate::queue::job::BuildManifest;
use std::path::Path;
use tokio::process::Command;

/// Xcode/SDK version info for macOS Info.plist DT* keys.
pub struct MacSdkInfo {
    pub platform_version: String,
    pub sdk_name: String,
    pub sdk_build: String,
    pub xcode: String,
    pub xcode_build: String,
}

pub fn create_app_bundle(
    manifest: &BuildManifest,
    binary_path: &Path,
    icns_path: Option<&Path>,
    app_path: &Path,
    sdk_info: Option<&MacSdkInfo>,
) -> Result<(), String> {
    let contents = app_path.join("Contents");
    let macos_dir = contents.join("MacOS");
    let resources_dir = contents.join("Resources");

    std::fs::create_dir_all(&macos_dir)
        .map_err(|e| format!("Failed to create MacOS dir: {e}"))?;
    std::fs::create_dir_all(&resources_dir)
        .map_err(|e| format!("Failed to create Resources dir: {e}"))?;

    // Copy binary
    let dest_binary = macos_dir.join(&manifest.app_name);
    std::fs::copy(binary_path, &dest_binary)
        .map_err(|e| format!("Failed to copy binary: {e}"))?;

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest_binary, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("Failed to set permissions: {e}"))?;
    }

    // Copy icon
    let icon_file_name = if let Some(icns) = icns_path {
        let name = "AppIcon.icns";
        std::fs::copy(icns, resources_dir.join(name))
            .map_err(|e| format!("Failed to copy icon: {e}"))?;
        Some(name.to_string())
    } else {
        None
    };

    // Generate Info.plist
    let info_plist = generate_info_plist(manifest, icon_file_name.as_deref(), sdk_info);
    std::fs::write(contents.join("Info.plist"), info_plist)
        .map_err(|e| format!("Failed to write Info.plist: {e}"))?;

    Ok(())
}

pub fn write_entitlements_plist(manifest: &BuildManifest, path: &Path) -> Result<(), String> {
    let entitlements = manifest.entitlements.as_deref().unwrap_or(&[]);
    let plist = generate_entitlements_plist(entitlements);
    std::fs::write(path, plist).map_err(|e| format!("Failed to write entitlements: {e}"))?;
    Ok(())
}

/// Create a signed .pkg installer for Mac App Store submission.
///
/// Signs the .pkg with the installer identity derived from the app signing identity
/// (by replacing "Application" → "Installer" in the identity name).
pub async fn create_pkg(
    app_path: &Path,
    pkg_path: &Path,
    installer_identity: &str,
) -> Result<(), String> {
    let output = Command::new("productbuild")
        .arg("--component")
        .arg(app_path)
        .arg("/Applications")
        .arg("--sign")
        .arg(installer_identity)
        .arg(pkg_path)
        .output()
        .await
        .map_err(|e| format!("Failed to run productbuild: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!("productbuild failed: {stderr}\n{stdout}"));
    }

    Ok(())
}

pub async fn create_dmg(app_name: &str, app_path: &Path, dmg_path: &Path) -> Result<(), String> {
    let staging_dir = dmg_path
        .parent()
        .unwrap()
        .join(format!("{app_name}-dmg-staging"));
    std::fs::create_dir_all(&staging_dir)
        .map_err(|e| format!("Failed to create DMG staging dir: {e}"))?;

    // Copy .app into staging
    copy_dir_recursive(app_path, &staging_dir.join(format!("{app_name}.app")))?;

    // Create /Applications symlink
    #[cfg(unix)]
    std::os::unix::fs::symlink("/Applications", staging_dir.join("Applications"))
        .map_err(|e| format!("Failed to create Applications symlink: {e}"))?;

    // Create DMG using hdiutil
    let output = Command::new("hdiutil")
        .arg("create")
        .arg("-volname")
        .arg(app_name)
        .arg("-srcfolder")
        .arg(&staging_dir)
        .arg("-ov")
        .arg("-format")
        .arg("UDZO")
        .arg(dmg_path)
        .output()
        .await
        .map_err(|e| format!("Failed to run hdiutil: {e}"))?;

    // Cleanup staging
    std::fs::remove_dir_all(&staging_dir).ok();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("hdiutil failed: {stderr}"));
    }

    Ok(())
}

fn generate_info_plist(manifest: &BuildManifest, icon_file: Option<&str>, sdk_info: Option<&MacSdkInfo>) -> String {
    let short_version = manifest
        .short_version
        .as_deref()
        .unwrap_or(&manifest.version);
    let min_os = manifest
        .minimum_os_version
        .as_deref()
        .unwrap_or("13.0");
    let category = manifest
        .category
        .as_deref()
        .unwrap_or("public.app-category.developer-tools");

    let icon_entry = icon_file
        .map(|f| format!("\t<key>CFBundleIconFile</key>\n\t<string>{}</string>\n", escape_xml(f)))
        .unwrap_or_default();

    let encryption_entry = manifest.macos_encryption_exempt
        .map(|exempt| {
            let value = if exempt { "<false/>" } else { "<true/>" };
            format!("\t<key>ITSAppUsesNonExemptEncryption</key>\n\t{value}\n")
        })
        .unwrap_or_default();

    let sdk_entries = sdk_info
        .map(|info| format!(
            "\t<key>CFBundleSupportedPlatforms</key>\n\t<array>\n\t\t<string>MacOSX</string>\n\t</array>\n\
             \t<key>DTPlatformName</key>\n\t<string>macosx</string>\n\
             \t<key>DTPlatformVersion</key>\n\t<string>{platform_version}</string>\n\
             \t<key>DTSDKName</key>\n\t<string>{sdk_name}</string>\n\
             \t<key>DTSDKBuild</key>\n\t<string>{sdk_build}</string>\n\
             \t<key>DTXcode</key>\n\t<string>{xcode}</string>\n\
             \t<key>DTXcodeBuild</key>\n\t<string>{xcode_build}</string>\n\
             \t<key>DTCompiler</key>\n\t<string>com.apple.compilers.llvm.clang.1_0</string>\n",
            platform_version = escape_xml(&info.platform_version),
            sdk_name = escape_xml(&info.sdk_name),
            sdk_build = escape_xml(&info.sdk_build),
            xcode = escape_xml(&info.xcode),
            xcode_build = escape_xml(&info.xcode_build),
        ))
        .unwrap_or_default();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>{executable}</string>
	<key>CFBundleIdentifier</key>
	<string>{bundle_id}</string>
	<key>CFBundleName</key>
	<string>{name}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>CFBundleShortVersionString</key>
	<string>{short_version}</string>
{icon_entry}{encryption_entry}{sdk_entries}	<key>LSMinimumSystemVersion</key>
	<string>{min_os}</string>
	<key>LSApplicationCategoryType</key>
	<string>{category}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
</dict>
</plist>"#,
        executable = escape_xml(&manifest.app_name),
        bundle_id = escape_xml(&manifest.bundle_id),
        name = escape_xml(&manifest.app_name),
        version = escape_xml(&manifest.version),
        short_version = escape_xml(short_version),
        min_os = escape_xml(min_os),
        category = escape_xml(category),
    )
}

fn generate_entitlements_plist(entitlements: &[String]) -> String {
    let entries: String = entitlements
        .iter()
        .map(|e| format!("\t<key>{}</key>\n\t<true/>", escape_xml(e)))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
{entries}
</dict>
</plist>"#
    )
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("Failed to create dir: {e}"))?;
    for entry in
        std::fs::read_dir(src).map_err(|e| format!("Failed to read dir {}: {e}", src.display()))?
    {
        let entry = entry.map_err(|e| format!("Dir entry error: {e}"))?;
        let ty = entry
            .file_type()
            .map_err(|e| format!("File type error: {e}"))?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_path)?;
        } else if ty.is_symlink() {
            #[cfg(unix)]
            {
                let target = std::fs::read_link(entry.path())
                    .map_err(|e| format!("Symlink read error: {e}"))?;
                std::os::unix::fs::symlink(target, &dest_path)
                    .map_err(|e| format!("Symlink create error: {e}"))?;
            }
        } else {
            std::fs::copy(entry.path(), &dest_path)
                .map_err(|e| format!("Copy error: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_info_plist_generation() {
        let manifest = BuildManifest {
            app_name: "MyApp".into(),
            bundle_id: "com.example.myapp".into(),
            version: "1.0.0".into(),
            short_version: Some("1.0".into()),
            entry: "src/main.ts".into(),
            icon: None,
            targets: vec!["macos".into()],
            category: None,
            minimum_os_version: None,
            entitlements: None,
            ios_deployment_target: None,
            ios_device_family: None,
            ios_orientations: None,
            ios_capabilities: None,
            ios_distribute: None,
            ios_encryption_exempt: None,
            android_min_sdk: None,
            android_target_sdk: None,
            android_permissions: None,
            android_distribute: None,
            macos_distribute: None,
            macos_encryption_exempt: None,
        };

        let plist = generate_info_plist(&manifest, Some("AppIcon.icns"));
        assert!(plist.contains("<string>MyApp</string>"));
        assert!(plist.contains("<string>com.example.myapp</string>"));
        assert!(plist.contains("<string>1.0.0</string>"));
        assert!(plist.contains("<string>1.0</string>"));
        assert!(plist.contains("<string>AppIcon.icns</string>"));
        assert!(plist.contains("<string>13.0</string>"));
        assert!(plist.contains("APPL"));
    }

    #[test]
    fn test_info_plist_no_icon() {
        let manifest = BuildManifest {
            app_name: "Test".into(),
            bundle_id: "com.test".into(),
            version: "0.1.0".into(),
            short_version: None,
            entry: "main.ts".into(),
            icon: None,
            targets: vec![],
            category: Some("public.app-category.games".into()),
            minimum_os_version: Some("14.0".into()),
            entitlements: None,
            ios_deployment_target: None,
            ios_device_family: None,
            ios_orientations: None,
            ios_capabilities: None,
            ios_distribute: None,
            ios_encryption_exempt: None,
            android_min_sdk: None,
            android_target_sdk: None,
            android_permissions: None,
            android_distribute: None,
            macos_distribute: None,
            macos_encryption_exempt: None,
        };

        let plist = generate_info_plist(&manifest, None);
        assert!(!plist.contains("CFBundleIconFile"));
        assert!(plist.contains("public.app-category.games"));
        assert!(plist.contains("<string>14.0</string>"));
    }

    #[test]
    fn test_entitlements_plist() {
        let entitlements = vec![
            "com.apple.security.app-sandbox".into(),
            "com.apple.security.network.client".into(),
        ];
        let plist = generate_entitlements_plist(&entitlements);
        assert!(plist.contains("com.apple.security.app-sandbox"));
        assert!(plist.contains("com.apple.security.network.client"));
        assert!(plist.contains("<true/>"));
    }
}
