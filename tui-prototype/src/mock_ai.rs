//! The briefing: new articles grouped into cross-feed stories, then by folder.
//! Stories are found by shared links, shared title words, or (with embeddings)
//! closeness in meaning. What the briefing says about each article is still a
//! stand-in: its lead sentence, unless an AI summary has been written for it.

use chrono::{DateTime, Utc};

use crate::data::Dataset;
use crate::semantic::Semantic;

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
    /// Items from two or more feeds that look like the same story. `by_meaning`
    /// when at least one joined only because the embeddings say so.
    Story {
        label: String,
        items: Vec<usize>,
        by_meaning: bool,
    },
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

/// True when the two sorted word lists share at least one word.
fn shares_word(a: &[String], b: &[String]) -> bool {
    a.iter().any(|w| b.binary_search(w).is_ok())
}

/// Briefing of items first seen after `since`: cross-feed stories first, then
/// the remainder grouped by folder. Indices refer to `ds.items`.
pub fn briefing(ds: &Dataset, since: DateTime<Utc>, semantic: Option<&Semantic>) -> Vec<Entry> {
    // Oldest first, so each story is seeded by its earliest report.
    let fresh: Vec<usize> = (0..ds.items.len())
        .rev()
        .filter(|&i| ds.items[i].first_seen_at > since)
        .collect();

    // An item joins a story when it links to the same page as one of its
    // reports, its title is close to the seed's, or its text is close in
    // meaning to the seed's. Comparing with the seed only (not every member)
    // stops loosely related articles chaining together.
    struct Cluster {
        seed: usize,
        seed_words: Vec<String>,
        urls: Vec<String>,
        feeds: Vec<i64>,
        items: Vec<usize>,
        by_meaning: bool,
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
            .filter_map(|(k, c)| {
                if !url.is_empty() && c.urls.contains(&url) {
                    return Some((k, 1.0, false));
                }
                let by_words = similarity(&words, &c.seed_words);
                if by_words >= 0.5 {
                    return Some((k, by_words, false));
                }
                let sem = semantic?;
                let t = sem.thresholds;
                let meaning = sem.similarity(i, c.seed)?;
                (meaning >= t.story
                    || (meaning >= t.story_with_shared_word && shares_word(&words, &c.seed_words)))
                .then_some((k, f64::from(meaning), true))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((k, _, by_meaning)) => {
                let c = &mut clusters[k];
                c.urls.push(url);
                c.feeds.push(item.feed_id);
                c.items.push(i);
                c.by_meaning |= by_meaning;
            }
            None => clusters.push(Cluster {
                seed: i,
                seed_words: words,
                urls: vec![url],
                feeds: vec![item.feed_id],
                items: vec![i],
                by_meaning: false,
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
                by_meaning: cluster.by_meaning,
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use chrono::Duration;

    use super::*;

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/fake.json");

    fn stories(entries: &[Entry], ds: &Dataset) -> Vec<(Vec<i64>, bool)> {
        entries
            .iter()
            .filter_map(|e| match e {
                Entry::Story {
                    items, by_meaning, ..
                } => Some((items.iter().map(|&i| ds.items[i].id).collect(), *by_meaning)),
                Entry::Folder { .. } => None,
            })
            .collect()
    }

    fn index(ds: &Dataset, id: i64) -> usize {
        ds.items.iter().position(|i| i.id == id).unwrap()
    }

    #[test]
    fn without_embeddings_stories_come_from_links_and_titles() {
        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        let entries = briefing(&ds, ds.now - Duration::days(30), None);
        let found = stories(&entries, &ds);
        assert_eq!(found.len(), 4, "the fixture has four cross-feed stories");
        assert!(found.iter().all(|(_, by_meaning)| !by_meaning));
        let total: usize = entries.iter().map(|e| e.items().len()).sum();
        assert_eq!(total, ds.items.len(), "every new item appears once");
    }

    #[test]
    fn close_meaning_joins_articles_whose_titles_differ() {
        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        // "Telescope spots water vapour…" (feed 4) and "Smartwatch maker recalls…"
        // (feed 2) share no title words; give them the same vector.
        let (a, b) = (index(&ds, 105), index(&ds, 134));
        let vectors = (0..ds.items.len())
            .map(|i| {
                let mut v = vec![0.0; ds.items.len()];
                v[if i == b { a } else { i }] = 1.0;
                Some(v)
            })
            .collect();
        let sem = Semantic::new("bge-m3", "", vectors, crate::semantic::UNREACHABLE).unwrap();
        let entries = briefing(&ds, ds.now - Duration::days(30), Some(&sem));
        let found = stories(&entries, &ds);
        assert!(found.contains(&(vec![105, 134], true)), "{found:?}");
        assert_eq!(found.len(), 5, "the four title stories stay as they were");
    }

    /// The real trial snapshot with bge-m3 vectors; skips when either is missing.
    #[test]
    fn trial_stories_by_meaning() {
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/data/trial.json"));
        if !fixture.exists() {
            eprintln!("SKIP: no data/trial.json");
            return;
        }
        let ds = Dataset::load(fixture).unwrap();
        let Some(sem) = Semantic::load(&ds, fixture)
            .unwrap()
            .filter(|s| s.model == "bge-m3")
        else {
            eprintln!("SKIP: no bge-m3 embeddings for the trial");
            return;
        };
        let entries = briefing(&ds, ds.now - Duration::days(365), Some(&sem));
        let story_of = |needle: &str| {
            entries.iter().position(|e| {
                matches!(e, Entry::Story { .. })
                    && e.items()
                        .iter()
                        .any(|&i| ds.items[i].title().contains(needle))
            })
        };
        // Same story, headlines with no words in common.
        let dogs = story_of("Warhound raises");
        assert!(dogs.is_some() && dogs == story_of("Genetically Modified"));
        // bge-m3 scores these 0.76 apart; they must not become one story.
        let rust = story_of("Announcing Rust 1.99.0");
        assert!(rust.is_none() || rust != story_of("Bird Feeder"));
        let total: usize = entries.iter().map(|e| e.items().len()).sum();
        assert_eq!(total, ds.items.len());
    }
}
