use std::{
    net::IpAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
};

use eframe::egui::{self, Color32, RichText};
use isideload::auth::apple_account::{TwoFactorCallbackParams, TwoFactorCallbackResponse};
use sideward_core::{
    Config, anisette,
    device::{Device, TransportKind},
    install::{InstallRequest, Progress},
    ipa::{AppInfo, AppOptions},
    registry::InstalledApp,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::worker::{self, Cmd, Event, PickKind, Prompt};

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Install,
    Apps,
    Settings,
}

#[derive(Clone)]
enum LogLine {
    Info(String),
    Error(String),
}

/// A prompt from the worker plus the UI's in-progress answer.
enum ActivePrompt {
    TwoFactor {
        params: TwoFactorCallbackParams,
        code: String,
        reply: Option<tokio::sync::oneshot::Sender<TwoFactorCallbackResponse>>,
    },
    Team {
        teams: Vec<isideload::dev::teams::DeveloperTeam>,
        selected: usize,
        reply: Option<tokio::sync::oneshot::Sender<Option<String>>>,
    },
    Certificates {
        certs: Vec<isideload::dev::certificates::DevelopmentCertificate>,
        checked: Vec<bool>,
        reply: Option<tokio::sync::oneshot::Sender<Option<Vec<String>>>>,
    },
}

#[derive(Default)]
struct OptionsForm {
    name: String,
    bundle_id: String,
    version: String,
    min_ios: String,
    file_sharing: bool,
    remove_supported_devices: bool,
    remove_extensions: bool,
    remove_watch_app: bool,
    inject: Vec<PathBuf>,
    register_extensions: bool,
    increased_memory_limit: bool,
}

impl OptionsForm {
    fn to_options(&self) -> AppOptions {
        let opt = |s: &str| {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        };
        AppOptions {
            name: opt(&self.name),
            bundle_id: opt(&self.bundle_id),
            version: opt(&self.version),
            min_ios: opt(&self.min_ios),
            file_sharing: self.file_sharing,
            remove_supported_devices: self.remove_supported_devices,
            remove_extensions: self.remove_extensions,
            remove_watch_app: self.remove_watch_app,
            inject: self.inject.clone(),
            register_extensions: self.register_extensions,
            increased_memory_limit: self.increased_memory_limit,
        }
    }

    fn changed_count(&self) -> usize {
        let o = self.to_options();
        [
            o.name.is_some(),
            o.bundle_id.is_some(),
            o.version.is_some(),
            o.min_ios.is_some(),
            o.file_sharing,
            o.remove_supported_devices,
            o.remove_extensions,
            o.remove_watch_app,
            !o.inject.is_empty(),
            o.register_extensions,
            o.increased_memory_limit,
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }
}

/// Bring the window back from the tray, the taskbar, or behind other windows.
pub fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}

/// How the app was started.
pub struct Launch {
    /// Started by the login item: stay out of the way.
    pub background: bool,
    /// Lets later launches (e.g. double-clicking the exe) bring this window up.
    pub instance_listener: Option<std::net::TcpListener>,
}

pub struct SidewardApp {
    cmd: UnboundedSender<Cmd>,
    events: Receiver<Event>,
    tab: Tab,

    config: Config,
    apple_id: String,
    password: String,
    has_saved_password: bool,
    logged_in: Option<(String, String, String)>,

    devices: Vec<Device>,
    selected_device: Option<String>,
    ip_input: String,

    app_path: Option<PathBuf>,
    app_info: Option<Result<AppInfo, String>>,
    options: OptionsForm,
    remember: bool,

    apps: Vec<InstalledApp>,

    busy: Option<String>,
    step: String,
    fraction: f32,
    log: Vec<LogLine>,
    prompt: Option<ActivePrompt>,

    /// Actual OS login-item state (the config value is only what the user last chose).
    launch_at_login: bool,
    /// Set by the tray's Quit so closing really quits.
    quitting: Arc<AtomicBool>,
    #[cfg(any(windows, target_os = "macos"))]
    tray: Option<tray_icon::TrayIcon>,
    minimize_on_first_frame: bool,
    told_about_background: bool,
}

impl SidewardApp {
    pub fn new(cc: &eframe::CreationContext<'_>, launch: Launch) -> anyhow::Result<Self> {
        let ctx = cc.egui_ctx.clone();
        let (cmd, events) = worker::spawn(ctx.clone())?;
        if let Some(listener) = launch.instance_listener {
            let show_ctx = ctx.clone();
            crate::system::listen_for_launches(listener, move || show_window(&show_ctx));
        }
        let quitting = Arc::new(AtomicBool::new(false));
        #[cfg(any(windows, target_os = "macos"))]
        let tray = match crate::tray::create(&ctx, cmd.clone(), quitting.clone()) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("No tray icon: {e:#}");
                None
            }
        };
        let mut app = Self {
            cmd,
            events,
            tab: Tab::Install,
            config: Config::default(),
            apple_id: String::new(),
            password: String::new(),
            has_saved_password: false,
            logged_in: None,
            devices: Vec::new(),
            selected_device: None,
            ip_input: String::new(),
            app_path: None,
            app_info: None,
            options: OptionsForm::default(),
            remember: true,
            apps: Vec::new(),
            busy: None,
            step: String::new(),
            fraction: 0.0,
            log: Vec::new(),
            prompt: None,
            launch_at_login: crate::system::launch_at_login_enabled(),
            quitting,
            #[cfg(any(windows, target_os = "macos"))]
            tray,
            minimize_on_first_frame: false,
            told_about_background: false,
        };
        // Started at login: hidden in the tray, or minimized where there's no tray
        if launch.background {
            if app.has_tray() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                app.minimize_on_first_frame = true;
            }
        }
        // `sideward-gui App.ipa` (or opening an .ipa with it) preselects the app
        if let Some(path) = std::env::args_os()
            .skip(1)
            .find(|a| a != crate::system::BACKGROUND_ARG)
            .map(PathBuf::from)
        {
            app.set_app(path);
        }
        Ok(app)
    }

    fn has_tray(&self) -> bool {
        #[cfg(any(windows, target_os = "macos"))]
        return self.tray.is_some();
        #[cfg(not(any(windows, target_os = "macos")))]
        false
    }

    /// Closing the window keeps Sideward running (to re-sign apps when devices connect)
    /// unless that's turned off or the user chose Quit from the tray.
    fn handle_close(&mut self, ctx: &egui::Context) {
        // Without a tray (Linux) there'd be no way back, so closing quits there
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if !close_requested
            || !self.has_tray()
            || !self.config.run_in_background
            || self.quitting.load(Ordering::Relaxed)
        {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        if !self.told_about_background {
            self.told_about_background = true;
            self.log.push(LogLine::Info(
                "Sideward keeps running in the tray so it can re-sign apps when devices connect. Quit from the tray icon, or turn this off in Settings.".into(),
            ));
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmd.send(cmd);
    }

    fn device_key(d: &Device) -> String {
        format!("{}|{:?}", d.udid, d.transport)
    }

    fn selected_device(&self) -> Option<&Device> {
        let key = self.selected_device.as_ref()?;
        self.devices.iter().find(|d| &Self::device_key(d) == key)
    }

    fn set_app(&mut self, path: PathBuf) {
        self.app_info = None;
        self.send(Cmd::Inspect(path.clone()));
        self.app_path = Some(path);
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Attention => show_window(ctx),
                Event::SignedOut => {
                    self.logged_in = None;
                    self.has_saved_password = false;
                }
                Event::Log(m) => self.log.push(LogLine::Info(m)),
                Event::Error(m) => self.log.push(LogLine::Error(m)),
                Event::Config {
                    config,
                    has_saved_password,
                } => {
                    if self.apple_id.is_empty() {
                        self.apple_id = config.apple_id.clone().unwrap_or_default();
                    }
                    self.has_saved_password = has_saved_password;
                    self.config = config;
                }
                Event::LoggedIn {
                    apple_id,
                    team,
                    server,
                } => {
                    self.log
                        .push(LogLine::Info(format!("Logged in as {apple_id}")));
                    self.password.clear();
                    self.logged_in = Some((apple_id, team, server));
                }
                Event::Devices(devices) => {
                    let still_there = self
                        .selected_device
                        .as_ref()
                        .is_some_and(|k| devices.iter().any(|d| &Self::device_key(d) == k));
                    if !still_there {
                        self.selected_device = devices.first().map(Self::device_key);
                    }
                    self.devices = devices;
                }
                Event::AppInfo(path, info) => {
                    if self.app_path.as_ref() == Some(&path) {
                        self.app_info = Some(info);
                    }
                }
                Event::Progress(p) => match p {
                    Progress::Step(s) => {
                        self.step = s;
                        self.fraction = 0.0;
                    }
                    Progress::Fraction(f) => self.fraction = f,
                    Progress::Warning(w) => self.log.push(LogLine::Error(format!("Warning: {w}"))),
                },
                Event::Busy(b) => {
                    if b.is_none() {
                        self.step.clear();
                        self.fraction = 0.0;
                    }
                    self.busy = b;
                }
                Event::Apps(apps) => self.apps = apps,
                Event::Prompt(p) => {
                    // 2FA etc. may arrive while Sideward is in the tray
                    show_window(ctx);
                    self.prompt = Some(match p {
                        Prompt::TwoFactor { params, reply } => ActivePrompt::TwoFactor {
                            params,
                            code: String::new(),
                            reply: Some(reply),
                        },
                        Prompt::Team { teams, reply } => ActivePrompt::Team {
                            teams,
                            selected: 0,
                            reply: Some(reply),
                        },
                        Prompt::Certificates { certs, reply } => {
                            let checked = vec![false; certs.len()];
                            ActivePrompt::Certificates {
                                certs,
                                checked,
                                reply: Some(reply),
                            }
                        }
                    })
                }
                Event::Picked(kind, path) => {
                    let Some(path) = path else { continue };
                    match kind {
                        PickKind::App => self.set_app(path),
                        PickKind::Tweak => {
                            if !self.options.inject.contains(&path) {
                                self.options.inject.push(path);
                            }
                        }
                        PickKind::ExportTarget => {
                            if let Some(source) = self.app_path.clone() {
                                self.send(Cmd::Export {
                                    source,
                                    options: self.options.to_options(),
                                    out: path,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    fn account_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Apple ID");
        if let Some((apple_id, team, server)) = &self.logged_in {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Signed in as {apple_id}"))
                        .color(Color32::from_rgb(60, 170, 90)),
                );
                ui.weak(format!("{team} · anisette: {server}"));
            });
            let note = if self.config.stay_signed_in {
                "You stay signed in until your password changes or Apple asks again."
            } else {
                "Your password isn't saved; you'll sign in again next time."
            };
            ui.horizontal(|ui| {
                ui.weak(note);
                if ui.small_button("Sign out").clicked() {
                    self.send(Cmd::SignOut);
                }
            });
            return;
        }
        let busy = self.busy.is_some();
        egui::Grid::new("login")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Email");
                ui.add(
                    egui::TextEdit::singleline(&mut self.apple_id)
                        .hint_text("you@icloud.com")
                        .desired_width(260.0),
                );
                ui.end_row();
                ui.label("Password");
                let hint = if self.has_saved_password {
                    "saved in keychain"
                } else {
                    ""
                };
                ui.add(
                    egui::TextEdit::singleline(&mut self.password)
                        .password(true)
                        .hint_text(hint)
                        .desired_width(260.0),
                );
                ui.end_row();
            });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.config.stay_signed_in, "Stay signed in")
                .on_hover_text("Keeps your password in the system keychain until it changes or Apple asks you to sign in again. Needed for AutoRefresh.");
            let can_login = !busy
                && !self.apple_id.trim().is_empty()
                && (!self.password.is_empty() || self.has_saved_password);
            if ui
                .add_enabled(can_login, egui::Button::new("Sign in"))
                .clicked()
            {
                self.send(Cmd::SaveConfig(self.config.clone()));
                self.send(Cmd::Login {
                    apple_id: self.apple_id.trim().to_string(),
                    password: self.password.clone(),
                });
            }
        });
        ui.weak("Your password goes only to Apple. Anisette servers just supply the device data Apple's login needs.");
    }

    fn device_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Device");
            if ui
                .add_enabled(self.busy.is_none(), egui::Button::new("Refresh"))
                .clicked()
            {
                self.send(Cmd::Discover);
            }
        });
        if self.devices.is_empty() {
            ui.label("No devices found. Plug in over USB, unlock, and tap Trust.");
        } else {
            let selected_text = self
                .selected_device()
                .map(device_label)
                .unwrap_or_else(|| "Choose a device".into());
            egui::ComboBox::from_id_salt("device")
                .width(ui.available_width().min(560.0))
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for d in &self.devices {
                        let key = Self::device_key(d);
                        ui.selectable_value(&mut self.selected_device, Some(key), device_label(d));
                    }
                });
        }
        let selected = self.selected_device().cloned();
        ui.horizontal(|ui| {
            if let Some(d) = &selected
                && d.transport == TransportKind::Usb
                && ui
                    .add_enabled(self.busy.is_none(), egui::Button::new("Enable Wi-Fi installs"))
                    .on_hover_text("Saves this device's pairing so Sideward can install and refresh over Wi-Fi, no cable or iTunes needed")
                    .clicked()
            {
                self.send(Cmd::SetupWifi { udid: d.udid.clone() });
            }
            ui.add(egui::TextEdit::singleline(&mut self.ip_input).hint_text("Device IP").desired_width(120.0));
            let ip: Option<IpAddr> = self.ip_input.trim().parse().ok();
            if ui
                .add_enabled(ip.is_some() && self.busy.is_none(), egui::Button::new("Connect by IP"))
                .clicked()
                && let Some(ip) = ip
            {
                self.send(Cmd::ConnectIp { ip });
            }
        });
    }

    fn app_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("App");
        let frame = egui::Frame::group(ui.style()).inner_margin(12.0);
        frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                if ui.button("Choose IPA…").clicked() {
                    self.send(Cmd::Pick(PickKind::App));
                }
                match &self.app_path {
                    Some(p) => ui.label(p.file_name().unwrap_or_default().to_string_lossy()),
                    None => ui.weak("or drop an .ipa onto this window"),
                };
            });
            match &self.app_info {
                Some(Ok(info)) => {
                    ui.label(format!(
                        "{}  ·  {}  ·  v{}  ·  iOS {}+",
                        info.name,
                        info.bundle_id,
                        info.version.as_deref().unwrap_or("?"),
                        info.min_ios.as_deref().unwrap_or("?")
                    ));
                    if !info.extensions.is_empty() {
                        ui.weak(format!(
                            "{} extension(s): {}",
                            info.extensions.len(),
                            info.extensions.join(", ")
                        ));
                    }
                    if info.encrypted {
                        ui.colored_label(
                            Color32::LIGHT_RED,
                            "This IPA is encrypted (App Store copy) and can't be sideloaded.",
                        );
                    }
                }
                Some(Err(e)) => {
                    ui.colored_label(Color32::LIGHT_RED, e);
                }
                None if self.app_path.is_some() => {
                    ui.spinner();
                }
                None => {}
            }
        });

        let changed = self.options.changed_count();
        let title = if changed > 0 {
            format!("Advanced options ({changed} set)")
        } else {
            "Advanced options".into()
        };
        egui::CollapsingHeader::new(title)
            .id_salt("advanced")
            .show(ui, |ui| {
                egui::Grid::new("opts")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        let placeholder = |field: Option<&str>| field.unwrap_or("").to_string();
                        let info = self.app_info.as_ref().and_then(|r| r.as_ref().ok());
                        ui.label("App name");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.options.name)
                                .hint_text(placeholder(info.map(|i| i.name.as_str()))),
                        );
                        ui.end_row();
                        ui.label("Bundle ID");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.options.bundle_id)
                                .hint_text(placeholder(info.map(|i| i.bundle_id.as_str()))),
                        )
                        .on_hover_text("Change it to install a second copy of an app");
                        ui.end_row();
                        ui.label("Version");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.options.version)
                                .hint_text(placeholder(info.and_then(|i| i.version.as_deref()))),
                        );
                        ui.end_row();
                        ui.label("Minimum iOS");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.options.min_ios)
                                .hint_text(placeholder(info.and_then(|i| i.min_ios.as_deref()))),
                        );
                        ui.end_row();
                    });
                ui.checkbox(
                    &mut self.options.file_sharing,
                    "Enable file sharing (show in the Files app)",
                );
                ui.checkbox(
                    &mut self.options.remove_supported_devices,
                    "Remove device restrictions",
                );
                ui.checkbox(
                    &mut self.options.remove_extensions,
                    "Remove app extensions (saves App IDs)",
                );
                ui.checkbox(&mut self.options.remove_watch_app, "Remove Apple Watch app");
                ui.checkbox(
                    &mut self.options.register_extensions,
                    "Register separate App IDs for extensions",
                );
                ui.checkbox(
                    &mut self.options.increased_memory_limit,
                    "Increased memory limit",
                );
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Tweaks");
                    if ui.button("Add .dylib / .deb…").clicked() {
                        self.send(Cmd::Pick(PickKind::Tweak));
                    }
                });
                let mut remove = None;
                for (i, p) in self.options.inject.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("Remove").clicked() {
                            remove = Some(i);
                        }
                        ui.label(p.file_name().unwrap_or_default().to_string_lossy());
                    });
                }
                if let Some(i) = remove {
                    self.options.inject.remove(i);
                }
            });
        ui.checkbox(&mut self.remember, "Remember for auto-refresh");

        ui.add_space(6.0);
        let app_ok = matches!(&self.app_info, Some(Ok(info)) if !info.encrypted);
        let idle = self.busy.is_none();
        ui.horizontal(|ui| {
            let can_install = idle && app_ok && self.selected_device().is_some();
            let install = ui.add_enabled(
                can_install,
                egui::Button::new(RichText::new("Install").strong().size(16.0)).min_size(egui::vec2(120.0, 32.0)),
            );
            if install.clicked()
                && let (Some(device), Some(source)) = (self.selected_device().cloned(), self.app_path.clone())
            {
                self.send(Cmd::Install {
                    device: Box::new(device),
                    request: InstallRequest {
                        source,
                        options: self.options.to_options(),
                        remember: self.remember,
                    },
                });
            }
            if ui
                .add_enabled(idle && app_ok, egui::Button::new("Export signed IPA…"))
                .on_hover_text("Sign without installing. Free-account apps only run on devices registered to your Apple ID.")
                .clicked()
            {
                self.send(Cmd::Pick(PickKind::ExportTarget));
            }
        });
    }

    fn apps_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Installed apps");
            let idle = self.busy.is_none();
            if ui
                .add_enabled(idle, egui::Button::new("Re-sign due apps"))
                .clicked()
            {
                self.send(Cmd::Refresh { force: false });
            }
            if ui
                .add_enabled(idle, egui::Button::new("Re-sign all"))
                .clicked()
            {
                self.send(Cmd::Refresh { force: true });
            }
        });
        ui.weak("Free Apple IDs sign apps for 7 days. With AutoRefresh on, Sideward re-signs an app when its device connects over USB or Wi-Fi, keeping the app's data.");
        if !self.config.auto_refresh_on_connect {
            ui.colored_label(
                Color32::from_rgb(230, 160, 40),
                "AutoRefresh on connect is turned off in Settings.",
            );
        }
        ui.add_space(6.0);
        if self.apps.is_empty() {
            ui.label("Apps you install with “Remember for auto-refresh” show up here.");
            return;
        }
        let now = chrono::Utc::now();
        let mut forget = None;
        let mut toggled = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("apps")
                .striped(true)
                .min_col_width(60.0)
                .num_columns(5)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    ui.strong("App");
                    ui.strong("Device");
                    ui.strong("Expires");
                    ui.strong("AutoRefresh");
                    ui.label("");
                    ui.end_row();
                    for a in &self.apps {
                        ui.vertical(|ui| {
                            ui.set_min_width(220.0);
                            ui.add(egui::Label::new(&a.name).extend());
                            ui.add(egui::Label::new(RichText::new(&a.bundle_id).weak().small()).extend());
                        });
                        ui.label(&a.device_name);
                        match a.expires_at {
                            Some(e) if e <= now => {
                                ui.colored_label(Color32::LIGHT_RED, "Expired");
                            }
                            Some(e) => {
                                let left = e - now;
                                let text =
                                    format!("in {}d {}h", left.num_days(), left.num_hours() % 24);
                                if left.num_days() < 2 {
                                    ui.colored_label(Color32::from_rgb(230, 160, 40), text);
                                } else {
                                    ui.label(text);
                                }
                            }
                            None => {
                                ui.weak("unknown");
                            }
                        }
                        let mut auto = a.auto_refresh;
                        if ui
                            .checkbox(&mut auto, "")
                            .on_hover_text("Re-sign this app automatically before its 7 days pass, whenever its device connects")
                            .changed()
                        {
                            toggled = Some((a.key(), auto));
                        }
                        if ui.small_button("Forget").clicked() {
                            forget = Some(a.key());
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some((key, enabled)) = toggled {
            self.send(Cmd::SetAppAutoRefresh { key, enabled });
        }
        if let Some(key) = forget {
            self.send(Cmd::ForgetApp(key));
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let mut config = self.config.clone();

        ui.heading("AutoRefresh");
        ui.checkbox(
            &mut config.auto_refresh_on_connect,
            "Re-sign apps automatically when a device connects (USB or Wi-Fi)",
        );
        ui.add_enabled_ui(config.auto_refresh_on_connect, |ui| {
            ui.horizontal(|ui| {
                ui.label("Re-sign when an app has fewer than");
                ui.add(
                    egui::Slider::new(&mut config.refresh_days_left, 1..=7).suffix(" days left"),
                );
            });
            if config.refresh_days_left >= 7 {
                ui.weak("That's every time the device connects (at most every 12 hours per app).");
            }
        });
        ui.weak("Wi-Fi devices are checked every 5 minutes, and connected devices every hour.");

        ui.add_space(12.0);
        ui.heading("Account");
        ui.checkbox(&mut config.stay_signed_in, "Stay signed in")
            .on_hover_text("Turning this off forgets the saved password.");
        ui.weak("The password stays in the system keychain until it changes, Apple asks you to sign in again, or you sign out.");

        ui.add_space(12.0);
        ui.heading("Startup");
        let mut launch = self.launch_at_login;
        if ui
            .checkbox(
                &mut launch,
                "Start Sideward when I sign in to this computer",
            )
            .changed()
        {
            match crate::system::set_launch_at_login(launch) {
                Ok(()) => {
                    self.launch_at_login = launch;
                    config.launch_at_login = launch;
                }
                Err(e) => self.log.push(LogLine::Error(format!(
                    "Couldn't change the startup setting: {e:#}"
                ))),
            }
        }
        if self.has_tray() {
            ui.checkbox(
                &mut config.run_in_background,
                "Keep running in the tray when the window is closed",
            );
        } else {
            ui.weak("AutoRefresh runs while this window is open (minimized is fine).");
        }

        ui.add_space(12.0);
        ui.heading("Anisette server");
        ui.label("Apple's login needs device data from an anisette server. If Apple rejects one (errors like -80045 or -80046), Sideward can switch automatically.");
        let current = anisette::normalize(&config.anisette_server);
        egui::ComboBox::from_id_salt("anisette")
            .width(360.0)
            .selected_text(current.clone())
            .show_ui(ui, |ui| {
                for (url, label, reliable) in anisette::SERVERS {
                    let text = if *reliable {
                        format!("{label} — {url}")
                    } else {
                        format!("{label} — {url} (unreliable)")
                    };
                    ui.selectable_value(&mut config.anisette_server, url.to_string(), text);
                }
            });
        ui.checkbox(
            &mut config.anisette_auto_fallback,
            "Try other servers automatically",
        );
        ui.add_space(12.0);
        ui.heading("Certificates");
        ui.horizontal(|ui| {
            ui.label("Machine name shown on Apple's side");
            ui.text_edit_singleline(&mut config.machine_name);
        });
        if config_changed(&config, &self.config) {
            if config.anisette_server != self.config.anisette_server {
                self.logged_in = None;
            }
            self.config = config.clone();
            self.send(Cmd::SaveConfig(config));
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        if let Some(busy) = &self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                let step = if self.step.is_empty() {
                    busy.clone()
                } else {
                    self.step.clone()
                };
                ui.add(
                    egui::ProgressBar::new(self.fraction)
                        .text(step)
                        .desired_width(ui.available_width()),
                );
            });
        }
        egui::CollapsingHeader::new("Log")
            .id_salt("log")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(110.0)
                    .stick_to_bottom(true)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.weak("Ready.");
                        }
                        for line in &self.log {
                            match line {
                                LogLine::Info(m) => ui.label(m),
                                LogLine::Error(m) => ui.colored_label(Color32::LIGHT_RED, m),
                            };
                        }
                    });
            });
    }

    fn prompt_modal(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        let mut done = false;
        egui::Modal::new(egui::Id::unique("sideward-prompt")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            match prompt {
                ActivePrompt::TwoFactor { params, code, reply } => {
                    ui.heading("Two-factor authentication");
                    if let Some(err) = &params.last_error {
                        ui.colored_label(Color32::LIGHT_RED, err);
                    }
                    let target = if params.sms {
                        params
                            .numbers
                            .iter()
                            .find(|n| Some(n.id) == params.selected_number_id)
                            .map(|n| format!("by text to {}", n.number_with_dial_code))
                            .unwrap_or_else(|| "by text".into())
                    } else {
                        "to your trusted devices".into()
                    };
                    ui.label(format!("Enter the 6-digit code sent {target}."));
                    let edit = ui.add(egui::TextEdit::singleline(code).hint_text("123456").desired_width(120.0));
                    edit.request_focus();
                    let valid = code.trim().len() == 6 && code.trim().chars().all(|c| c.is_ascii_digit());
                    let mut answer = None;
                    ui.horizontal(|ui| {
                        if ui.add_enabled(valid, egui::Button::new("Verify")).clicked()
                            || (valid && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        {
                            answer = Some(TwoFactorCallbackResponse::SubmitCode(code.trim().to_string()));
                        }
                        if ui.button("Cancel").clicked() {
                            answer = Some(TwoFactorCallbackResponse::Abort);
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        if !params.unknown && ui.small_button("Resend").clicked() {
                            answer = Some(TwoFactorCallbackResponse::ResendCode);
                        }
                        if (params.sms || params.unknown) && ui.small_button("Send to my devices").clicked() {
                            answer = Some(TwoFactorCallbackResponse::SendToDevices);
                        }
                        for n in params.numbers.iter().filter(|n| Some(n.id) != params.selected_number_id) {
                            if ui.small_button(format!("Text {}", n.number_with_dial_code)).clicked() {
                                answer = Some(TwoFactorCallbackResponse::SendSms(n.id));
                            }
                        }
                    });
                    if let Some(a) = answer
                        && let Some(r) = reply.take()
                    {
                        let _ = r.send(a);
                        done = true;
                    }
                }
                ActivePrompt::Team { teams, selected, reply } => {
                    ui.heading("Choose a team");
                    for (i, t) in teams.iter().enumerate() {
                        ui.radio_value(selected, i, format!("{} ({})", t.name.as_deref().unwrap_or("Unnamed"), t.team_id));
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Use this team").clicked()
                            && let Some(r) = reply.take()
                        {
                            let _ = r.send(teams.get(*selected).map(|t| t.team_id.clone()));
                            done = true;
                        }
                        if ui.button("Cancel").clicked()
                            && let Some(r) = reply.take()
                        {
                            let _ = r.send(None);
                            done = true;
                        }
                    });
                }
                ActivePrompt::Certificates { certs, checked, reply } => {
                    ui.heading("Certificate limit reached");
                    ui.label("Revoke a certificate to continue. Apps signed with it stop opening until they're reinstalled.");
                    for (c, on) in certs.iter().zip(checked.iter_mut()) {
                        ui.checkbox(
                            on,
                            format!(
                                "{} — {}",
                                c.machine_name.as_deref().unwrap_or("unknown machine"),
                                c.name.as_deref().unwrap_or("certificate")
                            ),
                        );
                    }
                    let any = checked.iter().any(|b| *b);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(any, egui::Button::new("Revoke selected")).clicked()
                            && let Some(r) = reply.take()
                        {
                            let serials = certs
                                .iter()
                                .zip(checked.iter())
                                .filter(|(_, on)| **on)
                                .filter_map(|(c, _)| c.serial_number.clone())
                                .collect();
                            let _ = r.send(Some(serials));
                            done = true;
                        }
                        if ui.button("Cancel").clicked()
                            && let Some(r) = reply.take()
                        {
                            let _ = r.send(None);
                            done = true;
                        }
                    });
                }
            }
        });
        if done {
            self.prompt = None;
        }
    }
}

fn config_changed(a: &Config, b: &Config) -> bool {
    serde_json::to_value(a).ok() != serde_json::to_value(b).ok()
}

fn device_label(d: &Device) -> String {
    let kind = match d.transport {
        TransportKind::Usb => "USB",
        TransportKind::UsbmuxdWifi | TransportKind::DirectWifi => "Wi-Fi",
    };
    let mut s = format!("[{kind}] {}", d.name);
    if let Some(v) = &d.ios_version {
        s.push_str(&format!("  ·  iOS {v}"));
    }
    if let Some(ip) = d.ip {
        s.push_str(&format!("  ·  {ip}"));
    }
    s
}

impl eframe::App for SidewardApp {
    /// Runs even while the window is hidden in the tray, so prompts can bring it back.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if std::mem::take(&mut self.minimize_on_first_frame) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        self.handle_close(&ctx);

        let dropped: Option<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .find(|p| !p.as_os_str().is_empty())
        });
        if let Some(path) = dropped {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            match ext.as_str() {
                "dylib" | "deb" | "framework" | "bundle" => {
                    if !self.options.inject.contains(&path) {
                        self.options.inject.push(path);
                    }
                }
                _ => {
                    self.tab = Tab::Install;
                    self.set_app(path);
                }
            }
        }

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("Sideward").strong());
                ui.add_space(16.0);
                ui.selectable_value(&mut self.tab, Tab::Install, "Install");
                ui.selectable_value(
                    &mut self.tab,
                    Tab::Apps,
                    format!("My apps ({})", self.apps.len()),
                );
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
            });
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(4.0);
            self.status_bar(ui);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                Tab::Install => {
                    self.account_section(ui);
                    ui.separator();
                    self.device_section(ui);
                    ui.separator();
                    self.app_section(ui);
                }
                Tab::Apps => self.apps_tab(ui),
                Tab::Settings => self.settings_tab(ui),
            });
        });

        self.prompt_modal(&ctx);
    }
}
