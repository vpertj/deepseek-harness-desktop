mod config;
mod envcheck;
mod kernel;
mod settings;
mod theme;
mod updater;

use kernel::KernelManager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(KernelManager::default())
        .setup(|app| {
            theme::start_watcher(app.handle().clone());
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
