//! Background thread that owns the tokio runtime and the logged-in session. The UI sends
//! [`Cmd`]s and receives [`Event`]s, so the window never blocks on the network or a device.

use std::{
    collections::HashMap,
    net::IpAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use async_trait::async_trait;
use isideload::{
    auth::apple_account::{TwoFactorCallbackParams, TwoFactorCallbackResponse},
    dev::{certificates::DevelopmentCertificate, teams::DeveloperTeam},
};
use sideward_core::{
    Config, Paths, Prompter, Session,
    account::{self, LoginRequest},
    credentials,
    device::{self, Device, DiscoverOptions},
    install::{self, InstallRequest, Progress, ProgressFn, RefreshFilter},
    ipa::{self, AppInfo, AppOptions},
    registry::{self, InstalledApp},
    watch, wifi,
};

/// Log in again after this long; Apple's developer session tokens expire.
const SESSION_MAX_AGE: Duration = Duration::from_secs(30 * 60);
/// After an automatic refresh fails on a device, leave it alone for this long.
const FAILURE_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// How often to re-check devices that stay connected, and to look for Wi-Fi devices.
const PERIODIC_CHECK: Duration = Duration::from_secs(60 * 60);
const WIFI_SCAN: Duration = Duration::from_secs(5 * 60);
use tokio::sync::{mpsc, oneshot};

pub enum Cmd {
    Login {
        apple_id: String,
        /// Empty means "use the saved password".
        password: String,
    },
    /// Forget the saved password and the session.
    SignOut,
    /// A device just connected over USB or appeared on Wi-Fi (from the watcher).
    DeviceConnected(Box<Device>),
    /// Hourly: re-sign due apps on devices that stayed connected.
    PeriodicCheck,
    SetAppAutoRefresh {
        key: String,
        enabled: bool,
    },
    Discover,
    SetupWifi {
        udid: String,
    },
    ConnectIp {
        ip: IpAddr,
    },
    Inspect(PathBuf),
    Install {
        device: Box<Device>,
        request: InstallRequest,
    },
    Export {
        source: PathBuf,
        options: AppOptions,
        out: PathBuf,
    },
    Refresh {
        force: bool,
    },
    ForgetApp(String),
    SaveConfig(Config),
    Pick(PickKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    App,
    Tweak,
    ExportTarget,
}

pub enum Prompt {
    TwoFactor {
        params: TwoFactorCallbackParams,
        reply: oneshot::Sender<TwoFactorCallbackResponse>,
    },
    Team {
        teams: Vec<DeveloperTeam>,
        reply: oneshot::Sender<Option<String>>,
    },
    Certificates {
        certs: Vec<DevelopmentCertificate>,
        reply: oneshot::Sender<Option<Vec<String>>>,
    },
}

pub enum Event {
    Log(String),
    Error(String),
    Config {
        config: Config,
        has_saved_password: bool,
    },
    LoggedIn {
        apple_id: String,
        team: String,
        server: String,
    },
    SignedOut,
    /// Something needs the user (e.g. the saved password stopped working): show the window.
    Attention,
    Devices(Vec<Device>),
    AppInfo(PathBuf, Result<AppInfo, String>),
    Progress(Progress),
    Busy(Option<String>),
    Apps(Vec<InstalledApp>),
    Prompt(Prompt),
    Picked(PickKind, Option<PathBuf>),
}

#[derive(Clone)]
struct Outbox {
    tx: std::sync::mpsc::Sender<Event>,
    ctx: eframe::egui::Context,
}

impl Outbox {
    fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        self.ctx.request_repaint();
    }

    fn error(&self, e: anyhow::Error) {
        self.send(Event::Error(format!("{e:#}")));
    }
}

struct GuiPrompter {
    out: Outbox,
}

#[async_trait]
impl Prompter for GuiPrompter {
    async fn two_factor(
        &self,
        params: TwoFactorCallbackParams,
    ) -> Result<TwoFactorCallbackResponse> {
        let (reply, rx) = oneshot::channel();
        self.out
            .send(Event::Prompt(Prompt::TwoFactor { params, reply }));
        Ok(rx.await.unwrap_or(TwoFactorCallbackResponse::Abort))
    }

    async fn select_team(&self, teams: &[DeveloperTeam]) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.out.send(Event::Prompt(Prompt::Team {
            teams: teams.to_vec(),
            reply,
        }));
        Ok(rx.await.unwrap_or(None))
    }

    async fn revoke_certificates(
        &self,
        certs: Vec<DevelopmentCertificate>,
    ) -> Result<Option<Vec<String>>> {
        let (reply, rx) = oneshot::channel();
        self.out
            .send(Event::Prompt(Prompt::Certificates { certs, reply }));
        Ok(rx.await.unwrap_or(None))
    }
}

pub fn spawn(
    ctx: eframe::egui::Context,
) -> Result<(mpsc::UnboundedSender<Cmd>, std::sync::mpsc::Receiver<Event>)> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel();
    let out = Outbox { tx: evt_tx, ctx };
    let paths = Paths::new(None)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let worker = Worker::new(paths, out, cmd_tx.clone());
    std::thread::Builder::new()
        .name("sideward-worker".into())
        .spawn(move || runtime.block_on(worker.run(cmd_rx)))?;
    Ok((cmd_tx, evt_rx))
}

struct Worker {
    paths: Paths,
    config: Config,
    session: Option<Session>,
    out: Outbox,
    /// For background tasks (device watcher, periodic check) to queue work.
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    failed_at: HashMap<String, Instant>,
}

impl Worker {
    fn new(paths: Paths, out: Outbox, cmd_tx: mpsc::UnboundedSender<Cmd>) -> Self {
        let config = Config::load(&paths).unwrap_or_default();
        Self {
            paths,
            config,
            session: None,
            out,
            cmd_tx,
            failed_at: HashMap::new(),
        }
    }

    /// Feed connect events and an hourly tick into the command queue, so automatic refreshes
    /// run one at a time with everything else.
    fn start_background_tasks(&self) {
        let (tx, mut connected) = mpsc::unbounded_channel();
        watch::spawn(self.paths.clone(), WIFI_SCAN, tx);
        let cmd = self.cmd_tx.clone();
        tokio::spawn(async move {
            while let Some(device) = connected.recv().await {
                if cmd.send(Cmd::DeviceConnected(Box::new(device))).is_err() {
                    break;
                }
            }
        });
        let cmd = self.cmd_tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PERIODIC_CHECK);
            tick.tick().await;
            loop {
                tick.tick().await;
                if cmd.send(Cmd::PeriodicCheck).is_err() {
                    break;
                }
            }
        });
    }

    /// Stay signed in: log in with the saved password at startup.
    async fn sign_in_saved(&mut self) {
        if !self.config.stay_signed_in || self.config.apple_id.is_none() {
            return;
        }
        self.out.send(Event::Busy(Some("Signing in".into())));
        if let Err(e) = self.login_saved().await {
            self.login_failed(e);
        }
        self.out.send(Event::Busy(None));
    }

    async fn login_saved(&mut self) -> Result<bool> {
        let prompter = Arc::new(GuiPrompter {
            out: self.out.clone(),
        });
        match account::login_saved(&self.paths, &mut self.config, prompter).await? {
            Some(session) => {
                self.set_session(session);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn login_failed(&mut self, e: anyhow::Error) {
        if e.is::<account::SavedPasswordRejected>() {
            self.out.send(Event::SignedOut);
            self.out.send(Event::Attention);
            self.send_config();
        }
        self.out.error(e);
    }

    fn set_session(&mut self, session: Session) {
        self.out.send(Event::LoggedIn {
            apple_id: session.apple_id.clone(),
            team: format!(
                "{} ({})",
                session.team.name.as_deref().unwrap_or("Team"),
                session.team.team_id
            ),
            server: session.anisette_server.clone(),
        });
        self.session = Some(session);
        self.send_config();
    }

    fn progress(&self) -> ProgressFn {
        let out = self.out.clone();
        Arc::new(move |p| out.send(Event::Progress(p)))
    }

    fn send_config(&self) {
        let has_saved_password = self
            .config
            .apple_id
            .as_deref()
            .and_then(|id| credentials::get_password(id).ok().flatten())
            .is_some();
        self.out.send(Event::Config {
            config: self.config.clone(),
            has_saved_password,
        });
    }

    fn send_apps(&self) {
        match registry::list(&self.paths) {
            Ok(apps) => self.out.send(Event::Apps(apps)),
            Err(e) => self.out.error(e),
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        self.send_config();
        self.send_apps();
        self.sign_in_saved().await;
        self.discover().await;
        self.start_background_tasks();
        while let Some(cmd) = rx.recv().await {
            if let Cmd::Pick(kind) = cmd {
                // File dialogs don't block other work
                let out = self.out.clone();
                tokio::spawn(async move { out.send(Event::Picked(kind, pick(kind).await)) });
                continue;
            }
            let label = match &cmd {
                Cmd::Login { .. } => Some("Signing in"),
                Cmd::Install { .. } => Some("Installing"),
                Cmd::Export { .. } => Some("Exporting"),
                Cmd::Refresh { .. } => Some("Refreshing apps"),
                Cmd::SetupWifi { .. } => Some("Setting up Wi-Fi"),
                Cmd::ConnectIp { .. } => Some("Connecting"),
                _ => None,
            };
            if let Some(label) = label {
                self.out.send(Event::Busy(Some(label.into())));
            }
            if let Err(e) = self.handle(cmd).await {
                if e.is::<account::SavedPasswordRejected>() {
                    self.login_failed(e);
                } else {
                    self.out.error(e);
                }
            }
            if label.is_some() {
                self.out.send(Event::Busy(None));
            }
        }
    }

    async fn discover(&mut self) {
        match device::discover(&self.paths, &DiscoverOptions::default()).await {
            Ok(devices) => self.out.send(Event::Devices(devices)),
            Err(e) => self.out.error(e),
        }
    }

    /// A logged-in session, signing in again with the saved password when there's none or
    /// it's getting old.
    async fn ensure_session(&mut self) -> Result<&mut Session> {
        if self
            .session
            .as_ref()
            .is_none_or(|s| s.age() > SESSION_MAX_AGE)
        {
            self.session = None;
            match self.login_saved().await {
                Ok(true) => {}
                Ok(false) => anyhow::bail!("Sign in with your Apple ID first"),
                Err(e) => {
                    if e.is::<account::SavedPasswordRejected>() {
                        self.out.send(Event::SignedOut);
                        self.out.send(Event::Attention);
                    }
                    return Err(e);
                }
            }
        }
        Ok(self.session.as_mut().expect("logged in"))
    }

    async fn login(&mut self, apple_id: String, password: String) -> Result<()> {
        let saved = if password.is_empty() {
            Some(
                credentials::get_password(&apple_id)?
                    .ok_or_else(|| anyhow::anyhow!("Enter your Apple ID password"))?,
            )
        } else {
            None
        };
        let from_keychain = saved.is_some();
        let prompter = Arc::new(GuiPrompter {
            out: self.out.clone(),
        });
        let remember = self.config.stay_signed_in;
        let session = account::login_and_remember(
            &self.paths,
            &mut self.config,
            LoginRequest {
                apple_id,
                password: saved.unwrap_or(password),
                anisette_server: None,
            },
            from_keychain,
            remember,
            prompter,
        )
        .await?;
        self.set_session(session);
        Ok(())
    }

    /// Re-sign due apps (AutoRefresh on) on `devices`. Quiet when there's nothing to do.
    async fn auto_refresh(&mut self, devices: Vec<Device>) -> Result<()> {
        if !self.config.auto_refresh_on_connect {
            return Ok(());
        }
        let devices: Vec<Device> = devices
            .into_iter()
            .filter(|d| {
                self.failed_at
                    .get(&d.udid)
                    .is_none_or(|t| t.elapsed() > FAILURE_BACKOFF)
            })
            .collect();
        let filter = RefreshFilter::Auto(self.config.refresh_policy());
        let apple_id = self.config.apple_id.clone();
        let pending =
            install::pending_refreshes(&self.paths, apple_id.as_deref(), &devices, filter)?;
        if pending.is_empty() {
            return Ok(());
        }
        let names: Vec<&str> = pending.iter().map(|a| a.name.as_str()).collect();
        self.out.send(Event::Busy(Some(format!(
            "AutoRefresh: re-signing {}",
            names.join(", ")
        ))));
        let result = self.run_refresh(&devices, filter).await;
        self.out.send(Event::Busy(None));
        if let Ok(failed) = &result {
            for udid in failed {
                self.failed_at.insert(udid.clone(), Instant::now());
            }
        }
        result.map(|_| ())
    }

    /// Returns the UDIDs where something failed.
    async fn run_refresh(
        &mut self,
        devices: &[Device],
        filter: RefreshFilter,
    ) -> Result<Vec<String>> {
        let progress = self.progress();
        let session = self.ensure_session().await?;
        let results = install::refresh(session, devices, filter, progress).await?;
        let mut failed = Vec::new();
        for r in &results {
            match &r.result {
                Ok(o) => {
                    let mut msg = format!("Re-signed {} on {}.", r.app.name, r.app.device_name);
                    if let Some(exp) = o.expires_at {
                        let local: chrono::DateTime<chrono::Local> = exp.into();
                        msg.push_str(&format!(" Good until {}.", local.format("%a %b %e %H:%M")));
                    }
                    self.out.send(Event::Log(msg));
                }
                Err(e) => {
                    failed.push(r.app.udid.clone());
                    self.out.send(Event::Error(format!(
                        "Re-signing {} failed: {e:#}",
                        r.app.name
                    )));
                }
            }
        }
        if !failed.is_empty() {
            // A failure may be an expired session; sign in fresh next time
            self.session = None;
        }
        self.send_apps();
        Ok(failed)
    }

    async fn handle(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Login { apple_id, password } => {
                self.session = None;
                self.login(apple_id, password).await?;
            }
            Cmd::SignOut => {
                if let Some(apple_id) = &self.config.apple_id {
                    credentials::delete_password(apple_id)?;
                }
                self.session = None;
                self.out.send(Event::SignedOut);
                self.send_config();
            }
            Cmd::DeviceConnected(device) => {
                self.out
                    .send(Event::Log(format!("{} connected", device.name)));
                self.discover().await;
                self.auto_refresh(vec![*device]).await?;
            }
            Cmd::PeriodicCheck => {
                let devices = device::discover(&self.paths, &DiscoverOptions::default()).await?;
                self.out.send(Event::Devices(devices.clone()));
                self.auto_refresh(devices).await?;
            }
            Cmd::SetAppAutoRefresh { key, enabled } => {
                registry::set_auto_refresh(&self.paths, &key, enabled)?;
                self.send_apps();
            }
            Cmd::Discover => self.discover().await,
            Cmd::SetupWifi { udid } => {
                let saved = wifi::setup_over_usb(&self.paths, Some(&udid)).await?;
                self.out.send(Event::Log(format!(
                    "Wi-Fi is set up for {}. You can unplug it; keep it on the same network.",
                    saved.name
                )));
                self.discover().await;
            }
            Cmd::ConnectIp { ip } => {
                let mut found = None;
                for saved in wifi::list_saved(&self.paths)? {
                    if let Ok(pairing) = wifi::load_pairing(&self.paths, &saved.udid)
                        && let Ok(dev) = device::direct_device(&self.paths, ip, pairing).await
                    {
                        found = Some(dev);
                        break;
                    }
                }
                let dev = found.ok_or_else(|| {
                    anyhow::anyhow!(
                        "None of the saved pairings work with {ip}. Set up Wi-Fi over USB first."
                    )
                })?;
                self.out.send(Event::Log(format!("Connected to {dev}")));
                self.discover().await;
            }
            Cmd::Inspect(path) => {
                let p = path.clone();
                let info = tokio::task::spawn_blocking(move || ipa::inspect(&p)).await?;
                self.out
                    .send(Event::AppInfo(path, info.map_err(|e| format!("{e:#}"))));
            }
            Cmd::Install { device, request } => {
                let progress = self.progress();
                let session = self.ensure_session().await?;
                let outcome = session.install(&device, &request, progress).await?;
                let mut msg = format!("Installed {} on {}.", outcome.app.name, device.name);
                if let Some(exp) = outcome.expires_at {
                    let local: chrono::DateTime<chrono::Local> = exp.into();
                    msg.push_str(&format!(" Expires {}.", local.format("%a %b %e %H:%M")));
                }
                self.out.send(Event::Log(msg));
                self.send_apps();
            }
            Cmd::Export {
                source,
                options,
                out,
            } => {
                let progress = self.progress();
                let session = self.ensure_session().await?;
                let outcome = session.export(&source, &options, &out, progress).await?;
                self.out.send(Event::Log(format!(
                    "Saved signed {} to {}",
                    outcome.app.name,
                    out.display()
                )));
            }
            Cmd::Refresh { force } => {
                let devices = device::discover(&self.paths, &DiscoverOptions::default()).await?;
                self.out.send(Event::Devices(devices.clone()));
                let filter = if force {
                    RefreshFilter::All
                } else {
                    RefreshFilter::ExpiringWithin(chrono::Duration::days(
                        self.config.refresh_policy().days_left,
                    ))
                };
                let apple_id = self.config.apple_id.clone();
                if install::pending_refreshes(&self.paths, apple_id.as_deref(), &devices, filter)?
                    .is_empty()
                {
                    self.out.send(Event::Log(
                        "Nothing to re-sign (apps not due, or their devices aren't reachable)."
                            .into(),
                    ));
                    return Ok(());
                }
                self.run_refresh(&devices, filter).await?;
            }
            Cmd::ForgetApp(key) => {
                registry::remove(&self.paths, &key)?;
                self.send_apps();
            }
            Cmd::SaveConfig(config) => {
                // A different anisette server needs a fresh login
                if config.anisette_server != self.config.anisette_server {
                    self.session = None;
                }
                if self.config.stay_signed_in
                    && !config.stay_signed_in
                    && let Some(apple_id) = &config.apple_id
                {
                    credentials::delete_password(apple_id)?;
                }
                self.config = config;
                self.config.save(&self.paths)?;
                self.send_config();
            }
            Cmd::Pick(_) => unreachable!("handled in run"),
        }
        Ok(())
    }
}

async fn pick(kind: PickKind) -> Option<PathBuf> {
    let dialog = rfd::AsyncFileDialog::new();
    let handle = match kind {
        PickKind::App => {
            dialog
                .add_filter("iOS app", &["ipa", "tipa"])
                .pick_file()
                .await
        }
        PickKind::Tweak => {
            dialog
                .add_filter("Tweak", &["dylib", "deb"])
                .pick_file()
                .await
        }
        PickKind::ExportTarget => {
            dialog
                .add_filter("iOS app", &["ipa"])
                .set_file_name("signed.ipa")
                .save_file()
                .await
        }
    };
    handle.map(|h| h.path().to_path_buf())
}
