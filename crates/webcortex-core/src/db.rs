//! Database access for `Query` ops.
//!
//! The point of this module is that a CRUD endpoint should never wake the
//! interpreter. Python declares the SQL and the parameter names; Rust binds,
//! executes, and serialises rows straight to JSON bytes.

use crate::manifest::{DatabaseConfig, QueryReturns};
use serde_json::{Map, Value};
use sqlx::{AssertSqlSafe, Column, Row, TypeInfo, sqlite::SqlitePool};

// sqlx 0.9 only implicitly trusts `&'static str` as SQL; anything dynamic must
// be wrapped in `AssertSqlSafe`. Every use of that wrapper below is asserting
// the same invariant: SQL text originates from the developer-authored manifest,
// never from a request. Request data reaches the database exclusively through
// bound parameters, which is what makes the assertion sound.

pub struct Db {
    pool: SqlitePool,
}

impl Db {
    pub async fn connect(cfg: &DatabaseConfig) -> Result<Self, String> {
        let filename = strip_scheme(&cfg.url);
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(filename)
            .create_if_missing(true);
        let mut pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(cfg.max_connections);
        if filename == ":memory:" {
            // Every connection to `:memory:` opens a private, empty database
            // that vanishes when the connection closes. Hold exactly one for
            // the life of the pool, so every query sees the same tables.
            pool = pool
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None);
        }
        let pool = pool
            .connect_with(opts)
            .await
            .map_err(|e| format!("database connect failed: {e}"))?;
        let db = Self { pool };
        if !cfg.schema.trim().is_empty() {
            db.execute_raw(&cfg.schema).await?;
        }
        Ok(db)
    }

    /// Run one or more statements with no bindings. Used for schema setup.
    pub async fn execute_raw(&self, sql: &str) -> Result<(), String> {
        use sqlx::Executor;
        self.pool
            .execute(AssertSqlSafe(sql))
            .await
            .map(|_| ())
            .map_err(|e| format!("migration failed: {e}"))
    }

    pub async fn run(
        &self,
        sql: &str,
        bindings: &[Value],
        returns: QueryReturns,
    ) -> Result<Value, QueryError> {
        if returns == QueryReturns::Affected {
            let mut q = sqlx::query(AssertSqlSafe(sql));
            for b in bindings {
                q = bind_value(q, b);
            }
            let res = q.execute(&self.pool).await.map_err(QueryError::from_sqlx)?;
            return Ok(serde_json::json!({
                "affected": res.rows_affected(),
                "last_insert_id": res.last_insert_rowid(),
            }));
        }

        let mut q = sqlx::query(AssertSqlSafe(sql));
        for b in bindings {
            q = bind_value(q, b);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(QueryError::from_sqlx)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(row_to_json(row));
        }

        match returns {
            QueryReturns::One => Ok(out.into_iter().next().unwrap_or(Value::Null)),
            _ => Ok(Value::Array(out)),
        }
    }
}

fn strip_scheme(url: &str) -> &str {
    url.strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url)
}

type SqliteQuery<'a> = sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>;

fn bind_value<'a>(q: SqliteQuery<'a>, v: &'a Value) -> SqliteQuery<'a> {
    match v {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                q.bind(i)
            } else {
                q.bind(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => q.bind(s.as_str()),
        // Objects and arrays round-trip as JSON text, matching how SQLite's
        // json1 functions expect to receive them.
        other => q.bind(other.to_string()),
    }
}

fn row_to_json(row: &sqlx::sqlite::SqliteRow) -> Value {
    let mut map = Map::new();
    for (i, col) in row.columns().iter().enumerate() {
        let name = col.name().to_string();
        let value = decode_column(row, i, col.type_info().name());
        map.insert(name, value);
    }
    Value::Object(map)
}

/// Distinguishes "the caller sent something the query cannot use" from "the
/// database is broken". Conflating them turns a malformed query string into a
/// 500, which reads as a server fault and inflates error budgets.
#[derive(Debug)]
pub enum QueryError {
    /// Caused by the request's own data: a bad type, a constraint violation.
    BadInput(String),
    /// Anything else. The detail stays in the log.
    Internal(String),
}

impl QueryError {
    fn from_sqlx(e: sqlx::Error) -> Self {
        let text = e.to_string();
        let lowered = text.to_ascii_lowercase();
        let caller_fault = matches!(&e, sqlx::Error::Database(db) if {
            let msg = db.message().to_ascii_lowercase();
            msg.contains("constraint")
                || msg.contains("datatype mismatch")
                || msg.contains("not a")
                || msg.contains("no such column")
        }) || lowered.contains("datatype mismatch")
            || lowered.contains("constraint")
            || lowered.contains("error occurred while decoding")
            || lowered.contains("mismatched types");

        // Full detail is logged; the client is told only which side is at fault.
        tracing::warn!(error = %text, caller_fault, "query failed");
        if caller_fault {
            Self::BadInput("the request contained a value this query cannot use".into())
        } else {
            Self::Internal("query failed".into())
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::BadInput(_) => 400,
            Self::Internal(_) => 500,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::BadInput(m) | Self::Internal(m) => m,
        }
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// SQLite is dynamically typed, so the declared column type is a hint rather
/// than a guarantee. Try the declared type first, then fall back through the
/// other representations before giving up and returning null.
fn decode_column(row: &sqlx::sqlite::SqliteRow, i: usize, type_name: &str) -> Value {
    let upper = type_name.to_ascii_uppercase();
    if upper.contains("INT") {
        if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
            return v.map(Value::from).unwrap_or(Value::Null);
        }
    }
    if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        if let Ok(v) = row.try_get::<Option<f64>, _>(i) {
            return v.map(Value::from).unwrap_or(Value::Null);
        }
    }
    if upper.contains("BOOL") {
        if let Ok(v) = row.try_get::<Option<bool>, _>(i) {
            return v.map(Value::from).unwrap_or(Value::Null);
        }
    }
    if let Ok(v) = row.try_get::<Option<String>, _>(i) {
        return v.map(Value::from).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
        return v.map(Value::from).unwrap_or(Value::Null);
    }
    if let Ok(v) = row.try_get::<Option<f64>, _>(i) {
        return v.map(Value::from).unwrap_or(Value::Null);
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS books (id INTEGER PRIMARY KEY, title TEXT);\n\
                          CREATE TABLE IF NOT EXISTS authors (id INTEGER PRIMARY KEY, name TEXT);";

    fn config(url: &str) -> DatabaseConfig {
        DatabaseConfig { url: url.into(), max_connections: 16, schema: SCHEMA.into() }
    }

    #[tokio::test]
    async fn an_in_memory_database_gets_its_schema_and_keeps_it_across_queries() {
        let db = Db::connect(&config("sqlite://:memory:")).await.expect("connects");
        // Concurrent queries: a pool of private `:memory:` connections would
        // hand some of them a database without the table.
        let inserts = (0..8).map(|i| {
            let db = &db;
            async move {
                let title = json!(format!("t{i}"));
                db.run("INSERT INTO books (title) VALUES (?)", std::slice::from_ref(&title), QueryReturns::Affected)
                    .await
            }
        });
        for result in futures::future::join_all(inserts).await {
            result.expect("every insert sees the table");
        }
        let count = db.run("SELECT COUNT(*) AS n FROM books", &[], QueryReturns::One).await.expect("count");
        assert_eq!(count["n"], json!(8));
        // Every statement of the schema ran, not only the first.
        db.run("SELECT id, name FROM authors", &[], QueryReturns::Many).await.expect("second table exists");
    }

    #[tokio::test]
    async fn reapplying_the_schema_to_an_existing_file_keeps_its_rows() {
        let path = std::env::temp_dir().join(format!("webcortex-db-test-{}.sqlite", std::process::id()));
        let url = format!("sqlite://{}", path.display());
        {
            let db = Db::connect(&config(&url)).await.expect("first boot");
            db.run("INSERT INTO books (title) VALUES ('kept')", &[], QueryReturns::Affected).await.expect("insert");
            db.pool.close().await;
        }
        let db = Db::connect(&config(&url)).await.expect("second boot");
        let count = db.run("SELECT COUNT(*) AS n FROM books", &[], QueryReturns::One).await.expect("count");
        assert_eq!(count["n"], json!(1));
        db.pool.close().await;
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
