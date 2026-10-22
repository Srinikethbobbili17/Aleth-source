# Understanding the RSS Backend: Progress, Architecture, and Next Steps

Snapshot date: 2026-10-21  
Audience: You, as the person building and learning this project.  
Scope: What exists in the workspace, how it works, what has been verified, and what to build next.

## 1. Where the project stands

We have built a **working manual feed-ingestion backend in Rust**. You can register a public feed, request a refresh, store its articles in PostgreSQL, and retrieve those articles through an HTTP API. Repeated unchanged articles do not create new records, and changed articles can retain their earlier versions.

The backend is currently a local, single-user development application. It does not yet refresh feeds automatically, identify users, track read/unread state, synchronize devices, or run AI models. Those are planned features, not hidden capabilities of the existing code.

The recommended next milestone is **reliable automatic polling**, supported by repeatable builds and tests. Account isolation and reader state should follow before a hosted pilot.

Use these three documents for different purposes:

| Document | What it helps you do |
| --- | --- |
| [Original platform proposal](rss-platform-design.md) | Understand the larger product idea, competitor research, AI opportunities, Rust assessment, and original delivery assumptions. It is a proposal, not an implementation checklist already completed. |
| This guide | Understand the current implementation and choose the next engineering work. |
| [Backend README](../backend/README.md) | Run the backend, call its endpoints, and execute its tests. |

All executable code, SQL migrations, fixtures, and development configuration live in `NEW RSS/backend/`. The `docs/` directory contains written documentation only.

## 2. What we have done so far

### Product and architecture work

We created the original design document, researched existing RSS products, and identified useful directions such as source-linked summaries, story grouping, and briefings about developments since the reader last caught up. We assessed Rust and chose a Rust backend with PostgreSQL, while leaving the first client and AI execution as separate concerns.

We then narrowed the implementation to a complete ingestion path. This gives later client and AI features real articles, stable database IDs, and versioned content to work with.

### Implemented capabilities

| Area | What exists today | Practical meaning |
| --- | --- | --- |
| Application foundation | Rust package, async runtime, API server, configuration, JSON logs, shutdown handling | There is a runnable backend application. |
| Database setup | PostgreSQL connection pool and two embedded migrations | Startup creates or upgrades the application tables. |
| Feed registration | URL normalization and duplicate-safe registration | Registering the same normalized URL returns the existing feed. |
| Feed inspection | Feed listing, individual feed details, and health fields | You can see when fetching succeeded or failed. |
| Preview parsing | RSS, Atom, and JSON Feed previews from supplied content | You can inspect a feed without saving its articles. |
| Remote fetching | Manual refresh with bounded HTTP requests and destination checks | The backend can retrieve eligible public feeds. |
| Conditional fetching | ETag and Last-Modified support | A publisher can say its feed has not changed without resending the body. |
| Article storage | Feed-scoped identities, normalized content, and versions | Repeated delivery is handled separately from content changes. |
| Article API | Paginated metadata and individual article detail | A future client has a basic reading API. |
| Refresh coordination | Database leases, unique ownership tokens, cooldowns, and failure backoff | Overlapping refreshes and stale results are controlled. |
| Verification | Unit, HTTP, and PostgreSQL tests; formatting and Clippy checks | Important behavior has automated regression coverage. |
| Development environment | PostgreSQL Compose file and Rust development container | The project can be built without installing every build dependency on the host. |

### Not implemented yet

Automatic polling; a background worker; a general job queue; accounts and authentication; per-user subscriptions; folders; read/star/save state; OPML import/export; search; offline synchronization; article-page extraction; version-history endpoints; embeddings; summaries; model-provider integrations; billing; production deployment automation; monitoring dashboards; backup/restore automation.

Keeping this list explicit prevents the larger design proposal from being mistaken for delivered functionality.

## 3. The concepts to understand first

| Term | Meaning in this project |
| --- | --- |
| Feed | A publisher's RSS, Atom, or JSON document containing entries. |
| Item/article | One entry tracked by our database. It may contain a full article, an excerpt, or little more than metadata. |
| Ingestion | Fetching a feed, parsing it, deciding what is new or changed, and storing the result. |
| Normalization | Converting different feed formats into the same internal fields. |
| Migration | A numbered SQL change applied to the database schema. |
| Transaction | A group of database changes that either commits together or rolls back together. |
| Idempotency | Repeating an operation without creating unintended extra effects, such as duplicate articles. |
| Content hash | A deterministic fingerprint used to compare normalized article snapshots. |
| Version | A stored snapshot of an article's content and metadata. |
| Lease | Temporary ownership of a refresh, recorded in the database. |
| Ownership token | A unique identifier checked before accepting a refresh result. It prevents an older operation from overwriting a newer one. |
| Backoff | Waiting longer after failures before trying again. |
| Pagination cursor | A value used to request the next portion of a list. Our current cursors are item or feed IDs. |

## 4. How the application is organized

The current application is one Rust program with modules for different responsibilities. It does not need separate network services for parsing, storage, and the API at this stage.

```mermaid
flowchart LR
    Caller[Local API caller or future client] --> API[Axum API]
    API --> Feeds[Feed registration and queries]
    API --> Ingest[Refresh coordination]
    Ingest --> Fetch[HTTP fetcher]
    Fetch --> Publisher[Public feed publisher]
    Ingest --> Parse[Parse and normalize articles]
    Parse --> Commit[Atomic persistence]
    Feeds --> DB[(PostgreSQL)]
    Commit --> DB
    API --> Items[Article queries]
    Items --> DB
```

### File-by-file reading map

Paths below are relative to this document and link to the implementation.

| File | Responsibility | What to look for |
| --- | --- | --- |
| [Cargo.toml](../backend/Cargo.toml) | Package and dependency declarations | Which libraries are used and which optional features are enabled. |
| [Cargo.lock](../backend/Cargo.lock) | Resolved dependency versions | Reproducible application builds; commit this with the project. |
| [src/main.rs](../backend/src/main.rs) | Startup and shutdown | Load configuration → connect to PostgreSQL → migrate → start HTTP server. |
| [src/config.rs](../backend/src/config.rs) | Environment configuration | Database URL and listening address. |
| [src/db.rs](../backend/src/db.rs) | Connection pool and migrations | Pool limits and embedded migration execution. |
| [src/api.rs](../backend/src/api.rs) | HTTP routes and input/output handling | Request validation, concurrency controls, and status codes. |
| [src/feeds.rs](../backend/src/feeds.rs) | Feed registration, health queries, and previews | URL normalization and shared input checks. |
| [src/fetch.rs](../backend/src/fetch.rs) | HTTP retrieval | DNS/address validation, redirects, validators, timeouts, and body limits. |
| [src/items.rs](../backend/src/items.rs) | Article normalization and reading queries | Identity selection, hashing, sanitization, and pagination. |
| [src/ingest.rs](../backend/src/ingest.rs) | Refresh orchestration | Lease ownership, failure handling, versions, and transaction boundaries. |
| [src/lib.rs](../backend/src/lib.rs) | Module exports | How application code is shared with tests. |
| [First migration](../backend/migrations/0001_feeds.sql) | Initial feed table | Feed identity, scheduling fields, and validators. |
| [Second migration](../backend/migrations/0002_ingestion.sql) | Ingestion schema | Health/lease fields, items, and item versions. |
| [build.rs](../backend/build.rs) | Build dependency tracking | SQL changes cause embedded migrations to be rebuilt. |
| [tests/api.rs](../backend/tests/api.rs) | HTTP behavior tests | Validation, preview responses, liveness, and readiness. |
| [tests/database.rs](../backend/tests/database.rs) | Database-backed registration test | Duplicate registration and feed pagination. |
| [Fixture directory](../backend/tests/fixtures) | Small example feed documents | Starting samples for RSS, Atom, and JSON Feed. |
| [compose.yaml](../backend/compose.yaml) | Local PostgreSQL container | Local port, development credentials, health check, and persistent volume. |
| [Containerfile.dev](../backend/Containerfile.dev) | Development build image | Rust, rustfmt, and Clippy; this is not a production runtime image. |

Additional tests live inside `feeds.rs`, `fetch.rs`, `items.rs`, and `ingest.rs`, under `#[cfg(test)]`. Looking only in `tests/` would miss most of the ingestion coverage.

### What the main libraries do

Tokio provides asynchronous execution. Axum receives HTTP requests. Reqwest fetches publisher feeds. SQLx executes SQL against PostgreSQL. Serde converts Rust data to and from JSON. feed-rs understands the feed formats, while quick-xml performs an additional XML structure check. SHA-256 hashes support identity fallbacks and content comparison. Tracing produces structured logs.

The parser's optional HTML sanitization dependency is enabled explicitly. Setting a sanitization option in application code without enabling the corresponding library feature was insufficient; this was corrected before storing fetched article content.

## 5. Follow one refresh from beginning to end

Imagine you register a real feed and receive database ID `42`.

1. **Registration stores the URL.** `POST /v1/feeds` normalizes the URL and creates or returns a feed row. It does not fetch the publisher immediately.
2. **You request a refresh.** `POST /v1/feeds/42/refresh` checks the ID and available refresh capacity.
3. **The backend claims the feed.** It sets a unique token and a lease lasting 90 seconds, provided the feed is eligible and no live lease already owns it.
4. **The fetcher checks the destination.** It validates the URL, port, literal IP or DNS results, and every redirect destination.
5. **It asks for the current representation.** If validators were saved for the exact requested URL, it sends conditional headers.
6. **An unchanged response takes a short path.** A valid HTTP 304 updates health and validator state without parsing a new document or creating article versions.
7. **A new body is parsed and normalized.** Input limits are checked, HTML is sanitized, relative links are resolved, and every article receives an identity and content hash.
8. **Persistence checks ownership again.** A database transaction verifies the refresh token and lease before changing content.
9. **Articles are inserted or versioned.** Identical current content stays unchanged; changed content creates a new version. Feed metadata and validators are committed with the articles.
10. **The result is returned.** The lease is cleared, health is updated, and the next permitted refresh is recorded.

Network requests and parsing happen before the persistence transaction. A slow publisher therefore does not hold the article transaction open while the network request runs.

A successful report might look like this illustrative response:

```json
{
  "inserted": 3,
  "updated": 1,
  "unchanged": 8,
  "not_modified": false
}
```

`unchanged` counts entries compared in a downloaded body. A 304 response has no downloaded article set to compare, so it can return zero counts with `not_modified: true`.

### What happens when something fails

Network and parse failures leave the previously stored article content and validators intact, record an error, and calculate a retry time. A failed persistence transaction rolls its content changes back; its lease may remain until expiry. A crash or cancelled operation can also leave a lease until expiry.

The saved retry time does not execute a retry by itself. Another manual request is currently required. This is the main gap the scheduler milestone will close.

## 6. The database model and duplicate handling

There are three application tables today, plus SQLx's migration bookkeeping.

```mermaid
erDiagram
    FEEDS ||--o{ ITEMS : contains
    ITEMS ||--o{ ITEM_VERSIONS : retains
    FEEDS {
        bigint id PK
        text url UK
        timestamp next_fetch_at
        uuid refresh_token
        timestamp refresh_until
    }
    ITEMS {
        bigint id PK
        bigint feed_id FK
        text identity
        bigint current_version_id FK
    }
    ITEM_VERSIONS {
        bigint id PK
        bigint item_id FK
        text content_hash
        text title
        text content
    }
```

The diagram shows selected fields. `items.current_version_id` selects the version returned by the article API. A composite foreign key ensures that this version belongs to the same item. It is deferred until transaction completion so an item and its first version can be created together.

### Identity answers “which article is this?”

The identity preference is:

1. An explicit publisher ID, such as an RSS GUID or Atom ID.
2. A normalized article URL when the publisher supplied no ID.
3. A hash of the normalized content when both ID and URL are missing.

Identity is unique **within a feed**, using the pair `(feed_id, identity)`. Two different feeds using the same publisher ID do not collide. Cross-publisher story deduplication is not implemented.

URL normalization removes fragments and preserves query parameters. Missing publication dates remain null. A publisher repeating an identity within one document uses the first occurrence.

### Versioning answers “what changed?”

Suppose feed 42 publishes GUID `article-100`:

| Fetch | Publisher content | Database result |
| --- | --- | --- |
| First | Title “Rust update”, body A | Create one item and its first version. |
| Second | Same title and body A | Reuse the item and its current version. |
| Third | Same GUID, corrected body B | Keep the item; add a new version and update its current-version pointer. |
| Fourth | Same GUID, body returns to A | Add a new observed version because it differs from the current body B. |

The fingerprint includes normalized metadata as well as content, so a changed author or source timestamp can create a version too. This is snapshot comparison, not a semantic judgment about how important the change is.

There are deliberate limits: changing a publisher ID can create a new item, and an entry without both ID and URL cannot reliably be associated with an older entry after its content changes. Items that disappear from a publisher's rolling feed are retained. There is no retention cleanup policy yet.

Stored content comes from the feed. We do not fetch article pages to turn short excerpts into full articles.

## 7. API capabilities and constraints

| Method and path | Purpose |
| --- | --- |
| `GET /health/live` | Check whether the HTTP process responds. |
| `GET /health/ready` | Check database availability, with a two-second deadline. |
| `POST /v1/feeds` | Register a feed URL; return 200 for both new and duplicate registration. |
| `GET /v1/feeds` | List feeds using `after_id` and `limit`. |
| `GET /v1/feeds/{id}` | Read feed details and health. |
| `POST /v1/feeds/{id}/refresh` | Run a manual refresh and return its report. |
| `POST /v1/feed-preview` | Parse supplied feed text without storing articles. |
| `GET /v1/items` | List article metadata, optionally filtered by feed. |
| `GET /v1/items/{id}` | Read the latest stored version of an article. |

Feed pages move forward through increasing IDs. Article pages use `before_id` and decreasing IDs, with the newest inserted items first. Both default to 50 results and allow 1–100. Article order is ingestion order, not publication-date order. Neither endpoint is a cross-device change stream.

Expected application errors include 404 for a missing resource, 409 for refresh ownership/capacity conflicts, 422 for domain validation, 429 for cooldown, 502 for publisher/network/parse failure, and 503 for unavailable database or overall capacity. Malformed request handling still uses some Axum defaults, so the error shape is not yet fully uniform.

### Current resource limits

| Limit | Current value or behavior |
| --- | --- |
| Concurrent refresh requests | 4 per application instance |
| Requests to one hostname | 2 per shared fetcher instance |
| Concurrent parsing tasks | 2 per application instance |
| Connection timeout | 10 seconds for DNS, TCP, and TLS; at most two addresses per IP family; one retry after 500 ms |
| Per-request HTTP timeout | 15 seconds |
| Whole fetch deadline | 40 seconds, including slot waiting, a connection retry, redirects, and body streaming |
| Redirects | At most 5; HTTPS downgrade redirects rejected |
| Downloaded feed size | At most 16 MiB after gzip decompression, including streamed responses without a declared size |
| Entries in a parsed feed | At most 1,000 |
| XML structure | At most 64 nesting levels and 4,194,304 events (scaled to the size limit); document types rejected |
| Preview request body | At most 2 MiB of JSON containing at most 1 MiB of decoded feed text |
| Refresh lease | 90 seconds |
| Successful manual refresh cooldown | 30 seconds |
| Failure backoff | Starts at 30 seconds, increases to one hour; eligible Retry-After values can extend it up to 24 hours |
| PostgreSQL pool | At most 10 connections; three-second acquisition timeout |

The fetcher requests uncompressed content and rejects compressed responses. It permits HTTP(S) on ports 80 and 443, disables environment proxies, and uses a conservative public-address policy. DNS results are validated and passed to the connector without a second independent lookup. These choices constrain what arbitrary feed URLs can cause the server to contact.

The 40-second deadline covers fetching, not the complete refresh transaction. SQL statements during persistence have individual timeouts; an overall refresh/shutdown budget is still a useful improvement.

Concurrency limits are mostly local to one process. Starting several API or worker instances would multiply their outbound capacity. The database lease still coordinates ownership of the same feed, but different feeds on the same publisher need deployment-wide fairness if workers are scaled out.

## 8. Running it and exploring it yourself

Follow the [backend README](../backend/README.md) for complete native and Podman commands. The basic workflow, when Rust and a Compose provider are available, is:

```bash
cd '/home/sriniketh/source/NEW RSS/backend'
cp -n .env.example .env
docker compose up -d postgres
cargo run --locked
```

The copy command preserves an existing `.env`. Wait for PostgreSQL to become healthy before launching the application. Startup requires `DATABASE_URL`, applies migrations, and listens on `127.0.0.1:3000` by default. `BIND_ADDR` changes that address; `RUST_LOG` controls logging.

The Compose file starts PostgreSQL only. `cargo run` starts the API. The Podman development image can replace the host Rust build environment; it is not a packaged production application. Build outputs and `.env` are ignored by the backend's Git ignore rules, and PostgreSQL data persists in its configured volume.

Try this learning exercise:

1. Check both health endpoints.
2. Register a real public feed you use, then register the identical URL again. Compare returned IDs.
3. Refresh the returned feed ID and inspect its report.
4. List the stored items and open one detail response.
5. Immediately refresh again and inspect the cooldown response and `next_fetch_at`.
6. After the cooldown, repeat the refresh. Depending on publisher support, expect either a 304-based report or a downloaded body with unchanged entries.

Example requests, assuming the feed you registered has ID 42:

```bash
curl --fail http://127.0.0.1:3000/health/ready
curl --fail -X POST http://127.0.0.1:3000/v1/feeds/42/refresh
curl --fail http://127.0.0.1:3000/v1/feeds/42
curl --fail 'http://127.0.0.1:3000/v1/items?feed_id=42&limit=10'
```

Use the actual returned ID rather than assuming 42 exists. The original README's `example.com` registration demonstrates request syntax, not a working publisher feed.

## 9. What has been tested, and what that proves

The most recent implementation validation passed **15 tests: 10 ordinary tests and 5 PostgreSQL-backed tests**, plus formatting and Clippy. This records the previous implementation run; tests were not rerun merely to write this guide.

Coverage includes:

- Parsing the three feed formats, preserving missing dates, rejecting invalid/oversized/deep XML input, and sanitizing HTML.
- Stable identity fallbacks, relative links, duplicate registration, and pagination.
- Address/port checks, dropping non-public addresses from mixed DNS results, unsafe redirects, streaming size limits, conditional requests, and Retry-After handling.
- The refresh API talking to a mock publisher and writing to a real PostgreSQL database.
- Repeated content, updated versions, 304 responses, malformed later responses, lease conflicts, stale ownership, cooldown, and transaction rollback.
- Liveness without a working database, readiness failure, and request validation.

The earlier foundation also passed an actual process startup/readiness/shutdown smoke test. The ingestion suite exercises the HTTP router directly with a local mock publisher. It is not evidence that every public feed works, that the service meets a particular throughput target, or that a complete security review has happened.

Run checks from `backend/`:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
DATABASE_URL=postgres://rss:rss_dev_password@127.0.0.1:5432/rss \
  cargo test --locked -- --ignored
```

The last command matters: database tests are explicitly ignored in the ordinary suite. They create isolated test databases and require a local PostgreSQL role with database-creation permission. Cargo tests do not automatically load the application's `.env`. See the README for equivalent container commands.

## 10. Recommended sequence toward a proper backend

Treat “proper” as reliable unattended ingestion, controlled access to user data, a usable reader API, and recoverable operations. The following milestones are recommendations; their proposed files, tables, and endpoints do not exist yet.

### Milestone 0: Make development reproducible

Establish a project Git repository if needed; repository initialization and commits have not been part of the work so far. Commit the source, fixtures, migrations, and lockfile. Choose a Rust toolchain version and an image update policy so development and CI agree.

Add CI that runs formatting, Clippy, ordinary tests, and the ignored tests against disposable PostgreSQL. Include an upgrade test that starts from migration 0001 with existing feed data before applying migration 0002 and future migrations. Add a small startup/shutdown check to the release workflow.

**Done when:** a clean checkout can build, apply migrations, and pass all test groups with documented commands. Secrets and `target/` remain outside version control.

### Milestone 1: Add reliable automatic polling

This is the next feature to implement. For feed polling alone, the existing feed table can be the initial durable work store: due times and refresh ownership already live in PostgreSQL. Introduce a more general jobs table when tasks such as extraction or AI require independent queues and attempt histories.

Recommended changes:

1. Separate automatic poll timing, manual refresh cooldown, and publisher failure backoff. For example, introduce `next_poll_at`, `manual_refresh_after`, and `retry_not_before` with clear ownership of each field. Manual actions must still respect publisher backoff.
2. Add a scheduler module and a worker entry point that scan a bounded batch of due feeds and claim them atomically. A database query using `FOR UPDATE SKIP LOCKED` is one option for avoiding contention between workers; release the claim transaction before network work. PostgreSQL describes this mechanism for queue-like consumers in its [SELECT documentation](https://www.postgresql.org/docs/current/sql-select.html).
3. Refactor ingestion into claim and execute-with-lease steps. The scheduler must pass the lease it claimed into execution; calling today's `refresh()` after already claiming the same feed would try to claim twice.
4. Use a reasonable initial polling interval and add jitter so many feeds do not all run at the same moment. Start with one worker process and measured publisher limits before scaling workers.
5. Stop accepting work during shutdown, allow a bounded drain period, and recover expired leases after restart. Extend the application's existing shutdown design to worker tasks using the principles in [Tokio's shutdown guide](https://tokio.rs/tokio/topics/shutdown).
6. Keep delivery at least once and preserve idempotent persistence. Do not promise that an external HTTP request can happen exactly once.

Keep the current synchronous refresh API working initially. If manual refresh later becomes asynchronous, introduce a documented job/status contract rather than silently changing its response semantics.

**Done when:** feeds update without API calls; restarting a worker resumes due work; two workers do not persist competing results for the same lease; a slow or failing publisher does not block other feeds; shutdown and retry behavior have tests.

### Milestone 2: Improve publisher compatibility and API contracts

Build a corpus of 10–20 public feeds that represent what you actually read. Include RSS, Atom, JSON Feed, missing IDs/dates, relative URLs, repeated IDs, source updates, redirects, and empty feeds.

Bounded gzip decompression is implemented. Keep destination checks active on every hop. Add deterministic tests for slow responses, redirect loops, changed redirect targets, and cancellation. Keep hardware/network-dependent timing tests conservative.

Standardize API errors, add request IDs and an OpenAPI contract, and make limits configurable with safe defaults. Record useful failure categories and HTTP status without recording token-bearing URLs or raw article content. Add a version-history endpoint if the client needs to display corrections.

**Done when:** the chosen feed corpus is supported or has documented limitations, a compressed oversized response cannot bypass limits, and API clients can handle errors consistently.

### Milestone 3: Add accounts and scoped subscriptions

The current `feeds` table is global and has no user ownership. Add accounts, authentication/session handling, and a `subscriptions` relationship that connects accounts to feeds. Public feed content may be shared internally; reading state and access decisions belong to each account.

Define access rules for every feed, article, refresh request, export, and future job. Private or credentialed feeds need explicit isolation and secret storage; a global unique URL alone is not a complete private-feed model. Avoid exposing the present global feed list as a multi-user product.

Validate permissions on every request, including when IDs are guessed or changed. This is consistent with the [OWASP authorization guidance](https://cheatsheetseries.owasp.org/cheatsheets/Authorization_Cheat_Sheet.html).

**Done when:** account A cannot retrieve or change resources available only to account B, with tests covering list endpoints, individual resources, jobs, and failure responses. Unauthenticated access is intentionally defined.

### Milestone 4: Build the reader's essential data features

Add per-user read/unread, starred, and saved state. Keep these records separate from publisher content so a feed refresh cannot erase user actions. Add folders, subscription removal, and OPML import/export with per-feed errors.

Implement operation IDs and revisions for state changes before designing offline clients. Decide conflict rules explicitly, including how bulk “mark read” applies to items arriving during the operation. A paginated article list alone does not provide reliable synchronization or deletion events.

Start keyword search with PostgreSQL's text search facilities, maintaining a normalized searchable text representation alongside stored content. Apply authorization while retrieving results. The database supports parsing, indexing, and ranking as described in its [full-text search documentation](https://www.postgresql.org/docs/current/textsearch.html).

**Done when:** a user can import subscriptions, read and save articles, search accessible content, and preserve state across retries and reconnects. Device synchronization has a specified cursor, deletion, and conflict contract.

### Milestone 5: Make a hosted pilot operable

Prepare a release image that runs the compiled application with a non-root runtime user. Add deployment configuration, secret injection, a deliberate migration step, and rollback instructions. Review database roles so the running service does not require the same privileges as schema migration tooling.

Add metrics for due-feed lag, request latency, publisher errors, lease expiry, parser failures, database pool usage, and worker throughput. Logs already exist; dashboards, alerts, and metrics do not.

Automate backups and actually restore one into an isolated environment. Define retention for articles, versions, logs, and deleted accounts. Measure database storage and query plans on a representative dataset before deciding to partition tables or introduce another data store.

Add request/account quotas and review outbound limits across multiple workers. The current per-process semaphores do not impose a whole-deployment publisher budget. Test hard restart, disk pressure, database outage, and slow-worker recovery against a declared operating target.

**Done when:** a pilot deployment can be installed, upgraded, observed, rolled back, backed up, and restored using documented procedures, and access isolation is already in place.

### Milestone 6: Introduce AI on a dependable foundation

Start with an optional, on-demand summary that references stored article versions. Put it in a durable enrichment queue with separate capacity from feed fetching. Save input version IDs, provider/model and prompt versions, output, evidence references, usage, and failure state.

Add account/source processing policies, provider adapters, an atomic spending reservation, and a cache scoped to authorized inputs. Local-only processing must not silently fall back to a hosted provider. A provider outage must leave reading and ingestion usable.

Versioned articles are a useful starting point, but they do not yet provide paragraph-level citation anchors, extraction provenance, or an evaluation corpus. Build those deliberately and evaluate supported claims, omissions, latency, and cost before automatic background summaries.

**Done when:** the feature is optional, attributable to a stored input version, permission-aware, budgeted, evaluated, and safely retryable. Then consider embeddings, story groups, and the original “what changed?” briefing concept.

## 11. A concrete plan for the next development cycle

Recommended scope: **reproducible checks plus one reliable polling worker**. Do not combine this with accounts, search, and AI in the same cycle.

| Work item | Proposed location | Acceptance evidence |
| --- | --- | --- |
| Configure CI and toolchain | Project-level CI configuration and backend toolchain settings | All 15 existing tests run automatically, including PostgreSQL tests. |
| Separate scheduling fields | A new migration under `backend/migrations/` | Existing feeds migrate with defined initial due times; failure backoff cannot be bypassed. |
| Refactor refresh ownership | `backend/src/ingest.rs` | Manual and worker-triggered execution use the same persistence behavior without double claims. |
| Claim due work in bounded batches | Proposed `backend/src/scheduler.rs` | Concurrent claimers do not own the same feed simultaneously. |
| Run and stop a worker | Proposed `backend/src/bin/worker.rs` | Restart recovery and bounded shutdown are demonstrated. |
| Measure behavior | Worker logs, basic counters, integration tests | Successful/failed attempts and due-time lag are visible. |

For one developer already comfortable with this code and Rust, use roughly **one to two weeks as an initial planning allowance**, then re-estimate after the scheduling schema and claim logic are tested. This is a scope estimate, not a benchmark or delivery commitment. A Rust learning period, unresolved feed edge cases, or deployment work can extend it.

Before expanding scope, run a small unattended trial using the feeds you actually follow. Record which feeds fail, whether articles duplicate, how long feeds remain overdue, and whether resource usage stays bounded.

## 12. How you should approach the code

Read in this order: `README.md` → `main.rs` → `api.rs` → `feeds.rs` → `items.rs` → `fetch.rs` → `ingest.rs` → migrations and tests.

As you read, trace one example article through each stage. Keep three questions separate: how it is identified, how its contents change, and which user will eventually be allowed to read it. The implementation currently answers the first two; accounts and authorization will answer the third.

Your most useful immediate tasks are to run the existing flow, collect representative feed fixtures, and understand the lease/transaction tests. Then implement the polling milestone with explicit acceptance criteria. Keep the current modular Rust application and PostgreSQL foundation while measurements tell us which parts actually need to grow.
