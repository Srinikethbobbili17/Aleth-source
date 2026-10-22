use std::{sync::Arc, time::Duration};

use serde::Serialize;
use sqlx::{FromRow, PgPool};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    failure::{Failure, FailureKind},
    fetch::{FetchResult, Fetcher, Validators},
    items::{self, ParsedFeed},
};

#[derive(Debug)]
pub enum RefreshError {
    NotFound,
    /// Another refresh of this feed holds the lease.
    Busy,
    /// The feed was refreshed recently; manual refreshes wait for `manual_refresh_after`.
    CoolingDown,
    /// The feed failed recently; every refresh waits for `retry_not_before`.
    BackingOff,
    /// The feed downloaded, but no parser slot freed up in time.
    ParserBusy,
    Upstream(Failure),
    Database,
    LeaseExpired,
}

impl From<sqlx::Error> for RefreshError {
    fn from(error: sqlx::Error) -> Self {
        crate::db::log_error("refresh", &error);
        Self::Database
    }
}

#[derive(Serialize, Default, Debug)]
pub struct RefreshReport {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Older entries left out because the feed exceeds the entry limit.
    pub skipped: usize,
    pub not_modified: bool,
}

/// Consecutive failures after which a feed is marked dead. Automatic polling
/// skips dead feeds; a successful manual refresh revives one.
pub const DEAD_AFTER_FAILURES: i32 = 10;

/// How long a downloaded feed waits for a parser slot before giving up. The
/// download is already paid for, so waiting briefly beats discarding it.
const PARSER_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(100)
} else {
    Duration::from_secs(10)
};

/// Ownership of one feed's refresh, recorded in `feeds.refresh_token`.
#[derive(FromRow, Debug)]
pub struct Lease {
    pub(crate) id: i64,
    #[sqlx(rename = "refresh_token")]
    token: Uuid,
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    validator_url: Option<String>,
}

const LEASE_COLUMNS: &str = "id, refresh_token, url, etag, last_modified, validator_url";

/// Refresh machinery shared by the API and the scheduler, so manual and
/// automatic refreshes draw on the same capacity.
#[derive(Clone)]
pub struct Refresher {
    pub pool: PgPool,
    pub fetcher: Fetcher,
    /// Parsing runs on blocking threads; this bounds how many run at once.
    pub parser_slots: Arc<Semaphore>,
    /// Concurrent refreshes, manual and scheduled together.
    pub refresh_slots: Arc<Semaphore>,
    /// Base interval between automatic refreshes of a healthy feed.
    pub poll_interval: Duration,
}

impl Refresher {
    pub fn new(pool: PgPool, fetcher: Fetcher, poll_interval: Duration) -> Self {
        Self {
            pool,
            fetcher,
            parser_slots: Arc::new(Semaphore::new(2)),
            refresh_slots: Arc::new(Semaphore::new(4)),
            poll_interval,
        }
    }

    /// A user-requested refresh. It waits for the cooldown and failure backoff
    /// but ignores the polling schedule.
    pub async fn refresh_now(&self, feed_id: i64) -> Result<RefreshReport, RefreshError> {
        let lease = claim(&self.pool, feed_id, Uuid::new_v4()).await?;
        self.run(lease).await
    }

    /// Fetches, parses, and stores one claimed feed, then releases its lease.
    pub async fn run(&self, lease: Lease) -> Result<RefreshReport, RefreshError> {
        let Lease {
            id: feed_id,
            token,
            url,
            etag,
            last_modified,
            validator_url,
        } = lease;
        let pool = &self.pool;
        let poll = self.poll_interval;
        let validators = Validators {
            url: validator_url,
            etag,
            last_modified,
        };
        let fetched = match self.fetcher.fetch(&url, &validators).await {
            Ok(fetched) => fetched,
            Err(failure) => {
                fail(pool, feed_id, token, &failure, poll).await?;
                return Err(RefreshError::Upstream(failure));
            }
        };
        match fetched {
            FetchResult::NotModified { validators, moved } => {
                let moved_to = moved.then(|| validators.url.clone()).flatten();
                complete(pool, feed_id, token, None, validators, moved_to, poll).await
            }
            FetchResult::Modified {
                body,
                validators,
                moved,
            } => {
                let moved_to = moved.then(|| validators.url.clone()).flatten();
                let base = validators.url.clone().unwrap_or(url);
                let slots = self.parser_slots.clone();
                let permit = match tokio::time::timeout(PARSER_WAIT, slots.acquire_owned()).await {
                    Ok(Ok(permit)) => permit,
                    _ => {
                        // The feed stays due, so the scheduler retries it soon.
                        release(pool, feed_id, token).await?;
                        return Err(RefreshError::ParserBusy);
                    }
                };
                let parsed = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    items::parse(&body, &base)
                })
                .await
                .unwrap_or_else(|_| {
                    Err(Failure::new(FailureKind::Internal, "feed parser crashed"))
                });
                match parsed {
                    Ok(parsed) => {
                        let parsed = Some(parsed);
                        complete(pool, feed_id, token, parsed, validators, moved_to, poll).await
                    }
                    Err(failure) => {
                        fail(pool, feed_id, token, &failure, poll).await?;
                        Err(RefreshError::Upstream(failure))
                    }
                }
            }
        }
    }
}

async fn claim(pool: &PgPool, id: i64, token: Uuid) -> Result<Lease, RefreshError> {
    let lease = sqlx::query_as::<_, Lease>(&format!(
        "UPDATE feeds SET refresh_token = $2, refresh_until = now() + interval '90 seconds' \
         WHERE id = $1 AND (refresh_until IS NULL OR refresh_until <= now()) \
         AND (retry_not_before IS NULL OR retry_not_before <= now()) \
         AND (manual_refresh_after IS NULL OR manual_refresh_after <= now()) \
         RETURNING {LEASE_COLUMNS}"
    ))
    .bind(id)
    .bind(token)
    .fetch_optional(pool)
    .await?;
    if let Some(lease) = lease {
        return Ok(lease);
    }
    let state: Option<(bool, bool)> = sqlx::query_as(
        "SELECT COALESCE(refresh_until > now(), false), COALESCE(retry_not_before > now(), false) \
         FROM feeds WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    match state {
        None => Err(RefreshError::NotFound),
        Some((true, _)) => Err(RefreshError::Busy),
        Some((_, true)) => Err(RefreshError::BackingOff),
        Some(_) => Err(RefreshError::CoolingDown),
    }
}

/// Claims up to `limit` due feeds for the scheduler, oldest due first. `SKIP
/// LOCKED` lets several schedulers share one database without claiming the
/// same feed, and an expired lease from a crashed refresh is claimable again.
pub async fn claim_due(pool: &PgPool, limit: i64) -> Result<Vec<Lease>, sqlx::Error> {
    sqlx::query_as(&format!(
        "UPDATE feeds SET refresh_token = gen_random_uuid(), refresh_until = now() + interval '90 seconds' \
         WHERE id IN ( \
             SELECT id FROM feeds \
             WHERE dead_at IS NULL AND next_poll_at <= now() \
             AND (retry_not_before IS NULL OR retry_not_before <= now()) \
             AND (refresh_until IS NULL OR refresh_until <= now()) \
             ORDER BY next_poll_at, id LIMIT $1 \
             FOR UPDATE SKIP LOCKED) \
         RETURNING {LEASE_COLUMNS}"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
}

async fn release(pool: &PgPool, id: i64, token: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE feeds SET refresh_token = NULL, refresh_until = NULL WHERE id = $1 AND refresh_token = $2")
        .bind(id).bind(token).execute(pool).await?;
    Ok(())
}

// Backoff grows from 30 seconds to one hour; a larger Retry-After wins.
const BACKOFF_SECONDS: &str = "GREATEST(LEAST(30.0 * power(2.0, LEAST(failure_count, 7)), 3600.0), \
                               COALESCE($4::double precision, 0))";

async fn fail(
    pool: &PgPool,
    id: i64,
    token: Uuid,
    failure: &Failure,
    poll_interval: Duration,
) -> Result<(), sqlx::Error> {
    tracing::warn!(feed_id = id, code = failure.code(), "feed refresh failed");
    // Automatic polling waits at least one poll interval after a failure, so
    // a feed is declared dead only after hours of failures, not minutes.
    let failures: Option<i32> = sqlx::query_scalar(&format!(
        "UPDATE feeds SET failure_count = LEAST(failure_count + 1, 100), last_error = $3, last_error_code = $5, \
         dead_at = CASE WHEN failure_count + 1 >= $6 THEN COALESCE(dead_at, now()) ELSE dead_at END, \
         last_fetched_at = now(), refresh_token = NULL, refresh_until = NULL, \
         retry_not_before = now() + make_interval(secs => {BACKOFF_SECONDS}), \
         next_poll_at = now() + make_interval(secs => GREATEST({BACKOFF_SECONDS}, $7 * (0.9 + random() * 0.2))) \
         WHERE id = $1 AND refresh_token = $2 RETURNING failure_count"
    ))
    .bind(id)
    .bind(token)
    .bind(failure.message.as_ref())
    .bind(failure.retry_after_seconds.map(|n| n as f64))
    .bind(failure.code())
    .bind(DEAD_AFTER_FAILURES)
    .bind(poll_interval.as_secs_f64())
    .fetch_optional(pool)
    .await?;
    if failures == Some(DEAD_AFTER_FAILURES) {
        tracing::warn!(feed_id = id, failures, "feed marked dead");
    }
    Ok(())
}

/// A new version row, sent to PostgreSQL as one JSON array per refresh.
#[derive(Serialize)]
struct NewVersion<'a> {
    item_id: i64,
    content_hash: &'a str,
    #[serde(flatten)]
    data: &'a items::ArticleData,
}

// Network and parsing happen before opening this transaction. Fencing prevents
// a cancelled or expired fetch from overwriting a newer refresh's result.
// Storage takes a fixed number of statements however many entries the feed has.
async fn complete(
    pool: &PgPool,
    feed_id: i64,
    token: Uuid,
    parsed: Option<ParsedFeed>,
    validators: Validators,
    moved_to: Option<String>,
    poll_interval: Duration,
) -> Result<RefreshReport, RefreshError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    let owns_lease: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM feeds WHERE id = $1 AND refresh_token = $2 AND refresh_until > now() FOR UPDATE"
    ).bind(feed_id).bind(token).fetch_optional(&mut *tx).await?;
    if owns_lease.is_none() {
        return Err(RefreshError::LeaseExpired);
    }
    let mut report = RefreshReport {
        not_modified: parsed.is_none(),
        ..Default::default()
    };
    if let Some(parsed) = parsed {
        report.skipped = parsed.skipped;
        sqlx::query("UPDATE feeds SET title = $2 WHERE id = $1")
            .bind(feed_id)
            .bind(parsed.title)
            .execute(&mut *tx)
            .await?;
        // Feeds list newest first; insert oldest first so newer items get higher IDs.
        let entries: Vec<_> = parsed.entries.into_iter().rev().collect();
        let identities: Vec<&str> = entries
            .iter()
            .map(|entry| entry.identity.as_str())
            .collect();
        // Upsert every item and read its current fingerprint in one statement.
        // Identities are unique within a parse, so each row is touched once.
        let rows: Vec<(i64, Option<i64>, Option<String>)> = sqlx::query_as(
            "WITH input AS (SELECT identity, n FROM unnest($2::text[]) WITH ORDINALITY AS t(identity, n)), \
             upserted AS (INSERT INTO items (feed_id, identity) SELECT $1, identity FROM input ORDER BY n \
                          ON CONFLICT (feed_id, identity) DO UPDATE SET last_seen_at = now() \
                          RETURNING id, identity, current_version_id) \
             SELECT u.id, u.current_version_id, v.content_hash FROM input \
             JOIN upserted u USING (identity) \
             LEFT JOIN item_versions v ON v.id = u.current_version_id \
             ORDER BY input.n",
        )
        .bind(feed_id)
        .bind(&identities)
        .fetch_all(&mut *tx)
        .await?;
        if rows.len() != entries.len() {
            tracing::error!(feed_id, "item upsert returned an unexpected row count");
            return Err(RefreshError::Database);
        }
        let mut new_versions = Vec::new();
        let mut rehash: (Vec<i64>, Vec<&str>) = Default::default();
        for (entry, (item_id, current_version, current_hash)) in entries.iter().zip(&rows) {
            match current_hash.as_deref() {
                Some(hash) if hash == entry.content_hash => report.unchanged += 1,
                Some(hash) if hash == entry.legacy_hash => {
                    // Same content under the old fingerprint: upgrade it in place.
                    report.unchanged += 1;
                    rehash.0.extend(*current_version);
                    rehash.1.push(&entry.content_hash);
                }
                _ => {
                    if current_version.is_none() {
                        report.inserted += 1;
                    } else {
                        report.updated += 1;
                    }
                    new_versions.push(NewVersion {
                        item_id: *item_id,
                        content_hash: &entry.content_hash,
                        data: &entry.data,
                    });
                }
            }
        }
        if !new_versions.is_empty() {
            let json = serde_json::to_string(&new_versions).map_err(|_| RefreshError::Database)?;
            let created: Vec<(i64, i64)> = sqlx::query_as(
                "INSERT INTO item_versions (item_id, content_hash, external_id, title, url, authors, \
                     summary, content, content_type, published_at, source_updated_at) \
                 SELECT item_id, content_hash, external_id, title, url, \
                     ARRAY(SELECT jsonb_array_elements_text(authors)), summary, content, content_type, \
                     published_at, source_updated_at \
                 FROM jsonb_to_recordset($1::jsonb) AS v(item_id bigint, content_hash text, \
                     external_id text, title text, url text, authors jsonb, summary text, content text, \
                     content_type text, published_at timestamptz, source_updated_at timestamptz) \
                 RETURNING item_id, id",
            )
            .bind(json)
            .fetch_all(&mut *tx)
            .await?;
            let (item_ids, version_ids): (Vec<i64>, Vec<i64>) = created.into_iter().unzip();
            sqlx::query(
                "UPDATE items SET current_version_id = v.id \
                 FROM unnest($1::bigint[], $2::bigint[]) AS v(item_id, id) WHERE items.id = v.item_id",
            )
            .bind(&item_ids)
            .bind(&version_ids)
            .execute(&mut *tx)
            .await?;
        }
        if !rehash.0.is_empty() {
            sqlx::query(
                "UPDATE item_versions SET content_hash = v.hash \
                 FROM unnest($1::bigint[], $2::text[]) AS v(id, hash) WHERE item_versions.id = v.id",
            )
            .bind(&rehash.0)
            .bind(&rehash.1)
            .execute(&mut *tx)
            .await?;
        }
    }
    if let Some(url) = moved_to {
        // Follow a permanent move, unless another registered feed already
        // uses that URL; merging two feeds is out of scope here.
        let moved = sqlx::query(
            "UPDATE feeds SET url = $2 WHERE id = $1 AND url <> $2 \
             AND NOT EXISTS (SELECT 1 FROM feeds WHERE url = $2)",
        )
        .bind(feed_id)
        .bind(&url)
        .execute(&mut *tx)
        .await?;
        if moved.rows_affected() == 1 {
            tracing::info!(feed_id, "feed URL updated after a permanent redirect");
        }
    }
    // Next automatic poll: the base interval with ±10% jitter, so feeds added
    // together drift apart instead of being fetched in bursts.
    sqlx::query(
        "UPDATE feeds SET etag = $3, last_modified = $4, validator_url = $5, last_fetched_at = now(), \
         last_success_at = now(), last_error = NULL, last_error_code = NULL, dead_at = NULL, failure_count = 0, \
         retry_not_before = NULL, manual_refresh_after = now() + interval '30 seconds', \
         next_poll_at = now() + make_interval(secs => $6 * (0.9 + random() * 0.2)), \
         refresh_token = NULL, refresh_until = NULL WHERE id = $1 AND refresh_token = $2"
    ).bind(feed_id).bind(token).bind(validators.etag).bind(validators.last_modified).bind(validators.url)
        .bind(poll_interval.as_secs_f64())
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: Duration = Duration::from_secs(600);

    async fn ready(pool: &PgPool, id: i64) -> Uuid {
        sqlx::query(
            "UPDATE feeds SET next_poll_at = now(), manual_refresh_after = NULL, retry_not_before = NULL WHERE id = $1",
        )
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        let token = Uuid::new_v4();
        claim(pool, id, token).await.unwrap();
        token
    }

    fn document(title: &str) -> ParsedFeed {
        items::parse(format!(r#"{{"version":"https://jsonfeed.org/version/1.1","title":"Feed","items":[{{"id":"same","title":"{title}","content_text":"Text"}}]}}"#).as_bytes(), "https://example.com/feed").unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn ingestion_versions_idempotency_feed_scope_and_304(pool: PgPool) {
        let feed = crate::feeds::register(&pool, "https://example.com/feed")
            .await
            .unwrap();
        let validators = Validators {
            url: Some(feed.url.clone()),
            etag: Some("v1".into()),
            last_modified: None,
        };
        let token = ready(&pool, feed.id).await;
        assert_eq!(
            complete(
                &pool,
                feed.id,
                token,
                Some(document("One")),
                validators.clone(),
                None,
                POLL
            )
            .await
            .unwrap()
            .inserted,
            1
        );
        let token = ready(&pool, feed.id).await;
        assert_eq!(
            complete(
                &pool,
                feed.id,
                token,
                Some(document("One")),
                validators.clone(),
                None,
                POLL
            )
            .await
            .unwrap()
            .unchanged,
            1
        );
        let token = ready(&pool, feed.id).await;
        assert_eq!(
            complete(
                &pool,
                feed.id,
                token,
                Some(document("Two")),
                validators.clone(),
                None,
                POLL
            )
            .await
            .unwrap()
            .updated,
            1
        );
        let token = ready(&pool, feed.id).await;
        assert!(
            complete(&pool, feed.id, token, None, validators, None, POLL)
                .await
                .unwrap()
                .not_modified
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM item_versions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
        let list = items::list(&pool, Some(feed.id), None, 10).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(
            items::get(&pool, list[0].id)
                .await
                .unwrap()
                .unwrap()
                .data
                .title
                .as_deref(),
            Some("Two")
        );
        let other = crate::feeds::register(&pool, "https://other.example/feed")
            .await
            .unwrap();
        let token = ready(&pool, other.id).await;
        assert_eq!(
            complete(
                &pool,
                other.id,
                token,
                Some(document("One")),
                Validators::default(),
                None,
                POLL
            )
            .await
            .unwrap()
            .inserted,
            1
        );
        let all = items::list(&pool, None, None, 1).await.unwrap();
        assert_eq!(all[0].feed_id, other.id);
        assert_eq!(
            items::list(&pool, None, Some(all[0].id), 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn newest_feed_entry_is_listed_first(pool: PgPool) {
        let feed = crate::feeds::register(&pool, "https://example.com/feed")
            .await
            .unwrap();
        let token = ready(&pool, feed.id).await;
        let parsed = items::parse(
            br#"{"version":"https://jsonfeed.org/version/1.1","title":"Feed","items":[
            {"id":"new","title":"Newest","content_text":"B"},
            {"id":"old","title":"Oldest","content_text":"A"}]}"#,
            "https://example.com/feed",
        )
        .unwrap();
        complete(
            &pool,
            feed.id,
            token,
            Some(parsed),
            Validators::default(),
            None,
            POLL,
        )
        .await
        .unwrap();
        let list = items::list(&pool, Some(feed.id), None, 10).await.unwrap();
        let titles: Vec<_> = list.iter().map(|item| item.title.as_deref()).collect();
        assert_eq!(titles, [Some("Newest"), Some("Oldest")]);
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn ten_failures_mark_a_feed_dead_and_success_revives_it(pool: PgPool) {
        let feed = crate::feeds::register(&pool, "https://example.com/feed")
            .await
            .unwrap();
        let failure = Failure::new(
            FailureKind::DnsFailed,
            "could not resolve the feed's host name",
        );
        for attempt in 1..=DEAD_AFTER_FAILURES {
            let token = ready(&pool, feed.id).await;
            fail(&pool, feed.id, token, &failure, POLL).await.unwrap();
            let dead = crate::feeds::get(&pool, feed.id)
                .await
                .unwrap()
                .unwrap()
                .dead_at;
            assert_eq!(
                dead.is_some(),
                attempt == DEAD_AFTER_FAILURES,
                "attempt {attempt}"
            );
        }
        // A manual refresh is still allowed, and success revives the feed.
        let token = ready(&pool, feed.id).await;
        let report = complete(
            &pool,
            feed.id,
            token,
            Some(document("Back")),
            Validators::default(),
            None,
            POLL,
        )
        .await
        .unwrap();
        assert_eq!(report.inserted, 1);
        let revived = crate::feeds::get(&pool, feed.id).await.unwrap().unwrap();
        assert_eq!((revived.dead_at, revived.failure_count), (None, 0));
    }

    fn stamped(title: &str, modified: &str) -> ParsedFeed {
        items::parse(
            format!(
                r#"{{"version":"https://jsonfeed.org/version/1.1","title":"Feed","items":[
                {{"id":"a","title":"{title}","content_text":"Text","date_modified":"{modified}"}}]}}"#
            )
            .as_bytes(),
            "https://example.com/feed",
        )
        .unwrap()
    }

    async fn versions(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM item_versions")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn timestamp_only_changes_do_not_create_versions(pool: PgPool) {
        let feed = crate::feeds::register(&pool, "https://example.com/feed")
            .await
            .unwrap();
        let store = |parsed| {
            let pool = pool.clone();
            async move {
                let token = ready(&pool, feed.id).await;
                complete(
                    &pool,
                    feed.id,
                    token,
                    Some(parsed),
                    Validators::default(),
                    None,
                    POLL,
                )
                .await
                .unwrap()
            }
        };
        assert_eq!(
            store(stamped("One", "2026-10-01T00:00:00Z")).await.inserted,
            1
        );
        // Only the publisher's `updated` timestamp moved.
        assert_eq!(
            store(stamped("One", "2026-10-02T00:00:00Z"))
                .await
                .unchanged,
            1
        );
        assert_eq!(versions(&pool).await, 1);
        // A real edit still makes a version.
        assert_eq!(
            store(stamped("Two", "2026-10-02T00:00:00Z")).await.updated,
            1
        );
        assert_eq!(versions(&pool).await, 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn legacy_fingerprints_are_upgraded_without_new_versions(pool: PgPool) {
        let feed = crate::feeds::register(&pool, "https://example.com/feed")
            .await
            .unwrap();
        let parsed = stamped("One", "2026-10-01T00:00:00Z");
        let (legacy, current) = (
            parsed.entries[0].legacy_hash.clone(),
            parsed.entries[0].content_hash.clone(),
        );
        let token = ready(&pool, feed.id).await;
        complete(
            &pool,
            feed.id,
            token,
            Some(parsed),
            Validators::default(),
            None,
            POLL,
        )
        .await
        .unwrap();
        // Simulate a version stored before the fingerprint change.
        sqlx::query("UPDATE item_versions SET content_hash = $1")
            .bind(&legacy)
            .execute(&pool)
            .await
            .unwrap();
        let token = ready(&pool, feed.id).await;
        let report = complete(
            &pool,
            feed.id,
            token,
            Some(stamped("One", "2026-10-01T00:00:00Z")),
            Validators::default(),
            None,
            POLL,
        )
        .await
        .unwrap();
        assert_eq!((report.unchanged, report.updated), (1, 0));
        let stored: String = sqlx::query_scalar("SELECT content_hash FROM item_versions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, current);
        assert_eq!(versions(&pool).await, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn permanent_moves_update_the_feed_url_unless_taken(pool: PgPool) {
        let feed = crate::feeds::register(&pool, "https://old.example.com/feed")
            .await
            .unwrap();
        let other = crate::feeds::register(&pool, "https://taken.example.com/feed")
            .await
            .unwrap();
        let url = |id| {
            let pool = pool.clone();
            async move { crate::feeds::get(&pool, id).await.unwrap().unwrap().url }
        };
        for (target, expected) in [
            // Another feed already uses this URL, so the move is not applied.
            (
                "https://taken.example.com/feed",
                "https://old.example.com/feed",
            ),
            (
                "https://new.example.com/feed",
                "https://new.example.com/feed",
            ),
        ] {
            let token = ready(&pool, feed.id).await;
            complete(
                &pool,
                feed.id,
                token,
                None,
                Validators::default(),
                Some(target.to_owned()),
                POLL,
            )
            .await
            .unwrap();
            assert_eq!(url(feed.id).await, expected);
        }
        assert_eq!(url(other.id).await, "https://taken.example.com/feed");
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn skipped_entries_are_reported(pool: PgPool) {
        let feed = crate::feeds::register(&pool, "https://example.com/feed")
            .await
            .unwrap();
        let items: Vec<String> = (0..1005)
            .map(|i| format!(r#"{{"id":"{i}","content_text":"x"}}"#))
            .collect();
        let document = format!(
            r#"{{"version":"https://jsonfeed.org/version/1.1","title":"Big","items":[{}]}}"#,
            items.join(",")
        );
        let parsed = items::parse(document.as_bytes(), "https://example.com/feed").unwrap();
        let token = ready(&pool, feed.id).await;
        let report = complete(
            &pool,
            feed.id,
            token,
            Some(parsed),
            Validators::default(),
            None,
            POLL,
        )
        .await
        .unwrap();
        assert_eq!((report.inserted, report.skipped), (1000, 5));
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn refresh_fencing_failure_backoff_and_validator_preservation(pool: PgPool) {
        let feed = crate::feeds::register(&pool, "https://example.com/feed")
            .await
            .unwrap();
        let token = ready(&pool, feed.id).await;
        assert!(matches!(
            claim(&pool, feed.id, Uuid::new_v4()).await,
            Err(RefreshError::Busy)
        ));
        sqlx::query("UPDATE feeds SET etag = 'old', refresh_until = now() - interval '1 second' WHERE id = $1")
            .bind(feed.id).execute(&pool).await.unwrap();
        let newer = ready(&pool, feed.id).await;
        assert!(matches!(
            complete(
                &pool,
                feed.id,
                token,
                Some(document("Old")),
                Validators::default(),
                None,
                POLL
            )
            .await,
            Err(RefreshError::LeaseExpired)
        ));
        let failure = Failure {
            retry_after_seconds: Some(120),
            ..Failure::new(
                FailureKind::HttpStatus,
                "publisher returned HTTP 429 Too Many Requests",
            )
        };
        fail(&pool, feed.id, newer, &failure, POLL).await.unwrap();
        // Retry-After (120 s) sets the backoff; automatic polling waits at
        // least the poll interval (600 s ± 10%) after a failure.
        let row: (String, i32, bool, bool, Option<Uuid>, Option<String>) = sqlx::query_as(
            "SELECT etag, failure_count, \
             retry_not_before BETWEEN now() + interval '100 seconds' AND now() + interval '130 seconds', \
             next_poll_at > now() + interval '530 seconds', refresh_token, last_error_code \
             FROM feeds WHERE id = $1",
        )
        .bind(feed.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            row,
            (
                "old".to_owned(),
                1,
                true,
                true,
                None,
                Some("http_status".to_owned())
            )
        );
        // Manual refreshes respect the failure backoff and get a distinct error.
        assert!(matches!(
            claim(&pool, feed.id, Uuid::new_v4()).await,
            Err(RefreshError::BackingOff)
        ));
        assert!(items::list(&pool, None, None, 10).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn persistence_failure_rolls_back_items_and_validators(pool: PgPool) {
        let feed = crate::feeds::register(&pool, "https://example.com/feed")
            .await
            .unwrap();
        sqlx::query(
            "ALTER TABLE item_versions ADD CONSTRAINT test_reject_title CHECK (title <> 'reject')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut parsed = document("accept");
        let mut rejected = document("reject").entries.remove(0);
        rejected.identity = "different".into();
        parsed.entries.push(rejected);
        let token = ready(&pool, feed.id).await;
        assert!(matches!(
            complete(
                &pool,
                feed.id,
                token,
                Some(parsed),
                Validators {
                    etag: Some("new".into()),
                    ..Default::default()
                },
                None,
                POLL
            )
            .await,
            Err(RefreshError::Database)
        ));
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM items")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let etag: Option<String> = sqlx::query_scalar("SELECT etag FROM feeds WHERE id = $1")
            .bind(feed.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(etag.is_none());
    }
}
