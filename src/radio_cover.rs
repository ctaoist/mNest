use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context;
use md5::{Digest, Md5};
use tempfile::Builder;
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

use crate::{
    config::CoverCacheSettings,
    network,
    tags::{AudioArtwork, detect_artwork_mime},
};

const MAX_RADIO_COVER_BYTES: usize = 5 * 1024 * 1024;
const RADIO_COVER_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Clone)]
pub struct RadioCoverCache {
    settings: CoverCacheSettings,
    limiter: Arc<Semaphore>,
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl RadioCoverCache {
    pub fn new(settings: CoverCacheSettings) -> Self {
        Self {
            limiter: Arc::new(Semaphore::new(settings.concurrency)),
            settings,
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn get(&self, station_id: &str, cover_url: &str) -> anyhow::Result<AudioArtwork> {
        let cache_path = self.cache_path(station_id, cover_url);
        let cached = if self.settings.enabled {
            read_cached(&cache_path).await?
        } else {
            None
        };
        if let Some((artwork, true)) = cached.as_ref() {
            return Ok(artwork.clone());
        }

        let key = cache_path.to_string_lossy().into_owned();
        let lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        let cached = if self.settings.enabled {
            read_cached(&cache_path).await?
        } else {
            None
        };
        if let Some((artwork, true)) = cached.as_ref() {
            return Ok(artwork.clone());
        }
        let stale = cached.map(|(artwork, _)| artwork);

        let _permit = self
            .limiter
            .acquire()
            .await
            .context("radio cover cache is unavailable")?;
        let refreshed = async {
            let data = download_cover(cover_url, &self.download_directory()).await?;
            let mime_type = detect_artwork_mime(&data)
                .context("remote radio cover is not a supported raster image")?
                .to_owned();
            Ok::<_, anyhow::Error>(AudioArtwork { mime_type, data })
        }
        .await;
        match refreshed {
            Ok(artwork) => {
                if self.settings.enabled
                    && let Err(error) = write_cached(&cache_path, &artwork.data).await
                {
                    tracing::warn!(%station_id, %error, "failed to write radio cover cache");
                }
                Ok(artwork)
            }
            Err(error) => {
                if let Some(stale) = stale {
                    tracing::warn!(%station_id, %error, "failed to refresh radio cover; using stale cache");
                    Ok(stale)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub async fn clear_station(&self, station_id: &str) -> anyhow::Result<()> {
        if !self.settings.enabled {
            return Ok(());
        }
        let directory = self.cache_directory();
        let prefix = format!("{}-", digest(station_id));
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".cover"))
            {
                match tokio::fs::remove_file(entry.path()).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn seed(
        &self,
        station_id: &str,
        cover_url: &str,
        data: &[u8],
    ) -> anyhow::Result<()> {
        write_cached(&self.cache_path(station_id, cover_url), data).await
    }

    fn cache_path(&self, station_id: &str, cover_url: &str) -> PathBuf {
        self.cache_directory().join(format!(
            "{}-{}.cover",
            digest(station_id),
            digest(cover_url)
        ))
    }

    fn cache_directory(&self) -> PathBuf {
        self.settings.path.join("radio")
    }

    fn download_directory(&self) -> PathBuf {
        if self.settings.enabled {
            self.cache_directory()
        } else {
            std::env::temp_dir().join("mnest-radio-covers")
        }
    }
}

async fn download_cover(cover_url: &str, cache_directory: &Path) -> anyhow::Result<Vec<u8>> {
    let directory = cache_directory.to_owned();
    tokio::fs::create_dir_all(&directory).await?;
    let partial = directory.join(format!(
        ".mNest-radio-cover-{}.part",
        Uuid::new_v4().simple()
    ));
    let result = async {
        network::download_public_image_to_file(cover_url, &partial, MAX_RADIO_COVER_BYTES as u64)
            .await?;
        Ok::<_, anyhow::Error>(tokio::fs::read(&partial).await?)
    }
    .await;
    let _ = tokio::fs::remove_file(&partial).await;
    result
}

async fn read_cached(path: &Path) -> anyhow::Result<Option<(AudioArtwork, bool)>> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() == 0 || metadata.len() > MAX_RADIO_COVER_BYTES as u64 {
        let _ = tokio::fs::remove_file(path).await;
        return Ok(None);
    }
    let data = tokio::fs::read(path).await?;
    let Some(mime_type) = detect_artwork_mime(&data) else {
        let _ = tokio::fs::remove_file(path).await;
        return Ok(None);
    };
    let fresh = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_none_or(|age| age <= RADIO_COVER_MAX_AGE);
    Ok(Some((
        AudioArtwork {
            mime_type: mime_type.to_owned(),
            data,
        },
        fresh,
    )))
}

async fn write_cached(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let path = path.to_owned();
    let data = data.to_vec();
    tokio::task::spawn_blocking(move || {
        let parent = path.parent().context("radio cover cache has no parent")?;
        fs::create_dir_all(parent)?;
        let temp = Builder::new()
            .prefix(".mNest-radio-cover-")
            .tempfile_in(parent)?;
        fs::write(temp.path(), data)?;
        temp.persist(path).map_err(|error| error.error)?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

fn digest(value: &str) -> String {
    hex::encode(Md5::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nradio-cover";

    #[tokio::test]
    async fn reads_a_fresh_radio_cover_without_contacting_the_remote_url() {
        let directory = tempfile::tempdir().unwrap();
        let settings = CoverCacheSettings {
            enabled: true,
            path: directory.path().to_owned(),
            concurrency: 1,
        };
        let cache = RadioCoverCache::new(settings);
        let path = cache.cache_path("station-1", "https://radio.invalid/cover.png");
        write_cached(&path, PNG).await.unwrap();

        let artwork = cache
            .get("station-1", "https://radio.invalid/cover.png")
            .await
            .unwrap();

        assert_eq!(artwork.mime_type, "image/png");
        assert_eq!(artwork.data, PNG);
    }

    #[tokio::test]
    async fn clears_only_the_requested_stations_cached_covers() {
        let directory = tempfile::tempdir().unwrap();
        let settings = CoverCacheSettings {
            enabled: true,
            path: directory.path().to_owned(),
            concurrency: 1,
        };
        let cache = RadioCoverCache::new(settings);
        let first = cache.cache_path("station-1", "https://radio.invalid/one.png");
        let second = cache.cache_path("station-2", "https://radio.invalid/two.png");
        write_cached(&first, PNG).await.unwrap();
        write_cached(&second, PNG).await.unwrap();

        cache.clear_station("station-1").await.unwrap();

        assert!(!first.exists());
        assert!(second.exists());
    }
}
