use anyhow::{Context, Result};
use rss_backend::{api, config::Config, db, fetch::Fetcher, ingest::Refresher, scheduler};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck();
    }
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
    let fetcher = Fetcher::new().context("could not initialize feed HTTP client")?;
    let refresher = Refresher::new(pool.clone(), fetcher, config.poll_interval);
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let scheduler = if config.polling {
        Some(tokio::spawn(scheduler::run(
            refresher.clone(),
            scheduler::TICK,
            stopped,
        )))
    } else {
        tracing::info!("automatic polling disabled");
        None
    };
    tracing::info!(address = %config.bind_addr, "RSS backend listening");
    axum::serve(listener, api::router_with(refresher))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = stop.send(true);
        })
        .await
        .context("HTTP server failed")?;
    if let Some(scheduler) = scheduler {
        // Waits for in-flight scheduled refreshes, up to the drain timeout.
        let _ = scheduler.await;
    }
    pool.close().await;
    Ok(())
}

/// `rss-backend healthcheck` exits 0 when the local server reports ready. It
/// lets container runtimes probe the minimal image, which has no shell or curl.
fn healthcheck() -> Result<()> {
    use std::{
        io::{Read, Write},
        net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream},
        time::Duration,
    };
    let bind: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse()
        .context("BIND_ADDR must be an IP address and port")?;
    let address = match bind {
        SocketAddr::V4(v4) if v4.ip().is_unspecified() => (Ipv4Addr::LOCALHOST, v4.port()).into(),
        SocketAddr::V6(v6) if v6.ip().is_unspecified() => (Ipv6Addr::LOCALHOST, v6.port()).into(),
        other => other,
    };
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .context("server is not accepting connections")?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(b"GET /health/ready HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut status_line = [0u8; 16];
    stream.read_exact(&mut status_line)?;
    let status = String::from_utf8_lossy(&status_line);
    anyhow::ensure!(
        status.split_whitespace().nth(1) == Some("200"),
        "server is not ready"
    );
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
