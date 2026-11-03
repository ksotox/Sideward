//! The Device tab (libimobiledevice / ideviceinstaller style tools) and the Sources tab
//! (AltStore/SideStore sources).

use eframe::egui::{self, Color32, RichText};
use sideward_core::{
    device::Device,
    provisioning::ProfileInfo,
    sources::Source,
    tools::{DeviceApp, DeviceDetails, Power, format_bytes},
};

use crate::worker::{Cmd, SourceUpdate};

const MAX_LOG_LINES: usize = 2000;

/// What a confirmation dialog will do once the user agrees.
pub struct Confirm {
    pub message: String,
    pub cmd: Cmd,
}

#[derive(Default)]
pub struct DeviceTools {
    pub details: Option<DeviceDetails>,
    pub apps: Option<Vec<DeviceApp>>,
    pub show_system: bool,
    pub profiles: Option<Vec<ProfileInfo>>,
    pub crashes: Option<Vec<String>>,
    pub crash_filter: String,
    pub log: Vec<String>,
    pub log_filter: String,
    pub log_running: bool,
    /// UDID the data above belongs to; cleared when the selection changes.
    pub for_udid: Option<String>,
}

impl DeviceTools {
    pub fn push_log(&mut self, lines: Vec<String>) {
        self.log.extend(lines);
        if self.log.len() > MAX_LOG_LINES {
            let extra = self.log.len() - MAX_LOG_LINES;
            self.log.drain(..extra);
        }
    }

    /// Returns a command to run now, or one to confirm first.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        device: Option<&Device>,
        idle: bool,
    ) -> (Option<Cmd>, Option<Confirm>) {
        let mut run = None;
        let mut confirm = None;
        let Some(device) = device else {
            ui.label("Pick a device on the Install tab, or connect one.");
            return (None, None);
        };
        if self.for_udid.as_deref() != Some(device.udid.as_str()) {
            *self = DeviceTools {
                for_udid: Some(device.udid.clone()),
                log_filter: std::mem::take(&mut self.log_filter),
                ..Default::default()
            };
            run = Some(Cmd::DeviceDetails(Box::new(device.clone())));
        }
        let boxed = || Box::new(device.clone());

        // Info card
        ui.horizontal(|ui| {
            ui.heading(&device.name);
            if ui.add_enabled(idle, egui::Button::new("Refresh")).clicked() {
                run = Some(Cmd::DeviceDetails(boxed()));
            }
        });
        egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            match &self.details {
                None => {
                    ui.spinner();
                }
                Some(d) => {
                    egui::Grid::new("details").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                        let row = |ui: &mut egui::Ui, k: &str, v: String| {
                            ui.weak(k);
                            ui.label(v);
                            ui.end_row();
                        };
                        row(ui, "Model", d.model_name.clone().unwrap_or_else(|| d.product_type.clone()));
                        row(ui, "iOS", format!("{} ({})", d.ios_version, d.build.as_deref().unwrap_or("?")));
                        if let Some(b) = d.battery_percent {
                            let charging = if d.charging == Some(true) { ", charging" } else { "" };
                            row(ui, "Battery", format!("{b}%{charging}"));
                        }
                        if let (Some(t), Some(f)) = (d.storage_total, d.storage_free) {
                            row(ui, "Storage", format!("{} free of {}", format_bytes(f), format_bytes(t)));
                        }
                        row(ui, "UDID", d.udid.clone());
                        row(ui, "Connection", device.transport.label().to_string());
                    });
                    if d.developer_mode == Some(false) {
                        ui.colored_label(
                            Color32::from_rgb(230, 160, 40),
                            "Developer Mode is off: sideloaded apps won't open. Turn it on in Settings > Privacy & Security > Developer Mode.",
                        );
                    }
                }
            }
            ui.horizontal(|ui| {
                for (label, action, what) in [
                    ("Restart", Power::Restart, "restart"),
                    ("Shut down", Power::Shutdown, "shut down"),
                    ("Sleep", Power::Sleep, "put to sleep"),
                ] {
                    if ui.add_enabled(idle, egui::Button::new(label)).clicked() {
                        confirm = Some(Confirm {
                            message: format!("{} will {what}. Continue?", device.name),
                            cmd: Cmd::Power { device: boxed(), action },
                        });
                    }
                }
            });
        });

        // Installed apps (ideviceinstaller)
        ui.add_space(8.0);
        egui::CollapsingHeader::new("Installed apps")
            .id_salt("dev-apps")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.add_enabled(idle, egui::Button::new("Load")).clicked() {
                        run = Some(Cmd::DeviceApps {
                            device: boxed(),
                            system: self.show_system,
                        });
                    }
                    ui.checkbox(&mut self.show_system, "Include Apple's apps");
                });
                if let Some(apps) = &self.apps {
                    egui::ScrollArea::vertical()
                        .id_salt("dev-apps-scroll")
                        .max_height(260.0)
                        .show(ui, |ui| {
                            egui::Grid::new("dev-apps-grid")
                                .striped(true)
                                .num_columns(4)
                                .spacing([12.0, 4.0])
                                .show(ui, |ui| {
                                    for a in apps {
                                        ui.vertical(|ui| {
                                            ui.set_min_width(200.0);
                                            ui.add(egui::Label::new(&a.name).extend());
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(&a.bundle_id).weak().small(),
                                                )
                                                .extend(),
                                            );
                                        });
                                        ui.label(a.version.as_deref().unwrap_or(""));
                                        if a.sideloaded {
                                            ui.colored_label(
                                                Color32::from_rgb(60, 170, 90),
                                                "sideloaded",
                                            );
                                        } else {
                                            ui.label("");
                                        }
                                        if !a.system
                                            && ui
                                                .add_enabled(
                                                    idle,
                                                    egui::Button::new("Uninstall").small(),
                                                )
                                                .clicked()
                                        {
                                            confirm = Some(Confirm {
                                                message: format!(
                                                    "Remove {} and its data from {}?",
                                                    a.name, device.name
                                                ),
                                                cmd: Cmd::UninstallApp {
                                                    device: boxed(),
                                                    bundle_id: a.bundle_id.clone(),
                                                },
                                            });
                                        }
                                        ui.end_row();
                                    }
                                });
                        });
                }
            });

        // Provisioning profiles (ideviceprovision)
        egui::CollapsingHeader::new("Provisioning profiles").id_salt("dev-profiles").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new("Load")).clicked() {
                    run = Some(Cmd::Profiles(boxed()));
                }
                if ui
                    .add_enabled(idle, egui::Button::new("Remove expired"))
                    .on_hover_text("Old free-account profiles pile up on the device; expired ones are safe to remove")
                    .clicked()
                {
                    run = Some(Cmd::RemoveExpiredProfiles(boxed()));
                }
            });
            if let Some(profiles) = &self.profiles {
                if profiles.is_empty() {
                    ui.weak("No profiles installed.");
                }
                let now = chrono::Utc::now();
                for p in profiles {
                    ui.horizontal(|ui| {
                        let expiry = match p.expires {
                            Some(e) if e <= now => RichText::new("expired").color(Color32::LIGHT_RED),
                            Some(e) => RichText::new(format!("until {}", e.format("%b %e"))),
                            None => RichText::new(""),
                        };
                        ui.label(expiry);
                        ui.label(p.bundle_id().unwrap_or(&p.name));
                        ui.weak(p.team_name.as_deref().unwrap_or(""));
                    });
                }
            }
        });

        // Crash reports (idevicecrashreport)
        egui::CollapsingHeader::new("Crash reports")
            .id_salt("dev-crashes")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.add_enabled(idle, egui::Button::new("Load")).clicked() {
                        run = Some(Cmd::CrashReports(boxed()));
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.crash_filter)
                            .hint_text("Filter by app")
                            .desired_width(160.0),
                    );
                });
                if let Some(crashes) = &self.crashes {
                    let shown: Vec<&String> = crashes
                        .iter()
                        .filter(|c| c.to_lowercase().contains(&self.crash_filter.to_lowercase()))
                        .collect();
                    if shown.is_empty() {
                        ui.weak("No crash reports.");
                    } else if ui
                        .add_enabled(
                            idle,
                            egui::Button::new(format!("Save {} to Downloads", shown.len())),
                        )
                        .clicked()
                    {
                        run = Some(Cmd::SaveCrashReports {
                            device: boxed(),
                            names: shown.iter().map(|s| s.to_string()).collect(),
                        });
                    }
                    egui::ScrollArea::vertical()
                        .id_salt("crash-scroll")
                        .max_height(160.0)
                        .show(ui, |ui| {
                            for c in shown {
                                ui.label(RichText::new(c).monospace().small());
                            }
                        });
                }
            });

        // Live log (idevicesyslog)
        egui::CollapsingHeader::new("Live log")
            .id_salt("dev-log")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.log_filter)
                            .hint_text("Only lines containing… (e.g. the app name)")
                            .desired_width(240.0),
                    );
                    if self.log_running {
                        if ui.button("Stop").clicked() {
                            run = Some(Cmd::StopSyslog);
                        }
                    } else if ui.button("Start").clicked() {
                        self.log_running = true;
                        run = Some(Cmd::StartSyslog {
                            device: boxed(),
                            filter: self.log_filter.clone(),
                        });
                    }
                    if ui.button("Clear").clicked() {
                        self.log.clear();
                    }
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(self.log.join("\n"));
                    }
                });
                egui::ScrollArea::vertical()
                    .id_salt("log-scroll")
                    .max_height(280.0)
                    .stick_to_bottom(true)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(RichText::new(line).monospace().small());
                        }
                    });
            });
        (run, confirm)
    }
}

#[derive(Default)]
pub struct SourcesUi {
    pub sources: Vec<Source>,
    pub errors: Vec<String>,
    pub updates: Vec<SourceUpdate>,
    pub new_url: String,
    pub search: String,
    pub loaded: bool,
}

impl SourcesUi {
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        device: Option<&Device>,
        urls: &[String],
        idle: bool,
    ) -> Option<Cmd> {
        let mut run = None;
        if !self.loaded && idle {
            self.loaded = true;
            if !urls.is_empty() {
                run = Some(Cmd::LoadSources);
            }
        }
        ui.heading("Sources");
        ui.weak("AltStore and SideStore sources list apps with download links. Add a source's URL to browse and install its apps.");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_url)
                    .hint_text("https://…")
                    .desired_width(320.0),
            );
            let valid = self.new_url.trim().starts_with("http");
            if ui
                .add_enabled(idle && valid, egui::Button::new("Add source"))
                .clicked()
            {
                run = Some(Cmd::AddSource(std::mem::take(&mut self.new_url)));
            }
            if ui
                .add_enabled(idle && !urls.is_empty(), egui::Button::new("Reload"))
                .clicked()
            {
                run = Some(Cmd::LoadSources);
            }
        });
        for url in urls {
            ui.horizontal(|ui| {
                let name = self
                    .sources
                    .iter()
                    .find(|s| &s.url == url)
                    .map(|s| format!("{} ({} apps)", s.name, s.apps.len()))
                    .unwrap_or_else(|| url.clone());
                ui.label(name).on_hover_text(url);
                if ui
                    .add_enabled(idle, egui::Button::new("Remove").small())
                    .clicked()
                {
                    run = Some(Cmd::RemoveSource(url.clone()));
                }
            });
        }
        for e in &self.errors {
            ui.colored_label(Color32::LIGHT_RED, e);
        }

        if !self.updates.is_empty() {
            ui.add_space(8.0);
            ui.heading("Updates");
            for u in &self.updates {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} on {}: {} → {}",
                        u.name,
                        u.device_name,
                        u.current.as_deref().unwrap_or("?"),
                        u.version.version
                    ));
                    let target = device.filter(|d| d.udid == u.device_udid);
                    if ui
                        .add_enabled(idle && target.is_some(), egui::Button::new("Update"))
                        .on_disabled_hover_text(format!(
                            "Connect {} and select it first",
                            u.device_name
                        ))
                        .clicked()
                        && let Some(d) = target
                    {
                        run = Some(Cmd::InstallFromSource {
                            device: Box::new(d.clone()),
                            app: Box::new(u.app.clone()),
                            version: u.version.clone(),
                            options: u.options.clone(),
                        });
                    }
                });
            }
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading("Apps");
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search")
                    .desired_width(200.0),
            );
        });
        if device.is_none() {
            ui.weak("Select a device on the Install tab to install apps.");
        }
        let q = self.search.to_lowercase();
        egui::ScrollArea::vertical()
            .id_salt("source-apps")
            .show(ui, |ui| {
                for source in &self.sources {
                    for app in &source.apps {
                        if !q.is_empty()
                            && !app.name.to_lowercase().contains(&q)
                            && !app.bundle_identifier.to_lowercase().contains(&q)
                        {
                            continue;
                        }
                        let Some(version) = app.latest() else {
                            continue;
                        };
                        egui::Frame::group(ui.style())
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label(RichText::new(&app.name).strong());
                                        let mut sub =
                                            format!("{} · v{}", source.name, version.version);
                                        if let Some(dev) = &app.developer_name {
                                            sub = format!("{dev} · {sub}");
                                        }
                                        if let Some(size) = version.size {
                                            sub.push_str(&format!(" · {}", format_bytes(size)));
                                        }
                                        ui.weak(sub);
                                        if let Some(s) = app
                                            .subtitle
                                            .as_ref()
                                            .or(app.localized_description.as_ref())
                                        {
                                            ui.add(
                                                egui::Label::new(RichText::new(s).small())
                                                    .truncate(),
                                            );
                                        }
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add_enabled(
                                                    idle && device.is_some(),
                                                    egui::Button::new("Install"),
                                                )
                                                .clicked()
                                                && let Some(d) = device
                                            {
                                                run = Some(Cmd::InstallFromSource {
                                                    device: Box::new(d.clone()),
                                                    app: Box::new(app.clone()),
                                                    version: version.clone(),
                                                    options: Default::default(),
                                                });
                                            }
                                        },
                                    );
                                });
                            });
                    }
                }
            });
        run
    }
}
