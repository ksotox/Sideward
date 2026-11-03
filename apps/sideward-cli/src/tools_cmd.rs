//! `sideward device ...` (libimobiledevice / ideviceinstaller / ios-deploy style tools) and
//! `sideward source ...` (AltStore/SideStore sources).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use sideward_core::{
    install::InstallRequest,
    ipa::AppInfo,
    provisioning::ProfileInfo,
    registry, sources,
    tools::{self, Power, format_bytes},
};

use super::{
    AccountArgs, AppOptionArgs, Ctx, DeviceArgs, ProgressUi, login, print_expiry, resolve_device,
    truncate,
};

#[derive(Args)]
pub struct DeviceCmd {
    #[command(flatten)]
    target: DeviceArgs,
    #[command(subcommand)]
    action: DeviceAction,
}

#[derive(Subcommand)]
enum DeviceAction {
    /// Model, iOS version, battery, storage, Developer Mode (like ideviceinfo)
    Info,
    /// Installed apps (like ideviceinstaller list)
    Apps {
        /// Include Apple's system apps
        #[arg(long)]
        system: bool,
    },
    /// Remove an app by bundle id (like ideviceinstaller uninstall)
    Uninstall {
        bundle_id: String,
    },
    Restart,
    Shutdown,
    Sleep,
    /// Stream the device's live log (like idevicesyslog)
    Syslog {
        /// Only lines containing this text (e.g. the app name)
        #[arg(short, long, default_value = "")]
        filter: String,
    },
    /// Crash reports (like idevicecrashreport)
    Crashes {
        /// Only reports for this app (executable name)
        #[arg(long)]
        app: Option<String>,
        /// Copy the reports into this folder
        #[arg(long, value_name = "DIR")]
        save: Option<PathBuf>,
    },
    /// Provisioning profiles on the device (like ideviceprovision)
    #[command(subcommand)]
    Profiles(ProfilesAction),
    /// Files inside an app (like ios-deploy --list/--download/--upload)
    #[command(subcommand)]
    Files(FilesAction),
}

#[derive(Subcommand)]
enum ProfilesAction {
    List,
    Remove {
        uuid: String,
    },
    /// Remove every expired profile
    Clean,
}

#[derive(Subcommand)]
enum FilesAction {
    /// List a folder (default: the app's top folder)
    Ls {
        bundle_id: String,
        path: Option<String>,
    },
    /// Copy a file or folder from the app to this computer
    Pull {
        bundle_id: String,
        remote: String,
        #[arg(long, default_value = ".")]
        to: PathBuf,
    },
    /// Copy a file or folder into the app (default: its Documents folder)
    Push {
        bundle_id: String,
        local: PathBuf,
        #[arg(long)]
        to: Option<String>,
    },
}

pub fn print_signature(info: &AppInfo) {
    match &info.signature {
        None => println!("Signature:   unsigned"),
        Some(sig) => {
            let kind = if sig.has_certificate {
                "certificate"
            } else {
                "ad-hoc (pseudo-signed)"
            };
            println!(
                "Signature:   {kind}{}",
                sig.team_id
                    .as_deref()
                    .map(|t| format!(", team {t}"))
                    .unwrap_or_default()
            );
            let keys: Vec<String> = sig
                .entitlements
                .as_ref()
                .map(|e| e.keys().cloned().collect())
                .unwrap_or_default();
            println!(
                "Entitlements: {}",
                if keys.is_empty() {
                    "none".into()
                } else {
                    keys.join(", ")
                }
            );
        }
    }
    if let Some(p) = &info.profile {
        print_profile_line("Profile:    ", p);
        println!(
            "             {} device(s) provisioned",
            p.provisioned_devices.len()
        );
    }
}

fn print_profile_line(prefix: &str, p: &ProfileInfo) {
    let now = chrono::Utc::now();
    let expiry = match p.expires {
        Some(e) if e <= now => "EXPIRED".to_string(),
        Some(e) => format!("expires {}", e.format("%Y-%m-%d")),
        None => "no expiry".into(),
    };
    println!(
        "{prefix} {} ({}), team {}, {expiry}",
        p.name,
        p.bundle_id().unwrap_or("?"),
        p.team_name
            .as_deref()
            .or(p.team_id.as_deref())
            .unwrap_or("?")
    );
}

pub async fn device(ctx: &Ctx, cmd: DeviceCmd) -> Result<()> {
    let device = resolve_device(ctx, &cmd.target).await?;
    let provider = device.connect(&ctx.paths).await?;
    match cmd.action {
        DeviceAction::Info => {
            let d = tools::details(&provider).await?;
            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&d)?);
                return Ok(());
            }
            println!("Name:           {}", d.name);
            println!(
                "Model:          {}",
                d.model_name
                    .clone()
                    .unwrap_or_else(|| d.product_type.clone())
            );
            println!(
                "iOS:            {} ({})",
                d.ios_version,
                d.build.as_deref().unwrap_or("?")
            );
            println!("UDID:           {}", d.udid);
            if let Some(s) = &d.serial {
                println!("Serial:         {s}");
            }
            if let Some(b) = d.battery_percent {
                let charging = if d.charging == Some(true) {
                    ", charging"
                } else {
                    ""
                };
                println!("Battery:        {b}%{charging}");
            }
            if let (Some(total), Some(free)) = (d.storage_total, d.storage_free) {
                println!(
                    "Storage:        {} free of {}",
                    format_bytes(free),
                    format_bytes(total)
                );
            }
            match d.developer_mode {
                Some(true) => println!("Developer Mode: on"),
                Some(false) => println!(
                    "Developer Mode: OFF - sideloaded apps won't open. Turn it on in Settings > Privacy & Security."
                ),
                None => {}
            }
            println!("Connection:     {}", device.transport.label());
        }
        DeviceAction::Apps { system } => {
            let apps = tools::list_apps(&provider, system).await?;
            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&apps)?);
                return Ok(());
            }
            println!(
                "{:<26} {:<44} {:<12} SIDELOADED",
                "NAME", "BUNDLE ID", "VERSION"
            );
            for a in apps {
                println!(
                    "{:<26} {:<44} {:<12} {}",
                    truncate(&a.name, 26),
                    truncate(&a.bundle_id, 44),
                    truncate(a.version.as_deref().unwrap_or("?"), 12),
                    if a.sideloaded { "yes" } else { "" }
                );
            }
        }
        DeviceAction::Uninstall { bundle_id } => {
            tools::uninstall_app(&provider, &bundle_id).await?;
            println!("Uninstalled {bundle_id} from {}", device.name);
            // Stop auto-refreshing what isn't installed any more
            let key = format!("{bundle_id}@{}", device.udid);
            if registry::remove(&ctx.paths, &key)? > 0 {
                println!("Also removed it from `sideward apps`.");
            }
        }
        DeviceAction::Restart => {
            tools::power(&provider, Power::Restart).await?;
            println!("Restarting {}", device.name);
        }
        DeviceAction::Shutdown => {
            tools::power(&provider, Power::Shutdown).await?;
            println!("Shutting down {}", device.name);
        }
        DeviceAction::Sleep => {
            tools::power(&provider, Power::Sleep).await?;
            println!("{} is going to sleep", device.name);
        }
        DeviceAction::Syslog { filter } => {
            let mut log = tools::Syslog::open(&provider).await?;
            eprintln!(
                "Streaming the log from {}. Press Ctrl+C to stop.",
                device.name
            );
            loop {
                tokio::select! {
                    line = log.next_line() => {
                        let line = line?;
                        if tools::log_matches(&line, &filter) {
                            println!("{}", line.trim_end());
                        }
                    }
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                }
            }
        }
        DeviceAction::Crashes { app, save } => {
            let all = tools::crash_reports(&provider).await?;
            let names: Vec<&String> = match &app {
                Some(a) => tools::reports_for(&all, a),
                None => all.iter().collect(),
            };
            if names.is_empty() {
                println!("No crash reports.");
                return Ok(());
            }
            for n in &names {
                println!("{n}");
            }
            if let Some(dir) = save {
                std::fs::create_dir_all(&dir)?;
                for n in &names {
                    let data = tools::pull_crash_report(&provider, n).await?;
                    std::fs::write(dir.join(n.as_str()), data)?;
                }
                println!("Saved {} report(s) to {}", names.len(), dir.display());
            }
        }
        DeviceAction::Profiles(action) => match action {
            ProfilesAction::List => {
                let profiles = tools::installed_profiles(&provider).await?;
                if ctx.json {
                    println!("{}", serde_json::to_string_pretty(&profiles)?);
                    return Ok(());
                }
                if profiles.is_empty() {
                    println!("No provisioning profiles installed.");
                }
                for p in profiles {
                    print_profile_line(&p.uuid, &p);
                }
            }
            ProfilesAction::Remove { uuid } => {
                tools::remove_profile(&provider, &uuid).await?;
                println!("Removed profile {uuid}");
            }
            ProfilesAction::Clean => {
                let removed = tools::remove_expired_profiles(&provider).await?;
                println!("Removed {} expired profile(s)", removed.len());
            }
        },
        DeviceAction::Files(action) => match action {
            FilesAction::Ls { bundle_id, path } => {
                for f in tools::list_app_files(&provider, &bundle_id, path.as_deref()).await? {
                    if f.is_dir {
                        println!("{:>10}  {}/", "", f.path);
                    } else {
                        println!("{:>10}  {}", format_bytes(f.size), f.path);
                    }
                }
            }
            FilesAction::Pull {
                bundle_id,
                remote,
                to,
            } => {
                let n = tools::pull_app_files(&provider, &bundle_id, &remote, &to).await?;
                println!("Copied {n} file(s) to {}", to.display());
            }
            FilesAction::Push {
                bundle_id,
                local,
                to,
            } => {
                let n = tools::push_app_files(&provider, &bundle_id, &local, to.as_deref()).await?;
                println!("Copied {n} file(s) into {bundle_id}");
            }
        },
    }
    Ok(())
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once per run
pub enum SourceCommand {
    /// Add an AltStore/SideStore source by URL
    Add {
        url: String,
    },
    Remove {
        url: String,
    },
    /// Your sources
    List,
    /// Apps in your sources
    Apps {
        /// Only apps whose name or bundle id contains this
        #[arg(long)]
        search: Option<String>,
    },
    /// Download an app from your sources and install it
    Install {
        /// App name or bundle id
        app: String,
        #[command(flatten)]
        target: DeviceArgs,
        #[command(flatten)]
        options: AppOptionArgs,
        #[command(flatten)]
        account: AccountArgs,
    },
    /// Installed apps that have a newer version in your sources
    Updates {
        /// Install the updates on reachable devices
        #[arg(long)]
        install: bool,
        #[command(flatten)]
        account: AccountArgs,
    },
}

async fn fetch_sources(ctx: &Ctx) -> Result<Vec<sources::Source>> {
    if ctx.config.sources.is_empty() {
        bail!("No sources yet. Add one with `sideward source add <url>`.");
    }
    let (ok, failed) = sources::fetch_all(&ctx.config.sources).await;
    for (url, e) in failed {
        eprintln!("Couldn't load {url}: {e:#}");
    }
    Ok(ok)
}

pub async fn source(ctx: &mut Ctx, cmd: SourceCommand) -> Result<()> {
    match cmd {
        SourceCommand::Add { url } => {
            let source = sources::fetch(&url).await?;
            if !ctx.config.sources.contains(&url) {
                ctx.config.sources.push(url.clone());
                ctx.config.save(&ctx.paths)?;
            }
            println!("Added {} ({} apps)", source.name, source.apps.len());
        }
        SourceCommand::Remove { url } => {
            let before = ctx.config.sources.len();
            ctx.config.sources.retain(|u| u != &url);
            ctx.config.save(&ctx.paths)?;
            println!("Removed {} source(s)", before - ctx.config.sources.len());
        }
        SourceCommand::List => {
            if ctx.config.sources.is_empty() {
                println!("No sources yet. Add one with `sideward source add <url>`.");
            }
            let (ok, failed) = sources::fetch_all(&ctx.config.sources).await;
            for s in ok {
                println!(
                    "{:<30} {:>4} apps  {}",
                    truncate(&s.name, 30),
                    s.apps.len(),
                    s.url
                );
            }
            for (url, e) in failed {
                println!("{:<30} unreachable  {url} ({e:#})", "?");
            }
        }
        SourceCommand::Apps { search } => {
            let all = fetch_sources(ctx).await?;
            let q = search.unwrap_or_default().to_lowercase();
            if ctx.json {
                println!("{}", serde_json::to_string_pretty(&all)?);
                return Ok(());
            }
            println!(
                "{:<26} {:<36} {:<12} SOURCE",
                "NAME", "BUNDLE ID", "VERSION"
            );
            for s in &all {
                for a in &s.apps {
                    if !q.is_empty()
                        && !a.name.to_lowercase().contains(&q)
                        && !a.bundle_identifier.to_lowercase().contains(&q)
                    {
                        continue;
                    }
                    println!(
                        "{:<26} {:<36} {:<12} {}",
                        truncate(&a.name, 26),
                        truncate(&a.bundle_identifier, 36),
                        a.latest().map(|v| v.version).unwrap_or_default(),
                        s.name
                    );
                }
            }
        }
        SourceCommand::Install {
            app,
            target,
            options,
            account,
        } => {
            let options = options.to_options()?;
            let all = fetch_sources(ctx).await?;
            let matches = sources::find(&all, &app);
            let (_, found) = match matches.as_slice() {
                [] => bail!(
                    "No app called \"{app}\" in your sources (try `sideward source apps --search`)"
                ),
                [one] => *one,
                many => bail!(
                    "\"{app}\" matches several apps; use the bundle id: {}",
                    many.iter()
                        .map(|(_, a)| a.bundle_identifier.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            let version = found
                .latest()
                .context("That app lists no downloadable version")?;
            let device = resolve_device(ctx, &target).await?;
            let mut session = login(ctx, &account, true).await?;
            let progress = ProgressUi::new();
            let ipa = sources::download(&ctx.paths, found, &version, progress.callback()).await;
            progress.finish();
            let ipa = ipa?;
            println!("Installing {} {} to {device}", found.name, version.version);
            let progress = ProgressUi::new();
            let request = InstallRequest {
                source: ipa,
                options,
                remember: true,
            };
            let outcome = session
                .install(&device, &request, progress.callback())
                .await;
            progress.finish();
            let outcome = outcome?;
            println!(
                "Installed {} ({})",
                outcome.app.name, outcome.installed_bundle_id
            );
            print_expiry(outcome.expires_at);
        }
        SourceCommand::Updates { install, account } => {
            let all = fetch_sources(ctx).await?;
            let installed = registry::list(&ctx.paths)?;
            let ups = sources::updates(&installed, &all);
            if ups.is_empty() {
                println!("Everything is up to date.");
                return Ok(());
            }
            for u in &ups {
                println!(
                    "{} on {}: {} -> {}",
                    u.installed.name,
                    u.installed.device_name,
                    u.installed.version.as_deref().unwrap_or("?"),
                    u.version.version
                );
            }
            if !install {
                println!("Run `sideward source updates --install` to install them.");
                return Ok(());
            }
            let devices = super::discover(ctx, &Default::default()).await?;
            let mut session = login(ctx, &account, true).await?;
            for u in ups {
                let Some(device) = devices.iter().find(|d| d.udid == u.installed.udid) else {
                    println!(
                        "Skipping {}: {} isn't connected",
                        u.installed.name, u.installed.device_name
                    );
                    continue;
                };
                let progress = ProgressUi::new();
                let ipa =
                    sources::download(&ctx.paths, &u.app, &u.version, progress.callback()).await;
                progress.finish();
                let request = InstallRequest {
                    source: ipa?,
                    options: u.installed.options.clone(),
                    remember: true,
                };
                let progress = ProgressUi::new();
                let result = session.install(device, &request, progress.callback()).await;
                progress.finish();
                match result {
                    Ok(o) => println!("Updated {} to {}", o.app.name, u.version.version),
                    Err(e) => eprintln!("Updating {} failed: {e:#}", u.installed.name),
                }
            }
        }
    }
    Ok(())
}
