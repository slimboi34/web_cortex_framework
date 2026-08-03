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
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(strip_scheme(&cfg.url))
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(cfg.max_connections)
            .connect_with(opts)
            .await
            .map_err(|e| format!("database connect failed: {e}"))?;
        Ok(Self { pool })
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
    ) -> Result<Value, String> {
        if returns == QueryReturns::Affected {
            let mut q = sqlx::query(AssertSqlSafe(sql));
            for b in bindings {
                q = bind_value(q, b);
            }
            let res = q
                .execute(&self.pool)
                .await
                .map_err(|e| format!("query failed: {e}"))?;
            return Ok(serde_json::json!({
                "affected": res.rows_affected(),
                "last_insert_id": res.last_insert_rowid(),
            }));
        }

        let mut q = sqlx::query(AssertSqlSafe(sql));
        for b in bindings {
            q = bind_value(q, b);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| format!("query failed: {e}"))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(row_to_json(row)?);
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

fn row_to_json(row: &sqlx::sqlite::SqliteRow) -> Result<Value, String> {
    let mut map = Map::new();
    for (i, col) in row.columns().iter().enumerate() {
        let name = col.name().to_string();
        let value = decode_column(row, i, col.type_info().name());
        map.insert(name, value);
    }
    Ok(Value::Object(map))
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
