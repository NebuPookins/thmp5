mod audio;
mod commands;
mod db;
pub mod fingerprint;
mod importer;
mod library;
mod logging;
mod models;

use audio::AudioEngineHandle;
use importer::ImportManager;
use sqlx::SqlitePool;
use tauri::Manager;

pub struct AppState {
    pub db: SqlitePool,
    /// AcoustID client API key, read from the `ACOUSTID_API_KEY` environment variable.
    /// AcoustID lookups are skipped when this is `None`.
    pub acoustid_api_key: Option<String>,
    pub importer: ImportManager,
    pub player: AudioEngineHandle,
    pub log_file_path: String,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let app_data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&app_data_dir)?;
            let db_path = app_data_dir.join("library.db");
            let log_path = app_data_dir.join("thmp5.log");

            logging::init(&log_path).map_err(|e| format!("Failed to initialize logging: {e}"))?;

            tracing::info!("Database path: {}", db_path.display());
            tracing::info!("Log path: {}", log_path.display());

            let pool = tauri::async_runtime::block_on(db::init_pool(&db_path))
                .map_err(|e| format!("Failed to initialize database: {e}"))?;

            let acoustid_api_key = std::env::var("ACOUSTID_API_KEY").ok();
            if acoustid_api_key.is_some() {
                tracing::info!("AcoustID API key loaded from environment");
            } else {
                tracing::info!("ACOUSTID_API_KEY not set — fingerprint lookups disabled");
            }
            let importer = ImportManager::new();
            let player = AudioEngineHandle::new(app.handle().clone())
                .map_err(|e| format!("Failed to initialize audio engine: {e}"))?;
            let state = AppState {
                db: pool.clone(),
                acoustid_api_key: acoustid_api_key.clone(),
                importer,
                player,
                log_file_path: log_path.display().to_string(),
            };

            if let Ok(Some(root_path)) =
                tauri::async_runtime::block_on(commands::load_music_root(&pool))
            {
                state
                    .importer
                    .spawn_scan(pool.clone(), root_path, acoustid_api_key.clone());
            }

            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::import_paths,
            commands::get_app_bootstrap,
            commands::complete_initial_setup,
            commands::get_import_progress,
            commands::trigger_library_scan,
            commands::update_queue_settings,
            commands::get_library_summary,
            commands::list_recordings,
            commands::list_artists,
            commands::list_release_groups,
            commands::record_play_history,
            commands::set_recording_rating,
            commands::get_player_state,
            commands::get_log_file_path,
            commands::play,
            commands::pause,
            commands::resume,
            commands::seek,
            commands::set_volume,
            commands::stop,
        ])
        .run(tauri::generate_context!())
        .expect("error while running thmp5");
}
