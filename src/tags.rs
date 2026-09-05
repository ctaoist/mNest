use std::{
    collections::HashSet,
    ffi::{CStr, CString, c_char, c_uint, c_void},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{DynamicImage, GenericImageView, ImageFormat, imageops::FilterType};
use lofty::{
    config::{ParseOptions, WriteOptions},
    file::{AudioFile, TaggedFileExt},
    picture::{MimeType, Picture, PictureInformation, PictureType},
    prelude::{Accessor, TagExt},
    probe::Probe,
    tag::{ItemKey, Tag},
};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use tempfile::{Builder, NamedTempFile};

use crate::config::{CoverCacheSettings, ToolSettings};

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "flac", "mp3", "mp2", "ape", "wav", "aiff", "aif", "wv", "tta", "m4a", "mp4", "ogg",
    "mpc", "opus", "wma", "wmv", "dsf", "dff", "spx",
];
pub const ARTWORK_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";
pub const EMBEDDED_ARTWORK_MARKER: &str = "embedded";
pub const MISSING_ARTWORK_MARKER: &str = "";
const MAX_CACHED_ARTWORK_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FAST_ID3V2_SCAN_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub albumartist: String,
    pub genre: String,
    pub year: String,
    pub language: String,
    pub lyrics: String,
    pub comment: String,
    pub tracknumber: String,
    pub discnumber: String,
    pub duration: f64,
    pub bit_rate: u32,
    pub size: u64,
    pub suffix: String,
    pub filename: String,
    pub file_full_path: String,
    pub album_img: String,
    pub artwork_mime: String,
    pub artwork_w: u32,
    pub artwork_h: u32,
    pub artwork_size: f64,
    pub is_save_lyrics_file: bool,
    pub is_save_album_cover: bool,
    pub needs_scrape: bool,
}

#[derive(Debug, Clone)]
pub struct AudioArtwork {
    pub mime_type: String,
    pub data: Vec<u8>,
}

pub struct TagWriteResult {
    pub path: PathBuf,
    pub has_artwork: bool,
}

#[derive(Debug, Clone)]
pub struct TagService {
    tools: ToolSettings,
    cover_cache: CoverCacheSettings,
    cover_cache_limiter: Arc<CoverCacheLimiter>,
}

impl TagService {
    pub fn new(tools: ToolSettings) -> Self {
        let cover_cache = CoverCacheSettings::default();
        Self {
            tools,
            cover_cache_limiter: Arc::new(CoverCacheLimiter::new(cover_cache.concurrency)),
            cover_cache,
        }
    }

    pub fn with_cover_cache(tools: ToolSettings, cover_cache: CoverCacheSettings) -> Self {
        Self {
            tools,
            cover_cache_limiter: Arc::new(CoverCacheLimiter::new(cover_cache.concurrency)),
            cover_cache,
        }
    }

    pub fn read(&self, path: &Path) -> anyhow::Result<AudioMetadata> {
        match self.read_lofty(path) {
            Ok(value) => Ok(value),
            Err(lofty_error) if is_taglib_extension(path) => self
                .read_taglib(path)
                .with_context(|| format!("lofty: {lofty_error:#}; TagLib fallback failed")),
            Err(error) => Err(error),
        }
    }

    pub fn read_without_artwork(&self, path: &Path) -> anyhow::Result<AudioMetadata> {
        match self.read_lofty_with_options(path, ParseOptions::new().read_cover_art(false)) {
            Ok(value) => Ok(value),
            Err(lofty_error) if is_taglib_extension(path) => self
                .read_taglib(path)
                .with_context(|| format!("lofty: {lofty_error:#}; TagLib fallback failed")),
            Err(error) => Err(error),
        }
    }

    pub fn read_artwork(&self, path: &Path) -> anyhow::Result<Option<AudioArtwork>> {
        match self.read_artwork_lofty(path) {
            Ok(artwork) => Ok(artwork),
            Err(_) if is_taglib_extension(path) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn read_artwork_cached(
        &self,
        path: &Path,
        track_id: &str,
        modified: i64,
    ) -> anyhow::Result<Option<AudioArtwork>> {
        if self.cover_cache.enabled {
            match self.read_cached_artwork(track_id, modified) {
                Ok(Some(artwork)) => return Ok(Some(artwork)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%track_id, modified, %error, "failed to read cached artwork");
                }
            }
        }

        let _permit = self
            .cover_cache
            .enabled
            .then(|| self.cover_cache_limiter.acquire());
        self.read_artwork_cached_after_lock(path, track_id, modified)
    }

    pub fn read_artwork_cached_with_size(
        &self,
        path: &Path,
        track_id: &str,
        image_id: &str,
        modified: i64,
        size: Option<u32>,
    ) -> anyhow::Result<Option<AudioArtwork>> {
        let Some(size) = size.filter(|size| *size > 0) else {
            return self.read_artwork_cached(path, track_id, modified);
        };

        if self.cover_cache.enabled {
            match self.read_cached_resized_artwork(image_id, modified, size) {
                Ok(Some(artwork)) => return Ok(Some(artwork)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%image_id, modified, size, %error, "failed to read cached resized artwork");
                }
            }
        }

        let _permit = self
            .cover_cache
            .enabled
            .then(|| self.cover_cache_limiter.acquire());
        if self.cover_cache.enabled {
            match self.read_cached_resized_artwork(image_id, modified, size) {
                Ok(Some(artwork)) => return Ok(Some(artwork)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%image_id, modified, size, %error, "failed to recheck cached resized artwork");
                }
            }
        }

        let Some(artwork) = self.read_artwork_cached_after_lock(path, track_id, modified)? else {
            if self.cover_cache.enabled
                && let Err(error) = self.remove_resized_artwork_caches(image_id)
            {
                tracing::warn!(%image_id, %error, "failed to remove stale resized artwork cache");
            }
            return Ok(None);
        };

        let resized = match resize_artwork_to_fit(&artwork, size) {
            Ok(Some(resized)) => resized,
            Ok(None) => return Ok(Some(artwork)),
            Err(error) => {
                tracing::warn!(%image_id, modified, size, %error, "failed to resize artwork; returning original");
                return Ok(Some(artwork));
            }
        };

        if self.cover_cache.enabled
            && let Err(error) =
                self.write_cached_resized_artwork(image_id, modified, size, &resized)
        {
            tracing::warn!(%image_id, modified, size, %error, "failed to update resized artwork cache");
        }

        Ok(Some(resized))
    }

    fn read_artwork_cached_after_lock(
        &self,
        path: &Path,
        track_id: &str,
        modified: i64,
    ) -> anyhow::Result<Option<AudioArtwork>> {
        if self.cover_cache.enabled {
            match self.read_cached_artwork(track_id, modified) {
                Ok(Some(artwork)) => return Ok(Some(artwork)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%track_id, modified, %error, "failed to recheck cached artwork");
                }
            }
        }

        let artwork = self.read_artwork(path)?;

        if self.cover_cache.enabled {
            let cache_result = match artwork.as_ref() {
                Some(artwork) => self.write_cached_artwork(track_id, modified, artwork),
                None => self.remove_stale_artwork_cache(track_id),
            };
            if let Err(error) = cache_result {
                tracing::warn!(%track_id, modified, %error, "failed to update artwork cache");
            }
        }

        Ok(artwork)
    }

    pub fn clear_artwork_cache(&self, track_id: &str) -> anyhow::Result<()> {
        self.clear_artwork_caches([track_id])
    }

    pub fn clear_artwork_caches<'a>(
        &self,
        track_ids: impl IntoIterator<Item = &'a str>,
    ) -> anyhow::Result<()> {
        if !self.cover_cache.enabled {
            return Ok(());
        }
        let prefixes = track_ids
            .into_iter()
            .map(|track_id| self.artwork_cache_prefix(track_id))
            .collect::<HashSet<_>>();
        if prefixes.is_empty() {
            return Ok(());
        }
        let entries = match fs::read_dir(&self.cover_cache.path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let matches_track = entry
                .file_name()
                .to_str()
                .and_then(|name| name.get(..33))
                .is_some_and(|prefix| prefixes.contains(prefix));
            let is_artwork_cache = path
                .extension()
                .is_some_and(|value| value == "artwork" || value == "thumbnail");
            if matches_track && is_artwork_cache {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn read_artwork_lofty(&self, path: &Path) -> anyhow::Result<Option<AudioArtwork>> {
        // On remote filesystems, Lofty's full MP3 probe can take as long as reading the whole
        // track. Most cover-art misses can be answered from the small leading ID3v2 tag alone.
        if matches!(mp3_id3v2_picture_presence(path), Ok(Some(false))) {
            return Ok(None);
        }
        let tagged = Probe::open(path)?
            .options(ParseOptions::new().read_properties(false))
            .read()?;
        let Some(picture) = tagged
            .primary_tag()
            .or_else(|| tagged.first_tag())
            .and_then(|tag| tag.pictures().first())
        else {
            return Ok(None);
        };
        let mime_type = detect_artwork_mime(picture.data())
            .unwrap_or("application/octet-stream")
            .to_owned();
        Ok(Some(AudioArtwork {
            mime_type,
            data: picture.data().to_vec(),
        }))
    }

    fn read_cached_artwork(
        &self,
        track_id: &str,
        modified: i64,
    ) -> anyhow::Result<Option<AudioArtwork>> {
        let (path, _) = self.artwork_cache_entry(track_id, modified);
        self.read_cached_artwork_file(&path)
    }

    fn read_cached_resized_artwork(
        &self,
        image_id: &str,
        modified: i64,
        size: u32,
    ) -> anyhow::Result<Option<AudioArtwork>> {
        let (path, _, _) = self.resized_artwork_cache_entry(image_id, modified, size);
        self.read_cached_artwork_file(&path)
    }

    fn read_cached_artwork_file(&self, path: &Path) -> anyhow::Result<Option<AudioArtwork>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if metadata.len() == 0 || metadata.len() > MAX_CACHED_ARTWORK_BYTES {
            let _ = fs::remove_file(path);
            return Ok(None);
        }
        let data = fs::read(path)?;
        let Some(mime_type) = detect_artwork_mime(&data) else {
            let _ = fs::remove_file(path);
            return Ok(None);
        };
        Ok(Some(AudioArtwork {
            mime_type: mime_type.to_owned(),
            data,
        }))
    }

    fn write_cached_artwork(
        &self,
        track_id: &str,
        modified: i64,
        artwork: &AudioArtwork,
    ) -> anyhow::Result<()> {
        let (path, prefix) = self.artwork_cache_entry(track_id, modified);
        fs::create_dir_all(&self.cover_cache.path)?;
        if artwork.data.len() as u64 <= MAX_CACHED_ARTWORK_BYTES
            && detect_artwork_mime(&artwork.data).is_some()
        {
            atomic_write(&path, &artwork.data, false)?;
            self.prune_artwork_cache(&prefix, Some(&path));
        } else {
            self.prune_artwork_cache(&prefix, None);
        }
        Ok(())
    }

    fn write_cached_resized_artwork(
        &self,
        image_id: &str,
        modified: i64,
        size: u32,
        artwork: &AudioArtwork,
    ) -> anyhow::Result<()> {
        let (path, image_prefix, version_prefix) =
            self.resized_artwork_cache_entry(image_id, modified, size);
        fs::create_dir_all(&self.cover_cache.path)?;
        if artwork.data.len() as u64 <= MAX_CACHED_ARTWORK_BYTES
            && detect_artwork_mime(&artwork.data).is_some()
        {
            atomic_write(&path, &artwork.data, false)?;
            self.prune_resized_artwork_cache(&image_prefix, Some(&version_prefix));
        } else {
            self.prune_resized_artwork_cache(&image_prefix, None);
        }
        Ok(())
    }

    fn remove_stale_artwork_cache(&self, track_id: &str) -> anyhow::Result<()> {
        let prefix = self.artwork_cache_prefix(track_id);
        self.prune_artwork_cache(&prefix, None);
        self.remove_resized_artwork_caches(track_id)?;
        Ok(())
    }

    fn remove_resized_artwork_caches(&self, image_id: &str) -> anyhow::Result<()> {
        let prefix = self.artwork_cache_prefix(image_id);
        self.prune_resized_artwork_cache(&prefix, None);
        Ok(())
    }

    fn artwork_cache_entry(&self, track_id: &str, modified: i64) -> (PathBuf, String) {
        let prefix = self.artwork_cache_prefix(track_id);
        let signature = hex::encode(Md5::digest(format!("{track_id}:{modified}").as_bytes()));
        (
            self.cover_cache
                .path
                .join(format!("{prefix}{signature}.artwork")),
            prefix,
        )
    }

    fn artwork_cache_prefix(&self, track_id: &str) -> String {
        format!("{}-", hex::encode(Md5::digest(track_id.as_bytes())))
    }

    fn resized_artwork_cache_entry(
        &self,
        image_id: &str,
        modified: i64,
        size: u32,
    ) -> (PathBuf, String, String) {
        let image_prefix = self.artwork_cache_prefix(image_id);
        let version = hex::encode(Md5::digest(format!("{image_id}:{modified}").as_bytes()));
        let version_prefix = format!("{image_prefix}{version}-");
        (
            self.cover_cache
                .path
                .join(format!("{version_prefix}{size}.thumbnail")),
            image_prefix,
            version_prefix,
        )
    }

    fn prune_artwork_cache(&self, prefix: &str, keep: Option<&Path>) {
        let Ok(entries) = fs::read_dir(&self.cover_cache.path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let matches_source = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix) && name.ends_with(".artwork"));
            if matches_source && keep.is_none_or(|keep| keep != path) {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn prune_resized_artwork_cache(&self, image_prefix: &str, keep_version: Option<&str>) {
        let Ok(entries) = fs::read_dir(&self.cover_cache.path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let matches_source = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(image_prefix) && name.ends_with(".thumbnail"));
            let keep = keep_version.is_some_and(|version| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(version))
            });
            if matches_source && !keep {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn read_lofty(&self, path: &Path) -> anyhow::Result<AudioMetadata> {
        self.read_lofty_with_options(path, ParseOptions::new())
    }

    fn read_lofty_with_options(
        &self,
        path: &Path,
        options: ParseOptions,
    ) -> anyhow::Result<AudioMetadata> {
        let tagged = Probe::open(path)?.options(options).read()?;
        let properties = tagged.properties();
        let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
        let mut metadata = AudioMetadata {
            duration: properties.duration().as_secs_f64(),
            bit_rate: properties.overall_bitrate().unwrap_or(0),
            size: fs::metadata(path)?.len(),
            suffix: extension(path),
            filename: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            file_full_path: path.to_string_lossy().into_owned(),
            ..Default::default()
        };
        if let Some(tag) = tag {
            metadata.title = owned(tag.title());
            metadata.artist = owned(tag.artist());
            metadata.album = owned(tag.album());
            metadata.albumartist = tag
                .get_string(ItemKey::AlbumArtist)
                .unwrap_or_default()
                .to_owned();
            metadata.genre = owned(tag.genre());
            metadata.year = tag
                .get_string(ItemKey::Year)
                .or_else(|| tag.get_string(ItemKey::RecordingDate))
                .unwrap_or_default()
                .chars()
                .take(4)
                .collect();
            metadata.language = tag
                .get_string(ItemKey::Language)
                .unwrap_or_default()
                .to_owned();
            metadata.lyrics = tag
                .get_string(ItemKey::Lyrics)
                .or_else(|| tag.get_string(ItemKey::UnsyncLyrics))
                .unwrap_or_default()
                .to_owned();
            metadata.comment = owned(tag.comment());
            metadata.tracknumber = tag.track().map(|v| v.to_string()).unwrap_or_default();
            metadata.discnumber = tag.disk().map(|v| v.to_string()).unwrap_or_default();
            if let Some(picture) = tag.pictures().first() {
                let mime = picture
                    .mime_type()
                    .map(MimeType::as_str)
                    .unwrap_or("image/jpeg");
                metadata.album_img =
                    format!("data:{mime};base64,{}", STANDARD.encode(picture.data()));
                metadata.artwork_mime = mime.to_owned();
                metadata.artwork_size = picture.data().len() as f64 / 1_048_576.0;
                if let Ok(info) = PictureInformation::from_picture(picture) {
                    metadata.artwork_w = info.width;
                    metadata.artwork_h = info.height;
                }
            }
        }
        metadata.needs_scrape = required_metadata_missing(&metadata);
        if metadata.title.trim().is_empty() {
            metadata.title = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
        }
        Ok(metadata)
    }

    pub fn write(&self, path: &Path, metadata: &AudioMetadata) -> anyhow::Result<TagWriteResult> {
        if is_taglib_extension(path) && Probe::open(path).and_then(|p| p.read()).is_err() {
            return self.write_taglib_atomic(path, metadata);
        }

        let existing_artwork = if metadata.is_save_album_cover && metadata.album_img.is_empty() {
            self.read_artwork(path)?
        } else {
            None
        };
        let cover_file = cover_file_contents(metadata, existing_artwork.as_ref())?;

        let parent = path
            .parent()
            .context("audio file has no parent directory")?;
        let suffix = format!(".{}", extension(path));
        let temp = Builder::new()
            .prefix(".mNest-")
            .suffix(&suffix)
            .tempfile_in(parent)?;
        fs::copy(path, temp.path())?;
        let mut tagged = Probe::open(temp.path())?.read()?;
        if tagged.primary_tag().is_none() {
            tagged.insert_tag(Tag::new(tagged.primary_tag_type()));
        }
        let tag = tagged
            .primary_tag_mut()
            .context("failed to create primary tag")?;
        set_text(tag, metadata);
        if !metadata.album_img.is_empty() {
            let (mime, data) = decode_image(&metadata.album_img)?;
            while !tag.pictures().is_empty() {
                tag.remove_picture(0);
            }
            tag.push_picture(
                Picture::unchecked(data)
                    .mime_type(MimeType::from_str(&mime))
                    .pic_type(PictureType::CoverFront)
                    .build(),
            );
        }
        let has_artwork = !tag.pictures().is_empty();
        tag.save_to_path(temp.path(), WriteOptions::default())?;
        self.read_without_artwork(temp.path())
            .context("written tag verification failed")?;

        let target = tag_target_path(path, &metadata.filename)?;
        validate_sidecar_target(path, &target, metadata)?;
        persist_audio_file(temp, path, &target)?;
        write_auxiliary_files(path, &target, metadata, cover_file);
        Ok(TagWriteResult {
            path: target,
            has_artwork,
        })
    }

    fn read_taglib(&self, path: &Path) -> anyhow::Result<AudioMetadata> {
        let lib = unsafe { self.load_taglib()? };
        unsafe {
            let file_new = lib
                .get::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(b"taglib_file_new\0")?;
            let file_free = lib.get::<unsafe extern "C" fn(*mut c_void)>(b"taglib_file_free\0")?;
            let file_tag =
                lib.get::<unsafe extern "C" fn(*mut c_void) -> *mut c_void>(b"taglib_file_tag\0")?;
            let path_c = CString::new(path.to_string_lossy().as_bytes())?;
            let file = file_new(path_c.as_ptr());
            if file.is_null() {
                bail!("TagLib cannot open {}", path.display());
            }
            let tag = file_tag(file);
            if tag.is_null() {
                file_free(file);
                bail!("TagLib found no tag");
            }
            let get_string = |name: &[u8]| -> anyhow::Result<String> {
                let function =
                    lib.get::<unsafe extern "C" fn(*mut c_void) -> *const c_char>(name)?;
                let ptr = function(tag);
                Ok(if ptr.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(ptr).to_string_lossy().into_owned()
                })
            };
            let get_uint = |name: &[u8]| -> anyhow::Result<u32> {
                let function = lib.get::<unsafe extern "C" fn(*mut c_void) -> c_uint>(name)?;
                Ok(function(tag))
            };
            let mut value = AudioMetadata {
                title: get_string(b"taglib_tag_title\0")?,
                artist: get_string(b"taglib_tag_artist\0")?,
                album: get_string(b"taglib_tag_album\0")?,
                comment: get_string(b"taglib_tag_comment\0")?,
                genre: get_string(b"taglib_tag_genre\0")?,
                year: get_uint(b"taglib_tag_year\0")?.to_string(),
                tracknumber: get_uint(b"taglib_tag_track\0")?.to_string(),
                size: fs::metadata(path)?.len(),
                suffix: extension(path),
                filename: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                file_full_path: path.to_string_lossy().into_owned(),
                ..Default::default()
            };
            value.needs_scrape = required_metadata_missing(&value);
            if value.title.trim().is_empty() {
                value.title = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
            }
            file_free(file);
            Ok(value)
        }
    }

    fn write_taglib(&self, path: &Path, metadata: &AudioMetadata) -> anyhow::Result<()> {
        let lib = unsafe { self.load_taglib()? };
        unsafe {
            let file_new = lib
                .get::<unsafe extern "C" fn(*const c_char) -> *mut c_void>(b"taglib_file_new\0")?;
            let file_free = lib.get::<unsafe extern "C" fn(*mut c_void)>(b"taglib_file_free\0")?;
            let file_tag =
                lib.get::<unsafe extern "C" fn(*mut c_void) -> *mut c_void>(b"taglib_file_tag\0")?;
            let file_save =
                lib.get::<unsafe extern "C" fn(*mut c_void) -> i32>(b"taglib_file_save\0")?;
            let path_c = CString::new(path.to_string_lossy().as_bytes())?;
            let file = file_new(path_c.as_ptr());
            if file.is_null() {
                bail!("TagLib cannot open {}", path.display());
            }
            let tag = file_tag(file);
            for (symbol, value) in [
                (b"taglib_tag_set_title\0" as &[u8], metadata.title.as_str()),
                (b"taglib_tag_set_artist\0", metadata.artist.as_str()),
                (b"taglib_tag_set_album\0", metadata.album.as_str()),
                (b"taglib_tag_set_comment\0", metadata.comment.as_str()),
                (b"taglib_tag_set_genre\0", metadata.genre.as_str()),
            ] {
                let setter = lib.get::<unsafe extern "C" fn(*mut c_void, *const c_char)>(symbol)?;
                let value = CString::new(value)?;
                setter(tag, value.as_ptr());
            }
            let set_year =
                lib.get::<unsafe extern "C" fn(*mut c_void, c_uint)>(b"taglib_tag_set_year\0")?;
            let set_track =
                lib.get::<unsafe extern "C" fn(*mut c_void, c_uint)>(b"taglib_tag_set_track\0")?;
            set_year(tag, metadata.year.parse().unwrap_or(0));
            set_track(tag, metadata.tracknumber.parse().unwrap_or(0));
            let saved = file_save(file);
            file_free(file);
            if saved == 0 {
                bail!("TagLib failed to save {}", path.display());
            }
        }
        Ok(())
    }

    fn write_taglib_atomic(
        &self,
        path: &Path,
        metadata: &AudioMetadata,
    ) -> anyhow::Result<TagWriteResult> {
        let existing_artwork = if metadata.is_save_album_cover && metadata.album_img.is_empty() {
            self.read_artwork(path)?
        } else {
            None
        };
        let cover_file = cover_file_contents(metadata, existing_artwork.as_ref())?;
        let parent = path
            .parent()
            .context("audio file has no parent directory")?;
        let suffix = format!(".{}", extension(path));
        let temp = Builder::new()
            .prefix(".mNest-")
            .suffix(&suffix)
            .tempfile_in(parent)?;
        fs::copy(path, temp.path())?;
        self.write_taglib(temp.path(), metadata)?;
        let target = tag_target_path(path, &metadata.filename)?;
        validate_sidecar_target(path, &target, metadata)?;
        persist_audio_file(temp, path, &target)?;
        write_auxiliary_files(path, &target, metadata, cover_file);
        Ok(TagWriteResult {
            path: target,
            has_artwork: false,
        })
    }

    unsafe fn load_taglib(&self) -> anyhow::Result<libloading::Library> {
        let candidates = self.tools.taglib.iter().cloned().chain([
            PathBuf::from("libtag_c.so.0"),
            PathBuf::from("libtag_c.so"),
            PathBuf::from("libtag_c.dylib"),
        ]);
        for candidate in candidates {
            if let Ok(lib) = unsafe { libloading::Library::new(&candidate) } {
                return Ok(lib);
            }
        }
        bail!("TagLib C library not found; configure tools.taglib")
    }
}

fn mp3_id3v2_picture_presence(path: &Path) -> io::Result<Option<bool>> {
    if !matches!(extension(path).as_str(), "mp2" | "mp3") {
        return Ok(None);
    }

    let mut file = fs::File::open(path)?;
    let mut header = [0_u8; 10];
    if let Err(error) = file.read_exact(&mut header) {
        return if error.kind() == io::ErrorKind::UnexpectedEof {
            Ok(None)
        } else {
            Err(error)
        };
    }
    if &header[..3] != b"ID3" || !(2..=4).contains(&header[3]) {
        return Ok(None);
    }
    if header[6..10].iter().any(|byte| byte & 0x80 != 0) {
        return Ok(None);
    }
    // ID3v2.2 may compress the complete tag, including frame identifiers.
    if header[3] == 2 && header[5] & 0x40 != 0 {
        return Ok(None);
    }

    let tag_size = header[6..10]
        .iter()
        .fold(0_usize, |size, byte| (size << 7) | usize::from(*byte));
    if tag_size > MAX_FAST_ID3V2_SCAN_BYTES {
        return Ok(None);
    }
    let mut tag = vec![0_u8; tag_size];
    if let Err(error) = file.read_exact(&mut tag) {
        return if error.kind() == io::ErrorKind::UnexpectedEof {
            Ok(None)
        } else {
            Err(error)
        };
    }

    let picture_frame = if header[3] == 2 {
        b"PIC".as_slice()
    } else {
        b"APIC".as_slice()
    };
    Ok(Some(
        tag.windows(picture_frame.len())
            .any(|window| window == picture_frame),
    ))
}

#[derive(Debug)]
struct CoverCacheLimiter {
    limit: usize,
    active: Mutex<usize>,
    available: Condvar,
}

impl CoverCacheLimiter {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            active: Mutex::new(0),
            available: Condvar::new(),
        }
    }

    fn acquire(&self) -> CoverCachePermit<'_> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *active >= self.limit {
            active = self
                .available
                .wait(active)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *active += 1;
        CoverCachePermit { limiter: self }
    }
}

struct CoverCachePermit<'a> {
    limiter: &'a CoverCacheLimiter,
}

impl Drop for CoverCachePermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .limiter
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
        self.limiter.available.notify_one();
    }
}

fn set_text(tag: &mut Tag, value: &AudioMetadata) {
    tag.set_title(value.title.clone());
    tag.set_artist(value.artist.clone());
    tag.set_album(value.album.clone());
    tag.set_genre(value.genre.clone());
    tag.set_comment(value.comment.clone());
    tag.insert_text(ItemKey::AlbumArtist, value.albumartist.clone());
    tag.insert_text(ItemKey::Year, value.year.clone());
    tag.insert_text(ItemKey::RecordingDate, value.year.clone());
    tag.insert_text(ItemKey::Language, value.language.clone());
    tag.insert_text(ItemKey::Lyrics, value.lyrics.clone());
    tag.insert_text(ItemKey::UnsyncLyrics, value.lyrics.clone());
    if let Ok(track) = value.tracknumber.parse() {
        tag.set_track(track);
    }
    if let Ok(disc) = value.discnumber.parse() {
        tag.set_disk(disc);
    }
}

fn required_metadata_missing(metadata: &AudioMetadata) -> bool {
    metadata.title.trim().is_empty()
        || metadata.artist.trim().is_empty()
        || metadata.album.trim().is_empty()
}

fn decode_image(value: &str) -> anyhow::Result<(String, Vec<u8>)> {
    let (_, data) = value.split_once(',').context("invalid image data URL")?;
    let data = STANDARD.decode(data)?;
    let mime = detect_artwork_mime(&data).context("unsupported or invalid artwork image")?;
    Ok((mime.to_owned(), data))
}

fn resize_artwork_to_fit(
    artwork: &AudioArtwork,
    requested_size: u32,
) -> anyhow::Result<Option<AudioArtwork>> {
    let Some(format) = resizeable_artwork_format(&artwork.mime_type) else {
        return Ok(None);
    };
    let original = image::load_from_memory_with_format(&artwork.data, format)
        .context("failed to decode artwork for resizing")?;
    let (width, height) = original.dimensions();
    let original_size = width.max(height);
    if original_size == 0 {
        bail!("artwork has invalid dimensions");
    }
    if requested_size >= original_size {
        return Ok(Some(artwork.clone()));
    }

    let (target_width, target_height) = if width >= height {
        (
            requested_size,
            ((u64::from(height) * u64::from(requested_size)) / u64::from(width)).max(1) as u32,
        )
    } else {
        (
            ((u64::from(width) * u64::from(requested_size)) / u64::from(height)).max(1) as u32,
            requested_size,
        )
    };
    let resized: DynamicImage =
        original.resize_exact(target_width, target_height, FilterType::CatmullRom);
    let mut encoded = io::Cursor::new(Vec::new());
    resized
        .write_to(&mut encoded, format)
        .context("failed to encode resized artwork")?;
    Ok(Some(AudioArtwork {
        mime_type: artwork.mime_type.clone(),
        data: encoded.into_inner(),
    }))
}

fn resizeable_artwork_format(mime_type: &str) -> Option<ImageFormat> {
    match mime_type {
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/png" => Some(ImageFormat::Png),
        "image/webp" => Some(ImageFormat::WebP),
        _ => None,
    }
}

pub fn detect_artwork_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else if data.starts_with(b"BM") {
        Some("image/bmp")
    } else if data.starts_with(b"II*\0") || data.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if data.len() >= 12
        && &data[4..8] == b"ftyp"
        && (&data[8..12] == b"avif" || &data[8..12] == b"avis")
    {
        Some("image/avif")
    } else {
        None
    }
}
fn extension(path: &Path) -> String {
    path.extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase()
}
fn is_taglib_extension(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "wma" | "wmv" | "tta" | "dsf" | "dff"
    )
}
fn owned(value: Option<std::borrow::Cow<'_, str>>) -> String {
    value.map(|v| v.into_owned()).unwrap_or_default()
}
fn sanitize_filename(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn tag_target_path(path: &Path, filename: &str) -> anyhow::Result<PathBuf> {
    let filename = sanitize_filename(filename);
    if filename.is_empty() || filename == path.file_name().unwrap_or_default().to_string_lossy() {
        return Ok(path.to_path_buf());
    }
    anyhow::ensure!(
        !matches!(filename.as_str(), "." | ".."),
        "invalid audio filename"
    );
    let target = path
        .parent()
        .context("audio file has no parent directory")?
        .join(filename);
    anyhow::ensure!(
        extension(&target) == extension(path),
        "renaming cannot change the audio file extension"
    );
    anyhow::ensure!(!target.exists(), "target audio filename already exists");
    Ok(target)
}

fn persist_audio_file(temp: NamedTempFile, original: &Path, target: &Path) -> anyhow::Result<()> {
    if target == original {
        temp.persist(target).map_err(|error| error.error)?;
    } else {
        persist_noclobber_portable(temp, target)?;
    }
    if target != original
        && let Err(error) = fs::remove_file(original)
    {
        let _ = fs::remove_file(target);
        return Err(error.into());
    }
    Ok(())
}

fn validate_sidecar_target(
    original: &Path,
    target: &Path,
    metadata: &AudioMetadata,
) -> anyhow::Result<()> {
    if target == original {
        return Ok(());
    }
    let original_sidecar = original.with_extension("lrc");
    let target_sidecar = target.with_extension("lrc");
    if (original_sidecar.exists() || (metadata.is_save_lyrics_file && !metadata.lyrics.is_empty()))
        && target_sidecar.exists()
    {
        anyhow::bail!("target lyrics filename already exists");
    }
    Ok(())
}

fn write_auxiliary_files(
    original: &Path,
    target: &Path,
    metadata: &AudioMetadata,
    cover_file: Option<Vec<u8>>,
) {
    let original_sidecar = original.with_extension("lrc");
    let target_sidecar = target.with_extension("lrc");
    if metadata.is_save_lyrics_file && !metadata.lyrics.is_empty() {
        match atomic_write(
            &target_sidecar,
            metadata.lyrics.as_bytes(),
            target != original,
        ) {
            Ok(()) => {
                if target != original
                    && original_sidecar.exists()
                    && let Err(error) = fs::remove_file(&original_sidecar)
                {
                    tracing::warn!(path = %original_sidecar.display(), %error, "failed to remove old lyrics sidecar after audio rename");
                }
            }
            Err(error) => {
                tracing::warn!(path = %target_sidecar.display(), %error, "audio tags were saved but lyrics sidecar could not be written")
            }
        }
    } else if target != original && original_sidecar.exists() {
        match fs::hard_link(&original_sidecar, &target_sidecar)
            .or_else(|_| copy_file_noclobber(&original_sidecar, &target_sidecar))
        {
            Ok(()) => {
                if let Err(error) = fs::remove_file(&original_sidecar) {
                    tracing::warn!(path = %original_sidecar.display(), %error, "failed to remove old lyrics sidecar after linking renamed sidecar");
                }
            }
            Err(error) => {
                tracing::warn!(from = %original_sidecar.display(), to = %target_sidecar.display(), %error, "audio was renamed but lyrics sidecar could not be moved")
            }
        }
    }

    if let Some(image) = cover_file {
        let cover = target
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("cover.jpg");
        if let Err(error) = atomic_write(&cover, &image, false) {
            tracing::warn!(path = %cover.display(), %error, "audio tags were saved but cover.jpg could not be written");
        }
    }
}

fn cover_file_contents(
    metadata: &AudioMetadata,
    existing_artwork: Option<&AudioArtwork>,
) -> anyhow::Result<Option<Vec<u8>>> {
    if !metadata.is_save_album_cover {
        return Ok(None);
    }
    if metadata.album_img.is_empty() {
        Ok(existing_artwork.map(|artwork| artwork.data.clone()))
    } else {
        Ok(Some(decode_image(&metadata.album_img)?.1))
    }
}

fn atomic_write(path: &Path, contents: &[u8], no_clobber: bool) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .context("auxiliary file has no parent directory")?;
    let temp = Builder::new()
        .prefix(".mNest-sidecar-")
        .tempfile_in(parent)?;
    fs::write(temp.path(), contents)?;
    if no_clobber {
        persist_noclobber_portable(temp, path)?;
    } else {
        temp.persist(path).map_err(|error| error.error)?;
    }
    Ok(())
}

fn persist_noclobber_portable(temp: NamedTempFile, path: &Path) -> anyhow::Result<()> {
    match temp.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() != io::ErrorKind::AlreadyExists => {
            copy_file_noclobber(error.file.path(), path)?;
            Ok(())
        }
        Err(error) => Err(error.error.into()),
    }
}

fn copy_file_noclobber(source: &Path, target: &Path) -> io::Result<()> {
    let mut source = fs::File::open(source)?;
    let mut target_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let result = io::copy(&mut source, &mut target_file)
        .and_then(|_| target_file.sync_all())
        .map(|_| ());
    if result.is_err() {
        drop(target_file);
        let _ = fs::remove_file(target);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_id3v2_file(path: &Path, version: u8, flags: u8, tag: &[u8]) {
        assert!(tag.len() < 128);
        let mut file = vec![
            b'I',
            b'D',
            b'3',
            version,
            0,
            flags,
            0,
            0,
            0,
            tag.len() as u8,
        ];
        file.extend_from_slice(tag);
        file.extend_from_slice(b"audio");
        fs::write(path, file).unwrap();
    }

    #[test]
    fn quickly_rejects_mp3_id3v2_tags_without_picture_frames() {
        let directory = tempfile::tempdir().unwrap();
        let without_picture = directory.path().join("without-picture.mp3");
        let with_picture = directory.path().join("with-picture.mp3");
        let compressed_v22 = directory.path().join("compressed-v22.mp3");

        write_id3v2_file(&without_picture, 4, 0, b"TIT2 title");
        write_id3v2_file(&with_picture, 4, 0, b"TIT2 title APIC image");
        write_id3v2_file(&compressed_v22, 2, 0x40, b"TIT2 title");

        assert_eq!(
            mp3_id3v2_picture_presence(&without_picture).unwrap(),
            Some(false)
        );
        assert_eq!(
            mp3_id3v2_picture_presence(&with_picture).unwrap(),
            Some(true)
        );
        assert_eq!(mp3_id3v2_picture_presence(&compressed_v22).unwrap(), None);
        assert!(
            TagService::new(ToolSettings::default())
                .read_artwork(&without_picture)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sanitizes_only_path_separators() {
        assert_eq!(sanitize_filename("周杰伦/夜曲.mp3"), "周杰伦_夜曲.mp3");
        assert_eq!(sanitize_filename("AC\\DC.mp3"), "AC_DC.mp3");
    }

    #[test]
    fn accepts_raster_artwork_and_rejects_active_svg_content() {
        assert_eq!(
            detect_artwork_mime(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(
            detect_artwork_mime(b"<svg><script>alert(1)</script></svg>"),
            None
        );
    }

    fn encoded_artwork(
        width: u32,
        height: u32,
        format: ImageFormat,
        mime_type: &str,
    ) -> AudioArtwork {
        let image = DynamicImage::new_rgb8(width, height);
        let mut data = io::Cursor::new(Vec::new());
        image.write_to(&mut data, format).unwrap();
        AudioArtwork {
            mime_type: mime_type.into(),
            data: data.into_inner(),
        }
    }

    fn png_artwork(width: u32, height: u32) -> AudioArtwork {
        encoded_artwork(width, height, ImageFormat::Png, "image/png")
    }

    #[test]
    fn resizes_artwork_by_its_longest_edge_without_upscaling() {
        for (format, mime_type) in [
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::Png, "image/png"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let landscape = encoded_artwork(400, 200, format, mime_type);
            let resized = resize_artwork_to_fit(&landscape, 100).unwrap().unwrap();
            let decoded = image::load_from_memory(&resized.data).unwrap();
            assert_eq!(decoded.dimensions(), (100, 50));
            assert_eq!(resized.mime_type, mime_type);
        }

        let smaller = png_artwork(40, 20);
        let unchanged = resize_artwork_to_fit(&smaller, 100).unwrap().unwrap();
        assert_eq!(unchanged.data, smaller.data);
    }

    #[test]
    fn marks_audio_with_any_required_metadata_missing() {
        let complete = AudioMetadata {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            ..Default::default()
        };
        assert!(!required_metadata_missing(&complete));
        for incomplete in [
            AudioMetadata {
                title: String::new(),
                ..complete.clone()
            },
            AudioMetadata {
                artist: String::new(),
                ..complete.clone()
            },
            AudioMetadata {
                album: String::new(),
                ..complete
            },
        ] {
            assert!(required_metadata_missing(&incomplete));
        }
    }

    #[test]
    fn validates_renamed_audio_targets() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("old.mp3");
        std::fs::write(&original, b"audio").unwrap();
        assert_eq!(
            tag_target_path(&original, "new.mp3").unwrap(),
            directory.path().join("new.mp3")
        );
        assert!(tag_target_path(&original, "new.flac").is_err());
        assert!(tag_target_path(&original, "..").is_err());
        std::fs::write(directory.path().join("exists.mp3"), b"audio").unwrap();
        assert!(tag_target_path(&original, "exists.mp3").is_err());

        let metadata = AudioMetadata {
            filename: "new.mp3".into(),
            is_save_lyrics_file: true,
            lyrics: "lyrics".into(),
            ..Default::default()
        };
        std::fs::write(directory.path().join("new.lrc"), b"existing lyrics").unwrap();
        assert!(
            validate_sidecar_target(&original, &directory.path().join("new.mp3"), &metadata)
                .is_err()
        );
    }

    #[test]
    fn limits_concurrent_cover_cache_work() {
        let limiter = Arc::new(CoverCacheLimiter::new(1));
        let first = limiter.acquire();
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiting = limiter.clone();
        let handle = std::thread::spawn(move || {
            let _permit = waiting.acquire();
            sender.send(()).unwrap();
        });

        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(30))
                .is_err()
        );
        drop(first);
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn caches_artwork_by_track_id_and_invalidates_it_when_mtime_changes() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("covers");
        let service = TagService::with_cover_cache(
            ToolSettings::default(),
            CoverCacheSettings {
                enabled: true,
                path: cache.clone(),
                concurrency: 2,
            },
        );
        let artwork = AudioArtwork {
            mime_type: "image/jpeg".into(),
            data: b"\xff\xd8\xffcached artwork".to_vec(),
        };

        service
            .write_cached_artwork("track-1", 100, &artwork)
            .unwrap();
        let cached = service
            .read_cached_artwork("track-1", 100)
            .unwrap()
            .unwrap();
        assert_eq!(cached.mime_type, "image/jpeg");
        assert_eq!(cached.data, artwork.data);

        let unavailable_source = directory.path().join("unavailable-network-song.flac");
        let cached = service
            .read_artwork_cached(&unavailable_source, "track-1", 100)
            .unwrap()
            .unwrap();
        assert_eq!(cached.data, artwork.data);

        assert!(
            service
                .read_cached_artwork("track-1", 101)
                .unwrap()
                .is_none()
        );
        service
            .write_cached_artwork("track-1", 101, &artwork)
            .unwrap();
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);

        service.remove_stale_artwork_cache("track-1").unwrap();
        assert_eq!(std::fs::read_dir(cache).unwrap().count(), 0);
    }

    #[test]
    fn caches_resized_artwork_by_image_id_mtime_and_size() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("covers");
        let service = TagService::with_cover_cache(
            ToolSettings::default(),
            CoverCacheSettings {
                enabled: true,
                path: cache.clone(),
                concurrency: 2,
            },
        );
        let artwork = png_artwork(100, 50);

        service
            .write_cached_resized_artwork("img-album-1", 100, 100, &artwork)
            .unwrap();
        service
            .write_cached_resized_artwork("img-album-1", 100, 200, &artwork)
            .unwrap();
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 2);
        assert!(
            service
                .read_cached_resized_artwork("img-album-1", 100, 100)
                .unwrap()
                .is_some()
        );
        assert!(
            service
                .read_cached_resized_artwork("img-album-1", 100, 300)
                .unwrap()
                .is_none()
        );

        service
            .write_cached_resized_artwork("img-album-1", 101, 100, &artwork)
            .unwrap();
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
        assert!(
            service
                .read_cached_resized_artwork("img-album-1", 100, 200)
                .unwrap()
                .is_none()
        );
        assert!(
            service
                .read_cached_resized_artwork("img-album-1", 101, 100)
                .unwrap()
                .is_some()
        );
    }
}
