use crate::library::import::import_file;
use crate::library::scanner::is_audio_file;
use crate::models::ImportProgress;
use anyhow::Result;
use sqlx::SqlitePool;
use std::path::Path;
use std::sync::{Arc, Mutex};
use walkdir::WalkDir;

#[derive(Clone)]
pub struct ImportManager {
    progress: Arc<Mutex<ImportProgress>>,
}

impl ImportManager {
    pub fn new() -> Self {
        Self {
            progress: Arc::new(Mutex::new(ImportProgress::default())),
        }
    }

    pub fn snapshot(&self) -> ImportProgress {
        self.progress
            .lock()
            .expect("import progress mutex poisoned")
            .clone()
    }

    pub fn spawn_scan(&self, db: SqlitePool, root_path: String, acoustid_key: Option<String>) {
        {
            let mut progress = self
                .progress
                .lock()
                .expect("import progress mutex poisoned");
            if progress.is_running {
                return;
            }

            *progress = ImportProgress {
                is_running: true,
                root_path: Some(root_path.clone()),
                started_at: Some(now_iso()),
                ..ImportProgress::default()
            };
        }

        let progress = Arc::clone(&self.progress);
        tauri::async_runtime::spawn(async move {
            let result = run_scan(&db, &root_path, acoustid_key.as_deref(), &progress).await;
            let mut state = progress.lock().expect("import progress mutex poisoned");
            state.is_running = false;
            state.finished_at = Some(now_iso());
            if let Err(error) = result {
                state.errors += 1;
                state.error_messages.push(error.to_string());
            }
        });
    }
}

async fn run_scan(
    db: &SqlitePool,
    root_path: &str,
    acoustid_key: Option<&str>,
    progress: &Arc<Mutex<ImportProgress>>,
) -> Result<()> {
    let path = Path::new(root_path);
    if !path.exists() {
        anyhow::bail!("Import root does not exist: {}", path.display());
    }

    for entry in WalkDir::new(path).follow_links(true) {
        match entry {
            Ok(entry) if entry.file_type().is_file() && is_audio_file(entry.path()) => {
                {
                    let mut state = progress.lock().expect("import progress mutex poisoned");
                    state.scanned += 1;
                    state.current_path = Some(entry.path().display().to_string());
                }

                match import_file(db, entry.path(), acoustid_key).await {
                    Ok(true) => {
                        let mut state = progress.lock().expect("import progress mutex poisoned");
                        state.imported += 1;
                    }
                    Ok(false) => {
                        let mut state = progress.lock().expect("import progress mutex poisoned");
                        state.skipped += 1;
                    }
                    Err(error) => {
                        let mut state = progress.lock().expect("import progress mutex poisoned");
                        state.errors += 1;
                        state
                            .error_messages
                            .push(format!("{}: {}", entry.path().display(), error));
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                let mut state = progress.lock().expect("import progress mutex poisoned");
                state.errors += 1;
                state.error_messages.push(error.to_string());
            }
        }
    }

    let mut state = progress.lock().expect("import progress mutex poisoned");
    state.current_path = None;
    Ok(())
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    timestamp.to_string()
}
