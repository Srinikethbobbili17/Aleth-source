use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use url::Url;

pub const MAX_FEED_BYTES: usize = 1024 * 1024;
const MAX_PREVIEW_ENTRIES: usize = 1000;

pub(crate) fn validate_content(content: &[u8]) -> Result<(), &'static str> {
    if content.len() > MAX_FEED_BYTES {
        return Err("feed content exceeds 1 MiB");
    }
    if content.iter().find(|&&byte| byte == b'<' || byte == b'{') == Some(&b'<') {
        let mut reader = quick_xml::Reader::from_reader(content);
        let mut depth = 0usize;
        for _ in 0..50_000 {
            use quick_xml::events::Event;
            match reader.read_event().map_err(|_| "invalid XML structure")? {
                Event::DocType(_) => {
                    return Err("XML document types and entities are not supported");
                }
                Event::Start(_) => {
                    depth += 1;
                    if depth > 64 {
                        return Err("XML nesting exceeds 64 levels");
                    }
                }
                Event::End(_) => depth = depth.saturating_sub(1),
                Event::Eof => return Ok(()),
                _ => {}
            }
        }
        return Err("XML exceeds event limit");
    }
    Ok(())
}

#[derive(Debug, Serialize, FromRow)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_fetched_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub next_fetch_at: DateTime<Utc>,
    pub failure_count: i32,
    pub last_error: Option<String>,
}

/// Registration validates syntax only. A future fetcher must separately enforce
/// destination policy at DNS resolution, connection, and every redirect.
pub fn normalize_feed_url(input: &str) -> Result<String, &'static str> {
    if input.len() > 4096 {
        return Err("feed URL exceeds 4096 bytes");
    }
    let mut url = Url::parse(input).map_err(|_| "invalid absolute feed URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("feed URL must use HTTP or HTTPS and include a host");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("credentials in feed URLs are not supported");
    }
    url.set_fragment(None);
    let normalized = url.to_string();
    if normalized.len() > 4096 {
        return Err("normalized feed URL exceeds 4096 bytes");
    }
    Ok(normalized)
}

pub async fn register(pool: &PgPool, url: &str) -> Result<Feed, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO feeds (url) VALUES ($1) \
         ON CONFLICT (url) DO UPDATE SET url = EXCLUDED.url \
         RETURNING id, url, title, created_at, last_fetched_at, last_success_at, next_fetch_at, failure_count, last_error",
    )
    .bind(url)
    .fetch_one(pool)
    .await
}

pub async fn list(pool: &PgPool, after_id: i64, limit: i64) -> Result<Vec<Feed>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, url, title, created_at, last_fetched_at, last_success_at, next_fetch_at, failure_count, last_error FROM feeds \
         WHERE id > $1 ORDER BY id LIMIT $2",
    )
    .bind(after_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<Feed>, sqlx::Error> {
    sqlx::query_as("SELECT id, url, title, created_at, last_fetched_at, last_success_at, next_fetch_at, failure_count, last_error FROM feeds WHERE id = $1")
        .bind(id).fetch_optional(pool).await
}

#[derive(Debug, Serialize)]
pub struct FeedPreview {
    pub title: Option<String>,
    pub entries: Vec<EntryPreview>,
}

#[derive(Debug, Serialize)]
pub struct EntryPreview {
    /// feed-rs can synthesize IDs. Persistence must establish a stable ID policy
    /// before these previews are used for ingestion.
    pub parsed_id: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
}

pub fn preview(content: &[u8]) -> Result<FeedPreview, &'static str> {
    validate_content(content)?;
    let feed = feed_rs::parser::Builder::new()
        .sanitize_content(true)
        .build()
        .parse(content)
        .map_err(|_| "could not parse RSS, Atom, or JSON Feed content")?;
    if feed.entries.len() > MAX_PREVIEW_ENTRIES {
        return Err("preview exceeds 1000 entries");
    }
    Ok(FeedPreview {
        title: feed.title.map(|t| t.content),
        entries: feed
            .entries
            .into_iter()
            .map(|entry| EntryPreview {
                parsed_id: entry.id,
                title: entry.title.map(|t| t.content),
                url: entry
                    .links
                    .iter()
                    .filter(|link| link.rel.as_deref().is_none_or(|rel| rel == "alternate"))
                    .find_map(|link| normalize_feed_url(&link.href).ok()),
                published_at: entry.published,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls_without_removing_meaningful_queries() {
        assert_eq!(
            normalize_feed_url("HTTPS://example.com:443/feed?a=1#top").unwrap(),
            "https://example.com/feed?a=1"
        );
        for invalid in [
            "file:///etc/passwd",
            "/feed",
            "https://user:secret@example.com/feed",
        ] {
            assert!(normalize_feed_url(invalid).is_err());
        }
    }

    #[test]
    fn parses_rss_atom_and_json_without_inventing_publication_dates() {
        for content in [
            include_bytes!("../tests/fixtures/rss.xml").as_slice(),
            include_bytes!("../tests/fixtures/atom.xml").as_slice(),
            include_bytes!("../tests/fixtures/feed.json").as_slice(),
        ] {
            let parsed = preview(content).unwrap();
            assert_eq!(parsed.title.as_deref(), Some("Example feed"));
            assert_eq!(parsed.entries.len(), 1);
            assert_eq!(parsed.entries[0].title.as_deref(), Some("First article"));
            assert_eq!(
                parsed.entries[0].url.as_deref(),
                Some("https://example.com/first")
            );
            assert!(parsed.entries[0].published_at.is_none());
        }
    }

    #[test]
    fn rejects_invalid_and_oversized_content() {
        assert!(preview(b"this is not a feed").is_err());
        assert!(preview(&vec![b'x'; MAX_FEED_BYTES + 1]).is_err());
        let nested = format!("{}{}", "<a>".repeat(65), "</a>".repeat(65));
        assert!(validate_content(nested.as_bytes()).is_err());
        assert!(
            validate_content(br#"<!DOCTYPE rss [<!ENTITY x SYSTEM "file:///etc/passwd">]><rss/>"#)
                .is_err()
        );
    }
}
