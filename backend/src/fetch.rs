use std::{
    collections::HashMap,
    error::Error as StdError,
    fmt, io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use reqwest::{
    Client, StatusCode, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    header,
};
use tokio::sync::Semaphore;

use crate::{
    failure::{Failure, FailureKind},
    feeds::{MAX_FEED_BYTES, normalize_feed_url},
};

/// Whole-fetch deadline: slot waits, DNS, one connection retry, redirects, and the body.
const FETCH_DEADLINE: Duration = Duration::from_secs(40);
/// hyper-util divides the connect budget across a host's addresses of each
/// family. Keeping at most two per family leaves each address about five
/// seconds, enough for TCP to retransmit a dropped SYN after ~1 s and ~3 s.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ADDRS_PER_FAMILY: usize = 2;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Default)]
pub struct Validators {
    pub url: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug)]
pub enum FetchResult {
    Modified {
        body: Vec<u8>,
        validators: Validators,
    },
    NotModified {
        validators: Validators,
    },
}

#[derive(Clone)]
pub struct Fetcher {
    client: Client,
    slots: Arc<Semaphore>,
    hosts: Arc<Mutex<HashMap<String, Weak<Semaphore>>>>,
}

impl Fetcher {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self::with_client(
            client_builder()
                .dns_resolver(Arc::new(PublicResolver))
                .build()?,
        ))
    }

    fn with_client(client: Client) -> Self {
        Self {
            client,
            slots: Arc::new(Semaphore::new(4)),
            hosts: Arc::default(),
        }
    }

    pub async fn fetch(
        &self,
        input: &str,
        validators: &Validators,
    ) -> Result<FetchResult, Failure> {
        tokio::time::timeout(FETCH_DEADLINE, self.fetch_inner(input, validators))
            .await
            .map_err(|_| {
                Failure::new(
                    FailureKind::DeadlineExceeded,
                    "feed fetch exceeded 40 seconds",
                )
            })?
    }

    async fn fetch_inner(
        &self,
        input: &str,
        validators: &Validators,
    ) -> Result<FetchResult, Failure> {
        let unavailable = || Failure::new(FailureKind::Internal, "fetcher unavailable");
        let _slot = self.slots.acquire().await.map_err(|_| unavailable())?;
        let normalized = normalize_feed_url(input)
            .map_err(|message| Failure::new(FailureKind::InvalidUrl, message))?;
        let mut url = Url::parse(&normalized)
            .map_err(|_| Failure::new(FailureKind::InvalidUrl, "invalid feed URL"))?;
        for redirect in 0..=5 {
            validate_destination(&url)?;
            let host = url
                .host_str()
                .ok_or_else(|| Failure::new(FailureKind::InvalidUrl, "feed URL has no host"))?;
            let host_slots = {
                let mut hosts = self.hosts.lock().map_err(|_| unavailable())?;
                hosts.retain(|_, slots| slots.strong_count() > 0);
                match hosts.get(host).and_then(Weak::upgrade) {
                    Some(slots) => slots,
                    None => {
                        let slots = Arc::new(Semaphore::new(2));
                        hosts.insert(host.to_owned(), Arc::downgrade(&slots));
                        slots
                    }
                }
            };
            let _host_slot = host_slots.acquire().await.map_err(|_| unavailable())?;
            let mut request = self.client.get(url.clone());
            let mut conditional = false;
            // Validators describe one representation at one URL, not every redirect hop.
            if validators.url.as_deref() == Some(url.as_str()) {
                if let Some(etag) = &validators.etag {
                    request = request.header(header::IF_NONE_MATCH, etag);
                    conditional = true;
                }
                if let Some(modified) = &validators.last_modified {
                    request = request.header(header::IF_MODIFIED_SINCE, modified);
                    conditional = true;
                }
            }
            let mut response = send_with_retry(request).await?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                if redirect == 5 {
                    return Err(Failure::new(
                        FailureKind::TooManyRedirects,
                        "feed redirected more than 5 times",
                    ));
                }
                let invalid_redirect = || {
                    Failure::new(
                        FailureKind::InvalidRedirect,
                        "redirect Location is missing or not a valid URL",
                    )
                };
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(invalid_redirect)?;
                let next = url.join(location).map_err(|_| invalid_redirect())?;
                if url.scheme() == "https" && next.scheme() != "https" {
                    return Err(Failure::new(
                        FailureKind::InsecureRedirect,
                        "HTTPS to HTTP redirect blocked",
                    ));
                }
                let next = normalize_feed_url(next.as_str())
                    .map_err(|message| Failure::new(FailureKind::InvalidRedirect, message))?;
                url = Url::parse(&next).map_err(|_| invalid_redirect())?;
                continue;
            }
            let received = Validators {
                url: Some(url.to_string()),
                etag: header_string(response.headers(), header::ETAG),
                last_modified: header_string(response.headers(), header::LAST_MODIFIED),
            };
            if status == StatusCode::NOT_MODIFIED {
                if !conditional {
                    return Err(Failure::new(
                        FailureKind::UnexpectedNotModified,
                        "publisher sent 304 Not Modified to an unconditional request",
                    ));
                }
                return Ok(FetchResult::NotModified {
                    validators: Validators {
                        etag: received.etag.or_else(|| validators.etag.clone()),
                        last_modified: received
                            .last_modified
                            .or_else(|| validators.last_modified.clone()),
                        ..received
                    },
                });
            }
            if status != StatusCode::OK {
                return Err(Failure {
                    kind: FailureKind::HttpStatus,
                    message: format!("publisher returned HTTP {status}").into(),
                    retry_after_seconds: if matches!(status.as_u16(), 429 | 503) {
                        header_string(response.headers(), header::RETRY_AFTER)
                            .and_then(|v| retry_after(&v))
                    } else {
                        None
                    },
                });
            }
            // Reqwest decodes gzip and removes its header. Any encoding left
            // here was not requested and cannot be bounded, so reject it.
            if let Some(encoding) = response
                .headers()
                .get(header::CONTENT_ENCODING)
                .filter(|v| *v != "identity")
            {
                let encoding = encoding
                    .to_str()
                    .ok()
                    .filter(|v| v.len() <= 32)
                    .unwrap_or("unknown");
                return Err(Failure::new(
                    FailureKind::UnsupportedEncoding,
                    format!("publisher used unsupported compression: {encoding}"),
                ));
            }
            let too_large = || Failure::new(FailureKind::TooLarge, "feed response exceeds 16 MiB");
            if response
                .content_length()
                .is_some_and(|n| n > MAX_FEED_BYTES as u64)
            {
                return Err(too_large());
            }
            // Chunks arrive decompressed, so this bounds the expanded size and
            // stops reading a compression bomb as soon as it crosses the limit.
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| body_failure(&error))?
            {
                if body.len().saturating_add(chunk.len()) > MAX_FEED_BYTES {
                    return Err(too_large());
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(FetchResult::Modified {
                body,
                validators: received,
            });
        }
        Err(Failure::new(
            FailureKind::TooManyRedirects,
            "feed redirected more than 5 times",
        ))
    }
}

/// Retries once when no connection could be made. The request was never
/// delivered in that case, so repeating it is safe.
async fn send_with_retry(request: reqwest::RequestBuilder) -> Result<reqwest::Response, Failure> {
    let retry = request.try_clone();
    let failure = match request.send().await {
        Ok(response) => return Ok(response),
        Err(error) => classify(&error),
    };
    match (failure.kind, retry) {
        (FailureKind::ConnectTimeout | FailureKind::ConnectFailed, Some(retry)) => {
            tracing::info!(code = failure.code(), "retrying feed connection once");
            tokio::time::sleep(CONNECT_RETRY_DELAY).await;
            retry.send().await.map_err(|error| classify(&error))
        }
        _ => Err(failure),
    }
}

fn classify(error: &reqwest::Error) -> Failure {
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(dns) = cause.downcast_ref::<DnsError>() {
            return dns.failure();
        }
        if error.is_connect() && cause.downcast_ref::<io::Error>().is_some_and(is_tls_error) {
            return Failure::new(
                FailureKind::TlsFailed,
                "TLS handshake with the feed server failed",
            );
        }
        source = cause.source();
    }
    match (error.is_connect(), error.is_timeout()) {
        (true, true) => Failure::new(
            FailureKind::ConnectTimeout,
            "timed out connecting to the feed server",
        ),
        (true, false) => Failure::new(
            FailureKind::ConnectFailed,
            "could not connect to the feed server",
        ),
        (false, true) => Failure::new(
            FailureKind::ResponseTimeout,
            "feed server did not respond within 15 seconds",
        ),
        (false, false) => Failure::new(
            FailureKind::BodyReadFailed,
            "feed server closed the connection before responding",
        ),
    }
}

// TLS handshake failures are InvalidData I/O errors, nested inside an `Other`
// I/O error. `io::Error::source` skips the wrapped error, so unwrap it directly.
fn is_tls_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::InvalidData
        || error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<io::Error>())
            .is_some_and(is_tls_error)
}

fn body_failure(error: &reqwest::Error) -> Failure {
    if error.is_decode() {
        Failure::new(
            FailureKind::DecompressFailed,
            "feed body could not be decompressed",
        )
    } else if error.is_timeout() {
        Failure::new(
            FailureKind::ResponseTimeout,
            "feed body did not finish downloading within 15 seconds",
        )
    } else {
        Failure::new(
            FailureKind::BodyReadFailed,
            "connection dropped while downloading the feed",
        )
    }
}

fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(2)
        // Reqwest sends `Accept-Encoding: gzip` and decodes the body as it streams.
        .gzip(true)
        .no_brotli().no_deflate().no_zstd()
        .user_agent(concat!("rss-backend/", env!("CARGO_PKG_VERSION")))
        .default_headers([
            (header::ACCEPT, header::HeaderValue::from_static("application/atom+xml, application/rss+xml, application/feed+json, application/xml, text/xml, application/json")),
        ].into_iter().collect())
}

fn header_string(headers: &header::HeaderMap, key: header::HeaderName) -> Option<String> {
    headers
        .get(key)
        .filter(|v| v.len() <= 4096)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn retry_after(value: &str) -> Option<i64> {
    let seconds = value.parse::<i64>().ok().or_else(|| {
        chrono::DateTime::parse_from_rfc2822(value)
            .ok()
            .map(|date| (date.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds())
    })?;
    Some(seconds.clamp(30, 86_400))
}

fn validate_destination(url: &Url) -> Result<(), Failure> {
    normalize_feed_url(url.as_str())
        .map_err(|message| Failure::new(FailureKind::InvalidUrl, message))?;
    // A deliberately small public-web port policy for the initial fetcher.
    if !matches!(url.port_or_known_default(), Some(80 | 443)) {
        return Err(Failure::new(
            FailureKind::BlockedPort,
            "feed destination port must be 80 or 443",
        ));
    }
    let public = match url.host() {
        Some(url::Host::Ipv4(ip)) => public_ip(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => public_ip(IpAddr::V6(ip)),
        _ => true,
    };
    if public {
        Ok(())
    } else {
        Err(Failure::new(
            FailureKind::BlockedDestination,
            "feed address is private or reserved",
        ))
    }
}

// Conservative globally routed unicast policy. IPv6 transition mechanisms and
// special-use ranges are excluded, including mapped IPv4 and NAT64 prefixes.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(matches!(a, 0 | 10 | 127 | 224..=255)
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192
                    && (b == 168 || (b == 0 && matches!(c, 0 | 2)) || (b == 88 && c == 99)))
                || (a == 198 && (matches!(b, 18 | 19) || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

/// Resolver errors carry their cause through hyper so `classify` can tell a
/// blocked destination from a name that does not resolve.
#[derive(Debug)]
enum DnsError {
    LookupFailed,
    Blocked,
}

impl DnsError {
    fn failure(&self) -> Failure {
        match self {
            Self::LookupFailed => Failure::new(
                FailureKind::DnsFailed,
                "could not resolve the feed's host name",
            ),
            Self::Blocked => Failure::new(
                FailureKind::BlockedDestination,
                "feed host resolves only to private or reserved addresses",
            ),
        }
    }
}

impl fmt::Display for DnsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.failure().message)
    }
}

impl StdError for DnsError {}

struct PublicResolver;

// Drop non-public answers instead of rejecting the host: DNS64 networks add
// 64:ff9b:: addresses beside public IPv4 ones. Only public addresses are dialed.
fn validated_addresses(addresses: Vec<SocketAddr>) -> Result<Addrs, DnsError> {
    if addresses.is_empty() {
        return Err(DnsError::LookupFailed);
    }
    let (mut v4, mut v6) = (0, 0);
    let public: Vec<SocketAddr> = addresses
        .into_iter()
        .filter(|address| public_ip(address.ip()))
        .filter(|address| {
            let count = if address.is_ipv4() { &mut v4 } else { &mut v6 };
            *count += 1;
            *count <= MAX_ADDRS_PER_FAMILY
        })
        .collect();
    if public.is_empty() {
        return Err(DnsError::Blocked);
    }
    Ok(Box::new(public.into_iter()))
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await
                .map_err(|_| DnsError::LookupFailed)?
                .collect();
            // Return the validated addresses directly to the connector; it does
            // not perform an independent second lookup that could be rebound.
            Ok(validated_addresses(addresses)?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, Response},
        routing::get,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn blocks_special_addresses_and_ports() {
        assert!(matches!(
            validated_addresses(vec![]),
            Err(DnsError::LookupFailed)
        ));
        let capped: Vec<SocketAddr> = validated_addresses(
            [
                "1.1.1.1:0",
                "1.0.0.1:0",
                "8.8.8.8:0",
                "[2606:4700::1]:0",
                "[2606:4700::2]:0",
                "[2606:4700::3]:0",
            ]
            .map(|a| a.parse().unwrap())
            .to_vec(),
        )
        .unwrap()
        .collect();
        assert_eq!(capped.len(), 4);
        assert!(
            validated_addresses(vec![
                "127.0.0.1:0".parse().unwrap(),
                "[64:ff9b::7f00:1]:0".parse().unwrap()
            ])
            .is_err()
        );
        let mixed: Vec<SocketAddr> = validated_addresses(vec![
            "127.0.0.1:0".parse().unwrap(),
            "8.8.8.8:0".parse().unwrap(),
            "[64:ff9b::808:808]:0".parse().unwrap(),
        ])
        .unwrap()
        .collect();
        assert_eq!(mixed, vec!["8.8.8.8:0".parse().unwrap()]);
        assert!(validated_addresses(vec!["8.8.8.8:0".parse().unwrap()]).is_ok());
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.0.8",
            "198.18.0.1",
            "224.0.0.1",
            "::1",
            "::ffff:8.8.8.8",
            "fc00::1",
            "fe80::1",
            "64:ff9b::808:808",
            "2001:db8::1",
            "2002:808:808::1",
            "3fff::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
        for url in [
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://example.com:8080/",
            "http://2130706433/",
        ] {
            assert!(
                validate_destination(&Url::parse(url).unwrap()).is_err(),
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn streaming_limits_redirect_policy_and_conditional_headers() {
        let seen = Arc::new(AtomicUsize::new(0));
        let count = seen.clone();
        let app = Router::new().route(
            "/{kind}",
            get(move |request: Request<Body>| {
                count.fetch_add(1, Ordering::SeqCst);
                async move {
                    match request.uri().path() {
                        "/etag"
                            if request
                                .headers()
                                .get(header::IF_NONE_MATCH)
                                .is_some_and(|v| v == "\"v1\"") =>
                        {
                            Response::builder().status(304).body(Body::empty()).unwrap()
                        }
                        "/etag" => Response::builder()
                            .header("etag", "\"v1\"")
                            .body(Body::from("feed bytes"))
                            .unwrap(),
                        "/redirect" => Response::builder()
                            .status(302)
                            .header("location", "http://127.0.0.1/blocked")
                            .body(Body::empty())
                            .unwrap(),
                        "/large" => Response::new(Body::from_stream(futures_util::stream::iter([
                            Ok::<_, io::Error>(vec![b'x'; MAX_FEED_BYTES / 2]),
                            Ok(vec![b'x'; MAX_FEED_BYTES / 2 + 1]),
                        ]))),
                        "/gzip" => Response::builder()
                            .header("content-encoding", "gzip")
                            .body(Body::from(gzip(b"feed bytes")))
                            .unwrap(),
                        "/bomb" => Response::builder()
                            .header("content-encoding", "gzip")
                            .body(Body::from(gzip(&vec![0; MAX_FEED_BYTES + 1])))
                            .unwrap(),
                        "/corrupt" => Response::builder()
                            .header("content-encoding", "gzip")
                            .body(Body::from("not gzip"))
                            .unwrap(),
                        "/brotli" => Response::builder()
                            .header("content-encoding", "br")
                            .body(Body::from("bytes"))
                            .unwrap(),
                        "/missing" => Response::builder().status(404).body(Body::empty()).unwrap(),
                        "/unconditional" => {
                            Response::builder().status(304).body(Body::empty()).unwrap()
                        }
                        _ => Response::builder()
                            .status(429)
                            .header("retry-after", "120")
                            .body(Body::empty())
                            .unwrap(),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // Only this private unit-test constructor substitutes a local endpoint.
        // The production client always uses the public-address resolver.
        let client = client_builder()
            .resolve_to_addrs("fixture.test", &[address])
            .build()
            .unwrap();
        let fetcher = Fetcher::with_client(client);
        let first = fetcher
            .fetch("http://fixture.test/etag", &Validators::default())
            .await
            .unwrap();
        let FetchResult::Modified { body, validators } = first else {
            panic!("expected body")
        };
        assert_eq!(body, b"feed bytes");
        assert!(matches!(
            fetcher
                .fetch("http://fixture.test/etag", &validators)
                .await
                .unwrap(),
            FetchResult::NotModified { .. }
        ));
        let FetchResult::Modified { body, .. } = fetcher
            .fetch("http://fixture.test/gzip", &Validators::default())
            .await
            .unwrap()
        else {
            panic!("expected body")
        };
        assert_eq!(body, b"feed bytes");
        // Different causes must never share a message.
        let mut messages = HashMap::new();
        for (path, kind) in [
            ("redirect", FailureKind::BlockedDestination),
            ("large", FailureKind::TooLarge),
            ("bomb", FailureKind::TooLarge),
            ("corrupt", FailureKind::DecompressFailed),
            ("brotli", FailureKind::UnsupportedEncoding),
            ("unconditional", FailureKind::UnexpectedNotModified),
            ("missing", FailureKind::HttpStatus),
        ] {
            let failure = fetcher
                .fetch(
                    &format!("http://fixture.test/{path}"),
                    &Validators::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(failure.kind, kind, "{path}: {}", failure.message);
            let code = messages
                .entry(failure.message.clone())
                .or_insert(failure.code());
            assert_eq!(*code, failure.code(), "{path}");
        }
        assert!(messages.contains_key("publisher returned HTTP 404 Not Found"));
        let limited = fetcher
            .fetch("http://fixture.test/limited", &Validators::default())
            .await
            .unwrap_err();
        assert_eq!(
            limited.message,
            "publisher returned HTTP 429 Too Many Requests"
        );
        assert_eq!(limited.retry_after_seconds, Some(120));
        assert_eq!(seen.load(Ordering::SeqCst), 11);
        server.abort();
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
    async fn refresh_api_fetches_versions_and_preserves_content_after_parse_failure(
        pool: sqlx::PgPool,
    ) {
        use axum::body::to_bytes;
        use tower::ServiceExt;
        let phase = Arc::new(AtomicUsize::new(0));
        let response_phase = phase.clone();
        let publisher = Router::new().route("/feed", get(move |request: Request<Body>| {
            let phase = response_phase.load(Ordering::SeqCst);
            async move {
                if phase == 1 {
                    assert_eq!(request.headers()[header::IF_NONE_MATCH], "\"v0\"");
                    return Response::builder().status(304).body(Body::empty()).unwrap();
                }
                let body = if phase == 3 { "malformed feed".to_owned() } else {
                    format!(r#"{{"version":"https://jsonfeed.org/version/1.1","title":"Published","items":[{{"id":"one","title":"Title {phase}","content_text":"Article body"}}]}}"#)
                };
                Response::builder().header("etag", format!("\"v{phase}\""))
                    .body(Body::from(body)).unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, publisher).await.unwrap();
        });
        let client = client_builder()
            .resolve_to_addrs("fixture.test", &[address])
            .build()
            .unwrap();
        let app = crate::api::router_with_fetcher(pool.clone(), Fetcher::with_client(client));
        let registered = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/feeds")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"url":"http://fixture.test/feed"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(registered.status(), StatusCode::OK);
        let body = to_bytes(registered.into_body(), 64 * 1024).await.unwrap();
        let feed_id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_i64()
            .unwrap();
        let refresh_uri = format!("/v1/feeds/{feed_id}/refresh");
        for step in 0..4 {
            phase.store(step, Ordering::SeqCst);
            sqlx::query("UPDATE feeds SET next_fetch_at = now() WHERE id = $1")
                .bind(feed_id)
                .execute(&pool)
                .await
                .unwrap();
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(&refresh_uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if step == 3 {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::OK
                }
            );
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
            match step {
                0 => assert_eq!(report["inserted"], 1),
                1 => assert_eq!(report["not_modified"], true),
                2 => assert_eq!(report["updated"], 1),
                _ => assert_eq!(report["code"], "parse_failed"),
            }
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/items?feed_id={feed_id}&limit=1"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["title"], "Title 2");
        assert!(page["next_before_id"].is_null());
        let id = page["items"][0]["id"].as_i64().unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/items/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail["content"], "Article body");
        let state: (String, i32) =
            sqlx::query_as("SELECT etag, failure_count FROM feeds WHERE id = $1")
                .bind(feed_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, ("\"v2\"".into(), 1));
        let versions: i64 = sqlx::query_scalar("SELECT count(*) FROM item_versions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(versions, 2);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&refresh_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        server.abort();
    }
}
