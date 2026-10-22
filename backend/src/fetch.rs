use std::{
    collections::HashMap,
    io,
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

use crate::feeds::{MAX_FEED_BYTES, normalize_feed_url};

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

#[derive(Clone, Copy, Debug)]
pub struct FetchError {
    pub message: &'static str,
    pub retry_after_seconds: Option<i64>,
}

impl FetchError {
    fn new(message: &'static str) -> Self {
        Self {
            message,
            retry_after_seconds: None,
        }
    }
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
    ) -> Result<FetchResult, FetchError> {
        // Includes waiting for slots, DNS, redirects, and streaming the body.
        tokio::time::timeout(Duration::from_secs(20), self.fetch_inner(input, validators))
            .await
            .map_err(|_| FetchError::new("feed fetch timed out"))?
    }

    async fn fetch_inner(
        &self,
        input: &str,
        validators: &Validators,
    ) -> Result<FetchResult, FetchError> {
        let _slot = self
            .slots
            .acquire()
            .await
            .map_err(|_| FetchError::new("fetcher unavailable"))?;
        let normalized = normalize_feed_url(input).map_err(FetchError::new)?;
        let mut url = Url::parse(&normalized).map_err(|_| FetchError::new("invalid feed URL"))?;
        for redirect in 0..=5 {
            validate_destination(&url)?;
            let host = url
                .host_str()
                .ok_or_else(|| FetchError::new("missing host"))?;
            let host_slots = {
                let mut hosts = self
                    .hosts
                    .lock()
                    .map_err(|_| FetchError::new("fetcher unavailable"))?;
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
            let _host_slot = host_slots
                .acquire()
                .await
                .map_err(|_| FetchError::new("fetcher unavailable"))?;
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
            let mut response = request
                .send()
                .await
                .map_err(|_| FetchError::new("feed request failed or destination blocked"))?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                if redirect == 5 {
                    return Err(FetchError::new("too many feed redirects"));
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| FetchError::new("invalid redirect location"))?;
                let next = url
                    .join(location)
                    .map_err(|_| FetchError::new("invalid redirect URL"))?;
                if url.scheme() == "https" && next.scheme() != "https" {
                    return Err(FetchError::new("HTTPS downgrade redirect blocked"));
                }
                let next = normalize_feed_url(next.as_str()).map_err(FetchError::new)?;
                url = Url::parse(&next).map_err(|_| FetchError::new("invalid redirect URL"))?;
                continue;
            }
            let received = Validators {
                url: Some(url.to_string()),
                etag: header_string(response.headers(), header::ETAG),
                last_modified: header_string(response.headers(), header::LAST_MODIFIED),
            };
            if status == StatusCode::NOT_MODIFIED {
                if !conditional {
                    return Err(FetchError::new("unexpected unconditional 304 response"));
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
                return Err(FetchError {
                    message: "publisher returned an unsuccessful HTTP status",
                    retry_after_seconds: if matches!(status.as_u16(), 429 | 503) {
                        header_string(response.headers(), header::RETRY_AFTER)
                            .and_then(|v| retry_after(&v))
                    } else {
                        None
                    },
                });
            }
            // Request identity encoding and reject unsolicited compressed responses.
            // This bounds both wire bytes and parser input without decompression bombs.
            if response
                .headers()
                .get(header::CONTENT_ENCODING)
                .is_some_and(|v| v != "identity")
            {
                return Err(FetchError::new(
                    "compressed feed response is not supported yet",
                ));
            }
            if response
                .content_length()
                .is_some_and(|n| n > MAX_FEED_BYTES as u64)
            {
                return Err(FetchError::new("feed response exceeds 1 MiB"));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| FetchError::new("feed body read failed"))?
            {
                if body.len().saturating_add(chunk.len()) > MAX_FEED_BYTES {
                    return Err(FetchError::new("feed response exceeds 1 MiB"));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(FetchResult::Modified {
                body,
                validators: received,
            });
        }
        Err(FetchError::new("too many feed redirects"))
    }
}

fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(2)
        .no_gzip().no_brotli().no_deflate().no_zstd()
        .user_agent(concat!("rss-backend/", env!("CARGO_PKG_VERSION")))
        .default_headers([
            (header::ACCEPT_ENCODING, header::HeaderValue::from_static("identity")),
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

fn validate_destination(url: &Url) -> Result<(), FetchError> {
    normalize_feed_url(url.as_str()).map_err(FetchError::new)?;
    // A deliberately small public-web port policy for the initial fetcher.
    if !matches!(url.port_or_known_default(), Some(80 | 443)) {
        return Err(FetchError::new("feed destination port must be 80 or 443"));
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) if !public_ip(IpAddr::V4(ip)) => {
            Err(FetchError::new("non-public feed destination blocked"))
        }
        Some(url::Host::Ipv6(ip)) if !public_ip(IpAddr::V6(ip)) => {
            Err(FetchError::new("non-public feed destination blocked"))
        }
        _ => Ok(()),
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

struct PublicResolver;

// Drop non-public answers instead of rejecting the host: DNS64 networks add
// 64:ff9b:: addresses beside public IPv4 ones. Only public addresses are dialed.
fn validated_addresses(addresses: Vec<SocketAddr>) -> Result<Addrs, io::Error> {
    let public: Vec<SocketAddr> = addresses
        .into_iter()
        .filter(|address| public_ip(address.ip()))
        .collect();
    if public.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "non-public DNS result blocked",
        ));
    }
    Ok(Box::new(public.into_iter()))
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addresses: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
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

    #[test]
    fn blocks_special_addresses_and_ports() {
        assert!(validated_addresses(vec![]).is_err());
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
                        "/compressed" => Response::builder()
                            .header("content-encoding", "gzip")
                            .body(Body::from("bytes"))
                            .unwrap(),
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
        for path in ["redirect", "large", "compressed", "unconditional"] {
            assert!(
                fetcher
                    .fetch(
                        &format!("http://fixture.test/{path}"),
                        &Validators::default()
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            fetcher
                .fetch("http://fixture.test/limited", &Validators::default())
                .await
                .unwrap_err()
                .retry_after_seconds,
            Some(120)
        );
        assert_eq!(seen.load(Ordering::SeqCst), 7);
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
                _ => assert!(report["error"].is_string()),
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
