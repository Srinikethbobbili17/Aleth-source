use anyhow::{Context, Result};
use rss_backend::{api, config::Config, db};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    if let Err(error) = dotenvy::dotenv()
        && !error.not_found()
    {
        anyhow::bail!("could not load .env; check its format and permissions");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rss_backend=info,tower_http=info".into()),
        )
        .json()
        .init();

    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await.map_err(|_| {
        anyhow::anyhow!(
            "database connection failed; check DATABASE_URL and PostgreSQL availability"
        )
    })?;
    db::migrate(&pool)
        .await
        .context("database migration failed")?;
    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .context("could not bind HTTP listener")?;
    tracing::info!(address = %config.bind_addr, "RSS backend listening");
    axum::serve(listener, api::router(pool.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed")?;
    pool.close().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("could not install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("could not install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    tracing::info!("shutdown requested");
}
