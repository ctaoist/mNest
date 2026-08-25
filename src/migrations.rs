use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    QueryFilter, Set, Statement, StatementBuilder, TransactionTrait,
    sea_query::{Alias, ColumnDef, Index, Table, TableCreateStatement},
};

use crate::entities::schema_migration;

pub(crate) const BASELINE: &str = "baseline-4";

pub async fn run(db: &DatabaseConnection) -> anyhow::Result<()> {
    create_schema_migrations(db).await?;

    if !applied(db, BASELINE).await? {
        let transaction = db.begin().await?;
        create_baseline(&transaction).await?;
        ensure_compatibility_columns(&transaction).await?;
        backfill_user_track_stats(&transaction).await?;
        backfill_user_subsonic_access(&transaction).await?;
        record(&transaction, BASELINE).await?;
        remove_non_baseline_versions(&transaction).await?;
        transaction.commit().await?;
    } else {
        remove_non_baseline_versions(db).await?;
    }
    if !has_column(db, "tracks", "needs_scrape").await? {
        anyhow::bail!(
            "database baseline is outdated: recreate the database to add tracks.needs_scrape"
        );
    }
    if !has_column(db, "app_settings", "key").await? {
        anyhow::bail!("database baseline is outdated: recreate the database to add app_settings");
    }

    Ok(())
}

async fn create_schema_migrations<C: ConnectionTrait>(db: &C) -> anyhow::Result<()> {
    let mut table = create_table("schema_migrations");
    table
        .col(text_primary_key("version"))
        .col(required_text("applied_at"));
    execute(db, table).await
}

async fn create_baseline<C: ConnectionTrait>(db: &C) -> anyhow::Result<()> {
    let mut users = create_table("users");
    users
        .col(text_primary_key("id"))
        .col(required_text("username"))
        .col(required_text("password_hash"))
        .col(text_default("email", ""))
        .col(text_default("role", "user"))
        .col(text_default("subsonic_token", ""))
        .col(text_default("subsonic_password", ""))
        .col(required_text("created_at"));
    execute(db, users).await?;
    create_index(db, "uq_users_username", "users", &["username"], true).await?;

    let mut user_access = create_table("user_subsonic_access");
    user_access
        .col(text_primary_key("user_id"))
        .col(bigint_default("ldap_authenticated", 0))
        .col(bigint_default("settings_role", 1))
        .col(bigint_default("stream_role", 1))
        .col(bigint_default("jukebox_role", 0))
        .col(bigint_default("download_role", 0))
        .col(bigint_default("upload_role", 0))
        .col(bigint_default("playlist_role", 0))
        .col(bigint_default("cover_art_role", 0))
        .col(bigint_default("comment_role", 0))
        .col(bigint_default("podcast_role", 0))
        .col(bigint_default("share_role", 0))
        .col(bigint_default("video_conversion_role", 0))
        .col(bigint_default("max_bit_rate", 0))
        .col(text_default("folder_ids", "*"));
    execute(db, user_access).await?;

    let mut folders = create_table("music_folders");
    folders
        .col(text_primary_key("id"))
        .col(required_text("name"))
        .col(required_text("path"))
        .col(bigint_default("enabled", 1));
    execute(db, folders).await?;
    create_index(
        db,
        "uq_music_folders_name",
        "music_folders",
        &["name"],
        true,
    )
    .await?;
    create_index(
        db,
        "uq_music_folders_path",
        "music_folders",
        &["path"],
        true,
    )
    .await?;

    let mut artists = create_table("artists");
    artists
        .col(text_primary_key("id"))
        .col(required_text("name"))
        .col(required_text("sort_name"))
        .col(text("cover_path"))
        .col(bigint_default("album_count", 0))
        .col(bigint_default("song_count", 0));
    execute(db, artists).await?;
    create_index(db, "uq_artists_name", "artists", &["name"], true).await?;

    let mut albums = create_table("albums");
    albums
        .col(text_primary_key("id"))
        .col(required_text("name"))
        .col(required_text("artist_id"))
        .col(required_text("artist_name"))
        .col(bigint_default("year", 0))
        .col(text_default("genre", ""))
        .col(text("cover_path"))
        .col(bigint_default("song_count", 0))
        .col(double_default("duration", 0.0))
        .col(required_text("created_at"));
    execute(db, albums).await?;
    create_index(
        db,
        "uq_albums_name_artist",
        "albums",
        &["name", "artist_id"],
        true,
    )
    .await?;

    let mut tracks = create_table("tracks");
    tracks
        .col(text_primary_key("id"))
        .col(required_text("folder_id"))
        .col(required_text("path"))
        .col(required_text("relative_path"))
        .col(required_text("title"))
        .col(required_text("artist_id"))
        .col(required_text("artist_name"))
        .col(text_default("artists_json", "[]"))
        .col(text("album_id"))
        .col(text_default("album_name", ""))
        .col(text_default("album_artist", ""))
        .col(text_default("genre", ""))
        .col(bigint_default("year", 0))
        .col(bigint_default("track_number", 0))
        .col(bigint_default("disc_number", 0))
        .col(double_default("duration", 0.0))
        .col(bigint_default("bit_rate", 0))
        .col(bigint_default("size", 0))
        .col(text_default("suffix", ""))
        .col(text_default("mimetype", ""))
        .col(text_default("lyrics", ""))
        .col(text_default("comment", ""))
        .col(text("cover_path"))
        .col(bigint_default("mtime", 0))
        .col(text_default("fingerprint", ""))
        .col(bigint_default("play_count", 0))
        .col(bigint_default("needs_scrape", 0))
        .col(required_text("created_at"))
        .col(required_text("updated_at"));
    execute(db, tracks).await?;
    create_index(db, "uq_tracks_path", "tracks", &["path"], true).await?;

    let mut jobs = create_table("jobs");
    jobs.col(text_primary_key("id"))
        .col(required_text("kind"))
        .col(required_text("state"))
        .col(required_text("payload"))
        .col(double_default("progress", 0.0))
        .col(text_default("message", ""))
        .col(bigint_default("attempts", 0))
        .col(text("lease_until"))
        .col(required_text("created_at"))
        .col(required_text("updated_at"));
    execute(db, jobs).await?;

    let mut favorites = create_table("favorites");
    favorites
        .col(required_text("user_id"))
        .col(required_text("item_type"))
        .col(required_text("item_id"))
        .col(required_text("created_at"))
        .primary_key(
            Index::create()
                .col(alias("user_id"))
                .col(alias("item_type"))
                .col(alias("item_id")),
        );
    execute(db, favorites).await?;

    let mut ratings = create_table("ratings");
    ratings
        .col(required_text("user_id"))
        .col(required_text("item_type"))
        .col(required_text("item_id"))
        .col(required_bigint("rating"))
        .primary_key(
            Index::create()
                .col(alias("user_id"))
                .col(alias("item_type"))
                .col(alias("item_id")),
        );
    execute(db, ratings).await?;

    let mut playlists = create_table("playlists");
    playlists
        .col(text_primary_key("id"))
        .col(required_text("user_id"))
        .col(required_text("name"))
        .col(text_default("comment", ""))
        .col(bigint_default("public", 0))
        .col(required_text("created_at"))
        .col(required_text("updated_at"));
    execute(db, playlists).await?;

    let mut playlist_tracks = create_table("playlist_tracks");
    playlist_tracks
        .col(required_text("playlist_id"))
        .col(required_text("track_id"))
        .col(required_bigint("position"))
        .primary_key(
            Index::create()
                .col(alias("playlist_id"))
                .col(alias("position")),
        );
    execute(db, playlist_tracks).await?;

    let mut bookmarks = create_table("bookmarks");
    bookmarks
        .col(required_text("user_id"))
        .col(required_text("track_id"))
        .col(required_bigint("position"))
        .col(text_default("comment", ""))
        .col(required_text("changed_at"))
        .primary_key(Index::create().col(alias("user_id")).col(alias("track_id")));
    execute(db, bookmarks).await?;

    let mut queues = create_table("play_queues");
    queues
        .col(text_primary_key("user_id"))
        .col(text_default("track_ids", "[]"))
        .col(text("current_id"))
        .col(bigint("current_index"))
        .col(bigint_default("position", 0))
        .col(required_text("changed_at"))
        .col(text_default("changed_by", ""));
    execute(db, queues).await?;

    let mut shares = create_table("shares");
    shares
        .col(text_primary_key("id"))
        .col(required_text("user_id"))
        .col(required_text("item_ids"))
        .col(text_default("description", ""))
        .col(text("expires_at"))
        .col(required_text("created_at"))
        .col(bigint_default("play_count", 0))
        .col(text("last_visited_at"));
    execute(db, shares).await?;

    let mut stations = create_table("internet_radio_stations");
    stations
        .col(text_primary_key("id"))
        .col(required_text("name"))
        .col(required_text("stream_url"))
        .col(text_default("home_page_url", ""));
    execute(db, stations).await?;

    let mut scrobbles = create_table("scrobbles");
    scrobbles
        .col(text_primary_key("id"))
        .col(required_text("user_id"))
        .col(required_text("track_id"))
        .col(required_text("played_at"))
        .col(bigint_default("submission", 0));
    execute(db, scrobbles).await?;

    let mut playback_states = create_table("playback_states");
    playback_states
        .col(text_primary_key("user_id"))
        .col(required_text("media_id"))
        .col(required_text("media_type"))
        .col(bigint_default("position_ms", 0))
        .col(required_text("state"))
        .col(double_default("playback_rate", 1.0))
        .col(bigint_default("ignore_scrobble", 0))
        .col(bigint_default("scrobbled", 0))
        .col(required_text("updated_at"))
        .col(text_default("client", ""));
    execute(db, playback_states).await?;

    let mut user_track_stats = create_table("user_track_stats");
    user_track_stats
        .col(required_text("user_id"))
        .col(required_text("track_id"))
        .col(bigint_default("play_count", 0))
        .col(required_text("last_played_at"))
        .primary_key(Index::create().col(alias("user_id")).col(alias("track_id")));
    execute(db, user_track_stats).await?;

    let mut track_artists = create_table("track_artists");
    track_artists
        .col(required_text("track_id"))
        .col(required_text("artist_id"))
        .col(bigint_default("position", 0))
        .primary_key(
            Index::create()
                .col(alias("track_id"))
                .col(alias("artist_id")),
        );
    execute(db, track_artists).await?;

    let mut sources = create_table("download_sources");
    sources
        .col(text_primary_key("id"))
        .col(required_text("kind"))
        .col(required_text("name"))
        .col(required_text("base_url"))
        .col(text_default("username", ""))
        .col(text_default("password", ""))
        .col(text_default("cookie", ""))
        .col(text_default("account_name", ""))
        .col(bigint_default("enabled", 1))
        .col(required_text("created_at"))
        .col(required_text("updated_at"));
    execute(db, sources).await?;
    create_index(
        db,
        "uq_download_sources_kind_name",
        "download_sources",
        &["kind", "name"],
        true,
    )
    .await?;

    let mut settings = create_table("app_settings");
    settings
        .col(text_primary_key("key"))
        .col(required_text("value"));
    execute(db, settings).await?;

    for (name, table, columns) in [
        ("idx_tracks_artist", "tracks", &["artist_id"][..]),
        ("idx_tracks_album", "tracks", &["album_id"][..]),
        ("idx_tracks_folder", "tracks", &["folder_id"][..]),
        ("idx_tracks_title", "tracks", &["title"][..]),
        (
            "idx_tracks_needs_scrape",
            "tracks",
            &["needs_scrape", "path"][..],
        ),
        ("idx_albums_artist", "albums", &["artist_id"][..]),
        ("idx_jobs_state", "jobs", &["state", "created_at"][..]),
        (
            "idx_track_artists_artist",
            "track_artists",
            &["artist_id", "position"][..],
        ),
        (
            "idx_track_artists_track",
            "track_artists",
            &["track_id", "position"][..],
        ),
        (
            "idx_download_sources_kind",
            "download_sources",
            &["kind", "enabled"][..],
        ),
        (
            "idx_user_track_stats_track",
            "user_track_stats",
            &["track_id"][..],
        ),
    ] {
        create_index(db, name, table, columns, false).await?;
    }

    Ok(())
}

async fn backfill_user_track_stats<C: ConnectionTrait>(db: &C) -> anyhow::Result<()> {
    let sql = match db.get_database_backend() {
        DbBackend::Sqlite => {
            "INSERT OR IGNORE INTO user_track_stats(user_id,track_id,play_count,last_played_at) \
             SELECT user_id,track_id,COUNT(*),MAX(played_at) FROM scrobbles \
             WHERE submission=1 GROUP BY user_id,track_id"
        }
        DbBackend::Postgres => {
            "INSERT INTO user_track_stats(user_id,track_id,play_count,last_played_at) \
             SELECT user_id,track_id,COUNT(*),MAX(played_at) FROM scrobbles \
             WHERE submission=1 GROUP BY user_id,track_id \
             ON CONFLICT(user_id,track_id) DO NOTHING"
        }
        other => anyhow::bail!("unsupported database backend: {other:?}"),
    };
    db.execute_unprepared(sql).await?;
    Ok(())
}

async fn ensure_compatibility_columns<C: ConnectionTrait>(db: &C) -> anyhow::Result<()> {
    if !has_column(db, "play_queues", "current_index").await? {
        db.execute_unprepared("ALTER TABLE play_queues ADD COLUMN current_index BIGINT")
            .await?;
    }
    Ok(())
}

async fn backfill_user_subsonic_access<C: ConnectionTrait>(db: &C) -> anyhow::Result<()> {
    let sql = match db.get_database_backend() {
        DbBackend::Sqlite => {
            "INSERT OR IGNORE INTO user_subsonic_access(\
             user_id,ldap_authenticated,settings_role,stream_role,jukebox_role,download_role,\
             upload_role,playlist_role,cover_art_role,comment_role,podcast_role,share_role,\
             video_conversion_role,max_bit_rate,folder_ids) \
             SELECT id,0,CASE WHEN role='admin' THEN 1 ELSE 0 END,1,0,1,\
             CASE WHEN role='admin' THEN 1 ELSE 0 END,1,1,1,0,1,0,0,'*' FROM users"
        }
        DbBackend::Postgres => {
            "INSERT INTO user_subsonic_access(\
             user_id,ldap_authenticated,settings_role,stream_role,jukebox_role,download_role,\
             upload_role,playlist_role,cover_art_role,comment_role,podcast_role,share_role,\
             video_conversion_role,max_bit_rate,folder_ids) \
             SELECT id,0,CASE WHEN role='admin' THEN 1 ELSE 0 END,1,0,1,\
             CASE WHEN role='admin' THEN 1 ELSE 0 END,1,1,1,0,1,0,0,'*' FROM users \
             ON CONFLICT(user_id) DO NOTHING"
        }
        other => anyhow::bail!("unsupported database backend: {other:?}"),
    };
    db.execute_unprepared(sql).await?;
    Ok(())
}

async fn has_column<C: ConnectionTrait>(db: &C, table: &str, column: &str) -> anyhow::Result<bool> {
    let backend = db.get_database_backend();
    let sql = match backend {
        DbBackend::Sqlite => format!(
            "SELECT COUNT(*) AS column_count FROM pragma_table_info('{table}') WHERE name='{column}'"
        ),
        DbBackend::Postgres => format!(
            "SELECT COUNT(*) AS column_count FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='{table}' AND column_name='{column}'"
        ),
        other => anyhow::bail!("unsupported database backend: {other:?}"),
    };
    let result = db
        .query_one(Statement::from_string(backend, sql))
        .await?
        .ok_or_else(|| anyhow::anyhow!("column inspection returned no rows"))?;
    Ok(result.try_get::<i64>("", "column_count")? > 0)
}

async fn applied<C: ConnectionTrait>(db: &C, version: &str) -> anyhow::Result<bool> {
    Ok(schema_migration::Entity::find_by_id(version)
        .one(db)
        .await?
        .is_some())
}

async fn remove_non_baseline_versions<C: ConnectionTrait>(db: &C) -> anyhow::Result<()> {
    schema_migration::Entity::delete_many()
        .filter(schema_migration::Column::Version.ne(BASELINE))
        .exec(db)
        .await?;
    Ok(())
}

async fn record<C: ConnectionTrait>(db: &C, version: &str) -> anyhow::Result<()> {
    schema_migration::ActiveModel {
        version: Set(version.to_owned()),
        applied_at: Set(Utc::now().to_rfc3339()),
    }
    .insert(db)
    .await?;
    Ok(())
}

async fn execute<C, S>(db: &C, statement: S) -> anyhow::Result<()>
where
    C: ConnectionTrait,
    S: StatementBuilder,
{
    db.execute(db.get_database_backend().build(&statement))
        .await?;
    Ok(())
}

async fn create_index<C: ConnectionTrait>(
    db: &C,
    name: &str,
    table: &str,
    columns: &[&str],
    unique: bool,
) -> anyhow::Result<()> {
    let mut index = Index::create();
    index.name(name).table(alias(table)).if_not_exists();
    for column in columns {
        index.col(alias(column));
    }
    if unique {
        index.unique();
    }
    execute(db, index.to_owned()).await
}

fn create_table(name: &str) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(alias(name)).if_not_exists();
    table.to_owned()
}

fn alias(name: &str) -> Alias {
    Alias::new(name)
}

fn text(name: &str) -> ColumnDef {
    let mut column = ColumnDef::new(alias(name));
    column.text();
    column
}

fn required_text(name: &str) -> ColumnDef {
    let mut column = text(name);
    column.not_null();
    column
}

fn text_primary_key(name: &str) -> ColumnDef {
    let mut column = required_text(name);
    column.primary_key();
    column
}

fn text_default(name: &str, value: &str) -> ColumnDef {
    let mut column = required_text(name);
    column.default(value);
    column
}

fn required_bigint(name: &str) -> ColumnDef {
    let mut column = ColumnDef::new(alias(name));
    column.big_integer().not_null();
    column
}

fn bigint(name: &str) -> ColumnDef {
    let mut column = ColumnDef::new(alias(name));
    column.big_integer();
    column
}

fn bigint_default(name: &str, value: i64) -> ColumnDef {
    let mut column = required_bigint(name);
    column.default(value);
    column
}

fn double_default(name: &str, value: f64) -> ColumnDef {
    let mut column = ColumnDef::new(alias(name));
    column.double().not_null().default(value);
    column
}
