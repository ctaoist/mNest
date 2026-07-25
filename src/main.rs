#![allow(non_snake_case)]

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use axum::{body::Body, http::Request};
use clap::Parser;
use mnest::{AppState, api, config::Settings, db, jobs::JobRunner, providers::ProviderRegistry};
use tokio_util::sync::CancellationToken;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};
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

    let app = api::router(state).layer(CompressionLayer::new()).layer(
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
