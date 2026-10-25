//! Stand-ins for AI features. Nothing here calls a model: "summaries" are lead
//! sentences pulled from the article, and "stories" are clusters of titles that
//! share words. The point is to try out how AI output is shown, cited, and
//! labelled in the TUI before deciding what real integration to build.

use chrono::{DateTime, Utc};

use crate::data::Dataset;

#[derive(Clone, Copy)]
pub enum Input {
    FullText,
    Excerpt,
    TitleOnly,
}

impl Input {
    pub fn label(self) -> &'static str {
        match self {
            Input::FullText => "full text",
            Input::Excerpt => "feed excerpt only",
            Input::TitleOnly => "title only",
        }
    }
}

pub struct Claim {
    /// 1-based paragraph number in the rendered article.
    pub paragraph: usize,
    pub text: String,
}

pub struct Summary {
    pub input: Input,
    pub words: usize,
    pub claims: Vec<Claim>,
}

/// Builds a fake summary from the article's paragraphs, as rendered for display,
/// so the paragraph numbers it cites match what the reader sees.
pub fn summarize(has_content: bool, paragraphs: &[String]) -> Summary {
    let words = paragraphs
        .iter()
        .map(|p| p.split_whitespace().count())
        .sum();
    let input = if paragraphs.is_empty() {
        Input::TitleOnly
    } else if has_content && words >= 120 {
        Input::FullText
    } else {
        Input::Excerpt
    };
    let claims = paragraphs
        .iter()
        .enumerate()
        .filter(|(_, p)| p.split_whitespace().count() >= 12)
        .take(3)
        .map(|(i, p)| Claim {
            paragraph: i + 1,
            text: lead_sentence(p, 180),
        })
        .collect();
    Summary {
        input,
        words,
        claims,
    }
}

/// The first sentence of a paragraph, joined onto one line and capped in length.
pub fn lead_sentence(paragraph: &str, max_chars: usize) -> String {
    let flat = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = flat
        .match_indices(". ")
        .map(|(i, _)| i + 1)
        .find(|&i| i >= 40)
        .unwrap_or(flat.len());
    let sentence = &flat[..end];
    if sentence.chars().count() > max_chars {
        let cut: String = sentence.chars().take(max_chars - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        sentence.to_string()
    }
}

pub enum Entry {
    /// Items from two or more feeds that look like the same story.
    Story { label: String, items: Vec<usize> },
    /// Everything else that is new in a folder.
    Folder { name: String, items: Vec<usize> },
}

impl Entry {
    pub fn items(&self) -> &[usize] {
        match self {
            Entry::Story { items, .. } | Entry::Folder { items, .. } => items,
        }
    }
}

const STOPWORDS: &[&str] = &[
    "about", "after", "again", "against", "also", "among", "been", "before", "being", "between",
    "could", "does", "from", "have", "here", "into", "just", "more", "most", "much", "only",
    "other", "over", "says", "should", "show", "some", "such", "than", "that", "their", "them",
    "then", "there", "these", "they", "this", "those", "through", "under", "until", "using",
    "very", "what", "when", "where", "which", "while", "will", "with", "would", "your", "year",
    "years", "news", "week", "today", "new",
];

/// Significant title words: four letters or more, or an acronym such as "VPN".
/// A plural "s" is dropped so "forecast" and "forecasts" match.
fn title_words(title: &str) -> Vec<String> {
    let mut words: Vec<String> = title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| {
            let acronym = w.len() >= 2
                && w.chars().any(|c| c.is_alphabetic())
                && w.chars().all(|c| c.is_uppercase() || c.is_ascii_digit());
            acronym || w.chars().count() >= 4
        })
        .map(str::to_lowercase)
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .map(|w| match w.strip_suffix('s') {
            Some(stem) if stem.len() >= 4 && !stem.ends_with('s') => stem.to_string(),
            _ => w,
        })
        .collect();
    words.sort();
    words.dedup();
    words
}

/// Overlap coefficient between two sorted word lists, requiring two shared words.
fn similarity(a: &[String], b: &[String]) -> f64 {
    let shared = a.iter().filter(|w| b.contains(w)).count();
    if shared < 2 {
        0.0
    } else {
        shared as f64 / a.len().min(b.len()) as f64
    }
}

/// Briefing of items first seen after `since`: cross-feed stories first, then
/// the remainder grouped by folder. Indices refer to `ds.items`.
pub fn briefing(ds: &Dataset, since: DateTime<Utc>) -> Vec<Entry> {
    // Oldest first, so each story is seeded by its earliest report.
    let fresh: Vec<usize> = (0..ds.items.len())
        .rev()
        .filter(|&i| ds.items[i].first_seen_at > since)
        .collect();

    // An item joins a story when it links to the same page as one of its
    // reports, or its title is close to the seed's. Comparing with the seed
    // only (not every member) stops loosely related titles chaining together.
    struct Cluster {
        seed_words: Vec<String>,
        urls: Vec<String>,
        feeds: Vec<i64>,
        items: Vec<usize>,
    }
    let mut clusters: Vec<Cluster> = Vec::new();
    for &i in &fresh {
        let item = &ds.items[i];
        let words = title_words(item.title());
        let url = item.url.clone().unwrap_or_default();
        let best = clusters
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.feeds.contains(&item.feed_id))
            .map(|(k, c)| {
                let same_url = !url.is_empty() && c.urls.contains(&url);
                (
                    k,
                    if same_url {
                        1.0
                    } else {
                        similarity(&words, &c.seed_words)
                    },
                )
            })
            .filter(|&(_, score)| score >= 0.5)
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((k, _)) => {
                let c = &mut clusters[k];
                c.urls.push(url);
                c.feeds.push(item.feed_id);
                c.items.push(i);
            }
            None => clusters.push(Cluster {
                seed_words: words,
                urls: vec![url],
                feeds: vec![item.feed_id],
                items: vec![i],
            }),
        }
    }

    let mut stories = Vec::new();
    let mut by_folder: Vec<(String, Vec<usize>)> =
        ds.folders().into_iter().map(|f| (f, Vec::new())).collect();
    let mut loose = Vec::new();
    for cluster in clusters {
        let mut members = cluster.items;
        if members.len() >= 2 {
            // Label with the earliest report's title, list newest first.
            let label = ds.items[members[0]].title().to_string();
            members.sort();
            stories.push(Entry::Story {
                label,
                items: members,
            });
            continue;
        }
        let i = members[0];
        match ds.folder_of(&ds.items[i]) {
            Some(f) => by_folder
                .iter_mut()
                .find(|(n, _)| n == f)
                .unwrap()
                .1
                .push(i),
            None => loose.push(i),
        }
    }
    stories.sort_by_key(|e| std::cmp::Reverse(e.items().len()));

    let mut out = stories;
    for (name, mut items) in by_folder
        .into_iter()
        .chain([("Unfiled".to_string(), loose)])
    {
        if !items.is_empty() {
            items.sort();
            out.push(Entry::Folder { name, items });
        }
    }
    out
}
