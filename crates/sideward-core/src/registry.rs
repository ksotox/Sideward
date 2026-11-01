//! Remembers installed apps (and a copy of their source IPA and tweaks) so they can be
//! re-signed before the free-account 7-day profile runs out.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    ipa::AppOptions,
    paths::{Paths, read_json, write_json},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledApp {
    pub udid: String,
    pub device_name: String,
    pub apple_id: String,
    pub team_id: String,
    /// Bundle id as installed (isideload appends the team id).
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
    /// Sideward's own copy of the IPA, so refresh doesn't depend on the original file.
    pub source: PathBuf,
    pub options: AppOptions,
    pub installed_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Re-sign automatically when the device connects (Sideloadly's "AutoRefresh").
    #[serde(default = "default_true")]
    pub auto_refresh: bool,
}

fn default_true() -> bool {
    true
}

/// Never re-sign the same app more often than this, so a device that keeps reconnecting (or
/// stays on Wi-Fi) doesn't trigger a re-sign on every scan.
pub const REFRESH_COOLDOWN_HOURS: i64 = 12;

/// When automatic refresh re-signs an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshPolicy {
    /// Re-sign once fewer than this many days are left (1-7).
    pub days_left: i64,
}

impl RefreshPolicy {
    pub fn new(days_left: i64) -> Self {
        Self {
            days_left: days_left.clamp(1, 7),
        }
    }
}

impl InstalledApp {
    pub fn key(&self) -> String {
        format!("{}@{}", self.bundle_id, self.udid)
    }

    /// Needs refreshing if it expires within `margin`, or if the expiry is unknown and it's
    /// older than six days.
    pub fn is_due(&self, margin: Duration, now: DateTime<Utc>) -> bool {
        match self.expires_at {
            Some(expiry) => expiry - margin <= now,
            None => self.installed_at + Duration::days(6) <= now,
        }
    }

    /// Whether automatic refresh (on connect, or the periodic check) should re-sign this app.
    pub fn wants_auto_refresh(&self, policy: &RefreshPolicy, now: DateTime<Utc>) -> bool {
        if !self.auto_refresh {
            return false;
        }
        // An expired app is always worth re-signing; otherwise respect the cooldown
        let expired = self.expires_at.is_some_and(|e| e <= now);
        if !expired && self.installed_at + Duration::hours(REFRESH_COOLDOWN_HOURS) > now {
            return false;
        }
        self.is_due(Duration::days(policy.days_left), now)
    }
}

/// Turn automatic refresh on or off for apps matching `query` (key, bundle id, or name).
pub fn set_auto_refresh(paths: &Paths, query: &str, enabled: bool) -> Result<usize> {
    let mut map: BTreeMap<String, InstalledApp> = read_json(&paths.registry_file())?;
    let mut changed = 0;
    for app in map.values_mut() {
        if app.key() == query || app.bundle_id == query || app.name.eq_ignore_ascii_case(query) {
            app.auto_refresh = enabled;
            changed += 1;
        }
    }
    write_json(&paths.registry_file(), &map)?;
    Ok(changed)
}

pub fn get(paths: &Paths, key: &str) -> Result<Option<InstalledApp>> {
    let map: BTreeMap<String, InstalledApp> = read_json(&paths.registry_file())?;
    Ok(map.get(key).cloned())
}

pub fn list(paths: &Paths) -> Result<Vec<InstalledApp>> {
    let map: BTreeMap<String, InstalledApp> = read_json(&paths.registry_file())?;
    let mut apps: Vec<_> = map.into_values().collect();
    apps.sort_by(|a, b| a.expires_at.cmp(&b.expires_at).then(a.name.cmp(&b.name)));
    Ok(apps)
}

pub fn upsert(paths: &Paths, app: InstalledApp) -> Result<()> {
    let mut map: BTreeMap<String, InstalledApp> = read_json(&paths.registry_file())?;
    map.insert(app.key(), app);
    write_json(&paths.registry_file(), &map)
}

/// Forget apps whose bundle id, name, or key matches `query`. Returns how many were removed.
pub fn remove(paths: &Paths, query: &str) -> Result<usize> {
    let mut map: BTreeMap<String, InstalledApp> = read_json(&paths.registry_file())?;
    let before = map.len();
    let removed: Vec<InstalledApp> = map
        .values()
        .filter(|a| a.key() == query || a.bundle_id == query || a.name.eq_ignore_ascii_case(query))
        .cloned()
        .collect();
    for app in &removed {
        map.remove(&app.key());
        let dir = app_dir(paths, &app.key());
        if app.source.starts_with(&dir) {
            let _ = fs::remove_dir_all(dir);
        }
    }
    write_json(&paths.registry_file(), &map)?;
    Ok(before - map.len())
}

fn app_dir(paths: &Paths, key: &str) -> PathBuf {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    paths.apps_dir().join(safe)
}

fn copy_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    let name = src.file_name().context("Invalid path")?;
    let dst = dir.join(name);
    if src == dst {
        return Ok(dst);
    }
    if src.is_dir() {
        for entry in walkdir::WalkDir::new(src) {
            let entry = entry?;
            let target = dst.join(entry.path().strip_prefix(src)?);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target)?;
            } else {
                fs::copy(entry.path(), &target)?;
            }
        }
    } else {
        fs::create_dir_all(dir)?;
        fs::copy(src, &dst).with_context(|| format!("Failed to copy {}", src.display()))?;
    }
    Ok(dst)
}

/// Copy the source app and tweak files into Sideward's data dir. Returns the stored source
/// path and options pointing at the stored tweak copies.
pub fn store_sources(
    paths: &Paths,
    key: &str,
    source: &Path,
    options: &AppOptions,
) -> Result<(PathBuf, AppOptions)> {
    let dir = app_dir(paths, key);
    if source.starts_with(&dir) {
        return Ok((source.to_path_buf(), options.clone()));
    }
    // Copy to a staging dir first so a failed copy never clobbers the last good one
    let staging = dir.with_extension("staging");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;
    let stored_source = copy_into(source, &staging)?;
    let mut stored_options = options.clone();
    let inject_dir = staging.join("inject");
    stored_options.inject = options
        .inject
        .iter()
        .map(|p| copy_into(p, &inject_dir))
        .collect::<Result<_>>()?;

    let _ = fs::remove_dir_all(&dir);
    fs::rename(&staging, &dir)?;
    let rebase = |p: &Path| dir.join(p.strip_prefix(&staging).unwrap_or(p));
    stored_options.inject = stored_options.inject.iter().map(|p| rebase(p)).collect();
    Ok((rebase(&stored_source), stored_options))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(expires_in_days: Option<i64>, installed_days_ago: i64) -> InstalledApp {
        let now = Utc::now();
        InstalledApp {
            udid: "U".into(),
            device_name: "Phone".into(),
            apple_id: "a@b.c".into(),
            team_id: "T".into(),
            bundle_id: "com.x.T".into(),
            name: "X".into(),
            version: None,
            source: PathBuf::from("x.ipa"),
            options: AppOptions::default(),
            installed_at: now - Duration::days(installed_days_ago),
            expires_at: expires_in_days.map(|d| now + Duration::days(d)),
            auto_refresh: true,
        }
    }

    #[test]
    fn due_logic() {
        let now = Utc::now();
        assert!(app(Some(1), 6).is_due(Duration::days(2), now));
        assert!(!app(Some(5), 2).is_due(Duration::days(2), now));
        assert!(app(None, 7).is_due(Duration::days(2), now));
        assert!(!app(None, 1).is_due(Duration::days(2), now));
    }

    #[test]
    fn auto_refresh_policy() {
        let now = Utc::now();
        let three_days = RefreshPolicy::new(3);
        let every_connect = RefreshPolicy::new(7);
        // 6 days left: not due under "3 days left", due under "every connect"
        let fresh = app(Some(6), 1);
        assert!(!fresh.wants_auto_refresh(&three_days, now));
        assert!(fresh.wants_auto_refresh(&every_connect, now));
        // Signed an hour ago: cooldown applies even under "every connect"
        let mut just_signed = app(Some(7), 0);
        just_signed.installed_at = now - Duration::hours(1);
        assert!(!just_signed.wants_auto_refresh(&every_connect, now));
        // Expired apps skip the cooldown
        let mut expired = app(Some(-1), 0);
        expired.installed_at = now - Duration::hours(1);
        assert!(expired.wants_auto_refresh(&three_days, now));
        // Turned off per app
        let mut off = app(Some(1), 6);
        off.auto_refresh = false;
        assert!(!off.wants_auto_refresh(&three_days, now));
        assert_eq!(RefreshPolicy::new(30).days_left, 7);
    }

    #[test]
    fn old_records_default_to_auto_refresh() {
        let mut json = serde_json::to_value(app(Some(3), 4)).unwrap();
        json.as_object_mut().unwrap().remove("auto_refresh");
        let parsed: InstalledApp = serde_json::from_value(json).unwrap();
        assert!(parsed.auto_refresh);
    }

    #[test]
    fn stores_and_forgets_sources() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("data"))).unwrap();
        let ipa = dir.path().join("X.ipa");
        let tweak = dir.path().join("Tweak.dylib");
        fs::write(&ipa, b"ipa").unwrap();
        fs::write(&tweak, b"dylib").unwrap();
        let options = AppOptions {
            inject: vec![tweak],
            ..Default::default()
        };

        let mut a = app(Some(3), 4);
        let (source, stored) = store_sources(&paths, &a.key(), &ipa, &options).unwrap();
        assert!(source.starts_with(paths.apps_dir()));
        assert_eq!(fs::read(&source).unwrap(), b"ipa");
        assert_eq!(fs::read(&stored.inject[0]).unwrap(), b"dylib");

        // Storing again from the stored copy is a no-op
        let (again, _) = store_sources(&paths, &a.key(), &source, &stored).unwrap();
        assert_eq!(again, source);

        a.source = source.clone();
        upsert(&paths, a.clone()).unwrap();
        assert_eq!(list(&paths).unwrap().len(), 1);
        assert_eq!(set_auto_refresh(&paths, "X", false).unwrap(), 1);
        assert!(!get(&paths, &a.key()).unwrap().unwrap().auto_refresh);
        assert_eq!(remove(&paths, "com.x.T").unwrap(), 1);
        assert!(list(&paths).unwrap().is_empty());
        assert!(!source.exists());
    }
}
