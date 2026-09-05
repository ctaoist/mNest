use std::{panic::AssertUnwindSafe, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, ensure};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use chrono::Utc;
use futures::{FutureExt, future::join_all};
use mnest::{
    AppState, api,
    auth::{authenticate_subsonic, reveal_subsonic_api_key},
    config::{DatabaseSettings, Settings},
    db,
    entities::{
        album, artist, music_folder, play_queue, playback_state, schema_migration, scrobble, track,
        track_artist, user, user_subsonic_access, user_track_stat,
    },
    providers::ProviderRegistry,
};
use reqwest::Url;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait,
    FromQueryResult, IntoActiveModel, PaginatorTrait, QueryFilter, Set,
};
use serde_json::Value;
use tower::ServiceExt;

const CONFIG_ENV: &str = "MNEST_VERIFY_CONFIG";
const SCHEMA_ENV: &str = "MNEST_VERIFY_SCHEMA";
const CLEANUP_ENV: &str = "MNEST_VERIFY_CLEANUP_ONLY";
const TEST_SECRET: &str = "postgres-live-test-secret-at-least-32-chars";

#[derive(Debug, FromQueryResult)]
struct TextValue {
    value: String,
}

#[derive(Debug, FromQueryResult)]
struct CountValue {
    value: i64,
}

// This test intentionally requires an explicit live PostgreSQL configuration and is ignored by
// ordinary test runs. It creates only a caller-named, isolated schema and removes it afterwards.
#[test]
#[ignore = "requires MNEST_VERIFY_CONFIG and an isolated PostgreSQL schema"]
fn verifies_postgres_in_an_isolated_schema() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build PostgreSQL verification runtime");
    if let Err(error) = runtime.block_on(run()) {
        panic!("isolated PostgreSQL verification failed: {error:#}");
    }
}

async fn run() -> anyhow::Result<()> {
    let config_path = std::env::var(CONFIG_ENV).context("MNEST_VERIFY_CONFIG is required")?;
    let schema = std::env::var(SCHEMA_ENV).context("MNEST_VERIFY_SCHEMA is required")?;
    validate_schema_name(&schema)?;

    let production_settings = Settings::load(Path::new(&config_path))
        .map_err(|_| anyhow::anyhow!("could not load PostgreSQL settings (details redacted)"))?;
    ensure!(
        production_settings.database.driver == "postgres",
        "configured database driver is not postgres"
    );

    let admin_settings = DatabaseSettings {
        driver: "postgres".into(),
        url: production_settings.database.url.clone(),
        max_connections: 1,
    };
    let admin = connect_redacted(&admin_settings, "administrator connection").await?;

    if std::env::var_os(CLEANUP_ENV).is_some() {
        drop_and_confirm_schema(&admin, &schema).await?;
        admin
            .close()
            .await
            .map_err(|_| anyhow::anyhow!("could not close cleanup connection"))?;
        println!("CLEANUP_CONFIRMED schema_removed=true");
        return Ok(());
    }

    // Build and validate the isolated DSN before creating anything, so a malformed URL cannot
    // leave an empty verification schema behind.
    let isolated_url = isolated_database_url(&production_settings.database.url, &schema)?;
    admin
        .execute_unprepared(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .map_err(|_| anyhow::anyhow!("could not create isolated schema"))?;

    let verification = AssertUnwindSafe(verify_isolated_database(isolated_url, &schema))
        .catch_unwind()
        .await;
    let cleanup = drop_and_confirm_schema(&admin, &schema).await;
    let close = admin.close().await;

    if let Err(error) = cleanup {
        return Err(error.context("isolated schema cleanup failed"));
    }
    close.map_err(|_| anyhow::anyhow!("could not close administrator connection"))?;
    match verification {
        Ok(result) => result?,
        Err(_) => anyhow::bail!("isolated verification panicked (details redacted)"),
    }
    println!("CLEANUP_CONFIRMED schema_removed=true");
    Ok(())
}

fn validate_schema_name(schema: &str) -> anyhow::Result<()> {
    ensure!(
        schema.starts_with("mnest_verify_")
            && schema.len() <= 63
            && schema
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
        "unsafe isolated schema name"
    );
    Ok(())
}

fn isolated_database_url(raw: &str, schema: &str) -> anyhow::Result<String> {
    let mut url = Url::parse(raw)
        .map_err(|_| anyhow::anyhow!("could not parse PostgreSQL URL (details redacted)"))?;
    ensure!(
        matches!(url.scheme(), "postgres" | "postgresql"),
        "database URL is not PostgreSQL"
    );

    let mut retained = Vec::new();
    let mut options = Vec::new();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "options" => options.push(value.into_owned()),
            "application_name" => {}
            _ => retained.push((key.into_owned(), value.into_owned())),
        }
    }
    options.push(format!("-csearch_path={schema}"));
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in retained {
            query.append_pair(&key, &value);
        }
        query.append_pair("options", &options.join(" "));
        query.append_pair("application_name", "mnest-isolated-verifier");
    }
    Ok(url.to_string())
}

async fn connect_redacted(
    settings: &DatabaseSettings,
    stage: &str,
) -> anyhow::Result<DatabaseConnection> {
    db::connect(settings)
        .await
        .map_err(|_| anyhow::anyhow!("{stage} failed (details redacted)"))
}

async fn drop_and_confirm_schema(db: &DatabaseConnection, schema: &str) -> anyhow::Result<()> {
    validate_schema_name(schema)?;
    db.execute_unprepared(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
        .await
        .map_err(|_| anyhow::anyhow!("could not drop isolated schema"))?;
    let remaining = db::raw(
        db,
        "SELECT COUNT(*)::BIGINT AS value FROM information_schema.schemata WHERE schema_name=$1",
    )
    .bind(schema.to_owned())
    .one::<CountValue>()
    .await
    .map_err(|_| anyhow::anyhow!("could not confirm isolated schema cleanup"))?
    .context("schema cleanup confirmation returned no row")?;
    ensure!(remaining.value == 0, "isolated schema still exists");
    Ok(())
}

async fn verify_isolated_database(url: String, schema: &str) -> anyhow::Result<()> {
    let settings = DatabaseSettings {
        driver: "postgres".into(),
        url,
        max_connections: 8,
    };
    let database = connect_redacted(&settings, "isolated connection").await?;
    let result = verify_on_connection(&database, &settings, schema).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let close = database.close().await;
    result?;
    close.map_err(|_| anyhow::anyhow!("could not close isolated connection"))?;
    Ok(())
}

async fn verify_on_connection(
    database: &DatabaseConnection,
    database_settings: &DatabaseSettings,
    schema: &str,
) -> anyhow::Result<()> {
    let current_schema = db::raw(database, "SELECT current_schema() AS value")
        .one::<TextValue>()
        .await
        .map_err(|_| anyhow::anyhow!("could not inspect current schema"))?
        .context("current_schema returned no row")?;
    ensure!(
        current_schema.value == schema,
        "isolated connection did not select the requested schema"
    );
    println!("PASS search_path_isolated");

    verify_fresh_and_upgrade_migrations(database).await?;
    println!("PASS migrations_fresh_and_baseline3_upgrade");

    verify_api_and_postgres_upserts(database, database_settings).await?;
    println!("PASS api_acl_upserts_and_concurrent_playback");
    Ok(())
}

async fn verify_fresh_and_upgrade_migrations(database: &DatabaseConnection) -> anyhow::Result<()> {
    db::migrate(database)
        .await
        .context("fresh migration failed")?;
    ensure!(
        schema_migration::Entity::find_by_id("baseline-4")
            .one(database)
            .await?
            .is_some(),
        "baseline-4 was not recorded"
    );
    for table in [
        "user_subsonic_access",
        "playback_states",
        "user_track_stats",
        "play_queues",
    ] {
        ensure!(has_table(database, table).await?, "missing table {table}");
    }
    ensure!(
        has_column(database, "play_queues", "current_index").await?,
        "missing play_queues.current_index"
    );

    user::ActiveModel {
        id: Set("legacy-user".into()),
        username: Set("legacy-user".into()),
        password_hash: Set("unused".into()),
        email: Set(String::new()),
        role: Set("admin".into()),
        subsonic_token: Set(String::new()),
        subsonic_password: Set(String::new()),
        created_at: Set("2026-08-01T00:00:00Z".into()),
    }
    .insert(database)
    .await?;
    for (id, played_at, submission) in [
        ("legacy-play", "2026-08-02T00:00:00Z", 1),
        ("legacy-now-playing", "2026-08-03T00:00:00Z", 0),
    ] {
        scrobble::ActiveModel {
            id: Set(id.into()),
            user_id: Set("legacy-user".into()),
            track_id: Set("legacy-track".into()),
            played_at: Set(played_at.into()),
            submission: Set(submission),
        }
        .insert(database)
        .await?;
    }
    database
        .execute_unprepared("DROP TABLE user_subsonic_access")
        .await?;
    database
        .execute_unprepared("DROP TABLE playback_states")
        .await?;
    database
        .execute_unprepared("DROP TABLE user_track_stats")
        .await?;
    database
        .execute_unprepared("ALTER TABLE play_queues DROP COLUMN current_index")
        .await?;
    schema_migration::Entity::delete_many()
        .exec(database)
        .await?;
    schema_migration::ActiveModel {
        version: Set("baseline-3".into()),
        applied_at: Set(Utc::now().to_rfc3339()),
    }
    .insert(database)
    .await?;

    db::migrate(database)
        .await
        .context("baseline-3 to baseline-4 migration failed")?;
    ensure!(
        schema_migration::Entity::find().count(database).await? == 1,
        "migration history was not normalized"
    );
    ensure!(
        schema_migration::Entity::find_by_id("baseline-4")
            .one(database)
            .await?
            .is_some(),
        "baseline-4 was not recorded after upgrade"
    );
    let access = user_subsonic_access::Entity::find_by_id("legacy-user")
        .one(database)
        .await?
        .context("legacy user access was not backfilled")?;
    ensure!(access.settings_role == 1, "admin role was not backfilled");
    let stats =
        user_track_stat::Entity::find_by_id(("legacy-user".to_owned(), "legacy-track".to_owned()))
            .one(database)
            .await?
            .context("legacy scrobble statistics were not backfilled")?;
    ensure!(
        stats.play_count == 1 && stats.last_played_at == "2026-08-02T00:00:00Z",
        "legacy scrobble backfill was incorrect"
    );
    ensure!(
        has_column(database, "play_queues", "current_index").await?,
        "current_index was not restored"
    );
    Ok(())
}

async fn has_table(database: &DatabaseConnection, table: &str) -> anyhow::Result<bool> {
    let count = db::raw(
        database,
        "SELECT COUNT(*)::BIGINT AS value FROM information_schema.tables WHERE table_schema=current_schema() AND table_name=$1",
    )
    .bind(table.to_owned())
    .one::<CountValue>()
    .await?
    .context("table inspection returned no row")?;
    Ok(count.value == 1)
}

async fn has_column(
    database: &DatabaseConnection,
    table: &str,
    column: &str,
) -> anyhow::Result<bool> {
    let count = db::raw(
        database,
        "SELECT COUNT(*)::BIGINT AS value FROM information_schema.columns WHERE table_schema=current_schema() AND table_name=$1 AND column_name=$2",
    )
    .bind(table.to_owned())
    .bind(column.to_owned())
    .one::<CountValue>()
    .await?
    .context("column inspection returned no row")?;
    Ok(count.value == 1)
}

async fn verify_api_and_postgres_upserts(
    database: &DatabaseConnection,
    database_settings: &DatabaseSettings,
) -> anyhow::Result<()> {
    let mut settings = Settings::default();
    settings.auth.jwt_secret = TEST_SECRET.into();
    settings.database = database_settings.clone();
    settings.scraper.enabled.clear();
    settings.cover_cache.enabled = false;
    settings.admin.username = "pg-live-admin".into();
    settings.admin.password = "isolated-test-password".into();
    settings.admin.email = "pg-live@example.invalid".into();
    settings.admin.overwrite_existing = false;
    db::bootstrap_admin(database, &settings.admin, TEST_SECRET).await?;

    let account = user::Entity::find()
        .filter(user::Column::Username.eq("pg-live-admin"))
        .one(database)
        .await?
        .context("test administrator was not bootstrapped")?;
    let api_key = reveal_subsonic_api_key(&account.subsonic_token, TEST_SECRET, &account.id)?;
    ensure!(
        account.subsonic_token.starts_with("k1:") && !account.subsonic_token.contains(&api_key),
        "API key was not protected at rest"
    );
    ensure!(
        authenticate_subsonic(
            database,
            &std::collections::HashMap::from([("apiKey".into(), api_key.clone())]),
            TEST_SECRET,
        )
        .await?
        .is_some(),
        "protected API key lookup failed"
    );

    insert_catalog(database).await?;
    let settings = Arc::new(settings);
    let providers = Arc::new(ProviderRegistry::new(settings.clone()));
    let state = AppState::new(settings, database.clone(), providers);
    let app = api::router(state);

    let extensions = request_json(app.clone(), "/rest/getOpenSubsonicExtensions?f=json").await?;
    ensure_api_ok(&extensions)?;
    let extension_names = extensions["subsonic-response"]["openSubsonicExtensions"]
        .as_array()
        .context("extension response was not an array")?
        .iter()
        .filter_map(|extension| extension["name"].as_str())
        .collect::<Vec<_>>();
    for expected in [
        "apiKeyAuthentication",
        "formPost",
        "playbackReport",
        "indexBasedQueue",
    ] {
        ensure!(
            extension_names.contains(&expected),
            "extension {expected} was not advertised"
        );
    }

    for max_bit_rate in [128, 320] {
        let uri = api_uri(
            "updateUser",
            &api_key,
            &format!(
                "&username=pg-live-admin&musicFolderId=folder-allowed&downloadRole=true&maxBitRate={max_bit_rate}"
            ),
        );
        let response = request_json(app.clone(), &uri).await?;
        ensure_api_ok(&response)?;
    }
    let access = user_subsonic_access::Entity::find_by_id(&account.id)
        .one(database)
        .await?
        .context("user access UPSERT did not create a row")?;
    ensure!(
        access.max_bit_rate == 320
            && access.download_role == 1
            && access.folder_ids == "[\"folder-allowed\"]",
        "user access UPSERT did not update the existing row"
    );

    let artists_response = request_json(app.clone(), &api_uri("getArtists", &api_key, "")).await?;
    ensure_api_ok(&artists_response)?;
    let returned_artists = artists_response["subsonic-response"]["artists"]["index"]
        .as_array()
        .context("artist indexes were not returned")?
        .iter()
        .flat_map(|index| index["artist"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    ensure!(
        returned_artists.len() == 1
            && returned_artists[0]["id"] == "artist-allowed"
            && returned_artists[0]["albumCount"] == 1
            && returned_artists[0]["coverArt"] == "img-album-allowed",
        "PostgreSQL artist aggregation did not enforce folder ACL and enabled state"
    );

    let first_queue = request_json(
        app.clone(),
        &api_uri(
            "savePlayQueueByIndex",
            &api_key,
            "&id=track-allowed,track-allowed,track-allowed&currentIndex=2&position=100",
        ),
    )
    .await?;
    ensure_api_ok(&first_queue)?;
    let second_queue = request_json(
        app.clone(),
        &api_uri(
            "savePlayQueueByIndex",
            &api_key,
            "&id=track-allowed,track-allowed&currentIndex=1&position=200",
        ),
    )
    .await?;
    ensure_api_ok(&second_queue)?;
    let queue = play_queue::Entity::find_by_id(&account.id)
        .one(database)
        .await?
        .context("queue UPSERT did not persist a row")?;
    ensure!(
        queue.current_index == Some(1) && queue.position == 200,
        "queue UPSERT did not update current_index and position"
    );

    let playback_uri = api_uri(
        "reportPlayback",
        &api_key,
        "&mediaId=track-allowed&mediaType=song&positionMs=60000&state=playing",
    );
    let concurrent = (0..8)
        .map(|_| request_json(app.clone(), &playback_uri))
        .collect::<Vec<_>>();
    for response in join_all(concurrent).await {
        ensure_api_ok(&response?)?;
    }
    verify_play_counts(database, &account.id, 1).await?;

    for tail in [
        "&mediaId=track-allowed&mediaType=song&positionMs=0&state=starting",
        "&mediaId=track-allowed&mediaType=song&positionMs=60000&state=playing",
    ] {
        let response =
            request_json(app.clone(), &api_uri("reportPlayback", &api_key, tail)).await?;
        ensure_api_ok(&response)?;
    }
    verify_play_counts(database, &account.id, 2).await?;
    let playback = playback_state::Entity::find_by_id(&account.id)
        .one(database)
        .await?
        .context("playback state UPSERT did not persist a row")?;
    ensure!(
        playback.media_id == "track-allowed" && playback.scrobbled == 1,
        "playback state UPSERT produced incorrect state"
    );
    Ok(())
}

async fn verify_play_counts(
    database: &DatabaseConnection,
    user_id: &str,
    expected: i64,
) -> anyhow::Result<()> {
    let submissions = scrobble::Entity::find()
        .filter(scrobble::Column::UserId.eq(user_id))
        .filter(scrobble::Column::TrackId.eq("track-allowed"))
        .filter(scrobble::Column::Submission.eq(1))
        .count(database)
        .await?;
    let stats =
        user_track_stat::Entity::find_by_id((user_id.to_owned(), "track-allowed".to_owned()))
            .one(database)
            .await?
            .context("per-user track statistics were not persisted")?;
    let track = track::Entity::find_by_id("track-allowed")
        .one(database)
        .await?
        .context("test track disappeared")?;
    ensure!(
        submissions == expected as u64
            && stats.play_count == expected
            && track.play_count == expected,
        "atomic scrobble claim or play-count UPSERT was incorrect"
    );
    Ok(())
}

async fn insert_catalog(database: &DatabaseConnection) -> anyhow::Result<()> {
    for (id, enabled) in [("folder-allowed", 1), ("folder-disabled", 0)] {
        music_folder::ActiveModel {
            id: Set(id.into()),
            name: Set(id.into()),
            path: Set(format!("/isolated/{id}")),
            enabled: Set(enabled),
        }
        .insert(database)
        .await?;
    }
    for (id, name) in [
        ("artist-allowed", "Allowed Artist"),
        ("artist-disabled", "Disabled Artist"),
    ] {
        artist::ActiveModel {
            id: Set(id.into()),
            name: Set(name.into()),
            sort_name: Set(name.to_lowercase()),
            cover_path: Set(None),
            album_count: Set(99),
            song_count: Set(99),
        }
        .insert(database)
        .await?;
    }
    for (id, artist_id, name) in [
        ("album-allowed", "artist-allowed", "Allowed Album"),
        ("album-disabled", "artist-disabled", "Disabled Album"),
    ] {
        album::ActiveModel {
            id: Set(id.into()),
            name: Set(name.into()),
            artist_id: Set(artist_id.into()),
            artist_name: Set(name.into()),
            year: Set(2026),
            genre: Set(String::new()),
            cover_path: Set(None),
            song_count: Set(1),
            duration: Set(120.0),
            created_at: Set("2026-08-01T00:00:00Z".into()),
        }
        .insert(database)
        .await?;
    }
    for (id, folder_id, artist_id, album_id) in [
        (
            "track-allowed",
            "folder-allowed",
            "artist-allowed",
            "album-allowed",
        ),
        (
            "track-disabled",
            "folder-disabled",
            "artist-disabled",
            "album-disabled",
        ),
    ] {
        track_model(id, folder_id, artist_id, album_id)
            .into_active_model()
            .insert(database)
            .await?;
        track_artist::ActiveModel {
            track_id: Set(id.into()),
            artist_id: Set(artist_id.into()),
            position: Set(0),
        }
        .insert(database)
        .await?;
    }
    Ok(())
}

fn track_model(id: &str, folder_id: &str, artist_id: &str, album_id: &str) -> track::Model {
    track::Model {
        id: id.into(),
        folder_id: folder_id.into(),
        path: format!("/isolated/{folder_id}/{id}.flac"),
        relative_path: format!("{id}.flac"),
        title: id.into(),
        artist_id: artist_id.into(),
        artist_name: artist_id.into(),
        artists_json: format!(r#"[{{"id":"{artist_id}","name":"{artist_id}"}}]"#),
        album_id: Some(album_id.into()),
        album_name: album_id.into(),
        album_artist: artist_id.into(),
        genre: String::new(),
        language: String::new(),
        year: 2026,
        track_number: 1,
        disc_number: 1,
        duration: 120.0,
        bit_rate: 320,
        size: 1,
        suffix: "flac".into(),
        mimetype: "audio/flac".into(),
        lyrics: String::new(),
        comment: String::new(),
        cover_path: None,
        mtime: 1,
        fingerprint: String::new(),
        play_count: 0,
        needs_scrape: 0,
        created_at: "2026-08-01T00:00:00Z".into(),
        updated_at: "2026-08-01T00:00:00Z".into(),
    }
}

fn api_uri(method: &str, api_key: &str, tail: &str) -> String {
    format!("/rest/{method}?apiKey={api_key}&v=1.16.1&c=pg-live&f=json{tail}")
}

async fn request_json(app: Router, uri: &str) -> anyhow::Result<Value> {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty())?)
        .await?;
    let body = to_bytes(response.into_body(), 2 * 1024 * 1024).await?;
    Ok(serde_json::from_slice(&body)?)
}

fn ensure_api_ok(response: &Value) -> anyhow::Result<()> {
    ensure!(
        response["subsonic-response"]["status"] == "ok",
        "OpenSubsonic request failed: {}",
        response["subsonic-response"]["error"]["code"]
    );
    Ok(())
}
