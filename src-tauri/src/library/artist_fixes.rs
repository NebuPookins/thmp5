use crate::library::scanner;
use crate::models::CompoundArtistCheck;
use crate::storage::Catalog;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

pub async fn check_artist_compound(
    catalog: &Arc<RwLock<Catalog>>,
    artist_id: &str,
) -> Result<CompoundArtistCheck, String> {
    let (artist_name_opt, paths) = {
        let cat = catalog.read().await;
        cat.source_paths_for_artist(artist_id)
    };

    let Some(artist_name) = artist_name_opt else {
        return Ok(CompoundArtistCheck {
            is_compound: false,
            evidence_count: 0,
            total_sources_checked: 0,
            individual_artist_names: vec![],
            source_examples: vec![],
        });
    };

    if paths.is_empty() {
        return Ok(CompoundArtistCheck {
            is_compound: false,
            evidence_count: 0,
            total_sources_checked: 0,
            individual_artist_names: vec![],
            source_examples: vec![],
        });
    }

    let mut individual_names: Vec<String> = Vec::new();
    let mut evidence_count = 0usize;

    for path_str in &paths {
        let path = Path::new(path_str);
        let (meta, _tags) = tokio::task::spawn_blocking({
            let owned = path.to_owned();
            move || {
                let meta = scanner::read_metadata(&owned).ok();
                let _tags = scanner::list_all_tags(&owned).ok();
                (meta, _tags)
            }
        })
        .await
        .map_err(|e| format!("Blocking task failed: {e}"))?;

        if let Some(meta) = meta {
            if let Some(artists) = &meta.meta.artists {
                let names: Vec<&str> = artists
                    .split(';')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                if names.len() >= 2 {
                    evidence_count += 1;
                    for name in names {
                        let n = name.to_string();
                        if !individual_names.contains(&n) {
                            individual_names.push(n);
                        }
                    }
                }
            }
        }
    }

    individual_names.retain(|n| n.to_lowercase() != artist_name.to_lowercase());

    let source_examples: Vec<String> = paths.iter().take(3).cloned().collect();

    Ok(CompoundArtistCheck {
        is_compound: evidence_count > 0 && !individual_names.is_empty(),
        evidence_count,
        total_sources_checked: paths.len(),
        individual_artist_names: individual_names,
        source_examples,
    })
}
