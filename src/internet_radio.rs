use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::body::Bytes;
use base64::Engine;
use ring::hmac;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, IntoActiveModel,
    QueryFilter, Set,
};
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::entities::app_setting;

const PROXY_SETTING_PREFIX: &str = "internet_radio.proxy.";
const PROXY_TOKEN_CONTEXT: &[u8] = b"mnest-internet-radio-proxy-v1\0";
const PROXY_STREAM_PATH: &str = "/api/internet_radio_stream.mp3";
const SHARED_STREAM_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
pub enum SharedStreamEvent {
    Audio(Bytes),
    Failed(Arc<str>),
    Ended,
}

struct SharedStreamSession {
    source_url: String,
    sender: broadcast::Sender<SharedStreamEvent>,
    cancellation: CancellationToken,
    subscribers: AtomicUsize,
}

impl SharedStreamSession {
    fn try_add_subscriber(&self) -> bool {
        loop {
            if self.cancellation.is_cancelled() {
                return false;
            }
            let subscribers = self.subscribers.load(Ordering::SeqCst);
            if subscribers == 0 {
                return false;
            }
            if self
                .subscribers
                .compare_exchange(
                    subscribers,
                    subscribers.saturating_add(1),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_ok()
            {
                return true;
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct SharedStreamHub {
    sessions: Arc<Mutex<HashMap<String, Arc<SharedStreamSession>>>>,
}

pub struct SharedStreamSubscription {
    receiver: broadcast::Receiver<SharedStreamEvent>,
    session: Arc<SharedStreamSession>,
}

impl SharedStreamSubscription {
    pub async fn recv(&mut self) -> Result<SharedStreamEvent, broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

impl Drop for SharedStreamSubscription {
    fn drop(&mut self) {
        if self.session.subscribers.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.session.cancellation.cancel();
        }
    }
}

pub struct SharedStreamProducer {
    hub: SharedStreamHub,
    station_id: String,
    session: Arc<SharedStreamSession>,
}

impl SharedStreamProducer {
    pub fn cancellation(&self) -> CancellationToken {
        self.session.cancellation.clone()
    }

    pub fn send_audio(&self, chunk: Bytes) -> bool {
        self.session
            .sender
            .send(SharedStreamEvent::Audio(chunk))
            .is_ok()
    }

    pub async fn finish(self, error: Option<String>) {
        let mut sessions = self.hub.sessions.lock().await;
        if sessions
            .get(&self.station_id)
            .is_some_and(|session| Arc::ptr_eq(session, &self.session))
        {
            sessions.remove(&self.station_id);
        }
        drop(sessions);
        let event = match error {
            Some(error) => SharedStreamEvent::Failed(Arc::from(error)),
            None => SharedStreamEvent::Ended,
        };
        let _ = self.session.sender.send(event);
        self.session.cancellation.cancel();
    }
}

impl SharedStreamHub {
    pub async fn cancel(&self, station_id: &str) {
        if let Some(session) = self.sessions.lock().await.remove(station_id) {
            session.cancellation.cancel();
        }
    }

    pub async fn subscribe(
        &self,
        station_id: &str,
        source_url: &str,
    ) -> (SharedStreamSubscription, Option<SharedStreamProducer>) {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(station_id) {
            if session.source_url == source_url && session.try_add_subscriber() {
                return (
                    SharedStreamSubscription {
                        receiver: session.sender.subscribe(),
                        session: session.clone(),
                    },
                    None,
                );
            }
            session.cancellation.cancel();
            sessions.remove(station_id);
        }

        let (sender, receiver) = broadcast::channel(SHARED_STREAM_CHANNEL_CAPACITY);
        let session = Arc::new(SharedStreamSession {
            source_url: source_url.to_owned(),
            sender,
            cancellation: CancellationToken::new(),
            subscribers: AtomicUsize::new(1),
        });
        sessions.insert(station_id.to_owned(), session.clone());
        let subscription = SharedStreamSubscription {
            receiver,
            session: session.clone(),
        };
        let producer = SharedStreamProducer {
            hub: self.clone(),
            station_id: station_id.to_owned(),
            session,
        };
        (subscription, Some(producer))
    }

    #[cfg(test)]
    pub async fn active_streams(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

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

    #[tokio::test]
    async fn shared_streams_reuse_sources_and_replace_changed_urls() {
        let hub = SharedStreamHub::default();
        let (first_subscription, first_producer) = hub
            .subscribe("radio-1", "https://radio.example/first.m3u8")
            .await;
        let first_producer = first_producer.unwrap();
        let first_cancellation = first_producer.cancellation();
        let (same_subscription, same_producer) = hub
            .subscribe("radio-1", "https://radio.example/first.m3u8")
            .await;
        assert!(same_producer.is_none());
        assert_eq!(hub.active_streams().await, 1);

        let (replacement_subscription, replacement_producer) = hub
            .subscribe("radio-1", "https://radio.example/second.m3u8")
            .await;
        let replacement_producer = replacement_producer.unwrap();
        assert!(first_cancellation.is_cancelled());
        first_producer.finish(None).await;
        assert_eq!(hub.active_streams().await, 1);

        drop(first_subscription);
        drop(same_subscription);
        drop(replacement_subscription);
        assert!(replacement_producer.cancellation().is_cancelled());
        replacement_producer.finish(None).await;
        assert_eq!(hub.active_streams().await, 0);
    }
}
