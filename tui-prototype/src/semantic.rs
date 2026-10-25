//! Semantic search with a local Ollama model. Article vectors are precomputed by
//! `scripts/embed_trial.py`. Queries are embedded live on a background thread
//! (about 130 ms each for bge-m3 on CPU, 25 ms for nomic), so typing never waits
//! for Ollama. The only network traffic is to Ollama on localhost.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use serde::Deserialize;

use crate::data::Dataset;

/// Supported models, preferred first, with the prefix each expects on queries.
/// bge-m3 is multilingual (100+ languages, and a query can match an article in
/// another language); nomic-embed-text is English-only but smaller. nomic was
/// trained with task prefixes; its documents use "search_document: ".
const MODELS: &[(&str, &str)] = &[("bge-m3", ""), ("nomic-embed-text", "search_query: ")];
pub const API: &str = "http://127.0.0.1:11434/api/embed";
/// Raw similarity varies a lot between queries (the best "ai" match scores 0.80,
/// the best "kimi" match 0.64), so a match must stand out from this query's own
/// scores instead: at least this many standard deviations above their mean.
/// Tuned by hand on the trial snapshot.
pub const MIN_SIGMA: f32 = 3.0;
/// Shorter queries are mid-typing fragments; their nearest articles are noise.
const MIN_QUERY_CHARS: usize = 3;

#[derive(Deserialize)]
struct EmbeddingsFile {
    model: String,
    items: HashMap<String, Vec<f32>>,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

pub struct Score {
    pub similarity: f32,
    /// Standard deviations above the mean similarity for this query.
    pub sigma: f32,
}

type Embedding = Result<Vec<f32>, String>;

pub struct Semantic {
    pub model: &'static str,
    /// Unit vectors by item index; `None` for articles missing from the file.
    vectors: Vec<Option<Vec<f32>>>,
    pub embedded: usize,
    #[cfg(test)]
    query_prefix: &'static str,
    /// Query embeddings (or the error) by query text, so editing a query back is
    /// free. Errors stay until restart, so a down Ollama is not asked again and again.
    cache: HashMap<String, Embedding>,
    /// The latest query sent to the worker and not answered yet.
    pending: Option<String>,
    requests: Sender<String>,
    results: Receiver<(String, Embedding)>,
}

/// `data/trial.json` → `data/trial.bge-m3.embeddings.json`.
fn embeddings_path(fixture: &Path, model: &str) -> PathBuf {
    fixture.with_extension(format!("{model}.embeddings.json"))
}

impl Semantic {
    /// Loads the precomputed vectors of the most preferred model that has a file
    /// next to the fixture, aligned with `ds.items`. `Ok(None)` when none has.
    pub fn load(ds: &Dataset, fixture: &Path) -> Result<Option<Self>, String> {
        let Some((model, query_prefix, path)) = MODELS.iter().find_map(|&(model, prefix)| {
            let path = embeddings_path(fixture, model);
            path.exists().then_some((model, prefix, path))
        }) else {
            return Ok(None);
        };
        let path = path.as_path();
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file: EmbeddingsFile =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if file.model != model {
            return Err(format!(
                "{} holds {} vectors, not {model}",
                path.display(),
                file.model
            ));
        }
        let vectors = ds
            .items
            .iter()
            .map(|item| file.items.get(&item.id.to_string()).map(|v| unit(v)))
            .collect();
        Self::new(model, query_prefix, vectors, API)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// `vectors` are by item index and must all have the same, non-zero length.
    pub(crate) fn new(
        model: &'static str,
        query_prefix: &'static str,
        vectors: Vec<Option<Vec<f32>>>,
        api: &str,
    ) -> Result<Self, String> {
        let mut known = vectors.iter().flatten();
        let dims = known.next().map_or(0, Vec::len);
        if dims == 0 || known.any(|v| v.len() != dims) {
            return Err("article vectors are empty or differ in length".into());
        }
        if vectors.iter().flatten().flatten().any(|x| !x.is_finite()) {
            return Err("article vectors contain NaN or infinity".into());
        }
        let embedder = Embedder {
            model,
            query_prefix,
            dims,
            api: api.to_string(),
            agent: ureq::Agent::config_builder()
                .timeout_connect(Some(Duration::from_millis(500)))
                // The first query loads the model into memory, which can take a few seconds.
                .timeout_global(Some(Duration::from_secs(30)))
                .build()
                .into(),
        };
        let (requests, results) = spawn_worker(embedder);
        Ok(Self {
            model,
            embedded: vectors.iter().filter(|v| v.is_some()).count(),
            vectors,
            #[cfg(test)]
            query_prefix,
            cache: HashMap::new(),
            pending: None,
            requests,
            results,
        })
    }

    /// Scores every article against `query`, by item index. Never waits for
    /// Ollama: `Ok(None)` for queries too short to search, or while the query is
    /// being embedded (`poll` reports when it is done); `Err` when embedding failed.
    pub fn scores(&mut self, query: &str) -> Result<Option<Vec<Option<Score>>>, String> {
        let query = query.trim();
        if query.chars().count() < MIN_QUERY_CHARS {
            return Ok(None);
        }
        match self.cache.get(query) {
            Some(Ok(q)) => Ok(Some(score(q, &self.vectors))),
            Some(Err(e)) => Err(e.clone()),
            None => {
                if self.pending.as_deref() != Some(query) {
                    if self.requests.send(query.to_string()).is_err() {
                        let e = "semantic search worker stopped".to_string();
                        self.cache.insert(query.to_string(), Err(e.clone()));
                        return Err(e);
                    }
                    self.pending = Some(query.to_string());
                }
                Ok(None)
            }
        }
    }

    /// Stores embeddings that finished since the last call. True when the latest
    /// query's result arrived, so the caller should search again.
    pub fn poll(&mut self) -> bool {
        let mut arrived = false;
        while let Ok((query, result)) = self.results.try_recv() {
            if self.pending.as_ref() == Some(&query) {
                self.pending = None;
                arrived = true;
            }
            self.cache.insert(query, result);
        }
        arrived
    }

    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Blocks until the pending query is answered.
    #[cfg(test)]
    pub(crate) fn wait(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while self.pending() {
            assert!(
                std::time::Instant::now() < deadline,
                "no answer from Ollama"
            );
            self.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Similarity and σ of every article for the unit query vector `q`.
fn score(q: &[f32], vectors: &[Option<Vec<f32>>]) -> Vec<Option<Score>> {
    let sims: Vec<Option<f32>> = vectors
        .iter()
        .map(|v| v.as_ref().map(|v| dot(q, v)))
        .collect();
    let known: Vec<f32> = sims.iter().flatten().copied().collect();
    let n = known.len().max(1) as f32;
    let mean = known.iter().sum::<f32>() / n;
    let sd = (known.iter().map(|s| (s - mean).powi(2)).sum::<f32>() / n)
        .sqrt()
        .max(f32::EPSILON);
    sims.into_iter()
        .map(|s| {
            s.map(|similarity| Score {
                similarity,
                sigma: (similarity - mean) / sd,
            })
        })
        .collect()
}

/// Embeds queries one at a time, always the latest one asked for: queries typed
/// past while Ollama was busy are skipped. Ends when the `Semantic` is dropped.
fn spawn_worker(embedder: Embedder) -> (Sender<String>, Receiver<(String, Embedding)>) {
    let (requests, inbox) = mpsc::channel::<String>();
    let (outbox, results) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(mut query) = inbox.recv() {
            while let Ok(newer) = inbox.try_recv() {
                query = newer;
            }
            let result = embedder.embed(&query);
            if outbox.send((query, result)).is_err() {
                break;
            }
        }
    });
    (requests, results)
}

/// Asks Ollama for query vectors.
#[derive(Clone)]
struct Embedder {
    model: &'static str,
    query_prefix: &'static str,
    /// Length of every article vector; query vectors must match it.
    dims: usize,
    api: String,
    agent: ureq::Agent,
}

impl Embedder {
    fn embed(&self, query: &str) -> Embedding {
        let body = serde_json::json!({ "model": self.model, "input": [format!("{}{query}", self.query_prefix)] });
        let response: EmbedResponse = self
            .agent
            .post(&self.api)
            .send_json(&body)
            .and_then(|mut r| r.body_mut().read_json())
            .map_err(|e| format!("Ollama: {e}"))?;
        let v = response
            .embeddings
            .into_iter()
            .next()
            .ok_or("Ollama returned no embedding")?;
        // A different length means the file was made with another model.
        if v.len() != self.dims {
            return Err(format!(
                "{} returned {} dimensions, the embeddings file has {}",
                self.model,
                v.len(),
                self.dims
            ));
        }
        Ok(unit(&v))
    }
}

fn unit(v: &[f32]) -> Vec<f32> {
    let norm = v
        .iter()
        .map(|x| x * x)
        .sum::<f32>()
        .sqrt()
        .max(f32::EPSILON);
    v.iter().map(|x| x / norm).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A one-shot stand-in for Ollama, so tests can check requests and failures offline.
#[cfg(test)]
pub(crate) fn fake_ollama(status: u16, body: &str) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{BufRead, BufReader, Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let api = format!("http://{}/api/embed", listener.local_addr().unwrap());
    let body = body.to_string();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
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
        let mut request = vec![0; len];
        reader.read_exact(&mut request).unwrap();
        write!(
            stream,
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        String::from_utf8(request).unwrap()
    });
    (api, handle)
}

/// Nothing listens on port 1, so connecting fails at once.
#[cfg(test)]
pub(crate) const UNREACHABLE: &str = "http://127.0.0.1:1/api/embed";

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data/fake.json");

    fn semantic(vectors: Vec<Option<Vec<f32>>>, api: &str) -> Semantic {
        Semantic::new("bge-m3", "", vectors, api).unwrap()
    }

    /// A fresh directory holding a copy of the fake fixture.
    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aleth-proto-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(FAKE, dir.join("fake.json")).unwrap();
        dir
    }

    /// Writes an embeddings file with a 2-d vector for every fake item but `skip`.
    fn write_embeddings(dir: &Path, file_model: &str, model: &str, skip: &[i64]) {
        let ds = Dataset::load(Path::new(FAKE)).unwrap();
        let items: HashMap<String, Vec<f32>> = ds
            .items
            .iter()
            .filter(|i| !skip.contains(&i.id))
            .map(|i| (i.id.to_string(), vec![1.0, i.id as f32]))
            .collect();
        let json = serde_json::json!({ "model": model, "items": items });
        let path = embeddings_path(&dir.join("fake.json"), file_model);
        std::fs::write(path, json.to_string()).unwrap();
    }

    /// Asks for `query`, waits for the worker, then returns the answer.
    pub(super) fn ask(
        sem: &mut Semantic,
        query: &str,
    ) -> Result<Option<Vec<Option<Score>>>, String> {
        assert!(
            sem.scores(query).unwrap().is_none(),
            "first ask must not block"
        );
        assert!(sem.pending());
        sem.wait();
        sem.scores(query)
    }

    fn load(dir: &Path) -> Result<Option<Semantic>, String> {
        let fixture = dir.join("fake.json");
        Semantic::load(&Dataset::load(&fixture).unwrap(), &fixture)
    }

    #[test]
    fn unit_vectors_and_dot_product() {
        let v = unit(&[3.0, 4.0]);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
        assert_eq!(
            unit(&[0.0, 0.0]),
            vec![0.0, 0.0],
            "zero vector must not become NaN"
        );
    }

    #[test]
    fn embeddings_file_sits_next_to_the_fixture() {
        assert_eq!(
            embeddings_path(Path::new("data/trial.json"), "bge-m3"),
            Path::new("data/trial.bge-m3.embeddings.json")
        );
    }

    #[test]
    fn no_embeddings_file_means_no_semantic_search() {
        let dir = fixture_dir("none");
        assert!(load(&dir).unwrap().is_none());
    }

    #[test]
    fn prefers_multilingual_bge_m3_over_nomic() {
        let dir = fixture_dir("prefer");
        write_embeddings(&dir, "nomic-embed-text", "nomic-embed-text", &[]);
        write_embeddings(&dir, "bge-m3", "bge-m3", &[]);
        let sem = load(&dir).unwrap().unwrap();
        assert_eq!((sem.model, sem.query_prefix), ("bge-m3", ""));
    }

    #[test]
    fn falls_back_to_nomic_with_its_query_prefix() {
        let dir = fixture_dir("fallback");
        write_embeddings(&dir, "nomic-embed-text", "nomic-embed-text", &[]);
        let sem = load(&dir).unwrap().unwrap();
        assert_eq!(
            (sem.model, sem.query_prefix),
            ("nomic-embed-text", "search_query: ")
        );
    }

    #[test]
    fn rejects_a_file_made_with_another_model() {
        let dir = fixture_dir("mismatch");
        write_embeddings(&dir, "bge-m3", "nomic-embed-text", &[]);
        let err = load(&dir).err().unwrap();
        assert!(err.contains("holds nomic-embed-text vectors"), "{err}");
    }

    #[test]
    fn rejects_corrupt_or_ragged_vectors() {
        let dir = fixture_dir("corrupt");
        std::fs::write(
            embeddings_path(&dir.join("fake.json"), "bge-m3"),
            "{not json",
        )
        .unwrap();
        assert!(load(&dir).is_err());

        let ragged = vec![Some(vec![1.0, 0.0]), Some(vec![1.0])];
        assert!(Semantic::new("bge-m3", "", ragged, UNREACHABLE).is_err());
        let nan = vec![Some(vec![1.0, f32::NAN])];
        assert!(Semantic::new("bge-m3", "", nan, UNREACHABLE).is_err());
        assert!(Semantic::new("bge-m3", "", vec![None], UNREACHABLE).is_err());
    }

    #[test]
    fn articles_missing_from_the_file_are_not_scored() {
        let dir = fixture_dir("missing");
        write_embeddings(&dir, "bge-m3", "bge-m3", &[101, 102]);
        let sem = load(&dir).unwrap().unwrap();
        let ds = Dataset::load(&dir.join("fake.json")).unwrap();
        assert_eq!(sem.embedded, ds.items.len() - 2);
        for (item, v) in ds.items.iter().zip(&sem.vectors) {
            assert_eq!(v.is_none(), [101, 102].contains(&item.id));
        }
    }

    #[test]
    fn short_queries_skip_ollama() {
        let mut sem = semantic(vec![Some(vec![1.0, 0.0])], UNREACHABLE);
        assert!(sem.scores("ai").unwrap().is_none());
        assert!(sem.scores("  ai  ").unwrap().is_none());
        assert!(sem.cache.is_empty() && !sem.pending());
    }

    #[test]
    fn sigma_is_relative_to_this_querys_scores() {
        let vectors = vec![
            Some(vec![1.0, 0.0, 0.0]),
            Some(vec![0.0, 1.0, 0.0]),
            None,
            Some(vec![0.0, 0.0, 1.0]),
        ];
        let mut sem = semantic(vectors, UNREACHABLE);
        sem.cache.insert("query".into(), Ok(vec![1.0, 0.0, 0.0]));
        let scores = sem.scores("query").unwrap().unwrap();
        // Similarities 1, 0, 0: mean 1/3, standard deviation √2/3.
        let first = scores[0].as_ref().unwrap();
        assert!((first.similarity - 1.0).abs() < 1e-6);
        assert!((first.sigma - 2f32.sqrt()).abs() < 1e-5, "{}", first.sigma);
        assert!((scores[1].as_ref().unwrap().sigma + 1.0 / 2f32.sqrt()).abs() < 1e-5);
        assert!(scores[2].is_none(), "unembedded article must stay unscored");
    }

    #[test]
    fn sends_model_and_prefix_and_normalises_the_reply() {
        let (api, server) = fake_ollama(200, r#"{"embeddings":[[3.0,4.0]]}"#);
        let mut sem = Semantic::new(
            "nomic-embed-text",
            "search_query: ",
            vec![Some(vec![0.6, 0.8]), Some(vec![0.8, -0.6])],
            &api,
        )
        .unwrap();
        let scores = ask(&mut sem, " electric cars ").unwrap().unwrap();
        let request: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(request["model"], "nomic-embed-text");
        assert_eq!(request["input"][0], "search_query: electric cars");
        assert!((scores[0].as_ref().unwrap().similarity - 1.0).abs() < 1e-6);
        // The server answered once; the repeat must come from the cache.
        assert!(sem.scores("electric cars").is_ok());
    }

    #[test]
    fn ollama_errors_are_reported_and_cached() {
        let mut sem = semantic(vec![Some(vec![1.0, 0.0])], UNREACHABLE);
        let err = ask(&mut sem, "electric cars").err().unwrap();
        assert!(err.starts_with("Ollama:"), "{err}");
        assert_eq!(sem.scores("electric cars").err(), Some(err));

        let (api, server) = fake_ollama(500, r#"{"error":"model not found"}"#);
        let mut sem = semantic(vec![Some(vec![1.0, 0.0])], &api);
        assert!(ask(&mut sem, "electric cars").is_err());
        server.join().unwrap();

        let (api, server) = fake_ollama(200, r#"{"embeddings":[]}"#);
        let mut sem = semantic(vec![Some(vec![1.0, 0.0])], &api);
        assert_eq!(
            ask(&mut sem, "electric cars").err().unwrap(),
            "Ollama returned no embedding"
        );
        server.join().unwrap();
    }

    #[test]
    fn never_waits_for_a_slow_ollama() {
        // Accepts connections but never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}/api/embed", listener.local_addr().unwrap());
        let mut sem = semantic(vec![Some(vec![1.0, 0.0])], &api);
        let start = std::time::Instant::now();
        for query in ["ele", "elec", "electric", "electric cars"] {
            assert!(sem.scores(query).unwrap().is_none());
        }
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "{:?}",
            start.elapsed()
        );
        assert!(sem.pending() && !sem.poll());
        assert_eq!(
            sem.pending.as_deref(),
            Some("electric cars"),
            "latest query wins"
        );
    }

    #[test]
    fn rejects_query_vectors_of_another_length() {
        let (api, server) = fake_ollama(200, r#"{"embeddings":[[1.0,0.0,0.0]]}"#);
        let mut sem = semantic(vec![Some(vec![1.0, 0.0])], &api);
        let err = ask(&mut sem, "electric cars").err().unwrap();
        assert!(
            err.contains("returned 3 dimensions, the embeddings file has 2"),
            "{err}"
        );
        server.join().unwrap();
    }
}

/// Against the real trial snapshot and a running Ollama. Each test skips (and
/// says so) when the snapshot, its embeddings or Ollama are missing.
#[cfg(test)]
mod integration {
    use super::*;

    const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/data");
    /// "Electric or gas? BMW goes both ways with new 2027 X5 and iX5."
    const BMW: i64 = 7815;

    fn trial() -> Option<(Dataset, PathBuf)> {
        let path = Path::new(DATA).join("trial.json");
        if !path.exists() {
            eprintln!("SKIP: no data/trial.json");
            return None;
        }
        Some((Dataset::load(&path).unwrap(), path))
    }

    fn ollama_up() -> bool {
        let up = std::net::TcpStream::connect_timeout(
            &"127.0.0.1:11434".parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok();
        if !up {
            eprintln!("SKIP: Ollama is not running on 127.0.0.1:11434");
        }
        up
    }

    /// σ of the BMW article for `query`, with whichever model `fixture` selects.
    fn bmw_sigma(ds: &Dataset, fixture: &Path, model: &str, query: &str) -> f32 {
        let mut sem = Semantic::load(ds, fixture).unwrap().unwrap();
        assert_eq!(sem.model, model);
        let scores = super::tests::ask(&mut sem, query).unwrap().unwrap();
        let i = ds.items.iter().position(|i| i.id == BMW).unwrap();
        scores[i].as_ref().unwrap().sigma
    }

    #[test]
    fn embeddings_files_cover_every_trial_article() {
        let Some((ds, path)) = trial() else { return };
        for (model, dims) in [("bge-m3", 1024), ("nomic-embed-text", 768)] {
            let file = embeddings_path(&path, model);
            if !file.exists() {
                eprintln!("SKIP: no {}", file.display());
                continue;
            }
            let parsed: EmbeddingsFile =
                serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            assert_eq!(parsed.model, model);
            assert_eq!(parsed.items.len(), ds.items.len(), "{model}: stale file?");
            for item in &ds.items {
                let v = &parsed.items[&item.id.to_string()];
                assert_eq!(v.len(), dims, "{model}: item {}", item.id);
                assert!(v.iter().all(|x| x.is_finite()), "{model}: item {}", item.id);
                assert!(
                    v.iter().any(|x| *x != 0.0),
                    "{model}: item {} is all zeros",
                    item.id
                );
            }
        }
    }

    #[test]
    fn bge_m3_finds_related_articles() {
        let Some((ds, path)) = trial() else { return };
        if !embeddings_path(&path, "bge-m3").exists() || !ollama_up() {
            return;
        }
        let sigma = bmw_sigma(&ds, &path, "bge-m3", "electric cars");
        assert!(sigma >= MIN_SIGMA, "{sigma}");
    }

    #[test]
    fn bge_m3_matches_across_languages() {
        let Some((ds, path)) = trial() else { return };
        if !embeddings_path(&path, "bge-m3").exists() || !ollama_up() {
            return;
        }
        // French for "electric cars"; the article is in English.
        let sigma = bmw_sigma(&ds, &path, "bge-m3", "voitures électriques");
        assert!(sigma >= MIN_SIGMA, "{sigma}");
    }

    #[test]
    fn nomic_fallback_works_with_real_ollama() {
        let Some((ds, path)) = trial() else { return };
        let nomic = embeddings_path(&path, "nomic-embed-text");
        if !nomic.exists() || !ollama_up() {
            return;
        }
        // Only the nomic file next to a copy of the fixture, so load falls back to it.
        let dir = std::env::temp_dir().join(format!("aleth-proto-{}-nomic", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = dir.join("trial.json");
        std::fs::copy(&path, &fixture).unwrap();
        std::fs::copy(&nomic, embeddings_path(&fixture, "nomic-embed-text")).unwrap();
        let sigma = bmw_sigma(&ds, &fixture, "nomic-embed-text", "electric cars");
        assert!(sigma >= MIN_SIGMA, "{sigma}");
    }
}
