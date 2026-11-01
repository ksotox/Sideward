//! OS integration: launch at login, single instance, and the app icon.

use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream},
    time::Duration,
};

use anyhow::{Context, Result};

/// Passed by the login item so Sideward starts quietly in the background.
pub const BACKGROUND_ARG: &str = "--background";

/// Local port used to make sure only one Sideward runs (two copies would fight over devices).
const INSTANCE_PORT: u16 = 47_613;

fn auto_launch() -> Result<auto_launch::AutoLaunch> {
    let exe = std::env::current_exe().context("Couldn't find Sideward's own path")?;
    // Linux reports a replaced binary as "path (deleted)"
    let exe = exe
        .to_string_lossy()
        .trim_end_matches(" (deleted)")
        .to_string();
    // The Windows Run key holds a command line; quote the path in case it has spaces
    #[cfg(windows)]
    let exe = format!("\"{exe}\"");
    let mut builder = auto_launch::AutoLaunchBuilder::new();
    builder
        .set_app_name("Sideward")
        .set_app_path(&exe)
        .set_args(&[BACKGROUND_ARG])
        // Per-user: no admin prompt on Windows
        .set_windows_enable_mode(auto_launch::WindowsEnableMode::CurrentUser)
        .set_macos_launch_mode(auto_launch::MacOSLaunchMode::LaunchAgent);
    Ok(builder.build()?)
}

/// Start Sideward when the user signs in (Windows: the Run registry key; macOS: a launch
/// agent; Linux: an XDG autostart entry).
pub fn set_launch_at_login(enabled: bool) -> Result<()> {
    let launcher = auto_launch()?;
    if enabled {
        launcher.enable()?;
    } else if launcher.is_enabled().unwrap_or(false) {
        launcher.disable()?;
    }
    Ok(())
}

pub fn launch_at_login_enabled() -> bool {
    auto_launch()
        .and_then(|l| Ok(l.is_enabled()?))
        .unwrap_or(false)
}

pub enum Instance {
    /// This is the only copy; keep the listener to hear from later launches.
    First(TcpListener),
    /// Another copy is already running and was asked to show its window.
    AlreadyRunning,
}

pub fn single_instance() -> Instance {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, INSTANCE_PORT);
    match TcpListener::bind(addr) {
        Ok(listener) => Instance::First(listener),
        Err(_) => {
            let shown = TcpStream::connect_timeout(&addr.into(), Duration::from_secs(2))
                .and_then(|mut s| s.write_all(b"show\n"));
            match shown {
                Ok(()) => Instance::AlreadyRunning,
                // The port is taken by something else; run anyway rather than refuse to start
                Err(_) => match TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)) {
                    Ok(l) => Instance::First(l),
                    Err(_) => Instance::AlreadyRunning,
                },
            }
        }
    }
}

/// Call `on_show` whenever another launch asks this copy to show itself.
pub fn listen_for_launches(listener: TcpListener, on_show: impl Fn() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("sideward-instance".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                let _ = BufReader::new(stream).read_line(&mut line);
                if line.trim() == "show" {
                    on_show();
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("Couldn't listen for other launches: {e}");
    }
}

/// The Sideward icon (a rounded blue square with a down arrow), drawn so no image file is
/// needed. Returns RGBA pixels.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let radius = s * 0.22;
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded-square mask
            let dx = (fx - s / 2.0).abs() - (s / 2.0 - radius);
            let dy = (fy - s / 2.0).abs() - (s / 2.0 - radius);
            let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() - radius;
            let alpha = (0.5 - outside).clamp(0.0, 1.0);
            if alpha == 0.0 {
                continue;
            }
            // Arrow: a stem and a chevron pointing down
            let cx = fx - s / 2.0;
            let stem = cx.abs() < s * 0.07 && fy > s * 0.2 && fy < s * 0.62;
            let head_y = fy - s * 0.42;
            let head = head_y > 0.0
                && head_y < s * 0.3
                && cx.abs() < (s * 0.3 - head_y)
                && cx.abs() > (s * 0.3 - head_y) - s * 0.14;
            let base = fy > s * 0.76 && fy < s * 0.84 && cx.abs() < s * 0.3;
            let (r, g, b) = if stem || head || base {
                (255, 255, 255)
            } else {
                // Vertical gradient
                let t = fy / s;
                (
                    (40.0 + 20.0 * t) as u8,
                    (120.0 - 30.0 * t) as u8,
                    (235.0 - 25.0 * t) as u8,
                )
            };
            let i = ((y * size + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&[r, g, b, (alpha * 255.0) as u8]);
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_expected_size_and_transparent_corners() {
        let rgba = icon_rgba(32);
        assert_eq!(rgba.len(), 32 * 32 * 4);
        assert_eq!(rgba[3], 0, "top-left corner is transparent");
        let center = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(rgba[center + 3], 255);
    }
}
