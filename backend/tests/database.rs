use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use rss_backend::{api::router, feeds};
use sqlx::PgPool;
use tower::ServiceExt;

// SQLx creates an isolated test database and applies migrations. Use a local
// PostgreSQL role with CREATEDB; never point these tests at production.
#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
async fn registration_is_idempotent_and_pagination_is_stable(pool: PgPool) {
    let first = feeds::register(&pool, "https://example.com/a.xml")
        .await
        .unwrap();
    let duplicate = feeds::register(&pool, "https://example.com/a.xml")
        .await
        .unwrap();
    assert_eq!(first.id, duplicate.id);
    let second = feeds::register(&pool, "https://example.com/b.xml")
        .await
        .unwrap();
    let app = router(pool);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/feeds?limit=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["feeds"].as_array().unwrap().len(), 1);
    assert_eq!(page["next_after_id"], first.id);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/feeds?after_id={}&limit=1", first.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["feeds"][0]["id"], second.id);
    assert!(page["next_after_id"].is_null());
}
