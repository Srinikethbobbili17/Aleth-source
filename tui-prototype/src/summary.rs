//! Article summaries from a local LLM through Ollama. The model sees the article
//! as numbered paragraphs and must cite the paragraphs each point comes from;
//! every point is then checked against the paragraphs it cites. Summaries are
//! written on a background thread and streamed into the reader, and finished
//! ones are cached next to the fixture, so each article is summarized once.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::article;
use crate::data::Item;

/// Small enough for a laptop CPU (about 20 s per article), and it follows the
/// citation format well. `ALETH_SUMMARY_MODEL` picks another Ollama model.
pub const DEFAULT_MODEL: &str = "qwen2.5:3b";
pub const API: &str = "http://127.0.0.1:11434/api/chat";
/// Shorter articles are their own summary.
pub const MIN_WORDS: usize = 80;
/// Longer articles are cut here, to bound CPU time and stay inside the context window.
const MAX_INPUT_WORDS: usize = 3_000;
/// A point counts as supported when at least this share of its content words
/// appear in the paragraphs it cites.
pub const MIN_SUPPORT: f32 = 0.5;

const SYSTEM: &str = "\
You summarize articles for a news reader. The article's paragraphs are numbered like [1], [2].

Write 2 to 4 bullet points with the most important facts.
- Each bullet is one sentence of at most 30 words.
- Use only facts stated in the article. Do not add opinions or outside knowledge.
- End each bullet with the numbers of the paragraphs it is based on, in square brackets, like [2] or [3, 5].
- Write in English.
Output only the bullets, each on its own line starting with \"- \".";

/// What the feed gives a summary to work with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    FullText,
    Excerpt,
    TitleOnly,
}

impl Input {
    /// Feeds put the article in `content`, or only in `summary`, which may be a
    /// teaser or the whole post; the word count says which.
    pub fn of(has_content: bool, words: usize) -> Self {
        if words == 0 {
            Input::TitleOnly
        } else if has_content {
            Input::FullText
        } else {
            Input::Excerpt
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Input::FullText => "full text",
            Input::Excerpt => "the feed's summary",
            Input::TitleOnly => "title only",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub text: String,
    /// 1-based paragraph numbers, all within the article.
    pub cites: Vec<usize>,
    /// Share of the point's content words found in the cited paragraphs.
    pub support: f32,
}

impl Point {
    pub fn supported(&self) -> bool {
        !self.cites.is_empty() && self.support >= MIN_SUPPORT
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub model: String,
    /// The article version that was summarized; a newer one is summarized again.
    pub version_id: i64,
    pub points: Vec<Point>,
    pub seconds: f32,
    /// Paragraphs the model was given, and whether the article was cut short.
    pub paragraphs: usize,
    pub truncated: bool,
}

pub enum State {
    /// Waiting for or receiving the model's answer; `text` is what has arrived.
    Writing {
        started: Instant,
        text: String,
    },
    Done(Summary),
    Failed(String),
}

struct Job {
    item_id: i64,
    version_id: i64,
    title: String,
    paragraphs: Vec<String>,
}

enum Event {
    Text(i64, String),
    Finished(i64, Result<Summary, String>),
    /// Abandoned for a newer request; asked for again when the article is reopened.
    Dropped(i64),
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    model: String,
    items: HashMap<i64, Summary>,
}

pub struct Summarizer {
    pub model: String,
    states: HashMap<i64, State>,
    jobs: Sender<Job>,
    events: Receiver<Event>,
    /// The article the reader wants; the worker gives up on any other.
    wanted: Arc<AtomicI64>,
    cache_path: Option<PathBuf>,
}

/// `data/trial.json` → `data/trial.qwen2.5-3b.summaries.json`.
fn cache_path(fixture: &Path, model: &str) -> PathBuf {
    let name: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    fixture.with_extension(format!("{name}.summaries.json"))
}

impl Summarizer {
    /// Uses `ALETH_SUMMARY_MODEL` or the default model, with the cache next to `fixture`.
    pub fn for_fixture(fixture: &Path) -> Self {
        let model = std::env::var("ALETH_SUMMARY_MODEL")
            .ok()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let path = cache_path(fixture, &model);
        Self::new(&model, API, Some(path))
    }

    pub(crate) fn new(model: &str, api: &str, cache_path: Option<PathBuf>) -> Self {
        let mut states = HashMap::new();
        // A missing or unreadable cache only means summarizing again.
        if let Some(cache) = cache_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<CacheFile>(&t).ok())
            .filter(|c| c.model == model)
        {
            for (id, summary) in cache.items {
                states.insert(id, State::Done(summary));
            }
        }
        let wanted = Arc::new(AtomicI64::new(i64::MIN));
        let (jobs, events) = spawn_worker(model.to_string(), api.to_string(), wanted.clone());
        Self {
            model: model.to_string(),
            states,
            jobs,
            events,
            wanted,
            cache_path,
        }
    }

    pub fn state(&self, item_id: i64) -> Option<&State> {
        self.states.get(&item_id)
    }

    /// A finished summary of this version of the article, if there is one.
    pub fn done(&self, item: &Item) -> Option<&Summary> {
        match self.states.get(&item.id) {
            Some(State::Done(s)) if s.version_id == item.version_id => Some(s),
            _ => None,
        }
    }

    /// Starts summarizing `item` unless it is too short, already summarized, or
    /// being written. `again` replaces a finished or failed summary. Any other
    /// article being written is abandoned.
    pub fn request(&mut self, item: &Item, again: bool) {
        self.wanted.store(item.id, Ordering::Relaxed);
        let current = match self.states.get(&item.id) {
            Some(State::Writing { .. }) => return,
            Some(State::Done(s)) => s.version_id == item.version_id,
            Some(State::Failed(_)) => true,
            None => false,
        };
        if current && !again {
            return;
        }
        let paragraphs = item.body_html().map(article::plain).unwrap_or_default();
        if word_count(&paragraphs) < MIN_WORDS {
            return;
        }
        let job = Job {
            item_id: item.id,
            version_id: item.version_id,
            title: item.title().to_string(),
            paragraphs,
        };
        let state = match self.jobs.send(job) {
            Ok(()) => State::Writing {
                started: Instant::now(),
                text: String::new(),
            },
            Err(_) => State::Failed("summary worker stopped".into()),
        };
        self.states.insert(item.id, state);
    }

    /// Applies what the worker sent since the last call. True if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            changed = true;
            match event {
                Event::Text(id, text) => {
                    if let Some(State::Writing { text: t, .. }) = self.states.get_mut(&id) {
                        *t = text;
                    }
                }
                Event::Finished(id, Ok(summary)) => {
                    self.states.insert(id, State::Done(summary));
                    self.save();
                }
                Event::Finished(id, Err(e)) => {
                    self.states.insert(id, State::Failed(e));
                }
                Event::Dropped(id) => {
                    if matches!(self.states.get(&id), Some(State::Writing { .. })) {
                        self.states.remove(&id);
                    }
                }
            }
        }
        changed
    }

    #[cfg(test)]
    pub(crate) fn writing(&self) -> bool {
        self.states
            .values()
            .any(|s| matches!(s, State::Writing { .. }))
    }

    fn save(&self) {
        let Some(path) = &self.cache_path else { return };
        let items = self
            .states
            .iter()
            .filter_map(|(id, s)| match s {
                State::Done(s) => Some((*id, s.clone())),
                _ => None,
            })
            .collect();
        let file = CacheFile {
            model: self.model.clone(),
            items,
        };
        // Write then rename, so a crash never leaves half a file.
        let tmp = path.with_extension("tmp");
        if let Ok(json) = serde_json::to_string(&file)
            && std::fs::write(&tmp, json).is_ok()
        {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    #[cfg(test)]
    pub(crate) fn insert(&mut self, item_id: i64, summary: Summary) {
        self.states.insert(item_id, State::Done(summary));
    }

    /// Blocks until nothing is being written.
    #[cfg(test)]
    pub(crate) fn wait(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(300);
        while self.writing() {
            assert!(Instant::now() < deadline, "no summary from Ollama");
            self.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub fn word_count(paragraphs: &[String]) -> usize {
    paragraphs
        .iter()
        .map(|p| p.split_whitespace().count())
        .sum()
}

/// Runs jobs one at a time, always the latest: older queued jobs are dropped.
fn spawn_worker(
    model: String,
    api: String,
    wanted: Arc<AtomicI64>,
) -> (Sender<Job>, Receiver<Event>) {
    let (jobs, inbox) = mpsc::channel::<Job>();
    let (outbox, events) = mpsc::channel();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_millis(500)))
        // The first request loads the model into memory.
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .timeout_recv_body(Some(Duration::from_secs(600)))
        .http_status_as_error(false)
        .build()
        .into();
    std::thread::spawn(move || {
        while let Ok(mut job) = inbox.recv() {
            while let Ok(newer) = inbox.try_recv() {
                let _ = outbox.send(Event::Dropped(job.item_id));
                job = newer;
            }
            if wanted.load(Ordering::Relaxed) != job.item_id {
                let _ = outbox.send(Event::Dropped(job.item_id));
                continue;
            }
            let event = write_summary(&agent, &api, &model, &job, &wanted, &outbox);
            if outbox.send(event).is_err() {
                break;
            }
        }
    });
    (jobs, events)
}

#[derive(Deserialize)]
struct Chunk {
    message: Option<ChunkMessage>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ChunkMessage {
    content: String,
}

/// The user message: the title, then the paragraphs numbered as the reader shows them.
fn prompt(job: &Job) -> (String, usize, bool) {
    let mut out = format!("Title: {}\n", job.title);
    let (mut words, mut used) = (0, 0);
    for (n, p) in job.paragraphs.iter().enumerate() {
        if words >= MAX_INPUT_WORDS {
            break;
        }
        words += p.split_whitespace().count();
        used += 1;
        out.push_str(&format!("\n[{}] {p}\n", n + 1));
    }
    (out, used, used < job.paragraphs.len())
}

fn write_summary(
    agent: &ureq::Agent,
    api: &str,
    model: &str,
    job: &Job,
    wanted: &AtomicI64,
    outbox: &Sender<Event>,
) -> Event {
    let started = Instant::now();
    let (user, used, truncated) = prompt(job);
    let body = serde_json::json!({
        "model": model,
        "stream": true,
        "messages": [
            { "role": "system", "content": SYSTEM },
            { "role": "user", "content": user },
        ],
        "options": { "temperature": 0, "num_ctx": 8192, "num_predict": 400 },
    });
    let mut response = match agent.post(api).send_json(&body) {
        Ok(r) => r,
        Err(e) => return Event::Finished(job.item_id, Err(format!("Ollama: {e}"))),
    };
    if response.status() != 200 {
        let status = response.status();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        let detail = serde_json::from_str::<Chunk>(&text)
            .ok()
            .and_then(|c| c.error)
            .unwrap_or(text);
        let hint = if status == 404 && detail.contains("not found") {
            format!("model {model} is not installed (ollama pull {model})")
        } else {
            format!("Ollama: {status} {detail}")
        };
        return Event::Finished(job.item_id, Err(hint));
    }

    let mut text = String::new();
    for line in BufReader::new(response.body_mut().as_reader()).lines() {
        // Closing the connection makes Ollama stop generating.
        if wanted.load(Ordering::Relaxed) != job.item_id {
            return Event::Dropped(job.item_id);
        }
        let line = match line {
            Ok(l) => l,
            Err(e) => return Event::Finished(job.item_id, Err(format!("Ollama: {e}"))),
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Chunk>(&line) {
            Ok(Chunk { error: Some(e), .. }) => {
                return Event::Finished(job.item_id, Err(format!("Ollama: {e}")));
            }
            Ok(Chunk {
                message: Some(m), ..
            }) => {
                text.push_str(&m.content);
                let _ = outbox.send(Event::Text(job.item_id, text.clone()));
            }
            Ok(_) => {}
            Err(e) => {
                return Event::Finished(job.item_id, Err(format!("bad reply from Ollama: {e}")));
            }
        }
    }

    let paragraphs = &job.paragraphs[..used];
    let points = parse(&text, paragraphs);
    if points.is_empty() {
        return Event::Finished(job.item_id, Err("the model wrote no usable points".into()));
    }
    Event::Finished(
        job.item_id,
        Ok(Summary {
            model: model.to_string(),
            version_id: job.version_id,
            points,
            seconds: started.elapsed().as_secs_f32(),
            paragraphs: used,
            truncated,
        }),
    )
}

/// Points from the model's bullets. Citations may sit anywhere in a line
/// ("[2]", "[3, 5]", "[¶4]", "[2-4]"); ones outside the article are dropped.
pub fn parse(text: &str, paragraphs: &[String]) -> Vec<Point> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let bullets: Vec<&str> = lines.iter().filter_map(|l| strip_bullet(l)).collect();
    // Without bullets, every line is a point; with them, other lines are preamble.
    let picked = if bullets.is_empty() { lines } else { bullets };
    picked
        .into_iter()
        .filter_map(|line| {
            let (text, cited) = take_citations(line);
            let cites: Vec<usize> = cited
                .into_iter()
                .filter(|&n| (1..=paragraphs.len()).contains(&n))
                .collect();
            if !text.chars().any(char::is_alphabetic) {
                return None;
            }
            let sources: Vec<&str> = cites.iter().map(|&n| paragraphs[n - 1].as_str()).collect();
            Some(Point {
                support: support(&text, &sources),
                text,
                cites,
            })
        })
        .collect()
}

fn strip_bullet(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "• "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim());
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let rest = &line[digits..];
    (digits > 0 && (rest.starts_with(". ") || rest.starts_with(") "))).then(|| rest[2..].trim())
}

/// Removes bracketed paragraph numbers from `line`; returns the text and the
/// numbers in order, without repeats.
fn take_citations(line: &str) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut cites = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']').map(|c| open + c) else {
            break;
        };
        let inner = &rest[open + 1..close];
        match citation_numbers(inner) {
            Some(nums) => {
                text.push_str(&rest[..open]);
                for n in nums {
                    if !cites.contains(&n) {
                        cites.push(n);
                    }
                }
            }
            None => text.push_str(&rest[..=close]),
        }
        rest = &rest[close + 1..];
    }
    text.push_str(rest);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text
        .replace(" .", ".")
        .replace(" ,", ",")
        .trim_matches(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .to_string();
    (text, cites)
}

/// "3, 5" / "¶4" / "2-4" / "2 and 3" → the numbers; `None` if not a citation.
fn citation_numbers(inner: &str) -> Option<Vec<usize>> {
    let cleaned = inner.replace(['¶', '§'], "").replace(" and ", ",");
    let mut out = Vec::new();
    for part in cleaned.split([',', ';']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once(['-', '–']) {
            let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
            if b < a || b - a > 20 {
                return None;
            }
            out.extend(a..=b);
        } else {
            out.push(part.parse().ok()?);
        }
    }
    (!out.is_empty()).then_some(out)
}

const STOPWORDS: &[&str] = &[
    "about", "after", "also", "been", "before", "being", "between", "both", "but", "could", "does",
    "down", "during", "each", "from", "have", "having", "here", "into", "just", "like", "made",
    "make", "many", "more", "most", "much", "only", "other", "over", "said", "same", "says",
    "should", "since", "some", "such", "than", "that", "their", "them", "then", "there", "these",
    "they", "this", "those", "through", "under", "until", "very", "were", "what", "when", "where",
    "which", "while", "will", "with", "would", "your", "article", "author",
];

/// Content words reduced to a crude stem, so "prices" and "priced" meet.
fn stems(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| {
            (w.chars().count() >= 4 && !STOPWORDS.contains(&w.as_str()))
                || (w.chars().any(|c| c.is_ascii_digit()) && w.len() >= 2)
        })
        .map(|w| w.chars().take(5).collect())
        .collect()
}

/// Share of `point`'s content words that appear in `sources`. A point the model
/// made up, or cited wrongly, shares few words with what it cites.
pub fn support(point: &str, sources: &[&str]) -> f32 {
    let words = stems(point);
    if words.is_empty() {
        return 1.0;
    }
    let known: std::collections::HashSet<String> = sources.iter().flat_map(|s| stems(s)).collect();
    words.iter().filter(|w| known.contains(*w)).count() as f32 / words.len() as f32
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::data::Dataset;
    use crate::semantic::fake_ollama_at;

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/fake.json");
    const UNREACHABLE: &str = "http://127.0.0.1:1/api/chat";

    fn paras(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| s.to_string()).collect()
    }

    /// The longest article in the fake fixture, so it is worth summarizing.
    pub(super) fn long_item(ds: &Dataset) -> &Item {
        ds.items
            .iter()
            .max_by_key(|i| word_count(&i.body_html().map(article::plain).unwrap_or_default()))
            .unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aleth-sum-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stream_line(content: &str) -> String {
        serde_json::json!({"message": {"role": "assistant", "content": content}, "done": false})
            .to_string()
            + "\n"
    }

    /// An Ollama /api/chat reply streamed as one JSON object per line.
    fn stream(parts: &[&str]) -> String {
        let mut out: String = parts.iter().map(|p| stream_line(p)).collect();
        out.push_str("{\"done\":true}\n");
        out
    }

    #[test]
    fn parses_bullets_and_citations_in_any_position() {
        let p = paras(&["alpha", "beta", "gamma", "delta", "epsilon"]);
        let text = "Here are the points:\n\
            - The cloud outage hit Europe [2].\n\
            - [3] Engineers restored service.\n\
            * Costs rose [¶4, ¶5] sharply\n\
            1. A range of paragraphs [1-3]\n\
            - Invented claim [9]\n\
            - Keeps [brackets] that are not citations [2 and 4]";
        let points = parse(text, &p);
        let got: Vec<(&str, Vec<usize>)> = points
            .iter()
            .map(|p| (p.text.as_str(), p.cites.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("The cloud outage hit Europe.", vec![2]),
                ("Engineers restored service.", vec![3]),
                ("Costs rose sharply", vec![4, 5]),
                ("A range of paragraphs", vec![1, 2, 3]),
                ("Invented claim", vec![]),
                ("Keeps [brackets] that are not citations", vec![2, 4]),
            ]
        );
        assert!(
            !points[4].supported(),
            "a citation outside the article is no source"
        );
    }

    #[test]
    fn plain_lines_count_when_there_are_no_bullets() {
        let points = parse(
            "The outage lasted hours. [1]\n\nIt is over. [1]",
            &paras(&["x"]),
        );
        assert_eq!(points.len(), 2);
    }

    #[test]
    fn support_measures_shared_content_words() {
        let source = "Prices for the new model start at $2.00 per million input tokens.";
        assert!(support("Input tokens are priced at $2.00 per million.", &[source]) >= 0.8);
        assert!(
            support(
                "The company was founded in Beijing by former students.",
                &[source]
            ) < 0.2
        );
        assert_eq!(support("It is so.", &[source]), 1.0, "nothing to check");
    }

    #[test]
    fn cache_file_is_named_after_fixture_and_model() {
        assert_eq!(
            cache_path(Path::new("data/trial.json"), "qwen2.5:3b"),
            Path::new("data/trial.qwen2.5-3b.summaries.json")
        );
    }

    #[test]
    fn short_articles_are_not_sent_to_the_model() {
        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        let short = ds
            .items
            .iter()
            .find(|i| {
                word_count(&i.body_html().map(article::plain).unwrap_or_default()) < MIN_WORDS
            })
            .unwrap();
        let mut s = Summarizer::new("m", UNREACHABLE, None);
        s.request(short, false);
        assert!(s.state(short.id).is_none() && !s.writing());
    }

    #[test]
    fn streams_a_summary_checks_it_and_caches_it() {
        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        let item = long_item(&ds);
        let paragraphs = article::plain(item.body_html().unwrap());
        // Quote paragraph 1 so the point is well supported.
        let quote: String = paragraphs[0]
            .split_whitespace()
            .take(12)
            .collect::<Vec<_>>()
            .join(" ");
        let reply = stream(&["- ", &quote, " [1]\n", "- Made up entirely [99]\n"]);
        let (api, server) = fake_ollama_at("/api/chat", 200, &reply);
        let dir = temp_dir("stream");
        let cache = dir.join("fake.m.summaries.json");

        let mut s = Summarizer::new("m", &api, Some(cache.clone()));
        s.request(item, false);
        assert!(matches!(s.state(item.id), Some(State::Writing { .. })));
        s.wait();
        let request: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(request["model"], "m");
        assert_eq!(request["stream"], true);
        assert_eq!(request["messages"][0]["content"], SYSTEM);
        let user = request["messages"][1]["content"].as_str().unwrap();
        assert!(
            user.starts_with(&format!("Title: {}\n\n[1] ", item.title())),
            "{user}"
        );
        assert!(user.contains(&format!("\n[{}] ", paragraphs.len())));

        let summary = s.done(item).unwrap();
        assert_eq!(summary.points.len(), 2);
        assert!(summary.points[0].supported(), "{:?}", summary.points[0]);
        assert_eq!(summary.points[1].cites, Vec::<usize>::new());
        assert!(!summary.points[1].supported());

        // A new summarizer reads the cache and does not ask the model again.
        let mut again = Summarizer::new("m", UNREACHABLE, Some(cache.clone()));
        assert_eq!(again.done(item).unwrap().points, summary.points);
        again.request(item, false);
        assert!(!again.writing());
        // A cache made with another model is ignored.
        assert!(
            Summarizer::new("other", UNREACHABLE, Some(cache))
                .done(item)
                .is_none()
        );
    }

    #[test]
    fn missing_model_says_how_to_install_it() {
        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        let item = long_item(&ds);
        let (api, server) = fake_ollama_at(
            "/api/chat",
            404,
            r#"{"error":"model \"m\" not found, try pulling it first"}"#,
        );
        let mut s = Summarizer::new("m", &api, None);
        s.request(item, false);
        s.wait();
        server.join().unwrap();
        match s.state(item.id) {
            Some(State::Failed(e)) => assert_eq!(e, "model m is not installed (ollama pull m)"),
            _ => panic!("expected a failure"),
        }
    }

    #[test]
    fn ollama_down_is_reported_and_retried_on_request() {
        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        let item = long_item(&ds);
        let mut s = Summarizer::new("m", UNREACHABLE, None);
        s.request(item, false);
        s.wait();
        assert!(matches!(s.state(item.id), Some(State::Failed(e)) if e.starts_with("Ollama:")));
        s.request(item, false);
        assert!(!s.writing(), "a failure is not retried by itself");
        s.request(item, true);
        assert!(s.writing(), "but is when asked again");
    }

    #[test]
    fn opening_another_article_abandons_the_first() {
        use std::io::{BufRead, Read, Write};

        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        let a = long_item(&ds);
        // A second long article: the same text under another id.
        let b = &Item {
            id: a.id + 1000,
            title: Some("Copy".into()),
            content: a.content.clone(),
            external_id: None,
            url: None,
            authors: Vec::new(),
            summary: None,
            content_type: None,
            ..*a
        };
        // Streams one chunk for `a`, then waits for the test before sending more.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}/api/chat", listener.local_addr().unwrap());
        let (go_on, wait) = mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            reader.read_exact(&mut vec![0; len]).unwrap();
            let chunk = |s: &mut std::net::TcpStream, body: &str| {
                write!(s, "{:x}\r\n{body}\r\n", body.len()).unwrap();
                s.flush().unwrap();
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
            )
            .unwrap();
            chunk(&mut stream, &stream_line("- First point"));
            wait.recv().unwrap();
            // The client may already have hung up.
            let _ = write!(stream, "{:x}\r\n{}\r\n0\r\n\r\n", 20, " ".repeat(19) + "\n");
        });

        let mut s = Summarizer::new("m", &api, None);
        s.request(a, false);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(s.state(a.id), Some(State::Writing { text, .. }) if !text.is_empty()) {
            assert!(Instant::now() < deadline, "first chunk never arrived");
            s.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        s.request(b, false);
        go_on.send(()).unwrap();
        server.join().unwrap();
        // `a` is abandoned; `b` then fails because the one-shot server is gone.
        s.wait();
        assert!(s.state(a.id).is_none(), "abandoned summary is forgotten");
        assert!(matches!(s.state(b.id), Some(State::Failed(_))));
        // Reopening `a` starts it again.
        s.request(a, false);
        assert!(matches!(s.state(a.id), Some(State::Writing { .. })));
    }

    #[test]
    fn a_new_article_version_is_summarized_again() {
        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        let item = long_item(&ds);
        let mut s = Summarizer::new("m", UNREACHABLE, None);
        s.states.insert(
            item.id,
            State::Done(Summary {
                model: "m".into(),
                version_id: item.version_id - 1,
                points: vec![],
                seconds: 1.0,
                paragraphs: 1,
                truncated: false,
            }),
        );
        assert!(
            s.done(item).is_none(),
            "stale summary is not shown as current"
        );
        s.request(item, false);
        assert!(s.writing());
    }
}

/// Against a running Ollama with the default model; skips when either is missing.
#[cfg(test)]
mod integration {
    use std::path::Path;

    use super::*;
    use crate::data::Dataset;

    fn model_installed() -> bool {
        let tags = ureq::get("http://127.0.0.1:11434/api/tags")
            .call()
            .and_then(|mut r| r.body_mut().read_to_string());
        match tags {
            Ok(t) if t.contains(&format!("\"{DEFAULT_MODEL}\"")) => true,
            Ok(_) => {
                eprintln!("SKIP: {DEFAULT_MODEL} not installed (ollama pull {DEFAULT_MODEL})");
                false
            }
            Err(_) => {
                eprintln!("SKIP: Ollama is not running on 127.0.0.1:11434");
                false
            }
        }
    }

    #[test]
    fn real_model_writes_cited_supported_points() {
        if !model_installed() {
            return;
        }
        let ds = Dataset::load(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/fake.json"
        )))
        .unwrap();
        let item = super::tests::long_item(&ds);
        let paragraphs = article::plain(item.body_html().unwrap());
        let mut s = Summarizer::new(DEFAULT_MODEL, API, None);
        s.request(item, false);
        s.wait();
        let summary = match s.state(item.id) {
            Some(State::Done(summary)) => summary,
            Some(State::Failed(e)) => panic!("{e}"),
            _ => panic!("no summary"),
        };
        eprintln!("{} in {:.1} s:", item.title(), summary.seconds);
        for p in &summary.points {
            eprintln!("  {:?} support {:.2}: {}", p.cites, p.support, p.text);
        }
        assert!((1..=6).contains(&summary.points.len()));
        assert!(
            summary
                .points
                .iter()
                .all(|p| p.cites.iter().all(|&n| n <= paragraphs.len()))
        );
        let supported = summary.points.iter().filter(|p| p.supported()).count();
        assert!(
            supported * 2 >= summary.points.len(),
            "most points must be supported"
        );
    }
}
