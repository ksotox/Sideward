//! Sign-and-install to a device, sign-and-export to an IPA, and refresh of remembered apps.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use idevice::{IdeviceService, provider::IdeviceProvider};
use isideload::{dev::devices::DevicesApi, util::device::IdeviceInfo};
use serde::Serialize;
use tracing::{info, warn};

use crate::{
    ReportExt,
    account::Session,
    device::{Device, Heartbeat},
    ipa::{self, AppInfo, AppOptions, PreparedApp},
    registry::{self, InstalledApp},
};

#[derive(Debug, Clone)]
pub enum Progress {
    /// A new stage started.
    Step(String),
    /// Progress within the current stage, 0.0 to 1.0.
    Fraction(f32),
    /// Something the user should know about that doesn't stop the install.
    Warning(String),
}

pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

pub fn no_progress() -> ProgressFn {
    Arc::new(|_| {})
}

#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub source: PathBuf,
    pub options: AppOptions,
    /// Keep a copy of the IPA so `refresh` can re-sign it later.
    pub remember: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallOutcome {
    pub app: AppInfo,
    /// Bundle id as installed (original id + team id).
    pub installed_bundle_id: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub warnings: Vec<String>,
}

/// Wi-Fi transfers that drop are retried this many times in total.
const WIFI_TRANSFER_ATTEMPTS: u32 = 3;

async fn start_heartbeat(
    device: &Device,
    paths: &crate::paths::Paths,
    progress: &ProgressFn,
) -> Option<Heartbeat> {
    match device.start_heartbeat(paths).await {
        Ok(h) => h,
        Err(e) => {
            warn!("No Wi-Fi heartbeat for {}: {e:#}", device.name);
            progress(Progress::Warning(format!(
                "Couldn't start the Wi-Fi keep-alive for {}; the transfer may drop: {e:#}",
                device.name
            )));
            None
        }
    }
}

/// Errors that mean the connection to the device went away mid-operation.
fn is_connection_error(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_lowercase();
    [
        "socket io failed",
        "connection reset",
        "broken pipe",
        "connection aborted",
        "timed out",
        "unexpected eof",
        "heartbeat",
    ]
    .iter()
    .any(|m| msg.contains(m))
}

/// Remove a half-uploaded copy from the device's staging folder before uploading again.
async fn clear_staging(provider: &crate::device::DeviceProvider, signed: &Path) {
    let Some(name) = signed.file_name() else {
        return;
    };
    let staged = format!("PublicStaging/{}", name.to_string_lossy());
    match idevice::afc::AfcClient::connect(provider).await {
        Ok(mut afc) => {
            if let Err(e) = afc.remove_all(staged.as_str()).await {
                tracing::debug!("Couldn't clear {staged}: {e}");
            }
        }
        Err(e) => tracing::debug!("Couldn't open AFC to clear {staged}: {e}"),
    }
}

/// Expiry date from the provisioning profile embedded in a signed app.
fn profile_expiry(app_dir: &Path) -> Option<DateTime<Utc>> {
    let data = std::fs::read(app_dir.join("embedded.mobileprovision")).ok()?;
    crate::provisioning::ProfileInfo::parse(&data)?.expires
}

async fn prepare_blocking(
    session: &Session,
    source: &Path,
    options: &AppOptions,
) -> Result<PreparedApp> {
    let paths = session.paths.clone();
    let source = source.to_path_buf();
    let options = options.clone();
    tokio::task::spawn_blocking(move || ipa::prepare(&paths, &source, &options)).await?
}

impl Session {
    /// Revoke an old development certificate without asking when the account is at its limit.
    pub fn set_auto_revoke(&self, auto_revoke: bool) {
        self.auto_revoke.store(auto_revoke, Ordering::Relaxed);
    }

    async fn sign_prepared(
        &mut self,
        prepared: &PreparedApp,
        options: &AppOptions,
        pairing: Option<&[u8]>,
        udid: Option<&str>,
        progress: &ProgressFn,
    ) -> Result<PathBuf> {
        self.register_extensions
            .store(options.register_extensions, Ordering::Relaxed);
        let team = self.team.clone();
        let p = progress.clone();
        let (signed, special) = self
            .sideloader
            .sign_app(
                prepared.app_dir.clone(),
                Some(team),
                options.increased_memory_limit,
                Some(move |f: f32| {
                    p(Progress::Fraction(f));
                    std::future::ready(())
                }),
                pairing,
                udid,
            )
            .await
            .anyhow()
            .context("Signing failed")?;
        if let Some(special) = special {
            info!("Applied special handling for {special:?}");
        }
        Ok(signed)
    }

    /// Sign `request.source` and install it on `device`.
    pub async fn install(
        &mut self,
        device: &Device,
        request: &InstallRequest,
        progress: ProgressFn,
    ) -> Result<InstallOutcome> {
        progress(Progress::Step("Preparing app".into()));
        let prepared = prepare_blocking(self, &request.source, &request.options).await?;
        for w in &prepared.warnings {
            progress(Progress::Warning(w.clone()));
        }
        let result = self
            .install_prepared(device, &prepared, request, &progress)
            .await;
        prepared.cleanup();
        let outcome = result?;

        if request.remember
            && let Err(e) = self.remember(device, request, &outcome)
        {
            warn!("Installed, but couldn't save the app for refresh: {e:#}");
            progress(Progress::Warning(format!(
                "Couldn't save the app for auto-refresh: {e:#}"
            )));
        }
        Ok(outcome)
    }

    async fn install_prepared(
        &mut self,
        device: &Device,
        prepared: &PreparedApp,
        request: &InstallRequest,
        progress: &ProgressFn,
    ) -> Result<InstallOutcome> {
        progress(Progress::Step(format!("Connecting to {}", device.name)));
        let mut heartbeat = start_heartbeat(device, &self.paths, progress).await;
        let mut provider = device.connect(&self.paths).await?;
        let device_info = IdeviceInfo::from_device(&provider)
            .await
            .anyhow()
            .context("Couldn't talk to the device. Make sure it's unlocked.")?;
        let pairing = provider
            .get_pairing_file()
            .await
            .ok()
            .and_then(|p| p.serialize().ok());

        progress(Progress::Step("Registering device with Apple".into()));
        let team = self.team.clone();
        self.dev_session()
            .ensure_device_registered(&team, &device_info.name, &device_info.udid, None)
            .await
            .anyhow()
            .context("Failed to register the device with your developer account")?;

        progress(Progress::Step("Signing".into()));
        let signed = self
            .sign_prepared(
                prepared,
                &request.options,
                pairing.as_deref(),
                Some(&device_info.udid),
                progress,
            )
            .await?;
        let expires_at = profile_expiry(&signed);

        progress(Progress::Step(format!("Installing on {}", device.name)));
        let mut attempt = 1;
        loop {
            let p = progress.clone();
            let result =
                isideload::sideload::install::install_app(&provider, &signed, move |pct| {
                    p(Progress::Fraction(pct as f32 / 100.0));
                })
                .await
                .anyhow();
            let err = match result {
                Ok(()) => break,
                Err(e) => e,
            };
            let dropped = device.transport.is_wifi() && is_connection_error(&err);
            if !dropped || attempt >= WIFI_TRANSFER_ATTEMPTS {
                return Err(if dropped {
                    err.context(format!(
                        "Transferring the app over Wi-Fi failed because the connection kept dropping. \
                         Keep {} unlocked and on the same Wi-Fi network as this computer, or use USB",
                        device.name
                    ))
                } else {
                    err.context("Transferring the app to the device failed")
                });
            }
            attempt += 1;
            warn!(
                "Wi-Fi transfer to {} dropped: {err:#}; retrying",
                device.name
            );
            progress(Progress::Warning(format!(
                "The Wi-Fi connection to {} dropped; trying again ({attempt}/{WIFI_TRANSFER_ATTEMPTS})",
                device.name
            )));
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if !heartbeat.as_ref().is_some_and(|h| h.is_running()) {
                heartbeat = start_heartbeat(device, &self.paths, progress).await;
            }
            provider = device.connect(&self.paths).await?;
            clear_staging(&provider, &signed).await;
            progress(Progress::Step(format!("Installing on {}", device.name)));
        }
        drop(heartbeat);

        Ok(InstallOutcome {
            installed_bundle_id: format!("{}.{}", prepared.info.bundle_id, self.team.team_id),
            app: prepared.info.clone(),
            expires_at,
            warnings: prepared.warnings.clone(),
        })
    }

    fn remember(
        &self,
        device: &Device,
        request: &InstallRequest,
        outcome: &InstallOutcome,
    ) -> Result<()> {
        let key = format!("{}@{}", outcome.installed_bundle_id, device.udid);
        // Re-signing keeps the user's AutoRefresh choice; new installs start with it on
        let auto_refresh = registry::get(&self.paths, &key)?.is_none_or(|a| a.auto_refresh);
        let (source, options) =
            registry::store_sources(&self.paths, &key, &request.source, &request.options)?;
        registry::upsert(
            &self.paths,
            InstalledApp {
                udid: device.udid.clone(),
                device_name: device.name.clone(),
                apple_id: self.apple_id.clone(),
                team_id: self.team.team_id.clone(),
                bundle_id: outcome.installed_bundle_id.clone(),
                name: outcome.app.name.clone(),
                version: outcome.app.version.clone(),
                source,
                options,
                installed_at: Utc::now(),
                expires_at: outcome.expires_at,
                auto_refresh,
            },
        )
    }

    /// Sign `source` and write the result to `out` as an IPA, without a device.
    /// Note: apps signed with a free account only launch on devices registered to the account.
    pub async fn export(
        &mut self,
        source: &Path,
        options: &AppOptions,
        out: &Path,
        progress: ProgressFn,
    ) -> Result<InstallOutcome> {
        progress(Progress::Step("Preparing app".into()));
        let prepared = prepare_blocking(self, source, options).await?;
        for w in &prepared.warnings {
            progress(Progress::Warning(w.clone()));
        }
        progress(Progress::Step("Signing".into()));
        let result = async {
            let signed = self
                .sign_prepared(&prepared, options, None, None, &progress)
                .await?;
            progress(Progress::Step("Writing IPA".into()));
            let expires_at = profile_expiry(&signed);
            let out = out.to_path_buf();
            tokio::task::spawn_blocking(move || ipa::write_ipa(&signed, &out)).await??;
            Ok(InstallOutcome {
                installed_bundle_id: format!("{}.{}", prepared.info.bundle_id, self.team.team_id),
                app: prepared.info.clone(),
                expires_at,
                warnings: prepared.warnings.clone(),
            })
        }
        .await;
        prepared.cleanup();
        result
    }
}

#[derive(Debug)]
pub struct RefreshResult {
    pub app: InstalledApp,
    pub result: Result<InstallOutcome>,
}

/// Which remembered apps a refresh should re-sign.
#[derive(Debug, Clone, Copy)]
pub enum RefreshFilter {
    /// Every app, whatever its expiry or AutoRefresh setting.
    All,
    /// Apps expiring within this margin (manual `refresh`).
    ExpiringWithin(chrono::Duration),
    /// Apps with AutoRefresh on that the policy says are due (on connect, periodic check).
    Auto(registry::RefreshPolicy),
}

impl RefreshFilter {
    pub fn matches(&self, app: &InstalledApp, now: DateTime<Utc>) -> bool {
        match self {
            RefreshFilter::All => true,
            RefreshFilter::ExpiringWithin(margin) => app.is_due(*margin, now),
            RefreshFilter::Auto(policy) => app.wants_auto_refresh(policy, now),
        }
    }
}

/// Apps signed with this session's Apple ID that `filter` selects, installed on one of
/// `devices`. Cheap (no network), so callers can check before logging in.
pub fn pending_refreshes(
    paths: &crate::paths::Paths,
    apple_id: Option<&str>,
    devices: &[Device],
    filter: RefreshFilter,
) -> Result<Vec<InstalledApp>> {
    let now = Utc::now();
    Ok(registry::list(paths)?
        .into_iter()
        .filter(|a| apple_id.is_none_or(|id| a.apple_id.eq_ignore_ascii_case(id)))
        .filter(|a| devices.iter().any(|d| d.udid == a.udid))
        .filter(|a| filter.matches(a, now))
        .collect())
}

/// Re-sign and reinstall the remembered apps `filter` selects, on whichever of `devices`
/// they were installed to. Apps from other Apple IDs, and apps whose device isn't reachable,
/// are skipped.
pub async fn refresh(
    session: &mut Session,
    devices: &[Device],
    filter: RefreshFilter,
    progress: ProgressFn,
) -> Result<Vec<RefreshResult>> {
    let now = Utc::now();
    let mut results = Vec::new();
    for app in registry::list(&session.paths)? {
        if !app.apple_id.eq_ignore_ascii_case(&session.apple_id) || !filter.matches(&app, now) {
            continue;
        }
        let Some(device) = devices
            .iter()
            .filter(|d| d.udid == app.udid)
            .min_by_key(|d| d.transport.is_wifi())
        else {
            info!("Skipping {}: {} isn't reachable", app.name, app.device_name);
            continue;
        };
        progress(Progress::Step(format!(
            "Refreshing {} on {}",
            app.name, device.name
        )));
        let request = InstallRequest {
            source: app.source.clone(),
            options: app.options.clone(),
            remember: true,
        };
        let result = session.install(device, &request, progress.clone()).await;
        results.push(RefreshResult { app, result });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_dropped_connections() {
        let dropped = anyhow::anyhow!("Failed to upload: device socket io failed");
        assert!(is_connection_error(&dropped));
        let other = anyhow::anyhow!("ApplicationVerificationFailed");
        assert!(!is_connection_error(&other));
    }

    #[test]
    fn reads_profile_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let profile = br#"0\x82garbage<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>ExpirationDate</key><date>2026-11-01T12:00:00Z</date></dict></plist>trailing"#;
        std::fs::write(dir.path().join("embedded.mobileprovision"), profile).unwrap();
        let expiry = profile_expiry(dir.path()).unwrap();
        assert_eq!(expiry.to_rfc3339(), "2026-11-01T12:00:00+00:00");
        assert!(profile_expiry(&dir.path().join("missing")).is_none());
    }
}
