# RSS backend

Working ingestion backend: register a public feed, have it refreshed automatically in the background (or on demand), store versioned articles, and read them through a paginated API. Includes RSS/Atom/JSON Feed parsing, conditional HTTP requests, feed health, PostgreSQL migrations, structured logs, readiness/liveness, and graceful shutdown.

All implementation lives here, outside `../docs/`. The broader proposal is in [the design document](../docs/rss-platform-design.md).

This is a local, single-user foundation. Registration stores the URL; the built-in scheduler then fetches it within a few seconds and keeps polling it, and the refresh endpoint fetches on demand. Previews still parse supplied content without persistence. Authentication, synchronization, and AI remain subsequent milestones. Keep this unauthenticated API on loopback during development.

## Run with Docker

The only requirement is Docker (Docker Desktop on Windows or macOS) or Podman with a Compose provider. No Rust toolchain or local PostgreSQL is needed. From this directory:

```bash
docker compose up -d --build
curl http://127.0.0.1:3000/health/ready
docker compose logs -f backend
docker compose down          # stop; add -v to also delete the database volume
```

After code changes, `docker compose up -d --build` rebuilds and replaces the backend container. **podman-compose does not replace running containers on its own**; use `podman compose up -d --build --force-recreate`, or the old binary keeps running.

Compose builds the image from `Dockerfile`, starts PostgreSQL, waits until it is healthy, then starts the backend, which applies migrations on startup. The API is published on `127.0.0.1:3000` only, because it has no authentication yet. Both services restart automatically unless stopped.

| Variable | Default | Purpose |
| --- | --- | --- |
| `POSTGRES_PASSWORD` | `rss_dev_password` | Database password, used by both services. Use URL-safe characters, because it is embedded in `DATABASE_URL`. |
| `RUST_LOG` | `rss_backend=info,tower_http=info` | Log filter |
| `POLLING` | `true` | Run the automatic polling scheduler |
| `POLL_INTERVAL_SECONDS` | `1800` | Seconds between automatic refreshes of a healthy feed (60–86400) |

Set them in the shell or in a `.env` file next to `compose.yaml`; Compose reads that file automatically.

**The image.** `Dockerfile` compiles a release binary in `rust:1.99-bookworm` and copies only the binary into `gcr.io/distroless/cc-debian12:nonroot`: no shell, no package manager, and a non-root user. HTTPS trust roots are compiled into the binary. The container health check runs `rss-backend healthcheck`, which exits 0 only when `/health/ready` returns 200. Build it on its own with:

```bash
docker build -t rss-backend .
docker build --build-arg CARGO_BUILD_JOBS=2 -t rss-backend .   # lower peak memory, e.g. Raspberry Pi
```

**Other devices.** Both base images are published for `linux/amd64` and `linux/arm64` (among others), so the same files build natively on Intel/AMD machines, Apple Silicon Macs, ARM servers, and a 64-bit Raspberry Pi. Docker Desktop runs Linux containers on Windows and macOS. To build an image for another architecture from one machine, use `docker buildx build --platform linux/amd64,linux/arm64 -t <registry>/rss-backend --push .`; cross-architecture builds run under emulation and are much slower.

## Run locally

Requirements: a current stable Rust toolchain, and PostgreSQL 17 (or Docker/Podman to run it). Dependencies are resolved in `Cargo.lock`; keep that file committed.

```bash
cd '/home/sriniketh/source/NEW RSS/backend'
cp .env.example .env
docker compose up -d postgres
cargo run --locked
```

Podman users with a Compose provider can substitute `podman compose`. Without Compose, start PostgreSQL directly:

```bash
podman run -d --name new-rss-postgres \
  -e POSTGRES_USER=rss -e POSTGRES_PASSWORD=rss_dev_password -e POSTGRES_DB=rss \
  -p 127.0.0.1:5432:5432 \
  -v new-rss-postgres-data:/var/lib/postgresql/data \
  docker.io/library/postgres:17-bookworm
```

The server requires the database at startup and applies embedded migrations before listening on `127.0.0.1:3000`. Stop it with Ctrl+C. The database volume persists across container restarts.

If Rust is unavailable on a Linux host, use a development container from this directory after starting PostgreSQL:

```bash
podman build -f Containerfile.dev -t localhost/new-rss-dev .
podman run --rm -it --userns=keep-id --network=host \
  -v "$PWD:/app:Z" -w /app \
  localhost/new-rss-dev cargo run --locked
```

This command uses host networking to reach the loopback database; the API still binds to loopback. The `:Z` mount option supports SELinux hosts. It is a development workflow, not a production image.

## Try the API

```bash
curl --fail http://127.0.0.1:3000/health/live
curl --fail http://127.0.0.1:3000/health/ready
curl --fail http://127.0.0.1:3000/v1/feeds \
  -H 'content-type: application/json' \
  -d '{"url":"https://example.com/feed.xml"}'
curl --fail 'http://127.0.0.1:3000/v1/feeds?limit=20'
curl --fail http://127.0.0.1:3000/v1/feed-preview \
  -H 'content-type: application/json' \
  -d '{"content":"{\"version\":\"https://jsonfeed.org/version/1.1\",\"title\":\"Demo\",\"items\":[]}"}'
```

Register an actual public feed URL, take its returned `id`, and refresh it. For example, after registering a feed with ID 1:

```bash
curl --fail -X POST http://127.0.0.1:3000/v1/feeds/1/refresh
curl --fail http://127.0.0.1:3000/v1/feeds/1
curl --fail 'http://127.0.0.1:3000/v1/items?feed_id=1&limit=20'
```

The refresh response contains `inserted`, `updated`, `unchanged`, and `not_modified`. Use an item ID from the list with `GET /v1/items/{id}` to read its latest content. The `example.com` registration above demonstrates syntax only; it is not a real feed.

| Route | Behavior |
| --- | --- |
| `GET /health/live` | Process liveness; no database dependency |
| `GET /health/ready` | Database check with a two-second deadline; returns 503 on failure |
| `POST /v1/feeds` | Body `{ "url": "https://…" }`; returns the registered feed with 200, including on duplicate registration |
| `GET /v1/feeds` | `after_id` defaults to 0; `limit` defaults to 50 and allows 1–100; response has `feeds` and nullable `next_after_id` |
| `GET /v1/feeds/{id}` | Feed details and health: last attempt, last success, next permitted attempt, failure count, and last error |
| `POST /v1/feeds/{id}/refresh` | Fetch and ingest now; no request body; returns a refresh report |
| `GET /v1/items` | Optional positive `feed_id` and `before_id`, `limit` 1–100; newest ingestion IDs first; response has `items` and nullable `next_before_id` |
| `GET /v1/items/{id}` | Latest stored article content, metadata, and version ID; 404 if missing |
| `POST /v1/feed-preview` | Body `{ "content": "raw feed text" }`; returns a title and entry metadata |
| `POST /v1/imports/opml` | Body: an OPML file; registers every feed in it and returns a per-feed report |
| `GET /v1/exports/opml` | Downloads every registered feed as `subscriptions.opml` |

Previews allow at most 1 MiB of decoded feed content, within a 2 MiB JSON request limit, and keep the latest 1,000 entries like refreshes do. Two parser tasks may run concurrently; additional preview work gets 503. Parsing runs outside the async executor. XML document types are rejected; XML input is limited to 64 nesting levels and an event count scaled to the 16 MiB feed limit (4,194,304 events); escaped HTML produces one event per entity, so this limit must stay generous. HTML sanitization is enabled in feed-rs. Clients must render titles and other metadata as text. Article content is tagged with its source content type; summaries can contain sanitized markup. A future reader still needs a restrictive content security policy and external-resource policy.

Domain validation errors return 422 with `{ "error": "…", "code": "…" }`; database failures return a generic 503. Axum handles malformed JSON/query parameters and request-size rejection using its default responses. Future client contracts should normalize those errors too.

Refresh errors use 404 for missing feeds, 409 for an active/expired lease, 429 during the cooldown or failure backoff, 502 for publisher/network/parse failures, and 503 for database, parser, or overall capacity problems. Check the feed's `manual_refresh_after` and `retry_not_before` before retrying.

Every error body carries a stable `code`; clients should branch on it, not on message text. A failed refresh also stores the code in the feed's `last_error_code` and a readable message in `last_error`. Each cause has its own code and message:

| Code | Meaning |
| --- | --- |
| `invalid_url` | The feed URL or redirect target is not a usable HTTP(S) URL |
| `blocked_destination` | The address is private/reserved, or the host resolves only to such addresses |
| `blocked_port` | The port is not 80 or 443 |
| `dns_failed` | The host name did not resolve |
| `connect_timeout` | No TCP/TLS connection within 10 seconds (retried once) |
| `connect_failed` | The connection was refused or reset (retried once) |
| `tls_failed` | The TLS handshake failed, for example an expired or invalid certificate |
| `response_timeout` | The server stopped responding or sending the body within 15 seconds |
| `deadline_exceeded` | The whole fetch exceeded 40 seconds |
| `too_many_redirects`, `invalid_redirect`, `insecure_redirect` | Redirect problems: more than 5 hops, a bad Location, or HTTPS to HTTP |
| `http_status` | A non-200 status; the message includes it, such as `HTTP 404 Not Found` |
| `unexpected_not_modified` | A 304 without a conditional request |
| `unsupported_encoding`, `decompress_failed` | Compression other than gzip, or corrupt gzip |
| `body_read_failed` | The connection dropped mid-download |
| `too_large` | More than 16 MiB after decompression |
| `not_a_feed` | The URL returned an HTML web page |
| `unsupported_xml`, `xml_too_deep`, `xml_too_complex` | XML document types, more than 64 nesting levels, or the event limit |
| `parse_failed` | Not valid RSS, Atom, or JSON Feed |
| `internal` | An unexpected backend fault |

Other API codes: `invalid_request` (malformed path, query, or JSON), `unsupported_media_type` (missing `Content-Type: application/json`), `too_large` (request body over 2 MiB), `not_found` (missing resource or route), `method_not_allowed`, `capacity_busy` (all refresh slots in use), `refresh_busy` (this feed is already being refreshed), `parser_busy` (a downloaded feed waited 10 seconds without a free parser slot; the download is discarded but not counted as a feed failure), `cooling_down` (refreshed in the last 30 seconds; wait for `manual_refresh_after`), `retry_backoff` (failed recently; wait for `retry_not_before`), `lease_expired`, and `database_unavailable`. Every error, including malformed requests and unknown routes, is JSON.

**Dead feeds.** After 10 consecutive failures a feed's `dead_at` is set and a `feed marked dead` warning is logged. Automatic polling skips dead feeds. A manual refresh is still allowed; success clears `dead_at` and the failure count.

**Database errors** are logged as `database operation failed` with `context`, `kind` (such as `io` or `pool_timed_out`), and, for PostgreSQL errors, `sqlstate` and `constraint`. Error messages are never logged, because they can contain feed URLs.

## Import and export subscriptions (OPML)

Most feed readers can export subscriptions as an OPML file. Send the file as the raw request body; any content type is accepted:

```bash
curl -s -X POST http://127.0.0.1:3000/v1/imports/opml --data-binary @subscriptions.opml | jq
```

The response summarizes the import and lists every feed:

```json
{
  "added": 5, "existing": 2, "duplicates": 1,
  "feeds": [{ "id": 15, "url": "https://feeds.bbci.co.uk/news/rss.xml", "status": "added", "title": "BBC News", "folders": ["News"] }],
  "rejected": [{ "url": "file:///srv/feed.xml", "code": "invalid_url", "error": "feed URL must use HTTP or HTTPS and include a host" }]
}
```

- Outlines with an `xmlUrl` (any letter case) are feeds; outlines without one are folders, reported in `folders` from outermost to innermost. Folders are not stored yet.
- URLs are normalized like registration, so repeats in the file count as `duplicates`, and feeds already registered are `existing`. Importing the same file twice is harmless.
- An invalid URL is listed in `rejected` without failing the import. A file that is not OPML fails with `invalid_opml`; document types are rejected (`unsupported_xml`), nesting is limited to 64 levels, and one file may list at most 10,000 feeds (`too_many_feeds`). Requests are limited to 2 MiB.
- New feeds are due immediately, so the scheduler fetches them within seconds (about 36 feeds per minute at the default limits). With `POLLING=false`, refresh them manually.

`tests/fixtures/subscriptions.opml` is a small example file.

Export every registered feed, for a backup or to move to another reader:

```bash
curl -s http://127.0.0.1:3000/v1/exports/opml -o subscriptions.opml
```

The export is a flat OPML 2.0 list (folders are not stored yet) using each feed's own title, or its URL before the first fetch. Importing an export gives back the same feeds.

## Ingestion behavior

- Item identity is scoped to a feed: explicit publisher ID first, normalized article URL second, deterministic content hash last. URL fragments are removed; query parameters are preserved. Relative article links resolve against the final feed URL.
- Identical content keeps the existing version. A changed title, URL, authors, summary, content, content type, or publisher `published` date creates a new version on the same item. A change to the publisher's `updated` timestamp alone does not, because some feeds bump it on every fetch. Versions stored before this rule carry an older fingerprint; a match against it counts as unchanged and upgrades the stored fingerprint in place.
- Some feeds embed live counters in their text (hnrss.org puts points and comment counts in each summary), so their items legitimately gain a version on most polls.
- Storage takes a fixed handful of SQL statements per refresh, whatever the number of entries: one upsert for all items, one insert for all new versions, and one update of current-version pointers. Older versions remain in PostgreSQL; a version-history API is not yet implemented.
- Items missing both ID and URL are stable for repeated identical snapshots. Changed content becomes a new item because there is no reliable identity to associate the update with. A publisher changing its ID also creates a new item.
- Duplicate IDs within one response use the first occurrence. Items disappearing from a rolling feed are retained. A missing publication date falls back to the entry's updated date (Atom requires only `updated`); it is null only when both are missing. Ingestion order is independent of publication dates.
- A feed with more than 1,000 entries keeps the latest 1,000: by date when every entry has one, otherwise the first 1,000 in document order (feeds normally list newest first). The refresh report's `skipped` counts the rest.
- Feed metadata, articles, versions, and HTTP validators commit in one transaction. Malformed responses preserve existing content and validators. A valid conditional 304 updates feed health without creating versions.
- Refresh uses a 90-second database lease and a unique token. Competing refreshes are rejected; completion verifies token ownership. Crashes or cancelled requests can leave the lease until expiry. Persistence failures roll back; their leases also expire before retry.
- Each feed has three timers, each with one owner:

  | Field | Set by | Who waits for it |
  | --- | --- | --- |
  | `next_poll_at` | Success: poll interval ± 10% jitter. Failure: the larger of the backoff and the poll interval | The scheduler |
  | `manual_refresh_after` | Success: 30 seconds | Manual refreshes |
  | `retry_not_before` | Failure: backoff from 30 seconds to one hour, or a larger `Retry-After` on HTTP 429/503 (up to 24 hours) | Everyone |

## Automatic polling

The scheduler runs inside the server process (`src/scheduler.rs`). Every 5 seconds it claims up to three due feeds (`next_poll_at` reached, not dead, not backing off, not leased) with `FOR UPDATE SKIP LOCKED`, and refreshes them concurrently. One of the four refresh slots stays free for manual refreshes, and a slow publisher only occupies its own slot.

- **New feeds** are due immediately, so they are fetched within seconds of registration.
- **Healthy feeds** are refreshed every `POLL_INTERVAL_SECONDS` (default 30 minutes) with ±10% jitter, so feeds added together drift apart.
- **Failing feeds** wait at least one poll interval between automatic attempts, so the 10 failures that mark a feed dead span roughly five hours of continuous failure at the default interval.
- **Restarts and crashes** are safe: a refresh in progress holds a 90-second lease, and an expired lease is claimed again. Delivery is at least once, and storage is idempotent, so a repeated refresh creates no duplicates.
- **Several instances** can share one database; `SKIP LOCKED` keeps them from claiming the same feed. Outbound limits are per process, though, so scale out deliberately.
- **Shutdown** stops claiming, then waits up to 30 seconds for in-flight refreshes.
- Set `POLLING=false` to run an instance that only serves the API.

Logs: `scheduler started`, `scheduled refresh succeeded` (with inserted/updated/unchanged/skipped counts), `feed refresh failed` (with its code), `feed marked dead`, and `scheduler stopping`.

## Fetch policy

The HTTP client permits HTTP(S) on ports 80 and 443. It rejects credentials in URLs, private/loopback/link-local/special-use IPs, IPv6 transition ranges, and non-public addresses in DNS answers (they are dropped; a host with no public address is rejected, which keeps DNS64 networks working). Validated DNS results go directly to the connector. Environment proxies are disabled. Every redirect is revalidated, with a maximum of five hops; HTTPS-to-HTTP redirects are rejected. Conditional validators are sent only to the exact URL they describe.

Limits: four concurrent refreshes, two requests per hostname, a ten-second connect budget (DNS, TCP, and TLS), a fifteen-second per-request timeout, and a forty-second total fetch deadline including redirects and body streaming. The resolver keeps at most two addresses per IP family because hyper-util divides the connect budget across them; this leaves each address time for TCP SYN retransmission. A failed connection is retried once after 500 ms, which is safe because the request was never delivered. On some networks a DNS lookup occasionally takes five seconds; the ten-second budget absorbs that.

The client requests gzip and decodes it while streaming. Bodies are capped at 16 MiB after decompression, so a compression bomb stops at the limit; other encodings are rejected. Only status 200 provides a new body; status 304 is accepted only after sending conditional headers. Requests accept common feed types and fall back to `*/*`, so strict publishers do not answer 406.

When every hop of a redirect chain is permanent (301 or 308), the feed's stored URL is updated to the final URL after a successful refresh, so later polls skip the redirect. A temporary redirect anywhere in the chain keeps the original URL. If another registered feed already uses the final URL, the URL is left unchanged.

This deliberately conservative destination policy may reject unusual public hosting arrangements or nonstandard ports. Its address classification should be reviewed as special-use allocations change. The implementation uses [Reqwest's resolver and redirect controls](https://docs.rs/reqwest/0.12.28/reqwest/struct.ClientBuilder.html).

## Verify changes

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
DATABASE_URL=postgres://rss:rss_dev_password@127.0.0.1:5432/rss \
  cargo test --locked -- --ignored
```

Ordinary tests require no PostgreSQL instance or external internet; HTTP tests use local mock publishers with an override available only inside the private test module. Explicitly ignored tests create isolated databases, apply migrations, and verify HTTP refresh end to end, versioning, duplicate registration, item pagination, lease fencing, retry state, and rollback. Use the local development role with database-creation privileges. Cargo does not automatically load `.env` for tests; supply `DATABASE_URL` as shown. Run all ignored tests, including those in library modules, using the command above.

For container-based checks, use the same `podman run` command above and replace `cargo run --locked` with the desired Cargo command. For database tests, add `-e DATABASE_URL=postgres://rss:rss_dev_password@127.0.0.1:5432/rss` before the image name. The development image includes rustfmt and Clippy.

## Next development milestone

Automatic polling is implemented. Next:

1. Run it unattended for a day against the feeds you read, and record failures and how late feeds are polled.
2. Add publication-aware intervals (poll busy feeds more often than quiet ones).
3. Add accounts and scoped subscriptions before exposing this API beyond local development; follow with read/star state and synchronization.

The full plan is in [the next-steps checklist](../docs/next-steps.md).

Your most useful immediate contribution remains a real-feed corpus and a short description of your daily reading workflow: folders you use, what you save, and what feels repetitive. Those inputs will shape both ingestion tests and the first client.

AI comes after reliable item storage and versioning. Source-linked summaries depend on those foundations.
