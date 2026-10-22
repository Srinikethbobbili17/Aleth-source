use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    fetch::{FetchResult, Fetcher, Validators},
    items::{self, ParsedFeed},
};

#[derive(Debug)]
pub enum RefreshError {
    NotFound,
    Busy,
    CoolingDown,
    Upstream(&'static str),
    Database,
    LeaseExpired,
}

impl From<sqlx::Error> for RefreshError {
    fn from(_: sqlx::Error) -> Self {
        Self::Database
    }
}

#[derive(Serialize, Default, Debug)]
pub struct RefreshReport {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub not_modified: bool,
}

#[derive(FromRow)]
struct Lease {
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    validator_url: Option<String>,
}

pub async fn refresh(
    pool: &PgPool,
    fetcher: &Fetcher,
    parser_slots: Arc<Semaphore>,
    feed_id: i64,
) -> Result<RefreshReport, RefreshError> {
    let token = Uuid::new_v4();
    let lease = claim(pool, feed_id, token).await?;
    let validators = Validators {
        url: lease.validator_url,
        etag: lease.etag,
        last_modified: lease.last_modified,
    };
    let fetched = match fetcher.fetch(&lease.url, &validators).await {
        Ok(fetched) => fetched,
        Err(error) => {
            fail(
                pool,
                feed_id,
                token,
                error.message,
                error.retry_after_seconds,
            )
            .await?;
            return Err(RefreshError::Upstream(error.message));
        }
    };
    match fetched {
        FetchResult::NotModified { validators } => {
            complete(pool, feed_id, token, None, validators).await
        }
        FetchResult::Modified { body, validators } => {
            let base = validators.url.clone().unwrap_or(lease.url);
            let permit = match parser_slots.try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    release(pool, feed_id, token).await?;
                    return Err(RefreshError::Busy);
                }
            };
            let parsed = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                items::parse(&body, &base)
            })
            .await
            .unwrap_or(Err("feed parser failed"));
            match parsed {
                Ok(parsed) => complete(pool, feed_id, token, Some(parsed), validators).await,
                Err(message) => {
                    fail(pool, feed_id, token, message, None).await?;
                    Err(RefreshError::Upstream(message))
                }
            }
        }
    }
}

async fn claim(pool: &PgPool, id: i64, token: Uuid) -> Result<Lease, RefreshError> {
    let lease = sqlx::query_as::<_, Lease>(
        "UPDATE feeds SET refresh_token = $2, refresh_until = now() + interval '90 seconds' \
         WHERE id = $1 AND next_fetch_at <= now() AND (refresh_until IS NULL OR refresh_until <= now()) \
         RETURNING url, etag, last_modified, validator_url"
    ).bind(id).bind(token).fetch_optional(pool).await?;
    if let Some(lease) = lease {
        return Ok(lease);
    }
    let state: Option<(Option<DateTime<Utc>>,)> =
        sqlx::query_as("SELECT refresh_until FROM feeds WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    match state {
        None => Err(RefreshError::NotFound),
        Some((Some(until),)) if until > Utc::now() => Err(RefreshError::Busy),
        _ => Err(RefreshError::CoolingDown),
    }
}

async fn release(pool: &PgPool, id: i64, token: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE feeds SET refresh_token = NULL, refresh_until = NULL WHERE id = $1 AND refresh_token = $2")
        .bind(id).bind(token).execute(pool).await?;
    Ok(())
}

async fn fail(
    pool: &PgPool,
    id: i64,
    token: Uuid,
    message: &str,
    retry_after: Option<i64>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE feeds SET failure_count = LEAST(failure_count + 1, 100), last_error = $3, \
         last_fetched_at = now(), refresh_token = NULL, refresh_until = NULL, \
         next_fetch_at = now() + make_interval(secs => GREATEST(LEAST(30.0 * power(2.0, LEAST(failure_count, 7)), 3600.0), COALESCE($4::double precision, 0))) \
         WHERE id = $1 AND refresh_token = $2"
    ).bind(id).bind(token).bind(message).bind(retry_after.map(|n| n as f64)).execute(pool).await?;
    Ok(())
}

// Network and parsing happen before opening this transaction. Fencing prevents
// a cancelled or expired fetch from overwriting a newer refresh's result.
async fn complete(
    pool: &PgPool,
    feed_id: i64,
    token: Uuid,
    parsed: Option<ParsedFeed>,
    validators: Validators,
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
        sqlx::query("UPDATE feeds SET title = $2 WHERE id = $1")
            .bind(feed_id)
            .bind(parsed.title)
            .execute(&mut *tx)
            .await?;
        // Feeds list newest first; insert oldest first so newer items get higher IDs.
        for entry in parsed.entries.into_iter().rev() {
            let row: (i64, Option<i64>) = sqlx::query_as(
                "INSERT INTO items (feed_id, identity) VALUES ($1, $2) \
                 ON CONFLICT (feed_id, identity) DO UPDATE SET last_seen_at = now() RETURNING id, current_version_id"
            ).bind(feed_id).bind(entry.identity).fetch_one(&mut *tx).await?;
            let current_hash: Option<String> =
                sqlx::query_scalar("SELECT content_hash FROM item_versions WHERE id = $1")
                    .bind(row.1)
                    .fetch_optional(&mut *tx)
                    .await?;
            if current_hash.as_deref() == Some(entry.content_hash.as_str()) {
                report.unchanged += 1;
                continue;
            }
            let data = entry.data;
            let version: i64 = sqlx::query_scalar(
                "INSERT INTO item_versions (item_id, content_hash, external_id, title, url, authors, summary, content, content_type, published_at, source_updated_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id"
            ).bind(row.0).bind(entry.content_hash).bind(data.external_id).bind(data.title).bind(data.url)
                .bind(data.authors).bind(data.summary).bind(data.content).bind(data.content_type)
                .bind(data.published_at).bind(data.source_updated_at).fetch_one(&mut *tx).await?;
            sqlx::query("UPDATE items SET current_version_id = $2 WHERE id = $1")
                .bind(row.0)
                .bind(version)
                .execute(&mut *tx)
                .await?;
            if row.1.is_none() {
                report.inserted += 1;
            } else {
                report.updated += 1;
            }
        }
    }
    sqlx::query(
        "UPDATE feeds SET etag = $3, last_modified = $4, validator_url = $5, last_fetched_at = now(), \
         last_success_at = now(), last_error = NULL, failure_count = 0, next_fetch_at = now() + interval '30 seconds', \
         refresh_token = NULL, refresh_until = NULL WHERE id = $1 AND refresh_token = $2"
    ).bind(feed_id).bind(token).bind(validators.etag).bind(validators.last_modified).bind(validators.url)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ready(pool: &PgPool, id: i64) -> Uuid {
        sqlx::query("UPDATE feeds SET next_fetch_at = now() WHERE id = $1")
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
                validators.clone()
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
                validators.clone()
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
                validators.clone()
            )
            .await
            .unwrap()
            .updated,
            1
        );
        let token = ready(&pool, feed.id).await;
        assert!(
            complete(&pool, feed.id, token, None, validators)
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
                Validators::default()
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
        complete(&pool, feed.id, token, Some(parsed), Validators::default())
            .await
            .unwrap();
        let list = items::list(&pool, Some(feed.id), None, 10).await.unwrap();
        let titles: Vec<_> = list.iter().map(|item| item.title.as_deref()).collect();
        assert_eq!(titles, [Some("Newest"), Some("Oldest")]);
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
                Validators::default()
            )
            .await,
            Err(RefreshError::LeaseExpired)
        ));
        fail(&pool, feed.id, newer, "invalid feed", Some(120))
            .await
            .unwrap();
        let row: (String, i32, bool, Option<Uuid>) = sqlx::query_as(
            "SELECT etag, failure_count, next_fetch_at > now() + interval '100 seconds', refresh_token FROM feeds WHERE id = $1"
        ).bind(feed.id).fetch_one(&pool).await.unwrap();
        assert_eq!(row, ("old".to_owned(), 1, true, None));
        assert!(matches!(
            claim(&pool, feed.id, Uuid::new_v4()).await,
            Err(RefreshError::CoolingDown)
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
                }
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
