use std::collections::HashSet;

use base64::Engine;
use ring::hmac;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, IntoActiveModel,
    QueryFilter, Set,
};

use crate::entities::app_setting;

const PROXY_SETTING_PREFIX: &str = "internet_radio.proxy.";
const PROXY_TOKEN_CONTEXT: &[u8] = b"mnest-internet-radio-proxy-v1\0";
const PROXY_STREAM_PATH: &str = "/api/internet_radio_stream.mp3";

pub async fn proxy_enabled<C: ConnectionTrait>(db: &C, station_id: &str) -> Result<bool, DbErr> {
    Ok(
        app_setting::Entity::find_by_id(proxy_setting_key(station_id))
            .one(db)
            .await?
            .is_some_and(|setting| setting.value == "true"),
    )
}

pub async fn proxy_enabled_ids<C: ConnectionTrait>(db: &C) -> Result<HashSet<String>, DbErr> {
    Ok(app_setting::Entity::find()
        .filter(app_setting::Column::Key.starts_with(PROXY_SETTING_PREFIX))
        .all(db)
        .await?
        .into_iter()
        .filter(|setting| setting.value == "true")
        .filter_map(|setting| {
            setting
                .key
                .strip_prefix(PROXY_SETTING_PREFIX)
                .map(str::to_owned)
        })
        .collect())
}

pub async fn set_proxy_enabled<C: ConnectionTrait>(
    db: &C,
    station_id: &str,
    enabled: bool,
) -> Result<(), DbErr> {
    let key = proxy_setting_key(station_id);
    if !enabled {
        app_setting::Entity::delete_by_id(key).exec(db).await?;
        return Ok(());
    }
    if let Some(setting) = app_setting::Entity::find_by_id(&key).one(db).await? {
        if setting.value != "true" {
            let mut active = setting.into_active_model();
            active.value = Set("true".into());
            active.update(db).await?;
        }
    } else {
        app_setting::ActiveModel {
            key: Set(key),
            value: Set("true".into()),
        }
        .insert(db)
        .await?;
    }
    Ok(())
}

pub fn proxy_stream_url(base_url: &str, station_id: &str, secret: &str) -> String {
    format!(
        "{}{PROXY_STREAM_PATH}?id={}&token={}",
        base_url.trim_end_matches('/'),
        urlencoding::encode(station_id),
        urlencoding::encode(&proxy_token(station_id, secret)),
    )
}

pub fn is_proxy_stream_url(url: &reqwest::Url) -> bool {
    let proxy_path = url.path().ends_with(PROXY_STREAM_PATH)
        || url.path().ends_with("/api/internet_radio_stream/");
    proxy_path && url.query_pairs().any(|(key, _)| key == "id")
}

pub fn verify_proxy_token(station_id: &str, token: &str, secret: &str) -> bool {
    let Ok(signature) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(token) else {
        return false;
    };
    hmac::verify(
        &proxy_signing_key(secret),
        &proxy_token_message(station_id),
        &signature,
    )
    .is_ok()
}

fn proxy_setting_key(station_id: &str) -> String {
    format!("{PROXY_SETTING_PREFIX}{station_id}")
}

fn proxy_token(station_id: &str, secret: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hmac::sign(
        &proxy_signing_key(secret),
        &proxy_token_message(station_id),
    ))
}

fn proxy_signing_key(secret: &str) -> hmac::Key {
    hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes())
}

fn proxy_token_message(station_id: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(PROXY_TOKEN_CONTEXT.len() + station_id.len());
    message.extend_from_slice(PROXY_TOKEN_CONTEXT);
    message.extend_from_slice(station_id.as_bytes());
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_tokens_are_bound_to_the_station_and_secret() {
        let token = proxy_token("radio-1", "a-secret-with-at-least-32-characters");
        assert!(verify_proxy_token(
            "radio-1",
            &token,
            "a-secret-with-at-least-32-characters"
        ));
        assert!(!verify_proxy_token(
            "radio-2",
            &token,
            "a-secret-with-at-least-32-characters"
        ));
        assert!(!verify_proxy_token(
            "radio-1",
            &token,
            "a-different-secret-with-32-characters"
        ));
    }

    #[test]
    fn recognizes_generated_proxy_urls_without_mistaking_original_streams() {
        let proxy = reqwest::Url::parse(
            "https://music.example/base/api/internet_radio_stream.mp3?id=radio-1&token=test",
        )
        .unwrap();
        let removed_proxy = reqwest::Url::parse(
            "https://music.example/api/internet_radio_stream/?id=radio-1&token=test",
        )
        .unwrap();
        let original = reqwest::Url::parse("https://radio.example/live.mp3?id=radio-1").unwrap();

        assert!(is_proxy_stream_url(&proxy));
        assert!(is_proxy_stream_url(&removed_proxy));
        assert!(!is_proxy_stream_url(&original));
    }
}
