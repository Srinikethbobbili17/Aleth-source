# Next Steps Checklist

Updated: 2026-10-22  
Tick items off as they land. Background and reasoning are in [backend-progress-and-next-steps.md](backend-progress-and-next-steps.md).

**Plan:** finish a complete, healthy, shippable backend first. Only then build a TUI on top of its API, to explore and understand the system. No client is connected to the backend before it meets the ship-ready definition below. A standalone TUI design prototype on fixture data runs in parallel (see below).

## Done

- [x] Manual ingestion backend: register, refresh, store versioned articles, read them through the API
- [x] Fix: DNS64 networks blocked most feeds (non-public DNS answers are now dropped, not fatal)
- [x] Fix: newest articles listed last (entries are now inserted oldest first)
- [x] Project under Git and pushed to GitHub; a fresh clone builds and passes all 16 tests
- [x] Native host build works (gcc installed)
- [x] Distinct failure codes: every fetch/parse failure has its own code (`feeds.last_error_code`, API `code` field) and message
- [x] Random timeouts fixed: 10 s connect budget (slow DNS on this network took 5 s), two addresses per family, one connection retry
- [x] Gzip support with a 16 MiB decompressed cap; XML event limit scaled to the size limit
- [x] Docker packaging: multi-stage `Dockerfile` (distroless, non-root, amd64/arm64), backend service in `compose.yaml`, built-in `healthcheck` command. Verified after a reboot: 36 MB image builds in about 1 minute; `compose up` brings both services to healthy; real feeds, error codes, clean shutdown, and data persistence across `down`/`up` all work.

## Ship-ready backend: definition of done

The backend is ready for a TUI when all of these hold:

- Feeds refresh automatically, survive restarts, and recover from failing publishers without manual action.
- Every API request is authenticated and scoped to its account; one account cannot see another's data.
- A reader's full workflow works through the API: subscribe, import OPML, read, mark read, star, search, export.
- Errors are consistent JSON with stable codes; the API has an OpenAPI description.
- CI runs formatting, lint, unit, and PostgreSQL tests on every push.
- It ships as a container image with documented config, migrations, backups, and health/metrics endpoints.

## Phase 1: Harden ingestion (now)

- [ ] **Test the image on a second device**, ideally arm64 (Apple Silicon Mac or Raspberry Pi), since only amd64 has been built so far.

- [x] **CI (GitHub Actions)** in `.github/workflows/backend.yml`: formatting, Clippy, unit/HTTP tests, PostgreSQL tests against a service container, and a Docker build + Compose health smoke test on every push and pull request.
- [x] **B1:** database errors logged by kind, SQLSTATE, and constraint.
- [x] **B2:** `parser_busy` separated from `refresh_busy`; a downloaded feed waits up to 10 s for a parser slot instead of being discarded at once.
- [x] **B3:** every error is JSON with a `code`, including malformed requests, unknown routes, and wrong methods.
- [x] **B6:** HTML detection skips XML declarations and comments.
- [x] **B7:** publication date falls back to the entry's updated date.
- [x] **Big feeds:** keep the latest 1,000 entries and report `skipped` (verified: arXiv CS 1,252 → 1,000; a 2,768-episode podcast → newest 1,000).
- [x] **Dead feeds:** `dead_at` set after 10 consecutive failures; a successful manual refresh revives the feed.
- [ ] **Very large feeds over 16 MiB** (for example The Daily, 19.6 MiB) still fail with `too_large`. Decide whether to raise the cap or make limits configurable.
- [ ] **YouTube descriptions** (`media:description`).
- [x] **`*/*` fallback in the `Accept` header** so strict publishers do not answer 406.
- [ ] **Build a real-feed corpus.** Register the 10–20 feeds you actually read, refresh them all, and note every failure.

## Phase 2: Automatic polling

- [x] Migration 0005 splits `next_fetch_at` into `next_poll_at`, `manual_refresh_after`, and `retry_not_before`; distinct `cooling_down` and `retry_backoff` errors
- [x] `ingest.rs` refactored into claim + run around a shared `Refresher`, used by the API and the scheduler
- [x] `scheduler.rs`: claims due feeds in batches with `FOR UPDATE SKIP LOCKED`, skips dead and backing-off feeds
- [x] Polling interval (`POLL_INTERVAL_SECONDS`, default 30 min) with ±10% jitter, one slot reserved for manual refreshes, clean shutdown with a 30 s drain, expired-lease recovery
- [x] Tests: concurrent claimers never share a feed, expired leases are reclaimed, a slow feed does not block others, shutdown drains in-flight work
- [x] CI warning fixed: `actions/checkout@v7` (Node.js 24)
- [ ] Run unattended for a day against the feed corpus and record failures and lag. **Attempt 1 (2026-10-22/24) was invalid:** the laptop suspended after about 1 hour (lid closed 20:58 IST, opened 02:32 IST two days later), so only one regular poll round and one catch-up burst ran. Findings below. Rerun on an always-on machine or with sleep inhibited.
- [ ] Trial finding: a failure pushes the next poll a full interval out (`next_poll_at = max(backoff, interval)` in `ingest.rs`). Three feeds hit `dns_failed` in the first second after resume, before the network was up, and then waited 30 min instead of retrying after the 30 s backoff. Retry after the backoff, capped at the interval.
- [ ] Trial finding: articles were lost during the gap. On resume, Hacker News, TechCrunch, Ars Technica, The Verge and NPR returned only new entries (zero already seen), so items published while the machine slept fell out of their feeds unseen. Record a gap per poll (no overlap with stored items) so it is visible; accept it as a limit of RSS for fast feeds.
- [ ] Trial finding: after the catch-up burst all 19 feeds are due within a 5-minute window; ±10% jitter never spreads them out again. Consider spreading polls after a burst.
- [ ] Publication-aware intervals: poll frequently updated feeds more often, quiet ones less
- [ ] Metrics for due-feed lag (how late polls run) and scheduler throughput

## Phase 3: Ingestion correctness and efficiency

- [x] Store the redirected URL after permanent (301/308) redirects
- [x] Stop `<updated>`-only changes from creating a new article version (legacy fingerprints upgrade in place, verified on the trial's 2,690 articles)
- [x] Batch the per-article SQL inside the refresh transaction (1,000-entry save: 3.8 s → 1.05 s in the test suite)
- [ ] Trial finding confirmed: hnrss.org summaries embed live points/comment counts. 21 Hacker News items hold 57 versions (19 of 20 items gained a version in a single poll). Other feeds' extra versions (Nature, BBC, The Verge, This Week in Rust) are genuine title/content edits.
- [ ] Trial finding: a changed publication date alone creates a version (BBC, OpenAI News). Decide whether that counts as a meaningful change.
- [ ] Trial finding: OpenAI News (1,313 entries) and Hugging Face (~900) never answer 304, so every poll downloads and re-hashes ~1,000 entries. Low priority at 30 min intervals.
- [ ] Article version-history endpoint
- [ ] Retention policy for old versions and items

## Phase 4: Accounts and access control

- [ ] Accounts and authentication (API tokens to start; suits a TUI)
- [ ] `subscriptions` table linking accounts to shared feeds; per-account feed list
- [ ] Authorization on every route, with cross-account tests

## Phase 5: Reader features in the API

- [ ] Read/unread, starred, and saved state per account, kept separate from article content
- [ ] Folders, subscription rename/remove
- [x] OPML import: `POST /v1/imports/opml` with per-feed status, folders, duplicates, and rejected URLs; imported feeds are fetched by the scheduler within seconds
- [x] OPML export: `GET /v1/exports/opml`; round-trips through import
- [ ] Store OPML folders once folders exist
- [ ] PostgreSQL full-text search over accessible articles. Requirement from the TUI prototype: search by topic, not exact title text. Match whole words in title, text, feed and folder; a topic query ("ai") must also find related terms (LLM, OpenAI, …), via embeddings or a synonym layer; newest first for now, importance ranking later.
- [ ] Change feed or sync cursor so clients can fetch "what changed since X"

## Phase 6: API polish and operations

- [ ] Uniform JSON error codes, request IDs, OpenAPI document
- [ ] Configurable limits with safe defaults
- [ ] Release container image (non-root), deployment config, migration step
- [ ] Metrics: due-feed lag, fetch errors, refresh duration, DB pool usage
- [ ] Automated backups with a tested restore

## In parallel: TUI design prototype

Separate folder, [`tui-prototype/`](../tui-prototype/README.md). It runs on fake data or a read-only trial snapshot and never calls the backend. Goal: settle layout, workflow, and AI preferences before the real client is built.

- [x] Prototype: three-pane inbox, briefing, feed health view, mock AI summaries and story grouping, trial snapshot script
- [x] Topic search: whole-word matching over title, text, feed and folder, with a hand-written topic table standing in for semantic search; newest first
- [x] Semantic search with local Ollama: multilingual `bge-m3` preferred, `nomic-embed-text` fallback; precomputed article vectors, queries embedded on a background thread, per-query 3σ cutoff, added to keyword matches; 32 unit + integration tests
- [ ] Cross-script queries (Hindi) score just under the 3σ cutoff; decide on a per-language cutoff or a lower one
- [x] Multi-page layout: Home page with logo and folder tiles; each tile opens a folder page; Esc/Backspace go back
- [ ] Search ranking: put the most important recent articles on top (user preference; needs an importance signal)
- [ ] Use it on the trial snapshot and answer the open questions in its README
- [ ] Record decisions here; turn them into API requirements for Phases 4–5

## After the backend ships: TUI

- [ ] TUI client over the HTTP API: feed list with health, inbox, article view, read/star actions, refresh controls
- [ ] Views that expose system internals for learning: lease state, retry times, article versions

## Much later

- [ ] Optional, source-cited AI summaries, then story grouping and the "what changed?" briefing
- [ ] Web client
