//! OPML subscription lists: the file format feed readers use to import and
//! export subscriptions.

use quick_xml::{XmlVersion, escape::escape, events::Event};
use serde::Serialize;

use crate::{
    failure::{Failure, FailureKind},
    feeds::normalize_feed_url,
};

/// Feeds accepted from one file. The 2 MiB request limit already bounds
/// parsing work; this keeps a single import to a reviewable size.
pub const MAX_FEEDS: usize = 10_000;
const MAX_DEPTH: usize = 64;

/// The subscriptions found in an OPML file, deduplicated by normalized URL.
#[derive(Debug, Default)]
pub struct Subscriptions {
    pub feeds: Vec<Subscription>,
    pub rejected: Vec<RejectedOutline>,
    /// Outlines whose URL repeats one seen earlier in the file.
    pub duplicates: usize,
}

#[derive(Debug, Serialize)]
pub struct Subscription {
    pub url: String,
    /// Title from the file. The feed's own title replaces it after the first fetch.
    pub title: Option<String>,
    /// Enclosing folder names, outermost first; empty at the top level.
    pub folders: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RejectedOutline {
    pub url: String,
    pub code: &'static str,
    pub error: String,
}

/// Parses an OPML document. Outlines with an `xmlUrl` are feeds; outlines
/// without one are folders. Invalid feed URLs are reported, not fatal.
pub fn parse(content: &[u8]) -> Result<Subscriptions, Failure> {
    let invalid = |message: &'static str| Failure::new(FailureKind::InvalidOpml, message);
    let mut reader = quick_xml::Reader::from_reader(content);
    let mut found = Subscriptions::default();
    let mut seen = std::collections::HashSet::new();
    let mut root_seen = false;
    let mut depth = 0usize;
    // One entry per open <outline>: Some(name) for a folder, None for a feed.
    let mut open: Vec<Option<String>> = Vec::new();
    loop {
        let event = reader
            .read_event()
            .map_err(|_| invalid("OPML is not well-formed XML"))?;
        let (element, has_children) = match &event {
            Event::Start(element) => (element, true),
            Event::Empty(element) => (element, false),
            Event::End(element) => {
                depth = depth.saturating_sub(1);
                if element.local_name().as_ref() == b"outline" {
                    open.pop();
                }
                continue;
            }
            Event::DocType(_) => {
                return Err(Failure::new(
                    FailureKind::UnsupportedXml,
                    "XML document types and entities are not supported",
                ));
            }
            Event::Eof => break,
            _ => continue,
        };
        if has_children {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(Failure::new(
                    FailureKind::XmlTooDeep,
                    "XML nesting exceeds 64 levels",
                ));
            }
        }
        if !root_seen {
            if element.local_name().as_ref() != b"opml" {
                return Err(invalid(
                    "not an OPML document: the root element must be <opml>",
                ));
            }
            root_seen = true;
            continue;
        }
        if element.local_name().as_ref() != b"outline" {
            continue;
        }
        let mut xml_url = None;
        let mut title = None;
        let mut text = None;
        for attribute in element.attributes().with_checks(false) {
            let attribute = attribute.map_err(|_| invalid("OPML has a malformed attribute"))?;
            let value = || {
                attribute
                    .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                    .map(|value| value.trim().to_owned())
                    .map_err(|_| invalid("OPML has a malformed attribute value"))
            };
            // Some exporters write `xmlurl` or `XMLURL`; accept any case.
            let key = attribute.key.local_name();
            if key.as_ref().eq_ignore_ascii_case(b"xmlUrl") {
                xml_url = Some(value()?);
            } else if key.as_ref() == b"title" {
                title = Some(value()?);
            } else if key.as_ref() == b"text" {
                text = Some(value()?);
            }
        }
        let name = title.or(text).filter(|name| !name.is_empty());
        match xml_url.filter(|url| !url.is_empty()) {
            None => {
                if has_children {
                    open.push(Some(name.unwrap_or_default()));
                }
            }
            Some(url) => {
                if has_children {
                    open.push(None);
                }
                match normalize_feed_url(&url) {
                    Err(error) => found.rejected.push(RejectedOutline {
                        url,
                        code: FailureKind::InvalidUrl.code(),
                        error: error.into(),
                    }),
                    Ok(url) if !seen.insert(url.clone()) => found.duplicates += 1,
                    Ok(url) => {
                        if found.feeds.len() == MAX_FEEDS {
                            return Err(Failure::new(
                                FailureKind::TooManyFeeds,
                                "OPML file lists more than 10000 feeds",
                            ));
                        }
                        found.feeds.push(Subscription {
                            url,
                            title: name,
                            folders: open
                                .iter()
                                .flatten()
                                .filter(|folder| !folder.is_empty())
                                .cloned()
                                .collect(),
                        });
                    }
                }
            }
        }
    }
    if !root_seen {
        return Err(invalid(
            "not an OPML document: the root element must be <opml>",
        ));
    }
    Ok(found)
}

/// Writes feeds as an OPML 2.0 document. Folders are not stored yet, so the
/// list is flat. A feed without a title yet is labelled with its URL.
pub fn write<'a>(feeds: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<opml version=\"2.0\">\n  \
         <head><title>RSS backend subscriptions</title></head>\n  <body>\n",
    );
    for (url, title) in feeds {
        let label = escape(
            title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or(url),
        );
        out.push_str(&format!(
            "    <outline type=\"rss\" text=\"{label}\" title=\"{label}\" xmlUrl=\"{}\"/>\n",
            escape(url)
        ));
    }
    out.push_str("  </body>\n</opml>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>My subscriptions</title></head>
  <body>
    <outline text="Rust Blog" type="rss" xmlUrl="https://blog.rust-lang.org/feed.xml"/>
    <outline text="Tech" title="Tech">
      <outline text="Hacker News" xmlUrl="https://hnrss.org/frontpage?points=100&amp;count=20"/>
      <outline title="News">
        <outline text="BBC" xmlurl="https://feeds.bbci.co.uk/news/rss.xml"></outline>
      </outline>
      <outline text="Again" xmlUrl="HTTPS://blog.rust-lang.org:443/feed.xml#top"/>
    </outline>
    <outline text="Broken" xmlUrl="ftp://example.com/feed"/>
    <outline text="No URL at all"/>
  </body>
</opml>"#;

    #[test]
    fn reads_feeds_folders_and_reports_problems() {
        let found = parse(SAMPLE.as_bytes()).unwrap();
        let feeds: Vec<(&str, Option<&str>, Vec<&str>)> = found
            .feeds
            .iter()
            .map(|feed| {
                (
                    feed.url.as_str(),
                    feed.title.as_deref(),
                    feed.folders.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            feeds,
            [
                (
                    "https://blog.rust-lang.org/feed.xml",
                    Some("Rust Blog"),
                    vec![]
                ),
                (
                    "https://hnrss.org/frontpage?points=100&count=20",
                    Some("Hacker News"),
                    vec!["Tech"]
                ),
                (
                    "https://feeds.bbci.co.uk/news/rss.xml",
                    Some("BBC"),
                    vec!["Tech", "News"]
                ),
            ]
        );
        // The second Rust Blog entry normalizes to the same URL.
        assert_eq!(found.duplicates, 1);
        assert_eq!(found.rejected.len(), 1);
        assert_eq!(found.rejected[0].code, "invalid_url");
    }

    #[test]
    fn rejects_files_that_are_not_safe_opml() {
        for (content, kind) in [
            ("", FailureKind::InvalidOpml),
            ("not xml at all", FailureKind::InvalidOpml),
            ("<rss><channel/></rss>", FailureKind::InvalidOpml),
            (
                "<opml><body><outline></body></opml>",
                FailureKind::InvalidOpml,
            ),
            (
                r#"<!DOCTYPE opml [<!ENTITY x SYSTEM "file:///etc/passwd">]><opml/>"#,
                FailureKind::UnsupportedXml,
            ),
        ] {
            assert_eq!(
                parse(content.as_bytes()).unwrap_err().kind,
                kind,
                "{content:?}"
            );
        }
        let deep = format!(
            "<opml><body>{}{}</body></opml>",
            "<outline text=\"f\">".repeat(70),
            "</outline>".repeat(70)
        );
        assert_eq!(
            parse(deep.as_bytes()).unwrap_err().kind,
            FailureKind::XmlTooDeep
        );
    }

    #[test]
    fn written_files_parse_back_to_the_same_feeds() {
        let feeds = [
            (
                "https://example.com/a?x=1&y=2",
                Some("Tom & Jerry's <feed>"),
            ),
            ("https://example.com/b", None),
            ("https://example.com/c", Some("  ")),
        ];
        let written = write(feeds);
        assert!(written.contains("x=1&amp;y=2"));
        let parsed = parse(written.as_bytes()).unwrap();
        let round_trip: Vec<(&str, Option<&str>)> = parsed
            .feeds
            .iter()
            .map(|feed| (feed.url.as_str(), feed.title.as_deref()))
            .collect();
        assert_eq!(
            round_trip,
            [
                (
                    "https://example.com/a?x=1&y=2",
                    Some("Tom & Jerry's <feed>")
                ),
                ("https://example.com/b", Some("https://example.com/b")),
                ("https://example.com/c", Some("https://example.com/c")),
            ]
        );
        assert!(write([]).contains("<body>"));
    }

    #[test]
    fn caps_the_number_of_feeds() {
        let outlines: String = (0..=MAX_FEEDS)
            .map(|n| format!(r#"<outline xmlUrl="https://example.com/{n}"/>"#))
            .collect();
        let content = format!("<opml><body>{outlines}</body></opml>");
        assert_eq!(
            parse(content.as_bytes()).unwrap_err().kind,
            FailureKind::TooManyFeeds
        );
    }
}
