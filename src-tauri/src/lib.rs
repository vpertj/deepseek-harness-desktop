mod config;
mod envcheck;
mod kernel;
mod logfile;
mod plugin;
mod settings;
mod theme;
mod updater;

use kernel::KernelManager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(KernelManager::default())
        .setup(|app| {
            eprintln!("[setup] begin");
            // Clean up kernel processes orphaned by a previous (crash/force
            // quit) run before the auto-start kicks in.
            kernel::kill_stale_owned();
            theme::start_watcher(app.handle().clone());
            eprintln!("[setup] done");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            kernel::kernel_status,
            kernel::kernel_set_dir,
            kernel::kernel_start,
            kernel::kernel_stop,
            updater::update_check,
            updater::update_apply,
            updater::kernel_install,
            settings::get_settings,
            settings::set_auto_start,
            settings::set_persist_logs,
            theme::get_theme,
            envcheck::check_env,
            envcheck::install_env,
            plugin::plugin_list,
            plugin::plugin_install,
            plugin::plugin_remove,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                // Stop the kernel we spawned (all DSH_DESKTOP_OWNED processes)
                // so quitting the app doesn't leave orphans behind.
                kernel::kill_stale_owned();
            }
        });
}
