pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            crate::commands::locale_messages,
            crate::commands::scan_selection,
            crate::commands::archive_info,
            crate::commands::pack,
            crate::commands::unpack,
            crate::commands::cancel
        ])
        .run(tauri::generate_context!())
        .expect("failed to launch spack");
}
