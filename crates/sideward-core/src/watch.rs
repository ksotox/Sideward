//! Notices devices as they connect, so remembered apps can be re-signed right away
//! (like Sideloadly's AutoRefresh).
//!
//! - USB and usbmuxd Wi-Fi: usbmuxd's `Listen` pushes an event the moment a device attaches.
//!   It also reports every device that's already attached when listening starts.
//! - Direct Wi-Fi: saved Wi-Fi devices are looked for on the network every `wifi_interval`;
//!   a device that wasn't there on the previous scan counts as connected.

use std::{collections::HashSet, time::Duration};

use futures_util::StreamExt;
use idevice::usbmuxd::UsbmuxdListenEvent;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info};

use crate::{
    device::{self, Device, TransportFilter},
    paths::Paths,
    wifi,
};

/// How long to wait before retrying when usbmuxd isn't running (e.g. iTunes not installed yet).
const USBMUXD_RETRY: Duration = Duration::from_secs(20);

/// Start watching. Each newly connected device is sent on `connected`. Runs until the
/// receiver is dropped. Must be called from inside a tokio runtime.
pub fn spawn(paths: Paths, wifi_interval: Duration, connected: UnboundedSender<Device>) {
    let usb_tx = connected.clone();
    // usbmuxd's listen stream isn't `Send`, so it gets its own thread and runtime
    let spawned = std::thread::Builder::new()
        .name("sideward-usb-watch".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::warn!("USB watcher couldn't start: {e}");
                    return;
                }
            };
            runtime.block_on(watch_usbmuxd(usb_tx));
        });
    if let Err(e) = spawned {
        tracing::warn!("USB watcher couldn't start: {e}");
    }
    tokio::spawn(watch_wifi(paths, wifi_interval, connected));
}

async fn watch_usbmuxd(tx: UnboundedSender<Device>) {
    while !tx.is_closed() {
        if let Err(e) = listen_once(&tx).await {
            debug!("usbmuxd watch: {e:#}");
        }
        tokio::time::sleep(USBMUXD_RETRY).await;
    }
}

async fn listen_once(tx: &UnboundedSender<Device>) -> anyhow::Result<()> {
    let addr = device::default_usbmuxd_addr()?;
    let mut mux = device::usbmuxd().await?;
    let mut events = mux.listen().await?;
    info!("Watching for devices connecting over USB");
    while let Some(event) = events.next().await {
        let UsbmuxdListenEvent::Connected(raw) = event? else {
            continue;
        };
        // Right after plugging in, lockdownd may not answer yet; give it a few tries
        let mut found = None;
        for attempt in 0..4 {
            tokio::time::sleep(Duration::from_secs(if attempt == 0 { 2 } else { 4 })).await;
            match device::from_usbmuxd(&raw, &addr, TransportFilter::Any).await {
                Some(d) if d.ios_version.is_some() => {
                    found = Some(d);
                    break;
                }
                other => found = other,
            }
        }
        if let Some(d) = found {
            info!("Device connected: {d}");
            if tx.send(d).is_err() {
                return Ok(());
            }
        }
    }
    anyhow::bail!("usbmuxd closed the connection")
}

async fn watch_wifi(paths: Paths, interval: Duration, tx: UnboundedSender<Device>) {
    let mut present: HashSet<String> = HashSet::new();
    while !tx.is_closed() {
        match wifi::find_direct_devices(&paths, Duration::from_secs(3)).await {
            Ok(devices) => {
                let now: HashSet<String> = devices.iter().map(|d| d.udid.clone()).collect();
                for d in devices.into_iter().filter(|d| !present.contains(&d.udid)) {
                    info!("Device found on Wi-Fi: {d}");
                    if tx.send(d).is_err() {
                        return;
                    }
                }
                present = now;
            }
            Err(e) => debug!("Wi-Fi watch: {e:#}"),
        }
        tokio::time::sleep(interval).await;
    }
}
