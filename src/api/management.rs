use std::{
    collections::HashMap,
    convert::Infallible,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use base64::Engine;
use futures::{Stream, StreamExt, stream};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder,
    QuerySelect, Set, TransactionTrait,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command};
use tokio_util::{io::ReaderStream, sync::CancellationToken};

use crate::{
    AppState,
    auth::{
        AdminUser, AuthUser, authenticate_password_with_subsonic, cookie_value, decode_token,
        decrypt_server_secret, encrypt_server_secret, issue_tokens, user_by_id,
    },
    entities::{app_setting, download_source, internet_radio_station, job, music_folder, track},
    jobs::{self, AutoTagPayload, ScanPayload},
    lastfm,
    models::{ApiResponse, MusicFolder},
    network,
    remote_download::{self, RemoteConnection, RemoteImportRequest, RemoteSearchRequest},
    scanner,
    tags::{ARTWORK_CACHE_CONTROL, AUDIO_EXTENSIONS, AudioMetadata, detect_artwork_mime},
};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/token/", post(login))
        .route("/api/token/refresh/", post(refresh_session))
        .route("/api/logout/", post(logout))
        .route("/api/file_list/", post(file_list))
        .route("/api/music_id3/", post(music_id3))
        .route("/api/music_artwork/", get(music_artwork))
        .route("/api/update_id3/", post(update_id3))
        .route("/api/batch_update_id3/", post(batch_update_id3))
        .route("/api/batch_auto_update_id3/", post(batch_auto_update_id3))
        .route("/api/fetch_id3_by_title/", post(fetch_id3_by_title))
        .route("/api/fetch_lyric/", post(fetch_lyric))
        .route("/api/translation_lyc/", post(translation_lyc))
        .route("/api/tidy_folder/", post(tidy_folder))
        .route("/api/upload_image/", post(upload_image))
        .route("/api/record/", get(job_records))
        .route("/api/events/jobs/", get(job_events))
        .route("/api/scan/", post(start_scan))
        .route("/api/config/status/", get(config_status))
        .route("/api/config/preferences/", post(save_preferences))
        .route("/api/lastfm/config/", post(save_lastfm_config))
        .route("/api/lastfm/status/", get(lastfm_status))
        .route("/api/lastfm/auth/start/", post(start_lastfm_auth))
        .route("/api/lastfm/auth/complete/", post(complete_lastfm_auth))
        .route("/api/lastfm/disconnect/", post(disconnect_lastfm))
        .route(
            "/api/library_roots/",
            get(library_roots).post(add_library_root),
        )
        .route("/api/library_roots/update/", post(update_library_root))
        .route("/api/library_roots/delete/", post(delete_library_root))
        .route(
            "/api/download_sources/",
            get(download_sources).post(save_download_source),
        )
        .route(
            "/api/download_sources/delete/",
            post(delete_download_source),
        )
        .route("/api/remote_download/search/", post(remote_download_search))
        .route("/api/internet_radio_stream/", get(internet_radio_stream))
        .route(
            "/api/remote_download/preview/",
            get(remote_download_preview),
        )
        .route("/api/remote_download/import/", post(remote_download_import))
        .route(
            "/api/remote_download/upload/",
            post(upload_song).layer(DefaultBodyLimit::max(
                jobs::MAX_IMPORT_BYTES as usize + 1024 * 1024,
            )),
        )
        .route(
            "/api/download_sources/netease/login/start/",
            post(netease_login_start),
        )
        .route(
            "/api/download_sources/netease/login/check/",
            post(netease_login_check),
        )
        .route("/api/events/netease-login/", get(netease_login_events))
        .route("/user/info/", get(user_info))
}

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    match state.db.ping().await {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({"status":"ok","version":crate::VERSION})),
        ),
        Err(error) => {
            tracing::error!(%error, "health check database ping failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status":"error","message":"database unavailable"})),
            )
        }
    }
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let user = authenticate_password_with_subsonic(
        &state.db,
        &request.username,
        &request.password,
        &state.settings.auth.jwt_secret,
    )
    .await?
    .ok_or_else(|| ApiError::unauthorized("用户名或密码错误"))?;
    let (token, refresh) = issue_tokens(
        &user,
        &state.settings.auth.jwt_secret,
        state.settings.auth.access_token_minutes,
        state.settings.auth.refresh_token_days,
    )?;
    let secure = state
        .settings
        .server
        .public_url
        .as_deref()
        .is_some_and(|url| url.starts_with("https://"));
    session_response(
        json!({"token": token, "access": token, "refresh": refresh, "user": {"username": user.username, "role": user.role}}),
        &token,
        &refresh,
        state.settings.auth.access_token_minutes * 60,
        state.settings.auth.refresh_token_days * 86_400,
        secure,
    )
}

async fn refresh_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let refresh = cookie_value(&headers, "mNest_refresh")
        .ok_or_else(|| ApiError::unauthorized("缺少刷新令牌"))?;
    let claims = decode_token(refresh, &state.settings.auth.jwt_secret)
        .map_err(|_| ApiError::unauthorized("刷新令牌无效或已过期"))?;
    if claims.kind != "refresh" {
        return Err(ApiError::unauthorized("刷新令牌类型无效"));
    }
    let user = user_by_id(&state.db, &claims.sub)
        .await?
        .ok_or_else(|| ApiError::unauthorized("用户不存在"))?;
    let (access, refresh) = issue_tokens(
        &user,
        &state.settings.auth.jwt_secret,
        state.settings.auth.access_token_minutes,
        state.settings.auth.refresh_token_days,
    )?;
    let secure = state
        .settings
        .server
        .public_url
        .as_deref()
        .is_some_and(|url| url.starts_with("https://"));
    session_response(
        json!({"access": access, "user": {"username": user.username, "role": user.role}}),
        &access,
        &refresh,
        state.settings.auth.access_token_minutes * 60,
        state.settings.auth.refresh_token_days * 86_400,
        secure,
    )
}

async fn logout() -> Result<Response, ApiError> {
    let mut response = Json(json!({"result": true, "message": "success"})).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static("mNest_access=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"),
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "mNest_refresh=; HttpOnly; SameSite=Lax; Path=/api/token/refresh/; Max-Age=0",
        ),
    );
    Ok(response)
}

fn session_response(
    body: Value,
    access: &str,
    refresh: &str,
    access_age: i64,
    refresh_age: i64,
    secure: bool,
) -> Result<Response, ApiError> {
    let secure = if secure { "; Secure" } else { "" };
    let access_cookie = format!(
        "mNest_access={access}; HttpOnly; SameSite=Lax; Path=/; Max-Age={access_age}{secure}"
    );
    let refresh_cookie = format!(
        "mNest_refresh={refresh}; HttpOnly; SameSite=Lax; Path=/api/token/refresh/; Max-Age={refresh_age}{secure}"
    );
    let mut response = Json(body).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&access_cookie)
            .map_err(|error| anyhow::anyhow!("invalid access cookie: {error}"))?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&refresh_cookie)
            .map_err(|error| anyhow::anyhow!("invalid refresh cookie: {error}"))?,
    );
    Ok(response)
}

async fn user_info(AuthUser(user): AuthUser) -> Json<ApiResponse<Value>> {
    Json(ApiResponse::success(
        json!({"username": user.username, "role": user.role, "email": user.email}),
    ))
}

#[derive(Deserialize)]
struct FileListRequest {
    file_path: String,
    #[serde(default)]
    sorted_fields: Vec<String>,
}

async fn file_list(
    State(state): State<AppState>,
    _user: AdminUser,
    Json(request): Json<FileListRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let path = allowed_path(&state, &request.file_path).await?;
    let mut entries = Vec::new();
    let mut audio_paths = Vec::new();
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let extension = entry_path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        if !metadata.is_dir() && !AUDIO_EXTENSIONS.contains(&extension.as_str()) {
            continue;
        }
        let mtime = metadata
            .modified()
            .ok()
            .map(chrono::DateTime::<chrono::Local>::from)
            .map(|v| v.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();
        let indexed_path = (!metadata.is_dir()).then(|| entry_path.to_string_lossy().into_owned());
        if let Some(indexed_path) = indexed_path.as_ref() {
            audio_paths.push(indexed_path.clone());
        }
        entries.push((
            indexed_path,
            json!({
                "id": entries.len() + 1,
                "name": name,
                "title": name,
                "icon": if metadata.is_dir() { "icon-folder" } else { "icon-script-file" },
                "state": "null",
                "children": if metadata.is_dir() { json!([]) } else { Value::Null },
                "size": metadata.len(),
                "update_time": mtime,
                "needs_scrape": false,
            }),
        ));
    }
    let mut scrape_flags = HashMap::new();
    for chunk in audio_paths.chunks(500) {
        let rows = track::Entity::find()
            .select_only()
            .column(track::Column::Path)
            .column(track::Column::NeedsScrape)
            .filter(track::Column::Path.is_in(chunk.to_vec()))
            .into_tuple::<(String, i64)>()
            .all(&state.db)
            .await?;
        scrape_flags.extend(rows);
    }
    let mut entries = entries
        .into_iter()
        .map(|(path, mut value)| {
            if let Some(path) = path {
                value["needs_scrape"] =
                    json!(scrape_flags.get(&path).is_none_or(|value| *value != 0));
            }
            value
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| {
        if request.sorted_fields.contains(&"size".to_owned()) {
            b["size"].as_u64().cmp(&a["size"].as_u64())
        } else if request.sorted_fields.contains(&"update_time".to_owned()) {
            b["update_time"].as_str().cmp(&a["update_time"].as_str())
        } else {
            a["name"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or_default().to_lowercase())
        }
    });
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    Ok(Json(ApiResponse::success(
        json!([{"name":name,"title":name,"expanded":true,"id":0,"children":entries,"icon":"icon-folder"}]),
    )))
}

#[derive(Deserialize)]
struct MusicId3Request {
    file_path: String,
    file_name: String,
}

async fn music_id3(
    State(state): State<AppState>,
    _user: AdminUser,
    Json(request): Json<MusicId3Request>,
) -> Result<Json<ApiResponse<AudioMetadata>>, ApiError> {
    let path = allowed_path(
        &state,
        &Path::new(&request.file_path).join(&request.file_name),
    )
    .await?;
    let tags = state.tags.clone();
    let metadata = tokio::task::spawn_blocking(move || tags.read_without_artwork(&path)).await??;
    Ok(Json(ApiResponse::success(metadata)))
}

async fn music_artwork(
    State(state): State<AppState>,
    _user: AdminUser,
    Query(request): Query<MusicId3Request>,
) -> Result<Response, ApiError> {
    let path = allowed_path(
        &state,
        &Path::new(&request.file_path).join(&request.file_name),
    )
    .await?;
    let tags = state.tags.clone();
    let artwork = tokio::task::spawn_blocking(move || tags.read_artwork(&path)).await??;
    let Some(artwork) = artwork else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let content_length = artwork.data.len();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, artwork.mime_type)
        .header(header::CONTENT_LENGTH, content_length)
        .header(header::CACHE_CONTROL, ARTWORK_CACHE_CONTROL)
        .header("x-content-type-options", "nosniff")
        .body(Body::from(artwork.data))
        .map_err(anyhow::Error::from)?)
}

#[derive(Deserialize)]
struct UpdateRequest {
    music_id3_info: Vec<AudioMetadata>,
}

async fn update_id3(
    State(state): State<AppState>,
    _user: AdminUser,
    Json(request): Json<UpdateRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let mut results = Vec::new();
    let mut updated_paths = Vec::new();
    for mut metadata in request.music_id3_info {
        materialize_remote_image(&mut metadata).await?;
        let path = allowed_path(&state, &metadata.file_full_path).await?;
        let tags = state.tags.clone();
        let write_path = path.clone();
        let result =
            tokio::task::spawn_blocking(move || tags.write(&write_path, &metadata)).await?;
        match result {
            Ok(updated_path) => {
                updated_paths.push((path.clone(), updated_path.clone()));
                results.push(json!({"path":updated_path,"success":true}));
            }
            Err(error) => {
                results.push(json!({"path":path,"success":false,"message":error.to_string()}))
            }
        }
    }
    scanner::refresh_path_changes(&state.db, state.tags.clone(), &updated_paths).await?;
    scanner::clear_needs_scrape(
        &state.db,
        updated_paths.iter().map(|(_, current)| current.clone()),
    )
    .await?;
    let success = results.iter().all(|v| v["success"] == true);
    Ok(Json(if success {
        ApiResponse::success(json!(results))
    } else {
        ApiResponse::failure(json!(results), "部分文件修改失败")
    }))
}

#[derive(Debug, Clone, Deserialize)]
struct SelectedFile {
    name: String,
    #[serde(default)]
    icon: String,
}

#[derive(Deserialize)]
struct BatchRequest {
    file_full_path: String,
    select_data: Vec<SelectedFile>,
    music_info: Value,
}

async fn batch_update_id3(
    State(state): State<AppState>,
    _user: AdminUser,
    Json(request): Json<BatchRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let paths = expand_selection(&state, &request.file_full_path, &request.select_data).await?;
    let mut results = Vec::new();
    let mut updated_paths = Vec::new();
    for path in paths {
        let tags = state.tags.clone();
        let read_path = path.clone();
        let mut metadata = tokio::task::spawn_blocking(move || tags.read(&read_path)).await??;
        apply_patch(&mut metadata, &request.music_info);
        materialize_remote_image(&mut metadata).await?;
        metadata.file_full_path = path.to_string_lossy().into_owned();
        let tags = state.tags.clone();
        let write_path = path.clone();
        match tokio::task::spawn_blocking(move || tags.write(&write_path, &metadata)).await? {
            Ok(new_path) => {
                updated_paths.push((path.clone(), new_path.clone()));
                results.push(json!({"path":new_path,"success":true}));
            }
            Err(error) => {
                results.push(json!({"path":path,"success":false,"message":error.to_string()}))
            }
        }
    }
    scanner::refresh_path_changes(&state.db, state.tags.clone(), &updated_paths).await?;
    scanner::clear_needs_scrape(
        &state.db,
        updated_paths.iter().map(|(_, current)| current.clone()),
    )
    .await?;
    let success = results.iter().all(|value| value["success"] == true);
    Ok(Json(if success {
        ApiResponse::success(json!(results))
    } else {
        ApiResponse::failure(json!(results), "部分文件修改失败")
    }))
}

async fn batch_auto_update_id3(
    State(state): State<AppState>,
    _user: AdminUser,
    Json(request): Json<BatchRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let paths = expand_selection(&state, &request.file_full_path, &request.select_data).await?;
    let sources = request
        .music_info
        .get("source_list")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mode = request
        .music_info
        .get("select_mode")
        .and_then(Value::as_str)
        .unwrap_or("hard")
        .to_owned();
    let id = jobs::enqueue(
        &state,
        "auto_tag",
        &AutoTagPayload {
            paths,
            sources,
            mode,
        },
    )
    .await?;
    Ok(Json(ApiResponse::success(json!({"job_id":id}))))
}

#[derive(Deserialize)]
struct SearchRequest {
    title: String,
    resource: String,
    #[serde(default)]
    full_path: String,
}
async fn fetch_id3_by_title(
    State(state): State<AppState>,
    _user: AdminUser,
    Json(request): Json<SearchRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if request.resource == "acoustid" {
        let path = allowed_path(&state, &request.full_path).await?;
        return Ok(Json(ApiResponse::success(json!(
            state.providers.fingerprint(&path).await?
        ))));
    }
    let (artist, album) = if request.full_path.is_empty() {
        (String::new(), String::new())
    } else {
        let path = allowed_path(&state, &request.full_path).await?;
        let tags = state.tags.clone();
        let metadata = tokio::task::spawn_blocking(move || tags.read(&path)).await??;
        (metadata.artist, metadata.album)
    };
    Ok(Json(ApiResponse::success(json!(
        state
            .providers
            .search(&request.resource, &request.title, &artist, &album)
            .await?
    ))))
}

#[derive(Deserialize)]
struct LyricRequest {
    song_id: String,
    resource: String,
}
async fn fetch_lyric(
    State(state): State<AppState>,
    _user: AdminUser,
    Json(request): Json<LyricRequest>,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    Ok(Json(ApiResponse::success(
        state
            .providers
            .lyrics(&request.resource, &request.song_id)
            .await
            .unwrap_or_default(),
    )))
}

#[derive(Deserialize)]
struct TranslationRequest {
    lyc: String,
}
async fn translation_lyc(
    _user: AdminUser,
    Json(request): Json<TranslationRequest>,
) -> Json<ApiResponse<String>> {
    Json(ApiResponse::success(request.lyc))
}

#[derive(Deserialize)]
struct TidyRequest {
    root_path: String,
    first_dir: String,
    #[serde(default)]
    second_dir: String,
    file_full_path: String,
    select_data: Vec<SelectedFile>,
}
async fn tidy_folder(
    State(state): State<AppState>,
    _user: AdminUser,
    Json(request): Json<TidyRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let root = allowed_path(&state, &request.root_path).await?;
    if !root.is_dir() {
        return Err(ApiError::bad_request("整理目标必须是曲库目录"));
    }
    let paths = expand_selection(&state, &request.file_full_path, &request.select_data).await?;
    let mut planned = Vec::new();
    let mut target_paths = std::collections::HashSet::new();
    for path in paths {
        let tags = state.tags.clone();
        let read_path = path.clone();
        let metadata = tokio::task::spawn_blocking(move || tags.read(&read_path)).await??;
        let first = render_template(&request.first_dir, &metadata);
        let mut target_dir = root.join(safe_component(&first));
        if !request.second_dir.is_empty() {
            target_dir = target_dir.join(safe_component(&render_template(
                &request.second_dir,
                &metadata,
            )));
        }
        std::fs::create_dir_all(&target_dir)?;
        let target_dir = target_dir.canonicalize()?;
        if !target_dir.starts_with(&root) {
            return Err(ApiError::forbidden("整理目标不能离开曲库目录"));
        }
        let target = target_dir.join(path.file_name().unwrap_or_default());
        if target != path && target.exists() {
            return Err(ApiError::bad_request(format!(
                "整理目标已存在：{}",
                target.display()
            )));
        }
        if !target_paths.insert(target.clone()) {
            return Err(ApiError::bad_request(format!(
                "多首歌曲会写入同一目标：{}",
                target.display()
            )));
        }
        planned.push((path, target));
    }
    let mut completed: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (source, target) in &planned {
        if source == target {
            continue;
        }
        if let Err(error) = std::fs::rename(source, target) {
            for (previous, current) in completed.iter().rev() {
                let _ = std::fs::rename(current, previous);
            }
            return Err(error.into());
        }
        completed.push((source.clone(), target.clone()));
    }
    if let Err(error) = scanner::refresh_path_changes(&state.db, state.tags.clone(), &planned).await
    {
        for (previous, current) in completed.iter().rev() {
            let _ = std::fs::rename(current, previous);
        }
        let reversed = completed
            .iter()
            .map(|(previous, current)| (current.clone(), previous.clone()))
            .collect::<Vec<_>>();
        let _ = scanner::refresh_path_changes(&state.db, state.tags.clone(), &reversed).await;
        return Err(error.into());
    }
    let moved = planned
        .into_iter()
        .map(|(from, to)| json!({"from":from,"to":to}))
        .collect::<Vec<_>>();
    Ok(Json(ApiResponse::success(json!(moved))))
}

async fn upload_image(
    _user: AdminUser,
    mut multipart: Multipart,
) -> Result<Json<ApiResponse<String>>, ApiError> {
    while let Some(field) = multipart.next_field().await? {
        if field.name() == Some("upload_file") {
            let bytes = field.bytes().await?;
            if bytes.len() > 5 * 1024 * 1024 {
                return Err(ApiError::bad_request("图片不能超过 5MB"));
            }
            let content_type = detect_artwork_mime(&bytes)
                .ok_or_else(|| ApiError::bad_request("只支持有效的栅格图片"))?;
            return Ok(Json(ApiResponse::success(format!(
                "data:{content_type};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ))));
        }
    }
    Err(ApiError::bad_request("缺少 upload_file"))
}

#[derive(Clone, Default, Deserialize)]
struct JobQuery {
    state: Option<String>,
    page: Option<i64>,
    page_size: Option<i64>,
}
async fn job_records(
    State(state): State<AppState>,
    _user: AdminUser,
    Query(query): Query<JobQuery>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    Ok(Json(ApiResponse::success(
        fetch_job_page(&state, &query).await?,
    )))
}

async fn fetch_job_page(state: &AppState, query: &JobQuery) -> Result<Value, ApiError> {
    let page = query.page.unwrap_or(1).max(1);
    let size = query.page_size.unwrap_or(20).clamp(1, 200);
    let mut request = job::Entity::find().order_by_desc(job::Column::CreatedAt);
    if let Some(job_state) = &query.state {
        request = request.filter(job::Column::State.eq(job_state));
    }
    let jobs = request
        .offset(((page - 1) * size) as u64)
        .limit(size as u64)
        .all(&state.db)
        .await?;
    let items = jobs.into_iter().map(job_summary).collect::<Vec<_>>();
    Ok(json!({"page":page,"items":items}))
}

fn job_summary(job: job::Model) -> Value {
    json!({
        "id": job.id,
        "kind": job.kind,
        "state": job.state,
        "progress": job.progress,
        "message": job.message,
        "attempts": job.attempts,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
    })
}

async fn job_events(
    State(state): State<AppState>,
    _user: AdminUser,
    Query(query): Query<JobQuery>,
) -> Response {
    let receiver = state.events.subscribe_jobs();
    let shutdown = state.shutdown.clone();
    let events = stream::unfold(
        (receiver, state, query, true),
        |(mut receiver, state, query, initial)| async move {
            if !initial {
                match receiver.recv().await {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
                tokio::time::sleep(Duration::from_millis(75)).await;
                while receiver.try_recv().is_ok() {}
            }

            let event = match fetch_job_page(&state, &query).await {
                Ok(value) => Event::default()
                    .event("jobs")
                    .json_data(value)
                    .unwrap_or_else(|error| {
                        Event::default().event("jobs-error").data(error.to_string())
                    }),
                Err(error) => Event::default().event("jobs-error").data(error.message),
            };
            Some((
                Ok::<Event, Infallible>(event),
                (receiver, state, query, false),
            ))
        },
    );
    sse_response(events, shutdown)
}

fn sse_response<S>(events: S, shutdown: CancellationToken) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    let mut response = Sse::new(events.take_until(shutdown.cancelled_owned()))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response.headers_mut().insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

async fn start_scan(
    State(state): State<AppState>,
    _user: AdminUser,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let id = jobs::enqueue(&state, "scan", &ScanPayload {}).await?;
    Ok(Json(ApiResponse::success(json!({"job_id":id}))))
}

#[derive(Deserialize)]
struct SaveDownloadSourceRequest {
    id: Option<String>,
    kind: String,
    name: String,
    base_url: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct DeleteDownloadSourceRequest {
    id: String,
}

#[derive(Deserialize)]
struct NeteaseLoginRequest {
    source_id: String,
    key: Option<String>,
}

#[derive(Deserialize)]
struct RemotePreviewQuery {
    source_id: String,
    song_id: String,
}

#[derive(Deserialize)]
struct InternetRadioStreamQuery {
    id: String,
}

#[derive(Deserialize)]
struct UploadSongQuery {
    root_id: String,
    #[serde(default)]
    directory: String,
}

#[derive(Deserialize)]
struct SavePreferencesRequest {
    download_filename_format: String,
}

#[derive(Deserialize)]
struct SaveLastFmConfigRequest {
    api_key: String,
    #[serde(default)]
    shared_secret: String,
}

async fn download_sources(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    let sources = download_source::Entity::find()
        .order_by_asc(download_source::Column::Kind)
        .order_by_asc(download_source::Column::Name)
        .all(&state.db)
        .await?;
    Ok(Json(ApiResponse::success(Value::Array(
        sources
            .into_iter()
            .map(|source| {
                json!({
                    "id": source.id,
                    "kind": source.kind,
                    "name": source.name,
                    "base_url": source.base_url,
                    "username": source.username,
                    "has_password": !source.password.is_empty(),
                    "has_cookie": !source.cookie.is_empty(),
                    "account_name": source.account_name,
                    "enabled": source.enabled != 0,
                })
            })
            .collect(),
    ))))
}

async fn save_download_source(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(request): Json<SaveDownloadSourceRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    if !matches!(request.kind.as_str(), "netease" | "qq" | "qq2" | "subsonic") {
        return Err(ApiError::bad_request("下载来源类型无效"));
    }
    let url = reqwest::Url::parse(request.base_url.trim())
        .map_err(|_| ApiError::bad_request("后端地址无效"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("后端地址只支持 HTTP 或 HTTPS"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ApiError::bad_request("后端地址不能包含 URL 凭据"));
    }
    if request.name.trim().is_empty() {
        return Err(ApiError::bad_request("来源名称不能为空"));
    }
    if request.kind == "subsonic" && request.username.trim().is_empty() {
        return Err(ApiError::bad_request("Subsonic 用户名不能为空"));
    }
    let now = chrono::Utc::now().to_rfc3339();
    let existing = if let Some(id) = request.id.as_deref() {
        download_source::Entity::find_by_id(id)
            .one(&state.db)
            .await?
    } else if request.kind == "subsonic" {
        None
    } else {
        download_source::Entity::find()
            .filter(download_source::Column::Kind.eq(&request.kind))
            .one(&state.db)
            .await?
    };
    let id = if let Some(existing) = existing {
        let id = existing.id.clone();
        let mut active = existing.into_active_model();
        active.kind = Set(request.kind);
        active.name = Set(request.name.trim().to_owned());
        active.base_url = Set(request.base_url.trim_end_matches('/').to_owned());
        active.username = Set(request.username.trim().to_owned());
        if !request.password.is_empty() {
            active.password = Set(seal_download_secret(
                &state,
                &id,
                "password",
                &request.password,
            )?);
        }
        active.enabled = Set(i64::from(request.enabled.unwrap_or(true)));
        active.updated_at = Set(now);
        active.update(&state.db).await?;
        id
    } else {
        let id = Uuid::new_v4().to_string();
        download_source::ActiveModel {
            id: Set(id.clone()),
            kind: Set(request.kind),
            name: Set(request.name.trim().to_owned()),
            base_url: Set(request.base_url.trim_end_matches('/').to_owned()),
            username: Set(request.username.trim().to_owned()),
            password: Set(seal_download_secret(
                &state,
                &id,
                "password",
                &request.password,
            )?),
            cookie: Set(String::new()),
            account_name: Set(String::new()),
            enabled: Set(i64::from(request.enabled.unwrap_or(true))),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(&state.db)
        .await?;
        id
    };
    Ok(Json(ApiResponse::success(json!({"id": id}))))
}

async fn delete_download_source(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(request): Json<DeleteDownloadSourceRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    download_source::Entity::delete_by_id(request.id)
        .exec(&state.db)
        .await?;
    Ok(Json(ApiResponse::success(json!([]))))
}

async fn remote_download_search(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(request): Json<RemoteSearchRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    let source = load_download_source(&state, &request.source_id).await?;
    let connection = remote_connection(&source);
    let songs = remote_download::search(&connection, &request.query)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(ApiResponse::success(json!(songs))))
}

async fn remote_download_preview(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    Query(request): Query<RemotePreviewQuery>,
) -> Result<Response, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    let source = load_download_source(&state, &request.source_id).await?;
    let connection = remote_connection(&source);
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let upstream = remote_download::preview_stream(&connection, &request.song_id, range)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .map_err(|_| ApiError::bad_request("远程试听返回了无效状态码"))?;
    let upstream_headers = upstream.headers().clone();
    let stream = upstream
        .bytes_stream()
        .take_until(state.shutdown.clone().cancelled_owned());
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
    ] {
        if let Some(value) = upstream_headers
            .get(&name)
            .and_then(|value| value.to_str().ok())
            && let Ok(value) = HeaderValue::from_str(value)
        {
            response.headers_mut().insert(name, value);
        }
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

async fn internet_radio_stream(
    State(state): State<AppState>,
    _user: AuthUser,
    headers: HeaderMap,
    Query(request): Query<InternetRadioStreamQuery>,
) -> Result<Response, ApiError> {
    if request.id.trim().is_empty() || request.id.len() > 128 {
        return Err(ApiError::bad_request("网络电台 ID 无效"));
    }
    let station = internet_radio_station::Entity::find_by_id(&request.id)
        .one(&state.db)
        .await?
        .ok_or_else(|| ApiError::bad_request("网络电台不存在"))?;
    let url = reqwest::Url::parse(station.stream_url.trim())
        .map_err(|_| ApiError::bad_request("网络电台流地址无效"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ApiError::bad_request(
            "网络电台流地址必须使用 HTTP 或 HTTPS",
        ));
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(12))
        .user_agent("mNest/internet-radio")
        .build()
        .map_err(|error| ApiError::bad_gateway(error.to_string()))?;
    let mut upstream_request = client.get(url).header("Icy-MetaData", "0");
    if let Some(range) = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
    {
        upstream_request = upstream_request.header(reqwest::header::RANGE, range);
    }
    let upstream = tokio::time::timeout(Duration::from_secs(20), upstream_request.send())
        .await
        .map_err(|_| ApiError::bad_gateway("连接网络电台超时"))?
        .map_err(|error| ApiError::bad_gateway(error.without_url().to_string()))?;
    if !upstream.status().is_success() {
        return Err(ApiError::bad_gateway(format!(
            "网络电台返回 HTTP {}",
            upstream.status().as_u16()
        )));
    }
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if is_hls_radio_stream(upstream.url(), content_type) {
        let url = upstream.url().clone();
        drop(upstream);
        return transcode_hls_radio(&state, &url).await;
    }
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .map_err(|_| ApiError::bad_gateway("网络电台返回了无效状态码"))?;
    let upstream_headers = upstream.headers().clone();
    let stream = upstream
        .bytes_stream()
        .take_until(state.shutdown.clone().cancelled_owned());
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
    ] {
        if let Some(value) = upstream_headers
            .get(&name)
            .and_then(|value| value.to_str().ok())
            && let Ok(value) = HeaderValue::from_str(value)
        {
            response.headers_mut().insert(name, value);
        }
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

fn is_hls_radio_stream(url: &reqwest::Url, content_type: Option<&str>) -> bool {
    if url.path().to_ascii_lowercase().ends_with(".m3u8") {
        return true;
    }
    matches!(
        content_type
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "application/vnd.apple.mpegurl"
                | "application/x-mpegurl"
                | "application/mpegurl"
                | "audio/mpegurl"
                | "audio/x-mpegurl"
        )
    )
}

async fn transcode_hls_radio(state: &AppState, url: &reqwest::Url) -> Result<Response, ApiError> {
    let mut command = Command::new(&state.settings.tools.ffmpeg);
    command.kill_on_drop(true);
    command
        .args([
            "-nostdin",
            "-v",
            "error",
            "-user_agent",
            "mNest/internet-radio",
            "-i",
        ])
        .arg(url.as_str())
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-c:a",
            "libmp3lame",
            "-b:a",
            "128k",
            "-f",
            "mp3",
            "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| ApiError::bad_gateway(format!("网络电台 HLS 转码启动失败：{error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::bad_gateway("网络电台 HLS 转码输出不可用"))?;
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
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

async fn remote_download_import(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(request): Json<RemoteImportRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    let source = load_download_source(&state, &request.source_id).await?;
    let connection = remote_connection(&source);
    let filename_format = download_filename_format(&state).await?;
    let payload = remote_download::prepare_import(&connection, &request, &filename_format)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let id = jobs::enqueue(&state, "remote_import", &payload).await?;
    Ok(Json(ApiResponse::success(json!({"job_id": id}))))
}

async fn upload_song(
    State(state): State<AppState>,
    _user: AdminUser,
    Query(request): Query<UploadSongQuery>,
    mut multipart: Multipart,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if request.root_id.trim().is_empty() || request.root_id.len() > 128 {
        return Err(ApiError::bad_request("目标曲库 ID 无效"));
    }
    if request.directory.len() > 1024 || request.directory.split(['/', '\\']).count() > 16 {
        return Err(ApiError::bad_request("上传目录过长"));
    }
    if Path::new(&request.directory).is_absolute() {
        return Err(ApiError::bad_request("曲库内目录必须是相对路径"));
    }
    let root = music_folder::Entity::find_by_id(&request.root_id)
        .filter(music_folder::Column::Enabled.eq(1))
        .one(&state.db)
        .await?
        .ok_or_else(|| ApiError::bad_request("目标曲库不存在或已停用"))?;
    let root = tokio::fs::canonicalize(root.path).await?;
    let mut directory = root.clone();
    for component in request.directory.split(['/', '\\']) {
        let component = component.trim();
        if component.is_empty() {
            continue;
        }
        if matches!(component, "." | "..") {
            return Err(ApiError::bad_request("曲库内目录不能包含 . 或 .."));
        }
        directory.push(remote_download::safe_component(component));
    }
    tokio::fs::create_dir_all(&directory).await?;
    let directory = tokio::fs::canonicalize(directory).await?;
    if !directory.starts_with(&root) {
        return Err(ApiError::forbidden("上传目录不能离开目标曲库"));
    }

    while let Some(mut field) = multipart.next_field().await? {
        if field.name() != Some("upload_file") {
            continue;
        }
        let original_name = field
            .file_name()
            .map(str::to_owned)
            .ok_or_else(|| ApiError::bad_request("上传文件缺少文件名"))?;
        let base_name = original_name
            .split(['/', '\\'])
            .next_back()
            .unwrap_or_default()
            .trim();
        let filename = uploaded_audio_filename(base_name)?;
        let destination = directory.join(filename);
        let partial = directory.join(format!(".upload-{}.part", Uuid::new_v4().simple()));
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .await?;
        let write_result: Result<(), ApiError> = async {
            let mut uploaded = 0u64;
            while let Some(chunk) = field.chunk().await? {
                uploaded = uploaded
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| ApiError::payload_too_large("上传音频大小溢出"))?;
                if uploaded > jobs::MAX_IMPORT_BYTES {
                    return Err(ApiError::payload_too_large("上传音频超过 2GiB 限制"));
                }
                file.write_all(&chunk).await?;
            }
            if uploaded == 0 {
                return Err(ApiError::bad_request("上传音频不能为空"));
            }
            file.flush().await?;
            file.sync_all().await?;
            Ok(())
        }
        .await;
        drop(file);
        if let Err(error) = write_result {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(error);
        }

        let committed = match jobs::commit_download(&partial, &destination).await {
            Ok(path) => path,
            Err(error) => {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(error.into());
            }
        };
        let indexed = scanner::refresh_paths(
            &state.db,
            state.tags.clone(),
            std::slice::from_ref(&committed),
        )
        .await;
        match indexed {
            Ok(1) => {
                let filename = committed
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_owned();
                return Ok(Json(ApiResponse::success(json!({
                    "path": committed,
                    "filename": filename,
                }))));
            }
            Ok(_) => {
                let _ = tokio::fs::remove_file(&committed).await;
                let _ = jobs::remove_import_index(&state, &committed).await;
                return Err(ApiError::bad_request("上传文件未能加入曲库索引"));
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&committed).await;
                if let Err(cleanup_error) = jobs::remove_import_index(&state, &committed).await {
                    tracing::warn!(path = %committed.display(), %cleanup_error, "failed to roll back uploaded song index");
                }
                return Err(ApiError::bad_request(format!(
                    "上传文件无法加入曲库：{error}"
                )));
            }
        }
    }
    Err(ApiError::bad_request("缺少 upload_file"))
}

fn uploaded_audio_filename(original_name: &str) -> Result<String, ApiError> {
    let original_path = Path::new(original_name);
    let extension = original_path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|value| AUDIO_EXTENSIONS.contains(&value.as_str()))
        .ok_or_else(|| ApiError::bad_request("只支持曲库可识别的音频文件"))?;
    let stem = original_path
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("上传文件名无效"))?;
    Ok(format!(
        "{}.{}",
        remote_download::safe_component(stem),
        extension
    ))
}

async fn netease_login_start(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(request): Json<NeteaseLoginRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    let mut source = load_download_source(&state, &request.source_id).await?;
    let connection = remote_connection(&source);
    let login = remote_download::netease_login_start(&connection)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    if !login.cookie.is_empty() {
        let mut active = source.clone().into_active_model();
        active.cookie = Set(seal_download_secret(
            &state,
            &source.id,
            "cookie",
            &login.cookie,
        )?);
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        source = active.update(&state.db).await?;
    }
    Ok(Json(ApiResponse::success(json!({
        "key": login.key,
        "qr_image": login.qr_image,
        "has_cookie": !source.cookie.is_empty(),
    }))))
}

async fn netease_login_check(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(request): Json<NeteaseLoginRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    Ok(Json(ApiResponse::success(
        check_netease_login(&state, &request).await?,
    )))
}

async fn check_netease_login(
    state: &AppState,
    request: &NeteaseLoginRequest,
) -> Result<Value, ApiError> {
    let source = load_download_source(state, &request.source_id).await?;
    let connection = remote_connection(&source);
    let status = remote_download::netease_login_check(
        &connection,
        request.key.as_deref().unwrap_or_default(),
    )
    .await
    .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let account_name = if status.code == 803 {
        let mut authenticated = connection;
        if !status.cookie.is_empty() {
            authenticated.cookie = status.cookie.clone();
        }
        remote_download::netease_account_name(&authenticated)
            .await
            .unwrap_or_default()
    } else {
        String::new()
    };
    if (!status.cookie.is_empty() && status.cookie != source.cookie)
        || (status.code == 803 && account_name != source.account_name)
    {
        let mut active = source.into_active_model();
        if !status.cookie.is_empty() {
            active.cookie = Set(seal_download_secret(
                state,
                &request.source_id,
                "cookie",
                &status.cookie,
            )?);
        }
        if status.code == 803 {
            active.account_name = Set(account_name.clone());
        }
        active.updated_at = Set(chrono::Utc::now().to_rfc3339());
        active.update(&state.db).await?;
    }
    Ok(json!({
        "code": status.code,
        "message": status.message,
        "logged_in": status.code == 803,
        "account_name": account_name,
    }))
}

async fn netease_login_events(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(request): Query<NeteaseLoginRequest>,
) -> Result<Response, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    if request.key.as_deref().unwrap_or_default().is_empty() {
        return Err(ApiError::bad_request("网易云登录 key 不能为空"));
    }

    let shutdown = state.shutdown.clone();
    let events = stream::unfold(
        (state, request, true, false),
        |(state, request, first, finished)| async move {
            if finished {
                return None;
            }
            if !first {
                tokio::time::sleep(Duration::from_millis(2200)).await;
            }
            let (event, finished) = match check_netease_login(&state, &request).await {
                Ok(value) => {
                    let code = value
                        .get("code")
                        .and_then(Value::as_i64)
                        .unwrap_or_default();
                    let event = Event::default()
                        .event("netease-login")
                        .json_data(value)
                        .unwrap_or_else(|error| {
                            Event::default()
                                .event("netease-login-error")
                                .data(error.to_string())
                        });
                    (event, matches!(code, 800 | 803))
                }
                Err(error) => (
                    Event::default()
                        .event("netease-login-error")
                        .data(error.message),
                    true,
                ),
            };
            Some((
                Ok::<Event, Infallible>(event),
                (state, request, false, finished),
            ))
        },
    );
    Ok(sse_response(events, shutdown))
}

async fn load_download_source(
    state: &AppState,
    id: &str,
) -> Result<download_source::Model, ApiError> {
    let mut source = download_source::Entity::find_by_id(id)
        .filter(download_source::Column::Enabled.eq(1))
        .one(&state.db)
        .await?
        .ok_or_else(|| ApiError::bad_request("下载来源不存在或已停用"))?;
    source.password = reveal_download_secret(state, &source.id, "password", &source.password)?;
    source.cookie = reveal_download_secret(state, &source.id, "cookie", &source.cookie)?;
    Ok(source)
}

fn download_secret_aad(id: &str, field: &str) -> String {
    format!("download-source:{id}:{field}")
}

fn seal_download_secret(
    state: &AppState,
    id: &str,
    field: &str,
    value: &str,
) -> Result<String, ApiError> {
    if value.is_empty() {
        return Ok(String::new());
    }
    Ok(encrypt_server_secret(
        value,
        &state.settings.auth.jwt_secret,
        &download_secret_aad(id, field),
    )?)
}

fn reveal_download_secret(
    state: &AppState,
    id: &str,
    field: &str,
    value: &str,
) -> Result<String, ApiError> {
    if value.is_empty() || !value.starts_with("v1:") {
        return Ok(value.to_owned());
    }
    Ok(decrypt_server_secret(
        value,
        &state.settings.auth.jwt_secret,
        &download_secret_aad(id, field),
    )?)
}

fn remote_connection(source: &download_source::Model) -> RemoteConnection {
    RemoteConnection {
        source: source.kind.clone(),
        gateway_url: if source.kind == "subsonic" {
            String::new()
        } else {
            source.base_url.clone()
        },
        cookie: source.cookie.clone(),
        subsonic_url: if source.kind == "subsonic" {
            source.base_url.clone()
        } else {
            String::new()
        },
        username: source.username.clone(),
        password: source.password.clone(),
    }
}

async fn config_status(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    let roots = fetch_library_roots(&state).await?;
    let download_filename_format = download_filename_format(&state).await?;
    let lastfm = lastfm::status(&state, &user.id).await?;
    Ok(Json(ApiResponse::success(json!({
        "database": state.settings.database.driver,
        "queue": state.settings.queue.driver,
        "library_roots": roots,
        "providers": state.providers.names(),
        "download_filename_format": download_filename_format,
        "cover_cache": {
            "enabled": state.settings.cover_cache.enabled,
            "path": state.settings.cover_cache.path.to_string_lossy(),
        },
        "lastfm": lastfm,
        "tools": {"ffmpeg":state.settings.tools.ffmpeg.exists(),"fpcalc":state.settings.tools.fpcalc.exists(),"taglib_configured":state.settings.tools.taglib.is_some()}
    }))))
}

async fn save_preferences(
    State(state): State<AppState>,
    _user: AdminUser,
    Json(request): Json<SavePreferencesRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if !remote_download::DOWNLOAD_FILENAME_FORMATS
        .contains(&request.download_filename_format.as_str())
    {
        return Err(ApiError::bad_request("下载文件名格式无效"));
    }
    if let Some(setting) =
        app_setting::Entity::find_by_id(remote_download::DOWNLOAD_FILENAME_FORMAT_KEY)
            .one(&state.db)
            .await?
    {
        let mut active = setting.into_active_model();
        active.value = Set(request.download_filename_format.clone());
        active.update(&state.db).await?;
    } else {
        app_setting::ActiveModel {
            key: Set(remote_download::DOWNLOAD_FILENAME_FORMAT_KEY.to_owned()),
            value: Set(request.download_filename_format.clone()),
        }
        .insert(&state.db)
        .await?;
    }
    Ok(Json(ApiResponse::success(json!({
        "download_filename_format": request.download_filename_format,
    }))))
}

async fn download_filename_format(state: &AppState) -> Result<String, ApiError> {
    Ok(
        app_setting::Entity::find_by_id(remote_download::DOWNLOAD_FILENAME_FORMAT_KEY)
            .one(&state.db)
            .await?
            .map(|setting| setting.value)
            .filter(|value| remote_download::DOWNLOAD_FILENAME_FORMATS.contains(&value.as_str()))
            .unwrap_or_else(|| remote_download::DEFAULT_DOWNLOAD_FILENAME_FORMAT.to_owned()),
    )
}

async fn save_lastfm_config(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Json(request): Json<SaveLastFmConfigRequest>,
) -> Result<Json<ApiResponse<lastfm::LastFmStatus>>, ApiError> {
    let status = lastfm::save_config(&state, &user.id, &request.api_key, &request.shared_secret)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    Ok(Json(ApiResponse::success(status)))
}

async fn lastfm_status(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<ApiResponse<lastfm::LastFmStatus>>, ApiError> {
    Ok(Json(ApiResponse::success(
        lastfm::status(&state, &user.id).await?,
    )))
}

async fn start_lastfm_auth(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let authorization_url = lastfm::begin_authorization(&state, &user.id)
        .await
        .map_err(|error| ApiError::bad_gateway(error.to_string()))?;
    Ok(Json(ApiResponse::success(json!({
        "authorization_url": authorization_url,
    }))))
}

async fn complete_lastfm_auth(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<ApiResponse<lastfm::LastFmStatus>>, ApiError> {
    let status = lastfm::complete_authorization(&state, &user.id)
        .await
        .map_err(|error| ApiError::bad_gateway(error.to_string()))?;
    Ok(Json(ApiResponse::success(status)))
}

async fn disconnect_lastfm(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<ApiResponse<lastfm::LastFmStatus>>, ApiError> {
    Ok(Json(ApiResponse::success(
        lastfm::disconnect(&state, &user.id).await?,
    )))
}

async fn library_roots(
    State(state): State<AppState>,
    _user: AdminUser,
) -> Result<Json<ApiResponse<Vec<MusicFolder>>>, ApiError> {
    Ok(Json(ApiResponse::success(
        fetch_library_roots(&state).await?,
    )))
}

#[derive(Deserialize)]
struct LibraryRootRequest {
    name: String,
    path: String,
}

async fn add_library_root(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(request): Json<LibraryRootRequest>,
) -> Result<Json<ApiResponse<MusicFolder>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("曲库名称不能为空"));
    }
    let path = PathBuf::from(request.path.trim());
    if !path.is_absolute() {
        return Err(ApiError::bad_request("曲库路径必须是绝对路径"));
    }
    let path = path
        .canonicalize()
        .map_err(|_| ApiError::bad_request("曲库路径不存在或无法访问"))?;
    if !path.is_dir() {
        return Err(ApiError::bad_request("曲库路径必须是目录"));
    }
    let path = path.to_string_lossy().into_owned();
    let duplicate = music_folder::Entity::find()
        .filter(
            sea_orm::Condition::any()
                .add(music_folder::Column::Name.eq(name))
                .add(music_folder::Column::Path.eq(&path)),
        )
        .one(&state.db)
        .await?;
    if duplicate.is_some() {
        return Err(ApiError::bad_request("曲库名称或路径已经存在"));
    }
    let folder = music_folder::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        name: Set(name.to_owned()),
        path: Set(path),
        enabled: Set(1),
    }
    .insert(&state.db)
    .await?;
    if let Err(error) = jobs::enqueue(&state, "scan", &ScanPayload {}).await {
        if let Err(cleanup_error) = music_folder::Entity::delete_by_id(&folder.id)
            .exec(&state.db)
            .await
        {
            tracing::warn!(folder_id = %folder.id, %cleanup_error, "failed to roll back library root after scan enqueue failure");
        }
        return Err(error.into());
    }
    Ok(Json(ApiResponse::success(folder)))
}

#[derive(Deserialize)]
struct DeleteLibraryRootRequest {
    id: String,
}

#[derive(Deserialize)]
struct UpdateLibraryRootRequest {
    id: String,
    name: String,
    path: String,
}

async fn update_library_root(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(request): Json<UpdateLibraryRootRequest>,
) -> Result<Json<ApiResponse<MusicFolder>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("曲库名称不能为空"));
    }
    let path = PathBuf::from(request.path.trim());
    if !path.is_absolute() {
        return Err(ApiError::bad_request("曲库路径必须是绝对路径"));
    }
    let path = path
        .canonicalize()
        .map_err(|_| ApiError::bad_request("曲库路径不存在或无法访问"))?;
    if !path.is_dir() {
        return Err(ApiError::bad_request("曲库路径必须是目录"));
    }
    let path = path.to_string_lossy().into_owned();
    let folder = music_folder::Entity::find_by_id(&request.id)
        .one(&state.db)
        .await?
        .ok_or_else(|| ApiError::bad_request("曲库不存在"))?;
    let duplicate = music_folder::Entity::find()
        .filter(music_folder::Column::Id.ne(&request.id))
        .filter(
            sea_orm::Condition::any()
                .add(music_folder::Column::Name.eq(name))
                .add(music_folder::Column::Path.eq(&path)),
        )
        .one(&state.db)
        .await?;
    if duplicate.is_some() {
        return Err(ApiError::bad_request("曲库名称或路径已经存在"));
    }

    let path_changed = folder.path != path;
    let transaction = state.db.begin().await?;
    let folder_id = folder.id.clone();
    let mut active = folder.into_active_model();
    active.name = Set(name.to_owned());
    active.path = Set(path.clone());
    let folder = active.update(&transaction).await?;

    if path_changed {
        let tracks = track::Entity::find()
            .filter(track::Column::FolderId.eq(&folder_id))
            .all(&transaction)
            .await?;
        for indexed_track in tracks {
            let relative_path = Path::new(&indexed_track.relative_path);
            if relative_path.is_absolute()
                || relative_path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
            {
                return Err(ApiError::bad_request("曲库歌曲相对路径无效"));
            }
            let next_path = PathBuf::from(&path)
                .join(relative_path)
                .to_string_lossy()
                .into_owned();
            let mut active = indexed_track.into_active_model();
            active.path = Set(next_path);
            active.update(&transaction).await?;
        }
    }
    transaction.commit().await?;
    Ok(Json(ApiResponse::success(folder)))
}

async fn delete_library_root(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Json(request): Json<DeleteLibraryRootRequest>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    if user.role != "admin" {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    let exists = music_folder::Entity::find_by_id(&request.id)
        .one(&state.db)
        .await?;
    if exists.is_none() {
        return Err(ApiError::bad_request("曲库不存在"));
    }
    let track_ids: Vec<String> = track::Entity::find()
        .select_only()
        .column(track::Column::Id)
        .filter(track::Column::FolderId.eq(&request.id))
        .into_tuple()
        .all(&state.db)
        .await?;
    scanner::remove_track_records(&state.db, &track_ids).await?;
    music_folder::Entity::delete_by_id(&request.id)
        .exec(&state.db)
        .await?;
    scanner::rebuild_aggregates(&state.db).await?;
    Ok(Json(ApiResponse::success(json!([]))))
}

async fn fetch_library_roots(state: &AppState) -> Result<Vec<MusicFolder>, ApiError> {
    Ok(music_folder::Entity::find()
        .filter(music_folder::Column::Enabled.eq(1))
        .order_by_asc(music_folder::Column::Name)
        .all(&state.db)
        .await?)
}

async fn allowed_path(state: &AppState, value: impl AsRef<Path>) -> Result<PathBuf, ApiError> {
    let value = value.as_ref();
    let absolute = if value.exists() {
        value.canonicalize()?
    } else {
        let parent = value
            .parent()
            .ok_or_else(|| ApiError::bad_request("无效路径"))?
            .canonicalize()?;
        parent.join(value.file_name().unwrap_or_default())
    };
    let roots = fetch_library_roots(state).await?;
    let allowed = roots.iter().any(|root| {
        PathBuf::from(&root.path)
            .canonicalize()
            .map(|path| absolute.starts_with(path))
            .unwrap_or(false)
    });
    if !allowed {
        return Err(ApiError::forbidden("路径不在数据库登记的曲库目录中"));
    }
    Ok(absolute)
}

async fn expand_selection(
    state: &AppState,
    base: &str,
    selection: &[SelectedFile],
) -> Result<Vec<PathBuf>, ApiError> {
    let base = allowed_path(state, base).await?;
    let mut paths = Vec::new();
    for selected in selection {
        let path = allowed_path(state, base.join(&selected.name)).await?;
        if selected.icon == "icon-folder" || path.is_dir() {
            for entry in walkdir::WalkDir::new(path)
                .into_iter()
                .filter_map(Result::ok)
            {
                if entry.file_type().is_file() && scanner::is_audio(entry.path()) {
                    paths.push(entry.path().to_path_buf());
                }
            }
        } else if scanner::is_audio(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn apply_patch(metadata: &mut AudioMetadata, patch: &Value) {
    macro_rules! set {
        ($field:ident) => {
            if let Some(value) = patch.get(stringify!($field)).and_then(Value::as_str) {
                if !value.is_empty() {
                    metadata.$field = render_template(value, metadata);
                }
            }
        };
    }
    set!(title);
    set!(artist);
    set!(album);
    set!(albumartist);
    set!(genre);
    set!(year);
    set!(language);
    set!(lyrics);
    set!(comment);
    set!(tracknumber);
    set!(discnumber);
    set!(filename);
    set!(album_img);
    if let Some(value) = patch.get("is_save_lyrics_file").and_then(Value::as_bool) {
        metadata.is_save_lyrics_file = value;
    }
    if let Some(value) = patch.get("is_save_album_cover").and_then(Value::as_bool) {
        metadata.is_save_album_cover = value;
    }
}

fn render_template(template: &str, metadata: &AudioMetadata) -> String {
    let values: HashMap<&str, &str> = HashMap::from([
        ("title", metadata.title.as_str()),
        ("artist", metadata.artist.as_str()),
        ("album", metadata.album.as_str()),
        ("albumartist", metadata.albumartist.as_str()),
        ("genre", metadata.genre.as_str()),
        ("year", metadata.year.as_str()),
        ("tracknumber", metadata.tracknumber.as_str()),
        ("discnumber", metadata.discnumber.as_str()),
        ("filename", metadata.filename.as_str()),
    ]);
    values
        .into_iter()
        .fold(template.to_owned(), |text, (key, value)| {
            text.replace(&format!("${{{key}}}"), value)
        })
}
fn safe_component(value: &str) -> String {
    let value = value
        .trim()
        .chars()
        .map(|c| {
            if matches!(
                c,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
            ) {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();
    if value.is_empty() || matches!(value.as_str(), "." | "..") {
        "未分类".into()
    } else {
        value
    }
}

async fn materialize_remote_image(metadata: &mut AudioMetadata) -> Result<(), ApiError> {
    if metadata.album_img.starts_with("http://") || metadata.album_img.starts_with("https://") {
        let (mime, bytes) = network::fetch_public_image(&metadata.album_img, 5 * 1024 * 1024)
            .await
            .map_err(|error| ApiError::bad_request(format!("远程封面读取失败：{error}")))?;
        metadata.album_img = format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
    }
    Ok(())
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}
impl ApiError {
    fn bad_request(value: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: value.into(),
        }
    }
    fn unauthorized(value: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: value.into(),
        }
    }
    fn forbidden(value: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: value.into(),
        }
    }
    fn payload_too_large(value: impl Into<String>) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: value.into(),
        }
    }
    fn bad_gateway(value: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: value.into(),
        }
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: value.to_string(),
        }
    }
}
impl From<sea_orm::DbErr> for ApiError {
    fn from(value: sea_orm::DbErr) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: value.to_string(),
        }
    }
}
impl From<std::io::Error> for ApiError {
    fn from(value: std::io::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: value.to_string(),
        }
    }
}
impl From<tokio::task::JoinError> for ApiError {
    fn from(value: tokio::task::JoinError) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: value.to_string(),
        }
    }
}
impl From<axum::extract::multipart::MultipartError> for ApiError {
    fn from(value: axum::extract::multipart::MultipartError) -> Self {
        Self {
            status: value.status(),
            message: value.body_text(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiResponse::failure(json!([]), self.message)),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::Request;
    use http_body_util::BodyExt;
    use sea_orm::PaginatorTrait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

    #[test]
    fn tidy_components_cannot_escape_the_library() {
        assert_eq!(safe_component(".."), "未分类");
        assert_eq!(safe_component("."), "未分类");
        assert_eq!(safe_component("Artist/Album"), "Artist_Album");
    }

    #[test]
    fn uploaded_audio_names_keep_the_original_stem_and_normalize_extension() {
        assert_eq!(
            uploaded_audio_filename("Original Song.MP3").unwrap(),
            "Original Song.mp3"
        );
        assert_eq!(
            uploaded_audio_filename("Artist/Song?.flac").unwrap(),
            "Song_.flac"
        );
        assert_eq!(
            uploaded_audio_filename("cover.jpg").unwrap_err().message,
            "只支持曲库可识别的音频文件"
        );
    }

    #[test]
    fn recognizes_hls_radio_from_the_final_redirect_url() {
        let url = reqwest::Url::parse(
            "http://ytcast.radio.cn/62/radios/21600/index_21600.m3u8?type=1&key=test",
        )
        .unwrap();

        assert!(is_hls_radio_stream(&url, Some("application/octet-stream")));
    }

    #[test]
    fn recognizes_standard_hls_content_types() {
        let url = reqwest::Url::parse("https://radio.example.test/live").unwrap();

        assert!(is_hls_radio_stream(
            &url,
            Some("application/vnd.apple.mpegurl; charset=utf-8")
        ));
        assert!(is_hls_radio_stream(&url, Some("application/x-mpegurl")));
        assert!(!is_hls_radio_stream(&url, Some("application/octet-stream")));
        assert!(!is_hls_radio_stream(&url, Some("audio/mpeg")));
    }

    #[tokio::test]
    async fn saves_the_remote_download_filename_preference() {
        let state = test_state().await;
        let admin = crate::auth::user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();

        let _ = save_preferences(
            State(state.clone()),
            AdminUser(admin),
            Json(SavePreferencesRequest {
                download_filename_format: "title-artist".into(),
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            download_filename_format(&state).await.unwrap(),
            "title-artist"
        );
    }

    #[tokio::test]
    async fn saves_lastfm_secrets_encrypted_without_returning_them() {
        let state = test_state().await;
        let admin = crate::auth::user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();

        let response = save_lastfm_config(
            State(state.clone()),
            AdminUser(admin.clone()),
            Json(SaveLastFmConfigRequest {
                api_key: "0123456789abcdef0123456789abcdef".into(),
                shared_secret: "abcdef0123456789abcdef0123456789".into(),
            }),
        )
        .await
        .unwrap()
        .0;
        let stored = app_setting::Entity::find_by_id("lastfm.shared_secret")
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();

        assert!(response.data.configured);
        assert!(!response.data.connected);
        assert!(response.data.has_shared_secret);
        assert!(stored.value.starts_with("v1:"));
        assert!(!stored.value.contains("abcdef0123456789"));

        let error = save_lastfm_config(
            State(state),
            AdminUser(admin),
            Json(SaveLastFmConfigRequest {
                api_key: "fedcba9876543210fedcba9876543210".into(),
                shared_secret: String::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.message,
            "修改 Last.fm API Key 时必须同时填写对应的 Shared Secret"
        );
    }

    #[tokio::test]
    async fn regular_users_can_read_personal_lastfm_status_but_not_admin_config() {
        let state = test_state().await;
        let user = crate::entities::user::ActiveModel {
            id: Set("listener-id".into()),
            username: Set("listener".into()),
            password_hash: Set(String::new()),
            email: Set(String::new()),
            role: Set("user".into()),
            subsonic_token: Set(String::new()),
            subsonic_password: Set(String::new()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
        }
        .insert(&state.db)
        .await
        .unwrap();
        let (token, _) = crate::auth::issue_tokens(
            &user,
            &state.settings.auth.jwt_secret,
            state.settings.auth.access_token_minutes,
            state.settings.auth.refresh_token_days,
        )
        .unwrap();
        let app = router().with_state(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/lastfm/status/")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/lastfm/config/")
                    .header("authorization", format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"api_key":"0123456789abcdef","shared_secret":"abcdef0123456789"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn uploads_and_indexes_a_song_without_overwriting_its_name() {
        let state = test_state().await;
        let admin = crate::auth::user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();
        let (token, _) = crate::auth::issue_tokens(
            &admin,
            &state.settings.auth.jwt_secret,
            state.settings.auth.access_token_minutes,
            state.settings.auth.refresh_token_days,
        )
        .unwrap();
        let library = tempfile::tempdir().unwrap();
        let root_id = Uuid::new_v4().to_string();
        music_folder::ActiveModel {
            id: Set(root_id.clone()),
            name: Set("Uploads".into()),
            path: Set(library.path().to_string_lossy().into_owned()),
            enabled: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        let boundary = "mNest-upload-boundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"upload_file\"; filename=\"Uploaded Song.WAV\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .into_bytes();
        body.extend(minimal_wav());
        body.extend(format!("\r\n--{boundary}--\r\n").into_bytes());
        let app = router().with_state(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/remote_download/upload/?root_id={root_id}&directory=incoming"
                    ))
                    .header("authorization", format!("Bearer {token}"))
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let response_body = response.into_body().collect().await.unwrap().to_bytes();

        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&response_body)
        );
        assert!(library.path().join("incoming/Uploaded Song.wav").is_file());
        assert_eq!(
            track::Entity::find()
                .filter(track::Column::FolderId.eq(root_id))
                .count(&state.db)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn proxies_an_authenticated_internet_radio_stream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let size = socket.read(&mut request).await.unwrap();
            assert!(
                String::from_utf8_lossy(&request[..size])
                    .to_ascii_lowercase()
                    .contains("icy-metadata: 0")
            );
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 5\r\n\r\nRADIO",
                )
                .await
                .unwrap();
        });
        let state = test_state().await;
        let admin = crate::auth::user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();
        let (token, _) = crate::auth::issue_tokens(
            &admin,
            &state.settings.auth.jwt_secret,
            state.settings.auth.access_token_minutes,
            state.settings.auth.refresh_token_days,
        )
        .unwrap();
        internet_radio_station::ActiveModel {
            id: Set("radio-1".into()),
            name: Set("Radio".into()),
            stream_url: Set(format!("http://{address}/live")),
            home_page_url: Set(String::new()),
        }
        .insert(&state.db)
        .await
        .unwrap();

        let response = router()
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/api/internet_radio_stream/?id=radio-1")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "audio/mpeg"
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"RADIO".as_slice()
        );
        upstream.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transcodes_a_redirected_hls_radio_with_a_generic_content_type() {
        use std::os::unix::fs::PermissionsExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let size = socket.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..size]).contains("GET /live "));
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{address}/stream/index.m3u8?token=test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();

            let (mut socket, _) = listener.accept().await.unwrap();
            let size = socket.read(&mut request).await.unwrap();
            assert!(
                String::from_utf8_lossy(&request[..size])
                    .contains("GET /stream/index.m3u8?token=test ")
            );
            let playlist = b"#EXTM3U\n#EXT-X-VERSION:3\n";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        playlist.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(playlist).await.unwrap();
        });

        let temp = tempfile::tempdir().unwrap();
        let fake_ffmpeg = temp.path().join("ffmpeg");
        std::fs::write(&fake_ffmpeg, "#!/bin/sh\nprintf 'HLS-RADIO'\n").unwrap();
        let mut permissions = std::fs::metadata(&fake_ffmpeg).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fake_ffmpeg, permissions).unwrap();
        let mut settings = crate::config::Settings::default();
        settings.tools.ffmpeg = fake_ffmpeg;
        let state = test_state_with_settings(settings).await;
        let admin = crate::auth::user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();
        let (token, _) = crate::auth::issue_tokens(
            &admin,
            &state.settings.auth.jwt_secret,
            state.settings.auth.access_token_minutes,
            state.settings.auth.refresh_token_days,
        )
        .unwrap();
        internet_radio_station::ActiveModel {
            id: Set("radio-hls".into()),
            name: Set("HLS Radio".into()),
            stream_url: Set(format!("http://{address}/live")),
            home_page_url: Set(String::new()),
        }
        .insert(&state.db)
        .await
        .unwrap();

        let response = router()
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/api/internet_radio_stream/?id=radio-hls")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "audio/mpeg"
        );
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"HLS-RADIO".as_slice()
        );
        upstream.await.unwrap();
    }

    fn minimal_wav() -> Vec<u8> {
        let mut wav = b"RIFF".to_vec();
        wav.extend(38u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(8_000u32.to_le_bytes());
        wav.extend(16_000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(2u32.to_le_bytes());
        wav.extend([0, 0]);
        wav
    }

    #[test]
    fn job_summaries_do_not_expose_payloads_or_leases() {
        let value = job_summary(job::Model {
            id: "job".into(),
            kind: "remote_import".into(),
            state: "pending".into(),
            payload: "https://example.test/?token=secret".into(),
            progress: 0.0,
            message: String::new(),
            attempts: 0,
            lease_until: Some("secret-lease".into()),
            created_at: String::new(),
            updated_at: String::new(),
        });
        assert!(value.get("payload").is_none());
        assert!(value.get("lease_until").is_none());
    }

    #[tokio::test]
    async fn sse_streams_close_when_the_server_shuts_down() {
        let shutdown = CancellationToken::new();
        let response = sse_response(
            stream::pending::<Result<Event, Infallible>>(),
            shutdown.clone(),
        );
        shutdown.cancel();

        tokio::time::timeout(Duration::from_secs(1), response.into_body().collect())
            .await
            .expect("SSE body should close after shutdown")
            .unwrap();
    }

    #[tokio::test]
    async fn adding_a_library_root_enqueues_a_scan() {
        let state = test_state().await;
        let admin = crate::auth::user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();

        let result = add_library_root(
            State(state.clone()),
            AuthUser(admin),
            Json(LibraryRootRequest {
                name: "Music".into(),
                path: directory.path().to_string_lossy().into_owned(),
            }),
        )
        .await;
        assert!(result.is_ok());

        assert!(
            job::Entity::find()
                .filter(job::Column::Kind.eq("scan"))
                .one(&state.db)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn updating_a_library_root_rewrites_paths_without_enqueuing_a_scan() {
        let state = test_state().await;
        let admin = crate::auth::user_by_name(&state.db, &state.settings.admin.username)
            .await
            .unwrap()
            .unwrap();
        let original = tempfile::tempdir().unwrap();
        let replacement = tempfile::tempdir().unwrap();
        let song_name = "song.wav";
        std::fs::write(original.path().join(song_name), minimal_wav()).unwrap();
        std::fs::write(replacement.path().join(song_name), minimal_wav()).unwrap();
        let root_id = Uuid::new_v4().to_string();
        music_folder::ActiveModel {
            id: Set(root_id.clone()),
            name: Set("Original".into()),
            path: Set(original.path().to_string_lossy().into_owned()),
            enabled: Set(1),
        }
        .insert(&state.db)
        .await
        .unwrap();
        scanner::scan_all(
            &state.db,
            state.tags.clone(),
            &tokio_util::sync::CancellationToken::new(),
            |_, _| {},
        )
        .await
        .unwrap();

        let result = update_library_root(
            State(state.clone()),
            AuthUser(admin),
            Json(UpdateLibraryRootRequest {
                id: root_id.clone(),
                name: "Replacement".into(),
                path: replacement.path().to_string_lossy().into_owned(),
            }),
        )
        .await
        .unwrap()
        .0
        .data;

        assert_eq!(result.name, "Replacement");
        assert_eq!(result.path, replacement.path().to_string_lossy());
        let indexed_track = track::Entity::find()
            .filter(track::Column::FolderId.eq(&root_id))
            .one(&state.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(indexed_track.relative_path, song_name);
        assert_eq!(
            indexed_track.path,
            replacement.path().join(song_name).to_string_lossy()
        );
        assert!(
            job::Entity::find()
                .filter(job::Column::Kind.eq("scan"))
                .one(&state.db)
                .await
                .unwrap()
                .is_none()
        );
    }
}
