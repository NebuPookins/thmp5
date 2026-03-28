use crate::file_issues::FileIssueLog;
use crate::library::import::{prepare_import, store_prepared_import};
use crate::library::scanner::is_audio_file;
use crate::models::ImportProgress;
use anyhow::Result;
use sqlx::SqlitePool;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::task::JoinSet;
use walkdir::WalkDir;

#[derive(Clone)]
pub struct ImportManager {
    progress: Arc<Mutex<ImportProgress>>,
    file_issues: FileIssueLog,
}

impl ImportManager {
    pub fn new(file_issues: FileIssueLog) -> Self {
        Self {
            progress: Arc::new(Mutex::new(ImportProgress::default())),
            file_issues,
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
        let file_issues = self.file_issues.clone();
        tauri::async_runtime::spawn(async move {
            let result =
                run_scan(&db, &root_path, acoustid_key.as_deref(), &progress, &file_issues).await;
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
    file_issues: &FileIssueLog,
) -> Result<()> {
    let concurrency = import_concurrency();
    let path = Path::new(root_path);
    if !path.exists() {
        anyhow::bail!("Import root does not exist: {}", path.display());
    }

    let mut tasks = JoinSet::new();
    for entry in WalkDir::new(path).follow_links(true) {
        match entry {
            Ok(entry) if entry.file_type().is_file() && is_audio_file(entry.path()) => {
                {
                    let mut state = progress.lock().expect("import progress mutex poisoned");
                    state.scanned += 1;
                    state.current_path = Some(entry.path().display().to_string());
                }

                if tasks.len() >= concurrency {
                    if let Some(result) = tasks.join_next().await {
                        handle_task_result(db, progress, file_issues, result).await;
                    }
                }

                let db = db.clone();
                let path = entry.path().to_path_buf();
                let acoustid_key = acoustid_key.map(ToOwned::to_owned);
                let progress = Arc::clone(progress);
                tasks.spawn(async move {
                    progress
                        .lock()
                        .expect("import progress mutex poisoned")
                        .fingerprinting_count += 1;
                    let prepared = prepare_import(&db, &path, acoustid_key.as_deref()).await;
                    let mut state = progress.lock().expect("import progress mutex poisoned");
                    state.fingerprinting_count = state.fingerprinting_count.saturating_sub(1);
                    (path, prepared)
                });
            }
            Ok(_) => {}
            Err(error) => {
                let mut state = progress.lock().expect("import progress mutex poisoned");
                state.errors += 1;
                let msg = error.to_string();
                if let Some(p) = error.path() {
                    file_issues.push_import_error(p.to_string_lossy(), msg.clone());
                }
                state.error_messages.push(msg);
            }
        }
    }

    while let Some(result) = tasks.join_next().await {
        handle_task_result(db, progress, file_issues, result).await;
    }

    let mut state = progress.lock().expect("import progress mutex poisoned");
    state.current_path = None;
    Ok(())
}

async fn handle_task_result(
    db: &SqlitePool,
    progress: &Arc<Mutex<ImportProgress>>,
    file_issues: &FileIssueLog,
    result: std::result::Result<
        (
            std::path::PathBuf,
            Result<Option<crate::library::import::PreparedImport>>,
        ),
        tokio::task::JoinError,
    >,
) {
    match result {
        Ok((path, Ok(Some(prepared)))) => match store_prepared_import(db, prepared).await {
            Ok(true) => {
                let mut state = progress.lock().expect("import progress mutex poisoned");
                state.imported += 1;
            }
            Ok(false) => {
                let mut state = progress.lock().expect("import progress mutex poisoned");
                state.skipped += 1;
            }
            Err(error) => {
                let detail = format!("{:#}", error);
                let msg = format!("{}: {}", path.display(), detail);
                file_issues.push_import_error(path.to_string_lossy(), detail);
                let mut state = progress.lock().expect("import progress mutex poisoned");
                state.errors += 1;
                state.error_messages.push(msg);
            }
        },
        Ok((_, Ok(None))) => {
            let mut state = progress.lock().expect("import progress mutex poisoned");
            state.skipped += 1;
        }
        Ok((path, Err(error))) => {
            let detail = format!("{:#}", error);
            let msg = format!("{}: {}", path.display(), detail);
            file_issues.push_import_error(path.to_string_lossy(), detail);
            let mut state = progress.lock().expect("import progress mutex poisoned");
            state.errors += 1;
            state.error_messages.push(msg);
        }
        Err(error) => {
            let mut state = progress.lock().expect("import progress mutex poisoned");
            state.errors += 1;
            state
                .error_messages
                .push(format!("Importer task failed: {error}"));
        }
    }
}

fn import_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get().min(6).max(2))
        .unwrap_or(4)
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    timestamp.to_string()
}
