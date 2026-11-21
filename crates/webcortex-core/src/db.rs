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
                q = bind_value(q, b)?;
            }
            // `last_insert_rowid` is per connection and outlives the statement
            // that set it, so on a pooled connection an UPDATE reported the id
            // some earlier INSERT made, maybe into another table. Read it
            // before and after, on the one connection the statement runs on.
            let mut conn = self.pool.acquire().await.map_err(QueryError::from_sqlx)?;
            let before: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
                .fetch_one(&mut *conn)
                .await
                .map_err(QueryError::from_sqlx)?;
            let res = q.execute(&mut *conn).await.map_err(QueryError::from_sqlx)?;
            let id = inserted_rowid(sql, res.rows_affected(), before, res.last_insert_rowid());
            return Ok(serde_json::json!({
                "affected": res.rows_affected(),
                "last_insert_id": id,
            }));
        }

        let mut q = sqlx::query(AssertSqlSafe(sql));
        for b in bindings {
            q = bind_value(q, b)?;
        }
        // `returns="one"` stops at the first row instead of materialising
        // every row the statement matches and keeping one.
        if returns == QueryReturns::One {
            let row = q.fetch_optional(&self.pool).await.map_err(QueryError::from_sqlx)?;
            return Ok(row.as_ref().map(row_to_json).unwrap_or(Value::Null));
        }
        let rows = q.fetch_all(&self.pool).await.map_err(QueryError::from_sqlx)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(row_to_json(row));
        }

        Ok(Value::Array(out))
    }
}

fn strip_scheme(url: &str) -> &str {
    url.strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url)
}

type SqliteQuery<'a> = sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>;

/// 2^63: the first integer magnitude an `i64` cannot hold.
const I64_LIMIT: f64 = 9_223_372_036_854_775_808.0;

fn bind_value<'a>(q: SqliteQuery<'a>, v: &'a Value) -> Result<SqliteQuery<'a>, QueryError> {
    Ok(match v {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                q.bind(i)
            } else {
                // A float is bound as one. An integer SQLite cannot store —
                // above i64::MAX, or a whole number of 2^63 or more that JSON
                // parsing could only hold as a float — is refused: binding it
                // as a float stored a rounded number and answered 200. (The
                // parsed body cannot tell `1e19` from the integer it equals,
                // so a float of that size is refused too.)
                let f = n.as_f64().unwrap_or(f64::NAN);
                if n.is_u64() || (f.fract() == 0.0 && f.abs() >= I64_LIMIT) {
                    tracing::warn!(value = %n, caller_fault = true, "query failed: an integer outside the 64-bit range");
                    return Err(QueryError::BadInput("the request contained a value this query cannot use".into()));
                }
                q.bind(f)
            }
        }
        Value::String(s) => q.bind(s.as_str()),
        // Objects and arrays round-trip as JSON text, matching how SQLite's
        // json1 functions expect to receive them.
        other => q.bind(other.to_string()),
    })
}

/// The `last_insert_id` an `affected` query reports: the row the statement
/// inserted, or `None` when it inserted none.
///
/// Only a top-level INSERT (or REPLACE) changes `last_insert_rowid`, so a
/// changed value is the new row. An unchanged one is either a statement that
/// inserted nothing into a rowid table (an UPDATE, a DELETE, an upsert that
/// took its `DO UPDATE` path) or a plain INSERT whose new row happens to have
/// the same rowid as the connection's previous insert (two tables both at
/// id 1); the statement's own first keyword tells them apart.
fn inserted_rowid(sql: &str, affected: u64, before: i64, after: i64) -> Option<i64> {
    if affected == 0 {
        return None;
    }
    if after != before {
        return Some(after);
    }
    (after != 0 && statement_kind(sql) == StatementKind::Insert).then_some(after)
}

#[derive(Debug, PartialEq, Eq)]
enum StatementKind {
    /// A plain INSERT or REPLACE.
    Insert,
    /// INSERT … ON CONFLICT … DO UPDATE.
    Upsert,
    Other,
}

/// The kind of the first statement in `sql`, from its top-level keywords:
/// comments, quoted text and anything in parentheses (a CTE body, a column
/// list) are skipped, and a leading `WITH …` clause is looked through.
fn statement_kind(sql: &str) -> StatementKind {
    let mut words: Vec<String> = Vec::new();
    let mut chars = sql.chars().peekable();
    let mut depth = 0usize;
    let mut word = String::new();
    fn flush(word: &mut String, depth: usize, words: &mut Vec<String>) {
        if !word.is_empty() {
            if depth == 0 {
                words.push(word.to_ascii_uppercase());
            }
            word.clear();
        }
    }
    while let Some(c) = chars.next() {
        match c {
            '-' if chars.peek() == Some(&'-') => {
                flush(&mut word, depth, &mut words);
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                flush(&mut word, depth, &mut words);
                chars.next();
                let mut star = false;
                for n in chars.by_ref() {
                    if star && n == '/' {
                        break;
                    }
                    star = n == '*';
                }
            }
            '\'' | '"' | '`' | '[' => {
                flush(&mut word, depth, &mut words);
                let close = if c == '[' { ']' } else { c };
                for n in chars.by_ref() {
                    if n == close {
                        break;
                    }
                }
            }
            '(' => {
                flush(&mut word, depth, &mut words);
                depth += 1;
            }
            ')' => {
                flush(&mut word, depth, &mut words);
                depth = depth.saturating_sub(1);
            }
            ';' => break,
            c if c.is_alphanumeric() || c == '_' => word.push(c),
            _ => flush(&mut word, depth, &mut words),
        }
    }
    flush(&mut word, depth, &mut words);
    let verbs = ["INSERT", "REPLACE", "UPDATE", "DELETE", "SELECT", "VALUES"];
    let verb = match words.first().map(String::as_str) {
        Some("WITH") => words.iter().position(|w| verbs.contains(&w.as_str())),
        Some(_) => Some(0),
        None => None,
    };
    match verb.map(|i| (i, words[i].as_str())) {
        Some((i, "INSERT" | "REPLACE")) => {
            let upsert = words[i..].windows(2).any(|w| w[0] == "DO" && w[1] == "UPDATE");
            if upsert { StatementKind::Upsert } else { StatementKind::Insert }
        }
        _ => StatementKind::Other,
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

    pub fn code(&self) -> &'static str {
        match self {
            Self::BadInput(_) => "WC211",
            Self::Internal(_) => "WC001",
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
    if upper.contains("INT")
        && let Ok(v) = row.try_get::<Option<i64>, _>(i) {
            return v.map(Value::from).unwrap_or(Value::Null);
        }
    if (upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB"))
        && let Ok(v) = row.try_get::<Option<f64>, _>(i) {
            return v.map(Value::from).unwrap_or(Value::Null);
        }
    if upper.contains("BOOL")
        && let Ok(v) = row.try_get::<Option<bool>, _>(i) {
            return v.map(Value::from).unwrap_or(Value::Null);
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
    async fn an_integer_sqlite_cannot_hold_is_refused_not_rounded() {
        let db = Db::connect(&config("sqlite://:memory:")).await.expect("connects");
        db.execute_raw("CREATE TABLE n (id INTEGER PRIMARY KEY, v INTEGER, r REAL)").await.unwrap();
        for big in [json!(u64::MAX), json!(9_223_372_036_854_775_808u64), json!(1.2345678901234568e29),
                    json!(-9_223_372_036_854_775_809.0f64),
                    // The parsed body cannot tell 6.02e23 from the 24-digit integer it equals,
                    // so every number of magnitude 2^63 or more is refused alike.
                    json!(6.02e23)] {
            let err = db.run("INSERT INTO n (v) VALUES (?)", std::slice::from_ref(&big), QueryReturns::Affected).await
                .expect_err("refused");
            assert_eq!((err.status(), err.code()), (400, "WC211"), "{big}");
            assert!(err.message().contains("cannot use"), "{err}");
        }
        // What fits is stored exactly; real floats are still floats.
        for (v, r) in [(json!(i64::MAX), json!(0.5)), (json!(i64::MIN), json!(-2.75)), (json!(3), json!(1e18))] {
            db.run("INSERT INTO n (v, r) VALUES (?, ?)", &[v.clone(), r.clone()], QueryReturns::Affected).await.unwrap();
            let row = db.run("SELECT v, r FROM n ORDER BY id DESC LIMIT 1", &[], QueryReturns::One).await.unwrap();
            assert_eq!((row["v"].clone(), row["r"].clone()), (v, r));
        }
        assert_eq!(db.run("SELECT COUNT(*) AS c FROM n", &[], QueryReturns::One).await.unwrap()["c"], json!(3));
    }

    #[tokio::test]
    async fn last_insert_id_is_the_row_this_statement_inserted_or_null() {
        let db = Db::connect(&config("sqlite://:memory:")).await.expect("connects");
        db.execute_raw("CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER)").await.unwrap();
        let run = |sql: &'static str, args: Vec<Value>| {
            let db = &db;
            async move { db.run(sql, &args, QueryReturns::Affected).await.unwrap() }
        };
        let first = run("INSERT INTO books (title) VALUES (?)", vec![json!("a")]).await;
        assert_eq!(first, json!({"affected": 1, "last_insert_id": 1}));
        // An UPDATE inserts nothing: no id, not the INSERT's before it.
        let upd = run("UPDATE books SET title = ? WHERE id = ?", vec![json!("b"), json!(1)]).await;
        assert_eq!(upd, json!({"affected": 1, "last_insert_id": null}));
        let none = run("INSERT OR IGNORE INTO books (id, title) VALUES (?, ?)", vec![json!(1), json!("dup")]).await;
        assert_eq!(none, json!({"affected": 0, "last_insert_id": null}));
        let del = run("DELETE FROM books WHERE id = ?", vec![json!(99)]).await;
        assert_eq!(del, json!({"affected": 0, "last_insert_id": null}));
        // Another table's first row has the same rowid as the last insert: still reported.
        let other = run("INSERT INTO authors (name) VALUES (?)", vec![json!("x")]).await;
        assert_eq!(other, json!({"affected": 1, "last_insert_id": 1}));
        let second = run("  -- a comment\n insert into books (title) values ('c')", vec![]).await;
        assert_eq!(second["last_insert_id"], json!(2));
        // An upsert that updates leaves the id unchanged: not reported.
        let up = "INSERT INTO books (id, title) VALUES (?, ?) ON CONFLICT(id) DO UPDATE SET title = excluded.title";
        assert_eq!(run(up, vec![json!(2), json!("d")]).await, json!({"affected": 1, "last_insert_id": null}));
        assert_eq!(run(up, vec![json!(7), json!("e")]).await, json!({"affected": 1, "last_insert_id": 7}));
        let cte = run("WITH t(x) AS (SELECT 'f') INSERT INTO books (title) SELECT x FROM t", vec![]).await;
        assert_eq!(cte["last_insert_id"], json!(8));
        let cte_upd = run("WITH t(x) AS (SELECT 'g') UPDATE books SET title = (SELECT x FROM t) WHERE id = 8", vec![]).await;
        assert_eq!(cte_upd["last_insert_id"], Value::Null);
    }

    #[test]
    fn statement_kinds_are_read_from_top_level_keywords() {
        use StatementKind::*;
        for (sql, kind) in [
            ("INSERT INTO t VALUES (1)", Insert),
            ("replace into t values (1)", Insert),
            ("/* x */ INSERT INTO t VALUES (1)", Insert),
            ("INSERT INTO \"update\" (a) VALUES ('do update')", Insert),
            ("INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = 2", Upsert),
            ("INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING", Insert),
            ("WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x", Insert),
            ("WITH x AS (INSERT INTO z VALUES (1)) UPDATE t SET a = 1", Other),
            ("UPDATE t SET a = 1", Other),
            ("DELETE FROM t", Other),
            ("", Other),
        ] {
            assert_eq!(statement_kind(sql), kind, "{sql}");
        }
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
