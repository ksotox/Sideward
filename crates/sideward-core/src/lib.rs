//! Sideward core: everything the CLI and GUI share.
//!
//! - [`account`]: Apple ID login through isideload, with automatic anisette server fallback
//! - [`device`]: device discovery over USB, usbmuxd Wi-Fi sync, and direct Wi-Fi (pairing file)
//! - [`wifi`]: one-time Wi-Fi setup over USB, pairing file import/export, Bonjour discovery
//! - [`ipa`]: Sideloadly-style app modifications (name, bundle id, tweaks, ...)
//! - [`install`]: sign + install to a device, or sign + export an IPA
//! - [`registry`]: remembers installed apps so they can be refreshed before they expire

pub mod account;
pub mod anisette;
pub mod config;
pub mod credentials;
pub mod device;
pub mod inject;
pub mod install;
pub mod ipa;
pub mod macho;
pub mod paths;
pub mod registry;
pub mod watch;
pub mod wifi;

pub use account::{Prompter, Session};
pub use config::Config;
pub use paths::Paths;

/// Install the rustls crypto provider and isideload's error hooks. Call once at startup.
pub fn init() -> anyhow::Result<()> {
    // Ignore the error if a provider was already installed by the host application
    let _ = rustls::crypto::ring::default_provider().install_default();
    isideload::init().map_err(report_to_anyhow)?;
    Ok(())
}

/// isideload reports errors as `rootcause::Report`, which isn't a `std::error::Error`.
pub(crate) fn report_to_anyhow(report: rootcause::Report) -> anyhow::Error {
    anyhow::anyhow!("{}", flatten_report(&report.to_string()))
}

/// Turn rootcause's tree output (one `●` line per context, plus source locations) into a
/// single `outer: inner: cause` line like anyhow's `{:#}`.
fn flatten_report(rendered: &str) -> String {
    let parts: Vec<&str> = rendered
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix('●'))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if parts.is_empty() {
        rendered.trim().to_string()
    } else {
        parts.join(": ")
    }
}

pub(crate) trait ReportExt<T> {
    fn anyhow(self) -> anyhow::Result<T>;
}

impl<T> ReportExt<T> for Result<T, rootcause::Report> {
    fn anyhow(self) -> anyhow::Result<T> {
        self.map_err(report_to_anyhow)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn flattens_rootcause_trees() {
        let rendered = "\n ● Failed to log in\n ├ src/a.rs:1\n │\n ● Auth error -22406: Bad password\n ╰ src/b.rs:2\n";
        assert_eq!(
            super::flatten_report(rendered),
            "Failed to log in: Auth error -22406: Bad password"
        );
        assert_eq!(super::flatten_report("plain"), "plain");
    }
}
