//! Topic search. Matches whole words in an article's title, text, authors, feed
//! and folder, and widens known topics to related terms, so "ai" also finds
//! articles about LLMs or OpenAI that never say "AI". The topic table is a
//! hand-written stand-in; `semantic.rs` adds matches by meaning on top.

use crate::data::{Dataset, html_to_text};

/// Topic names (any of which triggers the topic) and the related terms they add.
const TOPICS: &[(&[&str], &[&str])] = &[
    (
        &["ai", "artificial intelligence"],
        &[
            "machine learning",
            "ml",
            "llm",
            "llms",
            "large language model",
            "language model",
            "language models",
            "neural network",
            "neural networks",
            "deep learning",
            "generative",
            "genai",
            "gpt",
            "chatgpt",
            "openai",
            "anthropic",
            "claude",
            "gemini",
            "deepmind",
            "copilot",
            "mistral",
            "llama",
            "hugging face",
            "transformer",
            "transformers",
            "diffusion",
            "agentic",
            "ai agent",
            "ai agents",
            "chatbot",
            "chatbots",
            "fine tuning",
            "reinforcement learning",
            "multimodal",
            "foundation model",
            "foundation models",
            "reasoning model",
            "embeddings",
            "codex",
        ],
    ),
    (
        &["tech", "technology"],
        &[
            "ai",
            "software",
            "hardware",
            "startup",
            "startups",
            "silicon valley",
            "apple",
            "google",
            "microsoft",
            "amazon",
            "meta",
            "nvidia",
            "samsung",
            "semiconductor",
            "semiconductors",
            "chip",
            "chips",
            "chipmaker",
            "smartphone",
            "iphone",
            "android",
            "laptop",
            "gadget",
            "gadgets",
            "cloud",
            "data center",
            "data centers",
            "developer",
            "developers",
            "open source",
            "cybersecurity",
            "app",
            "apps",
            "internet",
            "computing",
            "robot",
            "robots",
        ],
    ),
    (
        &["security", "cybersecurity", "infosec"],
        &[
            "vulnerability",
            "vulnerabilities",
            "cve",
            "exploit",
            "exploits",
            "malware",
            "ransomware",
            "breach",
            "hack",
            "hacked",
            "hackers",
            "phishing",
            "patch",
            "zero day",
            "credentials",
            "supply chain attack",
            "backdoor",
            "jailbreak",
            "jailbreaking",
        ],
    ),
    (
        &["space", "astronomy"],
        &[
            "nasa",
            "esa",
            "spacex",
            "rocket",
            "orbit",
            "satellite",
            "satellites",
            "telescope",
            "exoplanet",
            "mars",
            "moon",
            "lunar",
            "asteroid",
            "galaxy",
            "astronaut",
            "astronauts",
            "cassini",
            "apod",
        ],
    ),
    (
        &["climate", "climate change"],
        &[
            "emissions",
            "carbon",
            "global warming",
            "warming",
            "fossil fuels",
            "renewable",
            "renewables",
            "solar power",
            "wind power",
            "methane",
            "heatwave",
        ],
    ),
    (
        &["programming", "coding", "software engineering"],
        &[
            "rust",
            "python",
            "javascript",
            "typescript",
            "golang",
            "compiler",
            "programming language",
            "git",
            "github",
            "gitlab",
            "open source",
            "linux",
            "kernel",
            "database",
            "postgres",
            "sql",
        ],
    ),
    (
        &["business", "markets", "economy", "finance"],
        &[
            "shares",
            "stocks",
            "earnings",
            "forecast",
            "forecasts",
            "revenue",
            "investors",
            "ipo",
            "inflation",
            "interest rates",
            "central bank",
            "funding",
            "raises",
            "acquired",
            "acquisition",
            "startup",
            "startups",
        ],
    ),
    (
        &["rust", "rustlang"],
        &[
            "cargo",
            "crates",
            "rustc",
            "borrow checker",
            "clippy",
            "miri",
        ],
    ),
    (
        &["science", "research"],
        &[
            "study",
            "scientists",
            "researchers",
            "physics",
            "biology",
            "chemistry",
            "genome",
            "species",
            "quantum",
            "neuroscience",
            "evolution",
        ],
    ),
];

/// Where an article matched, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    Title,
    Source,
    Text,
    Related,
    /// Close in meaning (semantic search) without a word match.
    Similar,
}

impl Strength {
    pub fn marker(self) -> &'static str {
        match self {
            Strength::Title => "T",
            Strength::Source => "F",
            Strength::Text => "B",
            Strength::Related => "~",
            Strength::Similar => "≈",
        }
    }
}

pub struct Hit {
    /// The weakest match across the query's words.
    pub strength: Strength,
    pub reasons: Vec<String>,
}

/// One word (or known phrase) of the query: the word itself plus related terms.
struct Group {
    label: String,
    direct: Vec<String>,
    related: Vec<String>,
}

impl Group {
    /// A broad topic ("ai") needs two mentions in the text, so a passing
    /// reference does not count; a specific word ("kimi") needs one.
    fn min_mentions(&self) -> usize {
        if self.related.is_empty() { 1 } else { 2 }
    }
}

pub struct Query {
    groups: Vec<Group>,
}

impl Query {
    pub fn parse(text: &str) -> Self {
        let words: Vec<String> = normalize(text)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let mut groups = Vec::new();
        let mut i = 0;
        while i < words.len() {
            // Prefer the longest known topic phrase starting here ("climate change").
            let topic = (1..=3.min(words.len() - i)).rev().find_map(|n| {
                let phrase = words[i..i + n].join(" ");
                TOPICS
                    .iter()
                    .find(|(names, _)| names.contains(&phrase.as_str()))
                    .map(|t| (n, phrase, t))
            });
            let (n, label, related) = match topic {
                Some((n, phrase, (names, related))) => {
                    let mut terms: Vec<String> = names.iter().map(|s| s.to_string()).collect();
                    terms.extend(related.iter().map(|s| s.to_string()));
                    (n, phrase, terms)
                }
                None => (1, words[i].clone(), Vec::new()),
            };
            groups.push(Group {
                direct: variants(&label),
                label,
                related,
            });
            i += n;
        }
        Self { groups }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// The word plus a simple singular/plural form, so "agent" finds "agents".
fn variants(word: &str) -> Vec<String> {
    let mut out = vec![word.to_string()];
    if word.len() > 3 && word.ends_with('s') {
        out.push(word[..word.len() - 1].to_string());
    } else if word.len() > 2 {
        out.push(format!("{word}s"));
    }
    out
}

struct Doc {
    title: String,
    source: String,
    text: String,
}

pub struct Index {
    docs: Vec<Doc>,
}

impl Index {
    /// Indexes every article in `ds.items`, in the same order.
    pub fn build(ds: &Dataset) -> Self {
        let docs = ds
            .items
            .iter()
            .map(|item| {
                let feed = ds.feed(item.feed_id);
                let source = format!(
                    "{} {} {}",
                    feed.map_or("", |f| f.name()),
                    feed.and_then(|f| f.folder.as_deref()).unwrap_or(""),
                    item.authors.join(" ")
                );
                Doc {
                    title: normalize(item.title()),
                    source: normalize(&source),
                    text: normalize(
                        &item
                            .body_html()
                            .map(|h| html_to_text(h, 10_000))
                            .unwrap_or_default(),
                    ),
                }
            })
            .collect();
        Self { docs }
    }

    /// Every query word must match somewhere. Related terms in the article text
    /// need two mentions in total so a passing reference does not count.
    pub fn search(&self, item: usize, query: &Query) -> Option<Hit> {
        let doc = &self.docs[item];
        let mut strength = Strength::Title;
        let mut reasons = Vec::new();
        for group in &query.groups {
            let all = || group.direct.iter().chain(&group.related);
            let (s, reason) = if let Some(t) = all().find(|t| contains(&doc.title, t)) {
                (Strength::Title, format!("title has “{t}”"))
            } else if let Some(t) = all().find(|t| contains(&doc.source, t)) {
                (Strength::Source, format!("feed/folder/author has “{t}”"))
            } else if let Some(t) = group
                .direct
                .iter()
                .find(|t| count(&doc.text, t) >= group.min_mentions())
            {
                (
                    Strength::Text,
                    format!("text has “{t}” ×{}", count(&doc.text, t)),
                )
            } else {
                let found: Vec<(&String, usize)> = group
                    .related
                    .iter()
                    .map(|t| (t, count(&doc.text, t)))
                    .filter(|&(_, n)| n > 0)
                    .collect();
                let total: usize = found.iter().map(|&(_, n)| n).sum();
                if total < 2 {
                    return None;
                }
                let terms: Vec<String> = found
                    .iter()
                    .take(4)
                    .map(|(t, n)| format!("“{t}” ×{n}"))
                    .collect();
                (
                    Strength::Related,
                    format!("text mentions {}", terms.join(", ")),
                )
            };
            strength = strength.max(s);
            reasons.push(format!("{}: {reason}", group.label));
        }
        Some(Hit { strength, reasons })
    }
}

/// Lowercase words separated by single spaces, padded so " word " matches whole words.
fn normalize(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    format!(" {} ", words.join(" "))
}

fn contains(haystack: &str, term: &str) -> bool {
    haystack.contains(&format!(" {term} "))
}

/// Matches only the leading space, so adjacent repeats (" rust rust ") both count.
fn count(haystack: &str, term: &str) -> usize {
    haystack
        .match_indices(&format!(" {term}"))
        .filter(|(i, _)| haystack[i + 1 + term.len()..].starts_with(' '))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(docs: &[(&str, &str, &str)]) -> Index {
        Index {
            docs: docs
                .iter()
                .map(|(title, source, text)| Doc {
                    title: normalize(title),
                    source: normalize(source),
                    text: normalize(text),
                })
                .collect(),
        }
    }

    fn find(index: &Index, query: &str) -> Vec<usize> {
        let query = Query::parse(query);
        (0..index.docs.len())
            .filter(|&i| index.search(i, &query).is_some())
            .collect()
    }

    #[test]
    fn matches_whole_words_only() {
        let idx = index(&[
            ("New maintainer for the railway crate", "", ""),
            ("AI in practice", "", ""),
        ]);
        assert_eq!(find(&idx, "ai"), vec![1]);
    }

    #[test]
    fn topics_widen_to_related_terms() {
        let idx = index(&[
            ("OpenAI ships a new model", "", ""),
            ("Gardening tips", "", ""),
        ]);
        let query = Query::parse("ai");
        let hit = idx.search(0, &query).unwrap();
        assert_eq!(hit.strength, Strength::Title);
        assert!(hit.reasons[0].contains("openai"), "{:?}", hit.reasons);
        assert!(idx.search(1, &query).is_none());
    }

    #[test]
    fn longest_topic_phrase_wins() {
        let idx = index(&[("Record emissions this year", "", "")]);
        assert_eq!(find(&idx, "climate change"), vec![0]);
    }

    #[test]
    fn plural_and_singular_forms_match() {
        let idx = index(&[("Agents everywhere", "", ""), ("One agent", "", "")]);
        assert_eq!(find(&idx, "agent"), vec![0, 1]);
        assert_eq!(find(&idx, "agents"), vec![0, 1]);
    }

    #[test]
    fn every_query_word_must_match() {
        let idx = index(&[("Rust compiler news", "", ""), ("Compiler news", "", "")]);
        assert_eq!(find(&idx, "rust compiler"), vec![0]);
    }

    #[test]
    fn broad_topics_need_two_mentions_in_the_text() {
        let once = "a passing mention of an llm";
        let twice = "an llm here and another llm there";
        let idx = index(&[("x", "", once), ("x", "", twice)]);
        assert_eq!(find(&idx, "ai"), vec![1]);
        // A specific word needs only one.
        let idx = index(&[("x", "", "kimi was released")]);
        assert_eq!(find(&idx, "kimi"), vec![0]);
    }

    #[test]
    fn source_and_text_matches_rank_below_title() {
        let idx = index(&[
            ("Weekly notes", "Rust Blog Programming", ""),
            ("Weekly notes", "", "rust rust"),
        ]);
        let query = Query::parse("rust");
        assert_eq!(idx.search(0, &query).unwrap().strength, Strength::Source);
        assert_eq!(idx.search(1, &query).unwrap().strength, Strength::Text);
        assert!(Strength::Related < Strength::Similar);
        assert_eq!(Strength::Similar.marker(), "≈");
    }

    #[test]
    fn counts_adjacent_repeats() {
        assert_eq!(count(&normalize("rust rust"), "rust"), 2);
        assert_eq!(count(&normalize("rust, trust rusty rust"), "rust"), 2);
        assert_eq!(
            count(
                &normalize("large language model large language model"),
                "large language model"
            ),
            2
        );
    }

    #[test]
    fn empty_query_is_empty() {
        assert!(Query::parse("  ").is_empty());
        assert!(Query::parse("-- !!").is_empty());
    }
}
