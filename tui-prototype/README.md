# TUI design prototype

A throwaway-friendly terminal UI for exploring how the Aleth reader should look, feel, and use AI. It runs on fixture data only and never calls the backend (its only network use is a local Ollama for search and summaries), so it can change freely while the backend work continues.

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
| `data/fake.json` | 15 hand-written feeds in 8 folders plus one unfiled feed, 35 articles: four cross-feed stories, failing feeds (`http_status` 503 and 429), a dead feed (`dns`), title-only and excerpt-only articles, one long article with headings, a list, code and a quote (the only one long enough to summarize), starred and read state. | yes |
| `data/trial.json` | Read-only snapshot of the polling trial: up to 40 articles per feed, content capped at 12 KB. | no (real publisher content) |
| `data/trial.<model>.embeddings.json` | One vector per trial article for semantic search, per model (`bge-m3`, `nomic-embed-text`; see below). | no (generated) |
| `data/<fixture>.<model>.summaries.json` | AI summaries written so far, so each article is summarized once. Delete it to start over. | no (generated) |

Refresh the trial snapshot while the backend is running:

```sh
python3 -I scripts/snapshot_trial.py
```

It only makes GET requests, so the trial is not disturbed. Folders come from `~/my-feeds.opml` until the backend stores folders. The fixture format matches the API's `/v1/feeds` and `/v1/items/{id}` responses, plus `folder` per feed and an optional `state: {read, starred}`. Without saved state, articles older than 24 hours count as read. Times are shown relative to the newest observation in the file, so old snapshots still look live.

Read, star, and caught-up state last only for the session.

## What is real and what is mocked

| Area | Status |
|---|---|
| Layout, navigation, filtering, search, read/star | Real behaviour on fixture data |
| Article rendering | Real: headings, bold/italic, links, code, quotes and lists are styled; lines stop at about 75 characters and are centred when zoomed; reading time, progress and a scrollbar |
| Feed health view (timers, failure codes, dead feeds) | Real fields from the API |
| Internals panel (`i`): item/version IDs, timestamps | Real fields from the API |
| Open in browser (`o`) | Real (`xdg-open`) |
| Refresh (`r`) | Mock: shows the request it would send |
| Search (`/`) | Real: whole-word matching with a **hand-written topic table**, plus **semantic search** with a local Ollama model when an embeddings file exists (see below) |
| AI summary at the end of each article | **Real**: a local LLM through Ollama, every point citing paragraphs `[¶n]` and checked against them (see below) |
| Related articles at the end of each article | **Real**: nearest articles by meaning, from the embeddings |
| Briefing view | Articles first seen since a checkpoint. Stories are **real** groupings: shared links, shared title words, or close meaning (with embeddings). What it says about each article is the article's AI summary when one exists, otherwise its lead sentence (**mock**). |

Everything mocked is labelled `MOCK` on screen.

## AI summaries

Opening an article (`⏎`) asks a local model for a summary. It is written on a background thread while you read and appears **after the article text**, with a line under the title saying it is there (`G` jumps to it). Previews are not summarized, so moving through the list costs nothing.

```sh
ollama pull qwen2.5:3b                         # the default, about 1.9 GB
ALETH_SUMMARY_MODEL=llama3.2:3b cargo run      # any other Ollama chat model
```

- The model gets the title and the paragraphs numbered as the reader shows them, and writes 2 to 4 one-sentence points, each ending with the paragraphs it is based on: `[¶3, ¶5]`. Paragraph numbers are the same at every terminal width, so citations always point at the right text.
- **Citations are checked.** A point whose content words mostly do not appear in the paragraphs it cites is flagged `⚠` with a note to check it; citations outside the article are dropped and the point shows `[no source]`. On the fake long article the model cited "Upgrading is a single command:" for the command itself, which is in the next paragraph; the check flagged it.
- Articles under 80 words are not summarized ("not needed"); title-only articles say there is nothing to summarize. Above 3,000 words the model reads the start and the summary says how much.
- The header names the model, time taken and the input: *full text* when the feed sends the article's content, *the feed's summary* when it sends only that (a teaser or the whole post; the word count tells which).
- Speed on this laptop's CPU: 16 to 20 s per article. Opening another article abandons the one being written (Ollama stops generating). Finished summaries are cached in `data/<fixture>.<model>.summaries.json` and redone when the article gets a new version.
- `A` writes it again (also after an error, e.g. Ollama not running or the model not pulled; the message says which). `a` hides the AI parts.

## Related articles

Below the summary, up to five articles closest in meaning, with their similarity. `]` and `[` choose one, `⏎` opens it. Needs the embeddings file; the bar is a similarity of 0.62 for bge-m3 and 0.76 for nomic, which score on different scales.

## Briefing stories

New articles from different feeds form one story when they share a link, enough title words, or (with embeddings) their meaning is close to the story's first report: at least 0.78 on its own, or at least 0.70 with one title word in common (bge-m3; nomic 0.88 and 0.84). The extra condition is needed because bge-m3 scores long announcement posts alike whatever they announce: "Announcing Rust 1.99.0" and a bird feeder deal score 0.76. Stories grouped partly by meaning are marked `≈`. On the trial's last 12 hours this adds four correct stories that share no title wording, such as "Palmer Luckey and Ben Lamm made a genetically modified…" and "Warhound raises $200M to engineer war dogs".

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

A fixed minimum similarity does not filter nonsense queries out: the best match for "xqzvbn" scores 0.444, while the second real match for "kimi" (a Kimi article) scores 0.427. So there is no floor, only the per-query cutoff below.

Raw similarity is not comparable between queries (the best "ai" match scores 0.80, the best "kimi" match 0.64), so an article matches when it scores at least 3 standard deviations above the mean for that query (`MIN_SIGMA` in `src/semantic.rs`, tuned by hand on the trial snapshot). Weak spots: a query always returns its few closest articles, even when none is relevant ("xqzvbn" matches one article); loose matches slip in (with bge-m3, "space" also finds "Memory is slow, Disk is fast"); and queries in scripts far from the articles score lower: Hindi "अंतरिक्ष" (space) ranks the right articles first but at 2.9σ, just under the cutoff.

Tests: `cargo test` runs unit tests offline (a fake Ollama checks requests, errors, streaming and the background workers) and integration tests against the real trial files and a running Ollama with `bge-m3` and `qwen2.5:3b`; those print `SKIP` when something is missing. One integration test summarizes the fake long article with the real model and checks that most points are supported by what they cite (about 16 s).

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

6. Summary shown by default, on demand, or only for long articles? (Now: written when an article is opened, shown at its end, skipped under 80 words.)
7. Is the per-paragraph citation `[¶n]` enough evidence, or should claims quote the source text? (Now: citations are checked by word overlap and weak ones flagged `⚠`.)
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
| `src/article.rs` | Article HTML to styled paragraphs, wrapped at any width with stable numbering |
| `src/semantic.rs` | Semantic search and related articles: loads article vectors, embeds queries with Ollama |
| `src/summary.rs` | AI summaries: prompt, streaming from Ollama, citation parsing and checking, cache |
| `src/mock_ai.rs` | Briefing: story grouping, and lead sentences where no summary exists |
