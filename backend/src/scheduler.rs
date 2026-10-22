//! Automatic polling: claims due feeds and refreshes them in the background.
//!
//! Each tick claims a batch of due feeds with database leases, then refreshes
//! them concurrently. Delivery is at least once: a crash or abandoned refresh
//! leaves a lease that expires after 90 seconds, and the feed is claimed again.

use std::time::Duration;

use tokio::{sync::watch, task::JoinSet, time::MissedTickBehavior};

use crate::ingest::{self, RefreshError, Refresher};

/// How often the scheduler looks for due feeds.
pub const TICK: Duration = Duration::from_secs(5);
/// Refresh slots the scheduler leaves free so manual refreshes are not starved.
const MANUAL_RESERVE: usize = 1;
/// How long shutdown waits for in-flight refreshes. Any still running are
/// abandoned; their leases expire and they are retried after restart.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs until `stop` changes to true or its sender is dropped.
pub async fn run(refresher: Refresher, tick: Duration, mut stop: watch::Receiver<bool>) {
    tracing::info!(
        poll_interval_seconds = refresher.poll_interval.as_secs(),
        "scheduler started"
    );
    let mut tasks = JoinSet::new();
    let mut ticks = tokio::time::interval(tick);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = ticks.tick() => {}
        }
        while let Some(finished) = tasks.try_join_next() {
            if let Err(error) = finished {
                tracing::error!(error = %error, "scheduled refresh task panicked");
            }
        }
        let free = refresher
            .refresh_slots
            .available_permits()
            .saturating_sub(MANUAL_RESERVE);
        if free == 0 {
            continue;
        }
        let leases = match ingest::claim_due(&refresher.pool, free as i64).await {
            Ok(leases) => leases,
            Err(error) => {
                crate::db::log_error("scheduler", &error);
                continue;
            }
        };
        for lease in leases {
            let refresher = refresher.clone();
            tasks.spawn(async move {
                let Ok(_permit) = refresher.refresh_slots.clone().acquire_owned().await else {
                    return;
                };
                let feed_id = lease.id;
                match refresher.run(lease).await {
                    Ok(report) => tracing::info!(
                        feed_id,
                        inserted = report.inserted,
                        updated = report.updated,
                        unchanged = report.unchanged,
                        skipped = report.skipped,
                        not_modified = report.not_modified,
                        "scheduled refresh succeeded"
                    ),
                    // `fail` already logged the cause and scheduled a retry.
                    Err(RefreshError::Upstream(_)) => {}
                    Err(error) => {
                        tracing::warn!(feed_id, error = ?error, "scheduled refresh did not complete")
                    }
                }
            });
        }
    }
    tracing::info!(in_flight = tasks.len(), "scheduler stopping");
    let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            abandoned = tasks.len(),
            "scheduler drain timed out; abandoned leases will expire"
        );
        tasks.abort_all();
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, net::SocketAddr};

    use axum::{Router, routing::get};
    use sqlx::PgPool;

    use super::*;
    use crate::{feeds, fetch::Fetcher};

    async fn feed(pool: &PgPool, url: &str, set: &str) -> i64 {
        let id = feeds::register(pool, url).await.unwrap().id;
        if !set.is_empty() {
            sqlx::query(&format!("UPDATE feeds SET {set} WHERE id = $1"))
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
        }
        id
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn concurrent_claims_are_disjoint_and_skip_ineligible_feeds(pool: PgPool) {
        let mut due = HashSet::new();
        for n in 0..4 {
            due.insert(feed(&pool, &format!("https://example.com/{n}"), "").await);
        }
        feed(&pool, "https://example.com/dead", "dead_at = now()").await;
        feed(
            &pool,
            "https://example.com/later",
            "next_poll_at = now() + interval '1 hour'",
        )
        .await;
        feed(
            &pool,
            "https://example.com/backoff",
            "retry_not_before = now() + interval '1 hour'",
        )
        .await;
        let crashed = feed(
            &pool,
            "https://example.com/leased",
            "refresh_token = gen_random_uuid(), refresh_until = now() + interval '1 minute'",
        )
        .await;

        // Two schedulers claim at the same moment, each asking for 3 feeds.
        let (a, b) = tokio::join!(ingest::claim_due(&pool, 3), ingest::claim_due(&pool, 3));
        let a: HashSet<i64> = a.unwrap().iter().map(|lease| lease.id).collect();
        let b: HashSet<i64> = b.unwrap().iter().map(|lease| lease.id).collect();
        assert!(a.is_disjoint(&b), "{a:?} {b:?}");
        assert_eq!(&a | &b, due);
        assert!(ingest::claim_due(&pool, 10).await.unwrap().is_empty());

        // A lease left behind by a crashed refresh is reclaimed once it expires.
        sqlx::query("UPDATE feeds SET refresh_until = now() - interval '1 second' WHERE id = $1")
            .bind(crashed)
            .execute(&pool)
            .await
            .unwrap();
        let reclaimed: Vec<i64> = ingest::claim_due(&pool, 10)
            .await
            .unwrap()
            .iter()
            .map(|lease| lease.id)
            .collect();
        assert_eq!(reclaimed, [crashed]);
    }

    async fn publisher() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let body = |title: &'static str| {
            format!(
                r#"{{"version":"https://jsonfeed.org/version/1.1","title":"{title}","items":[{{"id":"1","title":"{title} post"}}]}}"#
            )
        };
        let app = Router::new()
            .route("/fast", get(move || async move { body("Fast") }))
            .route(
                "/slow",
                get(move || async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    body("Slow")
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (address, server)
    }

    async fn succeeded(pool: &PgPool, id: i64) -> bool {
        sqlx::query_scalar("SELECT last_success_at IS NOT NULL FROM feeds WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn scheduler_refreshes_without_api_calls_and_drains_on_stop(pool: PgPool) {
        let (address, server) = publisher().await;
        // One host allows two concurrent requests, so both feeds start together.
        let slow = feed(&pool, "http://fixture.test/slow", "").await;
        let fast = feed(&pool, "http://fixture.test/fast", "").await;
        let fetcher = Fetcher::local("fixture.test", address);
        let refresher = Refresher::new(pool.clone(), fetcher, Duration::from_secs(600));
        let (stop, stopped) = watch::channel(false);
        let scheduler = tokio::spawn(run(refresher, Duration::from_millis(50), stopped));

        // The fast feed finishes while the slow one is still downloading.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !succeeded(&pool, fast).await {
            assert!(
                tokio::time::Instant::now() < deadline,
                "fast feed never refreshed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !succeeded(&pool, slow).await,
            "slow feed should still be in flight"
        );

        // Stopping waits for the in-flight slow refresh instead of abandoning it.
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), scheduler)
            .await
            .expect("scheduler did not stop")
            .unwrap();
        assert!(succeeded(&pool, slow).await);

        // Articles arrived with no API call, and the next poll is one interval
        // (600 s ± 10%) away.
        let (items, poll_ok): (i64, bool) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM items), \
             bool_and(next_poll_at BETWEEN now() + interval '530 seconds' AND now() + interval '670 seconds') \
             FROM feeds",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((items, poll_ok), (2, true));
        server.abort();
    }
}
