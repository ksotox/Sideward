//! Ad-hoc ("pseudo") signing, like `ldid -S`: no certificate and no Apple ID. Ad-hoc signed
//! apps install with TrollStore or on jailbroken devices (AppSync), not on stock iOS.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use plist::{Dictionary, Value};
use walkdir::WalkDir;

use crate::{
    ipa::{self, AppInfo, AppOptions},
    macho,
    paths::Paths,
};

/// Read an entitlements plist (XML or binary), as `ldid -S<file>` takes.
pub fn read_entitlements(path: &Path) -> Result<Dictionary> {
    Value::from_file(path)
        .with_context(|| format!("Failed to read entitlements from {}", path.display()))?
        .into_dictionary()
        .with_context(|| format!("{} is not an entitlements dictionary", path.display()))
}

/// Every bundle (main app, extensions, frameworks) with its bundle id and executable path.
fn bundles(app_dir: &Path) -> Vec<(String, PathBuf)> {
    WalkDir::new(app_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_dir() && e.path().join("Info.plist").is_file())
        .filter(|e| {
            e.depth() == 0
                || e.path()
                    .extension()
                    .is_some_and(|x| x == "appex" || x == "app" || x == "framework")
        })
        .filter_map(|e| {
            let info = Value::from_file(e.path().join("Info.plist"))
                .ok()?
                .into_dictionary()?;
            let id = info.get("CFBundleIdentifier")?.as_string()?.to_string();
            let exe = info.get("CFBundleExecutable")?.as_string()?.to_string();
            Some((id, e.path().join(exe)))
        })
        .collect()
}

/// Ad-hoc sign `app_dir` in place. Each bundle keeps the entitlements it was signed with,
/// unless `main_entitlements` replaces the main app's.
pub fn adhoc_sign(app_dir: &Path, main_entitlements: Option<Dictionary>) -> Result<()> {
    let all = bundles(app_dir);
    let (main_id, main_exe) = all.first().cloned().context("The app has no Info.plist")?;
    let existing = |exe: &Path| -> Dictionary {
        fs::read(exe)
            .ok()
            .and_then(|d| macho::code_signature(&d).ok().flatten())
            .and_then(|s| s.entitlements)
            .unwrap_or_default()
    };
    let mut by_id: BTreeMap<String, Dictionary> = all
        .iter()
        .map(|(id, exe)| (id.clone(), existing(exe)))
        .collect();
    let main = main_entitlements.unwrap_or_else(|| existing(&main_exe));
    by_id.insert(main_id, main.clone());

    // A profile from someone else's team would only confuse the installer
    for entry in WalkDir::new(app_dir).into_iter().filter_map(Result::ok) {
        if entry.file_name() == "embedded.mobileprovision" {
            fs::remove_file(entry.path())?;
        }
    }

    let mut settings = apple_codesign_quick::BundleSigningSettings::new("", main, None);
    settings.entitlements_by_bundle_id = by_id;
    apple_codesign_quick::sign_bundle(app_dir, &settings).context("Ad-hoc signing failed")?;
    Ok(())
}

/// Apply `options`, ad-hoc sign, and write an IPA to `out`. No Apple ID needed.
pub fn export_adhoc(
    paths: &Paths,
    source: &Path,
    options: &AppOptions,
    entitlements: Option<&Path>,
    out: &Path,
) -> Result<(AppInfo, Vec<String>)> {
    let entitlements = entitlements.map(read_entitlements).transpose()?;
    let prepared = ipa::prepare(paths, source, options)?;
    let result = adhoc_sign(&prepared.app_dir, entitlements)
        .and_then(|()| ipa::write_ipa(&prepared.app_dir, out));
    prepared.cleanup();
    result?;
    Ok((prepared.info.clone(), prepared.warnings.clone()))
}
