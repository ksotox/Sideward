//! Reading IPAs and applying Sideloadly-style modifications before signing.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::{inject, macho, paths::Paths};

/// Changes to make to the app before signing. Everything is off by default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppOptions {
    /// New home-screen name.
    pub name: Option<String>,
    /// New bundle identifier. Lets you install a second copy of an app.
    pub bundle_id: Option<String>,
    pub version: Option<String>,
    pub min_ios: Option<String>,
    /// Show the app's Documents folder in the Files app.
    pub file_sharing: bool,
    /// Allow installing on devices the app doesn't list (e.g. iPad-only apps on iPhone).
    pub remove_supported_devices: bool,
    /// Drop app extensions (widgets, share extensions...). Saves App IDs on free accounts.
    pub remove_extensions: bool,
    pub remove_watch_app: bool,
    /// Tweaks to inject: `.dylib`, `.framework`, `.bundle`, or `.deb`.
    pub inject: Vec<PathBuf>,
    /// Register a separate App ID per extension instead of reusing the main one.
    pub register_extensions: bool,
    /// Request the increased-memory-limit entitlement (emulators, LiveContainer...).
    pub increased_memory_limit: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
    pub build: Option<String>,
    pub min_ios: Option<String>,
    pub executable: String,
    pub extensions: Vec<String>,
    pub has_watch_app: bool,
    pub encrypted: bool,
    /// The main executable's code signature (`codesign -d` / `ldid -e`), if signed.
    pub signature: Option<macho::CodeSignature>,
    /// The embedded provisioning profile, if any.
    pub profile: Option<crate::provisioning::ProfileInfo>,
}

pub struct PreparedApp {
    pub work_dir: PathBuf,
    pub app_dir: PathBuf,
    pub info: AppInfo,
    pub warnings: Vec<String>,
}

impl PreparedApp {
    pub fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.work_dir);
    }
}

fn info_string(dict: &Dictionary, key: &str) -> Option<String> {
    dict.get(key).and_then(Value::as_string).map(str::to_string)
}

fn app_info_from_plist(dict: &Dictionary) -> Result<AppInfo> {
    let bundle_id =
        info_string(dict, "CFBundleIdentifier").context("Info.plist has no CFBundleIdentifier")?;
    let executable =
        info_string(dict, "CFBundleExecutable").context("Info.plist has no CFBundleExecutable")?;
    let name = info_string(dict, "CFBundleDisplayName")
        .or_else(|| info_string(dict, "CFBundleName"))
        .unwrap_or_else(|| executable.clone());
    Ok(AppInfo {
        bundle_id,
        name,
        version: info_string(dict, "CFBundleShortVersionString"),
        build: info_string(dict, "CFBundleVersion"),
        min_ios: info_string(dict, "MinimumOSVersion"),
        executable,
        extensions: Vec::new(),
        has_watch_app: false,
        encrypted: false,
        signature: None,
        profile: None,
    })
}

/// Read app details straight from the IPA without extracting it.
pub fn inspect(ipa: &Path) -> Result<AppInfo> {
    if ipa.is_dir() {
        return inspect_app_dir(ipa);
    }
    let file = fs::File::open(ipa).with_context(|| format!("Failed to open {}", ipa.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("Not a valid IPA (zip) file")?;

    let names: Vec<String> = zip.file_names().map(str::to_string).collect();
    let app_prefix = names
        .iter()
        .filter_map(|n| {
            let rest = n.strip_prefix("Payload/")?;
            let (app, _) = rest.split_once(".app/")?;
            (!app.contains('/')).then(|| format!("Payload/{app}.app/"))
        })
        .next()
        .context("No Payload/*.app in this IPA")?;

    let mut buf = Vec::new();
    zip.by_name(&format!("{app_prefix}Info.plist"))
        .context("The app has no Info.plist")?
        .read_to_end(&mut buf)?;
    let dict = Value::from_reader(std::io::Cursor::new(&buf))?
        .into_dictionary()
        .context("Info.plist is not a dictionary")?;
    let mut info = app_info_from_plist(&dict)?;

    info.extensions = names
        .iter()
        .filter_map(|n| {
            let rest = n.strip_prefix(&app_prefix)?;
            let rest = rest
                .strip_prefix("PlugIns/")
                .or_else(|| rest.strip_prefix("Extensions/"))?;
            let (ext, tail) = rest.split_once('/')?;
            (tail == "Info.plist").then(|| ext.to_string())
        })
        .collect();
    info.has_watch_app = names
        .iter()
        .any(|n| n.starts_with(&format!("{app_prefix}Watch/")));

    let mut exe = Vec::new();
    if let Ok(mut f) = zip.by_name(&format!("{app_prefix}{}", info.executable)) {
        f.read_to_end(&mut exe)?;
        read_binary_details(&mut info, &exe);
    }
    let mut profile = Vec::new();
    if let Ok(mut f) = zip.by_name(&format!("{app_prefix}embedded.mobileprovision")) {
        f.read_to_end(&mut profile)?;
        info.profile = crate::provisioning::ProfileInfo::parse(&profile);
    }
    Ok(info)
}

fn inspect_app_dir(app_dir: &Path) -> Result<AppInfo> {
    let dict = read_plist_dict(&app_dir.join("Info.plist"))?;
    let mut info = app_info_from_plist(&dict)?;
    for sub in ["PlugIns", "Extensions"] {
        if let Ok(entries) = fs::read_dir(app_dir.join(sub)) {
            info.extensions.extend(
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.ends_with(".appex")),
            );
        }
    }
    info.has_watch_app = app_dir.join("Watch").is_dir();
    if let Ok(exe) = fs::read(app_dir.join(&info.executable)) {
        read_binary_details(&mut info, &exe);
    }
    if let Ok(profile) = fs::read(app_dir.join("embedded.mobileprovision")) {
        info.profile = crate::provisioning::ProfileInfo::parse(&profile);
    }
    Ok(info)
}

fn read_binary_details(info: &mut AppInfo, exe: &[u8]) {
    if macho::is_macho(exe) {
        info.encrypted = macho::is_encrypted(exe).unwrap_or(false);
        info.signature = macho::code_signature(exe).ok().flatten();
    }
}

fn read_plist_dict(path: &Path) -> Result<Dictionary> {
    Value::from_file(path)
        .with_context(|| format!("Failed to read {}", path.display()))?
        .into_dictionary()
        .with_context(|| format!("{} is not a dictionary", path.display()))
}

fn write_plist_dict(path: &Path, dict: Dictionary) -> Result<()> {
    Value::Dictionary(dict)
        .to_file_xml(path)
        .with_context(|| format!("Failed to write {}", path.display()))
}

fn extract_zip(ipa: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(ipa).with_context(|| format!("Failed to open {}", ipa.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("Not a valid IPA (zip) file")?;
    // `extract` rejects entries with paths escaping `dest`
    zip.extract(dest).context("Failed to extract the IPA")?;
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let target = dst.join(entry.path().strip_prefix(src)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn find_app_dir(payload: &Path) -> Result<PathBuf> {
    let apps: Vec<PathBuf> = fs::read_dir(payload)
        .with_context(|| "No Payload folder in the IPA")?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().is_some_and(|e| e == "app"))
        .collect();
    match apps.len() {
        1 => Ok(apps.into_iter().next().expect("one app")),
        0 => bail!("No .app inside Payload"),
        _ => bail!("Several .app bundles inside Payload"),
    }
}

/// Bundles nested in the app (extensions, watch app and its extensions).
fn nested_bundles(app_dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(app_dir)
        .min_depth(1)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().is_dir()
                && e.path()
                    .extension()
                    .is_some_and(|x| x == "appex" || x == "app")
                && e.path().join("Info.plist").is_file()
        })
        .map(|e| e.into_path())
        .collect()
}

fn rename_bundle_ids(app_dir: &Path, old: &str, new: &str) -> Result<()> {
    let swap = |id: &str| -> Option<String> {
        if id == old {
            Some(new.to_string())
        } else {
            id.strip_prefix(&format!("{old}."))
                .map(|rest| format!("{new}.{rest}"))
        }
    };
    for bundle in nested_bundles(app_dir) {
        let plist_path = bundle.join("Info.plist");
        let mut dict = read_plist_dict(&plist_path)?;
        let mut changed = false;
        for key in ["CFBundleIdentifier", "WKCompanionAppBundleIdentifier"] {
            if let Some(updated) = info_string(&dict, key).and_then(|id| swap(&id)) {
                dict.insert(key.into(), updated.into());
                changed = true;
            }
        }
        if let Some(Value::Dictionary(ext)) = dict.get_mut("NSExtension")
            && let Some(Value::Dictionary(attrs)) = ext.get_mut("NSExtensionAttributes")
            && let Some(updated) =
                info_string(attrs, "WKAppBundleIdentifier").and_then(|id| swap(&id))
        {
            attrs.insert("WKAppBundleIdentifier".into(), updated.into());
            changed = true;
        }
        if changed {
            write_plist_dict(&plist_path, dict)?;
        }
    }
    Ok(())
}

/// Update localized display names stored as binary/XML plists. Old-style text `.strings`
/// files are left alone.
fn rename_localized(app_dir: &Path, name: &str) {
    let Ok(entries) = fs::read_dir(app_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let strings = entry.path().join("InfoPlist.strings");
        if !entry.file_name().to_string_lossy().ends_with(".lproj") || !strings.is_file() {
            continue;
        }
        if let Ok(Value::Dictionary(mut dict)) = Value::from_file(&strings) {
            let mut changed = false;
            for key in ["CFBundleDisplayName", "CFBundleName"] {
                if dict.contains_key(key) {
                    dict.insert(key.into(), name.into());
                    changed = true;
                }
            }
            if changed {
                let _ = Value::Dictionary(dict).to_file_binary(&strings);
            }
        }
    }
}

/// Extract (or copy) the app into a fresh work dir and apply `options`.
pub fn prepare(paths: &Paths, source: &Path, options: &AppOptions) -> Result<PreparedApp> {
    let work_dir = paths
        .work_dir()
        .join(uuid::Uuid::new_v4().simple().to_string());
    fs::create_dir_all(&work_dir)?;
    let result = prepare_in(&work_dir, source, options);
    if result.is_err() {
        let _ = fs::remove_dir_all(&work_dir);
    }
    result
}

fn prepare_in(work_dir: &Path, source: &Path, options: &AppOptions) -> Result<PreparedApp> {
    let payload = work_dir.join("Payload");
    if source.is_dir() {
        let name = source.file_name().context("Invalid app path")?;
        copy_dir(source, &payload.join(name))?;
    } else {
        extract_zip(source, work_dir)?;
    }
    let app_dir = find_app_dir(&payload)?;
    let plist_path = app_dir.join("Info.plist");
    let mut dict = read_plist_dict(&plist_path)?;
    let original = app_info_from_plist(&dict)?;

    let exe = fs::read(app_dir.join(&original.executable))
        .with_context(|| format!("The app's executable ({}) is missing", original.executable))?;
    if macho::is_macho(&exe) && macho::is_encrypted(&exe)? {
        bail!(
            "This IPA is encrypted (it came straight from the App Store). Sideloading needs a decrypted IPA."
        );
    }

    let mut warnings = Vec::new();
    if let Some(name) = &options.name {
        dict.insert("CFBundleDisplayName".into(), name.clone().into());
        dict.insert("CFBundleName".into(), name.clone().into());
        rename_localized(&app_dir, name);
    }
    if let Some(version) = &options.version {
        dict.insert("CFBundleShortVersionString".into(), version.clone().into());
    }
    if let Some(min_ios) = &options.min_ios {
        dict.insert("MinimumOSVersion".into(), min_ios.clone().into());
    }
    if options.file_sharing {
        dict.insert("UIFileSharingEnabled".into(), true.into());
        dict.insert("LSSupportsOpeningDocumentsInPlace".into(), true.into());
    }
    if options.remove_supported_devices {
        dict.remove("UISupportedDevices");
    }
    if let Some(new_id) = &options.bundle_id {
        dict.insert("CFBundleIdentifier".into(), new_id.clone().into());
    }
    write_plist_dict(&plist_path, dict)?;

    if options.remove_extensions {
        for sub in ["PlugIns", "Extensions"] {
            let _ = fs::remove_dir_all(app_dir.join(sub));
        }
    }
    if options.remove_watch_app {
        let _ = fs::remove_dir_all(app_dir.join("Watch"));
    }
    if let Some(new_id) = &options.bundle_id {
        rename_bundle_ids(&app_dir, &original.bundle_id, new_id)?;
    }

    if !options.inject.is_empty() {
        let scratch = work_dir.join("inject");
        fs::create_dir_all(&scratch)?;
        let items = inject::collect(&options.inject, &scratch)?;
        let report = inject::inject(&app_dir, &original.executable, &items)?;
        warnings.extend(report.warnings);
        let _ = fs::remove_dir_all(&scratch);
    }

    let info = inspect_app_dir(&app_dir)?;
    Ok(PreparedApp {
        work_dir: work_dir.to_path_buf(),
        app_dir,
        info,
        warnings,
    })
}

/// Zip a (signed) `.app` into an IPA, keeping executable bits.
pub fn write_ipa(app_dir: &Path, out: &Path) -> Result<()> {
    let app_name = app_dir
        .file_name()
        .context("Invalid app path")?
        .to_string_lossy()
        .into_owned();
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let file =
        fs::File::create(out).with_context(|| format!("Failed to create {}", out.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    for entry in WalkDir::new(app_dir).sort_by_file_name() {
        let entry = entry?;
        let rel = entry.path().strip_prefix(app_dir)?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        let name = if rel.is_empty() {
            format!("Payload/{app_name}/")
        } else {
            format!("Payload/{app_name}/{rel}")
        };
        let mode = unix_mode(entry.path(), entry.file_type().is_dir());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(mode);
        if entry.file_type().is_dir() {
            zip.add_directory(name, options)?;
        } else {
            zip.start_file(name, options)?;
            let mut f = fs::File::open(entry.path())?;
            std::io::copy(&mut f, &mut zip)?;
        }
    }
    zip.finish()?.flush()?;
    Ok(())
}

#[cfg(unix)]
fn unix_mode(path: &Path, _is_dir: bool) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o644)
}

#[cfg(not(unix))]
fn unix_mode(path: &Path, is_dir: bool) -> u32 {
    // No permission bits on Windows: mark directories and Mach-O files executable
    if is_dir {
        return 0o755;
    }
    let mut magic = [0u8; 4];
    let is_exec = fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map(|_| macho::is_macho(&magic))
        .unwrap_or(false);
    if is_exec { 0o755 } else { 0o644 }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Write a minimal unencrypted IPA with one extension.
    pub(crate) fn sample_ipa(dir: &Path) -> PathBuf {
        let app = dir.join("src/Payload/Demo.app");
        fs::create_dir_all(app.join("PlugIns/Widget.appex")).unwrap();
        let mut info = Dictionary::new();
        info.insert("CFBundleIdentifier".into(), "com.example.demo".into());
        info.insert("CFBundleExecutable".into(), "Demo".into());
        info.insert("CFBundleName".into(), "Demo".into());
        info.insert("CFBundleShortVersionString".into(), "1.0".into());
        info.insert("MinimumOSVersion".into(), "15.0".into());
        info.insert(
            "UISupportedDevices".into(),
            Value::Array(vec!["iPad8,1".into()]),
        );
        Value::Dictionary(info)
            .to_file_binary(app.join("Info.plist"))
            .unwrap();
        fs::write(app.join("Demo"), macho::tests::sample_macho(0)).unwrap();
        let mut ext = Dictionary::new();
        ext.insert(
            "CFBundleIdentifier".into(),
            "com.example.demo.widget".into(),
        );
        ext.insert("CFBundleExecutable".into(), "Widget".into());
        Value::Dictionary(ext)
            .to_file_xml(app.join("PlugIns/Widget.appex/Info.plist"))
            .unwrap();

        let ipa = dir.join("Demo.ipa");
        write_ipa(&app, &ipa).unwrap();
        ipa
    }

    #[test]
    fn inspects_without_extracting() {
        let dir = tempfile::tempdir().unwrap();
        let info = inspect(&sample_ipa(dir.path())).unwrap();
        assert_eq!(info.bundle_id, "com.example.demo");
        assert_eq!(info.version.as_deref(), Some("1.0"));
        assert_eq!(info.extensions, vec!["Widget.appex"]);
        assert!(!info.encrypted);
    }

    #[test]
    fn applies_options() {
        let dir = tempfile::tempdir().unwrap();
        let ipa = sample_ipa(dir.path());
        let paths = Paths::new(Some(dir.path().join("data"))).unwrap();
        let options = AppOptions {
            name: Some("Demo 2".into()),
            bundle_id: Some("com.me.demo2".into()),
            min_ios: Some("14.0".into()),
            file_sharing: true,
            remove_supported_devices: true,
            ..Default::default()
        };
        let prepared = prepare(&paths, &ipa, &options).unwrap();
        assert_eq!(prepared.info.bundle_id, "com.me.demo2");
        assert_eq!(prepared.info.name, "Demo 2");
        assert_eq!(prepared.info.min_ios.as_deref(), Some("14.0"));
        let dict = read_plist_dict(&prepared.app_dir.join("Info.plist")).unwrap();
        assert!(!dict.contains_key("UISupportedDevices"));
        assert_eq!(
            dict.get("UIFileSharingEnabled").and_then(Value::as_boolean),
            Some(true)
        );
        let ext =
            read_plist_dict(&prepared.app_dir.join("PlugIns/Widget.appex/Info.plist")).unwrap();
        assert_eq!(
            info_string(&ext, "CFBundleIdentifier").as_deref(),
            Some("com.me.demo2.widget")
        );
        prepared.cleanup();
        assert!(!prepared.work_dir.exists());
    }

    #[test]
    fn removes_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let ipa = sample_ipa(dir.path());
        let paths = Paths::new(Some(dir.path().join("data"))).unwrap();
        let options = AppOptions {
            remove_extensions: true,
            ..Default::default()
        };
        let prepared = prepare(&paths, &ipa, &options).unwrap();
        assert!(prepared.info.extensions.is_empty());
    }

    #[test]
    fn rejects_encrypted_apps() {
        let dir = tempfile::tempdir().unwrap();
        let ipa = sample_ipa(dir.path());
        // Re-pack with an encrypted executable
        let app = dir.path().join("src/Payload/Demo.app");
        fs::write(app.join("Demo"), macho::tests::sample_macho(1)).unwrap();
        write_ipa(&app, &ipa).unwrap();
        assert!(inspect(&ipa).unwrap().encrypted);
        let paths = Paths::new(Some(dir.path().join("data"))).unwrap();
        let err = prepare(&paths, &ipa, &AppOptions::default()).err().unwrap();
        assert!(err.to_string().contains("encrypted"));
    }
}
