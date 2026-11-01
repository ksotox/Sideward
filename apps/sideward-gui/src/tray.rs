//! System tray icon (Windows and macOS), so Sideward can keep running with its window closed
//! and re-sign apps when devices connect.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use eframe::egui;
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::worker::Cmd;

pub fn create(
    ctx: &egui::Context,
    cmd: UnboundedSender<Cmd>,
    quitting: Arc<AtomicBool>,
) -> anyhow::Result<TrayIcon> {
    let open = MenuItem::new("Open Sideward", true, None);
    let refresh = MenuItem::new("Re-sign apps now", true, None);
    let quit = MenuItem::new("Quit Sideward", true, None);
    let menu = Menu::new();
    menu.append(&open)?;
    menu.append(&refresh)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit)?;

    let (open_id, refresh_id, quit_id) =
        (open.id().clone(), refresh.id().clone(), quit.id().clone());
    let menu_ctx = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == open_id {
            crate::app::show_window(&menu_ctx);
        } else if event.id == refresh_id {
            let _ = cmd.send(Cmd::Refresh { force: false });
        } else if event.id == quit_id {
            quitting.store(true, Ordering::Relaxed);
            menu_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            menu_ctx.request_repaint();
        }
    }));

    let click_ctx = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } = event
        {
            crate::app::show_window(&click_ctx);
        }
    }));

    let icon = Icon::from_rgba(crate::system::icon_rgba(32), 32, 32)?;
    Ok(TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Sideward")
        .with_icon(icon)
        .with_menu_on_left_click(false)
        .build()?)
}
