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

async fn import(app: &axum::Router, body: &'static str) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/imports/opml")
                .header("content-type", "text/x-opml")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
async fn opml_import_registers_new_feeds_and_reports_the_rest(pool: PgPool) {
    let existing = feeds::register(&pool, "https://xkcd.com/rss.xml")
        .await
        .unwrap();
    let app = router(pool.clone());
    let file = include_str!("fixtures/subscriptions.opml");

    let report = import(&app, file).await;
    assert_eq!(
        (&report["added"], &report["existing"], &report["duplicates"]),
        (&3.into(), &1.into(), &1.into())
    );
    assert_eq!(report["rejected"][0]["code"], "invalid_url");
    let xkcd = report["feeds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|feed| feed["url"] == "https://xkcd.com/rss.xml")
        .unwrap();
    assert_eq!(
        (&xkcd["id"], &xkcd["status"], &xkcd["folders"][0]),
        (&existing.id.into(), &"existing".into(), &"Comics".into())
    );

    // Imported feeds are due for polling right away.
    let due: i64 = sqlx::query_scalar("SELECT count(*) FROM feeds WHERE next_poll_at <= now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(due, 4);

    // Importing the same file again changes nothing.
    let again = import(&app, file).await;
    assert_eq!(
        (&again["added"], &again["existing"]),
        (&0.into(), &4.into())
    );
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM feeds")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 4);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires DATABASE_URL and PostgreSQL with CREATEDB"]
async fn opml_export_round_trips_through_import(pool: PgPool) {
    let app = router(pool.clone());
    import(&app, include_str!("fixtures/subscriptions.opml")).await;
    let urls = || async {
        let mut urls: Vec<String> = sqlx::query_scalar("SELECT url FROM feeds")
            .fetch_all(&pool)
            .await
            .unwrap();
        urls.sort();
        urls
    };
    let before = urls().await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/exports/opml")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/x-opml; charset=utf-8"
    );
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("subscriptions.opml")
    );
    let exported = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let exported: &'static str = String::from_utf8(exported.to_vec()).unwrap().leak();

    // Start over and import the export: the same feeds come back.
    sqlx::query("TRUNCATE feeds CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    let report = import(&app, exported).await;
    assert_eq!(report["added"], before.len());
    assert_eq!(report["rejected"].as_array().unwrap().len(), 0);
    assert_eq!(urls().await, before);
}
