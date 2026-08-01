use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, IntoActiveModel, QueryFilter, Set,
};
use serde::{Deserialize, Serialize};

use crate::entities::app_setting;

pub const DEFAULT_PATH: &str = "/data/cache/transcodes";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TranscodeCacheSettings {
    pub enabled: bool,
    pub path: PathBuf,
}

pub struct TranscodeCacheEntry<'a> {
    pub folder_id: &'a str,
    pub artist: &'a str,
    pub album: &'a str,
    pub title: &'a str,
    pub format: &'a str,
    pub bitrate: u32,
    pub offset: Option<&'a str>,
}

impl Default for TranscodeCacheSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            path: DEFAULT_PATH.into(),
        }
    }
}

impl TranscodeCacheSettings {
    pub fn normalized(mut self) -> Self {
        let path = self.path.to_string_lossy();
        self.path = if path.trim().is_empty() {
            DEFAULT_PATH.into()
        } else {
            path.trim().into()
        };
        self
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.enabled {
            if self.path.as_os_str().is_empty() {
                bail!("转码缓存路径不能为空");
            }
            if !self.path.is_absolute() {
                bail!("转码缓存路径必须是绝对路径");
            }
            if self.path == Path::new("/") {
                bail!("转码缓存路径不能是文件系统根目录");
            }
        }
        Ok(())
    }

    pub async fn prepare(&self) -> anyhow::Result<()> {
        self.validate()?;
        if !self.enabled {
            return Ok(());
        }
        tokio::fs::create_dir_all(&self.path)
            .await
            .with_context(|| format!("无法创建转码缓存目录 {}", self.path.display()))?;
        if !tokio::fs::metadata(&self.path).await?.is_dir() {
            bail!("转码缓存路径不是目录：{}", self.path.display());
        }
        Ok(())
    }

    pub fn entry_path(&self, entry: TranscodeCacheEntry<'_>) -> anyhow::Result<PathBuf> {
        let format = entry.format.to_ascii_lowercase();
        if format.is_empty() || !format.chars().all(|value| value.is_ascii_alphanumeric()) {
            bail!("转码格式无法用于缓存文件名");
        }
        let artist = readable_name(entry.artist, "未知歌手", 56);
        let album = readable_name(entry.album, "未知专辑", 56);
        let title = readable_name(entry.title, "未知歌曲", 96);
        let mut filename = format!("{artist}-{album}-{title}-{}k", entry.bitrate.max(1));
        if let Some(offset) = entry
            .offset
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value > 0.0)
        {
            filename.push_str(&format!("-offset-{offset}s"));
        }
        let folder_id = safe_component(entry.folder_id, "library");
        Ok(self
            .path
            .join(folder_id)
            .join(format!("{filename}.{format}")))
    }
}

fn readable_name(value: &str, fallback: &str, max_bytes: usize) -> String {
    let value = value
        .chars()
        .map(|character| {
            if matches!(character, '/' | '\\' | '\0') || character.is_control() {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let value = value.trim();
    let value = if value.is_empty() { fallback } else { value };
    let mut end = value.len().min(max_bytes);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].trim().to_owned()
}

fn safe_component(value: &str, fallback: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        fallback.to_owned()
    } else {
        value
    }
}

pub async fn load<C: ConnectionTrait>(
    db: &C,
    folder_id: &str,
) -> anyhow::Result<TranscodeCacheSettings> {
    let enabled_key = enabled_key(folder_id);
    let path_key = path_key(folder_id);
    let values = app_setting::Entity::find()
        .filter(app_setting::Column::Key.is_in([enabled_key.as_str(), path_key.as_str()]))
        .all(db)
        .await?
        .into_iter()
        .map(|setting| (setting.key, setting.value))
        .collect::<HashMap<_, _>>();
    let defaults = TranscodeCacheSettings::default();
    Ok(TranscodeCacheSettings {
        enabled: values
            .get(&enabled_key)
            .and_then(|value| value.parse::<bool>().ok())
            .unwrap_or(defaults.enabled),
        path: values
            .get(&path_key)
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or(defaults.path),
    })
}

pub async fn save<C: ConnectionTrait>(
    db: &C,
    folder_id: &str,
    settings: &TranscodeCacheSettings,
) -> anyhow::Result<()> {
    settings.validate()?;
    upsert(db, &enabled_key(folder_id), &settings.enabled.to_string()).await?;
    upsert(db, &path_key(folder_id), &settings.path.to_string_lossy()).await?;
    Ok(())
}

pub async fn delete<C: ConnectionTrait>(db: &C, folder_id: &str) -> anyhow::Result<()> {
    app_setting::Entity::delete_many()
        .filter(app_setting::Column::Key.is_in([enabled_key(folder_id), path_key(folder_id)]))
        .exec(db)
        .await?;
    Ok(())
}

fn enabled_key(folder_id: &str) -> String {
    format!("music_folder.{folder_id}.transcode_cache.enabled")
}

fn path_key(folder_id: &str) -> String {
    format!("music_folder.{folder_id}.transcode_cache.path")
}

async fn upsert<C: ConnectionTrait>(db: &C, key: &str, value: &str) -> anyhow::Result<()> {
    if let Some(setting) = app_setting::Entity::find_by_id(key).one(db).await? {
        let mut active = setting.into_active_model();
        active.value = Set(value.to_owned());
        active.update(db).await?;
    } else {
        app_setting::ActiveModel {
            key: Set(key.to_owned()),
            value: Set(value.to_owned()),
        }
        .insert(db)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn saves_settings_in_the_existing_app_settings_table() {
        let db = crate::db::connect(&crate::config::DatabaseSettings {
            driver: "sqlite".into(),
            url: "sqlite::memory:".into(),
            max_connections: 1,
        })
        .await
        .unwrap();
        crate::db::migrate(&db).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let settings = TranscodeCacheSettings {
            enabled: true,
            path: directory.path().join("transcodes"),
        };

        settings.prepare().await.unwrap();
        save(&db, "library-1", &settings).await.unwrap();

        assert_eq!(load(&db, "library-1").await.unwrap(), settings);
        assert_eq!(
            load(&db, "library-2").await.unwrap(),
            TranscodeCacheSettings::default()
        );
        assert!(settings.path.is_dir());

        delete(&db, "library-1").await.unwrap();
        assert_eq!(
            load(&db, "library-1").await.unwrap(),
            TranscodeCacheSettings::default()
        );
    }

    #[test]
    fn cache_entry_is_readable_and_stable_when_the_source_changes() {
        let directory = tempfile::tempdir().unwrap();
        let settings = TranscodeCacheSettings {
            enabled: true,
            path: directory.path().join("cache"),
        };
        let mp3 = settings
            .entry_path(TranscodeCacheEntry {
                folder_id: "library-1",
                artist: "Artist",
                album: "Album",
                title: "Song",
                format: "mp3",
                bitrate: 128,
                offset: None,
            })
            .unwrap();
        let opus = settings
            .entry_path(TranscodeCacheEntry {
                folder_id: "library-1",
                artist: "Artist",
                album: "Album",
                title: "Song",
                format: "opus",
                bitrate: 128,
                offset: None,
            })
            .unwrap();
        let offset = settings
            .entry_path(TranscodeCacheEntry {
                folder_id: "library-1",
                artist: "Artist",
                album: "Album",
                title: "Song",
                format: "mp3",
                bitrate: 128,
                offset: Some("3.5"),
            })
            .unwrap();
        let same_source_after_change = settings
            .entry_path(TranscodeCacheEntry {
                folder_id: "library-1",
                artist: "Artist",
                album: "Album",
                title: "Song",
                format: "mp3",
                bitrate: 128,
                offset: None,
            })
            .unwrap();

        assert_ne!(mp3, opus);
        assert_ne!(mp3, offset);
        assert_eq!(mp3, same_source_after_change);
        assert!(mp3.ends_with("library-1/Artist-Album-Song-128k.mp3"));
        assert_eq!(
            mp3.extension().and_then(|value| value.to_str()),
            Some("mp3")
        );
    }
}
