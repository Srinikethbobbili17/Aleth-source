# RSS backend

Working ingestion slice: register a public feed, refresh it manually, store versioned articles, and read them through a paginated API. Includes RSS/Atom/JSON Feed parsing, conditional HTTP requests, feed health, PostgreSQL migrations, structured logs, readiness/liveness, and graceful shutdown.

All implementation lives here, outside `../docs/`. The broader proposal is in [the design document](../docs/rss-platform-design.md).

This is a local, single-user foundation. Registration stores the URL; the separate refresh endpoint fetches and persists articles. Previews still parse supplied content without persistence. Authentication, automatic scheduling, synchronization, and AI remain subsequent milestones. Keep this unauthenticated API on loopback during development.

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

Previews allow at most 1 MiB of decoded feed content and 1,000 entries, within a 2 MiB JSON request limit. Two parser tasks may run concurrently; additional preview work gets 503. Parsing runs outside the async executor. XML document types are rejected; XML input is limited to 64 nesting levels and 50,000 events. HTML sanitization is enabled in feed-rs. Clients must render titles and other metadata as text. Article content is tagged with its source content type; summaries can contain sanitized markup. A future reader still needs a restrictive content security policy and external-resource policy.

Domain validation errors return 422 with `{ "error": "…" }`; database failures return a generic 503. Axum handles malformed JSON/query parameters and request-size rejection using its default responses. Future client contracts should normalize those errors too.

Refresh errors use 404 for missing feeds, 409 for an active/expired lease or busy parser, 429 during cooldown, 502 for publisher/network/parse failures, and 503 for database or overall capacity problems. Check feed health for `next_fetch_at` before retrying.

## Ingestion behavior

- Item identity is scoped to a feed: explicit publisher ID first, normalized article URL second, deterministic content hash last. URL fragments are removed; query parameters are preserved. Relative article links resolve against the final feed URL.
- Identical content keeps the existing version. Changed title, URL, authors, summary, content, or source timestamps create a new version on the same item. Older versions remain in PostgreSQL; a version-history API is not yet implemented.
- Items missing both ID and URL are stable for repeated identical snapshots. Changed content becomes a new item because there is no reliable identity to associate the update with. A publisher changing its ID also creates a new item.
- Duplicate IDs within one response use the first occurrence. Items disappearing from a rolling feed are retained. Missing publication dates remain null; ingestion order is independent of publication dates.
- Feed metadata, articles, versions, and HTTP validators commit in one transaction. Malformed responses preserve existing content and validators. A valid conditional 304 updates feed health without creating versions.
- Refresh uses a 90-second database lease and a unique token. Competing refreshes are rejected; completion verifies token ownership. Crashes or cancelled requests can leave the lease until expiry. Persistence failures roll back; their leases also expire before retry.
- Successful refreshes have a 30-second manual cooldown. Failures back off from 30 seconds to one hour, honoring larger `Retry-After` values on HTTP 429/503 up to 24 hours. This records retry eligibility but does not schedule work automatically.

## Fetch policy

The HTTP client permits HTTP(S) on ports 80 and 443. It rejects credentials in URLs, private/loopback/link-local/special-use IPs, IPv6 transition ranges, and non-public addresses in DNS answers (they are dropped; a host with no public address is rejected, which keeps DNS64 networks working). Validated DNS results go directly to the connector. Environment proxies are disabled. Every redirect is revalidated, with a maximum of five hops; HTTPS-to-HTTP redirects are rejected. Conditional validators are sent only to the exact URL they describe.

Limits: four concurrent refreshes, two requests per hostname, five-second connection timeout, and a twenty-second total fetch deadline including redirects and body streaming. Responses are capped at 1 MiB even when streamed without Content-Length. The initial implementation requests identity encoding and rejects compressed responses, so a publisher that insists on gzip needs a later bounded decompression implementation. Only status 200 provides a new body; status 304 is accepted only after sending conditional headers.

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

The manual ingestion milestone is implemented. Next, make updates happen reliably without pressing refresh:

1. Add a durable scheduler/worker that selects due feeds, reuses ingestion leases, and recovers after restart.
2. Separate a user-triggered refresh cooldown from the automatic polling schedule; add publication-aware intervals and jitter.
3. Add fixture coverage for the actual feeds you read and bounded gzip decompression for publishers that require it.
4. Add accounts and scoped subscriptions before exposing this API beyond local development; follow with read/star state and synchronization.

Your most useful immediate contribution remains a real-feed corpus and a short description of your daily reading workflow: folders you use, what you save, and what feels repetitive. Those inputs will shape both ingestion tests and the first client.

AI comes after reliable item storage and versioning. Source-linked summaries depend on those foundations.
