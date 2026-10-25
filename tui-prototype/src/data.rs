//! Fixture loading. The JSON mirrors the backend's `/v1/feeds` and `/v1/items/{id}`
//! responses, plus a `folder` per feed and optional read/starred state.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

pub const UNFILED: &str = "Unfiled";

#[derive(Deserialize)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub title: Option<String>,
    pub folder: Option<String>,
    pub last_fetched_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub next_poll_at: Option<DateTime<Utc>>,
    pub retry_not_before: Option<DateTime<Utc>>,
    pub failure_count: i32,
    pub last_error: Option<String>,
    pub last_error_code: Option<String>,
    pub dead_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Failing,
    Dead,
}

impl Feed {
    pub fn name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.url)
    }

    pub fn health(&self) -> Health {
        if self.dead_at.is_some() {
            Health::Dead
        } else if self.failure_count > 0 {
            Health::Failing
        } else {
            Health::Ok
        }
    }
}

#[derive(Deserialize)]
pub struct Item {
    pub id: i64,
    pub feed_id: i64,
    pub version_id: i64,
    pub first_seen_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
    pub content_type: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
}

impl Item {
    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or("(untitled)")
    }

    /// The time shown to the reader: publication if known, otherwise first seen.
    pub fn when(&self) -> DateTime<Utc> {
        self.published_at.unwrap_or(self.first_seen_at)
    }

    pub fn body_html(&self) -> Option<&str> {
        self.content.as_deref().or(self.summary.as_deref())
    }
}

#[derive(Deserialize, Default)]
struct State {
    #[serde(default)]
    read: Vec<i64>,
    #[serde(default)]
    starred: Vec<i64>,
}

#[derive(Deserialize)]
struct Fixture {
    source: Option<String>,
    feeds: Vec<Feed>,
    items: Vec<Item>,
    state: Option<State>,
}

pub struct Dataset {
    pub source: String,
    pub feeds: Vec<Feed>,
    /// Newest first.
    pub items: Vec<Item>,
    pub read: HashSet<i64>,
    pub starred: HashSet<i64>,
    /// "Now" for relative times: the newest observation in the fixture, so old
    /// snapshots still read as if they were live.
    pub now: DateTime<Utc>,
    feed_index: HashMap<i64, usize>,
}

impl Dataset {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut fixture: Fixture =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        // Feeds outside any folder get one of their own, so every feed is reachable from Home.
        for feed in &mut fixture.feeds {
            feed.folder.get_or_insert_with(|| UNFILED.to_string());
        }
        fixture
            .items
            .sort_by(|a, b| b.when().cmp(&a.when()).then(b.id.cmp(&a.id)));

        let now = fixture
            .items
            .iter()
            .map(|i| i.observed_at)
            .chain(fixture.feeds.iter().filter_map(|f| f.last_fetched_at))
            .max()
            .unwrap_or_else(Utc::now);

        // Without saved state, pretend everything older than a day has been read.
        let (read, starred) = match fixture.state {
            Some(state) => (
                state.read.into_iter().collect(),
                state.starred.into_iter().collect(),
            ),
            None => (
                fixture
                    .items
                    .iter()
                    .filter(|i| i.when() < now - Duration::hours(24))
                    .map(|i| i.id)
                    .collect(),
                HashSet::new(),
            ),
        };

        let feed_index = fixture
            .feeds
            .iter()
            .enumerate()
            .map(|(i, f)| (f.id, i))
            .collect();
        Ok(Self {
            source: fixture.source.unwrap_or_else(|| "fixture".into()),
            feeds: fixture.feeds,
            items: fixture.items,
            read,
            starred,
            now,
            feed_index,
        })
    }

    pub fn feed(&self, feed_id: i64) -> Option<&Feed> {
        self.feed_index.get(&feed_id).map(|&i| &self.feeds[i])
    }

    pub fn feed_name(&self, feed_id: i64) -> &str {
        self.feed(feed_id).map_or("?", Feed::name)
    }

    pub fn folder_of(&self, item: &Item) -> Option<&str> {
        self.feed(item.feed_id).and_then(|f| f.folder.as_deref())
    }

    /// Folder names in first-seen order.
    pub fn folders(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for feed in &self.feeds {
            if let Some(folder) = &feed.folder
                && !out.contains(folder)
            {
                out.push(folder.clone());
            }
        }
        out
    }
}

/// Plain text without markdown-style decoration or link footnotes.
pub fn html_to_text(html: &str, width: usize) -> String {
    html2text::config::plain_no_decorate()
        .string_from_read(html.as_bytes(), width)
        .unwrap_or_else(|_| html.to_string())
}

/// Short relative duration, e.g. "5m", "3h", "2d".
pub fn short_duration(d: Duration) -> String {
    let mins = d.num_minutes().abs();
    match mins {
        0 => "now".into(),
        1..60 => format!("{mins}m"),
        60..1440 => format!("{}h", mins / 60),
        1440..43200 => format!("{}d", mins / 1440),
        _ => format!("{}mo", mins / 43200),
    }
}
