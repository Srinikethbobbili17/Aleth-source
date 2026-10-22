use std::{env, net::SocketAddr, time::Duration};

use anyhow::{Context, Result};

/// Used when `POLL_INTERVAL_SECONDS` is unset.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30 * 60);

pub struct Config {
    pub bind_addr: SocketAddr,
    pub database_url: String,
    /// Run the automatic polling scheduler in this process.
    pub polling: bool,
    /// Base interval between automatic refreshes of a healthy feed.
    pub poll_interval: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url = env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
        anyhow::ensure!(
            !database_url.trim().is_empty(),
            "DATABASE_URL must not be empty"
        );
        let bind_addr = env::var("BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3000".into())
            .parse()
            .context("BIND_ADDR must be an IP address and port")?;
        let polling = match env::var("POLLING").as_deref() {
            Err(_) | Ok("true" | "1" | "on") => true,
            Ok("false" | "0" | "off") => false,
            Ok(_) => anyhow::bail!("POLLING must be true or false"),
        };
        let poll_interval = match env::var("POLL_INTERVAL_SECONDS") {
            Err(_) => DEFAULT_POLL_INTERVAL,
            Ok(value) => {
                let seconds: u64 = value
                    .parse()
                    .context("POLL_INTERVAL_SECONDS must be a whole number of seconds")?;
                anyhow::ensure!(
                    (60..=86_400).contains(&seconds),
                    "POLL_INTERVAL_SECONDS must be between 60 and 86400"
                );
                Duration::from_secs(seconds)
            }
        };
        Ok(Self {
            bind_addr,
            database_url,
            polling,
            poll_interval,
        })
    }
}
