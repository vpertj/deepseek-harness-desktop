mod app_update;
mod config;
mod envcheck;
mod kernel;
mod logfile;
mod plugin;
mod proxy;
mod settings;
mod terminal;
mod theme;
mod tray;
mod updater;

use kernel::KernelManager;

/// Remove Gatekeeper quarantine from our own .app bundle. The app is signed
/// ad-hoc (no paid Developer ID), so the first launch after download requires
/// a right-click → Open. Once it runs, clearing the quarantine flag makes
/// every later launch open directly with no prompts. Runs on a background
/// thread a few seconds after startup, because the bundle is briefly locked
/// while the process is still loading.
fn clear_own_quarantine() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let Ok(exe) = std::env::current_exe() else {
            eprintln!("[quarantine] current_exe failed");
            return;
        };
        let mut dir = exe.parent();
        while let Some(d) = dir {
            if d.extension().map(|e| e == "app").unwrap_or(false) {
                eprintln!("[quarantine] clearing on {}", d.display());
                let out = std::process::Command::new("xattr")
                    .args(["-dr", "com.apple.quarantine"])
                    .arg(d)
                    .output();
                eprintln!("[quarantine] xattr result: {out:?}");
                return;
            }
            dir = d.parent();
        }
        eprintln!("[quarantine] no .app dir found");
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    clear_own_quarantine();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(KernelManager::default())
        .on_window_event(|_window, event| {
            // Single-window app: closing the window means quitting. Kill the
            // kernel we spawned so no orphaned dsh web processes survive.
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                kernel::kill_stale_owned();
            }
        })
        .setup(|app| {
            // Clean up kernel processes orphaned by a previous (crash/force
            // quit) run before the auto-start kicks in.
            kernel::kill_stale_owned();
            theme::start_watcher(app.handle().clone());
            let _ = tray::setup_tray(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            kernel::kernel_status,
            kernel::pick_workspace_dir,
            kernel::kernel_set_dir,
            kernel::kernel_start,
            kernel::kernel_stop,
            kernel::kernel_profiles,
            kernel::kernel_add_profile,
            kernel::kernel_remove_profile,
            kernel::kernel_set_active,
            updater::update_check,
            updater::update_apply,
            updater::kernel_install,
            settings::get_settings,
            settings::set_auto_start,
            settings::set_persist_logs,
            theme::get_theme,
            envcheck::check_env,
            envcheck::install_env,
            envcheck::env_setup_auto,
            plugin::plugin_list,
            plugin::plugin_install,
            plugin::plugin_remove,
            app_update::app_update_check,
            app_update::app_download_update,
            terminal::terminal_open,
            terminal::terminal_write,
            terminal::terminal_resize,
            terminal::terminal_close,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // Exit fires right before the process dies regardless of how the
            // app was quit (red button, Cmd+Q, AppleEvent). Kill the kernels
            // we spawned so no orphaned dsh web processes survive.
            if matches!(event, tauri::RunEvent::Exit) {
                kernel::kill_stale_owned();
            }
        });
}
