use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::{feeds, fetch::Fetcher, ingest, items};

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    parser_slots: Arc<Semaphore>,
    refresh_slots: Arc<Semaphore>,
    fetcher: Fetcher,
}

pub fn router(pool: PgPool) -> Router {
    router_with_fetcher(
        pool,
        Fetcher::new().expect("could not initialize feed HTTP client"),
    )
}

pub(crate) fn router_with_fetcher(pool: PgPool, fetcher: Fetcher) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/v1/feeds", post(register_feed).get(list_feeds))
        .route("/v1/feeds/{id}", get(get_feed))
        .route("/v1/feeds/{id}/refresh", post(refresh_feed))
        .route("/v1/items", get(list_items))
        .route("/v1/items/{id}", get(get_item))
        .route("/v1/feed-preview", post(preview_feed))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(AppState {
            pool,
            parser_slots: Arc::new(Semaphore::new(2)),
            refresh_slots: Arc::new(Semaphore::new(4)),
            fetcher,
        })
}

async fn get_feed(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<feeds::Feed>, ApiError> {
    positive_id(id)?;
    feeds::get(&state.pool, id)
        .await
        .map_err(ApiError::database)?
        .map(Json)
        .ok_or(ApiError(StatusCode::NOT_FOUND, "feed not found"))
}

async fn refresh_feed(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<ingest::RefreshReport>, ApiError> {
    positive_id(id)?;
    let _permit = state.refresh_slots.try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "refresh capacity busy; retry later",
        )
    })?;
    ingest::refresh(&state.pool, &state.fetcher, state.parser_slots, id)
        .await
        .map(Json)
        .map_err(|error| {
            use ingest::RefreshError;
            match error {
                RefreshError::NotFound => ApiError(StatusCode::NOT_FOUND, "feed not found"),
                RefreshError::Busy => ApiError(StatusCode::CONFLICT, "feed refresh or parser busy"),
                RefreshError::CoolingDown => ApiError(
                    StatusCode::TOO_MANY_REQUESTS,
                    "feed is cooling down; check next_fetch_at",
                ),
                RefreshError::LeaseExpired => {
                    ApiError(StatusCode::CONFLICT, "refresh lease expired; retry later")
                }
                RefreshError::Upstream(message) => ApiError(StatusCode::BAD_GATEWAY, message),
                RefreshError::Database => {
                    ApiError(StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
                }
            }
        })
}

fn positive_id(id: i64) -> Result<(), ApiError> {
    if id > 0 {
        Ok(())
    } else {
        Err(ApiError::invalid("id must be positive"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemQuery {
    feed_id: Option<i64>,
    before_id: Option<i64>,
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Serialize)]
struct ItemPage {
    items: Vec<items::ItemSummary>,
    next_before_id: Option<i64>,
}

async fn list_items(
    State(state): State<AppState>,
    Query(query): Query<ItemQuery>,
) -> Result<Json<ItemPage>, ApiError> {
    if query.feed_id.is_some_and(|id| id <= 0)
        || query.before_id.is_some_and(|id| id <= 0)
        || !(1..=100).contains(&query.limit)
    {
        return Err(ApiError::invalid(
            "IDs must be positive and limit must be between 1 and 100",
        ));
    }
    let mut items = items::list(&state.pool, query.feed_id, query.before_id, query.limit + 1)
        .await
        .map_err(ApiError::database)?;
    let has_more = items.len() > query.limit as usize;
    items.truncate(query.limit as usize);
    let next_before_id = if has_more {
        items.last().map(|item| item.id)
    } else {
        None
    };
    Ok(Json(ItemPage {
        items,
        next_before_id,
    }))
}

async fn get_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<items::ItemDetail>, ApiError> {
    positive_id(id)?;
    items::get(&state.pool, id)
        .await
        .map_err(ApiError::database)?
        .map(Json)
        .ok_or(ApiError(StatusCode::NOT_FOUND, "item not found"))
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn live() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    match tokio::time::timeout(
        Duration::from_secs(2),
        sqlx::query("SELECT 1").execute(&state.pool),
    )
    .await
    {
        Ok(Ok(_)) => (StatusCode::OK, Json(Health { status: "ready" })),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Health {
                status: "unavailable",
            }),
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterFeed {
    url: String,
}

async fn register_feed(
    State(state): State<AppState>,
    Json(input): Json<RegisterFeed>,
) -> Result<Json<feeds::Feed>, ApiError> {
    let url = feeds::normalize_feed_url(&input.url).map_err(ApiError::invalid)?;
    Ok(Json(
        feeds::register(&state.pool, &url)
            .await
            .map_err(ApiError::database)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedQuery {
    #[serde(default)]
    after_id: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

#[derive(Serialize)]
struct FeedPage {
    feeds: Vec<feeds::Feed>,
    next_after_id: Option<i64>,
}

async fn list_feeds(
    State(state): State<AppState>,
    Query(query): Query<FeedQuery>,
) -> Result<Json<FeedPage>, ApiError> {
    if query.after_id < 0 || !(1..=100).contains(&query.limit) {
        return Err(ApiError::invalid(
            "after_id must be nonnegative and limit must be between 1 and 100",
        ));
    }
    let mut feeds = feeds::list(&state.pool, query.after_id, query.limit + 1)
        .await
        .map_err(ApiError::database)?;
    let has_more = feeds.len() > query.limit as usize;
    feeds.truncate(query.limit as usize);
    let next_after_id = if has_more {
        feeds.last().map(|feed| feed.id)
    } else {
        None
    };
    Ok(Json(FeedPage {
        feeds,
        next_after_id,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewRequest {
    content: String,
}

async fn preview_feed(
    State(state): State<AppState>,
    Json(input): Json<PreviewRequest>,
) -> Result<Json<feeds::FeedPreview>, ApiError> {
    if input.content.len() > feeds::MAX_FEED_BYTES {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "feed content exceeds 1 MiB",
        ));
    }
    let permit = state
        .parser_slots
        .try_acquire_owned()
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "parser busy; retry later"))?;
    let parsed = tokio::task::spawn_blocking(move || {
        // Hold the slot until parsing ends even if the HTTP request is cancelled.
        let _permit = permit;
        feeds::preview(input.content.as_bytes())
    })
    .await
    .map_err(|_| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "parser failed"))?;
    Ok(Json(parsed.map_err(ApiError::invalid)?))
}

struct ApiError(StatusCode, &'static str);

impl ApiError {
    fn invalid(message: &'static str) -> Self {
        Self(StatusCode::UNPROCESSABLE_ENTITY, message)
    }

    fn database(_error: sqlx::Error) -> Self {
        // Database diagnostics may contain private URLs or query data.
        tracing::error!("database operation failed");
        Self(StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: &'static str,
        }
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}
