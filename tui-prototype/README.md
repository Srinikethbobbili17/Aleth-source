# TUI design prototype

A throwaway-friendly terminal UI for exploring how the Aleth reader should look, feel, and use AI. It runs on fixture data only and never calls the backend (its only network use is a local Ollama for search), so it can change freely while the backend work continues.

```sh
cd tui-prototype
cargo run              # trial snapshot if data/trial.json exists, otherwise fake data
cargo run -- --fake    # fake data: includes a failing feed and a dead feed
cargo run -- path.json # any fixture file
```

Press `?` inside the app for keys. Navigation is vim-style (`h j k l`), with arrow keys as alternatives.

## Data

| File | What it is | In Git |
|---|---|---|
| `data/fake.json` | 15 hand-written feeds in 8 folders plus one unfiled feed, 34 articles: four cross-feed stories, failing feeds (`http_status` 503 and 429), a dead feed (`dns`), title-only and excerpt-only articles, starred and read state. | yes |
| `data/trial.json` | Read-only snapshot of the polling trial: up to 40 articles per feed, content capped at 12 KB. | no (real publisher content) |
| `data/trial.<model>.embeddings.json` | One vector per trial article for semantic search, per model (`bge-m3`, `nomic-embed-text`; see below). | no (generated) |

Refresh the trial snapshot while the backend is running:

```sh
python3 -I scripts/snapshot_trial.py
```

It only makes GET requests, so the trial is not disturbed. Folders come from `~/my-feeds.opml` until the backend stores folders. The fixture format matches the API's `/v1/feeds` and `/v1/items/{id}` responses, plus `folder` per feed and an optional `state: {read, starred}`. Without saved state, articles older than 24 hours count as read. Times are shown relative to the newest observation in the file, so old snapshots still look live.

Read, star, and caught-up state last only for the session.

## What is real and what is mocked

| Area | Status |
|---|---|
| Layout, navigation, filtering, search, read/star, article rendering | Real behaviour on fixture data |
| Feed health view (timers, failure codes, dead feeds) | Real fields from the API |
| Internals panel (`i`): item/version IDs, timestamps | Real fields from the API |
| Open in browser (`o`) | Real (`xdg-open`) |
| Refresh (`r`) | Mock: shows the request it would send |
| Search (`/`) | Real: whole-word matching with a **hand-written topic table**, plus **semantic search** with a local Ollama model when an embeddings file exists (see below) |
| AI summary in the article view (`a`) | **Mock**: lead sentences of up to three paragraphs, each citing its paragraph `[¶n]`; shows input completeness (full text, excerpt, title only) |
| Briefing view | **Mock**: articles first seen since a checkpoint; titles that share words or links across feeds are grouped into "stories", the rest are grouped by folder. Claims cite numbered sources. |

Everything mocked is labelled `MOCK` on screen.

## Search

`/` searches by topic, not by exact title text:

- Whole words only, so "ai" no longer matches "maint**ai**ner" or "r**ai**lway".
- Looks in the title, article text, authors, feed name and folder.
- Known topics widen to related terms: "ai" also finds "LLM", "machine learning", "OpenAI", "Anthropic" and so on. Topics so far: AI, tech, security, space, climate, programming, Rust, science (`TOPICS` in `src/search.rs`).
- Every word of the query must match. "ai tech" finds AI articles that are also about tech.
- A broad topic needs two mentions in the article text; a passing mention does not count. A specific word ("kimi") needs one.
- Articles close in **meaning** match too, even with no shared words: "electric cars" finds GM, Tesla and robotaxi stories (1 keyword match, 7 by meaning on the trial snapshot). Needs 3+ characters.
- Results are newest first. Ranking by importance comes later.

Each result shows why it matched: `T` title, `F` feed/folder/author, `B` article text, `~` related terms only, `≈` meaning only. The article view lists the reasons, including the similarity score.

### Semantic search

Uses [Ollama](https://ollama.com) on `127.0.0.1:11434` (CPU is fine). Two models are supported; the app uses the first one whose vectors exist:

| Model | Languages | Query time (CPU) | Embedding the 556 trial articles |
|---|---|---|---|
| `bge-m3` (preferred) | 100+; a query in one language finds articles in another ("voitures électriques" finds English EV stories) | ~130 ms | 335 s |
| `nomic-embed-text` | English | ~25 ms | faster |

```sh
ollama pull bge-m3
python3 -I scripts/embed_trial.py --model bge-m3   # writes data/trial.bge-m3.embeddings.json
```

Article vectors are computed once. Queries are embedded on a background thread as you type, so keyword results appear at once and matches by meaning join when Ollama answers (the list title shows "matching meaning…" meanwhile). Only the latest query is embedded; ones typed past are skipped. The first query loads the model, which can take a few seconds.

Rerun the script after taking a new snapshot; articles missing from the file only get keyword matching. Without the file, or when Ollama is not running, search falls back to keywords and the status line says so.

Raw similarity is not comparable between queries (the best "ai" match scores 0.80, the best "kimi" match 0.64), so an article matches when it scores at least 3 standard deviations above the mean for that query (`MIN_SIGMA` in `src/semantic.rs`, tuned by hand on the trial snapshot). Weak spots: a query always returns its few closest articles, even when none is relevant ("xqzvbn" matches one article); loose matches slip in (with bge-m3, "space" also finds "Memory is slow, Disk is fast"); and queries in scripts far from the articles score lower: Hindi "अंतरिक्ष" (space) ranks the right articles first but at 2.9σ, just under the cutoff.

Tests: `cargo test` runs unit tests offline (a fake Ollama checks requests, errors and the background worker) and integration tests against the real trial files and a running Ollama; those print `SKIP` when either is missing.

The topic table and this file-based index only demonstrate the behaviour. The real version belongs in the backend: full-text search plus stored embeddings, so any topic works without a hand-written list.

## Pages

```
 1 Home   2 Briefing   3 Feeds                                   aleth prototype · trial · 190 unread
                                       Folders · 4
                                      ┏ Programming ━━━━━━━━━━━ 22 ┓╭ Tech News ───────────── 75 ╮
   █████  ██      ███████ ████████    ┃ 22 unread · 98 articles    ┃│ 75 unread · 182 articles   │
  ██   ██ ██      ██         ██       ┃ 5 feeds · ! 1 failing      ┃│ 5 feeds · ● healthy        │
  ███████ ██      █████      ██       ┃                            ┃│                            │
  ██   ██ ██      ██         ██       ┃ ● 15m Attention people w…  ┃│ ●  6m VCs, Superhuman Ma…  │
  ██   ██ ███████ ███████    ██       ┃ ● 19m Memory is slow, Di…  ┃│ ●  1h TechCrunch Founder…  │
                                      ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛╰────────────────────────────╯
  read what changed                   ╭ AI & Research ───────── 46 ╮
  190 unread of 556 articles          │ …                          │
```

- **Home** (start page): logo, reading stats and feed health on the left; one tile per folder on the right with unread count, feed health and the latest headlines. Feeds without a folder appear under *Unfiled*. `h j k l` or the arrows move between tiles, `⏎` opens one, `a` opens all articles, `s` starred, `/` searches everything.
- **Folder page** (`1 Home › Security`): the inbox limited to that folder's feeds. Three panes: feeds, article list, article. Below 120 columns the article replaces the list when opened; `z` zooms it to full screen. Feed badges: `!` failing, `✝` dead.
- **Briefing** and **Feeds** tabs as before. `⏎` on a feed opens its folder page with that feed selected.

`Esc` steps back: article → list → Home. `Backspace` goes straight to Home. Below 110 columns the logo shrinks to one line and the tile grid scrolls.

## Questions this prototype is meant to answer

Capabilities:

1. Three panes, or a single list that opens articles full screen (newsboat style)? Should folder tiles show headlines, or just counts so more folders fit?
2. One mixed timeline, or per-folder and per-feed lists first?
3. Should opening an article mark it read immediately, after scrolling, or only with an explicit key?
4. Are the internals (feed timers, versions, error codes) useful day to day, or only in a debug view?
5. Which actions are missing: save for later, tags, notes, share, subscribe from inside the TUI?

AI:

6. Summary shown by default, on demand, or only for long articles?
7. Is the per-paragraph citation `[¶n]` enough evidence, or should claims quote the source text?
8. Briefing as the home screen, or a separate tab?
9. What should happen for title-only feeds: skip, fetch the full article, or summarize the title?
10. Which other AI features to try: ask questions about an article, explain why it is ranked, translate, digest by email, local model only?

## Code

| File | Contents |
|---|---|
| `src/data.rs` | Fixture types (mirror the API) and loading |
| `src/app.rs` | State, pages and key handling |
| `src/home.rs` | Home page: logo and folder tiles |
| `src/ui.rs` | Rendering with ratatui |
| `src/semantic.rs` | Semantic search: loads article vectors, embeds queries with Ollama |
| `src/mock_ai.rs` | Fake summaries and story grouping |
