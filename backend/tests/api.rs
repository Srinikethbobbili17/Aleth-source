use std::time::Duration;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use rss_backend::api::router;
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

fn unavailable_database() -> sqlx::PgPool {
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(100))
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap()
}

#[tokio::test]
async fn liveness_survives_database_failure_but_readiness_fails() {
    let app = router(unavailable_database());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health/live")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn invalid_url_and_pagination_are_rejected_before_database_access() {
    let app = router(unavailable_database());
    let request = Request::builder()
        .method("POST")
        .uri("/v1/feeds")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"url":"file:///etc/passwd"}"#))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for uri in [
        "/v1/feeds?limit=101",
        "/v1/feeds?after_id=-1",
        "/v1/items?limit=0",
        "/v1/items?feed_id=-1",
        "/v1/items?before_id=0",
        "/v1/items/-1",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[tokio::test]
async fn preview_parses_uploaded_content_without_database_access() {
    let body = serde_json::json!({"content": include_str!("fixtures/rss.xml")});
    let response = router(unavailable_database())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/feed-preview")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["entries"][0]["title"], "First article");
}

#[tokio::test]
async fn every_rejection_is_json_with_a_code() {
    let app = router(unavailable_database());
    let json = |method: &str, uri: &str, content_type: Option<&str>, body: &'static str| {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        request.body(Body::from(body)).unwrap()
    };
    for (request, status, code) in [
        (
            json("GET", "/v1/items/abc", None, ""),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            json("GET", "/v1/items?foo=1", None, ""),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            json("POST", "/v1/feeds", Some("application/json"), "{bad"),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            json(
                "POST",
                "/v1/feeds",
                Some("application/json"),
                r#"{"url":"https://a.test/f","x":1}"#,
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            json("POST", "/v1/feeds", None, r#"{"url":"https://a.test/f"}"#),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
        ),
        (
            json("GET", "/v1/nope", None, ""),
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            json("DELETE", "/v1/feeds/1", None, ""),
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        ),
    ] {
        let uri = request.uri().to_string();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), status, "{uri}");
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or_else(|_| panic!("{uri}: body is not JSON"));
        assert_eq!(body["code"], code, "{uri}");
        assert!(body["error"].is_string(), "{uri}");
    }
}

#[tokio::test]
async fn invalid_opml_is_rejected_before_database_access() {
    let app = router(unavailable_database());
    for (body, code) in [
        ("<rss/>", "invalid_opml"),
        ("not xml", "invalid_opml"),
        (
            r#"<!DOCTYPE opml [<!ENTITY x "y">]><opml/>"#,
            "unsupported_xml",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/imports/opml")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], code);
    }
}
