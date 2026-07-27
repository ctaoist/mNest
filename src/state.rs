use std::sync::Arc;

use sea_orm::DatabaseConnection;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::{
    config::Settings, internet_radio::SharedStreamHub, providers::ProviderRegistry,
    tags::TagService,
};

#[derive(Clone)]
pub struct EventHub {
    jobs: broadcast::Sender<()>,
}

impl EventHub {
    fn new() -> Self {
        let (jobs, _) = broadcast::channel(128);
        Self { jobs }
    }

    pub fn notify_jobs(&self) {
        let _ = self.jobs.send(());
    }

    pub fn subscribe_jobs(&self) -> broadcast::Receiver<()> {
        self.jobs.subscribe()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<Settings>,
    pub db: DatabaseConnection,
    pub providers: Arc<ProviderRegistry>,
    pub tags: Arc<TagService>,
    pub events: EventHub,
    pub radio_streams: SharedStreamHub,
    pub shutdown: CancellationToken,
}

impl AppState {
    pub fn new(
        settings: Arc<Settings>,
        db: DatabaseConnection,
        providers: Arc<ProviderRegistry>,
    ) -> Self {
        let tags = Arc::new(TagService::with_cover_cache(
            settings.tools.clone(),
            settings.cover_cache.clone(),
        ));
        let events = EventHub::new();
        let radio_streams = SharedStreamHub::default();
        let shutdown = CancellationToken::new();
        Self {
            settings,
            db,
            providers,
            tags,
            events,
            radio_streams,
            shutdown,
        }
    }
}
