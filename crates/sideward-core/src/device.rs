//! Device discovery and connection over USB, usbmuxd Wi-Fi sync, and direct Wi-Fi.

use std::{fmt, future::Future, net::IpAddr, pin::Pin, time::Duration};

use anyhow::{Context, Result, bail};
use idevice::{
    Idevice, IdeviceError, IdeviceService,
    lockdown::LockdownClient,
    pairing_file::PairingFile,
    provider::{IdeviceProvider, TcpProvider, UsbmuxdProvider},
    usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice},
};
use serde::Serialize;
use tracing::debug;

use crate::{paths::Paths, wifi};

const LABEL: &str = "sideward";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TransportKind {
    /// Cable, through usbmuxd (Apple Mobile Device Support / iTunes on Windows).
    Usb,
    /// Wi-Fi sync through usbmuxd ("Show this iPhone when on Wi-Fi" in Finder/iTunes, or netmuxd).
    UsbmuxdWifi,
    /// Direct TCP to the device's lockdownd using a pairing file Sideward saved. No iTunes needed.
    DirectWifi,
}

impl TransportKind {
    pub fn label(&self) -> &'static str {
        match self {
            TransportKind::Usb => "USB",
            TransportKind::UsbmuxdWifi => "Wi-Fi (usbmuxd)",
            TransportKind::DirectWifi => "Wi-Fi (direct)",
        }
    }

    pub fn is_wifi(&self) -> bool {
        !matches!(self, TransportKind::Usb)
    }

    /// Lower is preferred when a device is reachable several ways.
    fn rank(&self) -> u8 {
        match self {
            TransportKind::Usb => 0,
            TransportKind::UsbmuxdWifi => 1,
            TransportKind::DirectWifi => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub udid: String,
    pub name: String,
    pub ios_version: Option<String>,
    pub transport: TransportKind,
    pub ip: Option<IpAddr>,
    #[serde(skip)]
    usbmuxd_id: Option<u32>,
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}", self.name, self.transport.label())?;
        if let Some(ip) = self.ip {
            write!(f, " {ip}")?;
        }
        if let Some(v) = &self.ios_version {
            write!(f, ", iOS {v}")?;
        }
        write!(f, ") {}", self.udid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportFilter {
    #[default]
    Any,
    UsbOnly,
    WifiOnly,
}

impl TransportFilter {
    fn allows(&self, kind: TransportKind) -> bool {
        match self {
            TransportFilter::Any => true,
            TransportFilter::UsbOnly => kind == TransportKind::Usb,
            TransportFilter::WifiOnly => kind.is_wifi(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscoverOptions {
    pub filter: TransportFilter,
    /// How long to listen for Bonjour announcements from Wi-Fi devices.
    pub wifi_timeout: Duration,
}

impl Default for DiscoverOptions {
    fn default() -> Self {
        Self {
            filter: TransportFilter::Any,
            wifi_timeout: Duration::from_secs(3),
        }
    }
}

/// A connection to one device, over whichever transport was chosen.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // one short-lived value per install
pub enum DeviceProvider {
    Usbmuxd(UsbmuxdProvider),
    Tcp(TcpProvider),
}

impl IdeviceProvider for DeviceProvider {
    fn connect(
        &self,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<Idevice, IdeviceError>> + Send>> {
        match self {
            DeviceProvider::Usbmuxd(p) => p.connect(port),
            DeviceProvider::Tcp(p) => p.connect(port),
        }
    }

    fn label(&self) -> &str {
        match self {
            DeviceProvider::Usbmuxd(p) => p.label(),
            DeviceProvider::Tcp(p) => p.label(),
        }
    }

    fn get_pairing_file(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<PairingFile, IdeviceError>> + Send>> {
        match self {
            DeviceProvider::Usbmuxd(p) => p.get_pairing_file(),
            DeviceProvider::Tcp(p) => p.get_pairing_file(),
        }
    }
}

pub async fn usbmuxd() -> Result<UsbmuxdConnection> {
    UsbmuxdConnection::default().await.context(
        "Could not reach usbmuxd. On Windows install iTunes (the apple.com version, not the \
         Microsoft Store one); on Linux install and start usbmuxd.",
    )
}

fn usbmuxd_addr() -> Result<UsbmuxdAddr> {
    UsbmuxdAddr::from_env_var().context("Invalid USBMUXD_SOCKET_ADDRESS")
}

/// Find every reachable device. A device reachable several ways is listed once per transport,
/// best transport first.
pub async fn discover(paths: &Paths, options: &DiscoverOptions) -> Result<Vec<Device>> {
    let mut devices = Vec::new();

    let wants_usbmuxd = options.filter.allows(TransportKind::Usb)
        || options.filter.allows(TransportKind::UsbmuxdWifi);
    let wants_direct = options.filter.allows(TransportKind::DirectWifi);

    // usbmuxd may be missing entirely (Linux without usbmuxd, Windows without iTunes);
    // direct Wi-Fi still works in that case
    let usbmuxd_task = async {
        if !wants_usbmuxd {
            return Vec::new();
        }
        match list_usbmuxd(options.filter).await {
            Ok(d) => d,
            Err(e) => {
                debug!("usbmuxd unavailable: {e:#}");
                Vec::new()
            }
        }
    };
    let direct_task = async {
        if !wants_direct {
            return Vec::new();
        }
        match wifi::find_direct_devices(paths, options.wifi_timeout).await {
            Ok(d) => d,
            Err(e) => {
                debug!("Direct Wi-Fi discovery failed: {e:#}");
                Vec::new()
            }
        }
    };
    let (usb, direct) = tokio::join!(usbmuxd_task, direct_task);
    devices.extend(usb);
    devices.extend(direct);

    devices.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.udid.cmp(&b.udid))
            .then(a.transport.rank().cmp(&b.transport.rank()))
    });
    devices.dedup_by(|a, b| a.udid == b.udid && a.transport == b.transport);
    Ok(devices)
}

async fn list_usbmuxd(filter: TransportFilter) -> Result<Vec<Device>> {
    let mut mux = usbmuxd().await?;
    let addr = usbmuxd_addr()?;
    let raw = mux
        .get_devices()
        .await
        .context("Failed to list devices from usbmuxd")?;

    let mut out = Vec::new();
    for d in raw {
        if let Some(device) = from_usbmuxd(&d, &addr, filter).await {
            out.push(device);
        }
    }
    Ok(out)
}

/// Describe a usbmuxd device, or `None` if the filter excludes it.
pub(crate) async fn from_usbmuxd(
    d: &UsbmuxdDevice,
    addr: &UsbmuxdAddr,
    filter: TransportFilter,
) -> Option<Device> {
    let (transport, ip) = match d.connection_type {
        Connection::Usb => (TransportKind::Usb, None),
        Connection::Network(ip) => (TransportKind::UsbmuxdWifi, Some(ip)),
        Connection::Unknown(_) => return None,
    };
    if !filter.allows(transport) {
        return None;
    }
    let provider = d.to_provider(addr.clone(), LABEL);
    // Some lockdownd versions only answer over Wi-Fi inside a session
    let identity = match read_identity(&provider, false).await {
        Ok(id) => Ok(id),
        Err(_) => read_identity(&provider, true).await,
    };
    let (name, ios_version) = match identity {
        Ok((_, name, version)) => (name, Some(version)),
        Err(e) => {
            debug!("Could not read info for {}: {e:#}", d.udid);
            ("Unknown device".to_string(), None)
        }
    };
    Some(Device {
        udid: d.udid.clone(),
        name,
        ios_version,
        transport,
        ip,
        usbmuxd_id: Some(d.device_id),
    })
}

pub(crate) fn default_usbmuxd_addr() -> Result<UsbmuxdAddr> {
    usbmuxd_addr()
}

/// `(udid, name, iOS version)` straight from lockdownd. Over plain TCP lockdownd requires a TLS
/// session, so `with_session` must be set.
pub(crate) async fn read_identity(
    provider: &dyn IdeviceProvider,
    with_session: bool,
) -> Result<(String, String, String)> {
    let mut lockdown = LockdownClient::connect(provider).await?;
    if with_session {
        let pairing = provider.get_pairing_file().await?;
        lockdown.start_session(&pairing).await?;
    }
    let get = async |lockdown: &mut LockdownClient, key: &str| -> Result<String> {
        lockdown
            .get_value(Some(key), None)
            .await?
            .as_string()
            .map(str::to_string)
            .with_context(|| format!("{key} is not a string"))
    };
    let udid = get(&mut lockdown, "UniqueDeviceID").await?;
    let name = get(&mut lockdown, "DeviceName").await?;
    let version = get(&mut lockdown, "ProductVersion").await?;
    Ok((udid, name, version))
}

/// Build a direct Wi-Fi device entry for a known address. Used by `--ip`.
pub async fn direct_device(paths: &Paths, ip: IpAddr, pairing: PairingFile) -> Result<Device> {
    let provider = TcpProvider {
        addr: ip,
        scope_id: None,
        pairing_file: pairing.clone(),
        label: LABEL.to_string(),
    };
    let (udid, name, version) = tokio::time::timeout(
        Duration::from_secs(10),
        read_identity(&provider, true),
    )
    .await
    .with_context(|| format!("Timed out connecting to {ip}"))?
    .with_context(|| {
        format!("Could not connect to the device at {ip}. Is it on the same network and unlocked?")
    })?;
    wifi::remember_ip(paths, &udid, ip).ok();
    Ok(Device {
        udid,
        name,
        ios_version: Some(version),
        transport: TransportKind::DirectWifi,
        ip: Some(ip),
        usbmuxd_id: None,
    })
}

impl Device {
    pub(crate) fn direct(
        udid: String,
        name: String,
        ios_version: Option<String>,
        ip: IpAddr,
    ) -> Self {
        Self {
            udid,
            name,
            ios_version,
            transport: TransportKind::DirectWifi,
            ip: Some(ip),
            usbmuxd_id: None,
        }
    }

    /// Open a provider for this device.
    pub async fn connect(&self, paths: &Paths) -> Result<DeviceProvider> {
        match self.transport {
            TransportKind::Usb | TransportKind::UsbmuxdWifi => {
                let device_id = match self.usbmuxd_id {
                    Some(id) => id,
                    None => usbmuxd().await?.get_device(&self.udid).await?.device_id,
                };
                Ok(DeviceProvider::Usbmuxd(UsbmuxdProvider {
                    addr: usbmuxd_addr()?,
                    tag: 0,
                    udid: self.udid.clone(),
                    device_id,
                    label: LABEL.to_string(),
                }))
            }
            TransportKind::DirectWifi => {
                let ip = self.ip.context("Direct Wi-Fi device has no address")?;
                let pairing = wifi::load_pairing(paths, &self.udid)?;
                Ok(DeviceProvider::Tcp(TcpProvider {
                    addr: ip,
                    scope_id: None,
                    pairing_file: pairing,
                    label: LABEL.to_string(),
                }))
            }
        }
    }
}

/// Pick a device by UDID, name (case-insensitive), or, when there's only one, automatically.
/// Among several transports for the same device, the best one wins.
pub fn select<'a>(devices: &'a [Device], selector: Option<&str>) -> Result<&'a Device> {
    let candidates: Vec<&Device> = match selector {
        Some(sel) => devices
            .iter()
            .filter(|d| {
                d.udid.eq_ignore_ascii_case(sel)
                    || d.name.eq_ignore_ascii_case(sel)
                    || d.ip.is_some_and(|ip| ip.to_string() == sel)
            })
            .collect(),
        None => devices.iter().collect(),
    };
    if candidates.is_empty() {
        match selector {
            Some(sel) => bail!(
                "No device matches \"{sel}\". Run `sideward devices` to see what's connected."
            ),
            None => bail!(
                "No devices found. Plug in over USB and tap Trust, or run `sideward wifi setup` once over USB to enable Wi-Fi installs."
            ),
        }
    }
    let first_udid = &candidates[0].udid;
    if candidates.iter().any(|d| &d.udid != first_udid) {
        let names: Vec<String> = candidates.iter().map(|d| format!("  {d}")).collect();
        bail!(
            "Several devices found; pick one with --device <udid or name>:\n{}",
            names.join("\n")
        );
    }
    Ok(candidates
        .into_iter()
        .min_by_key(|d| d.transport.rank())
        .expect("candidates is not empty"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(udid: &str, name: &str, transport: TransportKind) -> Device {
        Device {
            udid: udid.into(),
            name: name.into(),
            ios_version: None,
            transport,
            ip: None,
            usbmuxd_id: None,
        }
    }

    #[test]
    fn select_prefers_usb_for_the_same_device() {
        let devices = vec![
            dev("A", "Phone", TransportKind::DirectWifi),
            dev("A", "Phone", TransportKind::Usb),
        ];
        assert_eq!(
            select(&devices, None).unwrap().transport,
            TransportKind::Usb
        );
        assert_eq!(
            select(&devices, Some("phone")).unwrap().transport,
            TransportKind::Usb
        );
    }

    #[test]
    fn select_requires_choice_between_devices() {
        let devices = vec![
            dev("A", "Phone", TransportKind::Usb),
            dev("B", "iPad", TransportKind::Usb),
        ];
        assert!(select(&devices, None).is_err());
        assert_eq!(select(&devices, Some("b")).unwrap().name, "iPad");
        assert!(select(&devices, Some("nope")).is_err());
    }
}
