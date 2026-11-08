pub mod memory;

use crate::db::DbPool;
use crate::file_issues::FileIssue;
use crate::models::{
    ArtistDetail, ArtistRow, LibrarySummary, RecordingDetail, RecordingRow, ReleaseGroupDetail,
    ReleaseGroupRow, SmartPlaylistResult,
};
use anyhow::Result;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock, RwLockReadGuard};

pub trait CatalogReader: Send + Sync {
    fn list_recordings(&self) -> impl Future<Output = Result<Vec<RecordingRow>>> + Send;
    /// Returns at most `limit` recordings starting at `offset` in catalog order.
    fn list_recordings_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<RecordingRow>>> + Send;
    fn list_artists(&self) -> impl Future<Output = Result<Vec<ArtistRow>>> + Send;
    fn list_release_groups(
        &self,
        artist_id: Option<&str>,
        search: Option<&str>,
    ) -> impl Future<Output = Result<Vec<ReleaseGroupRow>>> + Send;
    fn get_library_summary(&self) -> impl Future<Output = Result<LibrarySummary>> + Send;
    fn list_all_tags(&self) -> impl Future<Output = Result<Vec<String>>> + Send;
    fn evaluate_smart_playlist(
        &self,
        query: &str,
    ) -> impl Future<Output = Result<SmartPlaylistResult>> + Send;
    fn get_artist_detail(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<ArtistDetail>>> + Send;
    fn get_release_group_detail(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<ReleaseGroupDetail>>> + Send;
    fn get_recording_detail(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<RecordingDetail>>> + Send;
}

/// Entity counts of a loaded catalog, logged by the memory diagnostics.
#[derive(Debug)]
pub struct CatalogStats {
    pub summary: LibrarySummary,
    pub playlists: usize,
    /// Issues derived from source tags (e.g. duplicate frames).
    pub file_issues: usize,
}

impl fmt::Display for CatalogStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "sources={} recordings={} artists={} release_groups={} playlists={} tag_file_issues={}",
            self.summary.source_count,
            self.summary.recording_count,
            self.summary.artist_count,
            self.summary.release_group_count,
            self.playlists,
            self.file_issues
        )
    }
}

pub enum Catalog {
    Memory(Box<memory::MemoryCatalog>),
}

impl Catalog {
    /// Generation number of the loaded catalog; changes on every reload.
    pub fn generation(&self) -> u64 {
        match self {
            Catalog::Memory(c) => c.generation(),
        }
    }

    pub fn source_paths_for_artist(&self, artist_id: &str) -> (Option<String>, Vec<String>) {
        match self {
            Catalog::Memory(c) => c.source_paths_for_artist(artist_id),
        }
    }

    pub fn source_paths_for_release_group(&self, rg_id: &str) -> Vec<String> {
        match self {
            Catalog::Memory(c) => c.source_paths_for_release_group(rg_id),
        }
    }

    pub fn stats(&self) -> CatalogStats {
        match self {
            Catalog::Memory(c) => c.stats(),
        }
    }

    pub fn file_issues(&self) -> Vec<FileIssue> {
        match self {
            Catalog::Memory(c) => c.file_issues().to_vec(),
        }
    }

    pub fn recording_id_for_source(&self, source_id: &str) -> Option<String> {
        match self {
            Catalog::Memory(c) => c.recording_id_for_source(source_id),
        }
    }

    pub fn effective_rating_lookup(&self) -> impl Fn(&str) -> Option<f64> + '_ {
        match self {
            Catalog::Memory(c) => c.effective_rating_lookup(),
        }
    }
}

/// Builds the in-memory catalog from the database, logging its size and
/// build time for memory diagnostics.
pub async fn load(db: &DbPool) -> Result<Catalog> {
    let started = Instant::now();
    let mem = memory::MemoryCatalog::load_from_db(db).await?;
    crate::memory_diagnostics::record_catalog_load(&mem.stats(), started.elapsed());
    Ok(Catalog::Memory(Box::new(mem)))
}

/// Told about every announced catalog swap, right after the new catalog is readable.
/// Implementations must handle (and log) their own delivery failures: a reload
/// has already happened by the time this is called, so it cannot fail.
pub trait ReloadNotifier: Send + Sync {
    fn catalog_reloaded(&self, generation: u64);
}

/// Whether a catalog reload is announced to the listener (the frontend).
/// Callers must choose explicitly, so a reload can't go unannounced by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Announce {
    /// Notify the listener, which refreshes everything derived from the catalog.
    Frontend,
    /// Don't notify: only for callers whose response already carries everything
    /// the frontend needs from this change. The new catalog is still readable
    /// and still has a new generation.
    Quiet,
}

/// The shared in-memory catalog. It is the only way to replace the catalog,
/// and every replacement states whether it is announced (see [`Announce`]).
#[derive(Clone)]
pub struct CatalogStore {
    catalog: Arc<RwLock<Catalog>>,
    notifier: Arc<dyn ReloadNotifier>,
    /// Held across a whole reload so reloads are strictly ordered: a reload
    /// that reads the database later always gets the larger generation and
    /// is the one that ends up in place.
    reload_order: Arc<Mutex<()>>,
}

impl CatalogStore {
    pub fn new(initial: Catalog, notifier: Arc<dyn ReloadNotifier>) -> Self {
        Self {
            catalog: Arc::new(RwLock::new(initial)),
            notifier,
            reload_order: Arc::new(Mutex::new(())),
        }
    }

    pub async fn read(&self) -> RwLockReadGuard<'_, Catalog> {
        self.catalog.read().await
    }

    /// Rebuilds the catalog from `db` and swaps it in. With
    /// [`Announce::Frontend`] the listener is then told its generation. Then
    /// logs a memory snapshot in the background. Concurrent reloads run one
    /// after another.
    pub async fn reload(&self, db: &DbPool, announce: Announce) -> Result<()> {
        let _ordered = self.reload_order.lock().await;
        let fresh = load(db).await?;
        let generation = fresh.generation();
        *self.catalog.write().await = fresh;
        match announce {
            Announce::Frontend => self.notifier.catalog_reloaded(generation),
            Announce::Quiet => {
                tracing::debug!(generation, "Catalog reloaded quietly (not announced)")
            }
        }
        tokio::task::spawn_blocking(|| crate::memory_diagnostics::log_snapshot("catalog_reload"));
        Ok(())
    }
}

/// Payload of the `catalog-reloaded` Tauri event.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CatalogReloaded {
    pub generation: u64,
}

/// Announces reloads to the frontend as `catalog-reloaded` events.
pub struct TauriReloadNotifier {
    app: tauri::AppHandle,
}

impl TauriReloadNotifier {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self { app }
    }
}

impl ReloadNotifier for TauriReloadNotifier {
    fn catalog_reloaded(&self, generation: u64) {
        use tauri::Emitter;
        if let Err(e) = self
            .app
            .emit("catalog-reloaded", CatalogReloaded { generation })
        {
            tracing::error!(generation, "Failed to emit catalog-reloaded event: {e:#}");
        }
    }
}

impl CatalogReader for Catalog {
    async fn list_recordings(&self) -> Result<Vec<RecordingRow>> {
        match self {
            Catalog::Memory(c) => c.list_recordings().await,
        }
    }

    async fn list_recordings_page(&self, offset: usize, limit: usize) -> Result<Vec<RecordingRow>> {
        match self {
            Catalog::Memory(c) => c.list_recordings_page(offset, limit).await,
        }
    }

    async fn list_artists(&self) -> Result<Vec<ArtistRow>> {
        match self {
            Catalog::Memory(c) => c.list_artists().await,
        }
    }

    async fn list_release_groups(
        &self,
        artist_id: Option<&str>,
        search: Option<&str>,
    ) -> Result<Vec<ReleaseGroupRow>> {
        match self {
            Catalog::Memory(c) => c.list_release_groups(artist_id, search).await,
        }
    }

    async fn get_library_summary(&self) -> Result<LibrarySummary> {
        match self {
            Catalog::Memory(c) => c.get_library_summary().await,
        }
    }

    async fn list_all_tags(&self) -> Result<Vec<String>> {
        match self {
            Catalog::Memory(c) => c.list_all_tags().await,
        }
    }

    async fn evaluate_smart_playlist(&self, query: &str) -> Result<SmartPlaylistResult> {
        match self {
            Catalog::Memory(c) => c.evaluate_smart_playlist(query).await,
        }
    }

    async fn get_artist_detail(&self, id: &str) -> Result<Option<ArtistDetail>> {
        match self {
            Catalog::Memory(c) => c.get_artist_detail(id).await,
        }
    }

    async fn get_release_group_detail(&self, id: &str) -> Result<Option<ReleaseGroupDetail>> {
        match self {
            Catalog::Memory(c) => c.get_release_group_detail(id).await,
        }
    }

    async fn get_recording_detail(&self, id: &str) -> Result<Option<RecordingDetail>> {
        match self {
            Catalog::Memory(c) => c.get_recording_detail(id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_pool;
    use crate::library::import::tests::import_titles;
    use tempfile::TempDir;

    /// Records every generation it is told about.
    #[derive(Default)]
    struct RecordingNotifier(std::sync::Mutex<Vec<u64>>);

    impl RecordingNotifier {
        fn heard(&self) -> Vec<u64> {
            self.0.lock().unwrap().clone()
        }
    }

    impl ReloadNotifier for RecordingNotifier {
        fn catalog_reloaded(&self, generation: u64) {
            self.0.lock().unwrap().push(generation);
        }
    }

    async fn store_with_one_recording(
        tmp: &TempDir,
    ) -> (
        crate::db::DbPool,
        tokio::sync::Semaphore,
        CatalogStore,
        Arc<RecordingNotifier>,
    ) {
        let pool = init_pool(&tmp.path().join("test.db")).await.unwrap();
        let serializer = tokio::sync::Semaphore::new(1);
        import_titles(tmp, &pool, &serializer, &["a"]).await;
        let notifier = Arc::new(RecordingNotifier::default());
        let store = CatalogStore::new(load(&pool).await.unwrap(), notifier.clone());
        (pool, serializer, store, notifier)
    }

    #[tokio::test]
    async fn test_announced_reload_notifies_with_the_generation_now_readable() {
        let tmp = TempDir::new().unwrap();
        let (pool, serializer, store, notifier) = store_with_one_recording(&tmp).await;
        let initial = store.read().await.generation();

        // Even a reload with identical data must be distinguishable.
        store.reload(&pool, Announce::Frontend).await.unwrap();
        let after_same = store.read().await.generation();
        import_titles(&tmp, &pool, &serializer, &["b"]).await;
        store.reload(&pool, Announce::Frontend).await.unwrap();
        let after_change = store.read().await.generation();

        assert_ne!(initial, after_same);
        assert_ne!(after_same, after_change);
        assert_ne!(initial, after_change);
        assert_eq!(notifier.heard(), vec![after_same, after_change]);
    }

    #[tokio::test]
    async fn test_quiet_reload_does_not_notify_but_new_data_is_readable() {
        let tmp = TempDir::new().unwrap();
        let (pool, serializer, store, notifier) = store_with_one_recording(&tmp).await;
        let initial = store.read().await.generation();
        import_titles(&tmp, &pool, &serializer, &["b"]).await;

        store.reload(&pool, Announce::Quiet).await.unwrap();

        let catalog = store.read().await;
        assert_ne!(catalog.generation(), initial);
        assert_eq!(catalog.list_recordings().await.unwrap().len(), 2);
        assert!(notifier.heard().is_empty());
    }

    #[tokio::test]
    async fn test_concurrent_reloads_leave_the_newest_generation_in_place() {
        let tmp = TempDir::new().unwrap();
        let (pool, _serializer, store, notifier) = store_with_one_recording(&tmp).await;

        let (first, second) = tokio::join!(
            store.reload(&pool, Announce::Frontend),
            store.reload(&pool, Announce::Frontend),
        );
        first.unwrap();
        second.unwrap();

        let heard = notifier.heard();
        assert_eq!(heard.len(), 2);
        assert!(heard[0] < heard[1], "announced out of order: {heard:?}");
        assert_eq!(store.read().await.generation(), heard[1]);
    }
}
