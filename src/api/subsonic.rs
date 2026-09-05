use std::{
    collections::{HashMap, HashSet},
    io::{self, SeekFrom},
    path::{Path as FsPath, PathBuf},
    time::Duration,
};

use anyhow::Context;
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use axum::{
    Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{TimeZone, Utc};
use futures::StreamExt;
use md5::{Digest, Md5};
use rand_core::OsRng;
use reqwest::Url;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseTransaction, EntityTrait, FromQueryResult,
    IntoActiveModel, QueryFilter, QueryOrder, QuerySelect, QueryTrait, Set, TransactionTrait,
    sea_query::{Expr, OnConflict, Order},
};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use uuid::Uuid;

use crate::{
    AppState,
    artist_credit::{ArtistCredit, parse_artist_names},
    auth::{
        authenticate_subsonic, encrypt_subsonic_password, protect_subsonic_api_key, user_by_name,
        web_user_from_headers,
    },
    db,
    entities::{
        album as album_entity, artist as artist_entity, bookmark as bookmark_entity,
        download_source as download_source_entity, favorite as favorite_entity,
        internet_radio_station as radio_entity, job as job_entity,
        music_folder as music_folder_entity, play_queue as play_queue_entity,
        playback_state as playback_state_entity, playlist as playlist_entity,
        playlist_track as playlist_track_entity, rating as rating_entity,
        scrobble as scrobble_entity, share as share_entity, track as track_entity,
        track_artist as track_artist_entity, user as user_entity,
        user_subsonic_access as access_entity, user_track_stat as user_track_stat_entity,
    },
    internet_radio,
    jobs::{self, ScanPayload},
    lastfm,
    media::{AudioFormat, MediaStream, TranscodeRequest},
    models::{Album, Artist, MusicFolder, Track, User},
    scanner,
    tags::MISSING_ARTWORK_MARKER,
    transcode_cache, user_preferences,
};

#[derive(FromQueryResult)]
struct IntValue {
    value: Option<i64>,
}

#[derive(FromQueryResult)]
struct ArtistCoverRow {
    artist_id: String,
    track_id: String,
    album_id: Option<String>,
}

#[derive(FromQueryResult)]
struct ArtistStatsRow {
    artist_id: String,
    album_count: i64,
    song_count: i64,
}

#[derive(FromQueryResult)]
struct AlbumStatsRow {
    album_id: String,
    song_count: i64,
    duration: f64,
}

const API_VERSION: &str = "1.16.1";
const XML_NAMESPACE: &str = "http://subsonic.org/restapi";
const MAX_COLLECTION_ITEMS: usize = 10_000;
const MAX_SCROBBLE_BATCH: usize = 1_000;
const MAX_CATALOG_MUTATION_ITEMS: usize = 1_000;

#[derive(Clone, Debug)]
struct SubsonicAccess {
    ldap_authenticated: bool,
    settings_role: bool,
    stream_role: bool,
    jukebox_role: bool,
    download_role: bool,
    upload_role: bool,
    playlist_role: bool,
    cover_art_role: bool,
    comment_role: bool,
    podcast_role: bool,
    share_role: bool,
    video_conversion_role: bool,
    max_bit_rate: i64,
    folder_ids: Option<HashSet<String>>,
}

impl SubsonicAccess {
    fn fallback(user: &User) -> Self {
        let admin = user.role == "admin";
        Self {
            ldap_authenticated: false,
            settings_role: admin,
            stream_role: true,
            jukebox_role: false,
            download_role: true,
            upload_role: admin,
            playlist_role: true,
            cover_art_role: true,
            comment_role: true,
            podcast_role: false,
            share_role: true,
            video_conversion_role: false,
            max_bit_rate: 0,
            folder_ids: None,
        }
    }

    fn from_model(model: access_entity::Model) -> Self {
        Self {
            ldap_authenticated: model.ldap_authenticated != 0,
            settings_role: model.settings_role != 0,
            stream_role: model.stream_role != 0,
            jukebox_role: model.jukebox_role != 0,
            download_role: model.download_role != 0,
            upload_role: model.upload_role != 0,
            playlist_role: model.playlist_role != 0,
            cover_art_role: model.cover_art_role != 0,
            comment_role: model.comment_role != 0,
            podcast_role: model.podcast_role != 0,
            share_role: model.share_role != 0,
            video_conversion_role: model.video_conversion_role != 0,
            max_bit_rate: model.max_bit_rate,
            folder_ids: (model.folder_ids != "*").then(|| {
                serde_json::from_str::<Vec<String>>(&model.folder_ids)
                    .unwrap_or_default()
                    .into_iter()
                    .collect()
            }),
        }
    }

    fn allows_folder(&self, folder_id: &str) -> bool {
        self.folder_ids
            .as_ref()
            .is_none_or(|folder_ids| folder_ids.contains(folder_id))
    }
}

async fn subsonic_access(state: &AppState, user: &User) -> Result<SubsonicAccess, ApiFailure> {
    Ok(access_entity::Entity::find_by_id(&user.id)
        .one(&state.db)
        .await?
        .map(SubsonicAccess::from_model)
        .unwrap_or_else(|| SubsonicAccess::fallback(user)))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rest/{method}", get(dispatch).post(dispatch))
        .route("/share/{id}", get(public_share))
        .route("/share/{id}/{track_id}", get(public_share_media))
}

async fn public_share(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let share = match active_share(&state, &id).await {
        Ok(Some(share)) => share,
        Ok(None) => return (StatusCode::NOT_FOUND, "Share not found or expired").into_response(),
        Err(error) => {
            tracing::error!(%error, share_id = %id, "failed to load public share");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let tracks = match shared_tracks(&state, &share).await {
        Ok(tracks) => tracks,
        Err(error) => {
            tracing::error!(%error, share_id = %id, "failed to load shared tracks");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let items = tracks
        .iter()
        .map(|track| {
            let artists = serde_json::from_str::<Vec<ArtistCredit>>(&track.artists_json)
                .unwrap_or_default()
                .into_iter()
                .map(|artist| artist.name)
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "<article><strong>{}</strong><span>{}</span><audio controls preload='none' src='/share/{}/{}'></audio></article>",
                xml_escape(&track.title),
                xml_escape(&artists),
                urlencoding::encode(&share.id),
                urlencoding::encode(&track.id),
            )
        })
        .collect::<String>();
    let html = format!(
        "<!doctype html><html lang='zh-CN'><head><meta charset='utf-8'><meta name='viewport' content='width=device-width,initial-scale=1'><title>{}</title><style>body{{font:16px system-ui,sans-serif;max-width:760px;margin:40px auto;padding:0 20px;color:#172033;background:#f7f8fa}}main{{display:grid;gap:14px}}article{{display:grid;gap:6px;padding:16px;background:#fff;border:1px solid #e4e7ec;border-radius:12px}}span{{color:#667085}}audio{{width:100%}}</style></head><body><main><h1>{}</h1>{}</main></body></html>",
        xml_escape(if share.description.is_empty() {
            "Music share"
        } else {
            &share.description
        }),
        xml_escape(if share.description.is_empty() {
            "Music share"
        } else {
            &share.description
        }),
        items,
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

async fn public_share_media(
    State(state): State<AppState>,
    Path((id, track_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let share = match active_share(&state, &id).await {
        Ok(Some(share)) => share,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let tracks = match shared_tracks(&state, &share).await {
        Ok(tracks) => tracks,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Some(track) = tracks.into_iter().find(|track| track.id == track_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let _ = share_entity::Entity::update_many()
        .col_expr(
            share_entity::Column::PlayCount,
            Expr::col(share_entity::Column::PlayCount).add(1),
        )
        .col_expr(
            share_entity::Column::LastVisitedAt,
            Expr::value(Utc::now().to_rfc3339()),
        )
        .filter(share_entity::Column::Id.eq(&share.id))
        .exec(&state.db)
        .await;
    match serve_file(
        PathBuf::from(track.path),
        false,
        headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
        state.shutdown.clone(),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn active_share(
    state: &AppState,
    id: &str,
) -> Result<Option<share_entity::Model>, sea_orm::DbErr> {
    Ok(share_entity::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .filter(|share| {
            share.expires_at.as_deref().is_none_or(|expires| {
                chrono::DateTime::parse_from_rfc3339(expires)
                    .map(|expires| expires.with_timezone(&Utc) > Utc::now())
                    .unwrap_or(false)
            })
        }))
}

async fn shared_tracks(
    state: &AppState,
    share: &share_entity::Model,
) -> Result<Vec<Track>, sea_orm::DbErr> {
    let owner = user_entity::Entity::find_by_id(&share.user_id)
        .one(&state.db)
        .await?;
    let access = if let Some(owner) = owner {
        access_entity::Entity::find_by_id(&owner.id)
            .one(&state.db)
            .await?
            .map(SubsonicAccess::from_model)
            .unwrap_or_else(|| SubsonicAccess::fallback(&owner))
    } else {
        return Ok(Vec::new());
    };
    let ids: Vec<String> = serde_json::from_str(&share.item_ids).unwrap_or_default();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let requested_ids = ids.iter().cloned().collect::<HashSet<_>>();
    let candidates = accessible_tracks(&access)
        .filter(
            Condition::any()
                .add(track_entity::Column::Id.is_in(requested_ids.iter().cloned()))
                .add(track_entity::Column::AlbumId.is_in(requested_ids.iter().cloned())),
        )
        .order_by_asc(track_entity::Column::DiscNumber)
        .order_by_asc(track_entity::Column::TrackNumber)
        .order_by_asc(track_entity::Column::Title)
        .order_by_asc(track_entity::Column::Id)
        .all(&state.db)
        .await?;
    let by_id = candidates
        .iter()
        .map(|track| (track.id.clone(), track.clone()))
        .collect::<HashMap<_, _>>();
    let mut by_album = HashMap::<String, Vec<Track>>::new();
    for track in candidates {
        if let Some(album_id) = &track.album_id {
            by_album.entry(album_id.clone()).or_default().push(track);
        }
    }
    let mut tracks = Vec::new();
    for id in ids {
        if let Some(track) = by_id.get(&id) {
            tracks.push(track.clone());
            continue;
        }
        if let Some(album_tracks) = by_album.get(&id) {
            tracks.extend(album_tracks.iter().cloned());
        }
    }
    Ok(tracks)
}

async fn dispatch(
    State(state): State<AppState>,
    Path(method): Path<String>,
    request: Request,
) -> Response {
    let method = method.trim_end_matches(".view");
    let method = if method == "hls.m3u8" { "hls" } else { method };
    let request_base_url = request_base_url(&state, request.headers());
    let web_user = web_user_from_headers(
        &state.db,
        request.headers(),
        &state.settings.auth.jwt_secret,
    )
    .await
    .ok()
    .flatten();
    let if_none_match = request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut params = match collect_params(request).await {
        Ok(value) => value,
        Err(error) => return subsonic_error(&HashMap::new(), 10, &error.to_string()),
    };
    if let Some(base_url) = request_base_url {
        params.insert("_mnest_base_url".into(), base_url);
    }
    if method == "getOpenSubsonicExtensions" {
        return subsonic_response(&params, open_subsonic_extensions(&state).await);
    }
    if web_user.is_none()
        && let Err(error) = validate_protocol_request(&params)
    {
        return subsonic_error(&params, error.code, &error.message);
    }
    let user = match web_user {
        Some(user) => user,
        None => match validate_authentication_request(&params) {
            Err(error) => return subsonic_error(&params, error.code, &error.message),
            Ok(invalid_code) => {
                match authenticate_subsonic(&state.db, &params, &state.settings.auth.jwt_secret)
                    .await
                {
                    Ok(Some(user)) => user,
                    Ok(None) => {
                        let message = if invalid_code == 44 {
                            "Invalid API key"
                        } else {
                            "Wrong username or password"
                        };
                        return subsonic_error(&params, invalid_code, message);
                    }
                    Err(error) => return subsonic_error(&params, invalid_code, &error.to_string()),
                }
            }
        },
    };
    let access = match subsonic_access(&state, &user).await {
        Ok(access) => access,
        Err(error) => return subsonic_error(&params, error.code, &error.message),
    };
    if method == "stream" && !access.stream_role {
        return subsonic_error(&params, 50, "Streaming role required");
    }
    if method == "download" && !access.download_role {
        return subsonic_error(&params, 50, "Download role required");
    }

    if matches!(method, "stream" | "download" | "getCoverArt" | "getAvatar") {
        return match binary_endpoint(
            &state,
            &user,
            &access,
            method,
            &params,
            if_none_match.as_deref(),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => subsonic_error(&params, 70, &error.to_string()),
        };
    }

    match json_endpoint(&state, &user, &access, method, &params).await {
        Ok(value) => subsonic_response(&params, value),
        Err(ApiFailure { code, message }) => subsonic_error(&params, code, &message),
    }
}

async fn collect_params(request: Request) -> anyhow::Result<HashMap<String, String>> {
    let range = request
        .headers()
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut params = request
        .uri()
        .query()
        .map(|query| decode_params(query.as_bytes()))
        .transpose()?
        .unwrap_or_default();
    if request.method() == axum::http::Method::POST {
        let body = axum::body::to_bytes(request.into_body(), 2 * 1024 * 1024).await?;
        if !body.is_empty() {
            let post = decode_params(&body)?;
            params.extend(post);
        }
    }
    if let Some(range) = range {
        params.insert("_range".into(), range);
    }
    Ok(params)
}

fn request_base_url(state: &AppState, headers: &HeaderMap) -> Option<String> {
    if let Some(public_url) = state
        .settings
        .server
        .public_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(normalized_http_base_url)
    {
        return Some(public_url);
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| matches!(*value, "http" | "https"))
        .unwrap_or("http");
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))?;
    normalized_http_base_url(&format!("{scheme}://{host}"))
}

fn normalized_http_base_url(value: &str) -> Option<String> {
    Url::parse(value)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .map(|url| url.as_str().trim_end_matches('/').to_owned())
}

fn decode_params(encoded: &[u8]) -> anyhow::Result<HashMap<String, String>> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(encoded)?;
    let mut params = HashMap::new();
    for (key, value) in pairs {
        params
            .entry(key)
            .and_modify(|stored: &mut String| {
                stored.push('\0');
                stored.push_str(&value);
            })
            .or_insert(value);
    }
    Ok(params)
}

fn validate_protocol_request(p: &HashMap<String, String>) -> Result<(), ApiFailure> {
    let version = required(p, "v")?;
    required(p, "c")?;
    let client = parse_api_version(version)
        .ok_or_else(|| ApiFailure::new(10, "Invalid protocol version"))?;
    let server = parse_api_version(API_VERSION).expect("static API version must be valid");
    if client > server {
        return Err(ApiFailure::new(
            30,
            "Server must upgrade to a newer Subsonic REST protocol version",
        ));
    }
    if client.0 != server.0 {
        return Err(ApiFailure::new(
            20,
            "Client must upgrade to a compatible Subsonic REST protocol version",
        ));
    }
    if let Some(format) = p.get("f")
        && !matches!(format.as_str(), "json" | "xml")
    {
        return Err(ApiFailure::new(10, "Response format must be json or xml"));
    }
    Ok(())
}

fn parse_api_version(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

fn validate_authentication_request(p: &HashMap<String, String>) -> Result<i32, ApiFailure> {
    let api_key = p.contains_key("apiKey");
    let password = p.contains_key("p");
    let token = p.contains_key("t") || p.contains_key("s");
    if api_key && (p.contains_key("u") || password || token) || password && token {
        return Err(ApiFailure::new(
            43,
            "Multiple conflicting authentication mechanisms provided",
        ));
    }
    if api_key {
        required(p, "apiKey")?;
        return Ok(44);
    }
    if password {
        required(p, "u")?;
        required(p, "p")?;
        return Ok(40);
    }
    if token {
        required(p, "u")?;
        required(p, "t")?;
        required(p, "s")?;
        return Ok(40);
    }
    Err(ApiFailure::new(
        42,
        "Provided authentication mechanism is not supported",
    ))
}

async fn open_subsonic_extensions(state: &AppState) -> Value {
    let radio_recognition = download_source_entity::Entity::find()
        .filter(download_source_entity::Column::Kind.eq("netease"))
        .filter(download_source_entity::Column::Enabled.eq(1))
        .one(&state.db)
        .await
        .ok()
        .flatten()
        .is_some();
    let mut extensions = vec![
        json!({"name":"apiKeyAuthentication","versions":[1]}),
        json!({"name":"formPost","versions":[1]}),
        json!({"name":"playbackReport","versions":[1]}),
        json!({"name":"songLyrics","versions":[1,2]}),
        json!({"name":"topSongsByArtistId","versions":[1]}),
        json!({"name":"transcodeOffset","versions":[1]}),
        json!({"name":"indexBasedQueue","versions":[1]}),
    ];
    if radio_recognition {
        extensions.push(json!({"name":"mnestRadioRecognition","versions":[1]}));
    }
    json!({"openSubsonicExtensions": extensions})
}

async fn json_endpoint(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    method: &str,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let mut value = match method {
        "ping" => Ok(json!({})),
        "getLicense" => Ok(
            json!({"license":{"valid":true,"email":user.email,"licenseExpires":"2099-12-31T23:59:59.000Z"}}),
        ),
        "tokenInfo" => Ok(json!({"tokenInfo":{"username":user.username}})),
        "getMusicFolders" => music_folders(state, access).await,
        "getArtists" => artists(state, access, p).await,
        "getIndexes" => indexes(state, access, p).await,
        "getArtist" => get_artist(state, access, required(p, "id")?).await,
        "getAlbum" => get_album(state, access, required(p, "id")?).await,
        "getSong" => get_song(state, user, access, required(p, "id")?).await,
        "getMusicDirectory" => music_directory(state, access, required(p, "id")?).await,
        "getGenres" => genres(state, access).await,
        "getArtistInfo" | "getArtistInfo2" => artist_info(state, access, method, p).await,
        "getAlbumInfo" | "getAlbumInfo2" => album_info(state, access, p).await,
        "getSimilarSongs" | "getSimilarSongs2" => {
            similar_songs(
                state,
                access,
                method,
                required(p, "id")?,
                int(p, "count", 50),
            )
            .await
        }
        "getTopSongs" => {
            let artist = if let Some(id) = p
                .get("id")
                .map(String::as_str)
                .filter(|value| !value.is_empty())
            {
                id
            } else {
                required(p, "artist")?
            };
            top_songs(state, access, artist, int(p, "count", 50)).await
        }
        "getAlbumList" | "getAlbumList2" => album_list(state, user, access, method, p).await,
        "getRandomSongs" => random_songs(state, access, p).await,
        "getSongsByGenre" => songs_by_genre(state, access, p).await,
        "getNowPlaying" => now_playing(state, access).await,
        "getStarred" | "getStarred2" => starred(state, user, access, method, p).await,
        "search" => legacy_search(state, access, p).await,
        "search2" | "search3" => search(state, access, method, p).await,
        "getPlaylists" => playlists(state, user, access, p).await,
        "getPlaylist" => playlist(state, user, access, required(p, "id")?).await,
        "createPlaylist" => create_playlist(state, user, access, p).await,
        "updatePlaylist" => update_playlist(state, user, access, p).await,
        "deletePlaylist" => delete_playlist(state, user, required(p, "id")?).await,
        "getLyrics" => get_lyrics_legacy(state, access, p).await,
        "getLyricsBySongId" => {
            get_lyrics_by_song(state, access, required(p, "id")?, bool_param(p, "enhanced")).await
        }
        "star" => favorite(state, user, access, p, true).await,
        "unstar" => favorite(state, user, access, p, false).await,
        "setRating" => {
            require_permission(access.comment_role, "Comment role required")?;
            set_rating(state, user, access, p).await
        }
        "scrobble" => scrobble(state, user, access, p).await,
        "reportPlayback" => report_playback(state, user, access, p).await,
        "getShares" => shares(state, user).await,
        "createShare" => {
            require_permission(access.share_role, "Share role required")?;
            create_share(state, user, access, p).await
        }
        "updateShare" => {
            require_permission(access.share_role, "Share role required")?;
            update_share(state, user, p).await
        }
        "deleteShare" => delete_share(state, user, required(p, "id")?).await,
        "getInternetRadioStations" => radio_stations(state, p).await,
        "createInternetRadioStation" => create_radio(state, user, p).await,
        "updateInternetRadioStation" => update_radio(state, user, p).await,
        "deleteInternetRadioStation" => Err(ApiFailure::new(
            50,
            "Deleting internet radio stations through OpenSubsonic is disabled",
        )),
        "getUser" => get_user(state, user, required(p, "username")?).await,
        "getUsers" => get_users(state, user).await,
        "createUser" => create_user(state, user, p).await,
        "updateUser" => update_user(state, user, p).await,
        "deleteUser" => delete_user(state, user, required(p, "username")?).await,
        "changePassword" => change_password(state, user, access, p).await,
        "getBookmarks" => bookmarks(state, user, access).await,
        "createBookmark" => create_bookmark(state, user, access, p).await,
        "deleteBookmark" => delete_bookmark(state, user, required(p, "id")?).await,
        "getPlayQueue" => get_play_queue(state, user, access, false).await,
        "getPlayQueueByIndex" => get_play_queue(state, user, access, true).await,
        "savePlayQueue" => save_play_queue(state, user, access, p, false).await,
        "savePlayQueueByIndex" => save_play_queue(state, user, access, p, true).await,
        "getScanStatus" => scan_status(state).await,
        "startScan" => start_scan(state, user).await,
        "getVideos"
        | "getVideoInfo"
        | "getCaptions"
        | "getPodcasts"
        | "getNewestPodcasts"
        | "refreshPodcasts"
        | "createPodcastChannel"
        | "deletePodcastChannel"
        | "deletePodcastEpisode"
        | "downloadPodcastEpisode"
        | "getChatMessages"
        | "addChatMessage"
        | "jukeboxControl" => Err(ApiFailure::new(
            0,
            "This server does not enable video, podcast, chat or jukebox domains",
        )),
        _ => Err(ApiFailure::new(
            0,
            format!("Endpoint {method} is not implemented"),
        )),
    }?;
    apply_user_play_counts(state, user, &mut value).await?;
    Ok(value)
}

async fn apply_user_play_counts(
    state: &AppState,
    user: &User,
    value: &mut Value,
) -> Result<(), ApiFailure> {
    let mut track_ids = HashSet::new();
    collect_track_json_ids(value, &mut track_ids);
    if track_ids.is_empty() {
        return Ok(());
    }
    let counts = user_track_stat_entity::Entity::find()
        .filter(user_track_stat_entity::Column::UserId.eq(&user.id))
        .filter(user_track_stat_entity::Column::TrackId.is_in(track_ids))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|row| (row.track_id, row.play_count))
        .collect::<HashMap<_, _>>();
    set_track_json_play_counts(value, &counts);
    Ok(())
}

fn collect_track_json_ids(value: &Value, track_ids: &mut HashSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_track_json_ids(value, track_ids);
            }
        }
        Value::Object(fields) => {
            if fields.get("isDir").and_then(Value::as_bool) == Some(false)
                && fields.contains_key("playCount")
                && let Some(id) = fields.get("id").and_then(Value::as_str)
            {
                track_ids.insert(id.to_owned());
            }
            for value in fields.values() {
                collect_track_json_ids(value, track_ids);
            }
        }
        _ => {}
    }
}

fn set_track_json_play_counts(value: &mut Value, counts: &HashMap<String, i64>) {
    match value {
        Value::Array(values) => {
            for value in values {
                set_track_json_play_counts(value, counts);
            }
        }
        Value::Object(fields) => {
            if fields.get("isDir").and_then(Value::as_bool) == Some(false)
                && fields.contains_key("playCount")
                && let Some(id) = fields.get("id").and_then(Value::as_str)
            {
                fields.insert(
                    "playCount".to_owned(),
                    json!(counts.get(id).copied().unwrap_or(0)),
                );
            }
            for value in fields.values_mut() {
                set_track_json_play_counts(value, counts);
            }
        }
        _ => {}
    }
}

async fn binary_endpoint(
    state: &AppState,
    _user: &User,
    access: &SubsonicAccess,
    method: &str,
    p: &HashMap<String, String>,
    if_none_match: Option<&str>,
) -> anyhow::Result<Response> {
    match method {
        "stream" | "download" => {
            let track_id = required_anyhow(p, "id")?;
            let track = accessible_tracks(access)
                .filter(track_entity::Column::Id.eq(track_id))
                .one(&state.db)
                .await?
                .context("Track not found")?;
            let raw = p.get("format").is_some_and(|value| value == "raw");
            let requested_format = p.get("format").filter(|value| value.as_str() != "raw");
            let requested_max_bitrate = p
                .get("maxBitRate")
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| *value > 0);
            let user_max_bitrate = u32::try_from(access.max_bit_rate)
                .ok()
                .filter(|value| *value > 0);
            let max_bitrate = match (requested_max_bitrate, user_max_bitrate) {
                (Some(requested), Some(user)) => Some(requested.min(user)),
                (requested, user) => requested.or(user),
            };
            let time_offset = p
                .get("timeOffset")
                .filter(|value| transcode_offset(value).is_some());
            if method == "download"
                || (raw && user_max_bitrate.is_none())
                || (requested_format.is_none() && max_bitrate.is_none() && time_offset.is_none())
            {
                serve_file(
                    PathBuf::from(track.path),
                    method == "download",
                    p.get("_range").map(String::as_str),
                    state.shutdown.clone(),
                )
                .await
            } else {
                transcode(
                    state,
                    &track,
                    requested_format.map(String::as_str).unwrap_or("mp3"),
                    max_bitrate,
                    time_offset.map(String::as_str),
                    p.get("_range").map(String::as_str),
                )
                .await
            }
        }
        "getCoverArt" => {
            let id = required_anyhow(p, "id")?;
            if let Some(station_id) = id.strip_prefix("radio-") {
                let station = radio_entity::Entity::find_by_id(station_id)
                    .one(&state.db)
                    .await?
                    .context("radio station not found")?;
                anyhow::ensure!(!station.cover_url.is_empty(), "radio cover not found");
                let artwork = state
                    .radio_covers
                    .get(&station.id, &station.cover_url)
                    .await?;
                let etag = radio_cover_etag(&artwork.data);
                if if_none_match.is_some_and(|value| if_none_match_matches(value, &etag)) {
                    let mut response = StatusCode::NOT_MODIFIED.into_response();
                    response
                        .headers_mut()
                        .insert(header::ETAG, HeaderValue::from_str(&etag)?);
                    response.headers_mut().insert(
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("private, no-cache"),
                    );
                    return Ok(response);
                }
                return Ok((
                    [
                        (header::CONTENT_TYPE, artwork.mime_type),
                        (header::CACHE_CONTROL, "private, no-cache".to_owned()),
                        (
                            header::HeaderName::from_static("x-content-type-options"),
                            "nosniff".to_owned(),
                        ),
                        (header::ETAG, etag),
                    ],
                    artwork.data,
                )
                    .into_response());
            }
            let image_id = id.strip_prefix("img-").context("invalid cover art id")?;
            let requested_size = p
                .get("size")
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|size| *size > 0);
            let source = if let Some(source) = accessible_tracks(access)
                .filter(track_entity::Column::AlbumId.eq(image_id))
                .filter(track_entity::Column::CoverPath.ne(MISSING_ARTWORK_MARKER))
                .order_by_asc(track_entity::Column::DiscNumber)
                .order_by_asc(track_entity::Column::TrackNumber)
                .one(&state.db)
                .await?
            {
                source
            } else if let Some(source) = accessible_tracks(access)
                .filter(track_entity::Column::AlbumId.eq(image_id))
                .filter(track_entity::Column::CoverPath.is_null())
                .order_by_asc(track_entity::Column::DiscNumber)
                .order_by_asc(track_entity::Column::TrackNumber)
                .one(&state.db)
                .await?
            {
                source
            } else if accessible_tracks(access)
                .filter(track_entity::Column::AlbumId.eq(image_id))
                .one(&state.db)
                .await?
                .is_some()
            {
                anyhow::bail!("cover art not found");
            } else {
                accessible_tracks(access)
                    .filter(track_entity::Column::Id.eq(image_id))
                    .one(&state.db)
                    .await?
                    .context("cover art source not found")?
            };
            if source.cover_path.as_deref() == Some(MISSING_ARTWORK_MARKER) {
                anyhow::bail!("cover art not found");
            }
            let etag = cover_art_etag(&source.id, source.mtime, requested_size);
            if if_none_match.is_some_and(|value| if_none_match_matches(value, &etag)) {
                let mut response = StatusCode::NOT_MODIFIED.into_response();
                response
                    .headers_mut()
                    .insert(header::ETAG, HeaderValue::from_str(&etag)?);
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static(crate::tags::ARTWORK_CACHE_CONTROL),
                );
                return Ok(response);
            }
            let tags = state.tags.clone();
            let artwork_cache_id = cover_art_cache_id(id, &source.id);
            let source_path = PathBuf::from(&source.path);
            let artwork = tokio::task::spawn_blocking(move || {
                tags.read_artwork_cached_with_size(
                    std::path::Path::new(&source.path),
                    &source.id,
                    &artwork_cache_id,
                    source.mtime,
                    requested_size,
                )
            })
            .await??;
            if let Err(error) =
                scanner::remember_artwork_statuses(&state.db, &[(source_path, artwork.is_some())])
                    .await
            {
                tracing::warn!(%error, "failed to remember embedded cover art status");
            }
            let artwork = artwork.context("cover art not found")?;
            Ok((
                [
                    (header::CONTENT_TYPE, artwork.mime_type),
                    (
                        header::CACHE_CONTROL,
                        crate::tags::ARTWORK_CACHE_CONTROL.to_owned(),
                    ),
                    (
                        header::HeaderName::from_static("x-content-type-options"),
                        "nosniff".to_owned(),
                    ),
                    (header::ETAG, etag),
                ],
                artwork.data,
            )
                .into_response())
        }
        "getAvatar" => {
            let username = required_anyhow(p, "username")?;
            let avatar_user = user_by_name(&state.db, username)
                .await?
                .context("User not found")?;
            let initial = avatar_user
                .username
                .chars()
                .next()
                .unwrap_or('M')
                .to_uppercase()
                .to_string();
            let svg = format!(
                "<svg xmlns='http://www.w3.org/2000/svg' width='256' height='256'><rect width='100%' height='100%' fill='#0f2940'/><text x='50%' y='56%' text-anchor='middle' font-size='120' fill='#e9b44c'>{}</text></svg>",
                xml_escape(&initial)
            );
            Ok(([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response())
        }
        _ => anyhow::bail!("unsupported binary endpoint"),
    }
}

fn cover_art_etag(source_id: &str, modified: i64, requested_size: Option<u32>) -> String {
    let requested_size = requested_size
        .map(|size| size.to_string())
        .unwrap_or_else(|| "original".to_owned());
    let digest =
        Md5::digest(format!("cover-v1:{source_id}:{modified}:{requested_size}").as_bytes());
    format!("W/\"{}\"", hex::encode(digest))
}

fn radio_cover_etag(data: &[u8]) -> String {
    format!("\"{}\"", hex::encode(Md5::digest(data)))
}

fn cover_art_cache_id(image_id: &str, source_id: &str) -> String {
    format!("{image_id}:{source_id}")
}

fn if_none_match_matches(value: &str, etag: &str) -> bool {
    let etag = etag.strip_prefix("W/").unwrap_or(etag);
    value.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

async fn music_folders(state: &AppState, access: &SubsonicAccess) -> Result<Value, ApiFailure> {
    let folders = enabled_music_folders(state)
        .await?
        .into_iter()
        .filter(|folder| access.allows_folder(&folder.id))
        .collect::<Vec<_>>();
    Ok(
        json!({"musicFolders":{"musicFolder":folders.into_iter().map(|folder| json!({"id":folder_api_id(&folder.id),"name":folder.name})).collect::<Vec<_>>()}}),
    )
}

async fn enabled_music_folders(state: &AppState) -> Result<Vec<MusicFolder>, ApiFailure> {
    Ok(music_folder_entity::Entity::find()
        .filter(music_folder_entity::Column::Enabled.eq(1))
        .order_by_asc(music_folder_entity::Column::Name)
        .all(&state.db)
        .await?)
}

fn accessible_tracks(access: &SubsonicAccess) -> sea_orm::Select<track_entity::Entity> {
    let enabled_folder_ids = music_folder_entity::Entity::find()
        .select_only()
        .column(music_folder_entity::Column::Id)
        .filter(music_folder_entity::Column::Enabled.eq(1))
        .into_query();
    let mut request = track_entity::Entity::find()
        .filter(track_entity::Column::FolderId.in_subquery(enabled_folder_ids));
    if let Some(folder_ids) = &access.folder_ids {
        request = request.filter(track_entity::Column::FolderId.is_in(folder_ids.iter().cloned()));
    }
    request
}

async fn accessible_track(
    state: &AppState,
    access: &SubsonicAccess,
    id: &str,
) -> Result<Track, ApiFailure> {
    accessible_tracks(access)
        .filter(track_entity::Column::Id.eq(id))
        .one(&state.db)
        .await?
        .ok_or_else(not_found)
}

async fn accessible_artist_exists(
    state: &AppState,
    access: &SubsonicAccess,
    artist_id: &str,
) -> Result<bool, ApiFailure> {
    let track_ids = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::Id)
        .into_query();
    Ok(track_artist_entity::Entity::find()
        .filter(track_artist_entity::Column::ArtistId.eq(artist_id))
        .filter(track_artist_entity::Column::TrackId.in_subquery(track_ids))
        .one(&state.db)
        .await?
        .is_some())
}

fn folder_api_id(id: &str) -> i32 {
    if let Ok(id) = id.parse::<i32>()
        && id >= 0
    {
        return id;
    }
    let digest = Md5::digest(id.as_bytes());
    let value =
        i32::from_be_bytes(digest[..4].try_into().expect("MD5 prefix has four bytes")) & i32::MAX;
    value.max(1)
}

async fn find_music_folder(
    state: &AppState,
    api_id: &str,
) -> Result<Option<MusicFolder>, ApiFailure> {
    Ok(enabled_music_folders(state)
        .await?
        .into_iter()
        .find(|folder| folder.id == api_id || folder_api_id(&folder.id).to_string() == api_id))
}

async fn requested_music_folder(
    state: &AppState,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Option<String>, ApiFailure> {
    let Some(api_id) = p.get("musicFolderId") else {
        return Ok(None);
    };
    let folder = find_music_folder(state, api_id)
        .await?
        .filter(|folder| access.allows_folder(&folder.id))
        .ok_or_else(not_found)?;
    Ok(Some(folder.id))
}

async fn library_artists(
    state: &AppState,
    access: &SubsonicAccess,
    folder_id: Option<&str>,
) -> Result<Vec<Artist>, ApiFailure> {
    let mut track_ids = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::Id);
    if let Some(folder_id) = folder_id {
        track_ids = track_ids.filter(track_entity::Column::FolderId.eq(folder_id));
    }
    let artist_ids = track_artist_entity::Entity::find()
        .select_only()
        .column(track_artist_entity::Column::ArtistId)
        .filter(track_artist_entity::Column::TrackId.in_subquery(track_ids.into_query()))
        .into_query();
    let mut artists = artist_entity::Entity::find()
        .filter(artist_entity::Column::Id.in_subquery(artist_ids))
        .order_by_asc(artist_entity::Column::SortName)
        .all(&state.db)
        .await?;
    scope_artist_stats(state, access, &mut artists, folder_id).await?;
    Ok(artists)
}

async fn scope_artist_stats(
    state: &AppState,
    access: &SubsonicAccess,
    artists: &mut [Artist],
    folder_id: Option<&str>,
) -> Result<(), ApiFailure> {
    let mut artist_ids = artists
        .iter()
        .map(|artist| artist.id.as_str())
        .collect::<Vec<_>>();
    artist_ids.sort_unstable();
    artist_ids.dedup();
    if artist_ids.is_empty() {
        return Ok(());
    }
    let allowed_folders = if let Some(folder_id) = folder_id {
        Some(vec![folder_id.to_owned()])
    } else {
        access
            .folder_ids
            .as_ref()
            .map(|folder_ids| folder_ids.iter().cloned().collect::<Vec<_>>())
    };
    let mut stats = HashMap::new();
    if !allowed_folders.as_ref().is_some_and(Vec::is_empty) {
        for chunk in artist_ids.chunks(500) {
            let artist_placeholders = (1..=chunk.len())
                .map(|index| format!("${index}"))
                .collect::<Vec<_>>()
                .join(",");
            let folder_filter = allowed_folders.as_ref().map(|folder_ids| {
                let first = chunk.len() + 1;
                let folder_placeholders = (first..first + folder_ids.len())
                    .map(|index| format!("${index}"))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(" AND t.folder_id IN ({folder_placeholders})")
            });
            let mut query = db::raw(
                &state.db,
                format!(
                    "SELECT ta.artist_id,COUNT(DISTINCT t.album_id) AS album_count,\
                     COUNT(DISTINCT t.id) AS song_count FROM track_artists ta \
                     JOIN tracks t ON t.id=ta.track_id \
                     JOIN music_folders mf ON mf.id=t.folder_id AND mf.enabled=1 \
                     WHERE ta.artist_id IN ({artist_placeholders}){} GROUP BY ta.artist_id",
                    folder_filter.unwrap_or_default()
                ),
            );
            for artist_id in chunk {
                query = query.bind(*artist_id);
            }
            if let Some(folder_ids) = &allowed_folders {
                for folder_id in folder_ids {
                    query = query.bind(folder_id.clone());
                }
            }
            for row in query.all::<ArtistStatsRow>().await? {
                stats.insert(row.artist_id, (row.album_count, row.song_count));
            }
        }
    }
    for artist in artists {
        let (album_count, song_count) = stats.get(&artist.id).copied().unwrap_or((0, 0));
        artist.album_count = album_count;
        artist.song_count = song_count;
    }
    Ok(())
}

async fn scope_album_stats(
    state: &AppState,
    access: &SubsonicAccess,
    albums: &mut [Album],
    folder_id: Option<&str>,
) -> Result<(), ApiFailure> {
    let album_ids = albums
        .iter()
        .map(|album| album.id.clone())
        .collect::<HashSet<_>>();
    if album_ids.is_empty() {
        return Ok(());
    }
    let mut request = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::AlbumId)
        .column_as(track_entity::Column::Id.count(), "song_count")
        .column_as(track_entity::Column::Duration.sum(), "duration")
        .filter(track_entity::Column::AlbumId.is_in(album_ids))
        .group_by(track_entity::Column::AlbumId);
    if let Some(folder_id) = folder_id {
        request = request.filter(track_entity::Column::FolderId.eq(folder_id));
    }
    let stats = request
        .into_model::<AlbumStatsRow>()
        .all(&state.db)
        .await?
        .into_iter()
        .map(|row| (row.album_id, (row.song_count, row.duration)))
        .collect::<HashMap<_, _>>();
    for album in albums {
        let (song_count, duration) = stats.get(&album.id).copied().unwrap_or((0, 0.0));
        album.song_count = song_count;
        album.duration = duration;
    }
    Ok(())
}

async fn artist_cover_art_map(
    state: &AppState,
    access: &SubsonicAccess,
    artist_ids: &[String],
    folder_id: Option<&str>,
) -> Result<HashMap<String, String>, ApiFailure> {
    let mut artist_ids = artist_ids.iter().map(String::as_str).collect::<Vec<_>>();
    artist_ids.sort_unstable();
    artist_ids.dedup();
    if artist_ids.is_empty() {
        return Ok(HashMap::new());
    }
    const SELECT: &str = "SELECT ta.artist_id,t.id AS track_id,t.album_id FROM track_artists ta JOIN tracks t ON t.id=ta.track_id JOIN music_folders mf ON mf.id=t.folder_id AND mf.enabled=1 AND (t.cover_path IS NULL OR t.cover_path <> '')";
    const ORDER: &str = " ORDER BY ta.artist_id,CASE WHEN t.album_id IS NULL THEN 1 ELSE 0 END,ta.position,t.album_id,t.disc_number,t.track_number,t.title,t.id";
    let mut covers = HashMap::new();
    for chunk in artist_ids.chunks(500) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(",");
        let allowed_folders = if let Some(folder_id) = folder_id {
            Some(vec![folder_id.to_owned()])
        } else {
            access
                .folder_ids
                .as_ref()
                .map(|folder_ids| folder_ids.iter().cloned().collect::<Vec<_>>())
        };
        if allowed_folders.as_ref().is_some_and(Vec::is_empty) {
            continue;
        }
        let folder_filter = allowed_folders.as_ref().map(|folder_ids| {
            let first = chunk.len() + 1;
            let placeholders = (first..first + folder_ids.len())
                .map(|index| format!("${index}"))
                .collect::<Vec<_>>()
                .join(",");
            format!(" AND t.folder_id IN ({placeholders})")
        });
        let mut query = db::raw(
            &state.db,
            format!(
                "{SELECT} WHERE ta.artist_id IN ({placeholders}){}{ORDER}",
                folder_filter.unwrap_or_default()
            ),
        );
        for artist_id in chunk {
            query = query.bind(*artist_id);
        }
        if let Some(folder_ids) = allowed_folders {
            for folder_id in folder_ids {
                query = query.bind(folder_id);
            }
        }
        for row in query.all::<ArtistCoverRow>().await? {
            covers.entry(row.artist_id).or_insert_with(|| {
                canonical_track_cover_art(&row.track_id, row.album_id.as_deref())
            });
        }
    }
    Ok(covers)
}

async fn library_last_modified(
    state: &AppState,
    access: &SubsonicAccess,
    folder_id: Option<&str>,
) -> Result<i64, ApiFailure> {
    let mut request = accessible_tracks(access)
        .select_only()
        .column_as(track_entity::Column::Mtime.max(), "value");
    if let Some(folder_id) = folder_id {
        request = request.filter(track_entity::Column::FolderId.eq(folder_id));
    }
    let value = request.into_model::<IntValue>().one(&state.db).await?;
    Ok(value
        .and_then(|value| value.value)
        .unwrap_or(0)
        .saturating_mul(1000))
}

async fn artists(
    state: &AppState,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let folder_id = requested_music_folder(state, access, p).await?;
    let artists = library_artists(state, access, folder_id.as_deref()).await?;
    let artist_ids = artists
        .iter()
        .map(|artist| artist.id.clone())
        .collect::<Vec<_>>();
    let cover_art = artist_cover_art_map(state, access, &artist_ids, folder_id.as_deref()).await?;
    let mut groups: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for artist in artists {
        let artist_cover_art = cover_art.get(&artist.id).map(String::as_str);
        groups
            .entry(initial(&artist.name))
            .or_default()
            .push(artist_json(&artist, artist_cover_art));
    }
    Ok(
        json!({"artists":{"ignoredArticles":"","index":groups.into_iter().map(|(name,artist)|json!({"name":name,"artist":artist})).collect::<Vec<_>>()}}),
    )
}
async fn indexes(
    state: &AppState,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let folder_id = requested_music_folder(state, access, p).await?;
    let artists = library_artists(state, access, folder_id.as_deref()).await?;
    let mut groups: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for artist in artists {
        groups
            .entry(initial(&artist.name))
            .or_default()
            .push(json!({"id":artist.id,"name":artist.name}));
    }
    let last_modified = library_last_modified(state, access, folder_id.as_deref()).await?;
    let modified_since = p
        .get("ifModifiedSince")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(-1);
    let indexes = if modified_since >= last_modified {
        Vec::new()
    } else {
        groups
            .into_iter()
            .map(|(name, artist)| json!({"name":name,"artist":artist}))
            .collect::<Vec<_>>()
    };
    Ok(json!({"indexes":{"ignoredArticles":"","lastModified":last_modified,"index":indexes}}))
}
async fn get_artist(
    state: &AppState,
    access: &SubsonicAccess,
    id: &str,
) -> Result<Value, ApiFailure> {
    let mut artist = artist_entity::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(not_found)?;
    let accessible_track_ids = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::Id)
        .into_query();
    let track_ids = track_artist_entity::Entity::find()
        .select_only()
        .column(track_artist_entity::Column::TrackId)
        .filter(track_artist_entity::Column::ArtistId.eq(id))
        .filter(track_artist_entity::Column::TrackId.in_subquery(accessible_track_ids))
        .into_query();
    let album_ids = track_entity::Entity::find()
        .select_only()
        .column(track_entity::Column::AlbumId)
        .filter(track_entity::Column::Id.in_subquery(track_ids))
        .into_query();
    let mut albums = album_entity::Entity::find()
        .filter(album_entity::Column::Id.in_subquery(album_ids))
        .order_by_asc(album_entity::Column::Year)
        .order_by_asc(album_entity::Column::Name)
        .all(&state.db)
        .await?;
    if albums.is_empty() {
        return Err(not_found());
    }
    scope_artist_stats(state, access, std::slice::from_mut(&mut artist), None).await?;
    scope_album_stats(state, access, &mut albums, None).await?;
    let cover_art =
        artist_cover_art_map(state, access, std::slice::from_ref(&artist.id), None).await?;
    let mut data = artist_json(&artist, cover_art.get(id).map(String::as_str));
    data["album"] = Value::Array(albums.iter().map(album_json).collect());
    Ok(json!({"artist":data}))
}
async fn get_album(
    state: &AppState,
    access: &SubsonicAccess,
    id: &str,
) -> Result<Value, ApiFailure> {
    let mut album = album(state, id).await?;
    let tracks = accessible_tracks(access)
        .filter(track_entity::Column::AlbumId.eq(id))
        .order_by_asc(track_entity::Column::DiscNumber)
        .order_by_asc(track_entity::Column::TrackNumber)
        .order_by_asc(track_entity::Column::Title)
        .all(&state.db)
        .await?;
    if tracks.is_empty() {
        return Err(not_found());
    }
    scope_album_stats(state, access, std::slice::from_mut(&mut album), None).await?;
    let mut data = album_json(&album);
    data["song"] = Value::Array(tracks.iter().map(|t| track_json(t, None)).collect());
    Ok(json!({"album":data}))
}
async fn get_song(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    id: &str,
) -> Result<Value, ApiFailure> {
    let track = accessible_track(state, access, id).await?;
    let starred = favorite_entity::Entity::find()
        .filter(favorite_entity::Column::UserId.eq(&user.id))
        .filter(favorite_entity::Column::ItemType.eq("track"))
        .filter(favorite_entity::Column::ItemId.eq(id))
        .one(&state.db)
        .await?;
    Ok(json!({"song":track_json(&track, starred.map(|favorite|favorite.created_at))}))
}
async fn music_directory(
    state: &AppState,
    access: &SubsonicAccess,
    id: &str,
) -> Result<Value, ApiFailure> {
    if let Some(folder) = find_music_folder(state, id)
        .await?
        .filter(|folder| access.allows_folder(&folder.id))
    {
        let folder_id = folder.id.clone();
        let parent_id = folder_api_id(&folder.id).to_string();
        let artists = library_artists(state, access, Some(&folder_id)).await?;
        let artist_ids = artists
            .iter()
            .map(|artist| artist.id.clone())
            .collect::<Vec<_>>();
        let cover_art = artist_cover_art_map(state, access, &artist_ids, Some(&folder_id)).await?;
        return Ok(
            json!({"directory":{"id":parent_id,"name":folder.name,"child":artists.iter().map(|artist|artist_child_json(artist,Some(&parent_id),cover_art.get(&artist.id).map(String::as_str))).collect::<Vec<_>>()}}),
        );
    }
    if let Some(artist) = artist_entity::Entity::find_by_id(id).one(&state.db).await? {
        let accessible_track_ids = accessible_tracks(access)
            .select_only()
            .column(track_entity::Column::Id)
            .into_query();
        let track_ids = track_artist_entity::Entity::find()
            .select_only()
            .column(track_artist_entity::Column::TrackId)
            .filter(track_artist_entity::Column::ArtistId.eq(id))
            .filter(track_artist_entity::Column::TrackId.in_subquery(accessible_track_ids))
            .into_query();
        let album_ids = track_entity::Entity::find()
            .select_only()
            .column(track_entity::Column::AlbumId)
            .filter(track_entity::Column::Id.in_subquery(track_ids))
            .into_query();
        let albums = album_entity::Entity::find()
            .filter(album_entity::Column::Id.in_subquery(album_ids))
            .order_by_asc(album_entity::Column::Year)
            .order_by_asc(album_entity::Column::Name)
            .all(&state.db)
            .await?;
        if albums.is_empty() {
            return Err(not_found());
        }
        let children = albums
            .into_iter()
            .map(|album| {
                let mut child = json!({"id":album.id,"parent":artist.id,"title":album.name,"album":album.name,"artist":album.artist_name,"isDir":true});
                if album.cover_path.as_deref() != Some(MISSING_ARTWORK_MARKER) {
                    child["coverArt"] = json!(format!("img-{}", album.id));
                }
                child
            })
            .collect::<Vec<_>>();
        return Ok(json!({"directory":{"id":artist.id,"name":artist.name,"child":children}}));
    }
    let album = album(state, id).await?;
    let tracks = accessible_tracks(access)
        .filter(track_entity::Column::AlbumId.eq(id))
        .order_by_asc(track_entity::Column::DiscNumber)
        .order_by_asc(track_entity::Column::TrackNumber)
        .order_by_asc(track_entity::Column::Title)
        .all(&state.db)
        .await?;
    if tracks.is_empty() {
        return Err(not_found());
    }
    Ok(
        json!({"directory":{"id":album.id,"name":album.name,"child":tracks.iter().map(|t|track_json(t,None)).collect::<Vec<_>>()}}),
    )
}
async fn genres(state: &AppState, access: &SubsonicAccess) -> Result<Value, ApiFailure> {
    #[derive(FromQueryResult)]
    struct GenreRow {
        genre: String,
        song_count: i64,
        album_count: i64,
    }
    let values = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::Genre)
        .column_as(track_entity::Column::Id.count(), "song_count")
        .column_as(
            Expr::col(track_entity::Column::AlbumId).count_distinct(),
            "album_count",
        )
        .filter(track_entity::Column::Genre.ne(""))
        .group_by(track_entity::Column::Genre)
        .order_by_asc(track_entity::Column::Genre)
        .into_model::<GenreRow>()
        .all(&state.db)
        .await?;
    Ok(
        json!({"genres":{"genre":values.into_iter().map(|v|json!({"value":v.genre,"songCount":v.song_count,"albumCount":v.album_count})).collect::<Vec<_>>()}}),
    )
}

async fn artist_info(
    state: &AppState,
    access: &SubsonicAccess,
    method: &str,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = required(p, "id")?;
    let artist_id = if accessible_artist_exists(state, access, id).await? {
        id.to_owned()
    } else if let Some(track) = accessible_tracks(access)
        .filter(track_entity::Column::AlbumId.eq(id))
        .one(&state.db)
        .await?
    {
        track.artist_id
    } else {
        accessible_track(state, access, id).await?.artist_id
    };
    let mut artist = artist_entity::Entity::find_by_id(&artist_id)
        .one(&state.db)
        .await?
        .ok_or_else(not_found)?;
    let own_track_ids = track_artist_entity::Entity::find()
        .select_only()
        .column(track_artist_entity::Column::TrackId)
        .filter(track_artist_entity::Column::ArtistId.eq(&artist_id))
        .into_query();
    let genres = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::Genre)
        .filter(track_entity::Column::Id.in_subquery(own_track_ids))
        .filter(track_entity::Column::Genre.ne(""))
        .into_tuple::<String>()
        .all(&state.db)
        .await?;
    let related_track_ids = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::Id)
        .filter(track_entity::Column::Genre.is_in(genres))
        .into_query();
    let related_artist_ids = track_artist_entity::Entity::find()
        .select_only()
        .column(track_artist_entity::Column::ArtistId)
        .filter(track_artist_entity::Column::TrackId.in_subquery(related_track_ids))
        .filter(track_artist_entity::Column::ArtistId.ne(&artist_id))
        .into_query();
    let mut similar = artist_entity::Entity::find()
        .filter(artist_entity::Column::Id.in_subquery(related_artist_ids))
        .order_by_asc(artist_entity::Column::Name)
        .limit(int(p, "count", 20).clamp(0, 100) as u64)
        .all(&state.db)
        .await?;
    scope_artist_stats(state, access, std::slice::from_mut(&mut artist), None).await?;
    scope_artist_stats(state, access, &mut similar, None).await?;
    let ids = similar
        .iter()
        .map(|artist| artist.id.clone())
        .collect::<Vec<_>>();
    let covers = artist_cover_art_map(state, access, &ids, None).await?;
    let key = if method.ends_with('2') {
        "artistInfo2"
    } else {
        "artistInfo"
    };
    Ok(json!({key:{
        "biography":format!("{} · {} albums · {} songs",artist.name,artist.album_count,artist.song_count),
        "similarArtist":similar.iter().map(|artist|artist_json(artist,covers.get(&artist.id).map(String::as_str))).collect::<Vec<_>>()
    }}))
}

async fn album_info(
    state: &AppState,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = required(p, "id")?;
    let album_id = if accessible_tracks(access)
        .filter(track_entity::Column::AlbumId.eq(id))
        .one(&state.db)
        .await?
        .is_some()
    {
        id.to_owned()
    } else {
        accessible_track(state, access, id)
            .await?
            .album_id
            .ok_or_else(not_found)?
    };
    let album = album(state, &album_id).await?;
    let comments = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::Comment)
        .filter(track_entity::Column::AlbumId.eq(&album_id))
        .filter(track_entity::Column::Comment.ne(""))
        .into_tuple::<String>()
        .all(&state.db)
        .await?;
    let mut notes = comments
        .into_iter()
        .map(|comment| comment.trim().to_owned())
        .filter(|comment| !comment.is_empty())
        .collect::<Vec<_>>();
    notes.sort();
    notes.dedup();
    let notes = if notes.is_empty() {
        format!("{} — {}", album.name, album.artist_name)
    } else {
        notes.join("\n\n")
    };
    Ok(json!({"albumInfo":{"notes":notes}}))
}

async fn similar_songs(
    state: &AppState,
    access: &SubsonicAccess,
    method: &str,
    id: &str,
    count: i64,
) -> Result<Value, ApiFailure> {
    let base = accessible_track(state, access, id).await?;
    let artist_ids = track_artist_entity::Entity::find()
        .select_only()
        .column(track_artist_entity::Column::ArtistId)
        .filter(track_artist_entity::Column::TrackId.eq(id))
        .into_query();
    let related_track_ids = track_artist_entity::Entity::find()
        .select_only()
        .column(track_artist_entity::Column::TrackId)
        .filter(track_artist_entity::Column::ArtistId.in_subquery(artist_ids))
        .into_query();
    let tracks = accessible_tracks(access)
        .filter(track_entity::Column::Id.ne(id))
        .filter(
            Condition::any()
                .add(track_entity::Column::Genre.eq(base.genre))
                .add(track_entity::Column::Id.in_subquery(related_track_ids)),
        )
        .order_by_desc(track_entity::Column::PlayCount)
        .limit(count.clamp(0, 500) as u64)
        .all(&state.db)
        .await?;
    let key = if method.ends_with('2') {
        "similarSongs2"
    } else {
        "similarSongs"
    };
    Ok(json!({key:{"song":tracks.iter().map(|v|track_json(v,None)).collect::<Vec<_>>()}}))
}
async fn top_songs(
    state: &AppState,
    access: &SubsonicAccess,
    artist: &str,
    count: i64,
) -> Result<Value, ApiFailure> {
    let artist = artist_entity::Entity::find()
        .filter(
            Condition::any()
                .add(artist_entity::Column::Id.eq(artist))
                .add(artist_entity::Column::Name.eq(artist)),
        )
        .one(&state.db)
        .await?
        .ok_or_else(not_found)?;
    if !accessible_artist_exists(state, access, &artist.id).await? {
        return Err(not_found());
    }
    let track_ids = track_artist_entity::Entity::find()
        .select_only()
        .column(track_artist_entity::Column::TrackId)
        .filter(track_artist_entity::Column::ArtistId.eq(artist.id))
        .into_query();
    let tracks = accessible_tracks(access)
        .filter(track_entity::Column::Id.in_subquery(track_ids))
        .order_by_desc(track_entity::Column::PlayCount)
        .limit(count.clamp(0, 500) as u64)
        .all(&state.db)
        .await?;
    Ok(json!({"topSongs":{"song":tracks.iter().map(|v|track_json(v,None)).collect::<Vec<_>>()}}))
}
async fn album_list(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    method: &str,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let kind = required(p, "type")?;
    if !matches!(
        kind,
        "random"
            | "newest"
            | "highest"
            | "frequent"
            | "recent"
            | "alphabeticalByName"
            | "alphabeticalByArtist"
            | "starred"
            | "byYear"
            | "byGenre"
    ) {
        return Err(ApiFailure::new(10, "Invalid album list type"));
    }

    let folder_id = requested_music_folder(state, access, p).await?;
    let mut request = album_entity::Entity::find();
    let mut album_ids = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::AlbumId);
    if let Some(folder_id) = folder_id.as_deref() {
        album_ids = album_ids.filter(track_entity::Column::FolderId.eq(folder_id))
    }
    request = request.filter(album_entity::Column::Id.in_subquery(album_ids.into_query()));
    request = match kind {
        "random" => request.order_by(Expr::cust("RANDOM()"), Order::Asc),
        "newest" => request.order_by_desc(album_entity::Column::CreatedAt),
        "highest" => request
            .order_by(
                Expr::cust("(SELECT COALESCE(AVG(rating),0) FROM ratings WHERE item_type='album' AND item_id=albums.id)"),
                Order::Desc,
            )
            .order_by_asc(album_entity::Column::Name),
        "frequent" => request
            .order_by(
                Expr::cust("(SELECT COALESCE(SUM(play_count),0) FROM tracks WHERE album_id=albums.id)"),
                Order::Desc,
            )
            .order_by_asc(album_entity::Column::Name),
        "recent" => request
            .order_by(
                Expr::cust("(SELECT MAX(scrobbles.played_at) FROM scrobbles JOIN tracks ON tracks.id=scrobbles.track_id WHERE tracks.album_id=albums.id)"),
                Order::Desc,
            )
            .order_by_asc(album_entity::Column::Name),
        "alphabeticalByName" => request.order_by_asc(album_entity::Column::Name),
        "starred" => {
            let album_ids = favorite_entity::Entity::find()
                .select_only()
                .column(favorite_entity::Column::ItemId)
                .filter(favorite_entity::Column::UserId.eq(&user.id))
                .filter(favorite_entity::Column::ItemType.eq("album"))
                .into_query();
            request
                .filter(album_entity::Column::Id.in_subquery(album_ids))
                .order_by_asc(album_entity::Column::Name)
        }
        "byYear" => {
            let from_year = required_i64(p, "fromYear")?;
            let to_year = required_i64(p, "toYear")?;
            let request = request
                .filter(album_entity::Column::Year.gte(from_year.min(to_year)))
                .filter(album_entity::Column::Year.lte(from_year.max(to_year)));
            if from_year > to_year {
                request
                    .order_by_desc(album_entity::Column::Year)
                    .order_by_asc(album_entity::Column::Name)
            } else {
                request
                    .order_by_asc(album_entity::Column::Year)
                    .order_by_asc(album_entity::Column::Name)
            }
        }
        "byGenre" => request
            .filter(album_entity::Column::Genre.eq(required(p, "genre")?))
            .order_by_asc(album_entity::Column::ArtistName)
            .order_by_asc(album_entity::Column::Name),
        _ => request
            .order_by_asc(album_entity::Column::ArtistName)
            .order_by_asc(album_entity::Column::Name),
    };
    let mut albums = request
        .order_by_asc(album_entity::Column::Id)
        .limit(int(p, "size", 10).clamp(1, 500) as u64)
        .offset(int(p, "offset", 0).max(0) as u64)
        .all(&state.db)
        .await?;
    scope_album_stats(state, access, &mut albums, folder_id.as_deref()).await?;
    let key = if method.ends_with('2') {
        "albumList2"
    } else {
        "albumList"
    };
    let albums = if method.ends_with('2') {
        albums.iter().map(album_json).collect::<Vec<_>>()
    } else {
        albums.iter().map(album_child_json).collect::<Vec<_>>()
    };
    Ok(json!({key:{"album":albums}}))
}
async fn random_songs(
    state: &AppState,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let folder_id = requested_music_folder(state, access, p).await?;
    let mut request = accessible_tracks(access);
    if let Some(folder_id) = folder_id {
        request = request.filter(track_entity::Column::FolderId.eq(folder_id));
    }
    if let Some(genre) = p.get("genre") {
        request = request.filter(track_entity::Column::Genre.eq(genre));
    }
    if let Some(from_year) = p.get("fromYear") {
        request = request.filter(
            track_entity::Column::Year.gte(
                from_year
                    .parse::<i64>()
                    .map_err(|_| ApiFailure::new(10, "Invalid fromYear"))?,
            ),
        );
    }
    if let Some(to_year) = p.get("toYear") {
        request = request.filter(
            track_entity::Column::Year.lte(
                to_year
                    .parse::<i64>()
                    .map_err(|_| ApiFailure::new(10, "Invalid toYear"))?,
            ),
        );
    }
    let tracks = request
        .order_by(Expr::cust("RANDOM()"), Order::Asc)
        .limit(int(p, "size", 10).clamp(1, 500) as u64)
        .all(&state.db)
        .await?;
    Ok(json!({"randomSongs":{"song":tracks.iter().map(|v|track_json(v,None)).collect::<Vec<_>>()}}))
}
async fn songs_by_genre(
    state: &AppState,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let genre = required(p, "genre")?;
    let folder_id = requested_music_folder(state, access, p).await?;
    let mut request = accessible_tracks(access).filter(track_entity::Column::Genre.eq(genre));
    if let Some(folder_id) = folder_id {
        request = request.filter(track_entity::Column::FolderId.eq(folder_id));
    }
    let tracks = request
        .order_by_asc(track_entity::Column::ArtistName)
        .order_by_asc(track_entity::Column::AlbumName)
        .order_by_asc(track_entity::Column::TrackNumber)
        .limit(int(p, "count", 10).clamp(0, 500) as u64)
        .offset(int(p, "offset", 0).max(0) as u64)
        .all(&state.db)
        .await?;
    Ok(
        json!({"songsByGenre":{"song":tracks.iter().map(|v|track_json(v,None)).collect::<Vec<_>>()}}),
    )
}
async fn starred(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    method: &str,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let folder_id = requested_music_folder(state, access, p).await?;
    let stars = favorite_entity::Entity::find()
        .filter(favorite_entity::Column::UserId.eq(&user.id))
        .order_by_desc(favorite_entity::Column::CreatedAt)
        .all(&state.db)
        .await?;
    let starred_artist_ids = stars
        .iter()
        .filter(|star| star.item_type == "artist")
        .map(|star| star.item_id.clone())
        .collect::<Vec<_>>();
    let artist_cover_art = if method.ends_with('2') {
        artist_cover_art_map(state, access, &starred_artist_ids, folder_id.as_deref()).await?
    } else {
        HashMap::new()
    };
    let mut songs = Vec::new();
    let mut albums = Vec::new();
    let mut artists = Vec::new();
    for star in stars {
        match star.item_type.as_str() {
            "track" => {
                if let Some(track) = accessible_tracks(access)
                    .filter(track_entity::Column::Id.eq(&star.item_id))
                    .one(&state.db)
                    .await?
                    .filter(|track| {
                        folder_id
                            .as_ref()
                            .is_none_or(|folder_id| &track.folder_id == folder_id)
                    })
                {
                    songs.push(track_json(&track, Some(star.created_at)));
                }
            }
            "album" => {
                let mut album_tracks = accessible_tracks(access)
                    .filter(track_entity::Column::AlbumId.eq(&star.item_id));
                if let Some(folder_id) = folder_id.as_ref() {
                    album_tracks =
                        album_tracks.filter(track_entity::Column::FolderId.eq(folder_id));
                }
                let in_folder = album_tracks.one(&state.db).await?.is_some();
                if in_folder
                    && let Some(mut album) = album_entity::Entity::find_by_id(&star.item_id)
                        .one(&state.db)
                        .await?
                {
                    scope_album_stats(
                        state,
                        access,
                        std::slice::from_mut(&mut album),
                        folder_id.as_deref(),
                    )
                    .await?;
                    let mut value = if method.ends_with('2') {
                        album_json(&album)
                    } else {
                        album_child_json(&album)
                    };
                    value["starred"] = json!(star.created_at);
                    albums.push(value);
                }
            }
            "artist" => {
                let mut track_ids = accessible_tracks(access)
                    .select_only()
                    .column(track_entity::Column::Id);
                if let Some(folder_id) = folder_id.as_ref() {
                    track_ids = track_ids.filter(track_entity::Column::FolderId.eq(folder_id));
                }
                let in_folder = track_artist_entity::Entity::find()
                    .filter(track_artist_entity::Column::ArtistId.eq(&star.item_id))
                    .filter(
                        track_artist_entity::Column::TrackId.in_subquery(track_ids.into_query()),
                    )
                    .one(&state.db)
                    .await?
                    .is_some();
                if in_folder
                    && let Some(mut artist) = artist_entity::Entity::find_by_id(&star.item_id)
                        .one(&state.db)
                        .await?
                {
                    scope_artist_stats(
                        state,
                        access,
                        std::slice::from_mut(&mut artist),
                        folder_id.as_deref(),
                    )
                    .await?;
                    let mut value = if method.ends_with('2') {
                        artist_json(
                            &artist,
                            artist_cover_art.get(&artist.id).map(String::as_str),
                        )
                    } else {
                        legacy_artist_json(&artist)
                    };
                    value["starred"] = json!(star.created_at);
                    artists.push(value);
                }
            }
            _ => {}
        }
    }
    let key = if method.ends_with('2') {
        "starred2"
    } else {
        "starred"
    };
    Ok(json!({key:{"song":songs,"album":albums,"artist":artists}}))
}

async fn legacy_search(
    state: &AppState,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let any = p
        .get("any")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let artist_query = p
        .get("artist")
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .or(any);
    let album_query = p
        .get("album")
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .or(any);
    let title_query = p
        .get("title")
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .or(any);
    let mut matches = Vec::new();
    if let Some(query) = artist_query {
        let allowed_artist_ids = library_artists(state, access, None)
            .await?
            .into_iter()
            .map(|artist| artist.id)
            .collect::<Vec<_>>();
        let artists = artist_entity::Entity::find()
            .filter(artist_entity::Column::Name.contains(query))
            .filter(artist_entity::Column::Id.is_in(allowed_artist_ids))
            .order_by_asc(artist_entity::Column::Name)
            .all(&state.db)
            .await?;
        let artist_ids = artists
            .iter()
            .map(|artist| artist.id.clone())
            .collect::<Vec<_>>();
        let cover_art = artist_cover_art_map(state, access, &artist_ids, None).await?;
        matches.extend(artists.iter().map(|artist| {
            artist_child_json(artist, None, cover_art.get(&artist.id).map(String::as_str))
        }));
    }
    if let Some(query) = album_query {
        let allowed_album_ids = accessible_tracks(access)
            .select_only()
            .column(track_entity::Column::AlbumId)
            .into_query();
        let mut albums = album_entity::Entity::find()
            .filter(album_entity::Column::Name.contains(query))
            .filter(album_entity::Column::Id.in_subquery(allowed_album_ids))
            .order_by_asc(album_entity::Column::Name)
            .all(&state.db)
            .await?;
        scope_album_stats(state, access, &mut albums, None).await?;
        matches.extend(albums.iter().map(album_child_json));
    }
    if let Some(query) = title_query {
        let tracks = accessible_tracks(access)
            .filter(track_entity::Column::Title.contains(query))
            .order_by_asc(track_entity::Column::Title)
            .all(&state.db)
            .await?;
        matches.extend(tracks.iter().map(|track| track_json(track, None)));
    }
    let total_hits = matches.len();
    let offset = int(p, "offset", 0).max(0) as usize;
    let count = int(p, "count", 20).max(0) as usize;
    let matches = matches
        .into_iter()
        .skip(offset)
        .take(count)
        .collect::<Vec<_>>();
    Ok(json!({"searchResult":{"offset":offset,"totalHits":total_hits,"match":matches}}))
}

async fn search(
    state: &AppState,
    access: &SubsonicAccess,
    method: &str,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let query = normalize_search_query(present(p, "query")?);
    let folder_id = requested_music_folder(state, access, p).await?;
    let artist_count = int(p, "artistCount", 20).clamp(0, MAX_COLLECTION_ITEMS as i64);
    let artist_offset = int(p, "artistOffset", 0).max(0);
    let album_count = int(p, "albumCount", 20).clamp(0, MAX_COLLECTION_ITEMS as i64);
    let album_offset = int(p, "albumOffset", 0).max(0);
    let song_count = int(p, "songCount", 20).clamp(0, MAX_COLLECTION_ITEMS as i64);
    let song_offset = int(p, "songOffset", 0).max(0);
    let mut artist_request = artist_entity::Entity::find()
        .filter(artist_entity::Column::Name.contains(&query))
        .order_by_asc(artist_entity::Column::Name)
        .order_by_asc(artist_entity::Column::Id)
        .limit(artist_count as u64)
        .offset(artist_offset as u64);
    let mut album_request = album_entity::Entity::find()
        .filter(
            Condition::any()
                .add(album_entity::Column::Name.contains(&query))
                .add(album_entity::Column::ArtistName.contains(&query)),
        )
        .order_by_asc(album_entity::Column::Name)
        .order_by_asc(album_entity::Column::Id)
        .limit(album_count as u64)
        .offset(album_offset as u64);
    let mut track_request = accessible_tracks(access)
        .filter(
            Condition::any()
                .add(track_entity::Column::Title.contains(&query))
                .add(track_entity::Column::ArtistName.contains(&query))
                .add(track_entity::Column::AlbumName.contains(&query)),
        )
        .order_by_asc(track_entity::Column::Title)
        .order_by_asc(track_entity::Column::Id)
        .limit(song_count as u64)
        .offset(song_offset as u64);
    {
        let mut folder_tracks = accessible_tracks(access)
            .select_only()
            .column(track_entity::Column::Id);
        if let Some(folder_id) = folder_id.as_deref() {
            folder_tracks = folder_tracks.filter(track_entity::Column::FolderId.eq(folder_id))
        }
        let folder_artists = track_artist_entity::Entity::find()
            .select_only()
            .column(track_artist_entity::Column::ArtistId)
            .filter(
                track_artist_entity::Column::TrackId
                    .in_subquery(folder_tracks.clone().into_query()),
            )
            .into_query();
        let mut folder_albums = accessible_tracks(access)
            .select_only()
            .column(track_entity::Column::AlbumId);
        if let Some(folder_id) = folder_id.as_deref() {
            folder_albums = folder_albums.filter(track_entity::Column::FolderId.eq(folder_id));
            track_request = track_request.filter(track_entity::Column::FolderId.eq(folder_id));
        }
        artist_request =
            artist_request.filter(artist_entity::Column::Id.in_subquery(folder_artists));
        album_request =
            album_request.filter(album_entity::Column::Id.in_subquery(folder_albums.into_query()));
    }
    let (mut artists, mut albums, tracks) = tokio::try_join!(
        artist_request.all(&state.db),
        album_request.all(&state.db),
        track_request.all(&state.db),
    )?;
    scope_artist_stats(state, access, &mut artists, folder_id.as_deref()).await?;
    scope_album_stats(state, access, &mut albums, folder_id.as_deref()).await?;
    let key = if method == "search3" {
        "searchResult3"
    } else {
        "searchResult2"
    };
    let albums = if method == "search3" {
        albums.iter().map(album_json).collect::<Vec<_>>()
    } else {
        albums.iter().map(album_child_json).collect::<Vec<_>>()
    };
    let artists = if method == "search3" {
        let artist_ids = artists
            .iter()
            .map(|artist| artist.id.clone())
            .collect::<Vec<_>>();
        let cover_art =
            artist_cover_art_map(state, access, &artist_ids, folder_id.as_deref()).await?;
        artists
            .iter()
            .map(|artist| artist_json(artist, cover_art.get(&artist.id).map(String::as_str)))
            .collect::<Vec<_>>()
    } else {
        artists.iter().map(legacy_artist_json).collect::<Vec<_>>()
    };
    Ok(
        json!({key:{"artist":artists,"album":albums,"song":tracks.iter().map(|v|track_json(v,None)).collect::<Vec<_>>()}}),
    )
}

async fn playlists(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let owner_id = if let Some(username) = p.get("username") {
        if user.role != "admin" && &user.username != username {
            return Err(ApiFailure::new(50, "Not authorized"));
        }
        user_by_name(&state.db, username)
            .await?
            .ok_or_else(not_found)?
            .id
    } else {
        user.id.clone()
    };
    let rows = playlist_entity::Entity::find()
        .filter(
            Condition::any()
                .add(playlist_entity::Column::UserId.eq(owner_id))
                .add(playlist_entity::Column::Public.eq(1)),
        )
        .order_by_asc(playlist_entity::Column::Name)
        .all(&state.db)
        .await?;
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        let tracks = playlist_tracks(state, access, &row.id).await?;
        values.push(playlist_json(state, &row, &tracks).await?);
    }
    Ok(json!({"playlists":{"playlist":values}}))
}
async fn playlist(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    id: &str,
) -> Result<Value, ApiFailure> {
    let row = accessible_playlist(state, user, id).await?;
    let tracks = playlist_tracks(state, access, id).await?;
    let mut value = playlist_json(state, &row, &tracks).await?;
    value["entry"] = Value::Array(tracks.iter().map(|v| track_json(v, None)).collect());
    Ok(json!({"playlist":value}))
}
async fn create_playlist(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = if let Some(id) = p.get("playlistId") {
        let playlist = owned_playlist(state, user, id).await?;
        if let Some(name) = p.get("name") {
            let mut active = playlist.into_active_model();
            active.name = Set(name.clone());
            active.updated_at = Set(Utc::now().to_rfc3339());
            active.update(&state.db).await?;
        }
        id.clone()
    } else {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        playlist_entity::ActiveModel {
            id: Set(id.clone()),
            user_id: Set(user.id.clone()),
            name: Set(required(p, "name")?.to_owned()),
            comment: Set(String::new()),
            public: Set(0),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(&state.db)
        .await?;
        id
    };
    replace_playlist_tracks(state, access, &id, p).await?;
    playlist(state, user, access, &id).await
}
async fn update_playlist(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = required(p, "playlistId")?;
    let row = owned_playlist(state, user, id).await?;
    let mut active = row.into_active_model();
    if let Some(name) = p.get("name") {
        active.name = Set(name.clone());
    }
    if let Some(comment) = p.get("comment") {
        active.comment = Set(comment.clone());
    }
    if p.contains_key("public") {
        active.public = Set(bool_param(p, "public") as i64);
    }
    active.updated_at = Set(Utc::now().to_rfc3339());
    active.update(&state.db).await?;

    let mut ids = playlist_track_ids(state, id).await?;
    let mut removals = multi(p, "songIndexToRemove")
        .into_iter()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| ApiFailure::new(10, "Invalid songIndexToRemove"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    removals.sort_unstable_by(|left, right| right.cmp(left));
    for index in removals {
        if index < ids.len() {
            ids.remove(index);
        }
    }
    ids.extend(multi(p, "songIdToAdd"));
    if p.contains_key("songIndexToRemove") || p.contains_key("songIdToAdd") {
        set_playlist_tracks(state, access, id, &ids).await?;
    }
    Ok(json!({}))
}
async fn delete_playlist(state: &AppState, user: &User, id: &str) -> Result<Value, ApiFailure> {
    owned_playlist(state, user, id).await?;
    playlist_track_entity::Entity::delete_many()
        .filter(playlist_track_entity::Column::PlaylistId.eq(id))
        .exec(&state.db)
        .await?;
    playlist_entity::Entity::delete_by_id(id)
        .exec(&state.db)
        .await?;
    Ok(json!({}))
}
async fn owned_playlist(
    state: &AppState,
    user: &User,
    id: &str,
) -> Result<playlist_entity::Model, ApiFailure> {
    playlist_entity::Entity::find_by_id(id)
        .filter(playlist_entity::Column::UserId.eq(&user.id))
        .one(&state.db)
        .await?
        .ok_or_else(not_found)
}

async fn accessible_playlist(
    state: &AppState,
    user: &User,
    id: &str,
) -> Result<playlist_entity::Model, ApiFailure> {
    playlist_entity::Entity::find_by_id(id)
        .filter(
            Condition::any()
                .add(playlist_entity::Column::UserId.eq(&user.id))
                .add(playlist_entity::Column::Public.eq(1)),
        )
        .one(&state.db)
        .await?
        .ok_or_else(not_found)
}
async fn replace_playlist_tracks(
    state: &AppState,
    access: &SubsonicAccess,
    id: &str,
    p: &HashMap<String, String>,
) -> Result<(), ApiFailure> {
    if p.contains_key("songId") {
        set_playlist_tracks(state, access, id, &multi(p, "songId")).await?;
    }
    Ok(())
}

async fn playlist_track_ids(state: &AppState, id: &str) -> Result<Vec<String>, ApiFailure> {
    Ok(playlist_track_entity::Entity::find()
        .filter(playlist_track_entity::Column::PlaylistId.eq(id))
        .order_by_asc(playlist_track_entity::Column::Position)
        .all(&state.db)
        .await?
        .into_iter()
        .map(|row| row.track_id)
        .collect())
}

async fn playlist_tracks(
    state: &AppState,
    access: &SubsonicAccess,
    id: &str,
) -> Result<Vec<Track>, ApiFailure> {
    let ids = playlist_track_ids(state, id).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let tracks = accessible_tracks(access)
        .filter(track_entity::Column::Id.is_in(ids.clone()))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|track| (track.id.clone(), track))
        .collect::<HashMap<_, _>>();
    Ok(ids
        .into_iter()
        .filter_map(|id| tracks.get(&id).cloned())
        .collect())
}

async fn set_playlist_tracks(
    state: &AppState,
    access: &SubsonicAccess,
    id: &str,
    track_ids: &[String],
) -> Result<(), ApiFailure> {
    validate_track_ids(state, access, track_ids, MAX_COLLECTION_ITEMS).await?;

    let transaction = state.db.begin().await?;
    playlist_track_entity::Entity::delete_many()
        .filter(playlist_track_entity::Column::PlaylistId.eq(id))
        .exec(&transaction)
        .await?;
    if !track_ids.is_empty() {
        playlist_track_entity::Entity::insert_many(track_ids.iter().enumerate().map(
            |(position, track_id)| playlist_track_entity::ActiveModel {
                playlist_id: Set(id.to_owned()),
                position: Set(position as i64),
                track_id: Set(track_id.clone()),
            },
        ))
        .exec(&transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

async fn get_lyrics_legacy(
    state: &AppState,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let artist = p.get("artist").map(String::as_str).unwrap_or("");
    let title = p.get("title").map(String::as_str).unwrap_or("");
    let mut request = accessible_tracks(access);
    if !title.is_empty() {
        request = request.filter(track_entity::Column::Title.eq(title));
    }
    if !artist.is_empty() {
        let artist_ids = artist_entity::Entity::find()
            .select_only()
            .column(artist_entity::Column::Id)
            .filter(artist_entity::Column::Name.eq(artist))
            .into_query();
        let track_ids = track_artist_entity::Entity::find()
            .select_only()
            .column(track_artist_entity::Column::TrackId)
            .filter(track_artist_entity::Column::ArtistId.in_subquery(artist_ids))
            .into_query();
        request = request.filter(track_entity::Column::Id.in_subquery(track_ids));
    }
    let row = request.one(&state.db).await?;
    Ok(
        json!({"lyrics":{"artist":artist,"title":title,"value":row.map(|track|track.lyrics).unwrap_or_default()}}),
    )
}
async fn get_lyrics_by_song(
    state: &AppState,
    access: &SubsonicAccess,
    id: &str,
    enhanced: bool,
) -> Result<Value, ApiFailure> {
    let track = accessible_track(state, access, id).await?;
    if track.lyrics.trim().is_empty() {
        return Ok(json!({"lyricsList":{"structuredLyrics":[]}}));
    }
    let parsed_lines = parse_lrc(&track.lyrics);
    let synced = !parsed_lines.is_empty();
    let lines: Vec<Value> = if synced {
        parsed_lines
            .iter()
            .map(|line| json!({"start":line.start,"value":line.value}))
            .collect()
    } else {
        track
            .lyrics
            .lines()
            .map(|value| json!({"value":value}))
            .collect()
    };
    let display_artist = serde_json::from_str::<Vec<ArtistCredit>>(&track.artists_json)
        .unwrap_or_default()
        .into_iter()
        .map(|artist| artist.name)
        .collect::<Vec<_>>()
        .join("; ");
    let mut lyrics = json!({
        "displayArtist": display_artist,
        "displayTitle": track.title,
        "lang": "und",
        "synced": synced,
        "line": lines,
    });
    if enhanced {
        lyrics["kind"] = json!("main");
        if synced {
            let cue_lines = enhanced_lrc_cue_lines(&parsed_lines, track.duration);
            if !cue_lines.is_empty() {
                lyrics["cueLine"] = Value::Array(cue_lines);
            }
        }
    }
    Ok(json!({"lyricsList":{"structuredLyrics":[lyrics]}}))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LrcLine {
    start: i64,
    value: String,
    cues: Vec<LrcCue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LrcCue {
    start: i64,
    end: Option<i64>,
    value: String,
    byte_start: usize,
    byte_end: usize,
}

fn parse_lrc(lyrics: &str) -> Vec<LrcLine> {
    let mut lines = lyrics.lines().flat_map(parse_lrc_line).collect::<Vec<_>>();
    lines.sort_by_key(|line| line.start);
    lines
}

fn parse_lrc_line(line: &str) -> Vec<LrcLine> {
    let mut remaining = line;
    let mut starts = Vec::new();
    while let Some(value) = remaining.strip_prefix('[') {
        let Some(end) = value.find(']') else {
            break;
        };
        let Some(start) = parse_lrc_timestamp(&value[..end]) else {
            break;
        };
        starts.push(start);
        remaining = &value[end + 1..];
    }
    let Some(first_start) = starts.first().copied() else {
        return Vec::new();
    };
    let (value, cues) = parse_enhanced_lrc_text(remaining);
    starts
        .into_iter()
        .map(|start| {
            let delta = start.checked_sub(first_start);
            let shifted_cues = delta
                .map(|delta| {
                    cues.iter()
                        .filter_map(|cue| {
                            Some(LrcCue {
                                start: cue.start.checked_add(delta)?,
                                end: cue.end.and_then(|end| end.checked_add(delta)),
                                value: cue.value.clone(),
                                byte_start: cue.byte_start,
                                byte_end: cue.byte_end,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            LrcLine {
                start,
                value: value.clone(),
                cues: shifted_cues,
            }
        })
        .collect()
}

fn parse_enhanced_lrc_text(input: &str) -> (String, Vec<LrcCue>) {
    let mut value = String::with_capacity(input.len());
    let mut cues = Vec::new();
    let mut cursor = 0;
    let mut cue_start = None;
    while let Some((marker_start, marker_end, timestamp)) = next_lrc_marker(input, cursor) {
        append_lrc_segment(
            &mut value,
            &mut cues,
            &input[cursor..marker_start],
            cue_start.take(),
            Some(timestamp),
        );
        cue_start = Some(timestamp);
        cursor = marker_end;
    }
    append_lrc_segment(&mut value, &mut cues, &input[cursor..], cue_start, None);
    (value, cues)
}

fn next_lrc_marker(input: &str, from: usize) -> Option<(usize, usize, i64)> {
    let mut cursor = from;
    while let Some(relative_start) = input.get(cursor..)?.find('<') {
        let marker_start = cursor + relative_start;
        let value_start = marker_start + 1;
        let relative_end = input.get(value_start..)?.find('>')?;
        let marker_end = value_start + relative_end;
        if let Some(timestamp) = parse_lrc_timestamp(&input[value_start..marker_end]) {
            return Some((marker_start, marker_end + 1, timestamp));
        }
        cursor = marker_end + 1;
    }
    None
}

fn append_lrc_segment(
    line: &mut String,
    cues: &mut Vec<LrcCue>,
    segment: &str,
    start: Option<i64>,
    end: Option<i64>,
) {
    let byte_start = line.len();
    line.push_str(segment);
    if let Some(start) = start
        && !segment.is_empty()
    {
        cues.push(LrcCue {
            start,
            end,
            value: segment.to_owned(),
            byte_start,
            byte_end: line.len() - 1,
        });
    }
}

fn enhanced_lrc_cue_lines(lines: &[LrcLine], duration_seconds: f64) -> Vec<Value> {
    let duration_ms = duration_millis(duration_seconds);
    let mut result = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.cues.is_empty()
            || line
                .cues
                .windows(2)
                .any(|pair| pair[0].start > pair[1].start)
        {
            continue;
        }
        let next_line_start = lines
            .iter()
            .skip(index + 1)
            .map(|line| line.start)
            .find(|start| *start > line.start);
        let boundary = next_line_start.or(duration_ms.filter(|end| *end > line.start));
        let final_cue = line.cues.last().expect("non-empty cues");
        let cue_line_end = final_cue
            .end
            .filter(|end| *end >= final_cue.start)
            .map(|end| boundary.map_or(end, |boundary| end.min(boundary)))
            .or(boundary)
            .filter(|end| *end >= final_cue.start);
        let mut ends = Vec::with_capacity(line.cues.len());
        for (cue_index, cue) in line.cues.iter().enumerate() {
            let next_cue_start = line.cues.get(cue_index + 1).map(|next| next.start);
            let mut end = cue.end.or(next_cue_start).or(cue_line_end);
            if let Some(value) = &mut end {
                *value = (*value).max(cue.start);
                if let Some(next) = next_cue_start {
                    *value = (*value).min(next.max(cue.start));
                }
                if let Some(line_end) = cue_line_end {
                    *value = (*value).min(line_end.max(cue.start));
                }
            }
            ends.push(end);
        }
        let include_ends = ends.iter().all(Option::is_some);
        let cues = line
            .cues
            .iter()
            .zip(ends)
            .map(|(cue, end)| {
                let mut value = json!({
                    "start": cue.start,
                    "value": cue.value,
                    "byteStart": cue.byte_start,
                    "byteEnd": cue.byte_end,
                });
                if include_ends {
                    value["end"] = json!(end.expect("all cue ends are present"));
                }
                value
            })
            .collect::<Vec<_>>();
        let mut cue_line = json!({
            "index": index,
            "start": line.cues[0].start,
            "value": line.value,
            "cue": cues,
        });
        if let Some(end) = cue_line_end {
            cue_line["end"] = json!(end);
        }
        result.push(cue_line);
    }
    result
}

fn duration_millis(seconds: f64) -> Option<i64> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    let milliseconds = (seconds * 1_000.0).round();
    (milliseconds <= i64::MAX as f64).then_some(milliseconds as i64)
}

fn parse_lrc_timestamp(value: &str) -> Option<i64> {
    let timestamp = value.split_once(':')?;
    let minutes = timestamp.0.parse::<u64>().ok()?;
    let seconds = timestamp.1.replace(',', ".").parse::<f64>().ok()?;
    if !(0.0..60.0).contains(&seconds) {
        return None;
    }
    let start = minutes
        .checked_mul(60_000)?
        .checked_add((seconds * 1000.0).round() as u64)?;
    i64::try_from(start).ok()
}
async fn favorite(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
    add: bool,
) -> Result<Value, ApiFailure> {
    let legacy_ids = multi(p, "id");
    let album_ids = multi(p, "albumId");
    let artist_ids = multi(p, "artistId");
    if legacy_ids.len() + album_ids.len() + artist_ids.len() > MAX_CATALOG_MUTATION_ITEMS {
        return Err(ApiFailure::new(10, "Too many IDs in one request"));
    }
    let mut items = HashSet::<(&'static str, String)>::new();
    if add {
        let all_ids = legacy_ids
            .iter()
            .chain(&album_ids)
            .chain(&artist_ids)
            .cloned()
            .collect::<HashSet<_>>();
        let track_matches = existing_catalog_ids(state, access, "track", &all_ids).await?;
        let album_matches = existing_catalog_ids(state, access, "album", &all_ids).await?;
        let artist_matches = existing_catalog_ids(state, access, "artist", &all_ids).await?;
        for id in legacy_ids {
            let kind = if track_matches.contains(&id) {
                "track"
            } else if album_matches.contains(&id) {
                "album"
            } else if artist_matches.contains(&id) {
                "artist"
            } else {
                return Err(not_found());
            };
            items.insert((kind, id));
        }
        for id in album_ids {
            if !album_matches.contains(&id) {
                return Err(not_found());
            }
            items.insert(("album", id));
        }
        for id in artist_ids {
            if !artist_matches.contains(&id) {
                return Err(not_found());
            }
            items.insert(("artist", id));
        }
    } else {
        for id in legacy_ids {
            for kind in ["track", "album", "artist"] {
                items.insert((kind, id.clone()));
            }
        }
        items.extend(album_ids.into_iter().map(|id| ("album", id)));
        items.extend(artist_ids.into_iter().map(|id| ("artist", id)));
    }
    let transaction = state.db.begin().await?;
    for (kind, id) in items {
        favorite_entity::Entity::delete_many()
            .filter(favorite_entity::Column::UserId.eq(&user.id))
            .filter(favorite_entity::Column::ItemType.eq(kind))
            .filter(favorite_entity::Column::ItemId.eq(&id))
            .exec(&transaction)
            .await?;
        if add {
            favorite_entity::ActiveModel {
                user_id: Set(user.id.clone()),
                item_type: Set(kind.to_owned()),
                item_id: Set(id),
                created_at: Set(Utc::now().to_rfc3339()),
            }
            .insert(&transaction)
            .await?;
        }
    }
    transaction.commit().await?;
    Ok(json!({}))
}

async fn existing_catalog_ids(
    state: &AppState,
    access: &SubsonicAccess,
    kind: &str,
    ids: &HashSet<String>,
) -> Result<HashSet<String>, ApiFailure> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    match kind {
        "track" => Ok(accessible_tracks(access)
            .select_only()
            .column(track_entity::Column::Id)
            .filter(track_entity::Column::Id.is_in(ids.iter().cloned()))
            .into_tuple::<String>()
            .all(&state.db)
            .await?
            .into_iter()
            .collect()),
        "album" => Ok(accessible_tracks(access)
            .select_only()
            .column(track_entity::Column::AlbumId)
            .filter(track_entity::Column::AlbumId.is_in(ids.iter().cloned()))
            .distinct()
            .into_tuple::<Option<String>>()
            .all(&state.db)
            .await?
            .into_iter()
            .flatten()
            .collect()),
        "artist" => {
            let track_ids = accessible_tracks(access)
                .select_only()
                .column(track_entity::Column::Id)
                .into_query();
            Ok(track_artist_entity::Entity::find()
                .select_only()
                .column(track_artist_entity::Column::ArtistId)
                .filter(track_artist_entity::Column::ArtistId.is_in(ids.iter().cloned()))
                .filter(track_artist_entity::Column::TrackId.in_subquery(track_ids))
                .distinct()
                .into_tuple::<String>()
                .all(&state.db)
                .await?
                .into_iter()
                .collect())
        }
        _ => Err(ApiFailure::new(10, "Invalid catalog item type")),
    }
}
async fn set_rating(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = required(p, "id")?;
    let rating = required_i64(p, "rating")?;
    if !(0..=5).contains(&rating) {
        return Err(ApiFailure::new(10, "Rating must be between 0 and 5"));
    }
    let item_type = if accessible_tracks(access)
        .filter(track_entity::Column::Id.eq(id))
        .one(&state.db)
        .await?
        .is_some()
    {
        "track"
    } else if accessible_tracks(access)
        .filter(track_entity::Column::AlbumId.eq(id))
        .one(&state.db)
        .await?
        .is_some()
    {
        "album"
    } else if accessible_artist_exists(state, access, id).await? {
        "artist"
    } else {
        return Err(not_found());
    };
    let transaction = state.db.begin().await?;
    rating_entity::Entity::delete_many()
        .filter(rating_entity::Column::UserId.eq(&user.id))
        .filter(rating_entity::Column::ItemType.eq(item_type))
        .filter(rating_entity::Column::ItemId.eq(id))
        .exec(&transaction)
        .await?;
    if rating > 0 {
        rating_entity::ActiveModel {
            user_id: Set(user.id.clone()),
            item_type: Set(item_type.to_owned()),
            item_id: Set(id.to_owned()),
            rating: Set(rating),
        }
        .insert(&transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(json!({}))
}

async fn report_playback(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let media_id = required(p, "mediaId")?;
    if required(p, "mediaType")? != "song" {
        return Err(ApiFailure::new(
            10,
            "Only song playback reports are supported",
        ));
    }
    let track = accessible_track(state, access, media_id).await?;
    let position_ms = required_i64(p, "positionMs")?;
    if position_ms < 0 {
        return Err(ApiFailure::new(10, "positionMs must not be negative"));
    }
    let playback_state = required(p, "state")?;
    if !matches!(
        playback_state,
        "starting" | "playing" | "paused" | "stopped"
    ) {
        return Err(ApiFailure::new(10, "Invalid playback state"));
    }
    let playback_rate = p
        .get("playbackRate")
        .map(|value| value.parse::<f64>())
        .transpose()
        .map_err(|_| ApiFailure::new(10, "Invalid playbackRate"))?
        .unwrap_or(1.0);
    if !playback_rate.is_finite() || playback_rate <= 0.0 || playback_rate > 16.0 {
        return Err(ApiFailure::new(10, "Invalid playbackRate"));
    }
    let ignore_scrobble = bool_param(p, "ignoreScrobble");
    let now = Utc::now();
    let updated_at = now.to_rfc3339();
    let threshold_ms = ((track.duration * 500.0).min(240_000.0)).round() as i64;
    let should_scrobble = !ignore_scrobble
        && track.duration > 30.0
        && playback_state != "starting"
        && position_ms >= threshold_ms;
    let transaction = state.db.begin().await?;
    playback_state_entity::Entity::insert(playback_state_entity::ActiveModel {
        user_id: Set(user.id.clone()),
        media_id: Set(media_id.to_owned()),
        media_type: Set("song".into()),
        position_ms: Set(position_ms),
        state: Set(playback_state.to_owned()),
        playback_rate: Set(playback_rate),
        ignore_scrobble: Set(ignore_scrobble as i64),
        scrobbled: Set(0),
        updated_at: Set(updated_at.clone()),
        client: Set(p.get("c").cloned().unwrap_or_default()),
    })
    .on_conflict(
        OnConflict::columns([playback_state_entity::Column::UserId])
            .update_columns([
                playback_state_entity::Column::MediaId,
                playback_state_entity::Column::MediaType,
                playback_state_entity::Column::PositionMs,
                playback_state_entity::Column::State,
                playback_state_entity::Column::PlaybackRate,
                playback_state_entity::Column::IgnoreScrobble,
                playback_state_entity::Column::UpdatedAt,
                playback_state_entity::Column::Client,
            ])
            .value(
                playback_state_entity::Column::Scrobbled,
                Expr::cust(
                    "CASE WHEN playback_states.media_id = excluded.media_id \
                     AND excluded.state <> 'starting' THEN playback_states.scrobbled ELSE 0 END",
                ),
            )
            .to_owned(),
    )
    .exec_without_returning(&transaction)
    .await?;
    let claimed_scrobble = if should_scrobble {
        playback_state_entity::Entity::update_many()
            .col_expr(playback_state_entity::Column::Scrobbled, Expr::value(1))
            .filter(playback_state_entity::Column::UserId.eq(&user.id))
            .filter(playback_state_entity::Column::MediaId.eq(media_id))
            .filter(playback_state_entity::Column::Scrobbled.eq(0))
            .exec(&transaction)
            .await?
            .rows_affected
            == 1
    } else {
        false
    };
    if claimed_scrobble {
        scrobble_entity::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            user_id: Set(user.id.clone()),
            track_id: Set(media_id.to_owned()),
            played_at: Set(updated_at.clone()),
            submission: Set(1),
        }
        .insert(&transaction)
        .await?;
        track_entity::Entity::update_many()
            .col_expr(
                track_entity::Column::PlayCount,
                Expr::col(track_entity::Column::PlayCount).add(1),
            )
            .filter(track_entity::Column::Id.eq(media_id))
            .exec(&transaction)
            .await?;
        increment_user_track_stats(&transaction, &user.id, media_id, 1, &updated_at).await?;
    }
    transaction.commit().await?;
    if claimed_scrobble {
        let state = state.clone();
        let user_id = user.id.clone();
        let reports = vec![(track, now.timestamp())];
        tokio::spawn(async move {
            if let Err(error) = lastfm::report(&state, &user_id, reports, true).await {
                tracing::warn!(%error, %user_id, "failed to report playback to Last.fm");
            }
        });
    }
    Ok(json!({}))
}

async fn now_playing(state: &AppState, access: &SubsonicAccess) -> Result<Value, ApiFailure> {
    let now = Utc::now();
    let states = playback_state_entity::Entity::find()
        .filter(playback_state_entity::Column::State.ne("stopped"))
        .order_by_desc(playback_state_entity::Column::UpdatedAt)
        .all(&state.db)
        .await?;
    let mut entries = Vec::new();
    for playback in states {
        let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(&playback.updated_at) else {
            continue;
        };
        let updated_at = updated_at.with_timezone(&Utc);
        let elapsed_ms = now
            .signed_duration_since(updated_at)
            .num_milliseconds()
            .max(0);
        let Ok(track) = accessible_track(state, access, &playback.media_id).await else {
            continue;
        };
        let duration_ms = (track.duration.max(0.0) * 1_000.0).round() as i64;
        let position_ms = if playback.state == "playing" {
            playback
                .position_ms
                .saturating_add(((elapsed_ms as f64) * playback.playback_rate).round() as i64)
        } else {
            playback.position_ms
        }
        .clamp(
            0,
            if duration_ms > 0 {
                duration_ms
            } else {
                playback.position_ms.max(0)
            },
        );
        let expires_after_ms = if playback.state == "playing" && duration_ms > 0 {
            let remaining_ms = (duration_ms - playback.position_ms).max(0) as f64;
            ((remaining_ms / playback.playback_rate).round() as i64).saturating_add(30 * 60_000)
        } else {
            30 * 60_000
        };
        if elapsed_ms > expires_after_ms {
            continue;
        }
        let Some(username) = user_entity::Entity::find_by_id(&playback.user_id)
            .one(&state.db)
            .await?
            .map(|user| user.username)
        else {
            continue;
        };
        let mut entry = track_json(&track, None);
        entry["username"] = json!(username);
        entry["minutesAgo"] = json!(elapsed_ms / 60_000);
        entry["playerId"] = json!(0);
        entry["playerName"] = json!(playback.client);
        entry["state"] = json!(playback.state);
        entry["positionMs"] = json!(position_ms);
        entry["playbackRate"] = json!(playback.playback_rate);
        entries.push(entry);
    }
    Ok(json!({"nowPlaying":{"entry":entries}}))
}

async fn scrobble(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let ids = multi(p, "id");
    if ids.is_empty() {
        return Err(ApiFailure::new(10, "Missing required parameter: id"));
    }
    validate_track_ids(state, access, &ids, MAX_SCROBBLE_BATCH).await?;
    let times = multi(p, "time");
    let submission = p
        .get("submission")
        .map(|_| bool_param(p, "submission"))
        .unwrap_or(true);
    let now = Utc::now();
    let played_times = ids
        .iter()
        .enumerate()
        .map(|(index, _)| {
            times
                .get(index)
                .map(|value| {
                    value
                        .parse::<i64>()
                        .ok()
                        .and_then(|timestamp| {
                            Utc.timestamp_millis_opt(timestamp)
                                .single()
                                .map(|time| (time.to_rfc3339(), timestamp.div_euclid(1_000)))
                        })
                        .ok_or_else(|| ApiFailure::new(10, "Invalid scrobble time"))
                })
                .transpose()?
                .map_or_else(|| Ok((now.to_rfc3339(), now.timestamp())), Ok)
        })
        .collect::<Result<Vec<_>, ApiFailure>>()?;
    let tracks = track_entity::Entity::find()
        .filter(track_entity::Column::Id.is_in(ids.iter().cloned()))
        .all(&state.db)
        .await?
        .into_iter()
        .map(|track| (track.id.clone(), track))
        .collect::<HashMap<_, _>>();
    let mut submitted = HashMap::<String, (i64, String)>::new();
    if submission {
        for (id, (played_at, _)) in ids.iter().zip(&played_times) {
            submitted
                .entry(id.clone())
                .and_modify(|(count, latest)| {
                    *count += 1;
                    if played_at > latest {
                        latest.clone_from(played_at);
                    }
                })
                .or_insert_with(|| (1, played_at.clone()));
        }
    }
    let transaction = state.db.begin().await?;
    for (id, (played_at, _)) in ids.iter().zip(&played_times) {
        scrobble_entity::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            user_id: Set(user.id.clone()),
            track_id: Set(id.clone()),
            played_at: Set(played_at.clone()),
            submission: Set(submission as i64),
        }
        .insert(&transaction)
        .await?;
    }
    for (id, (count, last_played_at)) in submitted {
        track_entity::Entity::update_many()
            .col_expr(
                track_entity::Column::PlayCount,
                Expr::col(track_entity::Column::PlayCount).add(count),
            )
            .filter(track_entity::Column::Id.eq(&id))
            .exec(&transaction)
            .await?;
        increment_user_track_stats(&transaction, &user.id, &id, count, &last_played_at).await?;
    }
    if !submission && let Some(media_id) = ids.last() {
        playback_state_entity::Entity::insert(playback_state_entity::ActiveModel {
            user_id: Set(user.id.clone()),
            media_id: Set(media_id.clone()),
            media_type: Set("song".into()),
            position_ms: Set(0),
            state: Set("playing".into()),
            playback_rate: Set(1.0),
            ignore_scrobble: Set(0),
            scrobbled: Set(0),
            updated_at: Set(Utc::now().to_rfc3339()),
            client: Set(p.get("c").cloned().unwrap_or_default()),
        })
        .on_conflict(
            OnConflict::columns([playback_state_entity::Column::UserId])
                .update_columns([
                    playback_state_entity::Column::MediaId,
                    playback_state_entity::Column::MediaType,
                    playback_state_entity::Column::PositionMs,
                    playback_state_entity::Column::State,
                    playback_state_entity::Column::PlaybackRate,
                    playback_state_entity::Column::IgnoreScrobble,
                    playback_state_entity::Column::Scrobbled,
                    playback_state_entity::Column::UpdatedAt,
                    playback_state_entity::Column::Client,
                ])
                .to_owned(),
        )
        .exec_without_returning(&transaction)
        .await?;
    }
    transaction.commit().await?;
    let reports = ids
        .iter()
        .zip(played_times)
        .filter_map(|(id, (_, timestamp))| tracks.get(id).cloned().map(|track| (track, timestamp)))
        .collect::<Vec<_>>();
    let state = state.clone();
    let user_id = user.id.clone();
    tokio::spawn(async move {
        if let Err(error) = lastfm::report(&state, &user_id, reports, submission).await {
            tracing::warn!(%error, %user_id, "failed to report playback to Last.fm");
        }
    });
    Ok(json!({}))
}

async fn increment_user_track_stats(
    db: &DatabaseTransaction,
    user_id: &str,
    track_id: &str,
    count: i64,
    last_played_at: &str,
) -> Result<(), sea_orm::DbErr> {
    if count <= 0 {
        return Ok(());
    }
    user_track_stat_entity::Entity::insert(user_track_stat_entity::ActiveModel {
        user_id: Set(user_id.to_owned()),
        track_id: Set(track_id.to_owned()),
        play_count: Set(count),
        last_played_at: Set(last_played_at.to_owned()),
    })
    .on_conflict(
        OnConflict::columns([
            user_track_stat_entity::Column::UserId,
            user_track_stat_entity::Column::TrackId,
        ])
        .value(
            user_track_stat_entity::Column::PlayCount,
            Expr::col((
                user_track_stat_entity::Entity,
                user_track_stat_entity::Column::PlayCount,
            ))
            .add(count),
        )
        .value(
            user_track_stat_entity::Column::LastPlayedAt,
            Expr::cust(
                "CASE WHEN user_track_stats.last_played_at >= excluded.last_played_at \
                 THEN user_track_stats.last_played_at ELSE excluded.last_played_at END",
            ),
        )
        .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    Ok(())
}

async fn shares(state: &AppState, user: &User) -> Result<Value, ApiFailure> {
    let rows = share_entity::Entity::find()
        .filter(share_entity::Column::UserId.eq(&user.id))
        .order_by_desc(share_entity::Column::CreatedAt)
        .all(&state.db)
        .await?;
    let mut values = Vec::with_capacity(rows.len());
    for row in &rows {
        values.push(share_json(state, user, row).await?);
    }
    Ok(json!({"shares":{"share":values}}))
}
async fn create_share(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = Uuid::new_v4().to_string();
    let ids = multi(p, "id");
    if ids.is_empty() {
        return Err(ApiFailure::new(10, "Missing required parameter: id"));
    }
    validate_share_ids(state, access, &ids).await?;
    let share = share_entity::ActiveModel {
        id: Set(id),
        user_id: Set(user.id.clone()),
        item_ids: Set(serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into())),
        description: Set(p.get("description").cloned().unwrap_or_default()),
        expires_at: Set(parse_optional_timestamp(p.get("expires"), "expires")?),
        created_at: Set(Utc::now().to_rfc3339()),
        play_count: Set(0),
        last_visited_at: Set(None),
    }
    .insert(&state.db)
    .await?;
    Ok(json!({"shares":{"share":[share_json(state,user,&share).await?]}}))
}
async fn update_share(
    state: &AppState,
    user: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = required(p, "id")?;
    let share = share_entity::Entity::find_by_id(id)
        .filter(share_entity::Column::UserId.eq(&user.id))
        .one(&state.db)
        .await?
        .ok_or_else(not_found)?;
    let mut active = share.into_active_model();
    if let Some(description) = p.get("description") {
        active.description = Set(description.clone());
    }
    if p.contains_key("expires") {
        active.expires_at = Set(parse_optional_timestamp(p.get("expires"), "expires")?);
    }
    active.update(&state.db).await?;
    Ok(json!({}))
}
async fn delete_share(state: &AppState, user: &User, id: &str) -> Result<Value, ApiFailure> {
    share_entity::Entity::delete_many()
        .filter(share_entity::Column::Id.eq(id))
        .filter(share_entity::Column::UserId.eq(&user.id))
        .exec(&state.db)
        .await?;
    Ok(json!({}))
}

async fn radio_stations(
    state: &AppState,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let rows = radio_entity::Entity::find()
        .order_by_asc(radio_entity::Column::Name)
        .all(&state.db)
        .await?;
    let proxied = internet_radio::proxy_enabled_ids(&state.db).await?;
    let proxy_base_url = p.get("_mnest_base_url");
    let pulse_client = p
        .get("c")
        .is_some_and(|client| client.trim().eq_ignore_ascii_case("Pulse"));
    Ok(
        json!({"internetRadioStations":{"internetRadioStation":rows.iter().map(|v| {
            let stream_url = if !pulse_client && proxied.contains(&v.id) {
                proxy_base_url
                    .map(|base_url| internet_radio::proxy_stream_url(base_url, &v.id, &state.settings.auth.jwt_secret))
                    .unwrap_or_else(|| v.stream_url.clone())
            } else {
                v.stream_url.clone()
            };
            let cover_art = (!v.cover_url.is_empty()).then(|| format!("radio-{}", v.id));
            let mut station = json!({"id":v.id,"name":v.name,"streamUrl":stream_url,"homePageUrl":v.home_page_url});
            if let Some(cover_art) = cover_art {
                station["coverArt"] = json!(cover_art);
            }
            station
        }).collect::<Vec<_>>()}}),
    )
}
async fn create_radio(
    state: &AppState,
    user: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    require_admin(user)?;
    let (name, stream_url, home_page_url, cover_url) = validated_radio_fields(p)?;
    let id = Uuid::new_v4().to_string();
    let transaction = state.db.begin().await?;
    radio_entity::ActiveModel {
        id: Set(id.clone()),
        name: Set(name),
        stream_url: Set(stream_url),
        home_page_url: Set(home_page_url),
        cover_url: Set(cover_url),
    }
    .insert(&transaction)
    .await?;
    internet_radio::set_proxy_enabled(&transaction, &id, bool_param(p, "proxy")).await?;
    transaction.commit().await?;
    Ok(json!({}))
}
async fn update_radio(
    state: &AppState,
    user: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    require_admin(user)?;
    let (name, stream_url, home_page_url, cover_url) = validated_radio_fields(p)?;
    let transaction = state.db.begin().await?;
    let radio = radio_entity::Entity::find_by_id(required(p, "id")?)
        .one(&transaction)
        .await?
        .ok_or_else(not_found)?;
    let radio_id = radio.id.clone();
    let stream_url_changed = radio.stream_url != stream_url;
    let mut active = radio.into_active_model();
    active.name = Set(name);
    active.stream_url = Set(stream_url);
    active.home_page_url = Set(home_page_url);
    active.cover_url = Set(cover_url);
    active.update(&transaction).await?;
    if p.contains_key("proxy") {
        internet_radio::set_proxy_enabled(&transaction, &radio_id, bool_param(p, "proxy")).await?;
    }
    transaction.commit().await?;
    if let Err(error) = state.radio_covers.clear_station(&radio_id).await {
        tracing::warn!(%radio_id, %error, "failed to clear updated radio cover cache");
    }
    if stream_url_changed {
        state.radio_streams.cancel(&radio_id).await;
    }
    Ok(json!({}))
}

fn validated_radio_fields(
    p: &HashMap<String, String>,
) -> Result<(String, String, String, String), ApiFailure> {
    let name = required(p, "name")?.trim();
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err(ApiFailure::new(10, "Invalid internet radio name"));
    }
    let stream_url = validate_radio_url(required(p, "streamUrl")?, "streamUrl")?;
    let home_page_url = match p.get("homepageUrl").map(|value| value.trim()) {
        Some(value) if !value.is_empty() => validate_radio_url(value, "homepageUrl")?,
        _ => String::new(),
    };
    let cover_url = match p.get("coverUrl").map(|value| value.trim()) {
        Some(value) if !value.is_empty() => validate_radio_url(value, "coverUrl")?,
        _ => String::new(),
    };
    Ok((name.to_owned(), stream_url, home_page_url, cover_url))
}

fn validate_radio_url(value: &str, parameter: &str) -> Result<String, ApiFailure> {
    let value = value.trim();
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(ApiFailure::new(
            10,
            format!("Invalid internet radio {parameter}"),
        ));
    }
    let url = Url::parse(value)
        .map_err(|_| ApiFailure::new(10, format!("Invalid internet radio {parameter}")))?;
    if parameter == "coverUrl" && (!url.username().is_empty() || url.password().is_some()) {
        return Err(ApiFailure::new(
            10,
            "Internet radio coverUrl must not contain credentials",
        ));
    }
    let valid_url = if parameter == "streamUrl" {
        internet_radio::is_supported_stream_url(&url)
    } else {
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
    };
    if !valid_url {
        return Err(ApiFailure::new(
            10,
            if parameter == "streamUrl" {
                "Internet radio streamUrl must use HTTP, HTTPS, RTSP, MMS, MMSH or MMST".into()
            } else {
                format!("Internet radio {parameter} must use HTTP or HTTPS")
            },
        ));
    }
    if parameter == "streamUrl" && internet_radio::is_proxy_stream_url(&url) {
        return Err(ApiFailure::new(
            10,
            "Internet radio streamUrl must be the original stream URL",
        ));
    }
    Ok(value.to_owned())
}

async fn get_user(state: &AppState, requester: &User, username: &str) -> Result<Value, ApiFailure> {
    if requester.role != "admin" && requester.username != username {
        return Err(ApiFailure::new(50, "Not authorized"));
    }
    let user = user_by_name(&state.db, username)
        .await?
        .ok_or_else(not_found)?;
    let access = subsonic_access(state, &user).await?;
    let folder_ids = user_folder_ids(state, &access).await?;
    Ok(json!({"user":user_json(&user, &access, &folder_ids)}))
}
async fn get_users(state: &AppState, requester: &User) -> Result<Value, ApiFailure> {
    require_admin(requester)?;
    let users = user_entity::Entity::find()
        .order_by_asc(user_entity::Column::Username)
        .all(&state.db)
        .await?;
    let mut values = Vec::with_capacity(users.len());
    for user in users {
        let access = subsonic_access(state, &user).await?;
        let folder_ids = user_folder_ids(state, &access).await?;
        values.push(user_json(&user, &access, &folder_ids));
    }
    Ok(json!({"users":{"user":values}}))
}

async fn access_from_params(
    state: &AppState,
    existing: Option<SubsonicAccess>,
    p: &HashMap<String, String>,
) -> Result<SubsonicAccess, ApiFailure> {
    let mut access = existing.unwrap_or(SubsonicAccess {
        ldap_authenticated: false,
        settings_role: true,
        stream_role: true,
        jukebox_role: false,
        download_role: false,
        upload_role: false,
        playlist_role: false,
        cover_art_role: false,
        comment_role: false,
        podcast_role: false,
        share_role: false,
        video_conversion_role: false,
        max_bit_rate: 0,
        folder_ids: None,
    });
    if bool_param(p, "ldapAuthenticated") {
        return Err(ApiFailure::new(
            10,
            "LDAP authentication is not enabled on this server",
        ));
    }
    access.ldap_authenticated = false;
    for (key, field) in [
        ("settingsRole", &mut access.settings_role),
        ("streamRole", &mut access.stream_role),
        ("jukeboxRole", &mut access.jukebox_role),
        ("downloadRole", &mut access.download_role),
        ("uploadRole", &mut access.upload_role),
        ("playlistRole", &mut access.playlist_role),
        ("coverArtRole", &mut access.cover_art_role),
        ("commentRole", &mut access.comment_role),
        ("podcastRole", &mut access.podcast_role),
        ("shareRole", &mut access.share_role),
        ("videoConversionRole", &mut access.video_conversion_role),
    ] {
        if p.contains_key(key) {
            *field = bool_param(p, key);
        }
    }
    if p.contains_key("maxBitRate") {
        let max_bit_rate = required_i64(p, "maxBitRate")?;
        if !matches!(
            max_bit_rate,
            0 | 32 | 40 | 48 | 56 | 64 | 80 | 96 | 112 | 128 | 160 | 192 | 224 | 256 | 320
        ) {
            return Err(ApiFailure::new(10, "Invalid maxBitRate"));
        }
        access.max_bit_rate = max_bit_rate;
    }
    if p.contains_key("musicFolderId") {
        let mut folder_ids = HashSet::new();
        for folder_id in multi(p, "musicFolderId") {
            let folder = find_music_folder(state, &folder_id)
                .await?
                .ok_or_else(not_found)?;
            folder_ids.insert(folder.id);
        }
        access.folder_ids = Some(folder_ids);
    }
    Ok(access)
}

async fn save_access(
    db: &DatabaseTransaction,
    user_id: &str,
    access: &SubsonicAccess,
) -> Result<(), ApiFailure> {
    let folder_ids = if let Some(folder_ids) = &access.folder_ids {
        let mut folder_ids = folder_ids.iter().cloned().collect::<Vec<_>>();
        folder_ids.sort();
        serde_json::to_string(&folder_ids).unwrap_or_else(|_| "[]".into())
    } else {
        "*".into()
    };
    access_entity::Entity::insert(access_entity::ActiveModel {
        user_id: Set(user_id.to_owned()),
        ldap_authenticated: Set(access.ldap_authenticated as i64),
        settings_role: Set(access.settings_role as i64),
        stream_role: Set(access.stream_role as i64),
        jukebox_role: Set(access.jukebox_role as i64),
        download_role: Set(access.download_role as i64),
        upload_role: Set(access.upload_role as i64),
        playlist_role: Set(access.playlist_role as i64),
        cover_art_role: Set(access.cover_art_role as i64),
        comment_role: Set(access.comment_role as i64),
        podcast_role: Set(access.podcast_role as i64),
        share_role: Set(access.share_role as i64),
        video_conversion_role: Set(access.video_conversion_role as i64),
        max_bit_rate: Set(access.max_bit_rate),
        folder_ids: Set(folder_ids),
    })
    .on_conflict(
        OnConflict::columns([access_entity::Column::UserId])
            .update_columns([
                access_entity::Column::LdapAuthenticated,
                access_entity::Column::SettingsRole,
                access_entity::Column::StreamRole,
                access_entity::Column::JukeboxRole,
                access_entity::Column::DownloadRole,
                access_entity::Column::UploadRole,
                access_entity::Column::PlaylistRole,
                access_entity::Column::CoverArtRole,
                access_entity::Column::CommentRole,
                access_entity::Column::PodcastRole,
                access_entity::Column::ShareRole,
                access_entity::Column::VideoConversionRole,
                access_entity::Column::MaxBitRate,
                access_entity::Column::FolderIds,
            ])
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    Ok(())
}

async fn create_user(
    state: &AppState,
    requester: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    require_admin(requester)?;
    let username = required(p, "username")?;
    let password = decode_subsonic_password(required(p, "password")?)?;
    validate_username(username)?;
    validate_new_password(&password)?;
    let hash = hash_password(&password)?;
    let user_id = Uuid::new_v4().to_string();
    let api_key = Uuid::new_v4().simple().to_string();
    let protected_api_key =
        protect_subsonic_api_key(&api_key, &state.settings.auth.jwt_secret, &user_id)?;
    let access = access_from_params(state, None, p).await?;
    let transaction = state.db.begin().await?;
    user_entity::ActiveModel {
        id: Set(user_id.clone()),
        username: Set(username.to_owned()),
        password_hash: Set(hash),
        email: Set(required(p, "email")?.to_owned()),
        role: Set(if bool_param(p, "adminRole") {
            "admin".into()
        } else {
            "user".into()
        }),
        subsonic_token: Set(protected_api_key),
        subsonic_password: Set(encrypt_subsonic_password(
            &password,
            &state.settings.auth.jwt_secret,
            username,
        )?),
        created_at: Set(Utc::now().to_rfc3339()),
    }
    .insert(&transaction)
    .await?;
    save_access(&transaction, &user_id, &access).await?;
    transaction.commit().await?;
    Ok(json!({}))
}
async fn update_user(
    state: &AppState,
    requester: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    require_admin(requester)?;
    let username = required(p, "username")?;
    let user = user_by_name(&state.db, username)
        .await?
        .ok_or_else(not_found)?;
    let user_id = user.id.clone();
    let access = access_from_params(state, Some(subsonic_access(state, &user).await?), p).await?;
    let mut active = user.into_active_model();
    if let Some(email) = p.get("email") {
        active.email = Set(email.clone());
    }
    if p.contains_key("adminRole") {
        if requester.username == username && !bool_param(p, "adminRole") {
            return Err(ApiFailure::new(
                50,
                "Cannot remove your own administrator role",
            ));
        }
        active.role = Set(if bool_param(p, "adminRole") {
            "admin".into()
        } else {
            "user".into()
        });
    }
    if let Some(password) = p.get("password") {
        let password = decode_subsonic_password(password)?;
        validate_new_password(&password)?;
        active.password_hash = Set(hash_password(&password)?);
        active.subsonic_password = Set(encrypt_subsonic_password(
            &password,
            &state.settings.auth.jwt_secret,
            username,
        )?);
    }
    let transaction = state.db.begin().await?;
    active.update(&transaction).await?;
    save_access(&transaction, &user_id, &access).await?;
    transaction.commit().await?;
    Ok(json!({}))
}
async fn delete_user(
    state: &AppState,
    requester: &User,
    username: &str,
) -> Result<Value, ApiFailure> {
    require_admin(requester)?;
    if requester.username == username {
        return Err(ApiFailure::new(50, "Cannot delete current administrator"));
    }
    let user = user_by_name(&state.db, username)
        .await?
        .ok_or_else(not_found)?;
    let transaction = state.db.begin().await?;
    let playlist_ids = playlist_entity::Entity::find()
        .select_only()
        .column(playlist_entity::Column::Id)
        .filter(playlist_entity::Column::UserId.eq(&user.id))
        .into_query();
    playlist_track_entity::Entity::delete_many()
        .filter(playlist_track_entity::Column::PlaylistId.in_subquery(playlist_ids))
        .exec(&transaction)
        .await?;
    playlist_entity::Entity::delete_many()
        .filter(playlist_entity::Column::UserId.eq(&user.id))
        .exec(&transaction)
        .await?;
    favorite_entity::Entity::delete_many()
        .filter(favorite_entity::Column::UserId.eq(&user.id))
        .exec(&transaction)
        .await?;
    rating_entity::Entity::delete_many()
        .filter(rating_entity::Column::UserId.eq(&user.id))
        .exec(&transaction)
        .await?;
    bookmark_entity::Entity::delete_many()
        .filter(bookmark_entity::Column::UserId.eq(&user.id))
        .exec(&transaction)
        .await?;
    scrobble_entity::Entity::delete_many()
        .filter(scrobble_entity::Column::UserId.eq(&user.id))
        .exec(&transaction)
        .await?;
    user_track_stat_entity::Entity::delete_many()
        .filter(user_track_stat_entity::Column::UserId.eq(&user.id))
        .exec(&transaction)
        .await?;
    share_entity::Entity::delete_many()
        .filter(share_entity::Column::UserId.eq(&user.id))
        .exec(&transaction)
        .await?;
    access_entity::Entity::delete_by_id(&user.id)
        .exec(&transaction)
        .await?;
    playback_state_entity::Entity::delete_by_id(&user.id)
        .exec(&transaction)
        .await?;
    play_queue_entity::Entity::delete_by_id(&user.id)
        .exec(&transaction)
        .await?;
    lastfm::delete_user_authorization(&transaction, &user.id).await?;
    user_preferences::delete(&transaction, &user.id).await?;
    user_entity::Entity::delete_by_id(user.id)
        .exec(&transaction)
        .await?;
    transaction.commit().await?;
    Ok(json!({}))
}
async fn change_password(
    state: &AppState,
    requester: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let username = required(p, "username")?;
    if requester.role != "admin" && requester.username != username {
        return Err(ApiFailure::new(50, "Not authorized"));
    }
    if requester.role != "admin" && !access.settings_role {
        return Err(ApiFailure::new(50, "Settings role required"));
    }
    let password = decode_subsonic_password(required(p, "password")?)?;
    validate_new_password(&password)?;
    let user = user_by_name(&state.db, username)
        .await?
        .ok_or_else(not_found)?;
    let mut active = user.into_active_model();
    active.password_hash = Set(hash_password(&password)?);
    active.subsonic_password = Set(encrypt_subsonic_password(
        &password,
        &state.settings.auth.jwt_secret,
        username,
    )?);
    active.update(&state.db).await?;
    Ok(json!({}))
}

async fn bookmarks(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
) -> Result<Value, ApiFailure> {
    let rows = bookmark_entity::Entity::find()
        .filter(bookmark_entity::Column::UserId.eq(&user.id))
        .order_by_desc(bookmark_entity::Column::ChangedAt)
        .all(&state.db)
        .await?;
    let mut values = Vec::new();
    for row in rows {
        if let Ok(track) = accessible_track(state, access, &row.track_id).await {
            values.push(json!({"position":row.position,"username":user.username,"comment":row.comment,"created":row.changed_at,"changed":row.changed_at,"entry":track_json(&track,None)}));
        }
    }
    Ok(json!({"bookmarks":{"bookmark":values}}))
}
async fn create_bookmark(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = required(p, "id")?;
    accessible_track(state, access, id).await?;
    bookmark_entity::Entity::delete_many()
        .filter(bookmark_entity::Column::UserId.eq(&user.id))
        .filter(bookmark_entity::Column::TrackId.eq(id))
        .exec(&state.db)
        .await?;
    bookmark_entity::ActiveModel {
        user_id: Set(user.id.clone()),
        track_id: Set(id.to_owned()),
        position: Set(required_i64(p, "position")?.max(0)),
        comment: Set(p.get("comment").cloned().unwrap_or_default()),
        changed_at: Set(Utc::now().to_rfc3339()),
    }
    .insert(&state.db)
    .await?;
    Ok(json!({}))
}
async fn delete_bookmark(state: &AppState, user: &User, id: &str) -> Result<Value, ApiFailure> {
    bookmark_entity::Entity::delete_many()
        .filter(bookmark_entity::Column::UserId.eq(&user.id))
        .filter(bookmark_entity::Column::TrackId.eq(id))
        .exec(&state.db)
        .await?;
    Ok(json!({}))
}
async fn get_play_queue(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    by_index: bool,
) -> Result<Value, ApiFailure> {
    let row = play_queue_entity::Entity::find_by_id(&user.id)
        .one(&state.db)
        .await?;
    let Some(row) = row else {
        let queue = json!({"username":user.username,"changed":"1970-01-01T00:00:00Z","changedBy":"","entry":[]});
        return Ok(if by_index {
            json!({"playQueueByIndex":queue})
        } else {
            json!({"playQueue":queue})
        });
    };
    let ids: Vec<String> = serde_json::from_str(&row.track_ids).unwrap_or_default();
    let requested_ids = ids.iter().cloned().collect::<HashSet<_>>();
    let tracks = if requested_ids.is_empty() {
        HashMap::new()
    } else {
        accessible_tracks(access)
            .filter(track_entity::Column::Id.is_in(requested_ids))
            .all(&state.db)
            .await?
            .into_iter()
            .map(|track| (track.id.clone(), track))
            .collect::<HashMap<_, _>>()
    };
    let mut visible_entries = Vec::new();
    for (original_index, id) in ids.iter().enumerate() {
        if let Some(track) = tracks.get(id) {
            visible_entries.push((original_index, track.clone()));
        }
    }
    let entries = visible_entries
        .iter()
        .map(|(_, track)| track_json(track, None))
        .collect::<Vec<_>>();
    let mut queue = json!({"position":row.position,"username":user.username,"changed":row.changed_at,"changedBy":row.changed_by,"entry":entries});
    if !visible_entries.is_empty() {
        let original_current_index = row
            .current_index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < ids.len())
            .or_else(|| {
                row.current_id
                    .as_ref()
                    .and_then(|current| ids.iter().position(|id| id == current))
            })
            .unwrap_or(visible_entries[0].0);
        let visible_current_index = visible_entries
            .iter()
            .position(|(original_index, _)| *original_index == original_current_index)
            .unwrap_or(0);
        if by_index {
            queue["currentIndex"] = json!(visible_current_index);
        } else {
            queue["current"] = json!(&visible_entries[visible_current_index].1.id);
        }
    }
    Ok(if by_index {
        json!({"playQueueByIndex":queue})
    } else {
        json!({"playQueue":queue})
    })
}
async fn save_play_queue(
    state: &AppState,
    user: &User,
    access: &SubsonicAccess,
    p: &HashMap<String, String>,
    by_index: bool,
) -> Result<Value, ApiFailure> {
    let ids = multi(p, "id");
    validate_track_ids(state, access, &ids, MAX_COLLECTION_ITEMS).await?;
    let (current_id, current_index) = if ids.is_empty() {
        if by_index && p.contains_key("currentIndex") {
            return Err(ApiFailure::new(
                10,
                "currentIndex must not be set for an empty play queue",
            ));
        }
        (None, None)
    } else if by_index {
        let index = required_i64(p, "currentIndex")?;
        let index = usize::try_from(index)
            .ok()
            .filter(|index| *index < ids.len())
            .ok_or_else(|| ApiFailure::new(10, "currentIndex is outside the play queue"))?;
        let current = ids
            .get(index)
            .cloned()
            .ok_or_else(|| ApiFailure::new(10, "currentIndex is outside the play queue"))?;
        (Some(current), Some(index as i64))
    } else {
        let current = required(p, "current")?.to_owned();
        if !ids.contains(&current) {
            return Err(ApiFailure::new(
                10,
                "current is not present in the play queue",
            ));
        }
        let index = ids
            .iter()
            .position(|id| id == &current)
            .map(|index| index as i64);
        (Some(current), index)
    };
    play_queue_entity::Entity::insert(play_queue_entity::ActiveModel {
        user_id: Set(user.id.clone()),
        track_ids: Set(serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into())),
        current_id: Set(current_id),
        current_index: Set(current_index),
        position: Set(int(p, "position", 0).max(0)),
        changed_at: Set(Utc::now().to_rfc3339()),
        changed_by: Set(p.get("c").cloned().unwrap_or_default()),
    })
    .on_conflict(
        OnConflict::columns([play_queue_entity::Column::UserId])
            .update_columns([
                play_queue_entity::Column::TrackIds,
                play_queue_entity::Column::CurrentId,
                play_queue_entity::Column::CurrentIndex,
                play_queue_entity::Column::Position,
                play_queue_entity::Column::ChangedAt,
                play_queue_entity::Column::ChangedBy,
            ])
            .to_owned(),
    )
    .exec_without_returning(&state.db)
    .await?;
    Ok(json!({}))
}

async fn validate_track_ids(
    state: &AppState,
    access: &SubsonicAccess,
    track_ids: &[String],
    max_items: usize,
) -> Result<(), ApiFailure> {
    if track_ids.len() > max_items {
        return Err(ApiFailure::new(10, "Too many song IDs in one request"));
    }
    let requested_ids = track_ids.iter().cloned().collect::<HashSet<_>>();
    if requested_ids.is_empty() {
        return Ok(());
    }
    let existing_ids = accessible_tracks(access)
        .select_only()
        .column(track_entity::Column::Id)
        .filter(track_entity::Column::Id.is_in(requested_ids.iter().cloned()))
        .into_tuple::<String>()
        .all(&state.db)
        .await?
        .into_iter()
        .collect::<HashSet<_>>();
    if existing_ids != requested_ids {
        return Err(ApiFailure::new(
            70,
            "Collection contains an unknown song ID",
        ));
    }
    Ok(())
}

async fn validate_share_ids(
    state: &AppState,
    access: &SubsonicAccess,
    ids: &[String],
) -> Result<(), ApiFailure> {
    if ids.len() > MAX_CATALOG_MUTATION_ITEMS {
        return Err(ApiFailure::new(10, "Too many IDs in one request"));
    }
    let requested = ids.iter().cloned().collect::<HashSet<_>>();
    let mut existing = existing_catalog_ids(state, access, "track", &requested).await?;
    existing.extend(existing_catalog_ids(state, access, "album", &requested).await?);
    if existing != requested {
        return Err(not_found());
    }
    Ok(())
}
async fn scan_status(state: &AppState) -> Result<Value, ApiFailure> {
    let job = job_entity::Entity::find()
        .filter(job_entity::Column::Kind.eq("scan"))
        .order_by_desc(job_entity::Column::CreatedAt)
        .one(&state.db)
        .await?;
    Ok(
        json!({"scanStatus":{"scanning":job.as_ref().is_some_and(|job|matches!(job.state.as_str(),"pending"|"running"))}}),
    )
}
async fn start_scan(state: &AppState, user: &User) -> Result<Value, ApiFailure> {
    require_admin(user)?;
    let id = jobs::enqueue(state, "scan", &ScanPayload {}).await?;
    Ok(json!({"scanStatus":{"scanning":true,"jobId":id}}))
}

async fn serve_file(
    path: PathBuf,
    download: bool,
    range_header: Option<&str>,
    shutdown: CancellationToken,
) -> anyhow::Result<Response> {
    let mut file = tokio::fs::File::open(&path).await?;
    let total = file.metadata().await?.len();
    let range = range_header.and_then(|value| parse_range(value, total));
    let (start, end) = range.unwrap_or((0, total.saturating_sub(1)));
    file.seek(SeekFrom::Start(start)).await?;
    let length = end.saturating_sub(start) + 1;
    let stream = ReaderStream::new(file.take(length)).take_until(shutdown.cancelled_owned());
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(
            mime_guess::from_path(&path)
                .first_or_octet_stream()
                .essence_str(),
        )?,
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string())?,
    );
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if range.is_some() {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{total}"))?,
        );
    }
    if download {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!(
                "attachment; filename*=UTF-8''{}",
                urlencoding::encode(&name)
            ))?,
        );
    }
    Ok(response)
}

fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?.split(',').next()?;
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(total);
        return Some((total.saturating_sub(suffix), total.saturating_sub(1)));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= total {
        return None;
    }
    let end = if end.is_empty() {
        total.saturating_sub(1)
    } else {
        end.parse::<u64>().ok()?.min(total.saturating_sub(1))
    };
    (start <= end).then_some((start, end))
}
async fn transcode(
    state: &AppState,
    track: &Track,
    format: &str,
    max_bitrate: Option<u32>,
    offset: Option<&str>,
    range_header: Option<&str>,
) -> anyhow::Result<Response> {
    let (audio_format, mime_extension) = match format.to_ascii_lowercase().as_str() {
        "mp3" => (AudioFormat::Mp3, "mp3"),
        "opus" => (AudioFormat::Opus, "opus"),
        "aac" => (AudioFormat::Aac, "aac"),
        "flac" => (AudioFormat::Flac, "flac"),
        "ogg" => (AudioFormat::OggVorbis, "ogg"),
        _ => anyhow::bail!("unsupported transcode format"),
    };
    let cache_path = transcode_cache_path(state, track, mime_extension, max_bitrate, offset).await;
    if let Some(path) = cache_path.as_ref() {
        match transcode_cache_is_fresh(FsPath::new(&track.path), path).await {
            Ok(true) => {
                return serve_file(path.clone(), false, range_header, state.shutdown.clone()).await;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, cache = %path.display(), "failed to validate transcode cache");
            }
        }
    }
    let mut request = TranscodeRequest::file(PathBuf::from(&track.path), audio_format);
    request.bitrate_kbps = max_bitrate.map(|rate| rate.clamp(16, 320));
    request.offset = offset.and_then(transcode_offset);
    let stdout = state.media.transcode(request)?;
    let cache = match cache_path {
        Some(path) => match PendingTranscodeCache::create(path).await {
            Ok(cache) => Some(cache),
            Err(error) => {
                tracing::warn!(%error, "failed to create transcode cache file");
                None
            }
        },
        None => None,
    };
    let stream = futures::stream::try_unfold(
        TranscodeOutputState { stdout, cache },
        |mut state| async move {
            match state.stdout.next().await {
                Some(Ok(chunk)) => {
                    if let Some(cache) = state.cache.as_mut()
                        && let Err(error) = cache.write_chunk(&chunk).await
                    {
                        tracing::warn!(%error, "failed to write transcode cache; continuing playback");
                        state.cache = None;
                    }
                    Ok(Some((chunk, state)))
                }
                Some(Err(error)) => Err(error),
                None => {
                    if let Some(cache) = state.cache.as_mut()
                        && let Err(error) = cache.commit().await
                    {
                        tracing::warn!(%error, "failed to finalize transcode cache");
                    }
                    Ok(None)
                }
            }
        },
    );
    let stream = stream.take_until(state.shutdown.clone().cancelled_owned());
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(
            mime_guess::from_ext(mime_extension)
                .first_or_octet_stream()
                .essence_str(),
        )?,
    );
    Ok(response)
}

fn transcode_offset(value: &str) -> Option<Duration> {
    let seconds = value.parse::<f64>().ok()?;
    (seconds.is_finite() && seconds > 0.0)
        .then(|| Duration::try_from_secs_f64(seconds).ok())
        .flatten()
}

struct TranscodeOutputState {
    stdout: MediaStream,
    cache: Option<PendingTranscodeCache>,
}

struct PendingTranscodeCache {
    file: Option<tokio::fs::File>,
    partial_path: Option<PathBuf>,
    publish_path: Option<PathBuf>,
    final_path: PathBuf,
}

impl PendingTranscodeCache {
    async fn create(final_path: PathBuf) -> io::Result<Self> {
        let parent = final_path
            .parent()
            .ok_or_else(|| io::Error::other("transcode cache path has no parent"))?;
        tokio::fs::create_dir_all(parent).await?;
        let temporary_directory = FsPath::new("/tmp/mnest-transcodes");
        tokio::fs::create_dir_all(temporary_directory).await?;
        let extension = final_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("audio");
        let partial_path =
            temporary_directory.join(format!("{}.{}.partial", Uuid::new_v4().simple(), extension));
        let file = tokio::fs::File::create(&partial_path).await?;
        Ok(Self {
            file: Some(file),
            partial_path: Some(partial_path),
            publish_path: None,
            final_path,
        })
    }

    async fn write_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.write_all(chunk).await?;
        }
        Ok(())
    }

    async fn commit(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush().await?;
            drop(file);
        }
        let Some(partial_path) = self.partial_path.take() else {
            return Ok(());
        };
        match tokio::fs::rename(&partial_path, &self.final_path).await {
            Ok(()) => Ok(()),
            Err(_) => {
                let extension = self
                    .final_path
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("audio");
                let publish_path = self.final_path.with_extension(format!(
                    "{extension}.publishing-{}",
                    Uuid::new_v4().simple()
                ));
                self.publish_path = Some(publish_path.clone());
                if let Err(error) = tokio::fs::copy(&partial_path, &publish_path).await {
                    self.partial_path = Some(partial_path);
                    return Err(error);
                }
                match tokio::fs::rename(&publish_path, &self.final_path).await {
                    Ok(()) => {
                        self.publish_path = None;
                        let _ = tokio::fs::remove_file(partial_path).await;
                        Ok(())
                    }
                    Err(_)
                        if tokio::fs::metadata(&self.final_path)
                            .await
                            .is_ok_and(|metadata| metadata.len() > 0) =>
                    {
                        self.publish_path = None;
                        let _ = tokio::fs::remove_file(publish_path).await;
                        let _ = tokio::fs::remove_file(partial_path).await;
                        Ok(())
                    }
                    Err(error) => {
                        self.partial_path = Some(partial_path);
                        Err(error)
                    }
                }
            }
        }
    }
}

impl Drop for PendingTranscodeCache {
    fn drop(&mut self) {
        if let Some(path) = self.partial_path.take() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(path) = self.publish_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn transcode_cache_path(
    state: &AppState,
    track: &Track,
    format: &str,
    max_bitrate: Option<u32>,
    offset: Option<&str>,
) -> Option<PathBuf> {
    let settings = match transcode_cache::load(&state.db, &track.folder_id).await {
        Ok(settings) if settings.enabled => settings,
        Ok(_) => return None,
        Err(error) => {
            tracing::warn!(%error, "failed to read transcode cache settings");
            return None;
        }
    };
    if let Err(error) = settings.prepare().await {
        tracing::warn!(%error, path = %settings.path.display(), "transcode cache is unavailable");
        return None;
    }
    let artist = transcode_cache_artist_name(track);
    match settings.entry_path(transcode_cache::TranscodeCacheEntry {
        folder_id: &track.folder_id,
        artist: &artist,
        album: &track.album_name,
        title: &track.title,
        format,
        bitrate: transcode_cache_bitrate(track, max_bitrate),
        offset,
    }) {
        Ok(path) => Some(path),
        Err(error) => {
            tracing::warn!(%error, source = %track.path, "failed to build transcode cache key");
            None
        }
    }
}

fn transcode_cache_artist_name(track: &Track) -> String {
    let artists = serde_json::from_str::<Vec<ArtistCredit>>(&track.artists_json)
        .unwrap_or_default()
        .into_iter()
        .map(|artist| artist.name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if artists.is_empty() {
        parse_artist_names(&track.artist_name).join("-")
    } else {
        artists.join("-")
    }
}

fn transcode_cache_bitrate(track: &Track, max_bitrate: Option<u32>) -> u32 {
    max_bitrate
        .map(|rate| rate.clamp(16, 320))
        .or_else(|| u32::try_from(track.bit_rate).ok().filter(|rate| *rate > 0))
        .unwrap_or(128)
}

async fn transcode_cache_is_fresh(source: &FsPath, cache: &FsPath) -> io::Result<bool> {
    let source_metadata = tokio::fs::metadata(source).await?;
    let cache_metadata = match tokio::fs::metadata(cache).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if cache_metadata.len() == 0 {
        return Ok(false);
    }
    Ok(cache_metadata.modified()? >= source_metadata.modified()?)
}

async fn album(state: &AppState, id: &str) -> Result<Album, ApiFailure> {
    album_entity::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(not_found)
}
fn artist_json(a: &Artist, cover_art: Option<&str>) -> Value {
    let mut value =
        json!({"id":a.id,"name":a.name,"albumCount":a.album_count,"sortName":a.sort_name});
    if let Some(cover_art) = cover_art {
        value["coverArt"] = json!(cover_art);
    }
    value
}
fn legacy_artist_json(a: &Artist) -> Value {
    json!({"id":a.id,"name":a.name})
}
fn artist_child_json(a: &Artist, parent: Option<&str>, cover_art: Option<&str>) -> Value {
    let mut value = json!({"id":a.id,"isDir":true,"title":a.name,"artist":a.name});
    if let Some(parent) = parent {
        value["parent"] = json!(parent);
    }
    if let Some(cover_art) = cover_art {
        value["coverArt"] = json!(cover_art);
    }
    value
}
fn album_json(a: &Album) -> Value {
    let mut value = json!({"id":a.id,"name":a.name,"artist":a.artist_name,"artistId":a.artist_id,"displayArtist":a.artist_name,"songCount":a.song_count,"duration":a.duration as i64,"year":a.year,"genre":a.genre,"created":a.created_at});
    if a.cover_path.as_deref() != Some(MISSING_ARTWORK_MARKER) {
        value["coverArt"] = json!(format!("img-{}", a.id));
    }
    let artist_names = parse_artist_names(&a.artist_name);
    // The album table stores only one artist ID, so do not attach that ID to a combined name.
    if artist_names.len() == 1 {
        value["artists"] = json!([{"id":a.artist_id,"name":artist_names[0]}]);
    }
    value
}
fn album_child_json(album: &Album) -> Value {
    let mut value = json!({"id":album.id,"parent":album.artist_id,"isDir":true,"title":album.name,"album":album.name,"artist":album.artist_name,"artistId":album.artist_id,"albumId":album.id,"duration":album.duration as i64,"year":album.year,"genre":album.genre,"created":album.created_at,"type":"music"});
    if album.cover_path.as_deref() != Some(MISSING_ARTWORK_MARKER) {
        value["coverArt"] = json!(format!("img-{}", album.id));
    }
    value
}
fn track_json(t: &Track, starred: Option<String>) -> Value {
    let artists = serde_json::from_str::<Vec<ArtistCredit>>(&t.artists_json).unwrap_or_default();
    let artist = artists
        .iter()
        .map(|artist| artist.name.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let artist_id = artists.first().map(|artist| artist.id.clone());
    let mut v = json!({"id":t.id,"isDir":false,"isVideo":false,"title":t.title,"album":t.album_name,"artist":artist,"displayArtist":artist,"artists":artists,"track":t.track_number,"discNumber":t.disc_number,"year":t.year,"genre":t.genre,"size":t.size,"contentType":t.mimetype,"suffix":t.suffix,"duration":t.duration as i64,"bitRate":t.bit_rate,"path":t.relative_path,"type":"music","mediaType":"song","playCount":t.play_count,"bookmarkPosition":0,"created":t.created_at,"comment":t.comment});
    if t.cover_path.as_deref() != Some(MISSING_ARTWORK_MARKER) {
        v["coverArt"] = json!(canonical_track_cover_art(&t.id, t.album_id.as_deref()));
    }
    if let Some(album_id) = &t.album_id {
        v["parent"] = json!(album_id);
        v["albumId"] = json!(album_id);
    }
    if let Some(artist_id) = artist_id {
        v["artistId"] = json!(artist_id);
    }
    if !t.album_artist.is_empty() {
        v["displayAlbumArtist"] = json!(t.album_artist);
    }
    if let Some(value) = starred {
        v["starred"] = Value::String(value);
    }
    v
}
fn canonical_track_cover_art(track_id: &str, album_id: Option<&str>) -> String {
    format!("img-{}", album_id.unwrap_or(track_id))
}
async fn playlist_json(
    state: &AppState,
    playlist: &playlist_entity::Model,
    tracks: &[Track],
) -> Result<Value, ApiFailure> {
    let owner = user_entity::Entity::find_by_id(&playlist.user_id)
        .one(&state.db)
        .await?
        .map(|user| user.username)
        .unwrap_or_else(|| playlist.user_id.clone());
    Ok(
        json!({"id":playlist.id,"name":playlist.name,"comment":playlist.comment,"owner":owner,"public":playlist.public!=0,"songCount":tracks.len(),"duration":tracks.iter().map(|track|track.duration as i64).sum::<i64>(),"created":playlist.created_at,"changed":playlist.updated_at}),
    )
}
async fn share_json(
    state: &AppState,
    user: &User,
    share: &share_entity::Model,
) -> Result<Value, ApiFailure> {
    let tracks = shared_tracks(state, share).await?;
    let entries = tracks
        .iter()
        .map(|track| track_json(track, None))
        .collect::<Vec<_>>();
    let base = state
        .settings
        .server
        .public_url
        .as_deref()
        .unwrap_or_default()
        .trim_end_matches('/');
    Ok(
        json!({"id":share.id,"url":format!("{base}/share/{}",share.id),"description":share.description,"username":user.username,"created":share.created_at,"expires":share.expires_at,"lastVisited":share.last_visited_at,"visitCount":share.play_count,"entry":entries}),
    )
}
async fn user_folder_ids(
    state: &AppState,
    access: &SubsonicAccess,
) -> Result<Vec<i32>, ApiFailure> {
    Ok(enabled_music_folders(state)
        .await?
        .iter()
        .filter(|folder| access.allows_folder(&folder.id))
        .map(|folder| folder_api_id(&folder.id))
        .collect())
}
fn user_json(v: &User, access: &SubsonicAccess, folder_ids: &[i32]) -> Value {
    json!({"username":v.username,"email":v.email,"scrobblingEnabled":true,"adminRole":v.role=="admin","ldapAuthenticated":access.ldap_authenticated,"settingsRole":access.settings_role,"downloadRole":access.download_role,"uploadRole":access.upload_role,"playlistRole":access.playlist_role,"coverArtRole":access.cover_art_role,"commentRole":access.comment_role,"podcastRole":access.podcast_role,"streamRole":access.stream_role,"jukeboxRole":access.jukebox_role,"shareRole":access.share_role,"videoConversionRole":access.video_conversion_role,"maxBitRate":access.max_bit_rate,"folder":folder_ids})
}

fn required<'a>(p: &'a HashMap<String, String>, key: &str) -> Result<&'a str, ApiFailure> {
    p.get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiFailure::new(10, format!("Missing required parameter: {key}")))
}
fn present<'a>(p: &'a HashMap<String, String>, key: &str) -> Result<&'a str, ApiFailure> {
    p.get(key)
        .map(String::as_str)
        .ok_or_else(|| ApiFailure::new(10, format!("Missing required parameter: {key}")))
}
fn normalize_search_query(value: &str) -> String {
    let value = value
        .trim()
        .strip_suffix('*')
        .unwrap_or(value.trim())
        .trim();
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
        .trim()
        .to_owned()
}
fn required_anyhow<'a>(p: &'a HashMap<String, String>, key: &str) -> anyhow::Result<&'a str> {
    p.get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .context(format!("Missing required parameter: {key}"))
}
fn required_i64(p: &HashMap<String, String>, key: &str) -> Result<i64, ApiFailure> {
    required(p, key)?
        .parse()
        .map_err(|_| ApiFailure::new(10, format!("Invalid integer parameter: {key}")))
}
fn parse_optional_timestamp(
    value: Option<&String>,
    key: &str,
) -> Result<Option<String>, ApiFailure> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let timestamp = value
        .parse::<i64>()
        .map_err(|_| ApiFailure::new(10, format!("Invalid timestamp parameter: {key}")))?;
    Ok(Some(
        Utc.timestamp_millis_opt(timestamp)
            .single()
            .ok_or_else(|| ApiFailure::new(10, format!("Invalid timestamp parameter: {key}")))?
            .to_rfc3339(),
    ))
}
fn int(p: &HashMap<String, String>, key: &str, default: i64) -> i64 {
    p.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn bool_param(p: &HashMap<String, String>, key: &str) -> bool {
    matches!(p.get(key).map(String::as_str), Some("true" | "1"))
}
fn multi(p: &HashMap<String, String>, key: &str) -> Vec<String> {
    p.get(key)
        .map(|v| {
            let separator = if v.contains('\0') { '\0' } else { ',' };
            v.split(separator)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
fn initial(name: &str) -> String {
    name.chars()
        .next()
        .map(|v| v.to_uppercase().to_string())
        .unwrap_or_else(|| "#".into())
}
fn not_found() -> ApiFailure {
    ApiFailure::new(70, "Requested data was not found")
}
fn require_admin(user: &User) -> Result<(), ApiFailure> {
    if user.role == "admin" {
        Ok(())
    } else {
        Err(ApiFailure::new(50, "Administrator role required"))
    }
}

fn require_permission(allowed: bool, message: &str) -> Result<(), ApiFailure> {
    if allowed {
        Ok(())
    } else {
        Err(ApiFailure::new(50, message))
    }
}

fn validate_username(username: &str) -> Result<(), ApiFailure> {
    if username.trim() != username
        || username.is_empty()
        || username.len() > 128
        || username.chars().any(char::is_control)
    {
        return Err(ApiFailure::new(10, "Invalid username"));
    }
    Ok(())
}

fn validate_new_password(password: &str) -> Result<(), ApiFailure> {
    if !(8..=1024).contains(&password.len()) {
        return Err(ApiFailure::new(
            10,
            "Password must contain between 8 and 1024 bytes",
        ));
    }
    Ok(())
}
fn hash_password(value: &str) -> Result<String, ApiFailure> {
    Ok(Argon2::default()
        .hash_password(value.as_bytes(), &SaltString::generate(&mut OsRng))
        .map_err(|e| ApiFailure::new(0, e.to_string()))?
        .to_string())
}
fn decode_subsonic_password(value: &str) -> Result<String, ApiFailure> {
    if let Some(encoded) = value.strip_prefix("enc:") {
        let bytes = hex::decode(encoded).map_err(|e| ApiFailure::new(10, e.to_string()))?;
        String::from_utf8(bytes).map_err(|e| ApiFailure::new(10, e.to_string()))
    } else {
        Ok(value.to_owned())
    }
}

fn subsonic_response(params: &HashMap<String, String>, data: Value) -> Response {
    render(params, wrapper("ok", data, None), StatusCode::OK)
}
fn subsonic_error(params: &HashMap<String, String>, code: i32, message: &str) -> Response {
    render(
        params,
        wrapper(
            "failed",
            json!({}),
            Some(json!({"code":code,"message":message})),
        ),
        StatusCode::OK,
    )
}
fn wrapper(status: &str, data: Value, error: Option<Value>) -> Value {
    let mut response = Map::new();
    response.insert("status".into(), json!(status));
    response.insert("version".into(), json!(API_VERSION));
    response.insert("type".into(), json!("mNest"));
    response.insert("serverVersion".into(), json!(crate::VERSION));
    response.insert("openSubsonic".into(), json!(true));
    if let Value::Object(values) = data {
        response.extend(values);
    }
    if let Some(error) = error {
        response.insert("error".into(), error);
    }
    json!({"subsonic-response":response})
}
fn render(params: &HashMap<String, String>, value: Value, status: StatusCode) -> Response {
    if params.get("f").map(String::as_str) == Some("json") {
        (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            JsonValue(value),
        )
            .into_response()
    } else {
        let xml = xml_document(&value);
        (
            status,
            [(header::CONTENT_TYPE, "text/xml; charset=utf-8")],
            xml,
        )
            .into_response()
    }
}
fn xml_document(value: &Value) -> String {
    let mut response = value["subsonic-response"].clone();
    if let Value::Object(attributes) = &mut response {
        attributes.insert("xmlns".into(), json!(XML_NAMESPACE));
    }
    let xml = value_to_xml("subsonic-response", &response);
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{xml}")
}
struct JsonValue(Value);
impl IntoResponse for JsonValue {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}
fn value_to_xml(name: &str, value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut attrs = String::new();
            let mut children = String::new();
            let mut text_content = String::new();
            let mut child_values = Vec::new();
            for (key, v) in map {
                match v {
                    Value::Null => {}
                    Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                        if xml_text_field(name, key) {
                            text_content.push_str(&xml_escape(&scalar(v)));
                        } else if xml_scalar_child(name, key) {
                            child_values.push((xml_child_rank(name, key), key, v));
                        } else {
                            attrs.push_str(&format!(" {}=\"{}\"", key, xml_escape(&scalar(v))))
                        }
                    }
                    Value::Array(_) | Value::Object(_) => {
                        child_values.push((xml_child_rank(name, key), key, v));
                    }
                }
            }
            child_values.sort_by(|(left_rank, left_key, _), (right_rank, right_key, _)| {
                left_rank
                    .cmp(right_rank)
                    .then_with(|| left_key.cmp(right_key))
            });
            for (_, key, value) in child_values {
                match value {
                    Value::Array(items) => {
                        for item in items {
                            children.push_str(&value_to_xml(key, item));
                        }
                    }
                    _ => children.push_str(&value_to_xml(key, value)),
                }
            }
            format!("<{name}{attrs}>{text_content}{children}</{name}>")
        }
        Value::Array(items) => items.iter().map(|v| value_to_xml(name, v)).collect(),
        _ => format!("<{name}>{}</{name}>", xml_escape(&scalar(value))),
    }
}

fn xml_text_field(element: &str, field: &str) -> bool {
    field == "value" && matches!(element, "genre" | "lyrics" | "line" | "cue")
}

fn xml_scalar_child(element: &str, field: &str) -> bool {
    matches!(
        (element, field),
        (
            "albumInfo" | "albumInfo2",
            "notes"
                | "musicBrainzId"
                | "lastFmUrl"
                | "smallImageUrl"
                | "mediumImageUrl"
                | "largeImageUrl"
        ) | (
            "artistInfo" | "artistInfo2",
            "biography"
                | "musicBrainzId"
                | "lastFmUrl"
                | "smallImageUrl"
                | "mediumImageUrl"
                | "largeImageUrl"
        )
    )
}

fn xml_child_rank(element: &str, field: &str) -> usize {
    let order: &[&str] = match element {
        "indexes" => &["shortcut", "index", "child"],
        "searchResult2" | "searchResult3" | "starred" | "starred2" => &["artist", "album", "song"],
        "artist" => &["roles", "album"],
        "album" => &[
            "recordLabels",
            "genres",
            "artists",
            "releaseTypes",
            "moods",
            "originalReleaseDate",
            "releaseDate",
            "discTitles",
            "song",
        ],
        "song" | "child" | "entry" | "match" => &[
            "replayGain",
            "genres",
            "artists",
            "albumArtists",
            "contributors",
            "moods",
            "works",
            "movements",
            "groupings",
        ],
        "playlist" => &["allowedUser", "entry"],
        "albumInfo" | "albumInfo2" => &[
            "notes",
            "musicBrainzId",
            "lastFmUrl",
            "smallImageUrl",
            "mediumImageUrl",
            "largeImageUrl",
        ],
        "artistInfo" | "artistInfo2" => &[
            "biography",
            "musicBrainzId",
            "lastFmUrl",
            "smallImageUrl",
            "mediumImageUrl",
            "largeImageUrl",
            "similarArtist",
        ],
        "structuredLyrics" => &["line", "cueLine"],
        "cueLine" => &["cue"],
        _ => &[],
    };
    order
        .iter()
        .position(|candidate| *candidate == field)
        .unwrap_or(order.len())
}
fn scalar(v: &Value) -> String {
    match v {
        Value::String(v) => v.clone(),
        Value::Bool(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        _ => String::new(),
    }
}
fn xml_escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[derive(Debug)]
struct ApiFailure {
    code: i32,
    message: String,
}
impl ApiFailure {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl From<anyhow::Error> for ApiFailure {
    fn from(value: anyhow::Error) -> Self {
        Self::new(0, value.to_string())
    }
}
impl From<sea_orm::DbErr> for ApiFailure {
    fn from(value: sea_orm::DbErr) -> Self {
        Self::new(0, value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::to_bytes, http::Request};
    use tower::ServiceExt;

    use super::*;

    async fn test_state() -> AppState {
        test_state_with_settings(crate::config::Settings::default()).await
    }

    async fn test_state_with_settings(settings: crate::config::Settings) -> AppState {
        let db = crate::db::connect(&crate::config::DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        crate::db::migrate(&db).await.unwrap();
        let settings = Arc::new(settings);
        crate::db::bootstrap_admin(&db, &settings.admin, &settings.auth.jwt_secret)
            .await
            .unwrap();
        let providers = Arc::new(crate::providers::ProviderRegistry::new(settings.clone()));
        AppState::new(settings, db, providers)
    }

    async fn request_json(app: Router, uri: &str) -> Value {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn test_track() -> Track {
        Track {
            id: "track-1".into(),
            folder_id: "folder-1".into(),
            path: "/music/song.flac".into(),
            relative_path: "song.flac".into(),
            title: "Song".into(),
            artist_id: "artist-1".into(),
            artist_name: "Artist A, Artist B".into(),
            artists_json: serde_json::to_string(&vec![
                ArtistCredit {
                    id: "artist-1".into(),
                    name: "Artist A".into(),
                },
                ArtistCredit {
                    id: "artist-2".into(),
                    name: "Artist B".into(),
                },
            ])
            .unwrap(),
            album_id: None,
            album_name: "Album".into(),
            album_artist: String::new(),
            genre: String::new(),
            language: String::new(),
            year: 0,
            track_number: 0,
            disc_number: 0,
            duration: 180.0,
            bit_rate: 0,
            size: 0,
            suffix: "flac".into(),
            mimetype: "audio/flac".into(),
            lyrics: String::new(),
            comment: String::new(),
            cover_path: None,
            mtime: 0,
            fingerprint: String::new(),
            play_count: 0,
            needs_scrape: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    async fn insert_test_music_folder(state: &AppState) {
        music_folder_entity::ActiveModel {
            id: Set("folder-1".into()),
            name: Set("Music".into()),
            path: Set("/music".into()),
            enabled: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
    }

    async fn insert_test_catalog(state: &AppState) {
        insert_test_music_folder(state).await;
        artist_entity::ActiveModel {
            id: Set("artist-1".into()),
            name: Set("Artist A".into()),
            sort_name: Set("artist a".into()),
            cover_path: Set(None),
            album_count: Set(1),
            song_count: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        album_entity::ActiveModel {
            id: Set("album-1".into()),
            name: Set("Album".into()),
            artist_id: Set("artist-1".into()),
            artist_name: Set("Artist A".into()),
            year: Set(2026),
            genre: Set("Rock".into()),
            cover_path: Set(None),
            song_count: Set(1),
            duration: Set(180.0),
            created_at: Set("2026-01-01T00:00:00Z".into()),
        }
        .insert(&state.db)
        .await
        .unwrap();
        let mut track = test_track();
        track.album_id = Some("album-1".into());
        track.genre = "Rock".into();
        track.comment = "Liner notes".into();
        track.into_active_model().insert(&state.db).await.unwrap();
        track_artist_entity::ActiveModel {
            track_id: Set("track-1".into()),
            artist_id: Set("artist-1".into()),
            position: Set(0),
        }
        .insert(&state.db)
        .await
        .unwrap();
    }

    #[test]
    fn joins_multiple_cache_artists_with_hyphens() {
        assert_eq!(
            transcode_cache_artist_name(&test_track()),
            "Artist A-Artist B"
        );
    }

    #[tokio::test]
    async fn serves_an_existing_transcode_result_without_starting_the_media_engine() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("song.flac");
        tokio::fs::write(&source, b"source audio").await.unwrap();
        let server_settings = crate::config::Settings::default();
        let state = test_state_with_settings(server_settings).await;
        let cache_settings = transcode_cache::TranscodeCacheSettings {
            enabled: true,
            path: directory.path().join("transcodes"),
        };
        let mut track = test_track();
        track.path = source.to_string_lossy().into_owned();
        cache_settings.prepare().await.unwrap();
        transcode_cache::save(&state.db, &track.folder_id, &cache_settings)
            .await
            .unwrap();
        let cache_path = cache_settings
            .entry_path(transcode_cache::TranscodeCacheEntry {
                folder_id: &track.folder_id,
                artist: &transcode_cache_artist_name(&track),
                album: &track.album_name,
                title: &track.title,
                format: "mp3",
                bitrate: 128,
                offset: None,
            })
            .unwrap();
        tokio::fs::create_dir_all(cache_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&cache_path, b"cached audio")
            .await
            .unwrap();

        let response = transcode(&state, &track, "mp3", Some(128), None, Some("bytes=0-5"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        assert_eq!(&body[..], b"cached");
    }

    #[tokio::test]
    async fn treats_a_cache_older_than_its_source_as_stale() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("song.flac");
        let cache = directory.path().join("song.mp3");
        tokio::fs::write(&cache, b"old cache").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::fs::write(&source, b"new source").await.unwrap();

        assert!(!transcode_cache_is_fresh(&source, &cache).await.unwrap());

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::fs::write(&cache, b"new cache").await.unwrap();
        assert!(transcode_cache_is_fresh(&source, &cache).await.unwrap());
    }

    #[tokio::test]
    async fn writes_in_progress_transcodes_under_tmp() {
        let directory = tempfile::tempdir().unwrap();
        let final_path = directory.path().join("cache/result.mp3");
        let cache = PendingTranscodeCache::create(final_path).await.unwrap();
        let partial_path = cache.partial_path.as_ref().unwrap().clone();

        assert!(partial_path.starts_with("/tmp/mnest-transcodes"));
        assert!(partial_path.is_file());
        drop(cache);
        assert!(!partial_path.exists());
    }

    #[test]
    fn validates_internet_radio_fields_and_schemes() {
        let valid = HashMap::from([
            ("name".into(), "  Radio One  ".into()),
            ("streamUrl".into(), " https://radio.example/live ".into()),
            ("homepageUrl".into(), "https://radio.example/".into()),
            (
                "coverUrl".into(),
                " https://radio.example/cover.png ".into(),
            ),
        ]);
        assert_eq!(
            validated_radio_fields(&valid).unwrap(),
            (
                "Radio One".into(),
                "https://radio.example/live".into(),
                "https://radio.example/".into(),
                "https://radio.example/cover.png".into(),
            )
        );

        for stream_url in [
            "rtsp://radio.example/live",
            "mms://radio.example/live",
            "mmsh://radio.example/live",
            "mmst://radio.example:1755/live",
        ] {
            let fields = HashMap::from([
                ("name".into(), "Legacy Radio".into()),
                ("streamUrl".into(), stream_url.into()),
            ]);
            assert_eq!(
                validated_radio_fields(&fields).unwrap().1,
                stream_url.to_owned()
            );
        }

        for invalid in [
            HashMap::from([
                ("name".into(), "Radio".into()),
                ("streamUrl".into(), "javascript:alert(1)".into()),
            ]),
            HashMap::from([
                ("name".into(), "   ".into()),
                ("streamUrl".into(), "https://radio.example/live".into()),
            ]),
            HashMap::from([
                ("name".into(), "Radio".into()),
                ("streamUrl".into(), "https://radio.example/live".into()),
                ("homepageUrl".into(), "file:///tmp/radio".into()),
            ]),
            HashMap::from([
                ("name".into(), "Radio".into()),
                ("streamUrl".into(), "https://radio.example/live".into()),
                ("coverUrl".into(), "file:///tmp/cover.png".into()),
            ]),
            HashMap::from([
                ("name".into(), "Radio".into()),
                ("streamUrl".into(), "https://radio.example/live".into()),
                (
                    "coverUrl".into(),
                    "https://user:secret@radio.example/cover.png".into(),
                ),
            ]),
            HashMap::from([
                ("name".into(), "Radio".into()),
                (
                    "streamUrl".into(),
                    "https://music.example/api/internet_radio_stream.mp3?id=radio-1&token=test"
                        .into(),
                ),
            ]),
        ] {
            assert!(validated_radio_fields(&invalid).is_err());
        }
    }

    #[tokio::test]
    async fn internet_radio_crud_round_trips_fields_and_proxy_preference() {
        let state = test_state().await;
        let admin = user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();
        let create = HashMap::from([
            ("name".into(), "Radio One".into()),
            ("streamUrl".into(), "https://radio.example/live".into()),
            ("homepageUrl".into(), "https://radio.example/".into()),
            ("coverUrl".into(), "https://radio.example/cover.png".into()),
            ("proxy".into(), "true".into()),
        ]);
        create_radio(&state, &admin, &create).await.unwrap();
        let created = radio_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let direct = HashMap::from([
            ("name".into(), "Radio Direct".into()),
            ("streamUrl".into(), "https://direct.example/live".into()),
        ]);
        create_radio(&state, &admin, &direct).await.unwrap();
        let list_params =
            HashMap::from([("_mnest_base_url".into(), "https://music.example".into())]);
        let stations = radio_stations(&state, &list_params).await.unwrap();
        let stations = stations["internetRadioStations"]["internetRadioStation"]
            .as_array()
            .unwrap();
        let proxied_station = stations
            .iter()
            .find(|station| station["name"] == "Radio One")
            .unwrap();
        assert_eq!(proxied_station["homePageUrl"], "https://radio.example/");
        assert_eq!(proxied_station["coverArt"], format!("radio-{}", created.id));
        let proxy_url = proxied_station["streamUrl"].as_str().unwrap();
        assert!(proxy_url.starts_with("https://music.example/api/internet_radio_stream.mp3?id="));
        assert!(proxy_url.contains("&token="));
        let direct_stream_url = stations
            .iter()
            .find(|station| station["name"] == "Radio Direct")
            .unwrap()["streamUrl"]
            .as_str()
            .unwrap();
        assert_eq!(direct_stream_url, "https://direct.example/live");

        let pulse_list_params = HashMap::from([
            ("_mnest_base_url".into(), "https://music.example".into()),
            ("c".into(), "Pulse".into()),
        ]);
        let pulse_stations = radio_stations(&state, &pulse_list_params).await.unwrap();
        let pulse_stations = pulse_stations["internetRadioStations"]["internetRadioStation"]
            .as_array()
            .unwrap();
        let pulse_stream_url = pulse_stations
            .iter()
            .find(|station| station["name"] == "Radio One")
            .unwrap()["streamUrl"]
            .as_str()
            .unwrap();
        assert_eq!(pulse_stream_url, "https://radio.example/live");

        let update = HashMap::from([
            ("id".into(), created.id.clone()),
            ("name".into(), "Radio Two".into()),
            ("streamUrl".into(), "http://radio.example/aac".into()),
            ("homepageUrl".into(), String::new()),
            (
                "coverUrl".into(),
                "https://radio.example/new-cover.jpg".into(),
            ),
        ]);
        update_radio(&state, &admin, &update).await.unwrap();
        let updated = radio_entity::Entity::find_by_id(&created.id)
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.name, "Radio Two");
        assert_eq!(updated.stream_url, "http://radio.example/aac");
        assert!(updated.home_page_url.is_empty());
        assert_eq!(updated.cover_url, "https://radio.example/new-cover.jpg");
        assert!(
            internet_radio::proxy_enabled(&state.db, &created.id)
                .await
                .unwrap()
        );

        let error = json_endpoint(
            &state,
            &admin,
            &subsonic_access(&state, &admin).await.unwrap(),
            "deleteInternetRadioStation",
            &HashMap::from([("id".into(), created.id.clone())]),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, 50);
        assert!(
            radio_entity::Entity::find_by_id(&created.id)
                .one(&state.db)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn serves_radio_covers_from_the_disk_cache_with_etags() {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = crate::config::Settings::default();
        settings.cover_cache.enabled = true;
        settings.cover_cache.path = directory.path().join("covers");
        let state = test_state_with_settings(settings).await;
        let cover_url = "https://radio.invalid/cover.png";
        let cover = b"\x89PNG\r\n\x1a\ncached-radio-cover";
        radio_entity::ActiveModel {
            id: Set("station-cover".into()),
            name: Set("Covered Radio".into()),
            stream_url: Set("https://radio.example/live".into()),
            home_page_url: Set(String::new()),
            cover_url: Set(cover_url.into()),
        }
        .insert(&state.db)
        .await
        .unwrap();
        state
            .radio_covers
            .seed("station-cover", cover_url, cover)
            .await
            .unwrap();
        let user = user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        let params = HashMap::from([("id".into(), "radio-station-cover".into())]);

        let response = binary_endpoint(&state, &user, &access, "getCoverArt", &params, None)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        let etag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .as_ref(),
            cover
        );

        let response = binary_endpoint(&state, &user, &access, "getCoverArt", &params, Some(&etag))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn open_subsonic_radio_listing_uses_the_request_host_for_proxy_urls() {
        let state = test_state().await;
        let admin = user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();
        let (access, _) = crate::auth::issue_tokens(
            &admin,
            &state.settings.auth.jwt_secret,
            state.settings.auth.access_token_minutes,
            state.settings.auth.refresh_token_days,
        )
        .unwrap();
        radio_entity::ActiveModel {
            id: Set("proxied-radio".into()),
            name: Set("Proxied radio".into()),
            stream_url: Set("https://radio.example/live".into()),
            home_page_url: Set(String::new()),
            cover_url: Set(String::new()),
        }
        .insert(&state.db)
        .await
        .unwrap();
        internet_radio::set_proxy_enabled(&state.db, "proxied-radio", true)
            .await
            .unwrap();

        let response = router()
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/rest/getInternetRadioStations?f=json&v=1.16.1&c=test")
                    .header(header::HOST, "music.example:4535")
                    .header("x-forwarded-proto", "https")
                    .header(header::AUTHORIZATION, format!("Bearer {access}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let stream_url = body["subsonic-response"]["internetRadioStations"]["internetRadioStation"]
            [0]["streamUrl"]
            .as_str()
            .unwrap();

        assert!(stream_url.starts_with(
            "https://music.example:4535/api/internet_radio_stream.mp3?id=proxied-radio&token="
        ));
        assert!(!stream_url.contains("radio.example"));
    }

    #[test]
    fn renders_subsonic_attributes_and_children() {
        let value = wrapper(
            "ok",
            json!({"album":{"id":"1","artists":[{"id":"artist-1","name":"Artist"}],"song":[{"id":"2","isDir":false,"title":"A&B"}]}}),
            None,
        );
        let xml = xml_document(&value);
        assert!(xml.contains("xmlns=\"http://subsonic.org/restapi\""));
        assert!(xml.contains("version=\"1.16.1\""));
        assert!(xml.contains("<artists id=\"artist-1\" name=\"Artist\"></artists>"));
        assert!(xml.contains("<song id=\"2\" isDir=\"false\" title=\"A&amp;B\"></song>"));
    }

    #[test]
    fn renders_xml_text_nodes_and_schema_child_order() {
        let genres = value_to_xml(
            "genres",
            &json!({"genre":[{"value":"Rock & Roll","songCount":2,"albumCount":1}]}),
        );
        assert_eq!(
            genres,
            "<genres><genre albumCount=\"1\" songCount=\"2\">Rock &amp; Roll</genre></genres>"
        );

        let search = value_to_xml(
            "searchResult3",
            &json!({
                "album":[{"id":"album-1","name":"Album","songCount":1,"duration":1,"created":"2026-01-01T00:00:00Z"}],
                "artist":[{"id":"artist-1","name":"Artist","albumCount":1}],
                "song":[{"id":"song-1","isDir":false,"title":"Song"}]
            }),
        );
        let artist = search.find("<artist ").unwrap();
        let album = search.find("<album ").unwrap();
        let song = search.find("<song ").unwrap();
        assert!(artist < album && album < song);

        let info = value_to_xml(
            "albumInfo",
            &json!({"lastFmUrl":"https://example.test/album","notes":"Notes"}),
        );
        assert_eq!(
            info,
            "<albumInfo><notes>Notes</notes><lastFmUrl>https://example.test/album</lastFmUrl></albumInfo>"
        );

        let line = value_to_xml("line", &json!({"start":2000,"value":"A & B"}));
        assert_eq!(line, "<line start=\"2000\">A &amp; B</line>");
    }

    #[test]
    fn parses_standard_and_suffix_ranges() {
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
    }

    #[test]
    fn validates_transcode_offsets_before_starting_media_work() {
        assert_eq!(transcode_offset("1.25"), Some(Duration::from_millis(1_250)));
        for invalid in ["0", "-1", "NaN", "inf", "1e999", "not-a-number"] {
            assert_eq!(transcode_offset(invalid), None, "accepted {invalid}");
        }
    }

    #[test]
    fn cover_art_etags_track_the_source_version_and_requested_size() {
        let original = cover_art_etag("track-1", 123, None);

        assert!(original.starts_with("W/\""));
        assert!(original.ends_with('"'));
        assert_eq!(original, cover_art_etag("track-1", 123, None));
        assert_ne!(original, cover_art_etag("track-1", 124, None));
        assert_ne!(original, cover_art_etag("track-2", 123, None));
        assert_ne!(original, cover_art_etag("track-1", 123, Some(300)));
    }

    #[test]
    fn album_thumbnail_cache_keys_are_isolated_by_source_track() {
        assert_ne!(
            cover_art_cache_id("img-album-1", "track-private"),
            cover_art_cache_id("img-album-1", "track-visible")
        );
        assert_eq!(
            cover_art_cache_id("img-album-1", "track-visible"),
            cover_art_cache_id("img-album-1", "track-visible")
        );
    }

    #[test]
    fn matches_weak_and_strong_if_none_match_values() {
        let etag = cover_art_etag("track-1", 123, Some(300));
        let strong_etag = etag.strip_prefix("W/").unwrap();

        assert!(if_none_match_matches(&etag, &etag));
        assert!(if_none_match_matches(strong_etag, &etag));
        assert!(if_none_match_matches(
            &format!("\"unrelated\", {strong_etag}"),
            &etag
        ));
        assert!(if_none_match_matches("*", &etag));
        assert!(!if_none_match_matches("\"unrelated\"", &etag));
    }

    #[tokio::test]
    async fn matching_cover_art_etag_returns_not_modified_without_reading_the_file() {
        let state = test_state().await;
        insert_test_music_folder(&state).await;
        let mut track = test_track();
        track.mtime = 123;
        track.into_active_model().insert(&state.db).await.unwrap();
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let params = HashMap::from([
            ("id".to_owned(), "img-track-1".to_owned()),
            ("size".to_owned(), "300".to_owned()),
        ]);
        let etag = cover_art_etag("track-1", 123, Some(300));

        let response = binary_endpoint(
            &state,
            &user,
            &subsonic_access(&state, &user).await.unwrap(),
            "getCoverArt",
            &params,
            Some(&etag),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers().get(header::ETAG).unwrap(), &etag);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            crate::tags::ARTWORK_CACHE_CONTROL
        );
        assert!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cover_art_misses_are_remembered_without_reopening_the_source() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("no-cover.mp3");
        std::fs::write(&path, b"ID3\x04\x00\x00\x00\x00\x00\x0aTIT2 titleaudio").unwrap();
        let track = track_entity::Entity::find_by_id("track-1")
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut active = track.into_active_model();
        active.path = Set(path.to_string_lossy().into_owned());
        active.relative_path = Set("no-cover.mp3".into());
        active.suffix = Set("mp3".into());
        active.mimetype = Set("audio/mpeg".into());
        active.mtime = Set(123);
        active.update(&state.db).await.unwrap();
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        let params = HashMap::from([("id".to_owned(), "img-album-1".to_owned())]);

        let error = binary_endpoint(&state, &user, &access, "getCoverArt", &params, None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "cover art not found");
        let track = track_entity::Entity::find_by_id("track-1")
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let album = album_entity::Entity::find_by_id("album-1")
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(track.cover_path.as_deref(), Some(MISSING_ARTWORK_MARKER));
        assert_eq!(album.cover_path.as_deref(), Some(MISSING_ARTWORK_MARKER));
        assert!(track_json(&track, None).get("coverArt").is_none());
        assert!(album_json(&album).get("coverArt").is_none());

        std::fs::remove_file(path).unwrap();
        let error = binary_endpoint(&state, &user, &access, "getCoverArt", &params, None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "cover art not found");
    }

    #[tokio::test]
    async fn album_cover_uses_a_source_from_an_accessible_music_folder() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        music_folder_entity::ActiveModel {
            id: Set("folder-private".into()),
            name: Set("Private".into()),
            path: Set("/private".into()),
            enabled: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        track_entity::Entity::update_many()
            .col_expr(
                track_entity::Column::FolderId,
                Expr::value("folder-private"),
            )
            .col_expr(track_entity::Column::Mtime, Expr::value(123))
            .filter(track_entity::Column::Id.eq("track-1"))
            .exec(&state.db)
            .await
            .unwrap();
        let mut visible = test_track();
        visible.id = "track-visible".into();
        visible.path = "/music/visible.flac".into();
        visible.relative_path = "visible.flac".into();
        visible.album_id = Some("album-1".into());
        visible.track_number = 1;
        visible.mtime = 456;
        visible.into_active_model().insert(&state.db).await.unwrap();

        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut access = subsonic_access(&state, &user).await.unwrap();
        access.folder_ids = Some(HashSet::from(["folder-1".to_owned()]));
        let params = HashMap::from([("id".to_owned(), "img-album-1".to_owned())]);
        let etag = cover_art_etag("track-visible", 456, None);

        let response = binary_endpoint(&state, &user, &access, "getCoverArt", &params, Some(&etag))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers().get(header::ETAG).unwrap(), &etag);
    }

    #[tokio::test]
    async fn an_empty_music_folder_acl_exposes_no_tracks() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut access = subsonic_access(&state, &user).await.unwrap();
        access.folder_ids = Some(HashSet::new());

        assert!(
            accessible_tracks(&access)
                .one(&state.db)
                .await
                .unwrap()
                .is_none()
        );
        assert!(accessible_track(&state, &access, "track-1").await.is_err());
    }

    #[tokio::test]
    async fn disabled_music_folders_are_hidden_and_use_the_same_error_as_missing_tracks() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        let folder = music_folder_entity::Entity::find_by_id("folder-1")
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut active = folder.into_active_model();
        active.enabled = Set(0);
        active.update(&state.db).await.unwrap();
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();

        assert!(accessible_track(&state, &access, "track-1").await.is_err());
        assert!(
            library_artists(&state, &access, None)
                .await
                .unwrap()
                .is_empty()
        );
        let hidden_error = binary_endpoint(
            &state,
            &user,
            &access,
            "stream",
            &HashMap::from([("id".into(), "track-1".into())]),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        let missing_error = binary_endpoint(
            &state,
            &user,
            &access,
            "stream",
            &HashMap::from([("id".into(), "missing".into())]),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(hidden_error, missing_error);
    }

    #[test]
    fn derives_subsonic_artist_fields_from_structured_artists() {
        let value = track_json(&test_track(), None);
        assert_eq!(value["artist"], "Artist A; Artist B");
        assert_eq!(value["artistId"], "artist-1");
        assert_eq!(value["artists"][0]["name"], "Artist A");
        assert_eq!(value["artists"][1]["name"], "Artist B");
        assert!(value.get("parent").is_none());
        assert!(value.get("albumId").is_none());
        assert_eq!(value["mediaType"], "song");
        assert_eq!(value["isVideo"], false);
        assert_eq!(value["bookmarkPosition"], 0);

        let xml = value_to_xml("song", &value);
        assert!(xml.contains(" displayArtist=\"Artist A; Artist B\""));
        assert!(xml.contains(" mediaType=\"song\""));
        assert!(xml.contains("<artists id=\"artist-1\" name=\"Artist A\"></artists>"));
        assert!(xml.contains("<artists id=\"artist-2\" name=\"Artist B\"></artists>"));
    }

    #[test]
    fn omits_fields_not_defined_on_album_and_artist_id3() {
        let artist = Artist {
            id: "artist-1".into(),
            name: "Artist".into(),
            sort_name: "artist".into(),
            cover_path: None,
            album_count: 2,
            song_count: 3,
        };
        let artist = artist_json(&artist, Some("img-album-1"));
        assert!(artist.get("songCount").is_none());
        assert_eq!(artist["sortName"], "artist");
        assert_eq!(artist["coverArt"], "img-album-1");
        let legacy_artist = legacy_artist_json(&Artist {
            id: "artist-1".into(),
            name: "Artist".into(),
            sort_name: "artist".into(),
            cover_path: None,
            album_count: 2,
            song_count: 3,
        });
        assert!(legacy_artist.get("albumCount").is_none());

        let mut album = Album {
            id: "album-1".into(),
            name: "Album".into(),
            artist_id: "artist-1".into(),
            artist_name: "Artist A; Artist B".into(),
            year: 2026,
            genre: "Pop".into(),
            cover_path: None,
            song_count: 3,
            duration: 180.0,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        let value = album_json(&album);
        assert!(value.get("isDir").is_none());
        assert!(value.get("artists").is_none());

        album.artist_name = "Artist A".into();
        assert_eq!(album_json(&album)["artists"][0]["name"], "Artist A");
        assert_eq!(album_json(&album)["coverArt"], "img-album-1");
        assert_eq!(canonical_track_cover_art("track-1", None), "img-track-1");
        assert_eq!(
            canonical_track_cover_art("track-1", Some("album-1")),
            "img-album-1"
        );
    }

    #[test]
    fn parses_repeated_form_parameters_without_losing_values() {
        let params = decode_params(b"id=one&id=two&id=three").unwrap();
        assert_eq!(multi(&params, "id"), ["one", "two", "three"]);

        let empty_query = decode_params(b"query=").unwrap();
        assert_eq!(present(&empty_query, "query").unwrap(), "");
        assert!(required(&empty_query, "query").is_err());

        let symfonium_query = decode_params(b"query=%22%22").unwrap();
        assert_eq!(symfonium_query["query"], "\"\"");
        assert_eq!(normalize_search_query(&symfonium_query["query"]), "");
        assert_eq!(normalize_search_query("\"Muse\"*"), "Muse");
    }

    #[tokio::test]
    async fn symfonium_empty_search_sentinel_returns_library_items() {
        let state = test_state().await;
        music_folder_entity::ActiveModel {
            id: Set("folder-1".into()),
            name: Set("Music".into()),
            path: Set("/music".into()),
            enabled: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        artist_entity::ActiveModel {
            id: Set("artist-1".into()),
            name: Set("Artist A".into()),
            sort_name: Set("artist a".into()),
            cover_path: Set(None),
            album_count: Set(0),
            song_count: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        test_track()
            .into_active_model()
            .insert(&state.db)
            .await
            .unwrap();

        let params = HashMap::from([
            ("query".into(), "\"\"".into()),
            ("artistCount".into(), "0".into()),
            ("albumCount".into(), "0".into()),
            ("songCount".into(), "1000".into()),
        ]);
        let access = subsonic_access(
            &state,
            &user_entity::Entity::find()
                .one(&state.db)
                .await
                .unwrap()
                .unwrap(),
        )
        .await
        .unwrap();
        let result = search(&state, &access, "search3", &params).await.unwrap();
        assert_eq!(result["searchResult3"]["song"].as_array().unwrap().len(), 1);

        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let user = get_user(&state, &user, &user.username).await.unwrap();
        assert_eq!(user["user"]["folder"][0], folder_api_id("folder-1"));
    }

    #[tokio::test]
    async fn artist_covers_reuse_the_representative_album_image_id() {
        let state = test_state().await;
        music_folder_entity::ActiveModel {
            id: Set("folder-1".into()),
            name: Set("Music".into()),
            path: Set("/music".into()),
            enabled: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        for (id, name) in [("artist-1", "Artist A"), ("artist-2", "Artist B")] {
            artist_entity::ActiveModel {
                id: Set(id.into()),
                name: Set(name.into()),
                sort_name: Set(name.to_lowercase()),
                cover_path: Set(None),
                album_count: Set(1),
                song_count: Set(1),
            }
            .insert(&state.db)
            .await
            .unwrap();
        }
        album_entity::ActiveModel {
            id: Set("album-1".into()),
            name: Set("Album".into()),
            artist_id: Set("artist-1".into()),
            artist_name: Set("Artist A; Artist B".into()),
            year: Set(2026),
            genre: Set(String::new()),
            cover_path: Set(None),
            song_count: Set(1),
            duration: Set(180.0),
            created_at: Set("2026-01-01T00:00:00Z".into()),
        }
        .insert(&state.db)
        .await
        .unwrap();
        let mut track = test_track();
        track.album_id = Some("album-1".into());
        track.into_active_model().insert(&state.db).await.unwrap();
        for (id, position) in [("artist-1", 0), ("artist-2", 1)] {
            track_artist_entity::ActiveModel {
                track_id: Set("track-1".into()),
                artist_id: Set(id.into()),
                position: Set(position),
            }
            .insert(&state.db)
            .await
            .unwrap();
        }

        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        let covers = artist_cover_art_map(
            &state,
            &access,
            &["artist-1".into(), "artist-2".into()],
            None,
        )
        .await
        .unwrap();
        assert_eq!(covers["artist-1"], "img-album-1");
        assert_eq!(covers["artist-2"], "img-album-1");

        let response = artists(&state, &access, &HashMap::new()).await.unwrap();
        let returned = response["artists"]["index"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|index| index["artist"].as_array().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(returned.len(), 2);
        assert!(
            returned
                .iter()
                .all(|artist| artist["coverArt"] == "img-album-1")
        );
    }

    #[tokio::test]
    async fn formal_scrobbles_update_and_return_the_authenticated_users_play_count() {
        let state = test_state().await;
        insert_test_music_folder(&state).await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        test_track()
            .into_active_model()
            .insert(&state.db)
            .await
            .unwrap();

        scrobble(
            &state,
            &user,
            &access,
            &HashMap::from([
                ("id".into(), "track-1".into()),
                ("submission".into(), "false".into()),
            ]),
        )
        .await
        .unwrap();
        assert!(
            user_track_stat_entity::Entity::find()
                .one(&state.db)
                .await
                .unwrap()
                .is_none()
        );

        scrobble(
            &state,
            &user,
            &access,
            &HashMap::from([("id".into(), "track-1,track-1".into())]),
        )
        .await
        .unwrap();
        let stats = user_track_stat_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.user_id, user.id);
        assert_eq!(stats.play_count, 2);

        let mut track = track_entity::Entity::find_by_id("track-1")
            .one(&state.db)
            .await
            .unwrap()
            .unwrap()
            .into_active_model();
        track.play_count = Set(99);
        track.update(&state.db).await.unwrap();
        let response = json_endpoint(
            &state,
            &user,
            &access,
            "getSong",
            &HashMap::from([("id".into(), "track-1".into())]),
        )
        .await
        .unwrap();

        assert_eq!(response["song"]["playCount"], 2);
    }

    #[test]
    fn validates_protocol_versions_and_authentication_conflicts() {
        let valid = HashMap::from([
            ("v".into(), "1.16.1".into()),
            ("c".into(), "test".into()),
            ("u".into(), "user".into()),
            ("p".into(), "password".into()),
        ]);
        assert!(validate_protocol_request(&valid).is_ok());
        assert_eq!(validate_authentication_request(&valid).unwrap(), 40);

        let mut conflict = valid;
        conflict.insert("t".into(), "token".into());
        conflict.insert("s".into(), "salt".into());
        assert_eq!(
            validate_authentication_request(&conflict).unwrap_err().code,
            43
        );
    }

    #[test]
    fn parses_synced_lrc_timestamps_in_milliseconds() {
        let lines = parse_lrc_line("[01:02.50]Line");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].start, 62_500);
        assert_eq!(lines[0].value, "Line");
        assert!(lines[0].cues.is_empty());
        assert!(parse_lrc_line("[ar:Artist]").is_empty());
    }

    #[test]
    fn expands_and_sorts_repeated_lrc_timestamps() {
        let lines = parse_lrc(
            "[01:56.29][00:16.47]万家穿针乞巧心系檀郎\n\
             [00:31.94]\n\
             [00:01,5]作曲 : 银临",
        );
        assert_eq!(
            lines
                .into_iter()
                .map(|line| (line.start, line.value))
                .collect::<Vec<_>>(),
            vec![
                (1_500, "作曲 : 银临".to_owned()),
                (16_470, "万家穿针乞巧心系檀郎".to_owned()),
                (31_940, String::new()),
                (116_290, "万家穿针乞巧心系檀郎".to_owned()),
            ]
        );
    }

    #[test]
    fn parses_enhanced_lrc_with_utf8_byte_offsets_and_complete_ends() {
        let lines = parse_lrc(
            "[00:01.000]<00:01.000>眼<00:01.500>睛<00:02.000>\n\
             [00:02.000]<00:02.000>done<00:03.000>",
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].value, "眼睛");
        assert_eq!(lines[0].cues.len(), 2);
        assert_eq!(lines[0].cues[0].byte_start, 0);
        assert_eq!(lines[0].cues[0].byte_end, 2);
        assert_eq!(lines[0].cues[1].byte_start, 3);
        assert_eq!(lines[0].cues[1].byte_end, 5);

        let cue_lines = enhanced_lrc_cue_lines(&lines, 180.0);
        assert_eq!(cue_lines[0]["start"], 1_000);
        assert_eq!(cue_lines[0]["end"], 2_000);
        assert_eq!(cue_lines[0]["value"], "眼睛");
        assert_eq!(cue_lines[0]["cue"][0]["start"], 1_000);
        assert_eq!(cue_lines[0]["cue"][0]["end"], 1_500);
        assert_eq!(cue_lines[0]["cue"][0]["byteStart"], 0);
        assert_eq!(cue_lines[0]["cue"][0]["byteEnd"], 2);
        assert_eq!(cue_lines[0]["cue"][1]["end"], 2_000);
    }

    #[test]
    fn enhanced_lrc_omits_all_cue_ends_when_the_final_boundary_is_unknown() {
        let lines = parse_lrc("[00:01.000]<00:01.000>A<00:02.000>B");
        let cue_lines = enhanced_lrc_cue_lines(&lines, 0.0);
        assert_eq!(cue_lines.len(), 1);
        assert!(cue_lines[0].get("end").is_none());
        assert!(cue_lines[0]["cue"][0].get("end").is_none());
        assert!(cue_lines[0]["cue"][1].get("end").is_none());
    }

    #[test]
    fn enhanced_lrc_drops_invalid_non_monotonic_cues_without_losing_text() {
        let lines = parse_lrc("[00:01.000]<00:02.000>later<00:01.500>earlier");
        assert_eq!(lines[0].value, "laterearlier");
        assert!(enhanced_lrc_cue_lines(&lines, 180.0).is_empty());

        let invalid_marker = parse_lrc("[00:01.000]<not-a-time>Text");
        assert_eq!(invalid_marker[0].value, "<not-a-time>Text");
        assert!(invalid_marker[0].cues.is_empty());
    }

    #[tokio::test]
    async fn song_lyrics_v2_is_opt_in_and_keeps_the_v1_fallback() {
        let state = test_state().await;
        insert_test_music_folder(&state).await;
        let mut track = test_track();
        track.lyrics = "[00:01.000]<00:01.000>眼<00:01.500>睛<00:02.000>".into();
        track.into_active_model().insert(&state.db).await.unwrap();
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let api_key = crate::auth::reveal_subsonic_api_key(
            &user.subsonic_token,
            &state.settings.auth.jwt_secret,
            &user.id,
        )
        .unwrap();

        let base =
            format!("/rest/getLyricsBySongId?apiKey={api_key}&v=1.16.1&c=test&f=json&id=track-1");
        let v1 = request_json(router().with_state(state.clone()), &base).await;
        let v1 = &v1["subsonic-response"]["lyricsList"]["structuredLyrics"][0];
        assert!(v1.get("kind").is_none());
        assert!(v1.get("cueLine").is_none());
        assert_eq!(v1["line"][0]["value"], "眼睛");

        let v2 = request_json(router().with_state(state), &format!("{base}&enhanced=true")).await;
        let v2 = &v2["subsonic-response"]["lyricsList"]["structuredLyrics"][0];
        assert_eq!(v2["kind"], "main");
        assert_eq!(v2["cueLine"][0]["index"], 0);
        assert_eq!(v2["cueLine"][0]["cue"][1]["value"], "睛");
        assert!(v2.get("agents").is_none());

        let xml = value_to_xml("structuredLyrics", v2);
        assert!(xml.find("<line ").unwrap() < xml.find("<cueLine ").unwrap());
        assert!(xml.contains("kind=\"main\""));
        assert!(xml.contains("value=\"眼睛\""));
        assert!(xml.contains(">眼</cue>"));
    }

    #[test]
    fn exposes_stable_integer_music_folder_ids() {
        let id = folder_api_id("88133187-fa8c-461d-b00c-631703004590");
        assert_eq!(id, folder_api_id("88133187-fa8c-461d-b00c-631703004590"));
        assert!((1..=i32::MAX).contains(&id));
        assert_eq!(folder_api_id("42"), 42);
    }

    #[tokio::test]
    async fn top_songs_supports_artist_name_and_advertised_artist_id() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        for params in [
            HashMap::from([("artist".into(), "Artist A".into())]),
            HashMap::from([("id".into(), "artist-1".into())]),
        ] {
            let response = json_endpoint(&state, &user, &access, "getTopSongs", &params)
                .await
                .unwrap();
            assert_eq!(response["topSongs"]["song"][0]["id"], "track-1");
        }
    }

    #[tokio::test]
    async fn index_based_queue_round_trips_a_duplicate_current_item() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        save_play_queue(
            &state,
            &user,
            &access,
            &HashMap::from([
                ("id".into(), "track-1,track-1,track-1".into()),
                ("currentIndex".into(), "2".into()),
                ("position".into(), "1234".into()),
            ]),
            true,
        )
        .await
        .unwrap();

        let response = get_play_queue(&state, &user, &access, true).await.unwrap();
        assert_eq!(response["playQueueByIndex"]["currentIndex"], 2);
        assert_eq!(
            response["playQueueByIndex"]["entry"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let stored = play_queue_entity::Entity::find_by_id(&user.id)
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.current_index, Some(2));

        let error = save_play_queue(
            &state,
            &user,
            &access,
            &HashMap::from([("currentIndex".into(), "0".into())]),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, 10);
        save_play_queue(&state, &user, &access, &HashMap::new(), true)
            .await
            .unwrap();
        let response = get_play_queue(&state, &user, &access, true).await.unwrap();
        assert!(response["playQueueByIndex"].get("currentIndex").is_none());
        assert!(
            response["playQueueByIndex"]["entry"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn play_queue_remaps_or_replaces_a_current_item_hidden_by_folder_acl() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        music_folder_entity::ActiveModel {
            id: Set("folder-private".into()),
            name: Set("Private".into()),
            path: Set("/private".into()),
            enabled: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        let mut private = test_track();
        private.id = "track-private".into();
        private.folder_id = "folder-private".into();
        private.path = "/private/song.flac".into();
        private.relative_path = "private-song.flac".into();
        private.into_active_model().insert(&state.db).await.unwrap();
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let unrestricted = subsonic_access(&state, &user).await.unwrap();
        let mut restricted = unrestricted.clone();
        restricted.folder_ids = Some(HashSet::from(["folder-1".to_owned()]));

        save_play_queue(
            &state,
            &user,
            &unrestricted,
            &HashMap::from([
                ("id".into(), "track-private,track-1".into()),
                ("currentIndex".into(), "1".into()),
            ]),
            true,
        )
        .await
        .unwrap();
        let response = get_play_queue(&state, &user, &restricted, true)
            .await
            .unwrap();
        assert_eq!(response["playQueueByIndex"]["currentIndex"], 0);
        assert_eq!(response["playQueueByIndex"]["entry"][0]["id"], "track-1");
        assert_eq!(
            response["playQueueByIndex"]["entry"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        save_play_queue(
            &state,
            &user,
            &unrestricted,
            &HashMap::from([
                ("id".into(), "track-private,track-1".into()),
                ("currentIndex".into(), "0".into()),
            ]),
            true,
        )
        .await
        .unwrap();
        let response = get_play_queue(&state, &user, &restricted, false)
            .await
            .unwrap();
        assert_eq!(response["playQueue"]["current"], "track-1");
    }

    #[tokio::test]
    async fn users_keep_passwords_optional_and_expose_configured_roles_and_folders() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        music_folder_entity::ActiveModel {
            id: Set("folder-2".into()),
            name: Set("Private".into()),
            path: Set("/private".into()),
            enabled: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        let admin = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        create_user(
            &state,
            &admin,
            &HashMap::from([
                ("username".into(), "listener".into()),
                ("password".into(), "listener-password".into()),
                ("email".into(), "listener@example.test".into()),
                ("streamRole".into(), "false".into()),
                ("downloadRole".into(), "true".into()),
                (
                    "musicFolderId".into(),
                    folder_api_id("folder-1").to_string(),
                ),
            ]),
        )
        .await
        .unwrap();
        let before = user_by_name(&state.db, "listener").await.unwrap().unwrap();
        update_user(
            &state,
            &admin,
            &HashMap::from([
                ("username".into(), "listener".into()),
                ("commentRole".into(), "true".into()),
            ]),
        )
        .await
        .unwrap();
        let after = user_by_name(&state.db, "listener").await.unwrap().unwrap();
        assert_eq!(after.password_hash, before.password_hash);
        let response = get_user(&state, &admin, "listener").await.unwrap();
        assert_eq!(response["user"]["streamRole"], false);
        assert_eq!(response["user"]["downloadRole"], true);
        assert_eq!(response["user"]["commentRole"], true);
        assert_eq!(
            response["user"]["folder"].as_array().unwrap(),
            &[json!(folder_api_id("folder-1"))]
        );
        let access = subsonic_access(&state, &after).await.unwrap();
        assert!(accessible_track(&state, &access, "track-1").await.is_ok());
        assert!(!access.allows_folder("folder-2"));
        assert!(
            access_from_params(
                &state,
                Some(access),
                &HashMap::from([("ldapAuthenticated".into(), "true".into())]),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn folder_acl_scopes_artist_and_album_aggregate_metadata() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        music_folder_entity::ActiveModel {
            id: Set("folder-private".into()),
            name: Set("Private".into()),
            path: Set("/private".into()),
            enabled: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        album_entity::ActiveModel {
            id: Set("album-private".into()),
            name: Set("Private Album".into()),
            artist_id: Set("artist-1".into()),
            artist_name: Set("Artist A".into()),
            year: Set(2026),
            genre: Set("Rock".into()),
            cover_path: Set(None),
            song_count: Set(1),
            duration: Set(120.0),
            created_at: Set("2026-01-02T00:00:00Z".into()),
        }
        .insert(&state.db)
        .await
        .unwrap();
        for (id, album_id, relative_path, duration) in [
            ("track-private-shared", "album-1", "shared.flac", 120.0),
            (
                "track-private-album",
                "album-private",
                "private.flac",
                120.0,
            ),
        ] {
            let mut track = test_track();
            track.id = id.into();
            track.folder_id = "folder-private".into();
            track.path = format!("/private/{relative_path}");
            track.relative_path = relative_path.into();
            track.album_id = Some(album_id.into());
            track.album_name = if album_id == "album-1" {
                "Album".into()
            } else {
                "Private Album".into()
            };
            track.duration = duration;
            track.into_active_model().insert(&state.db).await.unwrap();
            track_artist_entity::ActiveModel {
                track_id: Set(id.into()),
                artist_id: Set("artist-1".into()),
                position: Set(0),
            }
            .insert(&state.db)
            .await
            .unwrap();
        }
        artist_entity::Entity::update_many()
            .col_expr(artist_entity::Column::AlbumCount, Expr::value(2))
            .col_expr(artist_entity::Column::SongCount, Expr::value(3))
            .filter(artist_entity::Column::Id.eq("artist-1"))
            .exec(&state.db)
            .await
            .unwrap();
        album_entity::Entity::update_many()
            .col_expr(album_entity::Column::SongCount, Expr::value(2))
            .col_expr(album_entity::Column::Duration, Expr::value(300.0))
            .filter(album_entity::Column::Id.eq("album-1"))
            .exec(&state.db)
            .await
            .unwrap();
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let mut access = subsonic_access(&state, &user).await.unwrap();
        access.folder_ids = Some(HashSet::from(["folder-1".to_owned()]));

        let response = get_artist(&state, &access, "artist-1").await.unwrap();
        assert_eq!(response["artist"]["albumCount"], 1);
        assert_eq!(response["artist"]["album"].as_array().unwrap().len(), 1);
        assert_eq!(response["artist"]["album"][0]["songCount"], 1);
        assert_eq!(response["artist"]["album"][0]["duration"], 180);
        let info = artist_info(
            &state,
            &access,
            "getArtistInfo2",
            &HashMap::from([("id".into(), "artist-1".into())]),
        )
        .await
        .unwrap();
        assert_eq!(
            info["artistInfo2"]["biography"],
            "Artist A · 1 albums · 1 songs"
        );
    }

    #[tokio::test]
    async fn album_shares_and_legacy_directory_stars_use_catalog_item_types() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        favorite(
            &state,
            &user,
            &access,
            &HashMap::from([("id".into(), "album-1,artist-1".into())]),
            true,
        )
        .await
        .unwrap();
        let favorites = favorite_entity::Entity::find()
            .filter(favorite_entity::Column::UserId.eq(&user.id))
            .all(&state.db)
            .await
            .unwrap();
        assert!(
            favorites
                .iter()
                .any(|favorite| favorite.item_type == "album")
        );
        assert!(
            favorites
                .iter()
                .any(|favorite| favorite.item_type == "artist")
        );

        let response = create_share(
            &state,
            &user,
            &access,
            &HashMap::from([("id".into(), "album-1".into())]),
        )
        .await
        .unwrap();
        assert_eq!(response["shares"]["share"][0]["entry"][0]["id"], "track-1");
        let share_id = response["shares"]["share"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut revoked_access = access.clone();
        revoked_access.share_role = false;
        json_endpoint(
            &state,
            &user,
            &revoked_access,
            "deleteShare",
            &HashMap::from([("id".into(), share_id)]),
        )
        .await
        .unwrap();
        assert!(
            share_entity::Entity::find()
                .one(&state.db)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn catalog_mutation_batch_limit_is_bounded_and_sqlite_safe() {
        let state = test_state().await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        let at_limit = (0..MAX_CATALOG_MUTATION_ITEMS)
            .map(|index| format!("missing-{index}"))
            .collect::<Vec<_>>();
        let error = validate_share_ids(&state, &access, &at_limit)
            .await
            .unwrap_err();
        assert_eq!(error.code, 70);

        let over_limit = (0..=MAX_CATALOG_MUTATION_ITEMS)
            .map(|index| format!("missing-{index}"))
            .collect::<Vec<_>>();
        let error = validate_share_ids(&state, &access, &over_limit)
            .await
            .unwrap_err();
        assert_eq!(error.code, 10);
    }

    #[tokio::test]
    async fn playback_reports_drive_now_playing_and_scrobble_once() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        report_playback(
            &state,
            &user,
            &access,
            &HashMap::from([
                ("mediaId".into(), "track-1".into()),
                ("mediaType".into(), "song".into()),
                ("positionMs".into(), "100000".into()),
                ("state".into(), "playing".into()),
                ("ignoreScrobble".into(), "true".into()),
                ("c".into(), "test-player".into()),
            ]),
        )
        .await
        .unwrap();
        assert!(
            user_track_stat_entity::Entity::find()
                .one(&state.db)
                .await
                .unwrap()
                .is_none()
        );
        let current = now_playing(&state, &access).await.unwrap();
        assert_eq!(current["nowPlaying"]["entry"][0]["id"], "track-1");
        assert_eq!(current["nowPlaying"]["entry"][0]["state"], "playing");

        for (state_name, position) in [
            ("starting", "0"),
            ("playing", "100000"),
            ("paused", "110000"),
        ] {
            report_playback(
                &state,
                &user,
                &access,
                &HashMap::from([
                    ("mediaId".into(), "track-1".into()),
                    ("mediaType".into(), "song".into()),
                    ("positionMs".into(), position.into()),
                    ("state".into(), state_name.into()),
                ]),
            )
            .await
            .unwrap();
        }
        let stats = user_track_stat_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.play_count, 1);
    }

    #[tokio::test]
    async fn concurrent_playback_reports_claim_a_single_scrobble() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        let params = HashMap::from([
            ("mediaId".into(), "track-1".into()),
            ("mediaType".into(), "song".into()),
            ("positionMs".into(), "100000".into()),
            ("state".into(), "playing".into()),
        ]);

        let (first, second) = tokio::join!(
            report_playback(&state, &user, &access, &params),
            report_playback(&state, &user, &access, &params),
        );
        first.unwrap();
        second.unwrap();

        let stats = user_track_stat_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.play_count, 1);
        assert_eq!(
            scrobble_entity::Entity::find()
                .filter(scrobble_entity::Column::Submission.eq(1))
                .all(&state.db)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            track_entity::Entity::find_by_id("track-1")
                .one(&state.db)
                .await
                .unwrap()
                .unwrap()
                .play_count,
            1
        );
    }

    #[tokio::test]
    async fn local_artist_and_album_info_validate_ids_and_return_metadata() {
        let state = test_state().await;
        insert_test_catalog(&state).await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        let album = album_info(
            &state,
            &access,
            &HashMap::from([("id".into(), "track-1".into())]),
        )
        .await
        .unwrap();
        assert_eq!(album["albumInfo"]["notes"], "Liner notes");
        let artist = artist_info(
            &state,
            &access,
            "getArtistInfo2",
            &HashMap::from([("id".into(), "track-1".into())]),
        )
        .await
        .unwrap();
        assert!(
            artist["artistInfo2"]["biography"]
                .as_str()
                .unwrap()
                .contains("Artist A")
        );
        assert!(
            album_info(
                &state,
                &access,
                &HashMap::from([("id".into(), "missing".into())]),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn exposes_extensions_without_authentication() {
        let state = test_state().await;
        let response = request_json(
            router().with_state(state.clone()),
            "/rest/getOpenSubsonicExtensions?f=json",
        )
        .await;
        let response = &response["subsonic-response"];
        assert_eq!(response["status"], "ok");
        let names = response["openSubsonicExtensions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|extension| extension["name"].as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"apiKeyAuthentication"));
        assert!(names.contains(&"indexBasedQueue"));
        assert!(names.contains(&"playbackReport"));
        assert!(names.contains(&"topSongsByArtistId"));
        assert!(!names.contains(&"mnestRadioRecognition"));
        let song_lyrics = response["openSubsonicExtensions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|extension| extension["name"] == "songLyrics")
            .unwrap();
        assert_eq!(song_lyrics["versions"], json!([1, 2]));

        let now = chrono::Utc::now().to_rfc3339();
        download_source_entity::ActiveModel {
            id: Set("netease-recognition".into()),
            kind: Set("netease".into()),
            name: Set("Netease".into()),
            base_url: Set("https://netease.example.test".into()),
            username: Set(String::new()),
            password: Set(String::new()),
            cookie: Set(String::new()),
            account_name: Set(String::new()),
            enabled: Set(1),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(&state.db)
        .await
        .unwrap();
        let configured = request_json(
            router().with_state(state),
            "/rest/getOpenSubsonicExtensions?f=json",
        )
        .await;
        let configured_names = configured["subsonic-response"]["openSubsonicExtensions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|extension| extension["name"].as_str())
            .collect::<Vec<_>>();
        assert!(configured_names.contains(&"mnestRadioRecognition"));
    }

    #[tokio::test]
    async fn authenticates_api_keys_without_username() {
        let state = test_state().await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let api_key = crate::auth::reveal_subsonic_api_key(
            &user.subsonic_token,
            &state.settings.auth.jwt_secret,
            &user.id,
        )
        .unwrap();
        let uri = format!("/rest/ping?apiKey={}&v=1.16.1&c=test&f=json", api_key);
        let response = request_json(router().with_state(state), &uri).await;
        assert_eq!(response["subsonic-response"]["status"], "ok");
    }

    #[tokio::test]
    async fn uses_the_standard_album_info_response_key_for_both_endpoints() {
        let state = test_state().await;
        insert_test_music_folder(&state).await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        album_entity::ActiveModel {
            id: Set("album-1".into()),
            name: Set("Album".into()),
            artist_id: Set("artist-1".into()),
            artist_name: Set("Artist A".into()),
            year: Set(2026),
            genre: Set("Rock".into()),
            cover_path: Set(None),
            song_count: Set(1),
            duration: Set(180.0),
            created_at: Set(Utc::now().to_rfc3339()),
        }
        .insert(&state.db)
        .await
        .unwrap();
        let mut track = test_track();
        track.album_id = Some("album-1".into());
        track.into_active_model().insert(&state.db).await.unwrap();
        let access = subsonic_access(&state, &user).await.unwrap();
        let value = json_endpoint(
            &state,
            &user,
            &access,
            "getAlbumInfo2",
            &HashMap::from([("id".into(), "album-1".into())]),
        )
        .await
        .unwrap();
        assert!(value.get("albumInfo").is_some());
        assert!(value.get("albumInfo2").is_none());
    }

    #[tokio::test]
    async fn rejects_scan_requests_from_non_administrators() {
        let state = test_state().await;
        let mut user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        user.role = "user".into();

        let error = start_scan(&state, &user).await.unwrap_err();

        assert_eq!(error.code, 50);
        assert!(
            job_entity::Entity::find()
                .one(&state.db)
                .await
                .unwrap()
                .is_none()
        );
    }
}
