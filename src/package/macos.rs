use crate::queue::job::BuildManifest;
use std::path::Path;
use tokio::process::Command;

pub fn create_app_bundle(
    manifest: &BuildManifest,
    binary_path: &Path,
    icns_path: Option<&Path>,
    app_path: &Path,
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
    let info_plist = generate_info_plist(manifest, icon_file_name.as_deref());
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

fn generate_info_plist(manifest: &BuildManifest, icon_file: Option<&str>) -> String {
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
        .map(|f| format!("\t<key>CFBundleIconFile</key>\n\t<string>{f}</string>\n"))
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
{icon_entry}	<key>LSMinimumSystemVersion</key>
	<string>{min_os}</string>
	<key>LSApplicationCategoryType</key>
	<string>{category}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
</dict>
</plist>"#,
        executable = manifest.app_name,
        bundle_id = manifest.bundle_id,
        name = manifest.app_name,
        version = manifest.version,
    )
}

fn generate_entitlements_plist(entitlements: &[String]) -> String {
    let entries: String = entitlements
        .iter()
        .map(|e| format!("\t<key>{e}</key>\n\t<true/>"))
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
            android_min_sdk: None,
            android_target_sdk: None,
            android_permissions: None,
            android_distribute: None,
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
            android_min_sdk: None,
            android_target_sdk: None,
            android_permissions: None,
            android_distribute: None,
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
