use image::imageops::FilterType;
use image::DynamicImage;
use std::io::Write;
use std::path::Path;

/// iOS icon sizes (filename, size in pixels)
/// These are the required sizes for App Store submission
const IOS_ICON_SIZES: &[(&str, u32)] = &[
    ("Icon-20.png", 20),       // iPad @1x
    ("Icon-20@2x.png", 40),
    ("Icon-20@3x.png", 60),
    ("Icon-29.png", 29),       // iPad @1x
    ("Icon-29@2x.png", 58),
    ("Icon-29@3x.png", 87),
    ("Icon-40.png", 40),       // iPad @1x
    ("Icon-40@2x.png", 80),
    ("Icon-40@3x.png", 120),
    ("Icon-60@2x.png", 120),
    ("Icon-60@3x.png", 180),
    ("Icon-76.png", 76),
    ("Icon-76@2x.png", 152),
    ("Icon-83.5@2x.png", 167),
    ("Icon-1024.png", 1024),
];

/// Generate all required iOS icon sizes from a source icon
pub fn generate_ios_icons(icon_path: &Path, output_dir: &Path) -> Result<(), String> {
    let img = image::open(icon_path).map_err(|e| format!("Failed to open icon: {e}"))?;

    if img.width() < 1024 || img.height() < 1024 {
        return Err(format!(
            "Icon must be at least 1024x1024, got {}x{}",
            img.width(),
            img.height()
        ));
    }

    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create icon output dir: {e}"))?;

    for (filename, size) in IOS_ICON_SIZES {
        let resized = img.resize_exact(*size, *size, FilterType::Lanczos3);
        let output_path = output_dir.join(filename);
        resized
            .save(&output_path)
            .map_err(|e| format!("Failed to save {filename}: {e}"))?;
    }

    Ok(())
}

/// Android icon density buckets (path relative to res/, size in pixels)
const ANDROID_ICON_SIZES: &[(&str, u32)] = &[
    ("mipmap-mdpi/ic_launcher.png", 48),
    ("mipmap-hdpi/ic_launcher.png", 72),
    ("mipmap-xhdpi/ic_launcher.png", 96),
    ("mipmap-xxhdpi/ic_launcher.png", 144),
    ("mipmap-xxxhdpi/ic_launcher.png", 192),
    ("playstore-icon.png", 512),
];

/// Generate all required Android icon sizes from a source icon
pub fn generate_android_icons(icon_path: &Path, output_dir: &Path) -> Result<(), String> {
    let img = image::open(icon_path).map_err(|e| format!("Failed to open icon: {e}"))?;

    if img.width() < 1024 || img.height() < 1024 {
        return Err(format!(
            "Icon must be at least 1024x1024, got {}x{}",
            img.width(),
            img.height()
        ));
    }

    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create icon output dir: {e}"))?;

    for (rel_path, size) in ANDROID_ICON_SIZES {
        let resized = img.resize_exact(*size, *size, FilterType::Lanczos3);
        let output_path = output_dir.join(rel_path);
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create icon dir {}: {e}", parent.display()))?;
        }
        resized
            .save(&output_path)
            .map_err(|e| format!("Failed to save {rel_path}: {e}"))?;
    }

    Ok(())
}

/// ICNS icon type tags and their sizes
const ICNS_ENTRIES: &[(&[u8; 4], u32)] = &[
    (b"ic07", 128),   // 128x128 PNG
    (b"ic08", 256),   // 256x256 PNG
    (b"ic09", 512),   // 512x512 PNG
    (b"ic10", 1024),  // 1024x1024 PNG (retina 512x512)
    (b"ic11", 32),    // 32x32 PNG (retina 16x16)
    (b"ic12", 64),    // 64x64 PNG (retina 32x32)
    (b"ic13", 256),   // 256x256 PNG (retina 128x128)
    (b"ic14", 512),   // 512x512 PNG (retina 256x256)
];

/// ICNS file magic number
const ICNS_MAGIC: &[u8; 4] = b"icns";

pub fn generate_icns(icon_path: &Path, output_path: &Path) -> Result<(), String> {
    let img = image::open(icon_path).map_err(|e| format!("Failed to open icon: {e}"))?;

    if img.width() < 1024 || img.height() < 1024 {
        return Err(format!(
            "Icon must be at least 1024x1024, got {}x{}",
            img.width(),
            img.height()
        ));
    }

    let mut entries: Vec<(Vec<u8>, &[u8; 4])> = Vec::new();

    for (tag, size) in ICNS_ENTRIES {
        let resized = img.resize_exact(*size, *size, FilterType::Lanczos3);
        let png_data = encode_png(&resized)?;
        entries.push((png_data, tag));
    }

    write_icns(output_path, &entries)
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut buf);
    img.write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode PNG: {e}"))?;
    Ok(buf)
}

fn write_icns(output_path: &Path, entries: &[(Vec<u8>, &[u8; 4])]) -> Result<(), String> {
    let mut file =
        std::fs::File::create(output_path).map_err(|e| format!("Failed to create icns: {e}"))?;

    // Calculate total size: 8 (header) + sum of (8 + data_len) per entry
    let total_size: u32 = 8 + entries
        .iter()
        .map(|(data, _)| 8 + data.len() as u32)
        .sum::<u32>();

    // Write header
    file.write_all(ICNS_MAGIC)
        .map_err(|e| format!("Write error: {e}"))?;
    file.write_all(&total_size.to_be_bytes())
        .map_err(|e| format!("Write error: {e}"))?;

    // Write each entry
    for (data, tag) in entries {
        let entry_size = 8 + data.len() as u32;
        file.write_all(*tag)
            .map_err(|e| format!("Write error: {e}"))?;
        file.write_all(&entry_size.to_be_bytes())
            .map_err(|e| format!("Write error: {e}"))?;
        file.write_all(data)
            .map_err(|e| format!("Write error: {e}"))?;
    }

    Ok(())
}

/// Compile an iOS icon asset catalog (AppIcon.appiconset) from a directory of icon PNGs.
/// Returns the path to the compiled `Assets.car` file.
pub async fn compile_ios_icon_asset_catalog(
    icons_dir: &Path,
    deployment_target: &str,
    work_dir: &Path,
) -> Result<std::path::PathBuf, String> {
    let xcassets = work_dir.join("AppIcons.xcassets");
    let appiconset = xcassets.join("AppIcon.appiconset");
    std::fs::create_dir_all(&appiconset)
        .map_err(|e| format!("Failed to create appiconset dir: {e}"))?;

    // Copy icon PNGs into the appiconset
    let mut images_json = Vec::new();
    for (filename, size) in IOS_ICON_SIZES {
        let src = icons_dir.join(filename);
        if src.exists() {
            std::fs::copy(&src, appiconset.join(filename))
                .map_err(|e| format!("Failed to copy icon {filename}: {e}"))?;
            // Build Contents.json entry
            let (idiom, scale, pt_size) = icon_entry_metadata(filename, *size);
            images_json.push(format!(
                r#"    {{"filename": "{filename}", "idiom": "{idiom}", "scale": "{scale}", "size": "{pt_size}"}}"#
            ));
        }
    }
    // Always include the 1024 marketing icon
    let marketing = icons_dir.join("Icon-1024.png");
    if marketing.exists() {
        images_json.push(
            r#"    {"filename": "Icon-1024.png", "idiom": "ios-marketing", "scale": "1x", "size": "1024x1024"}"#
                .to_string(),
        );
    }

    let contents = format!(
        "{{\n  \"images\": [\n{}\n  ],\n  \"info\": {{\"author\": \"xcode\", \"version\": 1}}\n}}",
        images_json.join(",\n")
    );
    std::fs::write(appiconset.join("Contents.json"), &contents)
        .map_err(|e| format!("Failed to write Contents.json: {e}"))?;

    // Compile with actool
    let compiled_dir = work_dir.join("compiled_assets");
    std::fs::create_dir_all(&compiled_dir)
        .map_err(|e| format!("Failed to create compiled_assets dir: {e}"))?;

    let output = tokio::process::Command::new("xcrun")
        .arg("actool")
        .arg(&xcassets)
        .arg("--compile")
        .arg(&compiled_dir)
        .arg("--platform")
        .arg("iphoneos")
        .arg("--minimum-deployment-target")
        .arg(deployment_target)
        .arg("--app-icon")
        .arg("AppIcon")
        .arg("--output-partial-info-plist")
        .arg("/dev/null")
        .output()
        .await
        .map_err(|e| format!("Failed to run actool: {e}"))?;

    let assets_car = compiled_dir.join("Assets.car");
    if !assets_car.exists() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "actool did not produce Assets.car:\nstdout: {stdout}\nstderr: {stderr}"
        ));
    }

    Ok(assets_car)
}

fn icon_entry_metadata(filename: &str, size: u32) -> (&'static str, &'static str, String) {
    let scale = if filename.contains("@3x") {
        "3x"
    } else if filename.contains("@2x") {
        "2x"
    } else {
        "1x"
    };
    let divisor: u32 = match scale {
        "3x" => 3,
        "2x" => 2,
        _ => 1,
    };
    let pt = size / divisor;
    // For 83.5pt the file is "Icon-83.5@2x.png", size=167, pt=83.5
    let pt_str = if filename.contains("83.5") {
        "83.5x83.5".to_string()
    } else {
        format!("{pt}x{pt}")
    };
    // Determine idiom: files ending in @3x are iPhone-only
    let idiom = if filename.contains("@3x") || filename.contains("Icon-60") {
        "iphone"
    } else if filename.contains("Icon-83.5") || filename.contains("Icon-76") {
        "ipad"
    } else {
        "universal"
    };
    (idiom, scale, pt_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_icns_byte_layout() {
        // Create a minimal 1024x1024 test image
        let img = DynamicImage::new_rgba8(1024, 1024);
        let tmpdir = std::env::temp_dir().join("perry-test-icns");
        std::fs::create_dir_all(&tmpdir).unwrap();

        let icon_path = tmpdir.join("test.png");
        img.save(&icon_path).unwrap();

        let output_path = tmpdir.join("test.icns");
        generate_icns(&icon_path, &output_path).unwrap();

        let data = std::fs::read(&output_path).unwrap();

        // Check magic
        assert_eq!(&data[0..4], b"icns");

        // Check total size matches file size
        let total_size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        assert_eq!(total_size as usize, data.len());

        // Check first entry tag
        assert_eq!(&data[8..12], b"ic07");

        // Cleanup
        std::fs::remove_dir_all(&tmpdir).ok();
    }

    #[test]
    fn test_android_icon_generation() {
        let img = DynamicImage::new_rgba8(1024, 1024);
        let tmpdir = std::env::temp_dir().join("perry-test-android-icons");
        std::fs::create_dir_all(&tmpdir).unwrap();

        let icon_path = tmpdir.join("test.png");
        img.save(&icon_path).unwrap();

        let output_dir = tmpdir.join("res");
        generate_android_icons(&icon_path, &output_dir).unwrap();

        // Check that all density bucket files were created
        assert!(output_dir.join("mipmap-mdpi/ic_launcher.png").exists());
        assert!(output_dir.join("mipmap-hdpi/ic_launcher.png").exists());
        assert!(output_dir.join("mipmap-xhdpi/ic_launcher.png").exists());
        assert!(output_dir.join("mipmap-xxhdpi/ic_launcher.png").exists());
        assert!(output_dir.join("mipmap-xxxhdpi/ic_launcher.png").exists());
        assert!(output_dir.join("playstore-icon.png").exists());

        // Verify a specific size
        let mdpi = image::open(output_dir.join("mipmap-mdpi/ic_launcher.png")).unwrap();
        assert_eq!(mdpi.width(), 48);
        assert_eq!(mdpi.height(), 48);

        std::fs::remove_dir_all(&tmpdir).ok();
    }

    #[test]
    fn test_rejects_small_icon() {
        let tmpdir = std::env::temp_dir().join("perry-test-icns-small");
        std::fs::create_dir_all(&tmpdir).unwrap();

        let img = DynamicImage::new_rgba8(512, 512);
        let icon_path = tmpdir.join("small.png");
        img.save(&icon_path).unwrap();

        let output_path = tmpdir.join("small.icns");
        let result = generate_icns(&icon_path, &output_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("1024x1024"));

        std::fs::remove_dir_all(&tmpdir).ok();
    }
}
