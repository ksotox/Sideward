//! AltStore / SideStore app sources: JSON catalogs of apps with download links. Add a source
//! URL, browse its apps, install one, and see when a newer version is out.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::{
    install::{Progress, ProgressFn},
    paths::Paths,
    registry::InstalledApp,
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Source {
    pub name: String,
    pub identifier: Option<String>,
    pub source_url: Option<String>,
    pub apps: Vec<SourceApp>,
    /// Where it was fetched from (not part of the JSON).
    #[serde(skip_deserializing)]
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceApp {
    pub name: String,
    pub bundle_identifier: String,
    pub developer_name: Option<String>,
    pub subtitle: Option<String>,
    pub localized_description: Option<String>,
    #[serde(rename = "iconURL")]
    pub icon_url: Option<String>,
    pub versions: Vec<SourceVersion>,
    // Legacy (pre-2023) sources put the latest version on the app itself
    pub version: Option<String>,
    #[serde(rename = "downloadURL")]
    pub download_url: Option<String>,
    pub size: Option<u64>,
    pub version_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceVersion {
    pub version: String,
    pub date: Option<String>,
    pub localized_description: Option<String>,
    #[serde(rename = "downloadURL")]
    pub download_url: String,
    pub size: Option<u64>,
    #[serde(rename = "minOSVersion")]
    pub min_os_version: Option<String>,
}

impl SourceApp {
    /// The newest version, from `versions` or the legacy top-level fields.
    pub fn latest(&self) -> Option<SourceVersion> {
        if let Some(v) = self.versions.first() {
            return Some(v.clone());
        }
        Some(SourceVersion {
            version: self.version.clone()?,
            date: self.version_date.clone(),
            localized_description: None,
            download_url: self.download_url.clone()?,
            size: self.size,
            min_os_version: None,
        })
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("Sideward/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

pub async fn fetch(url: &str) -> Result<Source> {
    let response = client()?
        .get(url)
        .send()
        .await
        .with_context(|| format!("Couldn't reach {url}"))?
        .error_for_status()
        .with_context(|| format!("{url} returned an error"))?;
    let mut source: Source = response
        .json()
        .await
        .with_context(|| format!("{url} isn't an AltStore/SideStore source"))?;
    if source.name.is_empty() {
        source.name = url.to_string();
    }
    source.url = url.to_string();
    Ok(source)
}

/// Fetch every source; failures are returned alongside so one dead source doesn't hide the rest.
pub async fn fetch_all(urls: &[String]) -> (Vec<Source>, Vec<(String, anyhow::Error)>) {
    let results = futures_util::future::join_all(urls.iter().map(|u| fetch(u))).await;
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    for (url, r) in urls.iter().zip(results) {
        match r {
            Ok(s) => ok.push(s),
            Err(e) => failed.push((url.clone(), e)),
        }
    }
    (ok, failed)
}

/// Find an app by bundle id or (case-insensitive) name across sources.
pub fn find<'a>(sources: &'a [Source], query: &str) -> Vec<(&'a Source, &'a SourceApp)> {
    sources
        .iter()
        .flat_map(|s| s.apps.iter().map(move |a| (s, a)))
        .filter(|(_, a)| {
            a.bundle_identifier.eq_ignore_ascii_case(query) || a.name.eq_ignore_ascii_case(query)
        })
        .collect()
}

/// Download an app's IPA into Sideward's data dir (reused if already there).
pub async fn download(
    paths: &Paths,
    app: &SourceApp,
    version: &SourceVersion,
    progress: ProgressFn,
) -> Result<PathBuf> {
    let safe = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let dir = paths.root().join("downloads");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!(
        "{}-{}.ipa",
        safe(&app.bundle_identifier),
        safe(&version.version)
    ));
    if dest.is_file()
        && version
            .size
            .is_none_or(|s| std::fs::metadata(&dest).is_ok_and(|m| m.len() == s))
    {
        return Ok(dest);
    }

    progress(Progress::Step(format!(
        "Downloading {} {}",
        app.name, version.version
    )));
    let response = client()?
        .get(&version.download_url)
        .send()
        .await
        .with_context(|| format!("Couldn't download {}", version.download_url))?
        .error_for_status()?;
    let total = response.content_length().or(version.size);
    let tmp = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut done = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        done += chunk.len() as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            progress(Progress::Fraction(done as f32 / total as f32));
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);
    if done == 0 {
        bail!("The download was empty");
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Compare dotted versions numerically ("1.10" > "1.9"); non-numeric parts compare as text.
pub fn version_newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<String> { v.split(['.', '-', ' ']).map(str::to_string).collect() };
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).map(String::as_str).unwrap_or("0");
        let y = b.get(i).map(String::as_str).unwrap_or("0");
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => x.cmp(y),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord == std::cmp::Ordering::Greater;
        }
    }
    false
}

#[derive(Debug, Clone)]
pub struct Update {
    pub installed: InstalledApp,
    pub app: SourceApp,
    pub version: SourceVersion,
}

/// The installed bundle id without the team id Sideward appended when signing.
pub fn original_bundle_id(app: &InstalledApp) -> &str {
    app.bundle_id
        .strip_suffix(&format!(".{}", app.team_id))
        .unwrap_or(&app.bundle_id)
}

/// Remembered apps for which a source has a newer version.
pub fn updates(installed: &[InstalledApp], sources: &[Source]) -> Vec<Update> {
    let mut out = Vec::new();
    for inst in installed {
        let id = original_bundle_id(inst);
        let best = sources
            .iter()
            .flat_map(|s| &s.apps)
            .filter(|a| a.bundle_identifier == id)
            .filter_map(|a| a.latest().map(|v| (a, v)))
            .filter(|(_, v)| {
                inst.version
                    .as_deref()
                    .is_none_or(|cur| version_newer(&v.version, cur))
            })
            .max_by(|(_, x), (_, y)| {
                if version_newer(&x.version, &y.version) {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            });
        if let Some((app, version)) = best {
            out.push(Update {
                installed: inst.clone(),
                app: app.clone(),
                version,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"{
      "name": "Example Source",
      "identifier": "com.example.source",
      "apps": [
        {"name": "Delta", "bundleIdentifier": "com.rileytestut.Delta", "developerName": "Riley",
         "versions": [{"version": "1.6.2", "date": "2026-10-01", "downloadURL": "https://example.com/delta.ipa", "size": 1000, "minOSVersion": "14.0"}]},
        {"name": "Legacy", "bundleIdentifier": "com.example.legacy", "version": "2.0", "downloadURL": "https://example.com/legacy.ipa"}
      ],
      "news": []
    }"#;

    #[test]
    fn parses_current_and_legacy_sources() {
        let source: Source = serde_json::from_str(SOURCE).unwrap();
        assert_eq!(source.apps.len(), 2);
        let delta = source.apps[0].latest().unwrap();
        assert_eq!(delta.version, "1.6.2");
        assert_eq!(delta.min_os_version.as_deref(), Some("14.0"));
        assert_eq!(
            source.apps[1].latest().unwrap().download_url,
            "https://example.com/legacy.ipa"
        );
        assert_eq!(find(std::slice::from_ref(&source), "delta").len(), 1);
    }

    #[test]
    fn compares_versions() {
        assert!(version_newer("1.10", "1.9"));
        assert!(version_newer("2.0", "1.9.9"));
        assert!(!version_newer("1.6.2", "1.6.2"));
        assert!(!version_newer("1.6", "1.6.0"));
        assert!(version_newer("1.6.1", "1.6"));
    }

    #[test]
    fn finds_updates_for_installed_apps() {
        let source: Source = serde_json::from_str(SOURCE).unwrap();
        let installed = InstalledApp {
            udid: "U".into(),
            device_name: "Phone".into(),
            apple_id: "a@b.c".into(),
            team_id: "ABCDE12345".into(),
            bundle_id: "com.rileytestut.Delta.ABCDE12345".into(),
            name: "Delta".into(),
            version: Some("1.6.1".into()),
            source: PathBuf::from("x.ipa"),
            options: Default::default(),
            installed_at: chrono::Utc::now(),
            expires_at: None,
            auto_refresh: true,
        };
        assert_eq!(original_bundle_id(&installed), "com.rileytestut.Delta");
        let ups = updates(
            std::slice::from_ref(&installed),
            std::slice::from_ref(&source),
        );
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].version.version, "1.6.2");

        let mut current = installed;
        current.version = Some("1.6.2".into());
        assert!(updates(&[current], &[source]).is_empty());
    }
}
