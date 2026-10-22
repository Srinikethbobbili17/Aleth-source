use std::{borrow::Cow, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, FromRequest, FromRequestParts, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::{failure::Failure, feeds, fetch::Fetcher, ingest, items};

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
        .fallback(route_not_found)
        .method_not_allowed_fallback(method_not_allowed)
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
    ApiPath(id): ApiPath<i64>,
) -> Result<Json<feeds::Feed>, ApiError> {
    positive_id(id)?;
    feeds::get(&state.pool, id)
        .await
        .map_err(ApiError::database)?
        .map(Json)
        .ok_or(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "feed not found",
        ))
}

async fn refresh_feed(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> Result<Json<ingest::RefreshReport>, ApiError> {
    positive_id(id)?;
    let _permit = state.refresh_slots.try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "capacity_busy",
            "refresh capacity busy; retry later",
        )
    })?;
    ingest::refresh(&state.pool, &state.fetcher, state.parser_slots, id)
        .await
        .map(Json)
        .map_err(|error| {
            use ingest::RefreshError;
            match error {
                RefreshError::NotFound => {
                    ApiError::new(StatusCode::NOT_FOUND, "not_found", "feed not found")
                }
                RefreshError::Busy => ApiError::new(
                    StatusCode::CONFLICT,
                    "refresh_busy",
                    "this feed is already being refreshed",
                ),
                RefreshError::ParserBusy => ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "parser_busy",
                    "parser capacity busy; retry later",
                ),
                RefreshError::CoolingDown => ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "cooling_down",
                    "feed is cooling down; check next_fetch_at",
                ),
                RefreshError::LeaseExpired => ApiError::new(
                    StatusCode::CONFLICT,
                    "lease_expired",
                    "refresh lease expired; retry later",
                ),
                RefreshError::Upstream(failure) => {
                    ApiError::failure(StatusCode::BAD_GATEWAY, failure)
                }
                RefreshError::Database => ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "database_unavailable",
                    "database unavailable",
                ),
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
    ApiQuery(query): ApiQuery<ItemQuery>,
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
    ApiPath(id): ApiPath<i64>,
) -> Result<Json<items::ItemDetail>, ApiError> {
    positive_id(id)?;
    items::get(&state.pool, id)
        .await
        .map_err(ApiError::database)?
        .map(Json)
        .ok_or(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "item not found",
        ))
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
    ApiJson(input): ApiJson<RegisterFeed>,
) -> Result<Json<feeds::Feed>, ApiError> {
    let url = feeds::normalize_feed_url(&input.url).map_err(|message| {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_url", message)
    })?;
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
    ApiQuery(query): ApiQuery<FeedQuery>,
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
    ApiJson(input): ApiJson<PreviewRequest>,
) -> Result<Json<feeds::FeedPreview>, ApiError> {
    if input.content.len() > feeds::MAX_PREVIEW_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "feed content exceeds 1 MiB",
        ));
    }
    let permit = state.parser_slots.try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "parser_busy",
            "parser busy; retry later",
        )
    })?;
    let parsed = tokio::task::spawn_blocking(move || {
        // Hold the slot until parsing ends even if the HTTP request is cancelled.
        let _permit = permit;
        feeds::preview(input.content.as_bytes())
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "parser failed",
        )
    })?;
    Ok(Json(parsed.map_err(|failure| {
        ApiError::failure(StatusCode::UNPROCESSABLE_ENTITY, failure)
    })?))
}

// Axum's extractors reject malformed input with plain-text bodies. These
// wrappers route those rejections through ApiError so every error is JSON.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
struct ApiJson<T>(T);

#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
struct ApiPath<T>(T);

#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
struct ApiQuery<T>(T);

async fn route_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found", "no such route")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method not allowed for this route",
    )
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: Cow<'static, str>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn invalid(message: &'static str) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", message)
    }

    fn rejected(status: StatusCode, message: String) -> Self {
        let code = match status {
            StatusCode::PAYLOAD_TOO_LARGE => "too_large",
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
            _ => "invalid_request",
        };
        Self::new(status, code, message)
    }

    fn failure(status: StatusCode, failure: Failure) -> Self {
        Self::new(status, failure.code(), failure.message)
    }

    fn database(error: sqlx::Error) -> Self {
        crate::db::log_error("api", &error);
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "database_unavailable",
            "database unavailable",
        )
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        Self::rejected(rejection.status(), rejection.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        Self::rejected(rejection.status(), rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::rejected(rejection.status(), rejection.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: Cow<'static, str>,
            code: &'static str,
        }
        let body = ErrorBody {
            error: self.message,
            code: self.code,
        };
        (self.status, Json(body)).into_response()
    }
}
