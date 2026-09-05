use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait,
    FromQueryResult, IntoActiveModel, QueryFilter, QueryOrder, QuerySelect, QueryTrait, Set,
    TransactionTrait,
    sea_query::{Expr, OnConflict},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    artist_credit,
    entities::{
        album, artist, bookmark, favorite, music_folder, play_queue, playlist_track, rating,
        scrobble, share, track, track_artist, user_track_stat,
    },
    models::MusicFolder,
    tags::{AUDIO_EXTENSIONS, EMBEDDED_ARTWORK_MARKER, MISSING_ARTWORK_MARKER, TagService},
};

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanReport {
    pub discovered: usize,
    pub indexed: usize,
    pub failed: usize,
    pub removed: u64,
}

pub async fn scan_all(
    db: &DatabaseConnection,
    tags: Arc<TagService>,
    shutdown: &CancellationToken,
    mut progress: impl FnMut(f64, &str) + Send,
) -> anyhow::Result<ScanReport> {
    ensure_running(shutdown)?;
    let folders = music_folder::Entity::find()
        .filter(music_folder::Column::Enabled.eq(1))
        .all(db)
        .await?;
    let mut files = Vec::new();
    let mut failed = 0usize;
    let mut fully_scanned_folders = HashSet::new();
    for folder in &folders {
        ensure_running(shutdown)?;
        let root = Path::new(&folder.path);
        if !root.is_dir() {
            tracing::warn!(folder_id = %folder.id, path = %root.display(), "library folder is unavailable; keeping its existing index");
            failed += 1;
            continue;
        }
        let mut complete = true;
        for entry in WalkDir::new(root).follow_links(false) {
            ensure_running(shutdown)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(folder_id = %folder.id, %error, "failed to traverse library folder; keeping its existing index");
                    failed += 1;
                    complete = false;
                    continue;
                }
            };
            if entry.file_type().is_file() && is_audio(entry.path()) {
                files.push((folder.clone(), entry.path().to_path_buf()));
            }
        }
        if complete {
            fully_scanned_folders.insert(folder.id.clone());
        }
    }
    let discovered = files.len();
    let discovered_paths = files
        .iter()
        .map(|(_, path)| path.to_string_lossy().into_owned())
        .collect::<HashSet<_>>();
    let mut indexed = 0usize;
    let scan_started = Utc::now().to_rfc3339();

    for (index, (folder, path)) in files.into_iter().enumerate() {
        ensure_running(shutdown)?;
        let tag_service = tags.clone();
        let read_path = path.clone();
        let result =
            tokio::task::spawn_blocking(move || tag_service.read_without_artwork(&read_path))
                .await?;
        ensure_running(shutdown)?;
        match result {
            Ok(metadata) => {
                index_track(db, &folder, &path, metadata, &scan_started).await?;
                indexed += 1;
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to index audio file");
                failed += 1;
            }
        }
        progress(
            (index + 1) as f64 / discovered.max(1) as f64,
            &path.to_string_lossy(),
        );
    }

    ensure_running(shutdown)?;
    let removed_ids = if fully_scanned_folders.is_empty() {
        Vec::new()
    } else {
        track::Entity::find()
            .filter(track::Column::FolderId.is_in(fully_scanned_folders))
            .all(db)
            .await?
            .into_iter()
            .filter(|track| !discovered_paths.contains(&track.path))
            .map(|track| track.id)
            .collect::<Vec<_>>()
    };
    if let Err(error) = tags.clear_artwork_caches(removed_ids.iter().map(String::as_str)) {
        tracing::warn!(%error, removed = removed_ids.len(), "failed to clear artwork cache for missing tracks");
    }
    let removed = remove_track_records(db, &removed_ids).await?;
    rebuild_aggregates(db).await?;
    Ok(ScanReport {
        discovered,
        indexed,
        failed,
        removed,
    })
}

/// Re-index files that were changed through the tag editor without walking the whole library.
pub async fn refresh_paths(
    db: &DatabaseConnection,
    tags: Arc<TagService>,
    paths: &[PathBuf],
) -> anyhow::Result<usize> {
    if paths.is_empty() {
        return Ok(0);
    }
    let folders = music_folder::Entity::find()
        .filter(music_folder::Column::Enabled.eq(1))
        .all(db)
        .await?;
    let scan_time = Utc::now().to_rfc3339();
    let mut indexed = 0;
    for path in paths {
        if !path.is_file() || !is_audio(path) {
            continue;
        }
        let Some(folder) = folders
            .iter()
            .filter(|folder| path.starts_with(Path::new(&folder.path)))
            .max_by_key(|folder| folder.path.len())
        else {
            tracing::warn!(path = %path.display(), "updated audio file is outside enabled libraries");
            continue;
        };
        let tag_service = tags.clone();
        let read_path = path.clone();
        let metadata =
            tokio::task::spawn_blocking(move || tag_service.read_without_artwork(&read_path))
                .await??;
        index_track(db, folder, path, metadata, &scan_time).await?;
        indexed += 1;
    }
    if indexed > 0 {
        rebuild_aggregates(db).await?;
    }
    Ok(indexed)
}

pub async fn refresh_path_changes(
    db: &DatabaseConnection,
    tags: Arc<TagService>,
    changes: &[(PathBuf, PathBuf)],
) -> anyhow::Result<usize> {
    let transaction = db.begin().await?;
    for (previous, current) in changes {
        if previous == current {
            continue;
        }
        if let Some(existing) = track::Entity::find()
            .filter(track::Column::Path.eq(previous.to_string_lossy().into_owned()))
            .one(&transaction)
            .await?
        {
            let mut active = existing.into_active_model();
            active.path = Set(current.to_string_lossy().into_owned());
            active.update(&transaction).await?;
        }
    }
    transaction.commit().await?;
    let paths = changes
        .iter()
        .map(|(_, current)| current.clone())
        .collect::<Vec<_>>();
    refresh_paths(db, tags, &paths).await
}

pub async fn remember_artwork_statuses(
    db: &DatabaseConnection,
    statuses: &[(PathBuf, bool)],
) -> anyhow::Result<u64> {
    let transaction = db.begin().await?;
    let mut updated = 0;
    let affected_paths = statuses
        .iter()
        .map(|(path, _)| path.to_string_lossy().into_owned())
        .collect::<HashSet<_>>();
    for (has_artwork, marker) in [
        (false, MISSING_ARTWORK_MARKER),
        (true, EMBEDDED_ARTWORK_MARKER),
    ] {
        let paths = statuses
            .iter()
            .filter(|(_, present)| *present == has_artwork)
            .map(|(path, _)| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        if paths.is_empty() {
            continue;
        }
        updated += track::Entity::update_many()
            .col_expr(track::Column::CoverPath, Expr::value(marker))
            .filter(track::Column::Path.is_in(paths))
            .exec(&transaction)
            .await?
            .rows_affected;
    }

    let affected_album_ids = track::Entity::find()
        .filter(track::Column::Path.is_in(affected_paths))
        .all(&transaction)
        .await?
        .into_iter()
        .filter_map(|track| track.album_id)
        .collect::<HashSet<_>>();
    if !affected_album_ids.is_empty() {
        let album_tracks = track::Entity::find()
            .filter(track::Column::AlbumId.is_in(affected_album_ids))
            .all(&transaction)
            .await?;
        let mut statuses_by_album = HashMap::<String, Vec<Option<String>>>::new();
        for track in album_tracks {
            if let Some(album_id) = track.album_id {
                statuses_by_album
                    .entry(album_id)
                    .or_default()
                    .push(track.cover_path);
            }
        }
        let mut embedded = Vec::new();
        let mut missing = Vec::new();
        let mut unknown = Vec::new();
        for (album_id, statuses) in statuses_by_album {
            if statuses
                .iter()
                .any(|status| status.as_deref().is_some_and(|value| !value.is_empty()))
            {
                embedded.push(album_id);
            } else if statuses
                .iter()
                .all(|status| status.as_deref() == Some(MISSING_ARTWORK_MARKER))
            {
                missing.push(album_id);
            } else {
                unknown.push(album_id);
            }
        }
        for (album_ids, marker) in [
            (embedded, Some(EMBEDDED_ARTWORK_MARKER)),
            (missing, Some(MISSING_ARTWORK_MARKER)),
            (unknown, None),
        ] {
            if album_ids.is_empty() {
                continue;
            }
            album::Entity::update_many()
                .col_expr(
                    album::Column::CoverPath,
                    Expr::value(marker.map(str::to_owned)),
                )
                .filter(album::Column::Id.is_in(album_ids))
                .exec(&transaction)
                .await?;
        }
    }
    transaction.commit().await?;
    Ok(updated)
}

fn ensure_running(shutdown: &CancellationToken) -> anyhow::Result<()> {
    anyhow::ensure!(!shutdown.is_cancelled(), "shutdown requested");
    Ok(())
}

async fn index_track(
    db: &DatabaseConnection,
    folder: &MusicFolder,
    path: &Path,
    metadata: crate::tags::AudioMetadata,
    scan_time: &str,
) -> anyhow::Result<()> {
    let artist_names = artist_credit::parse_artist_names(&metadata.artist);
    let artist_name = artist_names.join("; ");
    let artist_credits = artist_credit::resolve_artist_credits(db, &artist_names).await?;
    let primary_artist = &artist_credits[0];
    let artists_json = serde_json::to_string(&artist_credits)?;
    let album_id = if metadata.album.trim().is_empty() {
        None
    } else {
        Some(
            get_or_create_album(
                db,
                metadata.album.trim(),
                &primary_artist.id,
                &artist_name,
                &metadata.year,
                &metadata.genre,
            )
            .await?,
        )
    };
    let full_path = path.to_string_lossy().into_owned();
    let relative_path = path
        .strip_prefix(PathBuf::from(&folder.path))
        .unwrap_or(path)
        .to_string_lossy()
        .trim_start_matches('/')
        .to_owned();
    let mtime = std::fs::metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let existing = track::Entity::find()
        .filter(track::Column::Path.eq(&full_path))
        .one(db)
        .await?;
    let needs_scrape = metadata.needs_scrape;
    let mimetype = mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    let values = TrackValues {
        folder_id: &folder.id,
        path: &full_path,
        relative_path: &relative_path,
        title: &metadata.title,
        artist_id: &primary_artist.id,
        artist_name: &artist_name,
        artists_json: &artists_json,
        album_id: album_id.as_deref(),
        album_name: &metadata.album,
        album_artist: &metadata.albumartist,
        genre: &metadata.genre,
        language: &metadata.language,
        year: metadata.year.parse().unwrap_or(0),
        track_number: metadata.tracknumber.parse().unwrap_or(0),
        disc_number: metadata.discnumber.parse().unwrap_or(0),
        duration: metadata.duration,
        bit_rate: metadata.bit_rate as i64,
        size: metadata.size as i64,
        suffix: &metadata.suffix,
        mimetype: &mimetype,
        lyrics: &metadata.lyrics,
        comment: &metadata.comment,
        needs_scrape,
        mtime,
        scan_time,
    };
    let track_id = if let Some(existing) = existing {
        let track_id = existing.id.clone();
        update_track(db, existing, &values).await?;
        track_id
    } else {
        let track_id = Uuid::new_v4().to_string();
        insert_track(db, &track_id, &values).await?;
        track_id
    };
    artist_credit::replace_track_artists(db, &track_id, &artist_credits).await?;
    Ok(())
}

struct TrackValues<'a> {
    folder_id: &'a str,
    path: &'a str,
    relative_path: &'a str,
    title: &'a str,
    artist_id: &'a str,
    artist_name: &'a str,
    artists_json: &'a str,
    album_id: Option<&'a str>,
    album_name: &'a str,
    album_artist: &'a str,
    genre: &'a str,
    language: &'a str,
    year: i64,
    track_number: i64,
    disc_number: i64,
    duration: f64,
    bit_rate: i64,
    size: i64,
    suffix: &'a str,
    mimetype: &'a str,
    lyrics: &'a str,
    comment: &'a str,
    needs_scrape: bool,
    mtime: i64,
    scan_time: &'a str,
}

async fn insert_track(
    db: &DatabaseConnection,
    id: &str,
    v: &TrackValues<'_>,
) -> anyhow::Result<()> {
    track::ActiveModel {
        id: Set(id.to_owned()),
        folder_id: Set(v.folder_id.to_owned()),
        path: Set(v.path.to_owned()),
        relative_path: Set(v.relative_path.to_owned()),
        title: Set(v.title.to_owned()),
        artist_id: Set(v.artist_id.to_owned()),
        artist_name: Set(v.artist_name.to_owned()),
        artists_json: Set(v.artists_json.to_owned()),
        album_id: Set(v.album_id.map(str::to_owned)),
        album_name: Set(v.album_name.to_owned()),
        album_artist: Set(v.album_artist.to_owned()),
        genre: Set(v.genre.to_owned()),
        language: Set(v.language.to_owned()),
        year: Set(v.year),
        track_number: Set(v.track_number),
        disc_number: Set(v.disc_number),
        duration: Set(v.duration),
        bit_rate: Set(v.bit_rate),
        size: Set(v.size),
        suffix: Set(v.suffix.to_owned()),
        mimetype: Set(v.mimetype.to_owned()),
        lyrics: Set(v.lyrics.to_owned()),
        comment: Set(v.comment.to_owned()),
        cover_path: Set(None),
        mtime: Set(v.mtime),
        fingerprint: Set(String::new()),
        play_count: Set(0),
        needs_scrape: Set(i64::from(v.needs_scrape)),
        created_at: Set(v.scan_time.to_owned()),
        updated_at: Set(v.scan_time.to_owned()),
    }
    .insert(db)
    .await?;
    Ok(())
}

async fn update_track(
    db: &DatabaseConnection,
    existing: track::Model,
    v: &TrackValues<'_>,
) -> anyhow::Result<()> {
    let source_changed = existing.mtime != v.mtime;
    let mut active = existing.into_active_model();
    active.folder_id = Set(v.folder_id.to_owned());
    active.relative_path = Set(v.relative_path.to_owned());
    active.title = Set(v.title.to_owned());
    active.artist_id = Set(v.artist_id.to_owned());
    active.artist_name = Set(v.artist_name.to_owned());
    active.artists_json = Set(v.artists_json.to_owned());
    active.album_id = Set(v.album_id.map(str::to_owned));
    active.album_name = Set(v.album_name.to_owned());
    active.album_artist = Set(v.album_artist.to_owned());
    active.genre = Set(v.genre.to_owned());
    active.language = Set(v.language.to_owned());
    active.year = Set(v.year);
    active.track_number = Set(v.track_number);
    active.disc_number = Set(v.disc_number);
    active.duration = Set(v.duration);
    active.bit_rate = Set(v.bit_rate);
    active.size = Set(v.size);
    active.suffix = Set(v.suffix.to_owned());
    active.mimetype = Set(v.mimetype.to_owned());
    active.lyrics = Set(v.lyrics.to_owned());
    active.comment = Set(v.comment.to_owned());
    active.needs_scrape = Set(i64::from(v.needs_scrape));
    if source_changed {
        active.cover_path = Set(None);
    }
    active.mtime = Set(v.mtime);
    active.updated_at = Set(v.scan_time.to_owned());
    active.update(db).await?;
    Ok(())
}

async fn get_or_create_album(
    db: &DatabaseConnection,
    name: &str,
    artist_id: &str,
    artist_name: &str,
    year: &str,
    genre: &str,
) -> anyhow::Result<String> {
    if let Some(existing) = album::Entity::find()
        .filter(album::Column::Name.eq(name))
        .order_by_asc(album::Column::CreatedAt)
        .order_by_asc(album::Column::Id)
        .one(db)
        .await?
    {
        return Ok(existing.id);
    }
    let id = Uuid::new_v4().to_string();
    album::Entity::insert(album::ActiveModel {
        id: Set(id.clone()),
        name: Set(name.to_owned()),
        artist_id: Set(artist_id.to_owned()),
        artist_name: Set(artist_name.to_owned()),
        year: Set(year.parse::<i64>().unwrap_or(0)),
        genre: Set(genre.to_owned()),
        cover_path: Set(None),
        song_count: Set(0),
        duration: Set(0.0),
        created_at: Set(Utc::now().to_rfc3339()),
    })
    .on_conflict(OnConflict::new().do_nothing().to_owned())
    .exec_without_returning(db)
    .await?;
    // A concurrent refresh may have inserted the same album name first.
    Ok(album::Entity::find()
        .filter(album::Column::Name.eq(name))
        .order_by_asc(album::Column::CreatedAt)
        .order_by_asc(album::Column::Id)
        .one(db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("album was not created"))?
        .id)
}

pub(crate) async fn remove_track_records(
    db: &DatabaseConnection,
    track_ids: &[String],
) -> anyhow::Result<u64> {
    if track_ids.is_empty() {
        return Ok(0);
    }
    let transaction = db.begin().await?;
    let removed = track_ids.iter().cloned().collect::<HashSet<_>>();
    for chunk in track_ids.chunks(500) {
        let chunk = chunk.to_vec();
        track_artist::Entity::delete_many()
            .filter(track_artist::Column::TrackId.is_in(chunk.clone()))
            .exec(&transaction)
            .await?;
        playlist_track::Entity::delete_many()
            .filter(playlist_track::Column::TrackId.is_in(chunk.clone()))
            .exec(&transaction)
            .await?;
        bookmark::Entity::delete_many()
            .filter(bookmark::Column::TrackId.is_in(chunk.clone()))
            .exec(&transaction)
            .await?;
        scrobble::Entity::delete_many()
            .filter(scrobble::Column::TrackId.is_in(chunk.clone()))
            .exec(&transaction)
            .await?;
        user_track_stat::Entity::delete_many()
            .filter(user_track_stat::Column::TrackId.is_in(chunk.clone()))
            .exec(&transaction)
            .await?;
        favorite::Entity::delete_many()
            .filter(favorite::Column::ItemType.eq("track"))
            .filter(favorite::Column::ItemId.is_in(chunk.clone()))
            .exec(&transaction)
            .await?;
        rating::Entity::delete_many()
            .filter(rating::Column::ItemType.eq("track"))
            .filter(rating::Column::ItemId.is_in(chunk))
            .exec(&transaction)
            .await?;
    }

    for queue in play_queue::Entity::find().all(&transaction).await? {
        let mut ids = serde_json::from_str::<Vec<String>>(&queue.track_ids).unwrap_or_default();
        let previous_len = ids.len();
        ids.retain(|id| !removed.contains(id));
        if ids.len() == previous_len {
            continue;
        }
        let previous_current = queue.current_id.clone();
        let current_id = queue
            .current_id
            .clone()
            .filter(|id| ids.contains(id))
            .or_else(|| ids.first().cloned());
        let position = if previous_current == current_id {
            queue.position
        } else {
            0
        };
        let mut active = queue.into_active_model();
        active.track_ids = Set(serde_json::to_string(&ids)?);
        active.current_id = Set(current_id);
        active.position = Set(position);
        active.changed_at = Set(Utc::now().to_rfc3339());
        active.update(&transaction).await?;
    }

    for shared in share::Entity::find().all(&transaction).await? {
        let mut ids = serde_json::from_str::<Vec<String>>(&shared.item_ids).unwrap_or_default();
        let previous_len = ids.len();
        ids.retain(|id| !removed.contains(id));
        if ids.len() == previous_len {
            continue;
        }
        let mut active = shared.into_active_model();
        active.item_ids = Set(serde_json::to_string(&ids)?);
        active.update(&transaction).await?;
    }

    let mut rows_affected = 0;
    for chunk in track_ids.chunks(500) {
        rows_affected += track::Entity::delete_many()
            .filter(track::Column::Id.is_in(chunk.to_vec()))
            .exec(&transaction)
            .await?
            .rows_affected;
    }
    transaction.commit().await?;
    Ok(rows_affected)
}

pub(crate) async fn rebuild_aggregates(db: &DatabaseConnection) -> anyhow::Result<()> {
    let transaction = db.begin().await?;
    transaction
        .execute_unprepared(
            "DELETE FROM track_artists WHERE track_id NOT IN (SELECT id FROM tracks)",
        )
        .await?;
    transaction.execute_unprepared("UPDATE artists SET song_count = (SELECT COUNT(*) FROM track_artists WHERE track_artists.artist_id=artists.id), album_count = (SELECT COUNT(DISTINCT tracks.album_id) FROM track_artists JOIN tracks ON tracks.id=track_artists.track_id WHERE track_artists.artist_id=artists.id AND tracks.album_id IS NOT NULL)").await?;
    transaction.execute_unprepared("UPDATE albums SET song_count = (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id), duration = COALESCE((SELECT SUM(duration) FROM tracks WHERE tracks.album_id = albums.id), 0), year = COALESCE((SELECT MAX(NULLIF(year, 0)) FROM tracks WHERE tracks.album_id = albums.id), 0), genre = COALESCE((SELECT genre FROM tracks WHERE tracks.album_id = albums.id AND genre <> '' ORDER BY disc_number, track_number, id LIMIT 1), ''), cover_path = CASE WHEN EXISTS (SELECT 1 FROM tracks WHERE tracks.album_id = albums.id AND tracks.cover_path IS NOT NULL AND tracks.cover_path <> '') THEN 'embedded' WHEN NOT EXISTS (SELECT 1 FROM tracks WHERE tracks.album_id = albums.id AND tracks.cover_path IS NULL) THEN '' ELSE NULL END").await?;

    // Album identity is its name; its display credits include all contributing singers.
    #[derive(sea_orm::FromQueryResult)]
    struct AlbumArtist {
        album_id: String,
        artist_id: String,
        artist_name: String,
    }
    let credits = AlbumArtist::find_by_statement(sea_orm::Statement::from_string(
        transaction.get_database_backend(),
        "SELECT DISTINCT t.album_id, a.id AS artist_id, a.name AS artist_name \
         FROM tracks t JOIN track_artists ta ON ta.track_id=t.id \
         JOIN artists a ON a.id=ta.artist_id WHERE t.album_id IS NOT NULL \
         ORDER BY t.album_id, a.name, a.id",
    ))
    .all(&transaction)
    .await?;
    let mut album_credits = HashMap::<String, Vec<AlbumArtist>>::new();
    for credit in credits {
        album_credits
            .entry(credit.album_id.clone())
            .or_default()
            .push(credit);
    }
    for (album_id, credits) in album_credits {
        album::Entity::update_many()
            .col_expr(
                album::Column::ArtistId,
                Expr::value(credits[0].artist_id.clone()),
            )
            .col_expr(
                album::Column::ArtistName,
                Expr::value(
                    credits
                        .iter()
                        .map(|credit| credit.artist_name.as_str())
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
            )
            .filter(album::Column::Id.eq(album_id))
            .exec(&transaction)
            .await?;
    }

    for item_type in ["album", "artist"] {
        let active_ids = if item_type == "album" {
            track::Entity::find()
                .select_only()
                .column(track::Column::AlbumId)
                .filter(track::Column::AlbumId.is_not_null())
                .into_query()
        } else {
            track_artist::Entity::find()
                .select_only()
                .column(track_artist::Column::ArtistId)
                .into_query()
        };
        favorite::Entity::delete_many()
            .filter(favorite::Column::ItemType.eq(item_type))
            .filter(favorite::Column::ItemId.not_in_subquery(active_ids.clone()))
            .exec(&transaction)
            .await?;
        rating::Entity::delete_many()
            .filter(rating::Column::ItemType.eq(item_type))
            .filter(rating::Column::ItemId.not_in_subquery(active_ids))
            .exec(&transaction)
            .await?;
    }

    let active_album_ids = track::Entity::find()
        .select_only()
        .column(track::Column::AlbumId)
        .filter(track::Column::AlbumId.is_not_null())
        .into_query();
    let empty_album_ids = album::Entity::find()
        .select_only()
        .column(album::Column::Id)
        .filter(album::Column::Id.not_in_subquery(active_album_ids.clone()))
        .into_tuple::<String>()
        .all(&transaction)
        .await?
        .into_iter()
        .collect::<HashSet<_>>();
    if !empty_album_ids.is_empty() {
        for shared in share::Entity::find().all(&transaction).await? {
            let Ok(mut ids) = serde_json::from_str::<Vec<String>>(&shared.item_ids) else {
                continue;
            };
            let previous_len = ids.len();
            ids.retain(|id| !empty_album_ids.contains(id));
            if ids.len() != previous_len {
                let mut active = shared.into_active_model();
                active.item_ids = Set(serde_json::to_string(&ids)?);
                active.update(&transaction).await?;
            }
        }
    }
    album::Entity::delete_many()
        .filter(album::Column::Id.not_in_subquery(active_album_ids))
        .exec(&transaction)
        .await?;

    let track_artist_ids = track_artist::Entity::find()
        .select_only()
        .column(track_artist::Column::ArtistId)
        .into_query();
    let album_artist_ids = album::Entity::find()
        .select_only()
        .column(album::Column::ArtistId)
        .into_query();
    artist::Entity::delete_many()
        .filter(artist::Column::Id.not_in_subquery(track_artist_ids))
        .filter(artist::Column::Id.not_in_subquery(album_artist_ids))
        .exec(&transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|v| v.to_str())
        .map(|v| AUDIO_EXTENSIONS.contains(&v.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use md5::{Digest, Md5};

    use super::*;
    use crate::config::{CoverCacheSettings, DatabaseSettings, ToolSettings};

    fn write_wav(path: &Path) {
        std::fs::write(
            path,
            [
                b'R', b'I', b'F', b'F', 38, 0, 0, 0, b'W', b'A', b'V', b'E', b'f', b'm', b't',
                b' ', 16, 0, 0, 0, 1, 0, 1, 0, 64, 31, 0, 0, 128, 62, 0, 0, 2, 0, 16, 0, b'd',
                b'a', b't', b'a', 2, 0, 0, 0, 0, 0,
            ],
        )
        .unwrap();
    }

    async fn test_database() -> DatabaseConnection {
        let db = crate::db::connect(&DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        crate::db::migrate(&db).await.unwrap();
        db
    }

    async fn add_library(db: &DatabaseConnection, id: &str, path: &Path) {
        music_folder::ActiveModel {
            id: Set(id.into()),
            name: Set(id.into()),
            path: Set(path.to_string_lossy().into_owned()),
            enabled: Set(1),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn index_test_track(
        db: &DatabaseConnection,
        folder: &MusicFolder,
        path: &Path,
        artist: &str,
        album: &str,
    ) {
        write_wav(path);
        index_track(
            db,
            folder,
            path,
            crate::tags::AudioMetadata {
                title: path.file_stem().unwrap().to_string_lossy().into_owned(),
                artist: artist.into(),
                album: album.into(),
                albumartist: format!("Album artist for {artist}"),
                duration: 120.0,
                suffix: "wav".into(),
                ..Default::default()
            },
            "2026-09-05T00:00:00Z",
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn album_names_group_different_singers_and_libraries() {
        let directory = tempfile::tempdir().unwrap();
        let db = test_database().await;
        for id in ["library-a", "library-b"] {
            let root = directory.path().join(id);
            std::fs::create_dir(&root).unwrap();
            add_library(&db, id, &root).await;
        }
        let folders = music_folder::Entity::find()
            .order_by_asc(music_folder::Column::Id)
            .all(&db)
            .await
            .unwrap();
        for (folder, artist, title) in [
            (&folders[1], "Singer B", "Song B"),
            (&folders[0], "Singer A; Singer C", "Song A"),
        ] {
            index_test_track(
                &db,
                folder,
                &Path::new(&folder.path).join(format!("{title}.wav")),
                artist,
                " Compilation ",
            )
            .await;
        }
        rebuild_aggregates(&db).await.unwrap();

        let albums = album::Entity::find().all(&db).await.unwrap();
        assert_eq!(albums.len(), 1);
        let compilation = &albums[0];
        assert_eq!(compilation.name, "Compilation");
        assert_eq!(compilation.song_count, 2);
        assert_eq!(compilation.duration, 240.0);
        assert_eq!(compilation.artist_name, "Singer A; Singer B; Singer C");
        let tracks = track::Entity::find()
            .order_by_asc(track::Column::Title)
            .all(&db)
            .await
            .unwrap();
        assert_eq!(tracks[0].artist_name, "Singer A; Singer C");
        assert_eq!(tracks[1].artist_name, "Singer B");
        assert!(
            tracks
                .iter()
                .all(|track| track.album_id.as_deref() == Some(compilation.id.as_str()))
        );
        for artist in artist::Entity::find().all(&db).await.unwrap() {
            assert_eq!(artist.album_count, 1);
            assert_eq!(artist.song_count, 1);
        }

        index_test_track(
            &db,
            &folders[0],
            &Path::new(&folders[0].path).join("Other.wav"),
            "Singer A",
            "Other Album",
        )
        .await;
        rebuild_aggregates(&db).await.unwrap();
        assert_eq!(album::Entity::find().all(&db).await.unwrap().len(), 2);
        assert_eq!(
            album::Entity::find_by_id(&compilation.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .song_count,
            2
        );
    }

    #[tokio::test]
    async fn changing_album_cleans_up_only_after_its_last_song_moves() {
        let directory = tempfile::tempdir().unwrap();
        let db = test_database().await;
        add_library(&db, "library", directory.path()).await;
        let tags = Arc::new(TagService::new(ToolSettings::default()));
        let paths = [
            directory.path().join("a.wav"),
            directory.path().join("b.wav"),
        ];
        let mut metadata = crate::tags::AudioMetadata {
            title: "Song".into(),
            artist: "Singer".into(),
            album: "Old Album".into(),
            ..Default::default()
        };
        for path in &paths {
            write_wav(path);
            tags.write(path, &metadata).unwrap();
        }
        refresh_paths(&db, tags.clone(), &paths).await.unwrap();
        let old_album = album::Entity::find().one(&db).await.unwrap().unwrap();
        let tracks = track::Entity::find()
            .order_by_asc(track::Column::Path)
            .all(&db)
            .await
            .unwrap();
        for (item_type, item_id) in [("album", &old_album.id), ("track", &tracks[0].id)] {
            favorite::ActiveModel {
                user_id: Set("user".into()),
                item_type: Set(item_type.into()),
                item_id: Set(item_id.clone()),
                created_at: Set("2026-09-05T00:00:00Z".into()),
            }
            .insert(&db)
            .await
            .unwrap();
            rating::ActiveModel {
                user_id: Set("user".into()),
                item_type: Set(item_type.into()),
                item_id: Set(item_id.clone()),
                rating: Set(5),
            }
            .insert(&db)
            .await
            .unwrap();
        }
        playlist_track::ActiveModel {
            playlist_id: Set("playlist".into()),
            position: Set(0),
            track_id: Set(tracks[0].id.clone()),
        }
        .insert(&db)
        .await
        .unwrap();
        share::ActiveModel {
            id: Set("shared".into()),
            user_id: Set("user".into()),
            item_ids: Set(serde_json::to_string(&[&old_album.id, &tracks[0].id]).unwrap()),
            description: Set(String::new()),
            expires_at: Set(None),
            created_at: Set("2026-09-05T00:00:00Z".into()),
            play_count: Set(0),
            last_visited_at: Set(None),
        }
        .insert(&db)
        .await
        .unwrap();

        metadata.album = "New Album".into();
        tags.write(&paths[0], &metadata).unwrap();
        refresh_paths(&db, tags.clone(), &paths[..1]).await.unwrap();
        assert_eq!(
            album::Entity::find_by_id(&old_album.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .song_count,
            1
        );
        assert_eq!(favorite::Entity::find().all(&db).await.unwrap().len(), 2);
        assert_eq!(
            share::Entity::find_by_id("shared")
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .item_ids,
            serde_json::to_string(&[&old_album.id, &tracks[0].id]).unwrap()
        );

        tags.write(&paths[1], &metadata).unwrap();
        refresh_paths(&db, tags.clone(), &paths[1..]).await.unwrap();
        assert!(
            album::Entity::find_by_id(&old_album.id)
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        let albums = album::Entity::find().all(&db).await.unwrap();
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].name, "New Album");
        assert_eq!(albums[0].song_count, 2);
        assert_eq!(
            share::Entity::find_by_id("shared")
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .item_ids,
            serde_json::to_string(&[&tracks[0].id]).unwrap()
        );
        assert_eq!(
            favorite::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .item_id,
            tracks[0].id
        );
        assert_eq!(
            rating::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .item_id,
            tracks[0].id
        );
        assert_eq!(
            playlist_track::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .track_id,
            tracks[0].id
        );

        metadata.album.clear();
        for path in &paths {
            tags.write(path, &metadata).unwrap();
        }
        refresh_paths(&db, tags, &paths).await.unwrap();
        assert!(album::Entity::find().all(&db).await.unwrap().is_empty());
        let refreshed = track::Entity::find()
            .order_by_asc(track::Column::Path)
            .all(&db)
            .await
            .unwrap();
        assert_eq!(refreshed.len(), 2);
        for (before, after) in tracks.iter().zip(&refreshed) {
            assert_eq!(before.id, after.id);
            assert_eq!(before.created_at, after.created_at);
            assert!(after.album_id.is_none());
        }
    }

    #[tokio::test]
    async fn concurrent_album_creation_reuses_the_same_name() {
        let db = test_database().await;
        let (first, second) = tokio::join!(
            get_or_create_album(&db, "Compilation", "artist-a", "Singer A", "", ""),
            get_or_create_album(&db, "Compilation", "artist-b", "Singer B", "", ""),
        );
        assert_eq!(first.unwrap(), second.unwrap());
        assert_eq!(album::Entity::find().all(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refresh_paths_indexes_a_changed_file_without_full_scan() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tone.wav");
        write_wav(&path);
        let db = test_database().await;
        add_library(&db, "library", directory.path()).await;

        let indexed = refresh_paths(
            &db,
            Arc::new(TagService::new(ToolSettings::default())),
            std::slice::from_ref(&path),
        )
        .await
        .unwrap();

        assert_eq!(indexed, 1);
        let indexed_track = track::Entity::find().one(&db).await.unwrap().unwrap();
        assert_eq!(indexed_track.title, "tone");
        assert_eq!(indexed_track.path, path.to_string_lossy());
        assert_eq!(indexed_track.needs_scrape, 1);

        track::Entity::update_many()
            .col_expr(track::Column::NeedsScrape, Expr::value(0))
            .filter(track::Column::Id.eq(&indexed_track.id))
            .exec(&db)
            .await
            .unwrap();
        let renamed = directory.path().join("renamed.wav");
        std::fs::rename(&path, &renamed).unwrap();
        refresh_path_changes(
            &db,
            Arc::new(TagService::new(ToolSettings::default())),
            &[(path, renamed.clone())],
        )
        .await
        .unwrap();
        let renamed_track = track::Entity::find().one(&db).await.unwrap().unwrap();
        assert_eq!(renamed_track.id, indexed_track.id);
        assert_eq!(renamed_track.path, renamed.to_string_lossy());
        assert_eq!(renamed_track.needs_scrape, 1);
    }

    #[tokio::test]
    async fn remembers_written_artwork_and_invalidates_it_when_the_source_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tone.wav");
        write_wav(&path);
        let db = test_database().await;
        add_library(&db, "library", directory.path()).await;
        let tags = Arc::new(TagService::new(ToolSettings::default()));
        refresh_paths(&db, tags.clone(), std::slice::from_ref(&path))
            .await
            .unwrap();

        assert_eq!(
            remember_artwork_statuses(&db, &[(path.clone(), true)])
                .await
                .unwrap(),
            1
        );
        let track = track::Entity::find().one(&db).await.unwrap().unwrap();
        assert_eq!(track.cover_path.as_deref(), Some(EMBEDDED_ARTWORK_MARKER));

        track::Entity::update_many()
            .col_expr(track::Column::Mtime, Expr::value(track.mtime - 1))
            .filter(track::Column::Id.eq(&track.id))
            .exec(&db)
            .await
            .unwrap();
        refresh_paths(&db, tags, std::slice::from_ref(&path))
            .await
            .unwrap();
        assert_eq!(
            track::Entity::find_by_id(track.id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .cover_path,
            None
        );
    }

    #[tokio::test]
    async fn scan_preserves_unreadable_and_disabled_library_tracks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tone.wav");
        write_wav(&path);
        let db = test_database().await;
        add_library(&db, "library", directory.path()).await;
        let tags = Arc::new(TagService::new(ToolSettings::default()));
        refresh_paths(&db, tags.clone(), std::slice::from_ref(&path))
            .await
            .unwrap();

        std::fs::write(&path, b"not audio").unwrap();
        let report = scan_all(&db, tags.clone(), &CancellationToken::new(), |_, _| {})
            .await
            .unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(report.removed, 0);
        assert_eq!(track::Entity::find().all(&db).await.unwrap().len(), 1);

        let folder = music_folder::Entity::find_by_id("library")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let mut active = folder.into_active_model();
        active.enabled = Set(0);
        active.update(&db).await.unwrap();
        std::fs::remove_file(&path).unwrap();
        let report = scan_all(&db, tags, &CancellationToken::new(), |_, _| {})
            .await
            .unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(track::Entity::find().all(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn scan_removes_missing_track_references() {
        let directory = tempfile::tempdir().unwrap();
        let cover_cache = tempfile::tempdir().unwrap();
        let path = directory.path().join("tone.wav");
        write_wav(&path);
        let db = test_database().await;
        add_library(&db, "library", directory.path()).await;
        let tags = Arc::new(TagService::with_cover_cache(
            ToolSettings::default(),
            CoverCacheSettings {
                enabled: true,
                path: cover_cache.path().to_path_buf(),
                concurrency: 2,
            },
        ));
        refresh_paths(&db, tags.clone(), std::slice::from_ref(&path))
            .await
            .unwrap();
        let track_id = track::Entity::find().one(&db).await.unwrap().unwrap().id;
        let artwork_cache = cover_cache.path().join(format!(
            "{}-stale.artwork",
            hex::encode(Md5::digest(track_id.as_bytes()))
        ));
        std::fs::write(&artwork_cache, b"stale artwork").unwrap();
        let now = Utc::now().to_rfc3339();
        playlist_track::ActiveModel {
            playlist_id: Set("playlist".into()),
            position: Set(0),
            track_id: Set(track_id.clone()),
        }
        .insert(&db)
        .await
        .unwrap();
        favorite::ActiveModel {
            user_id: Set("user".into()),
            item_type: Set("track".into()),
            item_id: Set(track_id.clone()),
            created_at: Set(now.clone()),
        }
        .insert(&db)
        .await
        .unwrap();
        rating::ActiveModel {
            user_id: Set("user".into()),
            item_type: Set("track".into()),
            item_id: Set(track_id.clone()),
            rating: Set(5),
        }
        .insert(&db)
        .await
        .unwrap();
        bookmark::ActiveModel {
            user_id: Set("user".into()),
            track_id: Set(track_id.clone()),
            position: Set(10),
            comment: Set(String::new()),
            changed_at: Set(now.clone()),
        }
        .insert(&db)
        .await
        .unwrap();
        scrobble::ActiveModel {
            id: Set("scrobble".into()),
            user_id: Set("user".into()),
            track_id: Set(track_id.clone()),
            played_at: Set(now.clone()),
            submission: Set(1),
        }
        .insert(&db)
        .await
        .unwrap();
        user_track_stat::ActiveModel {
            user_id: Set("user".into()),
            track_id: Set(track_id.clone()),
            play_count: Set(1),
            last_played_at: Set(now.clone()),
        }
        .insert(&db)
        .await
        .unwrap();
        play_queue::ActiveModel {
            user_id: Set("user".into()),
            track_ids: Set(serde_json::to_string(&[&track_id]).unwrap()),
            current_id: Set(Some(track_id.clone())),
            current_index: Set(Some(0)),
            position: Set(500),
            changed_at: Set(now.clone()),
            changed_by: Set("test".into()),
        }
        .insert(&db)
        .await
        .unwrap();
        share::ActiveModel {
            id: Set("share".into()),
            user_id: Set("user".into()),
            item_ids: Set(serde_json::to_string(&[&track_id]).unwrap()),
            description: Set(String::new()),
            expires_at: Set(None),
            created_at: Set(now),
            play_count: Set(0),
            last_visited_at: Set(None),
        }
        .insert(&db)
        .await
        .unwrap();

        std::fs::remove_file(path).unwrap();
        let report = scan_all(&db, tags, &CancellationToken::new(), |_, _| {})
            .await
            .unwrap();

        assert_eq!(report.removed, 1);
        assert!(!artwork_cache.exists());
        assert!(track::Entity::find().one(&db).await.unwrap().is_none());
        assert!(
            playlist_track::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        assert!(favorite::Entity::find().one(&db).await.unwrap().is_none());
        assert!(rating::Entity::find().one(&db).await.unwrap().is_none());
        assert!(bookmark::Entity::find().one(&db).await.unwrap().is_none());
        assert!(scrobble::Entity::find().one(&db).await.unwrap().is_none());
        assert!(
            user_track_stat::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
        let queue = play_queue::Entity::find().one(&db).await.unwrap().unwrap();
        assert_eq!(queue.track_ids, "[]");
        assert_eq!(queue.current_id, None);
        assert_eq!(queue.position, 0);
        let shared = share::Entity::find().one(&db).await.unwrap().unwrap();
        assert_eq!(shared.item_ids, "[]");
    }
}
