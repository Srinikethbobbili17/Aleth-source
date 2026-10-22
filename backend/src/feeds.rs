use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use url::Url;

use crate::failure::{Failure, FailureKind};

/// Largest feed body accepted after decompression.
pub const MAX_FEED_BYTES: usize = 16 * 1024 * 1024;
/// Previews arrive inside a JSON request body, which is capped at 2 MiB.
pub const MAX_PREVIEW_BYTES: usize = 1024 * 1024;
/// Entries kept from one feed document; older entries beyond this are skipped.
pub(crate) const MAX_ENTRIES: usize = 1000;
// Scaled to the byte limit: every XML event consumes at least a few bytes, and
// escaped HTML produces one event per entity such as `&lt;`.
const MAX_XML_EVENTS: usize = MAX_FEED_BYTES / 4;

pub(crate) fn validate_content(content: &[u8]) -> Result<(), Failure> {
    if content.len() > MAX_FEED_BYTES {
        return Err(Failure::new(
            FailureKind::TooLarge,
            "feed content exceeds 16 MiB",
        ));
    }
    if looks_like_html(content) {
        return Err(Failure::new(
            FailureKind::NotAFeed,
            "URL returned an HTML web page, not a feed",
        ));
    }
    if content.iter().find(|&&byte| byte == b'<' || byte == b'{') == Some(&b'<') {
        let mut reader = quick_xml::Reader::from_reader(content);
        let mut depth = 0usize;
        for _ in 0..MAX_XML_EVENTS {
            use quick_xml::events::Event;
            match reader.read_event().map_err(|_| {
                Failure::new(FailureKind::ParseFailed, "feed is not well-formed XML")
            })? {
                Event::DocType(_) => {
                    return Err(Failure::new(
                        FailureKind::UnsupportedXml,
                        "XML document types and entities are not supported",
                    ));
                }
                Event::Start(_) => {
                    depth += 1;
                    if depth > 64 {
                        return Err(Failure::new(
                            FailureKind::XmlTooDeep,
                            "XML nesting exceeds 64 levels",
                        ));
                    }
                }
                Event::End(_) => depth = depth.saturating_sub(1),
                Event::Eof => return Ok(()),
                _ => {}
            }
        }
        return Err(Failure::new(
            FailureKind::XmlTooComplex,
            "XML exceeds the event limit",
        ));
    }
    Ok(())
}

/// Detects HTML pages, skipping a leading XML declaration, processing
/// instructions, and comments, which pages often place before the doctype.
fn looks_like_html(content: &[u8]) -> bool {
    let mut rest = content.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(content);
    loop {
        rest = rest.trim_ascii_start();
        let terminator: &[u8] = if rest.starts_with(b"<?") {
            b"?>"
        } else if rest.starts_with(b"<!--") {
            b"-->"
        } else {
            break;
        };
        match rest.windows(terminator.len()).position(|w| w == terminator) {
            Some(end) => rest = &rest[end + terminator.len()..],
            None => return false,
        }
    }
    let starts_with = |prefix: &[u8]| {
        rest.get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    };
    starts_with(b"<!doctype html") || starts_with(b"<html")
}

#[derive(Debug, Serialize, FromRow)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_fetched_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    /// When automatic polling will next refresh this feed.
    pub next_poll_at: DateTime<Utc>,
    /// Manual refreshes are refused until then (short cooldown after a refresh).
    pub manual_refresh_after: Option<DateTime<Utc>>,
    /// After a failure, no refresh of any kind runs before this time.
    pub retry_not_before: Option<DateTime<Utc>>,
    pub failure_count: i32,
    pub last_error: Option<String>,
    pub last_error_code: Option<String>,
    /// When the feed was marked dead after repeated failures; null if healthy.
    pub dead_at: Option<DateTime<Utc>>,
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
         RETURNING id, url, title, created_at, last_fetched_at, last_success_at, next_poll_at, manual_refresh_after, retry_not_before, failure_count, last_error, last_error_code, dead_at",
    )
    .bind(url)
    .fetch_one(pool)
    .await
}

/// Registers many normalized URLs at once, in the given order. Returns each
/// URL's feed ID and whether this call added it. New feeds are due for
/// polling immediately.
pub async fn register_many(
    pool: &PgPool,
    urls: &[String],
) -> Result<std::collections::HashMap<String, (i64, bool)>, sqlx::Error> {
    // Statements in one WITH query share a snapshot, so the final SELECT sees
    // only feeds that existed before; the INSERT's RETURNING covers new ones.
    let rows: Vec<(String, i64, bool)> = sqlx::query_as(
        "WITH input AS (SELECT url, n FROM unnest($1::text[]) WITH ORDINALITY AS t(url, n)), \
         added AS (INSERT INTO feeds (url) SELECT url FROM input ORDER BY n \
                   ON CONFLICT (url) DO NOTHING RETURNING id, url) \
         SELECT url, id, true FROM added \
         UNION ALL SELECT f.url, f.id, false FROM feeds f JOIN input USING (url)",
    )
    .bind(urls)
    .fetch_all(pool)
    .await?;
    let mut feeds: std::collections::HashMap<String, (i64, bool)> = rows
        .into_iter()
        .map(|(url, id, added)| (url, (id, added)))
        .collect();
    // A concurrent registration can commit between our snapshot and INSERT;
    // its URLs appear in neither result, so look them up directly.
    let missing: Vec<&String> = urls
        .iter()
        .filter(|url| !feeds.contains_key(*url))
        .collect();
    if !missing.is_empty() {
        let found: Vec<(String, i64)> =
            sqlx::query_as("SELECT url, id FROM feeds WHERE url = ANY($1)")
                .bind(&missing)
                .fetch_all(pool)
                .await?;
        feeds.extend(found.into_iter().map(|(url, id)| (url, (id, false))));
    }
    Ok(feeds)
}

/// Every registered feed's URL and title, oldest first, for export.
pub async fn all_urls(pool: &PgPool) -> Result<Vec<(String, Option<String>)>, sqlx::Error> {
    sqlx::query_as("SELECT url, title FROM feeds ORDER BY id")
        .fetch_all(pool)
        .await
}

pub async fn list(pool: &PgPool, after_id: i64, limit: i64) -> Result<Vec<Feed>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, url, title, created_at, last_fetched_at, last_success_at, next_poll_at, manual_refresh_after, retry_not_before, failure_count, last_error, last_error_code, dead_at FROM feeds \
         WHERE id > $1 ORDER BY id LIMIT $2",
    )
    .bind(after_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<Feed>, sqlx::Error> {
    sqlx::query_as("SELECT id, url, title, created_at, last_fetched_at, last_success_at, next_poll_at, manual_refresh_after, retry_not_before, failure_count, last_error, last_error_code, dead_at FROM feeds WHERE id = $1")
        .bind(id).fetch_optional(pool).await
}

#[derive(Debug, Serialize)]
pub struct FeedPreview {
    pub title: Option<String>,
    pub entries: Vec<EntryPreview>,
    /// Older entries left out because the feed exceeds the entry limit.
    pub skipped: usize,
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

/// Publication date, falling back to the entry's last-updated date. Atom
/// requires `updated` but not `published`, and some feeds only provide one.
pub(crate) fn entry_date(entry: &feed_rs::model::Entry) -> Option<DateTime<Utc>> {
    entry.published.or(entry.updated)
}

/// Keeps the latest `MAX_ENTRIES` entries and returns how many were skipped.
/// Feeds normally list newest first. When every entry has a date, sort by it
/// so a feed listed oldest first still keeps its newest entries.
pub(crate) fn latest_entries(
    mut entries: Vec<feed_rs::model::Entry>,
) -> (Vec<feed_rs::model::Entry>, usize) {
    let skipped = entries.len().saturating_sub(MAX_ENTRIES);
    if skipped > 0 {
        if entries.iter().all(|entry| entry_date(entry).is_some()) {
            entries.sort_by_key(|entry| std::cmp::Reverse(entry_date(entry)));
        }
        entries.truncate(MAX_ENTRIES);
    }
    (entries, skipped)
}

pub fn preview(content: &[u8]) -> Result<FeedPreview, Failure> {
    validate_content(content)?;
    let feed = feed_rs::parser::Builder::new()
        .sanitize_content(true)
        .build()
        .parse(content)
        .map_err(|_| {
            Failure::new(
                FailureKind::ParseFailed,
                "could not parse RSS, Atom, or JSON Feed content",
            )
        })?;
    let (entries, skipped) = latest_entries(feed.entries);
    Ok(FeedPreview {
        title: feed.title.map(|t| t.content),
        skipped,
        entries: entries
            .into_iter()
            .map(|entry| EntryPreview {
                published_at: entry_date(&entry),
                parsed_id: entry.id,
                title: entry.title.map(|t| t.content),
                url: entry
                    .links
                    .iter()
                    .filter(|link| link.rel.as_deref().is_none_or(|rel| rel == "alternate"))
                    .find_map(|link| normalize_feed_url(&link.href).ok()),
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
        // The Atom fixture has only `updated`, which becomes the date; the
        // others have no date at all and stay undated.
        let atom_updated = "2026-10-21T00:00:00Z".parse::<DateTime<Utc>>().ok();
        for (content, date) in [
            (include_bytes!("../tests/fixtures/rss.xml").as_slice(), None),
            (
                include_bytes!("../tests/fixtures/atom.xml").as_slice(),
                atom_updated,
            ),
            (
                include_bytes!("../tests/fixtures/feed.json").as_slice(),
                None,
            ),
        ] {
            let parsed = preview(content).unwrap();
            assert_eq!(parsed.title.as_deref(), Some("Example feed"));
            assert_eq!(parsed.entries.len(), 1);
            assert_eq!(parsed.entries[0].title.as_deref(), Some("First article"));
            assert_eq!(
                parsed.entries[0].url.as_deref(),
                Some("https://example.com/first")
            );
            assert_eq!(parsed.entries[0].published_at, date);
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

    #[test]
    fn web_pages_and_escaped_html_get_distinct_results() {
        for page in [
            b"<!DOCTYPE html><html><body>hi</body></html>".as_slice(),
            b"\xEF\xBB\xBF  <HTML lang=en></HTML>",
        ] {
            assert_eq!(
                validate_content(page).unwrap_err().kind,
                FailureKind::NotAFeed
            );
        }
        let doctype = br#"<!DOCTYPE rss [<!ENTITY x "y">]><rss/>"#;
        assert_eq!(
            validate_content(doctype).unwrap_err().kind,
            FailureKind::UnsupportedXml
        );
        // TechCrunch-style descriptions: escaped HTML yields one event per entity.
        let escaped = format!(
            "<rss><channel><item><description>{}</description></item></channel></rss>",
            "&lt;p&gt;text&lt;/p&gt;".repeat(30_000)
        );
        assert!(validate_content(escaped.as_bytes()).is_ok());
    }

    #[test]
    fn html_detection_skips_declarations_and_comments() {
        for page in [
            b"<!-- site header --><!DOCTYPE html><html></html>".as_slice(),
            b"<?xml version=\"1.0\"?>\n<!DOCTYPE html><html></html>",
            b"<?xml version=\"1.0\"?><!-- a --> <!-- b --><html xmlns=\"x\"></html>",
        ] {
            assert_eq!(
                validate_content(page).unwrap_err().kind,
                FailureKind::NotAFeed
            );
        }
        let feed = b"<?xml version=\"1.0\"?><?xml-stylesheet href=\"s.xsl\"?><!-- c --><rss/>";
        assert!(validate_content(feed).is_ok());
        // An unterminated comment is not mistaken for a page.
        assert!(!looks_like_html(b"<!-- never closed <html>"));
    }

    fn json_feed(count: usize, dated: bool, oldest_first: bool) -> String {
        let items: Vec<String> = (0..count)
            .map(|i| {
                let day = if oldest_first { i } else { count - 1 - i };
                let date = chrono::DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::days(day as i64);
                if dated {
                    format!(
                        r#"{{"id":"{day}","title":"t{day}","date_published":"{}"}}"#,
                        date.to_rfc3339()
                    )
                } else {
                    format!(r#"{{"id":"{day}","title":"t{day}"}}"#)
                }
            })
            .collect();
        format!(
            r#"{{"version":"https://jsonfeed.org/version/1.1","title":"Big","items":[{}]}}"#,
            items.join(",")
        )
    }

    #[test]
    fn big_feeds_keep_the_latest_thousand_entries() {
        // Newest first, oldest first, and undated: all keep 1,000 and skip 500.
        for (dated, oldest_first, first_kept) in [
            (true, false, "1499"),
            (true, true, "1499"),
            (false, false, "1499"),
        ] {
            let preview = preview(json_feed(1500, dated, oldest_first).as_bytes()).unwrap();
            assert_eq!(preview.entries.len(), MAX_ENTRIES);
            assert_eq!(preview.skipped, 500);
            assert_eq!(preview.entries[0].parsed_id, first_kept);
            if dated {
                assert_eq!(preview.entries[MAX_ENTRIES - 1].parsed_id, "500");
            }
        }
        // Undated oldest-first feeds can only trust document order.
        let undated = preview(json_feed(1500, false, true).as_bytes()).unwrap();
        assert_eq!(undated.entries[0].parsed_id, "0");
        let small = preview(json_feed(10, true, false).as_bytes()).unwrap();
        assert_eq!((small.entries.len(), small.skipped), (10, 0));
    }
}
