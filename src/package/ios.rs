use crate::queue::job::BuildManifest;
use std::path::Path;
use tokio::process::Command;

/// Xcode/SDK version info injected into Info.plist.
pub struct SdkInfo {
    pub platform_version: String,
    pub sdk_name: String,
    pub sdk_build: String,
    pub xcode: String,
    pub xcode_build: String,
}

/// Device family constants for UIDeviceFamily
const DEVICE_IPHONE: u8 = 1;
const DEVICE_IPAD: u8 = 2;

pub fn create_ios_app_bundle(
    manifest: &BuildManifest,
    binary_path: &Path,
    icon_png_path: Option<&Path>,
    provisioning_profile: Option<&Path>,
    app_path: &Path,
    sdk_info: Option<&SdkInfo>,
) -> Result<(), String> {
    std::fs::create_dir_all(app_path)
        .map_err(|e| format!("Failed to create iOS .app dir: {e}"))?;

    // Copy binary
    let dest_binary = app_path.join(&manifest.app_name);
    std::fs::copy(binary_path, &dest_binary)
        .map_err(|e| format!("Failed to copy iOS binary: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest_binary, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("Failed to set permissions: {e}"))?;
    }

    // Copy icon PNG as AppIcon.png (will be in Assets.car if we compile asset catalog)
    if let Some(icon) = icon_png_path {
        if icon.exists() {
            std::fs::copy(icon, app_path.join("AppIcon.png"))
                .map_err(|e| format!("Failed to copy icon: {e}"))?;
        }
    }

    // Embed provisioning profile
    if let Some(profile) = provisioning_profile {
        if profile.exists() {
            std::fs::copy(profile, app_path.join("embedded.mobileprovision"))
                .map_err(|e| format!("Failed to embed provisioning profile: {e}"))?;
        }
    }

    // Generate Info.plist
    let info_plist = generate_ios_info_plist(manifest, sdk_info);
    let plist_path = app_path.join("Info.plist");
    std::fs::write(&plist_path, &info_plist)
        .map_err(|e| format!("Failed to write iOS Info.plist: {e}"))?;

    // Convert to binary plist format (required by altool validation)
    let _ = std::process::Command::new("plutil")
        .arg("-convert")
        .arg("binary1")
        .arg(&plist_path)
        .status();

    Ok(())
}

pub fn write_ios_entitlements_plist(
    manifest: &BuildManifest,
    team_id: &str,
    path: &Path,
) -> Result<(), String> {
    let capabilities = manifest.ios_capabilities.as_deref().unwrap_or(&[]);
    let plist = generate_ios_entitlements(
        &manifest.bundle_id,
        team_id,
        capabilities,
    );
    std::fs::write(path, plist).map_err(|e| format!("Failed to write entitlements: {e}"))?;
    Ok(())
}

/// Create .ipa by zipping Payload/MyApp.app into a zip archive
pub async fn create_ipa(
    app_name: &str,
    app_path: &Path,
    ipa_path: &Path,
) -> Result<(), String> {
    let staging_dir = ipa_path
        .parent()
        .unwrap()
        .join(format!("{app_name}-ipa-staging"));
    let payload_dir = staging_dir.join("Payload");
    std::fs::create_dir_all(&payload_dir)
        .map_err(|e| format!("Failed to create Payload dir: {e}"))?;

    // Copy .app into Payload/
    let dest_app = payload_dir.join(format!("{app_name}.app"));
    copy_dir_recursive(app_path, &dest_app)?;

    // Zip Payload/ into .ipa
    let output = Command::new("zip")
        .arg("-r")
        .arg("-q")
        .arg(ipa_path)
        .arg("Payload")
        .current_dir(&staging_dir)
        .output()
        .await
        .map_err(|e| format!("Failed to run zip: {e}"))?;

    // Cleanup staging
    std::fs::remove_dir_all(&staging_dir).ok();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("zip failed: {stderr}"));
    }

    Ok(())
}

fn generate_ios_info_plist(manifest: &BuildManifest, sdk_info: Option<&SdkInfo>) -> String {
    let short_version = manifest
        .short_version
        .as_deref()
        .unwrap_or(&manifest.version);
    let deployment_target = manifest
        .ios_deployment_target
        .as_deref()
        .unwrap_or("16.0");

    // UIDeviceFamily
    let device_families = resolve_device_families(
        manifest.ios_device_family.as_deref().unwrap_or(&[]),
    );
    let device_family_xml = device_families
        .iter()
        .map(|d| format!("\t\t<integer>{d}</integer>"))
        .collect::<Vec<_>>()
        .join("\n");
    let supports_ipad = device_families.contains(&DEVICE_IPAD);

    // Orientations
    let orientations = resolve_orientations(
        manifest.ios_orientations.as_deref().unwrap_or(&[]),
    );
    let orientation_xml = orientations
        .iter()
        .map(|o| format!("\t\t<string>{o}</string>"))
        .collect::<Vec<_>>()
        .join("\n");

    // iPad multitasking requires all 4 orientations in the ~ipad key
    let ipad_orientation_xml = if supports_ipad {
        let all_four = [
            "UIInterfaceOrientationPortrait",
            "UIInterfaceOrientationPortraitUpsideDown",
            "UIInterfaceOrientationLandscapeLeft",
            "UIInterfaceOrientationLandscapeRight",
        ];
        let xml = all_four
            .iter()
            .map(|o| format!("\t\t<string>{o}</string>"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\t<key>UISupportedInterfaceOrientations~ipad</key>\n\t<array>\n{xml}\n\t</array>"
        )
    } else {
        String::new()
    };

    // Xcode/SDK version keys — required by App Store Connect validation
    let dt_keys = if let Some(info) = sdk_info {
        format!(
            r#"	<key>DTPlatformName</key>
	<string>iphoneos</string>
	<key>DTPlatformVersion</key>
	<string>{platform_version}</string>
	<key>DTSDKName</key>
	<string>{sdk_name}</string>
	<key>DTSDKBuild</key>
	<string>{sdk_build}</string>
	<key>DTXcode</key>
	<string>{xcode}</string>
	<key>DTXcodeBuild</key>
	<string>{xcode_build}</string>
	<key>DTCompiler</key>
	<string>com.apple.compilers.llvm.clang.1_0</string>"#,
            platform_version = info.platform_version,
            sdk_name = info.sdk_name,
            sdk_build = info.sdk_build,
            xcode = info.xcode,
            xcode_build = info.xcode_build,
        )
    } else {
        "\t<key>DTPlatformName</key>\n\t<string>iphoneos</string>".to_string()
    };

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
	<key>MinimumOSVersion</key>
	<string>{deployment_target}</string>
	<key>UIDeviceFamily</key>
	<array>
{device_family_xml}
	</array>
	<key>UISupportedInterfaceOrientations</key>
	<array>
{orientation_xml}
	</array>
	<key>UIRequiredDeviceCapabilities</key>
	<array>
		<string>arm64</string>
	</array>
	<key>CFBundleIconName</key>
	<string>AppIcon</string>
	<key>CFBundleIcons</key>
	<dict>
		<key>CFBundlePrimaryIcon</key>
		<dict>
			<key>CFBundleIconFiles</key>
			<array>
				<string>Icon-20</string>
				<string>Icon-29</string>
				<string>Icon-40</string>
				<string>Icon-60</string>
			</array>
			<key>CFBundleIconName</key>
			<string>AppIcon</string>
		</dict>
	</dict>
	<key>CFBundleIcons~ipad</key>
	<dict>
		<key>CFBundlePrimaryIcon</key>
		<dict>
			<key>CFBundleIconFiles</key>
			<array>
				<string>Icon-20</string>
				<string>Icon-29</string>
				<string>Icon-40</string>
				<string>Icon-76</string>
				<string>Icon-83.5</string>
			</array>
			<key>CFBundleIconName</key>
			<string>AppIcon</string>
		</dict>
	</dict>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleSupportedPlatforms</key>
	<array>
		<string>iPhoneOS</string>
	</array>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
{dt_keys}
{ipad_orientation_xml}
	<key>UIApplicationSceneManifest</key>
	<dict>
		<key>UIApplicationSupportsMultipleScenes</key>
		<false/>
		<key>UISceneConfigurations</key>
		<dict>
			<key>UIWindowSceneSessionRoleApplication</key>
			<array>
				<dict>
					<key>UISceneConfigurationName</key>
					<string>Default Configuration</string>
					<key>UISceneDelegateClassName</key>
					<string>PerrySceneDelegate</string>
				</dict>
			</array>
		</dict>
	</dict>
	<key>UILaunchScreen</key>
	<dict/>
</dict>
</plist>"#,
        executable = manifest.app_name,
        bundle_id = manifest.bundle_id,
        name = manifest.app_name,
        version = manifest.version,
        short_version = short_version,
        deployment_target = deployment_target,
        device_family_xml = device_family_xml,
        orientation_xml = orientation_xml,
        dt_keys = dt_keys,
        ipad_orientation_xml = ipad_orientation_xml,
    )
}

fn resolve_device_families(families: &[String]) -> Vec<u8> {
    if families.is_empty() {
        return vec![DEVICE_IPHONE, DEVICE_IPAD];
    }
    families
        .iter()
        .filter_map(|f| match f.to_lowercase().as_str() {
            "iphone" => Some(DEVICE_IPHONE),
            "ipad" => Some(DEVICE_IPAD),
            _ => None,
        })
        .collect()
}

fn resolve_orientations(orientations: &[String]) -> Vec<&'static str> {
    if orientations.is_empty() {
        return vec![
            "UIInterfaceOrientationPortrait",
            "UIInterfaceOrientationLandscapeLeft",
            "UIInterfaceOrientationLandscapeRight",
        ];
    }
    orientations
        .iter()
        .filter_map(|o| match o.to_lowercase().as_str() {
            "portrait" => Some("UIInterfaceOrientationPortrait"),
            "upside-down" | "portrait-upside-down" => {
                Some("UIInterfaceOrientationPortraitUpsideDown")
            }
            "landscape-left" => Some("UIInterfaceOrientationLandscapeLeft"),
            "landscape-right" => Some("UIInterfaceOrientationLandscapeRight"),
            _ => None,
        })
        .collect()
}

fn generate_ios_entitlements(bundle_id: &str, team_id: &str, capabilities: &[String]) -> String {
    let app_id = if team_id.is_empty() {
        bundle_id.to_string()
    } else {
        format!("{team_id}.{bundle_id}")
    };
    let mut entries = vec![
        // App ID is always included — must be team_id.bundle_id (NOT $(AppIdentifierPrefix)...)
        format!("\t<key>application-identifier</key>\n\t<string>{app_id}</string>"),
    ];

    for cap in capabilities {
        match cap.as_str() {
            "push-notifications" => {
                entries.push("\t<key>aps-environment</key>\n\t<string>production</string>".into());
            }
            "camera" | "photo-library" => {
                // These are Info.plist usage descriptions, not entitlements
                // Handled separately during Info.plist generation
            }
            other => {
                // Pass through as a boolean entitlement
                entries.push(format!("\t<key>{other}</key>\n\t<true/>"));
            }
        }
    }

    let entries_xml = entries.join("\n");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
{entries_xml}
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
    fn test_ios_info_plist_generation() {
        let manifest = BuildManifest {
            app_name: "MyApp".into(),
            bundle_id: "com.example.myapp".into(),
            version: "1.0.0".into(),
            short_version: Some("1.0".into()),
            entry: "src/main_ios.ts".into(),
            icon: None,
            targets: vec!["ios".into()],
            category: None,
            minimum_os_version: None,
            entitlements: None,
            ios_deployment_target: Some("16.0".into()),
            ios_device_family: Some(vec!["iphone".into(), "ipad".into()]),
            ios_orientations: Some(vec!["portrait".into()]),
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

        let plist = generate_ios_info_plist(&manifest, None);
        assert!(plist.contains("<string>MyApp</string>"));
        assert!(plist.contains("<string>com.example.myapp</string>"));
        assert!(plist.contains("<string>16.0</string>"));
        assert!(plist.contains("<integer>1</integer>")); // iPhone
        assert!(plist.contains("<integer>2</integer>")); // iPad
        assert!(plist.contains("UIInterfaceOrientationPortrait"));
        assert!(plist.contains("UIApplicationSceneManifest"));
        assert!(plist.contains("PerrySceneDelegate"));
        assert!(plist.contains("UILaunchScreen"));
    }

    #[test]
    fn test_ios_info_plist_defaults() {
        let manifest = BuildManifest {
            app_name: "Test".into(),
            bundle_id: "com.test".into(),
            version: "0.1.0".into(),
            short_version: None,
            entry: "main.ts".into(),
            icon: None,
            targets: vec!["ios".into()],
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

        let plist = generate_ios_info_plist(&manifest, None);
        // Default deployment target
        assert!(plist.contains("<string>16.0</string>"));
        // Default device families (both iPhone + iPad)
        assert!(plist.contains("<integer>1</integer>"));
        assert!(plist.contains("<integer>2</integer>"));
        // Default orientations
        assert!(plist.contains("UIInterfaceOrientationPortrait"));
        assert!(plist.contains("UIInterfaceOrientationLandscapeLeft"));
        assert!(plist.contains("UIInterfaceOrientationLandscapeRight"));
    }

    #[test]
    fn test_device_family_resolution() {
        assert_eq!(resolve_device_families(&[]), vec![1, 2]);
        assert_eq!(
            resolve_device_families(&["iphone".into()]),
            vec![1]
        );
        assert_eq!(
            resolve_device_families(&["ipad".into()]),
            vec![2]
        );
    }

    #[test]
    fn test_orientation_resolution() {
        let default = resolve_orientations(&[]);
        assert_eq!(default.len(), 3);

        let portrait_only = resolve_orientations(&["portrait".into()]);
        assert_eq!(portrait_only, vec!["UIInterfaceOrientationPortrait"]);

        let upside = resolve_orientations(&["upside-down".into()]);
        assert_eq!(
            upside,
            vec!["UIInterfaceOrientationPortraitUpsideDown"]
        );
    }

    #[test]
    fn test_ios_entitlements() {
        let entitlements = generate_ios_entitlements(
            "com.example.app",
            "TEAM123",
            &["push-notifications".into()],
        );
        assert!(entitlements.contains("application-identifier"));
        assert!(entitlements.contains("com.example.app"));
        assert!(entitlements.contains("aps-environment"));
        assert!(entitlements.contains("production"));
    }
}
