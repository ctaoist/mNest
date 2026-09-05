use std::{future::Future, path::PathBuf, time::Duration};

use anyhow::Context;
use chrono::Utc;
use redis::AsyncCommands;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, sea_query::Expr,
};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    AppState, artist_credit,
    entities::{job, music_folder, track},
    models::Job,
    network,
    remote_download::{self, RemoteImportPayload},
    scanner,
};

pub(crate) const MAX_IMPORT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanPayload {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoTagPayload {
    pub paths: Vec<PathBuf>,
    pub sources: Vec<String>,
    pub mode: String,
}

pub async fn enqueue<T: Serialize>(
    state: &AppState,
    kind: &str,
    payload: &T,
) -> anyhow::Result<String> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    job::ActiveModel {
        id: Set(id.clone()),
        kind: Set(kind.to_owned()),
        state: Set("pending".into()),
        payload: Set(serde_json::to_string(payload)?),
        progress: Set(0.0),
        message: Set(String::new()),
        attempts: Set(0),
        lease_until: Set(None),
        created_at: Set(now.clone()),
        updated_at: Set(now),
    }
    .insert(&state.db)
    .await?;
    if state.settings.queue.driver == "redis" {
        let pushed = async {
            let client = redis::Client::open(
                state
                    .settings
                    .queue
                    .redis_url
                    .as_deref()
                    .unwrap_or_default(),
            )?;
            let mut connection = client.get_multiplexed_async_connection().await?;
            let _: usize = connection.lpush("mNest:jobs", &id).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(error) = pushed {
            if let Err(cleanup_error) = job::Entity::delete_by_id(&id).exec(&state.db).await {
                tracing::warn!(job_id = %id, %cleanup_error, "failed to roll back job after Redis enqueue failure");
            }
            return Err(error);
        }
    }
    state.events.notify_jobs();
    Ok(id)
}

pub struct JobRunner {
    shutdown: CancellationToken,
    handles: Vec<JoinHandle<()>>,
}

impl JobRunner {
    pub async fn start(state: AppState, shutdown: CancellationToken) -> anyhow::Result<Self> {
        if state.settings.queue.driver == "redis" {
            let client = redis::Client::open(
                state
                    .settings
                    .queue
                    .redis_url
                    .as_deref()
                    .unwrap_or_default(),
            )?;
            let _: redis::aio::MultiplexedConnection =
                client.get_multiplexed_async_connection().await?;
        }
        recover_interrupted_jobs(&state).await?;
        let mut handles = Vec::new();
        for worker in 0..state.settings.queue.workers.max(1) {
            let state = state.clone();
            let shutdown = shutdown.clone();
            handles.push(tokio::spawn(async move {
                loop {
                    let next = tokio::select! {
                        _ = shutdown.cancelled() => break,
                        result = next_job(&state) => result,
                    };
                    match next {
                        Ok(Some(job)) => {
                            match execute(&state, &job, &shutdown).await {
                                Ok(()) => {}
                                Err(error) if shutdown.is_cancelled() => {
                                    tracing::info!(job_id = %job.id, worker, %error, "job interrupted by shutdown");
                                    let _ = requeue_after_shutdown(&state, &job.id).await;
                                    break;
                                }
                                Err(error) => {
                                    tracing::error!(job_id = %job.id, worker, %error, "job failed");
                                    let _ = fail(&state, &job, &error.to_string()).await;
                                }
                            }
                        }
                        Ok(None) => {
                            tokio::select! {
                                _ = shutdown.cancelled() => break,
                                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                            }
                        }
                        Err(error) => {
                            tracing::error!(worker, %error, "job polling failed");
                            tokio::select! {
                                _ = shutdown.cancelled() => break,
                                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                            }
                        }
                    }
                }
            }));
        }
        Ok(Self { shutdown, handles })
    }

    pub async fn shutdown(self) {
        self.shutdown.cancel();
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

async fn recover_interrupted_jobs(state: &AppState) -> anyhow::Result<()> {
    let result = job::Entity::update_many()
        .col_expr(job::Column::State, Expr::value("pending"))
        .col_expr(job::Column::LeaseUntil, Expr::value(Option::<String>::None))
        .col_expr(
            job::Column::Attempts,
            Expr::case(
                Expr::col(job::Column::Attempts).gt(0),
                Expr::col(job::Column::Attempts).sub(1),
            )
            .finally(0)
            .into(),
        )
        .col_expr(
            job::Column::Message,
            Expr::value("检测到上次服务中断，任务已重新排队"),
        )
        .col_expr(job::Column::UpdatedAt, Expr::value(Utc::now().to_rfc3339()))
        .filter(job::Column::State.eq("running"))
        .exec(&state.db)
        .await?;
    if result.rows_affected > 0 {
        state.events.notify_jobs();
    }
    Ok(())
}

async fn requeue_after_shutdown(state: &AppState, id: &str) -> anyhow::Result<()> {
    let result = job::Entity::update_many()
        .col_expr(job::Column::State, Expr::value("pending"))
        .col_expr(job::Column::LeaseUntil, Expr::value(Option::<String>::None))
        .col_expr(
            job::Column::Attempts,
            Expr::case(
                Expr::col(job::Column::Attempts).gt(0),
                Expr::col(job::Column::Attempts).sub(1),
            )
            .finally(0)
            .into(),
        )
        .col_expr(
            job::Column::Message,
            Expr::value("服务停止，任务已重新排队"),
        )
        .col_expr(job::Column::UpdatedAt, Expr::value(Utc::now().to_rfc3339()))
        .filter(job::Column::Id.eq(id))
        .filter(job::Column::State.eq("running"))
        .exec(&state.db)
        .await?;
    if result.rows_affected > 0 {
        state.events.notify_jobs();
    }
    Ok(())
}

async fn claim_next(state: &AppState) -> anyhow::Result<Option<Job>> {
    let Some(job) = job::Entity::find()
        .filter(job::Column::State.eq("pending"))
        .order_by_asc(job::Column::CreatedAt)
        .one(&state.db)
        .await?
    else {
        return Ok(None);
    };
    let result = job::Entity::update_many()
        .col_expr(job::Column::State, Expr::value("running"))
        .col_expr(
            job::Column::Attempts,
            Expr::col(job::Column::Attempts).add(1),
        )
        .col_expr(job::Column::UpdatedAt, Expr::value(Utc::now().to_rfc3339()))
        .filter(job::Column::Id.eq(&job.id))
        .filter(job::Column::State.eq("pending"))
        .exec(&state.db)
        .await?;
    if result.rows_affected == 1 {
        state.events.notify_jobs();
    }
    Ok((result.rows_affected == 1).then_some(job))
}

async fn next_job(state: &AppState) -> anyhow::Result<Option<Job>> {
    if let Some(job) = claim_next(state).await? {
        return Ok(Some(job));
    }
    if state.settings.queue.driver != "redis" {
        return Ok(None);
    }
    let client = redis::Client::open(
        state
            .settings
            .queue
            .redis_url
            .as_deref()
            .unwrap_or_default(),
    )?;
    let mut connection = client.get_multiplexed_async_connection().await?;
    let item: Option<[String; 2]> = connection.brpop("mNest:jobs", 2.0).await?;
    let Some([_, id]) = item else {
        return Ok(None);
    };
    claim_by_id(state, &id).await
}

async fn claim_by_id(state: &AppState, id: &str) -> anyhow::Result<Option<Job>> {
    let Some(job) = job::Entity::find_by_id(id)
        .filter(job::Column::State.eq("pending"))
        .one(&state.db)
        .await?
    else {
        return Ok(None);
    };
    let result = job::Entity::update_many()
        .col_expr(job::Column::State, Expr::value("running"))
        .col_expr(
            job::Column::Attempts,
            Expr::col(job::Column::Attempts).add(1),
        )
        .col_expr(job::Column::UpdatedAt, Expr::value(Utc::now().to_rfc3339()))
        .filter(job::Column::Id.eq(&job.id))
        .filter(job::Column::State.eq("pending"))
        .exec(&state.db)
        .await?;
    if result.rows_affected == 1 {
        state.events.notify_jobs();
    }
    Ok((result.rows_affected == 1).then_some(job))
}

async fn execute(state: &AppState, job: &Job, shutdown: &CancellationToken) -> anyhow::Result<()> {
    ensure_running(shutdown)?;
    match job.kind.as_str() {
        "scan" => {
            let progress_state = state.clone();
            let id = job.id.clone();
            let report = scanner::scan_all(
                &state.db,
                state.tags.clone(),
                shutdown,
                move |progress, message| {
                    let state = progress_state.clone();
                    let id = id.clone();
                    let message = message.to_owned();
                    tokio::spawn(async move {
                        let _ = set_progress(&state, &id, progress, &message).await;
                    });
                },
            )
            .await?;
            complete(state, &job.id, &serde_json::to_string(&report)?).await?;
        }
        "auto_tag" => {
            let payload: AutoTagPayload = serde_json::from_str(&job.payload)?;
            let total = payload.paths.len().max(1);
            let mut failures = Vec::new();
            let mut updated_paths = Vec::new();
            let mut artwork_statuses = Vec::new();
            for (index, path) in payload.paths.iter().enumerate() {
                ensure_running(shutdown)?;
                match auto_tag_one(state, path, &payload.sources, &payload.mode, shutdown).await {
                    Ok(updated) => {
                        artwork_statuses.push((updated.path.clone(), updated.has_artwork));
                        updated_paths.push((path.clone(), updated.path));
                    }
                    Err(error) => failures.push(format!("{}: {error:#}", path.display())),
                }
                set_progress(
                    state,
                    &job.id,
                    (index + 1) as f64 / total as f64,
                    &path.to_string_lossy(),
                )
                .await?;
            }
            if !updated_paths.is_empty() {
                set_progress(state, &job.id, 0.98, "正在更新曲库索引").await?;
                match scanner::refresh_path_changes(&state.db, state.tags.clone(), &updated_paths)
                    .await
                {
                    Ok(_) => {
                        if let Err(error) =
                            scanner::remember_artwork_statuses(&state.db, &artwork_statuses).await
                        {
                            failures.push(format!("封面状态更新失败: {error:#}"));
                        }
                    }
                    Err(error) => failures.push(format!("曲库索引更新失败: {error:#}")),
                }
            }
            if failures.is_empty() {
                complete(state, &job.id, "completed").await?;
            } else {
                complete(
                    state,
                    &job.id,
                    &format!("{} file(s) failed: {}", failures.len(), failures.join("; ")),
                )
                .await?;
            }
        }
        "remote_import" => {
            let payload: RemoteImportPayload = serde_json::from_str(&job.payload)?;
            let destination = remote_import_one(state, &job.id, &payload, shutdown).await?;
            complete(
                state,
                &job.id,
                &format!("已下载并入库：{}", destination.display()),
            )
            .await?;
        }
        other => anyhow::bail!("unknown job kind {other}"),
    }
    Ok(())
}

async fn remote_import_one(
    state: &AppState,
    job_id: &str,
    payload: &RemoteImportPayload,
    shutdown: &CancellationToken,
) -> anyhow::Result<PathBuf> {
    ensure_running(shutdown)?;
    let root = music_folder::Entity::find_by_id(&payload.root_id)
        .filter(music_folder::Column::Enabled.eq(1))
        .one(&state.db)
        .await?
        .context("目标曲库不存在或已停用")?;
    let root = tokio::fs::canonicalize(root.path).await?;
    let mut directory = root.clone();
    for component in payload.directory.split(['/', '\\']) {
        let component = component.trim();
        if component.is_empty() || matches!(component, "." | "..") {
            continue;
        }
        directory.push(remote_download::safe_component(component));
    }
    tokio::fs::create_dir_all(&directory).await?;
    let directory = tokio::fs::canonicalize(directory).await?;
    anyhow::ensure!(directory.starts_with(&root), "下载目录不能离开目标曲库");
    let desired_destination = directory.join(remote_download::safe_component(&payload.filename));
    let partial = desired_destination.with_file_name(format!(
        ".{}.{}.part",
        desired_destination
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("download"),
        Uuid::new_v4().simple()
    ));
    let mut committed_destination = None;
    let result: anyhow::Result<PathBuf> = async {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30 * 60))
            .user_agent("mNest/remote-import")
            .build()?;
        let response = cancel_on_shutdown(shutdown, async {
            let response = client
                .get(&payload.download_url)
                .send()
                .await
                .map_err(|error| anyhow::anyhow!(error.without_url()))?;
            response
                .error_for_status()
                .map_err(|error| anyhow::anyhow!(error.without_url()))
        })
        .await?;
        let progress_state = state.clone();
        let progress_job_id = job_id.to_owned();
        let progress_title = payload.title.clone();
        let downloaded = network::download_response_to_file(
            response,
            &partial,
            MAX_IMPORT_BYTES,
            Some(shutdown.clone()),
            move |download| {
                let state = progress_state.clone();
                let job_id = progress_job_id.clone();
                let title = progress_title.clone();
                async move {
                    let progress = if download.total > 0 {
                        (download.downloaded as f64 / download.total as f64).min(1.0) * 0.82
                    } else {
                        0.35
                    };
                    set_progress(&state, &job_id, progress, &format!("正在下载 {title}")).await
                }
            },
        )
        .await
        .context("远程音频下载失败")?;
        let expected_extension = desired_destination
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let inspect_path = partial.clone();
        let inspect_content_type = downloaded.content_type.clone();
        let actual_extension = tokio::task::spawn_blocking(move || {
            remote_download::downloaded_audio_extension(
                &inspect_path,
                &expected_extension,
                inspect_content_type.as_deref(),
            )
        })
        .await??;
        let desired_destination = if desired_destination
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case(&actual_extension))
        {
            desired_destination.clone()
        } else {
            let corrected = desired_destination.with_extension(&actual_extension);
            tracing::warn!(
                requested_path = %desired_destination.display(),
                actual_extension,
                corrected_path = %corrected.display(),
                "download source returned a different audio format; corrected file extension"
            );
            corrected
        };
        let destination = commit_download(&partial, &desired_destination).await?;
        committed_destination = Some(destination.clone());
        set_progress(state, job_id, 0.85, "下载完成，正在扫描曲库").await?;
        let indexed = scanner::refresh_paths(
            &state.db,
            state.tags.clone(),
            std::slice::from_ref(&destination),
        )
        .await?;
        anyhow::ensure!(indexed == 1, "下载文件未能加入曲库索引");
        anyhow::Ok(destination)
    }
    .await;
    match result {
        Ok(destination) => Ok(destination),
        Err(error) => {
            let _ = tokio::fs::remove_file(&partial).await;
            if let Some(destination) = committed_destination {
                let _ = tokio::fs::remove_file(&destination).await;
                if let Err(cleanup_error) = remove_import_index(state, &destination).await {
                    tracing::warn!(path = %destination.display(), %cleanup_error, "failed to roll back remote import index");
                }
            }
            Err(error)
        }
    }
}

pub(crate) async fn remove_import_index(
    state: &AppState,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    let Some(indexed) = track::Entity::find()
        .filter(track::Column::Path.eq(path.to_string_lossy().into_owned()))
        .one(&state.db)
        .await?
    else {
        return Ok(());
    };
    scanner::remove_track_records(&state.db, &[indexed.id]).await?;
    scanner::rebuild_aggregates(&state.db).await
}

pub(crate) async fn commit_download(
    partial: &std::path::Path,
    path: &std::path::Path,
) -> anyhow::Result<PathBuf> {
    let parent = path.parent().map(PathBuf::from).unwrap_or_default();
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    for index in 1..10_000 {
        let candidate = if index == 1 {
            path.to_path_buf()
        } else {
            let filename = if extension.is_empty() {
                format!("{stem} ({index})")
            } else {
                format!("{stem} ({index}).{extension}")
            };
            parent.join(filename)
        };
        match tokio::fs::hard_link(partial, &candidate).await {
            Ok(()) => {
                remove_committed_temp(partial).await;
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(link_error) => match copy_download_noclobber(partial, &candidate).await {
                Ok(()) => {
                    remove_committed_temp(partial).await;
                    return Ok(candidate);
                }
                Err(copy_error) if copy_error.kind() == std::io::ErrorKind::AlreadyExists => {
                    continue;
                }
                Err(copy_error) => {
                    return Err(anyhow::anyhow!(
                        "failed to commit download (hard link: {link_error}; copy: {copy_error})"
                    ));
                }
            },
        }
    }
    anyhow::bail!("无法为下载文件分配不重复的文件名")
}

async fn copy_download_noclobber(
    partial: &std::path::Path,
    destination: &std::path::Path,
) -> std::io::Result<()> {
    let mut source = tokio::fs::File::open(partial).await?;
    let mut target = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await
    {
        Ok(target) => target,
        Err(error) => return Err(error),
    };
    let result = async {
        tokio::io::copy(&mut source, &mut target).await?;
        target.flush().await?;
        target.sync_all().await
    }
    .await;
    if result.is_err() {
        drop(target);
        let _ = tokio::fs::remove_file(destination).await;
    }
    result
}

async fn remove_committed_temp(partial: &std::path::Path) {
    if let Err(error) = tokio::fs::remove_file(partial).await {
        tracing::warn!(path = %partial.display(), %error, "failed to remove committed download temp file");
    }
}

async fn auto_tag_one(
    state: &AppState,
    path: &std::path::Path,
    sources: &[String],
    mode: &str,
    shutdown: &CancellationToken,
) -> anyhow::Result<crate::tags::TagWriteResult> {
    let tags = state.tags.clone();
    let read_path = path.to_path_buf();
    let mut current = tokio::task::spawn_blocking(move || tags.read(&read_path)).await??;
    ensure_running(shutdown)?;
    let query = if current.title.is_empty() {
        path.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        current.title.clone()
    };
    let mut best = None;
    for source in sources
        .iter()
        .map(String::as_str)
        .chain((sources.is_empty()).then_some("smart_tag"))
    {
        ensure_running(shutdown)?;
        let candidate = if source == "acoustid" {
            cancel_on_shutdown(shutdown, async {
                Ok(state.providers.fingerprint(path).await?.into_iter().next())
            })
            .await?
        } else {
            cancel_on_shutdown(shutdown, async {
                Ok(state
                    .providers
                    .search(source, &query, &current.artist, &current.album)
                    .await?
                    .into_iter()
                    .next())
            })
            .await?
        };
        if let Some(candidate) = candidate
            && best
                .as_ref()
                .map(|v: &crate::providers::MetadataCandidate| v.score)
                .unwrap_or(-1.0)
                < candidate.score
        {
            best = Some(candidate);
        }
    }
    let candidate = best.context("no matching metadata found")?;
    if mode == "soft" {
        if current.title.is_empty() {
            current.title = candidate.name;
        }
        if current.artist.is_empty() {
            current.artist = artist_credit::normalize_artist_metadata(&candidate.artist);
        }
        if current.album.is_empty() {
            current.album = candidate.album;
        }
        if current.year.is_empty() {
            current.year = candidate.year;
        }
        if current.tracknumber.is_empty() {
            current.tracknumber = candidate.tracknumber;
        }
        if current.discnumber.is_empty() {
            current.discnumber = candidate.discnumber;
        }
        if current.album_img.is_empty() {
            current.album_img = candidate.album_img;
        }
    } else {
        current.title = candidate.name;
        current.artist = artist_credit::normalize_artist_metadata(&candidate.artist);
        current.album = candidate.album;
        current.year = candidate.year;
        current.tracknumber = candidate.tracknumber;
        current.discnumber = candidate.discnumber;
        if !candidate.album_img.is_empty() {
            current.album_img = candidate.album_img;
        }
    }
    if current.lyrics.is_empty() && candidate.resource != "acoustid" {
        current.lyrics = cancel_on_shutdown(shutdown, async {
            Ok(state
                .providers
                .lyrics(&candidate.resource, &candidate.id)
                .await
                .unwrap_or_default())
        })
        .await?;
    }
    if current.album_img.starts_with("http") {
        let (mime, bytes) = cancel_on_shutdown(shutdown, async {
            network::fetch_public_image(&current.album_img, 5 * 1024 * 1024).await
        })
        .await?;
        current.album_img = format!(
            "data:{mime};base64,{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
        );
    }
    ensure_running(shutdown)?;
    let tags = state.tags.clone();
    let write_path = path.to_path_buf();
    tokio::task::spawn_blocking(move || tags.write(&write_path, &current)).await?
}

fn ensure_running(shutdown: &CancellationToken) -> anyhow::Result<()> {
    anyhow::ensure!(!shutdown.is_cancelled(), "shutdown requested");
    Ok(())
}

async fn cancel_on_shutdown<T>(
    shutdown: &CancellationToken,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        _ = shutdown.cancelled() => anyhow::bail!("shutdown requested"),
        result = future => result,
    }
}

async fn set_progress(
    state: &AppState,
    id: &str,
    progress: f64,
    message: &str,
) -> anyhow::Result<()> {
    job::Entity::update_many()
        .col_expr(job::Column::Progress, Expr::value(progress))
        .col_expr(job::Column::Message, Expr::value(message.to_owned()))
        .col_expr(job::Column::UpdatedAt, Expr::value(Utc::now().to_rfc3339()))
        .filter(job::Column::Id.eq(id))
        .exec(&state.db)
        .await?;
    state.events.notify_jobs();
    Ok(())
}
async fn complete(state: &AppState, id: &str, message: &str) -> anyhow::Result<()> {
    job::Entity::update_many()
        .col_expr(job::Column::State, Expr::value("completed"))
        .col_expr(job::Column::Progress, Expr::value(1.0))
        .col_expr(job::Column::Message, Expr::value(message.to_owned()))
        .col_expr(job::Column::UpdatedAt, Expr::value(Utc::now().to_rfc3339()))
        .filter(job::Column::Id.eq(id))
        .exec(&state.db)
        .await?;
    state.events.notify_jobs();
    Ok(())
}
async fn fail(state: &AppState, job: &Job, message: &str) -> anyhow::Result<()> {
    let next_state = if job.attempts + 1 < state.settings.queue.max_attempts as i64 {
        "pending"
    } else {
        "failed"
    };
    job::Entity::update_many()
        .col_expr(job::Column::State, Expr::value(next_state))
        .col_expr(job::Column::Message, Expr::value(message.to_owned()))
        .col_expr(job::Column::UpdatedAt, Expr::value(Utc::now().to_rfc3339()))
        .filter(job::Column::Id.eq(&job.id))
        .exec(&state.db)
        .await?;
    state.events.notify_jobs();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::DatabaseSettings;

    #[tokio::test]
    async fn cancellation_interrupts_pending_async_work() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let result =
            cancel_on_shutdown(&shutdown, std::future::pending::<anyhow::Result<()>>()).await;

        assert_eq!(result.unwrap_err().to_string(), "shutdown requested");
    }

    #[tokio::test]
    async fn committing_download_never_overwrites_an_existing_song() {
        let directory = tempfile::tempdir().unwrap();
        let desired = directory.path().join("song.mp3");
        let partial = directory.path().join(".song.part");
        std::fs::write(&desired, b"existing").unwrap();
        std::fs::write(&partial, b"new").unwrap();

        let committed = commit_download(&partial, &desired).await.unwrap();

        assert_eq!(committed, directory.path().join("song (2).mp3"));
        assert_eq!(std::fs::read(desired).unwrap(), b"existing");
        assert_eq!(std::fs::read(committed).unwrap(), b"new");
        assert!(!partial.exists());
    }

    #[tokio::test]
    async fn interrupted_jobs_are_requeued_without_consuming_an_attempt() {
        let db = crate::db::connect(&DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        crate::db::migrate(&db).await.unwrap();
        let settings = Arc::new(crate::config::Settings::default());
        let providers = Arc::new(crate::providers::ProviderRegistry::new(settings.clone()));
        let state = AppState::new(settings, db.clone(), providers);
        let now = Utc::now().to_rfc3339();
        job::ActiveModel {
            id: Set("interrupted-job".into()),
            kind: Set("scan".into()),
            state: Set("running".into()),
            payload: Set("{}".into()),
            progress: Set(0.4),
            message: Set("running".into()),
            attempts: Set(1),
            lease_until: Set(Some(now.clone())),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        recover_interrupted_jobs(&state).await.unwrap();

        let recovered = job::Entity::find_by_id("interrupted-job")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, "pending");
        assert_eq!(recovered.attempts, 0);
        assert_eq!(recovered.lease_until, None);
    }
}
