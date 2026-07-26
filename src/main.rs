#![allow(non_snake_case)]

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use axum::{body::Body, http::Request};
use clap::Parser;
use mnest::{AppState, api, config::Settings, db, jobs::JobRunner, providers::ProviderRegistry};
use tokio_util::sync::CancellationToken;
use tower_http::{
    compression::{
        CompressionLayer,
        predicate::{DefaultPredicate, Predicate},
    },
    trace::TraceLayer,
};
use tracing::{info, info_span, warn};

const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(version = mnest::VERSION, about)]
struct Cli {
    #[arg(long, default_value = "data/config.yaml", env = "MNEST_CONFIG")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mnest=info,mNest=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let settings = Arc::new(Settings::load(&cli.config)?);
    settings.validate()?;
    settings.prepare_runtime()?;

    let pool = db::connect(&settings.database).await?;
    db::migrate(&pool).await?;
    db::bootstrap_admin(&pool, &settings.admin, &settings.auth.jwt_secret).await?;
    db::protect_download_source_secrets(&pool, &settings.auth.jwt_secret).await?;
    let providers = Arc::new(ProviderRegistry::new(settings.clone()));
    let state = AppState::new(settings.clone(), pool, providers);
    let shutdown = state.shutdown.clone();
    let runner = JobRunner::start(state.clone(), shutdown.clone()).await?;

    let app = api::router(state)
        .layer(CompressionLayer::new().compress_when(compression_predicate()))
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<Body>| {
                info_span!(
                    "http_request",
                    method = %request.method(),
                    path = %request.uri().path()
                )
            }),
        );

    let address: SocketAddr = format!("{}:{}", settings.server.host, settings.server.port)
        .parse()
        .context("invalid server host or port")?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(%address, config = %cli.config.display(), "mNest server started");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown.clone()))
        .await?;
    shutdown.cancel();
    runner.shutdown().await;
    Ok(())
}

fn compression_predicate() -> impl Predicate {
    DefaultPredicate::new().and(
        |_: axum::http::StatusCode,
         _: axum::http::Version,
         headers: &axum::http::HeaderMap,
         _: &axum::http::Extensions| {
            match headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .map(str::trim)
            {
                Some(content_type) => {
                    content_type.starts_with("text/")
                        || matches!(
                            content_type,
                            "application/json"
                                | "application/javascript"
                                | "application/xml"
                                | "application/xhtml+xml"
                                | "image/svg+xml"
                        )
                }
                None => false,
            }
        },
    )
}

async fn shutdown_signal(shutdown: CancellationToken) {
    wait_for_shutdown_signal().await;
    info!("shutdown requested; waiting up to 5 seconds for active work");
    shutdown.cancel();

    tokio::spawn(async {
        tokio::select! {
            _ = wait_for_shutdown_signal() => {
                warn!("second shutdown signal received; forcing exit");
                std::process::exit(130);
            }
            _ = tokio::time::sleep(SHUTDOWN_GRACE_PERIOD) => {
                warn!("graceful shutdown timed out; forcing exit");
                std::process::exit(0);
            }
        }
    });
}

async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, header},
        response::IntoResponse,
        routing::get,
    };
    use tower::ServiceExt;
    use tower_http::compression::CompressionLayer;

    use super::compression_predicate;

    async fn audio() -> impl IntoResponse {
        ([(header::CONTENT_TYPE, "audio/mpeg")], vec![b'A'; 128])
    }

    async fn text() -> impl IntoResponse {
        ([(header::CONTENT_TYPE, "text/plain")], "A".repeat(128))
    }

    #[tokio::test]
    async fn compression_skips_audio_but_keeps_text_compression() {
        let app = Router::new()
            .route("/audio", get(audio))
            .route("/text", get(text))
            .layer(CompressionLayer::new().compress_when(compression_predicate()));

        let audio = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/audio")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(audio.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(
            to_bytes(audio.into_body(), usize::MAX).await.unwrap(),
            vec![b'A'; 128]
        );

        let text = app
            .oneshot(
                Request::builder()
                    .uri("/text")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            text.headers().get(header::CONTENT_ENCODING).unwrap(),
            "gzip"
        );
    }
}
