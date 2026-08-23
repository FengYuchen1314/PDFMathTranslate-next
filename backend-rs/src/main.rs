use std::net::SocketAddr;

use pdf2zh_web::{AppConfig, AppState, build_router};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhowless::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("pdf2zh_web=info,tower_http=info")),
        )
        .init();

    let config = AppConfig::from_env()?;
    let address = SocketAddr::from(([0, 0, 0, 0], config.port));
    let state = AppState::new(config).await?;
    let app = build_router(state.clone());
    state.start_maintenance();

    let listener = TcpListener::bind(address).await?;
    tracing::info!(%address, "Rust web service is ready");
    let stop_accepting = CancellationToken::new();
    let cleanup_complete = CancellationToken::new();
    let signal_state = state.clone();
    let signal_stop = stop_accepting.clone();
    let signal_complete = cleanup_complete.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        tracing::info!("shutdown requested; stopping new HTTP connections");
        signal_stop.cancel();
        signal_state.shutdown().await;
        signal_complete.cancel();
        tracing::info!("active translation cleanup is complete");
    });

    let server_stop = stop_accepting.clone();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move { server_stop.cancelled().await })
        .await;
    if stop_accepting.is_cancelled() {
        cleanup_complete.cancelled().await;
    }
    result?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
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

mod anyhowless {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}
