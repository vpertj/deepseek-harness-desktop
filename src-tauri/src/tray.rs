use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::kernel::{KernelManager, KernelStatus};

const MENU_OPEN: &str = "open";
const MENU_START: &str = "kernel_start";
const MENU_STOP: &str = "kernel_stop";
const MENU_CHECK: &str = "check_update";
const MENU_QUIT: &str = "quit";

/// App-managed handle to the tray menu (TrayIcon exposes no getter in tauri 2).
pub struct TrayMenu(pub Menu<tauri::Wry>);

/// Build the menu bar tray icon: left-click focuses the window, the menu
/// offers open / start / stop kernel / check update / quit. Menu labels
/// reflect the live kernel state (updated after each action).
pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, MENU_OPEN, "打开 DeepSeek Harness", true, None::<&str>)?;
    let start = MenuItem::with_id(app, MENU_START, "启动内核", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, MENU_STOP, "停止内核", false, None::<&str>)?;
    let check = MenuItem::with_id(app, MENU_CHECK, "检查内核更新", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &start, &stop, &check, &quit])?;
    app.manage(TrayMenu(menu.clone()));

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("app icon".into()))?;

    TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            MENU_OPEN => focus_window(app),
            MENU_START => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = app.try_state::<KernelManager>() {
                        let _ = state.start(&app).await;
                        refresh_menu(&app);
                    }
                });
            }
            MENU_STOP => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(state) = app.try_state::<KernelManager>() {
                        let _ = state.stop().await;
                        refresh_menu(&app);
                    }
                });
            }
            MENU_CHECK => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(manager) = app.try_state::<KernelManager>() {
                        let _ = crate::updater::check_update(&manager, &app).await;
                    }
                });
            }
            MENU_QUIT => {
                crate::kernel::kill_stale_owned();
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                focus_window(tray.app_handle());
            }
        })
        .build(app)?;

    refresh_menu(app);
    Ok(())
}

/// Focus (and un-minimize) the main window, creating it if needed.
fn focus_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// Sync the tray menu labels with the live kernel state.
pub fn refresh_menu(app: &AppHandle) {
    let Some(menu) = app.try_state::<TrayMenu>().map(|t| t.0.clone()) else {
        return;
    };
    let running = app
        .try_state::<KernelManager>()
        .and_then(|m| m.status_snapshot())
        .map(|s| matches!(s.status, KernelStatus::Running { .. }))
        .unwrap_or(false);
    use tauri::menu::MenuItemKind;
    if let Some(MenuItemKind::MenuItem(start)) = menu.get(MENU_START) {
        let _ = start.set_text(if running { "内核运行中" } else { "启动内核" });
        let _ = start.set_enabled(!running);
    }
    if let Some(MenuItemKind::MenuItem(stop)) = menu.get(MENU_STOP) {
        let _ = stop.set_enabled(running);
    }
}
