# Next Steps Checklist

Updated: 2026-10-22  
Tick items off as they land. Background and reasoning are in [backend-progress-and-next-steps.md](backend-progress-and-next-steps.md).

**Plan:** finish a complete, healthy, shippable backend first. Only then build a TUI on top of its API, to explore and understand the system. No CLI, TUI, or web client work happens before the backend meets the ship-ready definition below.

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

- [ ] **Add CI (GitHub Actions).** Run `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`, and the ignored PostgreSQL tests against a Postgres service container on every push.
- [ ] **Log database error kinds** instead of the generic "database operation failed" (`ingest.rs` `From<sqlx::Error>`, `api.rs` `ApiError::database`).
- [ ] **Large podcast feeds.** A 19.6 MiB / 3,013-episode podcast still fails (`too_large`, and it exceeds the 1,000-entry limit). Decide: keep the newest N entries instead of failing, and make limits configurable.
- [ ] **Dead-feed handling.** Use `last_error_code` to stop retrying permanent failures (`dns_failed`, `not_a_feed`, HTTP 404/410) after repeated attempts.
- [ ] **YouTube descriptions** (`media:description`) and a date fallback to `<updated>` when `published` is missing.
- [ ] **Add a `*/*` fallback to the `Accept` header** so strict publishers do not reject requests.
- [ ] **Build a real-feed corpus.** Register the 10–20 feeds you actually read, refresh them all, and note every failure.

## Phase 2: Automatic polling

- [ ] Migration that splits `next_fetch_at` into `next_poll_at`, `manual_refresh_after`, and `retry_not_before`
- [ ] Refactor `ingest.rs` so a caller that already claimed a feed can run the refresh with that claim
- [ ] `scheduler.rs`: claim due feeds in batches with `FOR UPDATE SKIP LOCKED`
- [ ] Worker loop with a polling interval plus jitter, clean shutdown, and recovery of expired leases after restart
- [ ] Tests: restart recovery, two workers never persisting the same lease, a slow feed not blocking others
- [ ] Run unattended for a day against the feed corpus and record failures and lag

## Phase 3: Ingestion correctness and efficiency

- [ ] Store the redirected URL after permanent (301/308) redirects
- [ ] Stop `<updated>`-only changes from creating a new article version on each fetch
- [ ] Batch the per-article SQL inside the refresh transaction
- [ ] Article version-history endpoint
- [ ] Retention policy for old versions and items

## Phase 4: Accounts and access control

- [ ] Accounts and authentication (API tokens to start; suits a TUI)
- [ ] `subscriptions` table linking accounts to shared feeds; per-account feed list
- [ ] Authorization on every route, with cross-account tests

## Phase 5: Reader features in the API

- [ ] Read/unread, starred, and saved state per account, kept separate from article content
- [ ] Folders, subscription rename/remove
- [ ] OPML import (with per-feed errors) and export
- [ ] PostgreSQL full-text search over accessible articles
- [ ] Change feed or sync cursor so clients can fetch "what changed since X"

## Phase 6: API polish and operations

- [ ] Uniform JSON error codes, request IDs, OpenAPI document
- [ ] Configurable limits with safe defaults
- [ ] Release container image (non-root), deployment config, migration step
- [ ] Metrics: due-feed lag, fetch errors, refresh duration, DB pool usage
- [ ] Automated backups with a tested restore

## After the backend ships: TUI

- [ ] TUI client over the HTTP API: feed list with health, inbox, article view, read/star actions, refresh controls
- [ ] Views that expose system internals for learning: lease state, retry times, article versions

## Much later

- [ ] Optional, source-cited AI summaries, then story grouping and the "what changed?" briefing
- [ ] Web client
