use std::{
    collections::{HashMap, HashSet},
    io::SeekFrom,
    path::PathBuf,
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
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, FromQueryResult, IntoActiveModel,
    QueryFilter, QueryOrder, QuerySelect, QueryTrait, Set, TransactionTrait,
    sea_query::{Expr, Order},
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    process::Command,
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};
use uuid::Uuid;

use crate::{
    AppState,
    artist_credit::{ArtistCredit, parse_artist_names},
    auth::{authenticate_subsonic, encrypt_subsonic_password, user_by_name, web_user_from_headers},
    db,
    entities::{
        album as album_entity, artist as artist_entity, bookmark as bookmark_entity,
        favorite as favorite_entity, internet_radio_station as radio_entity, job as job_entity,
        music_folder as music_folder_entity, play_queue as play_queue_entity,
        playlist as playlist_entity, playlist_track as playlist_track_entity,
        rating as rating_entity, scrobble as scrobble_entity, share as share_entity,
        track as track_entity, track_artist as track_artist_entity, user as user_entity,
    },
    internet_radio,
    jobs::{self, ScanPayload},
    lastfm,
    models::{Album, Artist, MusicFolder, Track, User},
    user_preferences,
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

const API_VERSION: &str = "1.16.1";
const XML_NAMESPACE: &str = "http://subsonic.org/restapi";
const MAX_COLLECTION_ITEMS: usize = 10_000;
const MAX_SCROBBLE_BATCH: usize = 1_000;

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
    let ids: Vec<String> = serde_json::from_str(&share.item_ids).unwrap_or_default();
    let mut tracks = Vec::new();
    for id in ids {
        if let Some(track) = track_entity::Entity::find_by_id(&id).one(&state.db).await? {
            tracks.push(track);
            continue;
        }
        tracks.extend(
            track_entity::Entity::find()
                .filter(track_entity::Column::AlbumId.eq(&id))
                .order_by_asc(track_entity::Column::DiscNumber)
                .order_by_asc(track_entity::Column::TrackNumber)
                .all(&state.db)
                .await?,
        );
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
    let mut params = match collect_params(request).await {
        Ok(value) => value,
        Err(error) => return subsonic_error(&HashMap::new(), 10, &error.to_string()),
    };
    if let Some(base_url) = request_base_url {
        params.insert("_mnest_base_url".into(), base_url);
    }
    if method == "getOpenSubsonicExtensions" {
        return subsonic_response(&params, open_subsonic_extensions());
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

    if matches!(method, "stream" | "download" | "getCoverArt" | "getAvatar") {
        return match binary_endpoint(&state, &user, method, &params).await {
            Ok(response) => response,
            Err(error) => subsonic_error(&params, 70, &error.to_string()),
        };
    }

    match json_endpoint(&state, &user, method, &params).await {
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

fn open_subsonic_extensions() -> Value {
    json!({"openSubsonicExtensions":[
        {"name":"apiKeyAuthentication","versions":[1]},
        {"name":"formPost","versions":[1]},
        {"name":"songLyrics","versions":[1]},
        {"name":"transcodeOffset","versions":[1]},
        {"name":"indexBasedQueue","versions":[1]}
    ]})
}

async fn json_endpoint(
    state: &AppState,
    user: &User,
    method: &str,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    match method {
        "ping" => Ok(json!({})),
        "getLicense" => Ok(
            json!({"license":{"valid":true,"email":user.email,"licenseExpires":"2099-12-31T23:59:59.000Z"}}),
        ),
        "tokenInfo" => Ok(json!({"tokenInfo":{"username":user.username}})),
        "getMusicFolders" => music_folders(state).await,
        "getArtists" => artists(state, p).await,
        "getIndexes" => indexes(state, p).await,
        "getArtist" => get_artist(state, required(p, "id")?).await,
        "getAlbum" => get_album(state, required(p, "id")?).await,
        "getSong" => get_song(state, user, required(p, "id")?).await,
        "getMusicDirectory" => music_directory(state, required(p, "id")?).await,
        "getGenres" => genres(state).await,
        "getArtistInfo" | "getArtistInfo2" => {
            Ok(json!({if method.ends_with('2') {"artistInfo2"} else {"artistInfo"}: {}}))
        }
        "getAlbumInfo" | "getAlbumInfo2" => Ok(json!({"albumInfo": {}})),
        "getSimilarSongs" | "getSimilarSongs2" => {
            similar_songs(state, method, required(p, "id")?, int(p, "count", 50)).await
        }
        "getTopSongs" => top_songs(state, required(p, "id")?, int(p, "count", 50)).await,
        "getAlbumList" | "getAlbumList2" => album_list(state, user, method, p).await,
        "getRandomSongs" => random_songs(state, p).await,
        "getSongsByGenre" => songs_by_genre(state, p).await,
        "getNowPlaying" => Ok(json!({"nowPlaying":{"entry":[]}})),
        "getStarred" | "getStarred2" => starred(state, user, method, p).await,
        "search" => legacy_search(state, p).await,
        "search2" | "search3" => search(state, method, p).await,
        "getPlaylists" => playlists(state, user, p).await,
        "getPlaylist" => playlist(state, user, required(p, "id")?).await,
        "createPlaylist" => create_playlist(state, user, p).await,
        "updatePlaylist" => update_playlist(state, user, p).await,
        "deletePlaylist" => delete_playlist(state, user, required(p, "id")?).await,
        "getLyrics" => get_lyrics_legacy(state, p).await,
        "getLyricsBySongId" => get_lyrics_by_song(state, required(p, "id")?).await,
        "star" => favorite(state, user, p, true).await,
        "unstar" => favorite(state, user, p, false).await,
        "setRating" => set_rating(state, user, p).await,
        "scrobble" => scrobble(state, user, p).await,
        "getShares" => shares(state, user).await,
        "createShare" => create_share(state, user, p).await,
        "updateShare" => update_share(state, user, p).await,
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
        "changePassword" => change_password(state, user, p).await,
        "getBookmarks" => bookmarks(state, user).await,
        "createBookmark" => create_bookmark(state, user, p).await,
        "deleteBookmark" => delete_bookmark(state, user, required(p, "id")?).await,
        "getPlayQueue" => get_play_queue(state, user, false).await,
        "getPlayQueueByIndex" => get_play_queue(state, user, true).await,
        "savePlayQueue" => save_play_queue(state, user, p, false).await,
        "savePlayQueueByIndex" => save_play_queue(state, user, p, true).await,
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
    }
}

async fn binary_endpoint(
    state: &AppState,
    _user: &User,
    method: &str,
    p: &HashMap<String, String>,
) -> anyhow::Result<Response> {
    match method {
        "stream" | "download" => {
            let track = track(state, required_anyhow(p, "id")?).await?;
            let raw = p.get("format").is_some_and(|value| value == "raw");
            let requested_format = p.get("format").filter(|value| value.as_str() != "raw");
            let max_bitrate = p
                .get("maxBitRate")
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| *value > 0);
            let time_offset = p
                .get("timeOffset")
                .filter(|value| value.parse::<f64>().is_ok_and(|value| value > 0.0));
            if method == "download"
                || raw
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
                    time_offset,
                )
                .await
            }
        }
        "getCoverArt" => {
            let id = required_anyhow(p, "id")?;
            let image_id = id.strip_prefix("img-").context("invalid cover art id")?;
            let path = if album_entity::Entity::find_by_id(image_id)
                .one(&state.db)
                .await?
                .is_some()
            {
                track_entity::Entity::find()
                    .filter(track_entity::Column::AlbumId.eq(image_id))
                    .order_by_asc(track_entity::Column::DiscNumber)
                    .order_by_asc(track_entity::Column::TrackNumber)
                    .one(&state.db)
                    .await?
                    .context("album cover source not found")?
                    .path
            } else {
                track(state, image_id).await?.path
            };
            let tags = state.tags.clone();
            let artwork =
                tokio::task::spawn_blocking(move || tags.read_artwork(std::path::Path::new(&path)))
                    .await??
                    .context("cover art not found")?;
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

async fn music_folders(state: &AppState) -> Result<Value, ApiFailure> {
    let folders = enabled_music_folders(state).await?;
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
    p: &HashMap<String, String>,
) -> Result<Option<String>, ApiFailure> {
    let Some(api_id) = p.get("musicFolderId") else {
        return Ok(None);
    };
    find_music_folder(state, api_id)
        .await?
        .map(|folder| Some(folder.id))
        .ok_or_else(not_found)
}

async fn library_artists(
    state: &AppState,
    folder_id: Option<&str>,
) -> Result<Vec<Artist>, ApiFailure> {
    let mut request = artist_entity::Entity::find();
    if let Some(folder_id) = folder_id {
        let track_ids = track_entity::Entity::find()
            .select_only()
            .column(track_entity::Column::Id)
            .filter(track_entity::Column::FolderId.eq(folder_id))
            .into_query();
        let artist_ids = track_artist_entity::Entity::find()
            .select_only()
            .column(track_artist_entity::Column::ArtistId)
            .filter(track_artist_entity::Column::TrackId.in_subquery(track_ids))
            .into_query();
        request = request.filter(artist_entity::Column::Id.in_subquery(artist_ids));
    } else {
        request = request.filter(
            Condition::any()
                .add(artist_entity::Column::SongCount.gt(0))
                .add(artist_entity::Column::AlbumCount.gt(0)),
        );
    }
    Ok(request
        .order_by_asc(artist_entity::Column::SortName)
        .all(&state.db)
        .await?)
}

async fn artist_cover_art_map(
    state: &AppState,
    artist_ids: &[String],
    folder_id: Option<&str>,
) -> Result<HashMap<String, String>, ApiFailure> {
    let mut artist_ids = artist_ids.iter().map(String::as_str).collect::<Vec<_>>();
    artist_ids.sort_unstable();
    artist_ids.dedup();
    if artist_ids.is_empty() {
        return Ok(HashMap::new());
    }
    const SELECT: &str = "SELECT ta.artist_id,t.id AS track_id,t.album_id FROM track_artists ta JOIN tracks t ON t.id=ta.track_id";
    const ORDER: &str = " ORDER BY ta.artist_id,CASE WHEN t.album_id IS NULL THEN 1 ELSE 0 END,ta.position,t.album_id,t.disc_number,t.track_number,t.title,t.id";
    let mut covers = HashMap::new();
    for chunk in artist_ids.chunks(500) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(",");
        let folder_filter = folder_id
            .map(|_| format!(" AND t.folder_id=${}", chunk.len() + 1))
            .unwrap_or_default();
        let mut query = db::raw(
            &state.db,
            format!("{SELECT} WHERE ta.artist_id IN ({placeholders}){folder_filter}{ORDER}"),
        );
        for artist_id in chunk {
            query = query.bind(*artist_id);
        }
        if let Some(folder_id) = folder_id {
            query = query.bind(folder_id);
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
    folder_id: Option<&str>,
) -> Result<i64, ApiFailure> {
    let mut request = track_entity::Entity::find()
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

async fn artists(state: &AppState, p: &HashMap<String, String>) -> Result<Value, ApiFailure> {
    let folder_id = requested_music_folder(state, p).await?;
    let artists = library_artists(state, folder_id.as_deref()).await?;
    let artist_ids = artists
        .iter()
        .map(|artist| artist.id.clone())
        .collect::<Vec<_>>();
    let cover_art = artist_cover_art_map(state, &artist_ids, folder_id.as_deref()).await?;
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
async fn indexes(state: &AppState, p: &HashMap<String, String>) -> Result<Value, ApiFailure> {
    let folder_id = requested_music_folder(state, p).await?;
    let artists = library_artists(state, folder_id.as_deref()).await?;
    let mut groups: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for artist in artists {
        groups
            .entry(initial(&artist.name))
            .or_default()
            .push(json!({"id":artist.id,"name":artist.name}));
    }
    let last_modified = library_last_modified(state, folder_id.as_deref()).await?;
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
async fn get_artist(state: &AppState, id: &str) -> Result<Value, ApiFailure> {
    let artist = artist_entity::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(not_found)?;
    let track_ids = track_artist_entity::Entity::find()
        .select_only()
        .column(track_artist_entity::Column::TrackId)
        .filter(track_artist_entity::Column::ArtistId.eq(id))
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
    let cover_art = artist_cover_art_map(state, std::slice::from_ref(&artist.id), None).await?;
    let mut data = artist_json(&artist, cover_art.get(id).map(String::as_str));
    data["album"] = Value::Array(albums.iter().map(album_json).collect());
    Ok(json!({"artist":data}))
}
async fn get_album(state: &AppState, id: &str) -> Result<Value, ApiFailure> {
    let album = album(state, id).await?;
    let tracks = track_entity::Entity::find()
        .filter(track_entity::Column::AlbumId.eq(id))
        .order_by_asc(track_entity::Column::DiscNumber)
        .order_by_asc(track_entity::Column::TrackNumber)
        .order_by_asc(track_entity::Column::Title)
        .all(&state.db)
        .await?;
    let mut data = album_json(&album);
    data["song"] = Value::Array(tracks.iter().map(|t| track_json(t, None)).collect());
    Ok(json!({"album":data}))
}
async fn get_song(state: &AppState, user: &User, id: &str) -> Result<Value, ApiFailure> {
    let track = track(state, id).await.map_err(ApiFailure::from)?;
    let starred = favorite_entity::Entity::find()
        .filter(favorite_entity::Column::UserId.eq(&user.id))
        .filter(favorite_entity::Column::ItemType.eq("track"))
        .filter(favorite_entity::Column::ItemId.eq(id))
        .one(&state.db)
        .await?;
    Ok(json!({"song":track_json(&track, starred.map(|favorite|favorite.created_at))}))
}
async fn music_directory(state: &AppState, id: &str) -> Result<Value, ApiFailure> {
    if let Some(folder) = find_music_folder(state, id).await? {
        let folder_id = folder.id.clone();
        let parent_id = folder_api_id(&folder.id).to_string();
        let artists = library_artists(state, Some(&folder_id)).await?;
        let artist_ids = artists
            .iter()
            .map(|artist| artist.id.clone())
            .collect::<Vec<_>>();
        let cover_art = artist_cover_art_map(state, &artist_ids, Some(&folder_id)).await?;
        return Ok(
            json!({"directory":{"id":parent_id,"name":folder.name,"child":artists.iter().map(|artist|artist_child_json(artist,Some(&parent_id),cover_art.get(&artist.id).map(String::as_str))).collect::<Vec<_>>()}}),
        );
    }
    if let Some(artist) = artist_entity::Entity::find_by_id(id).one(&state.db).await? {
        let track_ids = track_artist_entity::Entity::find()
            .select_only()
            .column(track_artist_entity::Column::TrackId)
            .filter(track_artist_entity::Column::ArtistId.eq(id))
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
        return Ok(
            json!({"directory":{"id":artist.id,"name":artist.name,"child":albums.into_iter().map(|a|json!({"id":a.id,"parent":artist.id,"title":a.name,"album":a.name,"artist":a.artist_name,"isDir":true,"coverArt":format!("img-{}",a.id)})).collect::<Vec<_>>()}}),
        );
    }
    let album = album(state, id).await?;
    let tracks = track_entity::Entity::find()
        .filter(track_entity::Column::AlbumId.eq(id))
        .order_by_asc(track_entity::Column::DiscNumber)
        .order_by_asc(track_entity::Column::TrackNumber)
        .order_by_asc(track_entity::Column::Title)
        .all(&state.db)
        .await?;
    Ok(
        json!({"directory":{"id":album.id,"name":album.name,"child":tracks.iter().map(|t|track_json(t,None)).collect::<Vec<_>>()}}),
    )
}
async fn genres(state: &AppState) -> Result<Value, ApiFailure> {
    #[derive(FromQueryResult)]
    struct GenreRow {
        genre: String,
        song_count: i64,
        album_count: i64,
    }
    let values = track_entity::Entity::find()
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
async fn similar_songs(
    state: &AppState,
    method: &str,
    id: &str,
    count: i64,
) -> Result<Value, ApiFailure> {
    let base = track(state, id).await.map_err(ApiFailure::from)?;
    let tracks = db::raw(
        &state.db,
        track_select("WHERE id<>$1 AND (genre=$2 OR id IN (SELECT other.track_id FROM track_artists mine JOIN track_artists other ON other.artist_id=mine.artist_id WHERE mine.track_id=$1)) ORDER BY play_count DESC LIMIT $3"),
    )
    .bind(id)
    .bind(base.genre)
    .bind(count.clamp(0, 500))
    .all::<Track>()
    .await?;
    let key = if method.ends_with('2') {
        "similarSongs2"
    } else {
        "similarSongs"
    };
    Ok(json!({key:{"song":tracks.iter().map(|v|track_json(v,None)).collect::<Vec<_>>()}}))
}
async fn top_songs(state: &AppState, artist: &str, count: i64) -> Result<Value, ApiFailure> {
    let artist = artist_entity::Entity::find()
        .filter(
            Condition::any()
                .add(artist_entity::Column::Id.eq(artist))
                .add(artist_entity::Column::Name.eq(artist)),
        )
        .one(&state.db)
        .await?
        .ok_or_else(not_found)?;
    let track_ids = track_artist_entity::Entity::find()
        .select_only()
        .column(track_artist_entity::Column::TrackId)
        .filter(track_artist_entity::Column::ArtistId.eq(artist.id))
        .into_query();
    let tracks = track_entity::Entity::find()
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

    let folder_id = requested_music_folder(state, p).await?;
    let mut request = album_entity::Entity::find();
    if let Some(folder_id) = folder_id {
        let album_ids = track_entity::Entity::find()
            .select_only()
            .column(track_entity::Column::AlbumId)
            .filter(track_entity::Column::FolderId.eq(folder_id))
            .into_query();
        request = request.filter(album_entity::Column::Id.in_subquery(album_ids));
    }
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
    let albums = request
        .order_by_asc(album_entity::Column::Id)
        .limit(int(p, "size", 10).clamp(1, 500) as u64)
        .offset(int(p, "offset", 0).max(0) as u64)
        .all(&state.db)
        .await?;
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
async fn random_songs(state: &AppState, p: &HashMap<String, String>) -> Result<Value, ApiFailure> {
    let folder_id = requested_music_folder(state, p).await?;
    let mut request = track_entity::Entity::find();
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
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let genre = required(p, "genre")?;
    let folder_id = requested_music_folder(state, p).await?;
    let mut request = track_entity::Entity::find().filter(track_entity::Column::Genre.eq(genre));
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
    method: &str,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let folder_id = requested_music_folder(state, p).await?;
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
        artist_cover_art_map(state, &starred_artist_ids, folder_id.as_deref()).await?
    } else {
        HashMap::new()
    };
    let mut songs = Vec::new();
    let mut albums = Vec::new();
    let mut artists = Vec::new();
    for star in stars {
        match star.item_type.as_str() {
            "track" => {
                if let Some(track) = track_entity::Entity::find_by_id(&star.item_id)
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
                let in_folder = if let Some(folder_id) = folder_id.as_ref() {
                    track_entity::Entity::find()
                        .filter(track_entity::Column::AlbumId.eq(&star.item_id))
                        .filter(track_entity::Column::FolderId.eq(folder_id))
                        .one(&state.db)
                        .await?
                        .is_some()
                } else {
                    true
                };
                if in_folder
                    && let Some(album) = album_entity::Entity::find_by_id(&star.item_id)
                        .one(&state.db)
                        .await?
                {
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
                let in_folder = if let Some(folder_id) = folder_id.as_ref() {
                    let track_ids = track_entity::Entity::find()
                        .select_only()
                        .column(track_entity::Column::Id)
                        .filter(track_entity::Column::FolderId.eq(folder_id))
                        .into_query();
                    track_artist_entity::Entity::find()
                        .filter(track_artist_entity::Column::ArtistId.eq(&star.item_id))
                        .filter(track_artist_entity::Column::TrackId.in_subquery(track_ids))
                        .one(&state.db)
                        .await?
                        .is_some()
                } else {
                    true
                };
                if in_folder
                    && let Some(artist) = artist_entity::Entity::find_by_id(&star.item_id)
                        .one(&state.db)
                        .await?
                {
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

async fn legacy_search(state: &AppState, p: &HashMap<String, String>) -> Result<Value, ApiFailure> {
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
        let artists = artist_entity::Entity::find()
            .filter(artist_entity::Column::Name.contains(query))
            .order_by_asc(artist_entity::Column::Name)
            .all(&state.db)
            .await?;
        let artist_ids = artists
            .iter()
            .map(|artist| artist.id.clone())
            .collect::<Vec<_>>();
        let cover_art = artist_cover_art_map(state, &artist_ids, None).await?;
        matches.extend(artists.iter().map(|artist| {
            artist_child_json(artist, None, cover_art.get(&artist.id).map(String::as_str))
        }));
    }
    if let Some(query) = album_query {
        let albums = album_entity::Entity::find()
            .filter(album_entity::Column::Name.contains(query))
            .order_by_asc(album_entity::Column::Name)
            .all(&state.db)
            .await?;
        matches.extend(albums.iter().map(album_child_json));
    }
    if let Some(query) = title_query {
        let tracks = track_entity::Entity::find()
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
    method: &str,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let query = normalize_search_query(present(p, "query")?);
    let folder_id = requested_music_folder(state, p).await?;
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
    let mut track_request = track_entity::Entity::find()
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
    if let Some(folder_id) = folder_id.as_deref() {
        let folder_tracks = track_entity::Entity::find()
            .select_only()
            .column(track_entity::Column::Id)
            .filter(track_entity::Column::FolderId.eq(folder_id))
            .into_query();
        let folder_artists = track_artist_entity::Entity::find()
            .select_only()
            .column(track_artist_entity::Column::ArtistId)
            .filter(track_artist_entity::Column::TrackId.in_subquery(folder_tracks))
            .into_query();
        let folder_albums = track_entity::Entity::find()
            .select_only()
            .column(track_entity::Column::AlbumId)
            .filter(track_entity::Column::FolderId.eq(folder_id))
            .into_query();
        artist_request =
            artist_request.filter(artist_entity::Column::Id.in_subquery(folder_artists));
        album_request = album_request.filter(album_entity::Column::Id.in_subquery(folder_albums));
        track_request = track_request.filter(track_entity::Column::FolderId.eq(folder_id));
    }
    let (artists, albums, tracks) = tokio::try_join!(
        artist_request.all(&state.db),
        album_request.all(&state.db),
        track_request.all(&state.db),
    )?;
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
        let cover_art = artist_cover_art_map(state, &artist_ids, folder_id.as_deref()).await?;
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
        let tracks = playlist_tracks(state, &row.id).await?;
        values.push(playlist_json(state, &row, &tracks).await?);
    }
    Ok(json!({"playlists":{"playlist":values}}))
}
async fn playlist(state: &AppState, user: &User, id: &str) -> Result<Value, ApiFailure> {
    let row = accessible_playlist(state, user, id).await?;
    let tracks = playlist_tracks(state, id).await?;
    let mut value = playlist_json(state, &row, &tracks).await?;
    value["entry"] = Value::Array(tracks.iter().map(|v| track_json(v, None)).collect());
    Ok(json!({"playlist":value}))
}
async fn create_playlist(
    state: &AppState,
    user: &User,
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
    replace_playlist_tracks(state, &id, p).await?;
    playlist(state, user, &id).await
}
async fn update_playlist(
    state: &AppState,
    user: &User,
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
        set_playlist_tracks(state, id, &ids).await?;
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
    id: &str,
    p: &HashMap<String, String>,
) -> Result<(), ApiFailure> {
    if p.contains_key("songId") {
        set_playlist_tracks(state, id, &multi(p, "songId")).await?;
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

async fn playlist_tracks(state: &AppState, id: &str) -> Result<Vec<Track>, ApiFailure> {
    let ids = playlist_track_ids(state, id).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let tracks = track_entity::Entity::find()
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
    id: &str,
    track_ids: &[String],
) -> Result<(), ApiFailure> {
    validate_track_ids(state, track_ids, MAX_COLLECTION_ITEMS).await?;

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
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let artist = p.get("artist").map(String::as_str).unwrap_or("");
    let title = p.get("title").map(String::as_str).unwrap_or("");
    let mut request = track_entity::Entity::find();
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
async fn get_lyrics_by_song(state: &AppState, id: &str) -> Result<Value, ApiFailure> {
    let track = track(state, id).await.map_err(ApiFailure::from)?;
    if track.lyrics.trim().is_empty() {
        return Ok(json!({"lyricsList":{"structuredLyrics":[]}}));
    }
    let timed_lines = track
        .lyrics
        .lines()
        .filter_map(parse_lrc_line)
        .map(|(start, value)| json!({"start":start,"value":value}))
        .collect::<Vec<_>>();
    let synced = !timed_lines.is_empty();
    let lines = if synced {
        timed_lines
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
    Ok(
        json!({"lyricsList":{"structuredLyrics":[{"displayArtist":display_artist,"displayTitle":track.title,"lang":"und","synced":synced,"line":lines}]}}),
    )
}

fn parse_lrc_line(line: &str) -> Option<(i64, String)> {
    let end = line.find(']')?;
    let timestamp = line.strip_prefix('[')?[..end - 1].split_once(':')?;
    let minutes = timestamp.0.parse::<u64>().ok()?;
    let seconds = timestamp.1.parse::<f64>().ok()?;
    if !(0.0..60.0).contains(&seconds) {
        return None;
    }
    let start = minutes
        .checked_mul(60_000)?
        .checked_add((seconds * 1000.0).round() as u64)?;
    Some((i64::try_from(start).ok()?, line[end + 1..].to_owned()))
}
async fn favorite(
    state: &AppState,
    user: &User,
    p: &HashMap<String, String>,
    add: bool,
) -> Result<Value, ApiFailure> {
    let groups = [
        ("track", multi(p, "id")),
        ("album", multi(p, "albumId")),
        ("artist", multi(p, "artistId")),
    ];
    if add {
        for (kind, ids) in &groups {
            validate_catalog_ids(state, kind, ids).await?;
        }
    } else if groups
        .iter()
        .any(|(_, ids)| ids.len() > MAX_COLLECTION_ITEMS)
    {
        return Err(ApiFailure::new(10, "Too many IDs in one request"));
    }
    let transaction = state.db.begin().await?;
    for (kind, ids) in groups {
        for id in ids {
            if add {
                favorite_entity::Entity::delete_many()
                    .filter(favorite_entity::Column::UserId.eq(&user.id))
                    .filter(favorite_entity::Column::ItemType.eq(kind))
                    .filter(favorite_entity::Column::ItemId.eq(&id))
                    .exec(&transaction)
                    .await?;
                favorite_entity::ActiveModel {
                    user_id: Set(user.id.clone()),
                    item_type: Set(kind.to_owned()),
                    item_id: Set(id),
                    created_at: Set(Utc::now().to_rfc3339()),
                }
                .insert(&transaction)
                .await?;
            } else {
                favorite_entity::Entity::delete_many()
                    .filter(favorite_entity::Column::UserId.eq(&user.id))
                    .filter(favorite_entity::Column::ItemType.eq(kind))
                    .filter(favorite_entity::Column::ItemId.eq(id))
                    .exec(&transaction)
                    .await?;
            }
        }
    }
    transaction.commit().await?;
    Ok(json!({}))
}

async fn validate_catalog_ids(
    state: &AppState,
    kind: &str,
    ids: &[String],
) -> Result<(), ApiFailure> {
    if ids.len() > MAX_COLLECTION_ITEMS {
        return Err(ApiFailure::new(10, "Too many IDs in one request"));
    }
    let requested = ids.iter().cloned().collect::<HashSet<_>>();
    if requested.is_empty() {
        return Ok(());
    }
    let existing = match kind {
        "track" => {
            track_entity::Entity::find()
                .select_only()
                .column(track_entity::Column::Id)
                .filter(track_entity::Column::Id.is_in(requested.iter().cloned()))
                .into_tuple::<String>()
                .all(&state.db)
                .await?
        }
        "album" => {
            album_entity::Entity::find()
                .select_only()
                .column(album_entity::Column::Id)
                .filter(album_entity::Column::Id.is_in(requested.iter().cloned()))
                .into_tuple::<String>()
                .all(&state.db)
                .await?
        }
        "artist" => {
            artist_entity::Entity::find()
                .select_only()
                .column(artist_entity::Column::Id)
                .filter(artist_entity::Column::Id.is_in(requested.iter().cloned()))
                .into_tuple::<String>()
                .all(&state.db)
                .await?
        }
        _ => return Err(ApiFailure::new(10, "Invalid catalog item type")),
    }
    .into_iter()
    .collect::<HashSet<_>>();
    if existing != requested {
        return Err(not_found());
    }
    Ok(())
}
async fn set_rating(
    state: &AppState,
    user: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = required(p, "id")?;
    let rating = required_i64(p, "rating")?;
    if !(0..=5).contains(&rating) {
        return Err(ApiFailure::new(10, "Rating must be between 0 and 5"));
    }
    let item_type = if track_entity::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .is_some()
    {
        "track"
    } else if album_entity::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .is_some()
    {
        "album"
    } else if artist_entity::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .is_some()
    {
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
async fn scrobble(
    state: &AppState,
    user: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let ids = multi(p, "id");
    if ids.is_empty() {
        return Err(ApiFailure::new(10, "Missing required parameter: id"));
    }
    validate_track_ids(state, &ids, MAX_SCROBBLE_BATCH).await?;
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
        if submission {
            track_entity::Entity::update_many()
                .col_expr(
                    track_entity::Column::PlayCount,
                    Expr::col(track_entity::Column::PlayCount).add(1),
                )
                .filter(track_entity::Column::Id.eq(id))
                .exec(&transaction)
                .await?;
        }
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
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = Uuid::new_v4().to_string();
    let ids = multi(p, "id");
    if ids.is_empty() {
        return Err(ApiFailure::new(10, "Missing required parameter: id"));
    }
    validate_track_ids(state, &ids, MAX_COLLECTION_ITEMS).await?;
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
            json!({"id":v.id,"name":v.name,"streamUrl":stream_url,"homePageUrl":v.home_page_url})
        }).collect::<Vec<_>>()}}),
    )
}
async fn create_radio(
    state: &AppState,
    user: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    require_admin(user)?;
    let (name, stream_url, home_page_url) = validated_radio_fields(p)?;
    let id = Uuid::new_v4().to_string();
    let transaction = state.db.begin().await?;
    radio_entity::ActiveModel {
        id: Set(id.clone()),
        name: Set(name),
        stream_url: Set(stream_url),
        home_page_url: Set(home_page_url),
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
    let (name, stream_url, home_page_url) = validated_radio_fields(p)?;
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
    active.update(&transaction).await?;
    if p.contains_key("proxy") {
        internet_radio::set_proxy_enabled(&transaction, &radio_id, bool_param(p, "proxy")).await?;
    }
    transaction.commit().await?;
    if stream_url_changed {
        state.radio_streams.cancel(&radio_id).await;
    }
    Ok(json!({}))
}

fn validated_radio_fields(
    p: &HashMap<String, String>,
) -> Result<(String, String, String), ApiFailure> {
    let name = required(p, "name")?.trim();
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err(ApiFailure::new(10, "Invalid internet radio name"));
    }
    let stream_url = validate_radio_url(required(p, "streamUrl")?, "streamUrl")?;
    let home_page_url = match p.get("homepageUrl").map(|value| value.trim()) {
        Some(value) if !value.is_empty() => validate_radio_url(value, "homepageUrl")?,
        _ => String::new(),
    };
    Ok((name.to_owned(), stream_url, home_page_url))
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
    let folder_ids = user_folder_ids(state).await?;
    Ok(json!({"user":user_json(&user, &folder_ids)}))
}
async fn get_users(state: &AppState, requester: &User) -> Result<Value, ApiFailure> {
    require_admin(requester)?;
    let users = user_entity::Entity::find()
        .order_by_asc(user_entity::Column::Username)
        .all(&state.db)
        .await?;
    let folder_ids = user_folder_ids(state).await?;
    Ok(
        json!({"users":{"user":users.iter().map(|user|user_json(user, &folder_ids)).collect::<Vec<_>>()}}),
    )
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
    user_entity::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        username: Set(username.to_owned()),
        password_hash: Set(hash),
        email: Set(required(p, "email")?.to_owned()),
        role: Set(if bool_param(p, "adminRole") {
            "admin".into()
        } else {
            "user".into()
        }),
        subsonic_token: Set(Uuid::new_v4().simple().to_string()),
        subsonic_password: Set(encrypt_subsonic_password(
            &password,
            &state.settings.auth.jwt_secret,
            username,
        )?),
        created_at: Set(Utc::now().to_rfc3339()),
    }
    .insert(&state.db)
    .await?;
    Ok(json!({}))
}
async fn update_user(
    state: &AppState,
    requester: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    require_admin(requester)?;
    let username = required(p, "username")?;
    let password = decode_subsonic_password(required(p, "password")?)?;
    validate_new_password(&password)?;
    let user = user_by_name(&state.db, username)
        .await?
        .ok_or_else(not_found)?;
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
    active.password_hash = Set(hash_password(&password)?);
    active.subsonic_password = Set(encrypt_subsonic_password(
        &password,
        &state.settings.auth.jwt_secret,
        username,
    )?);
    active.update(&state.db).await?;
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
    share_entity::Entity::delete_many()
        .filter(share_entity::Column::UserId.eq(&user.id))
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
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let username = required(p, "username")?;
    if requester.role != "admin" && requester.username != username {
        return Err(ApiFailure::new(50, "Not authorized"));
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

async fn bookmarks(state: &AppState, user: &User) -> Result<Value, ApiFailure> {
    let rows = bookmark_entity::Entity::find()
        .filter(bookmark_entity::Column::UserId.eq(&user.id))
        .order_by_desc(bookmark_entity::Column::ChangedAt)
        .all(&state.db)
        .await?;
    let mut values = Vec::new();
    for row in rows {
        if let Ok(track) = track(state, &row.track_id).await {
            values.push(json!({"position":row.position,"username":user.username,"comment":row.comment,"created":row.changed_at,"changed":row.changed_at,"entry":track_json(&track,None)}));
        }
    }
    Ok(json!({"bookmarks":{"bookmark":values}}))
}
async fn create_bookmark(
    state: &AppState,
    user: &User,
    p: &HashMap<String, String>,
) -> Result<Value, ApiFailure> {
    let id = required(p, "id")?;
    track(state, id).await.map_err(ApiFailure::from)?;
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
    let mut entries = Vec::new();
    for id in &ids {
        if let Ok(t) = track(state, id).await {
            entries.push(track_json(&t, None));
        }
    }
    let mut queue = json!({"position":row.position,"username":user.username,"changed":row.changed_at,"changedBy":row.changed_by,"entry":entries});
    if !ids.is_empty() {
        if by_index {
            queue["currentIndex"] = json!(
                row.current_id
                    .as_ref()
                    .and_then(|current| ids.iter().position(|id| id == current))
                    .unwrap_or(0)
            );
        } else {
            queue["current"] = json!(row.current_id.unwrap_or_else(|| ids[0].clone()));
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
    p: &HashMap<String, String>,
    by_index: bool,
) -> Result<Value, ApiFailure> {
    let ids = multi(p, "id");
    validate_track_ids(state, &ids, MAX_COLLECTION_ITEMS).await?;
    let current_id = if ids.is_empty() {
        None
    } else if by_index {
        let index = required_i64(p, "currentIndex")?;
        ids.get(index.max(0) as usize)
            .cloned()
            .ok_or_else(|| ApiFailure::new(10, "currentIndex is outside the play queue"))?
            .into()
    } else {
        let current = required(p, "current")?.to_owned();
        if !ids.contains(&current) {
            return Err(ApiFailure::new(
                10,
                "current is not present in the play queue",
            ));
        }
        Some(current)
    };
    let transaction = state.db.begin().await?;
    play_queue_entity::Entity::delete_by_id(&user.id)
        .exec(&transaction)
        .await?;
    play_queue_entity::ActiveModel {
        user_id: Set(user.id.clone()),
        track_ids: Set(serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into())),
        current_id: Set(current_id),
        position: Set(int(p, "position", 0).max(0)),
        changed_at: Set(Utc::now().to_rfc3339()),
        changed_by: Set(p.get("c").cloned().unwrap_or_default()),
    }
    .insert(&transaction)
    .await?;
    transaction.commit().await?;
    Ok(json!({}))
}

async fn validate_track_ids(
    state: &AppState,
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
    let existing_ids = track_entity::Entity::find()
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
    offset: Option<&String>,
) -> anyhow::Result<Response> {
    let (muxer, mime_extension) = match format.to_ascii_lowercase().as_str() {
        "mp3" => ("mp3", "mp3"),
        "opus" => ("opus", "opus"),
        "aac" => ("adts", "aac"),
        "flac" => ("flac", "flac"),
        "ogg" => ("ogg", "ogg"),
        _ => anyhow::bail!("unsupported transcode format"),
    };
    let mut command = Command::new(&state.settings.tools.ffmpeg);
    command.kill_on_drop(true);
    command.arg("-v").arg("error");
    if let Some(offset) = offset {
        command.args(["-ss", offset]);
    }
    command.args(["-i", &track.path, "-vn"]);
    if let Some(rate) = max_bitrate {
        command.args(["-b:a", &format!("{}k", rate.clamp(16, 320))]);
    }
    command
        .args(["-f", muxer, "pipe:1"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("ffmpeg stdout unavailable")?;
    let stream = futures::stream::try_unfold(
        (ReaderStream::new(stdout), child),
        |(mut stdout, mut child)| async move {
            match stdout.next().await {
                Some(Ok(chunk)) => Ok(Some((chunk, (stdout, child)))),
                Some(Err(error)) => {
                    let _ = child.kill().await;
                    Err(error)
                }
                None => {
                    let status = child.wait().await?;
                    if status.success() {
                        Ok(None)
                    } else {
                        Err(std::io::Error::other(format!(
                            "ffmpeg exited with status {status}"
                        )))
                    }
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

async fn track(state: &AppState, id: &str) -> anyhow::Result<Track> {
    track_entity::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .context("Track not found")
}
async fn album(state: &AppState, id: &str) -> Result<Album, ApiFailure> {
    album_entity::Entity::find_by_id(id)
        .one(&state.db)
        .await?
        .ok_or_else(not_found)
}
fn track_select(tail: &str) -> String {
    format!(
        "SELECT id,folder_id,path,relative_path,title,artist_id,artist_name,artists_json,album_id,album_name,album_artist,genre,year,track_number,disc_number,duration,bit_rate,size,suffix,mimetype,lyrics,comment,cover_path,mtime,fingerprint,play_count,needs_scrape,created_at,updated_at FROM tracks {tail}"
    )
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
    let mut value = json!({"id":a.id,"name":a.name,"artist":a.artist_name,"artistId":a.artist_id,"displayArtist":a.artist_name,"coverArt":format!("img-{}",a.id),"songCount":a.song_count,"duration":a.duration as i64,"year":a.year,"genre":a.genre,"created":a.created_at});
    let artist_names = parse_artist_names(&a.artist_name);
    // The album table stores only one artist ID, so do not attach that ID to a combined name.
    if artist_names.len() == 1 {
        value["artists"] = json!([{"id":a.artist_id,"name":artist_names[0]}]);
    }
    value
}
fn album_child_json(album: &Album) -> Value {
    json!({"id":album.id,"parent":album.artist_id,"isDir":true,"title":album.name,"album":album.name,"artist":album.artist_name,"artistId":album.artist_id,"albumId":album.id,"coverArt":format!("img-{}",album.id),"duration":album.duration as i64,"year":album.year,"genre":album.genre,"created":album.created_at,"type":"music"})
}
fn track_json(t: &Track, starred: Option<String>) -> Value {
    let cover_art = canonical_track_cover_art(&t.id, t.album_id.as_deref());
    let artists = serde_json::from_str::<Vec<ArtistCredit>>(&t.artists_json).unwrap_or_default();
    let artist = artists
        .iter()
        .map(|artist| artist.name.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let artist_id = artists.first().map(|artist| artist.id.clone());
    let mut v = json!({"id":t.id,"isDir":false,"isVideo":false,"title":t.title,"album":t.album_name,"artist":artist,"displayArtist":artist,"artists":artists,"track":t.track_number,"discNumber":t.disc_number,"year":t.year,"genre":t.genre,"coverArt":cover_art,"size":t.size,"contentType":t.mimetype,"suffix":t.suffix,"duration":t.duration as i64,"bitRate":t.bit_rate,"path":t.relative_path,"type":"music","mediaType":"song","playCount":t.play_count,"bookmarkPosition":0,"created":t.created_at,"comment":t.comment});
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
async fn user_folder_ids(state: &AppState) -> Result<Vec<i32>, ApiFailure> {
    Ok(enabled_music_folders(state)
        .await?
        .iter()
        .map(|folder| folder_api_id(&folder.id))
        .collect())
}
fn user_json(v: &User, folder_ids: &[i32]) -> Value {
    json!({"username":v.username,"email":v.email,"scrobblingEnabled":true,"adminRole":v.role=="admin","settingsRole":v.role=="admin","downloadRole":true,"uploadRole":v.role=="admin","playlistRole":true,"coverArtRole":true,"commentRole":true,"podcastRole":false,"streamRole":true,"jukeboxRole":false,"shareRole":true,"videoConversionRole":false,"folder":folder_ids})
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
        let db = crate::db::connect(&crate::config::DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        crate::db::migrate(&db).await.unwrap();
        let settings = Arc::new(crate::config::Settings::default());
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

    #[test]
    fn validates_internet_radio_fields_and_schemes() {
        let valid = HashMap::from([
            ("name".into(), "  Radio One  ".into()),
            ("streamUrl".into(), " https://radio.example/live ".into()),
            ("homepageUrl".into(), "https://radio.example/".into()),
        ]);
        assert_eq!(
            validated_radio_fields(&valid).unwrap(),
            (
                "Radio One".into(),
                "https://radio.example/live".into(),
                "https://radio.example/".into(),
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
        assert!(
            internet_radio::proxy_enabled(&state.db, &created.id)
                .await
                .unwrap()
        );

        let error = json_endpoint(
            &state,
            &admin,
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
        let result = search(&state, "search3", &params).await.unwrap();
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

        let covers = artist_cover_art_map(&state, &["artist-1".into(), "artist-2".into()], None)
            .await
            .unwrap();
        assert_eq!(covers["artist-1"], "img-album-1");
        assert_eq!(covers["artist-2"], "img-album-1");

        let response = artists(&state, &HashMap::new()).await.unwrap();
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
        let (start, value) = parse_lrc_line("[01:02.50]Line").unwrap();
        assert_eq!(start, 62_500);
        assert_eq!(value, "Line");
        assert!(parse_lrc_line("[ar:Artist]").is_none());
    }

    #[test]
    fn exposes_stable_integer_music_folder_ids() {
        let id = folder_api_id("88133187-fa8c-461d-b00c-631703004590");
        assert_eq!(id, folder_api_id("88133187-fa8c-461d-b00c-631703004590"));
        assert!((1..=i32::MAX).contains(&id));
        assert_eq!(folder_api_id("42"), 42);
    }

    #[tokio::test]
    async fn exposes_extensions_without_authentication() {
        let state = test_state().await;
        let response = request_json(
            router().with_state(state),
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
        assert!(!names.contains(&"playbackReport"));
    }

    #[tokio::test]
    async fn authenticates_api_keys_without_username() {
        let state = test_state().await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let uri = format!(
            "/rest/ping?apiKey={}&v=1.16.1&c=test&f=json",
            user.subsonic_token
        );
        let response = request_json(router().with_state(state), &uri).await;
        assert_eq!(response["subsonic-response"]["status"], "ok");
    }

    #[tokio::test]
    async fn uses_the_standard_album_info_response_key_for_both_endpoints() {
        let state = test_state().await;
        let user = user_entity::Entity::find()
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        let value = json_endpoint(&state, &user, "getAlbumInfo2", &HashMap::new())
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
