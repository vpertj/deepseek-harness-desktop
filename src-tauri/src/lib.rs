mod config;
mod kernel;
mod updater;

use kernel::KernelManager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(KernelManager::default())
        .invoke_handler(tauri::generate_handler![
            kernel::kernel_status,
            kernel::kernel_set_dir,
            kernel::kernel_start,
            kernel::kernel_stop,
            updater::update_check,
            updater::update_apply,
            updater::kernel_install,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
