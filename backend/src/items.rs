use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use url::Url;

use crate::{
    failure::{Failure, FailureKind},
    feeds::{normalize_feed_url, validate_content},
};

#[derive(Debug)]
pub struct ParsedFeed {
    pub title: Option<String>,
    pub entries: Vec<NormalizedEntry>,
}

#[derive(Debug)]
pub struct NormalizedEntry {
    pub identity: String,
    pub content_hash: String,
    pub data: ArticleData,
}

#[derive(Debug, Serialize, FromRow)]
pub struct ArticleData {
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub authors: Vec<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
    pub content_type: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub source_updated_at: Option<DateTime<Utc>>,
}

pub fn parse(content: &[u8], source_url: &str) -> Result<ParsedFeed, Failure> {
    validate_content(content)?;
    let base = Url::parse(source_url)
        .map_err(|_| Failure::new(FailureKind::InvalidUrl, "invalid feed source URL"))?;
    let parsed = feed_rs::parser::Builder::new()
        .base_uri(Some(source_url))
        // Preserve missing IDs rather than accepting feed-rs' random fallback.
        .id_generator(|_, _, _| String::new())
        .sanitize_content(true)
        .build()
        .parse(content)
        .map_err(|_| {
            Failure::new(
                FailureKind::ParseFailed,
                "could not parse RSS, Atom, or JSON Feed content",
            )
        })?;
    if parsed.entries.len() > 1000 {
        return Err(Failure::new(
            FailureKind::TooManyEntries,
            "feed has more than 1000 entries",
        ));
    }
    let mut entries = Vec::with_capacity(parsed.entries.len());
    let mut identities = std::collections::HashSet::new();
    for entry in parsed.entries {
        let url = entry
            .links
            .iter()
            .filter(|link| link.rel.as_deref().is_none_or(|rel| rel == "alternate"))
            .find_map(|link| {
                base.join(&link.href)
                    .ok()
                    .and_then(|url| normalize_feed_url(url.as_str()).ok())
            });
        let external_id = if entry.id.trim().is_empty() {
            None
        } else {
            Some(entry.id)
        };
        let (content, content_type) = entry
            .content
            .map(|content| {
                let kind = content.content_type.to_string();
                // Do not inline arbitrary XML, base64 binaries, or remote enclosures.
                if matches!(
                    kind.as_str(),
                    "text/html" | "text/plain" | "application/xhtml+xml"
                ) {
                    (content.body, Some(kind))
                } else {
                    (None, None)
                }
            })
            .unwrap_or_default();
        let data = ArticleData {
            external_id,
            title: entry.title.map(|text| text.content),
            url,
            authors: entry
                .authors
                .into_iter()
                .map(|author| author.name)
                .collect(),
            summary: entry.summary.map(|text| text.content),
            content,
            content_type,
            published_at: entry.published,
            source_updated_at: entry.updated,
        };
        let content_hash =
            hash(&serde_json::to_vec(&data).map_err(|_| {
                Failure::new(FailureKind::Internal, "could not normalize feed item")
            })?);
        let identity = if let Some(id) = &data.external_id {
            format!("id:{}", hash(id.as_bytes()))
        } else if let Some(url) = &data.url {
            format!("url:{}", hash(url.as_bytes()))
        } else {
            // With no persistent ID or link, content changes cannot safely be
            // attributed to an existing item. Identical snapshots remain stable.
            format!("content:{content_hash}")
        };
        // First occurrence wins when a publisher repeats an ID in one document.
        if identities.insert(identity.clone()) {
            entries.push(NormalizedEntry {
                identity,
                content_hash,
                data,
            });
        }
    }
    Ok(ParsedFeed {
        title: parsed.title.map(|title| title.content),
        entries,
    })
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Serialize, FromRow)]
pub struct ItemSummary {
    pub id: i64,
    pub feed_id: i64,
    pub version_id: i64,
    pub title: Option<String>,
    pub url: Option<String>,
    pub authors: Vec<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub first_seen_at: DateTime<Utc>,
}

#[derive(Serialize, FromRow)]
pub struct ItemDetail {
    pub id: i64,
    pub feed_id: i64,
    pub version_id: i64,
    pub first_seen_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    #[serde(flatten)]
    #[sqlx(flatten)]
    pub data: ArticleData,
}

pub async fn list(
    pool: &PgPool,
    feed_id: Option<i64>,
    before_id: Option<i64>,
    limit: i64,
) -> Result<Vec<ItemSummary>, sqlx::Error> {
    sqlx::query_as(
        "SELECT i.id, i.feed_id, v.id AS version_id, v.title, v.url, v.authors, v.published_at, i.first_seen_at \
         FROM items i JOIN item_versions v ON v.id = i.current_version_id \
         WHERE ($1::bigint IS NULL OR i.feed_id = $1) AND ($2::bigint IS NULL OR i.id < $2) \
         ORDER BY i.id DESC LIMIT $3"
    ).bind(feed_id).bind(before_id).bind(limit).fetch_all(pool).await
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<ItemDetail>, sqlx::Error> {
    sqlx::query_as(
        "SELECT i.id, i.feed_id, v.id AS version_id, i.first_seen_at, v.observed_at, \
         v.external_id, v.title, v.url, v.authors, v.summary, v.content, v.content_type, v.published_at, v.source_updated_at \
         FROM items i JOIN item_versions v ON v.id = i.current_version_id WHERE i.id = $1"
    ).bind(id).fetch_optional(pool).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_identity_missing_ids_relative_urls_and_missing_dates() {
        let document = br#"<rss version="2.0"><channel><title>Test</title>
            <item><title>Linked</title><link>/article</link></item>
            <item><title>Unlinked</title><description>Same content</description></item>
            </channel></rss>"#;
        let first = parse(document, "https://example.com/feed").unwrap();
        let second = parse(document, "https://example.com/feed").unwrap();
        for (a, b) in first.entries.iter().zip(second.entries.iter()) {
            assert_eq!(a.identity, b.identity);
            assert_eq!(a.content_hash, b.content_hash);
            assert!(a.data.published_at.is_none());
        }
        assert_eq!(
            first.entries[0].data.url.as_deref(),
            Some("https://example.com/article")
        );
        assert!(first.entries[0].identity.starts_with("url:"));
        assert!(first.entries[1].identity.starts_with("content:"));
    }

    #[test]
    fn content_is_sanitized_and_updates_keep_publisher_identity() {
        let document = r#"{"version":"https://jsonfeed.org/version/1.1","title":"Feed","items":[
            {"id":"one","title":"Title","content_html":"<p>Hello</p><script>bad()</script><img src=x onerror=bad()>"},
            {"id":"one","title":"Duplicate","content_text":"ignore"}]}"#;
        let first = parse(document.as_bytes(), "https://example.com/feed").unwrap();
        assert_eq!(first.entries.len(), 1);
        let content = first.entries[0].data.content.as_deref().unwrap();
        assert!(!content.contains("script"));
        assert!(!content.contains("onerror"));
        let updated = parse(
            document.replace("Hello", "Changed").as_bytes(),
            "https://example.com/feed",
        )
        .unwrap();
        assert_eq!(first.entries[0].identity, updated.entries[0].identity);
        assert_ne!(
            first.entries[0].content_hash,
            updated.entries[0].content_hash
        );
    }
}
