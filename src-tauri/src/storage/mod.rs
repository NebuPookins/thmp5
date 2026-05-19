pub mod sql;

use crate::models::{
    ArtistDetail, ArtistRow, LibrarySummary, RecordingDetail, RecordingRow, ReleaseGroupDetail,
    ReleaseGroupRow, SmartPlaylistResult,
};
use anyhow::Result;
use std::future::Future;

pub trait CatalogReader: Send + Sync {
    fn list_recordings(&self) -> impl Future<Output = Result<Vec<RecordingRow>>> + Send;
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

/// Runtime-selectable catalog backend.
///
/// Currently only `Sql` exists; `Memory` will be added in a later PR when the
/// in-memory catalog is built from `raw_id3_json`.
pub enum Catalog {
    Sql(sql::SqlCatalog),
}

impl CatalogReader for Catalog {
    async fn list_recordings(&self) -> Result<Vec<RecordingRow>> {
        match self {
            Catalog::Sql(c) => c.list_recordings().await,
        }
    }

    async fn list_artists(&self) -> Result<Vec<ArtistRow>> {
        match self {
            Catalog::Sql(c) => c.list_artists().await,
        }
    }

    async fn list_release_groups(
        &self,
        artist_id: Option<&str>,
        search: Option<&str>,
    ) -> Result<Vec<ReleaseGroupRow>> {
        match self {
            Catalog::Sql(c) => c.list_release_groups(artist_id, search).await,
        }
    }

    async fn get_library_summary(&self) -> Result<LibrarySummary> {
        match self {
            Catalog::Sql(c) => c.get_library_summary().await,
        }
    }

    async fn list_all_tags(&self) -> Result<Vec<String>> {
        match self {
            Catalog::Sql(c) => c.list_all_tags().await,
        }
    }

    async fn evaluate_smart_playlist(&self, query: &str) -> Result<SmartPlaylistResult> {
        match self {
            Catalog::Sql(c) => c.evaluate_smart_playlist(query).await,
        }
    }

    async fn get_artist_detail(&self, id: &str) -> Result<Option<ArtistDetail>> {
        match self {
            Catalog::Sql(c) => c.get_artist_detail(id).await,
        }
    }

    async fn get_release_group_detail(&self, id: &str) -> Result<Option<ReleaseGroupDetail>> {
        match self {
            Catalog::Sql(c) => c.get_release_group_detail(id).await,
        }
    }

    async fn get_recording_detail(&self, id: &str) -> Result<Option<RecordingDetail>> {
        match self {
            Catalog::Sql(c) => c.get_recording_detail(id).await,
        }
    }
}
