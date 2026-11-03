//! Device tools, after libimobiledevice (`ideviceinfo`, `idevicediagnostics`, `idevicesyslog`,
//! `idevicecrashreport`, `ideviceprovision`), ideviceinstaller (list/uninstall apps) and
//! ios-deploy (copy files in and out of an app). Everything works over USB and Wi-Fi.

use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use idevice::{
    IdeviceService,
    afc::{AfcClient, opcode::AfcFopenMode},
    crashreportcopymobile::{CrashReportCopyMobileClient, flush_reports},
    diagnostics_relay::DiagnosticsRelayClient,
    house_arrest::HouseArrestClient,
    installation_proxy::InstallationProxyClient,
    lockdown::LockdownClient,
    misagent::MisagentClient,
    provider::IdeviceProvider,
    syslog_relay::SyslogRelayClient,
};
use plist::{Dictionary, Value};
use serde::Serialize;

use crate::provisioning::ProfileInfo;

#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceDetails {
    pub name: String,
    pub udid: String,
    pub product_type: String,
    pub model_name: Option<String>,
    pub ios_version: String,
    pub build: Option<String>,
    pub serial: Option<String>,
    pub wifi_mac: Option<String>,
    pub battery_percent: Option<u64>,
    pub charging: Option<bool>,
    pub storage_total: Option<u64>,
    pub storage_free: Option<u64>,
    /// iOS 16+ needs Developer Mode on to open apps signed with a free account.
    pub developer_mode: Option<bool>,
}

/// Marketing names for recent iPhones (ProductType → name).
pub fn model_name(product_type: &str) -> Option<&'static str> {
    Some(match product_type {
        "iPhone12,1" => "iPhone 11",
        "iPhone12,3" => "iPhone 11 Pro",
        "iPhone12,5" => "iPhone 11 Pro Max",
        "iPhone12,8" => "iPhone SE (2nd generation)",
        "iPhone13,1" => "iPhone 12 mini",
        "iPhone13,2" => "iPhone 12",
        "iPhone13,3" => "iPhone 12 Pro",
        "iPhone13,4" => "iPhone 12 Pro Max",
        "iPhone14,4" => "iPhone 13 mini",
        "iPhone14,5" => "iPhone 13",
        "iPhone14,2" => "iPhone 13 Pro",
        "iPhone14,3" => "iPhone 13 Pro Max",
        "iPhone14,6" => "iPhone SE (3rd generation)",
        "iPhone14,7" => "iPhone 14",
        "iPhone14,8" => "iPhone 14 Plus",
        "iPhone15,2" => "iPhone 14 Pro",
        "iPhone15,3" => "iPhone 14 Pro Max",
        "iPhone15,4" => "iPhone 15",
        "iPhone15,5" => "iPhone 15 Plus",
        "iPhone16,1" => "iPhone 15 Pro",
        "iPhone16,2" => "iPhone 15 Pro Max",
        "iPhone17,3" => "iPhone 16",
        "iPhone17,4" => "iPhone 16 Plus",
        "iPhone17,1" => "iPhone 16 Pro",
        "iPhone17,2" => "iPhone 16 Pro Max",
        "iPhone17,5" => "iPhone 16e",
        _ => return None,
    })
}

async fn lockdown(provider: &dyn IdeviceProvider) -> Result<LockdownClient> {
    let mut lockdown = LockdownClient::connect(provider)
        .await
        .context("Couldn't reach the device. Is it unlocked and trusted?")?;
    let pairing = provider.get_pairing_file().await?;
    lockdown.start_session(&pairing).await?;
    Ok(lockdown)
}

/// `ideviceinfo`: identity, battery, storage and Developer Mode.
pub async fn details(provider: &dyn IdeviceProvider) -> Result<DeviceDetails> {
    let mut lockdown = lockdown(provider).await?;
    let mut get =
        async |key: &str, domain: Option<&str>| lockdown.get_value(Some(key), domain).await.ok();
    let text = |v: Option<Value>| v.and_then(|v| v.as_string().map(str::to_string));
    let number = |v: Option<Value>| v.and_then(|v| v.as_unsigned_integer());

    let product_type = text(get("ProductType", None).await).unwrap_or_default();
    Ok(DeviceDetails {
        name: text(get("DeviceName", None).await).unwrap_or_default(),
        udid: text(get("UniqueDeviceID", None).await).unwrap_or_default(),
        model_name: model_name(&product_type).map(str::to_string),
        product_type,
        ios_version: text(get("ProductVersion", None).await).unwrap_or_default(),
        build: text(get("BuildVersion", None).await),
        serial: text(get("SerialNumber", None).await),
        wifi_mac: text(get("WiFiAddress", None).await),
        battery_percent: number(
            get("BatteryCurrentCapacity", Some("com.apple.mobile.battery")).await,
        ),
        charging: get("BatteryIsCharging", Some("com.apple.mobile.battery"))
            .await
            .and_then(|v| v.as_boolean()),
        storage_total: number(get("TotalDiskCapacity", Some("com.apple.disk_usage")).await),
        storage_free: number(get("TotalDataAvailable", Some("com.apple.disk_usage")).await),
        developer_mode: get("DeveloperModeStatus", Some("com.apple.security.mac.amfi"))
            .await
            .and_then(|v| v.as_boolean()),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceApp {
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
    pub system: bool,
    /// Signing identity, e.g. "Apple Development: you@icloud.com (ABCDE12345)".
    pub signer: Option<String>,
    /// Installed with a development/free profile (sideloaded) rather than from the App Store.
    pub sideloaded: bool,
    pub file_sharing: bool,
}

fn device_app(bundle_id: String, info: &Dictionary) -> DeviceApp {
    let s = |k: &str| info.get(k).and_then(Value::as_string).map(str::to_string);
    let b = |k: &str| info.get(k).and_then(Value::as_boolean).unwrap_or(false);
    let signer = s("SignerIdentity");
    let sideloaded = b("ProfileValidated")
        || signer.as_deref().is_some_and(|i| {
            i.starts_with("Apple Development") || i.starts_with("iPhone Developer")
        });
    DeviceApp {
        name: s("CFBundleDisplayName")
            .or_else(|| s("CFBundleName"))
            .unwrap_or_else(|| bundle_id.clone()),
        version: s("CFBundleShortVersionString"),
        system: s("ApplicationType").is_some_and(|t| t == "System"),
        signer,
        sideloaded,
        file_sharing: b("UIFileSharingEnabled"),
        bundle_id,
    }
}

/// `ideviceinstaller list`: installed apps, user apps only unless `include_system`.
pub async fn list_apps(
    provider: &dyn IdeviceProvider,
    include_system: bool,
) -> Result<Vec<DeviceApp>> {
    let mut proxy = InstallationProxyClient::connect(provider).await?;
    let kind = if include_system { "Any" } else { "User" };
    let mut apps: Vec<DeviceApp> = proxy
        .get_apps(Some(kind), None)
        .await?
        .into_iter()
        .filter_map(|(id, v)| v.as_dictionary().map(|d| device_app(id, d)))
        .collect();
    apps.sort_by_key(|a| (a.system, a.name.to_lowercase()));
    Ok(apps)
}

/// `ideviceinstaller uninstall` / `ios-deploy --uninstall_only`.
pub async fn uninstall_app(provider: &dyn IdeviceProvider, bundle_id: &str) -> Result<()> {
    let mut proxy = InstallationProxyClient::connect(provider).await?;
    proxy
        .uninstall(bundle_id, None)
        .await
        .with_context(|| format!("Couldn't uninstall {bundle_id}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Restart,
    Shutdown,
    Sleep,
}

/// `idevicediagnostics restart|shutdown|sleep`.
pub async fn power(provider: &dyn IdeviceProvider, action: Power) -> Result<()> {
    let mut diag = DiagnosticsRelayClient::connect(provider).await?;
    match action {
        Power::Restart => diag.restart().await?,
        Power::Shutdown => diag.shutdown().await?,
        Power::Sleep => diag.sleep().await?,
    }
    Ok(())
}

/// `idevicesyslog`: the device's live system log.
pub struct Syslog(SyslogRelayClient);

impl Syslog {
    pub async fn open(provider: &dyn IdeviceProvider) -> Result<Self> {
        Ok(Self(SyslogRelayClient::connect(provider).await?))
    }

    pub async fn next_line(&mut self) -> Result<String> {
        Ok(self.0.next().await?)
    }
}

/// Whether a syslog line mentions `filter` (case-insensitive); an empty filter matches all.
pub fn log_matches(line: &str, filter: &str) -> bool {
    filter.is_empty() || line.to_lowercase().contains(&filter.to_lowercase())
}

/// `idevicecrashreport`: crash reports waiting on the device, newest first.
pub async fn crash_reports(provider: &dyn IdeviceProvider) -> Result<Vec<String>> {
    // Moves fresh reports into the folder crashreportcopymobile serves; harmless if it fails
    let _ = flush_reports(provider).await;
    let mut client = CrashReportCopyMobileClient::connect(provider).await?;
    let mut names: Vec<String> = client
        .ls(None)
        .await?
        .into_iter()
        .filter(|n| n.contains('.') && n != "." && n != "..")
        .collect();
    names.sort_by(|a, b| b.cmp(a));
    Ok(names)
}

pub async fn pull_crash_report(provider: &dyn IdeviceProvider, name: &str) -> Result<Vec<u8>> {
    let mut client = CrashReportCopyMobileClient::connect(provider).await?;
    Ok(client.pull(name).await?)
}

/// Crash reports for an app: report names start with the process (executable) name.
pub fn reports_for<'a>(names: &'a [String], process: &str) -> Vec<&'a String> {
    let process = process.to_lowercase();
    names
        .iter()
        .filter(|n| n.to_lowercase().starts_with(&process))
        .collect()
}

/// `ideviceprovision list`: provisioning profiles installed on the device.
pub async fn installed_profiles(provider: &dyn IdeviceProvider) -> Result<Vec<ProfileInfo>> {
    let mut misagent = MisagentClient::connect(provider).await?;
    let mut profiles: Vec<ProfileInfo> = misagent
        .copy_all()
        .await?
        .iter()
        .filter_map(|p| ProfileInfo::parse(p))
        .collect();
    profiles.sort_by_key(|p| p.expires);
    Ok(profiles)
}

pub async fn remove_profile(provider: &dyn IdeviceProvider, uuid: &str) -> Result<()> {
    let mut misagent = MisagentClient::connect(provider).await?;
    misagent
        .remove(uuid)
        .await
        .with_context(|| format!("Couldn't remove profile {uuid}"))
}

/// Remove every expired profile; returns what was removed.
pub async fn remove_expired_profiles(provider: &dyn IdeviceProvider) -> Result<Vec<ProfileInfo>> {
    let now = Utc::now();
    let expired: Vec<ProfileInfo> = installed_profiles(provider)
        .await?
        .into_iter()
        .filter(|p| p.is_expired(now))
        .collect();
    let mut misagent = MisagentClient::connect(provider).await?;
    for p in &expired {
        misagent.remove(&p.uuid).await?;
    }
    Ok(expired)
}

#[derive(Debug, Clone, Serialize)]
pub struct AppFile {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// An AFC connection into an app's sandbox (ios-deploy's `--bundle_id` file access).
/// Development-signed apps expose their whole container; others only `Documents`, and only
/// with file sharing on. Returns the client and the folder to start from.
async fn app_afc(
    provider: &dyn IdeviceProvider,
    bundle_id: &str,
) -> Result<(AfcClient, &'static str)> {
    let container = HouseArrestClient::connect(provider).await?;
    match container.vend_container(bundle_id).await {
        Ok(afc) => Ok((afc, "/")),
        Err(_) => {
            let docs = HouseArrestClient::connect(provider).await?;
            let afc = docs.vend_documents(bundle_id).await.with_context(|| {
                format!("{bundle_id} doesn't share its files (it needs file sharing or a development signature)")
            })?;
            Ok((afc, "/Documents"))
        }
    }
}

fn join(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name)
}

async fn list_dir(afc: &mut AfcClient, dir: &str) -> Result<Vec<AppFile>> {
    let mut out = Vec::new();
    for name in afc.list_dir(dir).await? {
        if name == "." || name == ".." {
            continue;
        }
        let path = join(dir, &name);
        let info = afc.get_file_info(path.as_str()).await?;
        out.push(AppFile {
            is_dir: info.st_ifmt == "S_IFDIR",
            size: info.size as u64,
            name,
            path,
        });
    }
    out.sort_by_key(|f| (!f.is_dir, f.name.to_lowercase()));
    Ok(out)
}

/// List a folder in an app's sandbox (`None` = the top it allows).
pub async fn list_app_files(
    provider: &dyn IdeviceProvider,
    bundle_id: &str,
    dir: Option<&str>,
) -> Result<Vec<AppFile>> {
    let (mut afc, root) = app_afc(provider, bundle_id).await?;
    list_dir(&mut afc, dir.unwrap_or(root)).await
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

fn pull_into<'a>(afc: &'a mut AfcClient, remote: String, local: PathBuf) -> BoxFut<'a, u64> {
    Box::pin(async move {
        let info = afc.get_file_info(remote.as_str()).await?;
        if info.st_ifmt == "S_IFDIR" {
            std::fs::create_dir_all(&local)?;
            let mut total = 0;
            for f in list_dir(afc, &remote).await? {
                total += pull_into(afc, f.path, local.join(&f.name)).await?;
            }
            Ok(total)
        } else {
            let mut file = afc.open(remote.as_str(), AfcFopenMode::RdOnly).await?;
            let bytes = file.read_entire().await?;
            file.close().await?;
            std::fs::write(&local, &bytes)
                .with_context(|| format!("Failed to write {}", local.display()))?;
            Ok(1)
        }
    })
}

/// Copy a file or folder out of an app (`ios-deploy --download`). Returns files copied.
pub async fn pull_app_files(
    provider: &dyn IdeviceProvider,
    bundle_id: &str,
    remote: &str,
    local_dir: &Path,
) -> Result<u64> {
    let (mut afc, _) = app_afc(provider, bundle_id).await?;
    let name = remote
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("files");
    let name = if name.is_empty() { "container" } else { name };
    std::fs::create_dir_all(local_dir)?;
    pull_into(&mut afc, remote.to_string(), local_dir.join(name)).await
}

fn push_from<'a>(afc: &'a mut AfcClient, local: PathBuf, remote: String) -> BoxFut<'a, u64> {
    Box::pin(async move {
        if local.is_dir() {
            afc.mk_dir(remote.as_str()).await?;
            let mut total = 0;
            for entry in std::fs::read_dir(&local)? {
                let entry = entry?;
                let child = join(&remote, &entry.file_name().to_string_lossy());
                total += push_from(afc, entry.path(), child).await?;
            }
            Ok(total)
        } else {
            let bytes = std::fs::read(&local)
                .with_context(|| format!("Failed to read {}", local.display()))?;
            let mut file = afc.open(remote.as_str(), AfcFopenMode::WrOnly).await?;
            file.write_entire(&bytes).await?;
            file.close().await?;
            Ok(1)
        }
    })
}

/// Copy a local file or folder into an app (`ios-deploy --upload`). Defaults to its Documents.
pub async fn push_app_files(
    provider: &dyn IdeviceProvider,
    bundle_id: &str,
    local: &Path,
    remote_dir: Option<&str>,
) -> Result<u64> {
    let (mut afc, root) = app_afc(provider, bundle_id).await?;
    let dir = remote_dir.unwrap_or(if root == "/" { "/Documents" } else { root });
    let Some(name) = local.file_name() else {
        bail!("Nothing to copy at {}", local.display());
    };
    push_from(
        &mut afc,
        local.to_path_buf(),
        join(dir, &name.to_string_lossy()),
    )
    .await
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_sideloaded_apps() {
        let mut info = Dictionary::new();
        info.insert("CFBundleDisplayName".into(), "YouTube".into());
        info.insert(
            "SignerIdentity".into(),
            "Apple Development: me@icloud.com (ABCDE12345)".into(),
        );
        info.insert("ApplicationType".into(), "User".into());
        let app = device_app("com.google.ios.youtube.ABCDE12345".into(), &info);
        assert!(app.sideloaded && !app.system);
        assert_eq!(app.name, "YouTube");

        let mut store = Dictionary::new();
        store.insert(
            "SignerIdentity".into(),
            "Apple iPhone OS Application Signing".into(),
        );
        assert!(!device_app("com.apple.store".into(), &store).sideloaded);
    }

    #[test]
    fn filters_and_formats() {
        assert!(log_matches("YouTube[123] <Error>: boom", "youtube"));
        assert!(!log_matches("SpringBoard: hi", "youtube"));
        assert!(log_matches("anything", ""));
        let names = vec![
            "YouTube-2026-11-01-120000.ips".to_string(),
            "Spotify-2026.ips".to_string(),
        ];
        assert_eq!(reports_for(&names, "youtube").len(), 1);
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(128_000_000_000), "128.0 GB");
        assert_eq!(model_name("iPhone15,2"), Some("iPhone 14 Pro"));
    }
}
