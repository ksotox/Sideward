//! Provisioning profiles (`embedded.mobileprovision`, or the profiles installed on a device).

use chrono::{DateTime, Utc};
use plist::{Dictionary, Value};
use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProfileInfo {
    pub name: String,
    pub uuid: String,
    pub app_id_name: Option<String>,
    /// `TEAMID.bundle.id` from the profile's entitlements.
    pub application_identifier: Option<String>,
    pub team_name: Option<String>,
    pub team_id: Option<String>,
    pub created: Option<DateTime<Utc>>,
    pub expires: Option<DateTime<Utc>>,
    pub provisioned_devices: Vec<String>,
    /// True for development profiles (debugger can attach).
    pub get_task_allow: bool,
    #[serde(skip)]
    pub entitlements: Dictionary,
}

fn date(d: &Dictionary, key: &str) -> Option<DateTime<Utc>> {
    let date = d.get(key)?.as_date()?;
    Some(std::time::SystemTime::from(date).into())
}

fn string(d: &Dictionary, key: &str) -> Option<String> {
    d.get(key).and_then(Value::as_string).map(str::to_string)
}

impl ProfileInfo {
    /// Parse a CMS-wrapped profile by pulling out the XML plist inside it.
    pub fn parse(data: &[u8]) -> Option<Self> {
        let start = data.windows(5).position(|w| w == b"<?xml")?;
        let end = data.windows(8).rposition(|w| w == b"</plist>")? + 8;
        let d = Value::from_reader_xml(data.get(start..end)?)
            .ok()?
            .into_dictionary()?;
        let entitlements = d
            .get("Entitlements")
            .and_then(Value::as_dictionary)
            .cloned()
            .unwrap_or_default();
        Some(Self {
            name: string(&d, "Name").unwrap_or_default(),
            uuid: string(&d, "UUID").unwrap_or_default(),
            app_id_name: string(&d, "AppIDName"),
            application_identifier: string(&entitlements, "application-identifier"),
            team_name: string(&d, "TeamName"),
            team_id: d
                .get("TeamIdentifier")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_string)
                .map(str::to_string),
            created: date(&d, "CreationDate"),
            expires: date(&d, "ExpirationDate"),
            provisioned_devices: d
                .get("ProvisionedDevices")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_string)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            get_task_allow: entitlements
                .get("get-task-allow")
                .and_then(Value::as_boolean)
                .unwrap_or(false),
            entitlements,
        })
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires.is_some_and(|e| e <= now)
    }

    /// Bundle id without the team prefix.
    pub fn bundle_id(&self) -> Option<&str> {
        self.application_identifier
            .as_deref()
            .and_then(|a| a.split_once('.').map(|(_, rest)| rest))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample_profile(expires: &str, devices: &[&str]) -> Vec<u8> {
        let devices: String = devices
            .iter()
            .map(|d| format!("<string>{d}</string>"))
            .collect();
        format!(
            r#"0\x82garbage<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>Name</key><string>iOS Team Provisioning Profile: com.example.demo</string>
<key>UUID</key><string>1111-2222</string>
<key>AppIDName</key><string>Demo</string>
<key>TeamName</key><string>Jane Doe</string>
<key>TeamIdentifier</key><array><string>ABCDE12345</string></array>
<key>CreationDate</key><date>2026-10-25T12:00:00Z</date>
<key>ExpirationDate</key><date>{expires}</date>
<key>ProvisionedDevices</key><array>{devices}</array>
<key>Entitlements</key><dict>
  <key>application-identifier</key><string>ABCDE12345.com.example.demo</string>
  <key>get-task-allow</key><true/>
</dict>
</dict></plist>trailing"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_profiles() {
        let p = ProfileInfo::parse(&sample_profile("2026-11-01T12:00:00Z", &["UDID-1"])).unwrap();
        assert_eq!(p.uuid, "1111-2222");
        assert_eq!(p.team_id.as_deref(), Some("ABCDE12345"));
        assert_eq!(p.bundle_id(), Some("com.example.demo"));
        assert_eq!(p.provisioned_devices, vec!["UDID-1"]);
        assert!(p.get_task_allow);
        assert_eq!(p.expires.unwrap().to_rfc3339(), "2026-11-01T12:00:00+00:00");
        assert!(p.is_expired("2026-11-02T00:00:00Z".parse().unwrap()));
        assert!(ProfileInfo::parse(b"not a profile").is_none());
    }
}
