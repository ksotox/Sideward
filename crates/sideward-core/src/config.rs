use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{anisette, paths::Paths};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Last Apple ID used, so commands don't need `--apple-id` every time.
    pub apple_id: Option<String>,
    pub anisette_server: String,
    /// Try the other Mac anisette servers when Apple rejects the current one.
    pub anisette_auto_fallback: bool,
    /// Remembered team per Apple ID, for accounts that belong to several teams.
    pub teams: BTreeMap<String, String>,
    /// Shown next to the development certificate on Apple's side.
    pub machine_name: String,
    /// Keep the Apple ID password in the OS keychain after a successful login, so the user stays
    /// signed in until the password changes or Apple asks them to sign in again.
    pub stay_signed_in: bool,
    /// Re-sign remembered apps automatically when their device connects over USB or Wi-Fi.
    pub auto_refresh_on_connect: bool,
    /// Re-sign once an app has fewer than this many days left (1-7). 7 means every time the
    /// device connects, at most every [`crate::registry::REFRESH_COOLDOWN_HOURS`] hours.
    pub refresh_days_left: i64,
    /// Desktop app: start when the user signs in to the computer.
    pub launch_at_login: bool,
    /// Desktop app: closing the window keeps Sideward running (tray on Windows/macOS) so it can
    /// re-sign apps when devices connect.
    pub run_in_background: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            apple_id: None,
            anisette_server: anisette::DEFAULT_SERVER.to_string(),
            anisette_auto_fallback: true,
            teams: BTreeMap::new(),
            machine_name: "Sideward".to_string(),
            stay_signed_in: true,
            auto_refresh_on_connect: true,
            refresh_days_left: 3,
            launch_at_login: false,
            run_in_background: true,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        crate::paths::read_json(&paths.config_file())
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        crate::paths::write_json(&paths.config_file(), self)
    }

    pub fn refresh_policy(&self) -> crate::registry::RefreshPolicy {
        crate::registry::RefreshPolicy::new(self.refresh_days_left)
    }
}
