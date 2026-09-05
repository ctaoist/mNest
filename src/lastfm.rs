use std::{collections::BTreeMap, time::Duration};

use anyhow::{Context, ensure};
use md5::{Digest, Md5};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, IntoActiveModel, QueryFilter, Set,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    AppState,
    artist_credit::ArtistCredit,
    auth::{decrypt_server_secret, encrypt_server_secret},
    entities::{app_setting, track},
};

const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
const AUTHORIZE_URL: &str = "https://www.last.fm/api/auth/";
const API_KEY_SETTING: &str = "lastfm.api_key";
const SHARED_SECRET_SETTING: &str = "lastfm.shared_secret";
const USER_SETTING_PREFIX: &str = "lastfm.user.";
const MAX_SCROBBLE_BATCH: usize = 50;

#[derive(Debug, Clone, Serialize)]
pub struct LastFmStatus {
    pub configured: bool,
    pub connected: bool,
    pub authorization_pending: bool,
    pub username: String,
    pub api_key: String,
    pub has_shared_secret: bool,
}

#[derive(Debug)]
struct Credentials {
    api_key: String,
    shared_secret: String,
    session_key: Option<String>,
}

pub async fn status(state: &AppState, user_id: &str) -> anyhow::Result<LastFmStatus> {
    let api_key = setting(state, API_KEY_SETTING).await?.unwrap_or_default();
    let has_shared_secret = setting_exists(state, SHARED_SECRET_SETTING).await?;
    let username_key = user_setting(user_id, "username");
    let session_key = user_setting(user_id, "session_key");
    let pending_token = user_setting(user_id, "pending_token");
    let username = setting(state, &username_key).await?.unwrap_or_default();
    let connected = setting_exists(state, &session_key).await? && !username.is_empty();
    Ok(LastFmStatus {
        configured: !api_key.is_empty() && has_shared_secret,
        connected,
        authorization_pending: setting_exists(state, &pending_token).await?,
        username,
        api_key,
        has_shared_secret,
    })
}

pub async fn save_config(
    state: &AppState,
    user_id: &str,
    api_key: &str,
    shared_secret: &str,
) -> anyhow::Result<LastFmStatus> {
    let api_key = api_key.trim();
    validate_api_key(api_key)?;
    let shared_secret = shared_secret.trim();
    let previous_api_key = setting(state, API_KEY_SETTING).await?.unwrap_or_default();
    if !shared_secret.is_empty() {
        validate_shared_secret(shared_secret)?;
    } else {
        ensure!(
            setting_exists(state, SHARED_SECRET_SETTING).await?,
            "首次配置必须填写 Last.fm Shared Secret"
        );
        ensure!(
            previous_api_key == api_key,
            "修改 Last.fm API Key 时必须同时填写对应的 Shared Secret"
        );
    }

    let credentials_changed = previous_api_key != api_key || !shared_secret.is_empty();
    set_setting(state, API_KEY_SETTING, api_key).await?;
    if !shared_secret.is_empty() {
        set_secret(state, SHARED_SECRET_SETTING, shared_secret).await?;
    }
    if credentials_changed {
        clear_all_authorizations(state).await?;
    }
    status(state, user_id).await
}

pub async fn begin_authorization(state: &AppState, user_id: &str) -> anyhow::Result<String> {
    let credentials = credentials(state, None).await?;
    let response = post_signed(&credentials, "auth.getToken", Vec::new()).await?;
    let token = response
        .get("token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("Last.fm 未返回授权 Token")?;
    set_secret(state, &user_setting(user_id, "pending_token"), token).await?;

    let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("api_key", &credentials.api_key)
        .append_pair("token", token);
    Ok(url.into())
}

pub async fn complete_authorization(
    state: &AppState,
    user_id: &str,
) -> anyhow::Result<LastFmStatus> {
    let credentials = credentials(state, None).await?;
    let pending_token = user_setting(user_id, "pending_token");
    let token = secret_setting(state, &pending_token)
        .await?
        .context("没有待完成的 Last.fm 授权")?;
    let response = post_signed(
        &credentials,
        "auth.getSession",
        vec![("token".to_owned(), token)],
    )
    .await?;
    let session = response
        .get("session")
        .and_then(Value::as_object)
        .context("Last.fm 未返回授权会话")?;
    let username = session
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("Last.fm 未返回用户名")?;
    let session_key = session
        .get("key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("Last.fm 未返回 Session Key")?;
    set_secret(state, &user_setting(user_id, "session_key"), session_key).await?;
    set_setting(state, &user_setting(user_id, "username"), username).await?;
    delete_setting(state, &pending_token).await?;
    status(state, user_id).await
}

pub async fn disconnect(state: &AppState, user_id: &str) -> anyhow::Result<LastFmStatus> {
    delete_user_authorization(&state.db, user_id).await?;
    status(state, user_id).await
}

pub async fn report(
    state: &AppState,
    user_id: &str,
    tracks: Vec<(track::Model, i64)>,
    submission: bool,
) -> anyhow::Result<()> {
    let Some(credentials) = optional_scrobble_credentials(state, user_id).await? else {
        return Ok(());
    };
    let tracks = tracks
        .into_iter()
        .filter_map(|(track, timestamp)| {
            let artist = track_artist(&track)?;
            (!track.title.trim().is_empty()).then_some((track, artist, timestamp))
        })
        .collect::<Vec<_>>();
    if tracks.is_empty() {
        return Ok(());
    }

    if submission {
        for chunk in tracks.chunks(MAX_SCROBBLE_BATCH) {
            let mut parameters = Vec::with_capacity(chunk.len() * 5);
            for (index, (track, artist, timestamp)) in chunk.iter().enumerate() {
                parameters.push((format!("artist[{index}]"), artist.clone()));
                parameters.push((format!("track[{index}]"), track.title.clone()));
                parameters.push((format!("timestamp[{index}]"), timestamp.to_string()));
                if !track.album_name.trim().is_empty() {
                    parameters.push((format!("album[{index}]"), track.album_name.clone()));
                }
                if track.duration > 0.0 {
                    parameters.push((
                        format!("duration[{index}]"),
                        (track.duration.round() as i64).to_string(),
                    ));
                }
            }
            post_signed(&credentials, "track.scrobble", parameters).await?;
        }
    } else if let Some((track, artist, _)) = tracks.last() {
        let mut parameters = vec![
            ("artist".to_owned(), artist.clone()),
            ("track".to_owned(), track.title.clone()),
        ];
        if !track.album_name.trim().is_empty() {
            parameters.push(("album".to_owned(), track.album_name.clone()));
        }
        if track.duration > 0.0 {
            parameters.push((
                "duration".to_owned(),
                (track.duration.round() as i64).to_string(),
            ));
        }
        post_signed(&credentials, "track.updateNowPlaying", parameters).await?;
    }
    Ok(())
}

fn validate_api_key(value: &str) -> anyhow::Result<()> {
    ensure!(
        (8..=128).contains(&value.len())
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric()),
        "Last.fm API Key 格式无效"
    );
    Ok(())
}

fn validate_shared_secret(value: &str) -> anyhow::Result<()> {
    ensure!(
        (8..=256).contains(&value.len())
            && !value.chars().any(char::is_control)
            && !value.chars().any(char::is_whitespace),
        "Last.fm Shared Secret 格式无效"
    );
    Ok(())
}

fn track_artist(track: &track::Model) -> Option<String> {
    let artists = serde_json::from_str::<Vec<ArtistCredit>>(&track.artists_json).ok()?;
    let names = artists
        .into_iter()
        .map(|artist| artist.name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| names.join("; "))
}

async fn post_signed(
    credentials: &Credentials,
    method: &str,
    parameters: Vec<(String, String)>,
) -> anyhow::Result<Value> {
    let parameters = signed_parameters(
        &credentials.api_key,
        &credentials.shared_secret,
        credentials.session_key.as_deref(),
        method,
        parameters,
    );
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("mNest/lastfm")
        .build()?
        .post(API_URL)
        .form(&parameters)
        .send()
        .await
        .map_err(|error| error.without_url())?;
    let status = response.status();
    let value = response
        .json::<Value>()
        .await
        .context("Last.fm 返回了无效响应")?;
    if !status.is_success() {
        anyhow::bail!("Last.fm 返回 HTTP {}", status.as_u16());
    }
    if let Some(code) = value.get("error").and_then(Value::as_i64) {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        anyhow::bail!("Last.fm 错误 {code}：{message}");
    }
    Ok(value)
}

fn signed_parameters(
    api_key: &str,
    shared_secret: &str,
    session_key: Option<&str>,
    method: &str,
    parameters: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut signed = BTreeMap::from([
        ("api_key".to_owned(), api_key.to_owned()),
        ("method".to_owned(), method.to_owned()),
    ]);
    if let Some(session_key) = session_key {
        signed.insert("sk".to_owned(), session_key.to_owned());
    }
    signed.extend(parameters);
    let mut signature_source = String::new();
    for (key, value) in &signed {
        signature_source.push_str(key);
        signature_source.push_str(value);
    }
    signature_source.push_str(shared_secret);
    signed.insert(
        "api_sig".to_owned(),
        format!("{:x}", Md5::digest(signature_source.as_bytes())),
    );
    signed.insert("format".to_owned(), "json".to_owned());
    signed.into_iter().collect()
}

async fn credentials(state: &AppState, user_id: Option<&str>) -> anyhow::Result<Credentials> {
    let api_key = setting(state, API_KEY_SETTING)
        .await?
        .filter(|value| !value.is_empty())
        .context("请先配置 Last.fm API Key")?;
    let shared_secret = secret_setting(state, SHARED_SECRET_SETTING)
        .await?
        .context("请先配置 Last.fm Shared Secret")?;
    let session_key = if let Some(user_id) = user_id {
        let value = secret_setting(state, &user_setting(user_id, "session_key")).await?;
        ensure!(value.is_some(), "Last.fm 尚未完成账户授权");
        value
    } else {
        None
    };
    Ok(Credentials {
        api_key,
        shared_secret,
        session_key,
    })
}

async fn optional_scrobble_credentials(
    state: &AppState,
    user_id: &str,
) -> anyhow::Result<Option<Credentials>> {
    let current = status(state, user_id).await?;
    if !current.configured || !current.connected {
        return Ok(None);
    }
    credentials(state, Some(user_id)).await.map(Some)
}

async fn clear_all_authorizations(state: &AppState) -> anyhow::Result<()> {
    app_setting::Entity::delete_many()
        .filter(app_setting::Column::Key.starts_with(USER_SETTING_PREFIX))
        .exec(&state.db)
        .await?;
    Ok(())
}

pub async fn delete_user_authorization<C: ConnectionTrait>(
    db: &C,
    user_id: &str,
) -> anyhow::Result<()> {
    for field in ["session_key", "username", "pending_token"] {
        app_setting::Entity::delete_by_id(user_setting(user_id, field))
            .exec(db)
            .await?;
    }
    Ok(())
}

async fn setting(state: &AppState, key: &str) -> anyhow::Result<Option<String>> {
    Ok(app_setting::Entity::find_by_id(key)
        .one(&state.db)
        .await?
        .map(|setting| setting.value))
}

async fn setting_exists(state: &AppState, key: &str) -> anyhow::Result<bool> {
    Ok(app_setting::Entity::find_by_id(key)
        .one(&state.db)
        .await?
        .is_some_and(|setting| !setting.value.is_empty()))
}

async fn secret_setting(state: &AppState, key: &str) -> anyhow::Result<Option<String>> {
    let Some(value) = setting(state, key).await? else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(decrypt_server_secret(
        &value,
        &state.settings.auth.jwt_secret,
        &secret_aad(key),
    )?))
}

async fn set_setting(state: &AppState, key: &str, value: &str) -> anyhow::Result<()> {
    if let Some(existing) = app_setting::Entity::find_by_id(key).one(&state.db).await? {
        let mut active = existing.into_active_model();
        active.value = Set(value.to_owned());
        active.update(&state.db).await?;
    } else {
        app_setting::ActiveModel {
            key: Set(key.to_owned()),
            value: Set(value.to_owned()),
        }
        .insert(&state.db)
        .await?;
    }
    Ok(())
}

async fn set_secret(state: &AppState, key: &str, value: &str) -> anyhow::Result<()> {
    let encrypted =
        encrypt_server_secret(value, &state.settings.auth.jwt_secret, &secret_aad(key))?;
    set_setting(state, key, &encrypted).await
}

async fn delete_setting(state: &AppState, key: &str) -> anyhow::Result<()> {
    app_setting::Entity::delete_by_id(key)
        .exec(&state.db)
        .await?;
    Ok(())
}

fn secret_aad(key: &str) -> String {
    format!("app-setting:{key}")
}

fn user_setting(user_id: &str, field: &str) -> String {
    format!("{USER_SETTING_PREFIX}{user_id}.{field}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
        let providers = Arc::new(crate::providers::ProviderRegistry::new(settings.clone()));
        AppState::new(settings, db, providers)
    }

    #[test]
    fn signs_lastfm_parameters_in_lexicographic_order_without_format() {
        let parameters = signed_parameters("key", "secret", None, "auth.getToken", Vec::new());
        let values = parameters.into_iter().collect::<BTreeMap<_, _>>();

        assert_eq!(
            values.get("api_sig").unwrap(),
            "b4705499705a550b07ca058a15bde9b0"
        );
        assert_eq!(values.get("format").unwrap(), "json");
    }

    #[test]
    fn uses_only_structured_artists_for_scrobbling() {
        let mut track = test_track();
        assert_eq!(track_artist(&track).as_deref(), Some("Artist A; Artist B"));

        track.artists_json = "not-json".into();
        track.artist_name = "Legacy Artist".into();
        assert_eq!(track_artist(&track), None);
    }

    #[tokio::test]
    async fn stores_and_deletes_lastfm_authorization_per_user() {
        let state = test_state().await;
        set_setting(&state, API_KEY_SETTING, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();
        set_secret(
            &state,
            SHARED_SECRET_SETTING,
            "abcdef0123456789abcdef0123456789",
        )
        .await
        .unwrap();
        for (user_id, username, session_key) in [
            ("user-a", "lastfm-a", "session-a"),
            ("user-b", "lastfm-b", "session-b"),
        ] {
            set_setting(&state, &user_setting(user_id, "username"), username)
                .await
                .unwrap();
            set_secret(&state, &user_setting(user_id, "session_key"), session_key)
                .await
                .unwrap();
        }

        assert_eq!(status(&state, "user-a").await.unwrap().username, "lastfm-a");
        assert_eq!(status(&state, "user-b").await.unwrap().username, "lastfm-b");
        assert_eq!(
            credentials(&state, Some("user-a"))
                .await
                .unwrap()
                .session_key
                .as_deref(),
            Some("session-a")
        );
        assert_eq!(
            credentials(&state, Some("user-b"))
                .await
                .unwrap()
                .session_key
                .as_deref(),
            Some("session-b")
        );
        let stored_a = app_setting::Entity::find_by_id(user_setting("user-a", "session_key"))
            .one(&state.db)
            .await
            .unwrap()
            .unwrap()
            .value;
        let stored_b = app_setting::Entity::find_by_id(user_setting("user-b", "session_key"))
            .one(&state.db)
            .await
            .unwrap()
            .unwrap()
            .value;
        assert!(stored_a.starts_with("v1:"));
        assert!(stored_b.starts_with("v1:"));
        assert!(!stored_a.contains("session-a"));
        assert!(!stored_b.contains("session-b"));
        assert_ne!(stored_a, stored_b);

        delete_user_authorization(&state.db, "user-a")
            .await
            .unwrap();
        assert!(!status(&state, "user-a").await.unwrap().connected);
        assert!(status(&state, "user-b").await.unwrap().connected);

        save_config(
            &state,
            "user-a",
            "0123456789abcdef0123456789abcdef",
            "fedcba9876543210fedcba9876543210",
        )
        .await
        .unwrap();
        assert!(!status(&state, "user-b").await.unwrap().connected);
    }

    fn test_track() -> track::Model {
        track::Model {
            id: "track-1".into(),
            folder_id: "folder-1".into(),
            path: "/music/song.flac".into(),
            relative_path: "song.flac".into(),
            title: "Song".into(),
            artist_id: "artist-1".into(),
            artist_name: "Artist A; Artist B".into(),
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
}
