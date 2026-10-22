use std::time::Duration;

use sqlx::{PgPool, postgres::PgPoolOptions};

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(3))
        .connect(database_url)
        .await
}

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

/// Logs a database failure by category. Error messages can echo query data
/// such as feed URLs, so only the kind, SQLSTATE, and constraint are recorded.
pub fn log_error(context: &'static str, error: &sqlx::Error) {
    let kind = match error {
        sqlx::Error::Database(_) => "database",
        sqlx::Error::PoolTimedOut => "pool_timed_out",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::Io(_) => "io",
        sqlx::Error::Tls(_) => "tls",
        sqlx::Error::Protocol(_) => "protocol",
        sqlx::Error::RowNotFound => "row_not_found",
        sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) => "decode",
        _ => "other",
    };
    let database = error.as_database_error();
    tracing::error!(
        context,
        kind,
        sqlstate = database.and_then(|e| e.code()).as_deref(),
        constraint = database.and_then(|e| e.constraint()),
        "database operation failed"
    );
}
