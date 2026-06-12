//! `Recorder` — SQLite write path + query path.
//!
//! Wraps an `SqlitePool` and exposes three operations:
//! - [`Recorder::open`] — open (or create) the DB and apply schema.
//! - [`Recorder::record_state`] — persist a `StateChangedEvent`.
//! - [`Recorder::record_event`] — persist a `DomainEvent`.
//! - [`Recorder::get_state_history`] — read back rows in time order.
//!
//! State attributes are deduped via `fnv64a_hash` (see [`crate::dedup`]):
//! if an identical attributes blob was previously written its
//! `attributes_id` is reused and no new row is inserted.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::debug;

use homecore::entity::{EntityId, State};
use homecore::event::{DomainEvent, StateChangedEvent};

use crate::dedup::fnv64a_hash;
use crate::schema::ALL_DDL;

/// Errors returned by `Recorder` operations.
#[derive(Error, Debug)]
pub enum RecorderError {
    #[error("SQLite error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("serialisation error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("URL parse error: {0}")]
    UrlParse(String),
}

/// Trait for pluggable semantic (vector) indexing of state writes.
///
/// The no-op [`NullSemanticIndex`] is used in P1. P2 ships a ruvector-backed
/// implementation behind the `ruvector` feature flag.
///
/// ## P2 API change
///
/// The `insert_state` method now accepts a `state_id` (SQLite rowid) so the
/// HNSW index can map vector results back to SQLite rows. `search` embeds a
/// free-text query and returns `(state_id, score)` pairs.
#[async_trait]
pub trait SemanticIndex: Send + Sync {
    /// Insert an embedding for `state` keyed by its SQLite `state_id`.
    /// Called after the SQLite insert succeeds. Must not propagate errors
    /// back to the recorder — failure is logged, not fatal.
    async fn insert_state(
        &mut self,
        state_id: i64,
        state: &State,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Search for the `k` nearest states to the free-text `query`.
    /// Returns `(state_id, score)` pairs sorted by ascending distance.
    async fn search(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<(i64, f32)>, Box<dyn std::error::Error + Send + Sync>>;
}

/// No-op `SemanticIndex`. Used by default when the `ruvector` feature is off.
pub struct NullSemanticIndex;

#[async_trait]
impl SemanticIndex for NullSemanticIndex {
    async fn insert_state(
        &mut self,
        _state_id: i64,
        _state: &State,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn search(
        &self,
        _query: &str,
        _k: usize,
    ) -> Result<Vec<(i64, f32)>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![])
    }
}

/// The recorder. Cheap to clone (Arc-backed pool). Pass copies to the
/// `RecorderListener` and the API history handler.
///
/// The `semantic` field is wrapped in `Arc<RwLock<...>>` so that
/// `insert_state` (which takes `&mut self` on the trait) can be called
/// without requiring `&mut Recorder` from callers.
#[derive(Clone)]
pub struct Recorder {
    pool: SqlitePool,
    semantic: Arc<RwLock<dyn SemanticIndex>>,
}

impl Recorder {
    /// Open (or create) the SQLite database at `path` and apply the schema.
    ///
    /// Pass `"sqlite::memory:"` for an in-memory database (tests).
    ///
    /// The schema DDL uses `CREATE TABLE IF NOT EXISTS` so calling this on an
    /// existing database is safe.
    pub async fn open(path: &str) -> Result<Self, RecorderError> {
        Self::open_with_index(path, Arc::new(RwLock::new(NullSemanticIndex))).await
    }

    /// Open with a custom `SemanticIndex` (P2 entry point).
    pub async fn open_with_index(
        path: &str,
        semantic: Arc<RwLock<dyn SemanticIndex>>,
    ) -> Result<Self, RecorderError> {
        let options = path
            .parse::<SqliteConnectOptions>()
            .map_err(|e| RecorderError::UrlParse(e.to_string()))?
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        let recorder = Self { pool, semantic };
        recorder.apply_schema().await?;
        Ok(recorder)
    }

    /// Apply all DDL statements. Idempotent.
    async fn apply_schema(&self) -> Result<(), RecorderError> {
        for ddl in ALL_DDL {
            // Each DDL block may contain multiple statements separated by `;`.
            // sqlx::query does not support multi-statement strings directly,
            // so we split on the statement boundary and execute individually.
            for stmt in split_statements(ddl) {
                let stmt = stmt.trim();
                if !stmt.is_empty() {
                    sqlx::query(stmt).execute(&self.pool).await?;
                }
            }
        }
        Ok(())
    }

    /// Persist a `StateChangedEvent`. Inserts into `states` and dedupes into
    /// `state_attributes`. Returns the `state_id` of the new row.
    pub async fn record_state(
        &self,
        event: &StateChangedEvent,
    ) -> Result<Option<i64>, RecorderError> {
        let new_state = match &event.new_state {
            Some(s) => s,
            None => return Ok(None), // removal event — no row to insert
        };

        let attrs_json = serde_json::to_string(&new_state.attributes)?;
        let hash = fnv64a_hash(&attrs_json);

        // Upsert into state_attributes (dedup by hash).
        let attributes_id: i64 = {
            // Try to find an existing row first.
            let existing: Option<(i64,)> =
                sqlx::query_as("SELECT attributes_id FROM state_attributes WHERE hash = ?")
                    .bind(hash)
                    .fetch_optional(&self.pool)
                    .await?;

            if let Some((id,)) = existing {
                debug!(hash, id, "reusing existing state_attributes row");
                id
            } else {
                let result =
                    sqlx::query("INSERT INTO state_attributes (shared_attrs, hash) VALUES (?, ?)")
                        .bind(&attrs_json)
                        .bind(hash)
                        .execute(&self.pool)
                        .await?;
                result.last_insert_rowid()
            }
        };

        let context_id = new_state.context.id.to_string();
        let last_changed_ts = new_state.last_changed.timestamp_micros() as f64 / 1_000_000.0;
        let last_updated_ts = new_state.last_updated.timestamp_micros() as f64 / 1_000_000.0;

        let result = sqlx::query(
            "INSERT INTO states \
             (entity_id, state, attributes_id, last_changed_ts, last_updated_ts, context_id) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(new_state.entity_id.as_str())
        .bind(&new_state.state)
        .bind(attributes_id)
        .bind(last_changed_ts)
        .bind(last_updated_ts)
        .bind(&context_id)
        .execute(&self.pool)
        .await?;

        let state_id = result.last_insert_rowid();

        // Best-effort semantic indexing — failure is logged, not propagated.
        if let Err(e) = self
            .semantic
            .write()
            .await
            .insert_state(state_id, new_state)
            .await
        {
            tracing::warn!(
                error = %e,
                entity_id = %new_state.entity_id,
                "semantic indexing failed"
            );
        }

        Ok(Some(state_id))
    }

    /// Search for state history rows that semantically match `query`.
    ///
    /// When a vector [`SemanticIndex`] is wired (the `ruvector` feature), this
    /// uses the HNSW index to find the top-`k` nearest state embeddings and
    /// fetches the full `StateRow` for each, in ascending distance order.
    ///
    /// When the index yields no hits — e.g. the default [`NullSemanticIndex`]
    /// with no `ruvector` feature — it transparently falls back to the SQL
    /// text query [`search_states_by_text`](Self::search_states_by_text), so a
    /// caller always gets real matching rows rather than a silent empty `Vec`.
    pub async fn search_semantic(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<StateRow>, RecorderError> {
        let hits = self
            .semantic
            .read()
            .await
            .search(query, k)
            .await
            .unwrap_or_default();

        // No vector backend (or no embeddings indexed) → real SQL text search.
        if hits.is_empty() {
            return self.search_states_by_text(query, k).await;
        }

        let mut rows = Vec::with_capacity(hits.len());
        for (state_id, _score) in hits {
            if let Some(row) = self.fetch_state_row(state_id).await? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// Real text search over state history: returns the most recent up-to-`k`
    /// rows whose `entity_id`, `state` value, or attribute blob contains
    /// `query` (case-insensitive `LIKE`). Ordered newest-first.
    ///
    /// This is the feature-independent query path — it returns real rows from
    /// SQLite with no vector backend required. An empty `query` matches all
    /// rows (most-recent-first), giving callers a "latest activity" view.
    pub async fn search_states_by_text(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<StateRow>, RecorderError> {
        // Escape LIKE metacharacters so user text is treated literally.
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");

        let rows: Vec<(i64, String, String, Option<String>, f64, f64, Option<String>)> =
            sqlx::query_as(
                "SELECT s.state_id, s.entity_id, s.state, sa.shared_attrs, \
                        s.last_changed_ts, s.last_updated_ts, s.context_id \
                 FROM states s \
                 LEFT JOIN state_attributes sa ON s.attributes_id = sa.attributes_id \
                 WHERE ?1 = '' \
                    OR s.entity_id   LIKE ?2 ESCAPE '\\' \
                    OR s.state        LIKE ?2 ESCAPE '\\' \
                    OR sa.shared_attrs LIKE ?2 ESCAPE '\\' \
                 ORDER BY s.last_updated_ts DESC \
                 LIMIT ?3",
            )
            .bind(query)
            .bind(&pattern)
            .bind(k as i64)
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter()
            .map(|(state_id, entity_id, state, shared_attrs, last_changed_ts, last_updated_ts, context_id)| {
                let eid = EntityId::parse(&entity_id)
                    .unwrap_or_else(|_| EntityId::parse("unknown.unknown").unwrap());
                let attributes = shared_attrs
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                Ok(StateRow {
                    state_id,
                    entity_id: eid,
                    state,
                    attributes,
                    last_changed_ts,
                    last_updated_ts,
                    context_id,
                })
            })
            .collect()
    }

    /// Fetch a single `StateRow` by its `state_id`, joining attributes.
    async fn fetch_state_row(&self, state_id: i64) -> Result<Option<StateRow>, RecorderError> {
        let row: Option<(String, String, Option<String>, f64, f64, Option<String>)> =
            sqlx::query_as(
                "SELECT s.entity_id, s.state, sa.shared_attrs, \
                         s.last_changed_ts, s.last_updated_ts, s.context_id \
                 FROM states s \
                 LEFT JOIN state_attributes sa ON s.attributes_id = sa.attributes_id \
                 WHERE s.state_id = ?",
            )
            .bind(state_id)
            .fetch_optional(&self.pool)
            .await?;

        let Some((entity_id, state, shared_attrs, last_changed_ts, last_updated_ts, context_id)) =
            row
        else {
            return Ok(None);
        };

        let eid = EntityId::parse(&entity_id)
            .unwrap_or_else(|_| EntityId::parse("unknown.unknown").unwrap());
        let attributes = shared_attrs
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or(serde_json::Value::Object(Default::default()));
        Ok(Some(StateRow {
            state_id,
            entity_id: eid,
            state,
            attributes,
            last_changed_ts,
            last_updated_ts,
            context_id,
        }))
    }

    /// Persist a `DomainEvent`. Returns the `event_id`.
    pub async fn record_event(&self, event: &DomainEvent) -> Result<i64, RecorderError> {
        let data_json = serde_json::to_string(&event.event_data)?;
        let time_fired_ts = event.fired_at.timestamp_micros() as f64 / 1_000_000.0;
        let context_id = event.context.id.to_string();

        let result = sqlx::query(
            "INSERT INTO events (event_type, event_data, time_fired_ts, context_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&event.event_type)
        .bind(&data_json)
        .bind(time_fired_ts)
        .bind(&context_id)
        .execute(&self.pool)
        .await?;

        Ok(result.last_insert_rowid())
    }

    /// Query state history for `entity_id` between `since` and `until`.
    /// Returns state snapshots in ascending `last_updated_ts` order.
    pub async fn get_state_history(
        &self,
        entity_id: &EntityId,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<StateRow>, RecorderError> {
        let since_ts = since.timestamp_micros() as f64 / 1_000_000.0;
        let until_ts = until.timestamp_micros() as f64 / 1_000_000.0;

        let rows: Vec<(i64, String, Option<String>, f64, f64, Option<String>)> = sqlx::query_as(
            "SELECT s.state_id, s.state, sa.shared_attrs, \
                    s.last_changed_ts, s.last_updated_ts, s.context_id \
             FROM states s \
             LEFT JOIN state_attributes sa ON s.attributes_id = sa.attributes_id \
             WHERE s.entity_id = ? \
               AND s.last_updated_ts >= ? \
               AND s.last_updated_ts <= ? \
             ORDER BY s.last_updated_ts ASC",
        )
        .bind(entity_id.as_str())
        .bind(since_ts)
        .bind(until_ts)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|(state_id, state, shared_attrs, last_changed_ts, last_updated_ts, context_id)| {
                let attributes = shared_attrs
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?
                    .unwrap_or(serde_json::Value::Object(Default::default()));

                Ok(StateRow {
                    state_id,
                    entity_id: entity_id.clone(),
                    state,
                    attributes,
                    last_changed_ts,
                    last_updated_ts,
                    context_id,
                })
            })
            .collect()
    }
}

/// A state row returned from `get_state_history`.
#[derive(Debug, Clone)]
pub struct StateRow {
    pub state_id: i64,
    pub entity_id: EntityId,
    pub state: String,
    pub attributes: serde_json::Value,
    /// Unix timestamp (seconds, fractional) when the state string last changed.
    pub last_changed_ts: f64,
    /// Unix timestamp (seconds, fractional) when this snapshot was written.
    pub last_updated_ts: f64,
    pub context_id: Option<String>,
}

/// Split a multi-statement DDL string on `;` boundaries.
/// Trims whitespace; skips empty fragments.
fn split_statements(ddl: &str) -> impl Iterator<Item = &str> {
    ddl.split(';').map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;

    use homecore::entity::{EntityId, State};
    use homecore::event::{Context, DomainEvent, StateChangedEvent};

    use super::*;

    async fn open_memory() -> Recorder {
        Recorder::open("sqlite::memory:").await.expect("open in-memory DB")
    }

    fn entity(s: &str) -> EntityId {
        EntityId::parse(s).unwrap()
    }

    fn make_state_event(entity_id: &str, state_val: &str, attrs: serde_json::Value) -> StateChangedEvent {
        let eid = entity(entity_id);
        let ctx = Context::new();
        let s = Arc::new(State::new(eid.clone(), state_val, attrs, ctx));
        StateChangedEvent {
            entity_id: eid,
            old_state: None,
            new_state: Some(s),
            fired_at: Utc::now(),
        }
    }

    // ── schema ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn schema_applies_on_fresh_db() {
        let recorder = open_memory().await;
        // Verify all four tables exist by querying sqlite_master.
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&recorder.pool)
                .await
                .unwrap();
        let names: Vec<&str> = tables.iter().map(|(n,)| n.as_str()).collect();
        assert!(names.contains(&"state_attributes"), "missing state_attributes");
        assert!(names.contains(&"states"), "missing states");
        assert!(names.contains(&"events"), "missing events");
        assert!(names.contains(&"recorder_runs"), "missing recorder_runs");
    }

    #[tokio::test]
    async fn schema_idempotent_double_open() {
        // Applying schema twice (on the same pool) must not panic or error.
        let recorder = open_memory().await;
        recorder.apply_schema().await.expect("second apply_schema must be a no-op");
    }

    // ── record_state ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn record_state_inserts_row() {
        let recorder = open_memory().await;
        let event = make_state_event("light.kitchen", "on", serde_json::json!({"brightness": 200}));

        let state_id = recorder.record_state(&event).await.unwrap();
        assert!(state_id.is_some(), "expected a state_id");

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM states WHERE entity_id = 'light.kitchen'")
                .fetch_one(&recorder.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn removal_event_returns_none() {
        let recorder = open_memory().await;
        let event = StateChangedEvent {
            entity_id: entity("light.kitchen"),
            old_state: None,
            new_state: None, // removal
            fired_at: Utc::now(),
        };
        let result = recorder.record_state(&event).await.unwrap();
        assert!(result.is_none(), "removal event should yield None state_id");
    }

    // ── attribute deduplication ────────────────────────────────────────────────

    #[tokio::test]
    async fn same_attrs_dedup_to_one_row() {
        let recorder = open_memory().await;
        let attrs = serde_json::json!({"brightness": 200, "color_temp": 4000});

        let e1 = make_state_event("light.a", "on", attrs.clone());
        let e2 = make_state_event("light.b", "on", attrs.clone());

        recorder.record_state(&e1).await.unwrap();
        recorder.record_state(&e2).await.unwrap();

        let attr_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM state_attributes")
                .fetch_one(&recorder.pool)
                .await
                .unwrap();
        // Both events share identical attrs → only one state_attributes row.
        assert_eq!(attr_count.0, 1, "identical attrs must share one state_attributes row");

        let state_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM states")
                .fetch_one(&recorder.pool)
                .await
                .unwrap();
        assert_eq!(state_count.0, 2, "two states rows expected");
    }

    #[tokio::test]
    async fn different_attrs_each_get_own_row() {
        let recorder = open_memory().await;
        let e1 = make_state_event("sensor.a", "20", serde_json::json!({"unit": "C"}));
        let e2 = make_state_event("sensor.b", "20", serde_json::json!({"unit": "F"}));

        recorder.record_state(&e1).await.unwrap();
        recorder.record_state(&e2).await.unwrap();

        let attr_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM state_attributes")
                .fetch_one(&recorder.pool)
                .await
                .unwrap();
        assert_eq!(attr_count.0, 2);
    }

    // ── get_state_history ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn history_returns_rows_in_time_order() {
        let recorder = open_memory().await;
        let eid = entity("sensor.temp");

        // Insert three states with slightly different timestamps by sleeping.
        for val in &["20.0", "21.0", "22.0"] {
            let e = make_state_event("sensor.temp", val, serde_json::json!({}));
            recorder.record_state(&e).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        let since = Utc::now() - chrono::Duration::seconds(10);
        let until = Utc::now() + chrono::Duration::seconds(10);
        let rows = recorder.get_state_history(&eid, since, until).await.unwrap();

        assert_eq!(rows.len(), 3, "expected 3 history rows");
        // Verify ascending order by last_updated_ts.
        for w in rows.windows(2) {
            assert!(
                w[0].last_updated_ts <= w[1].last_updated_ts,
                "rows must be in ascending time order"
            );
        }
        assert_eq!(rows[0].state, "20.0");
        assert_eq!(rows[2].state, "22.0");
    }

    // ── record_event ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn record_event_round_trips() {
        let recorder = open_memory().await;
        let ctx = Context::new();
        let event = DomainEvent::new(
            "call_service",
            serde_json::json!({"domain": "light", "service": "turn_on"}),
            ctx,
        );

        let event_id = recorder.record_event(&event).await.unwrap();
        assert!(event_id > 0);

        let row: (String, String) =
            sqlx::query_as("SELECT event_type, event_data FROM events WHERE event_id = ?")
                .bind(event_id)
                .fetch_one(&recorder.pool)
                .await
                .unwrap();

        assert_eq!(row.0, "call_service");
        let data: serde_json::Value = serde_json::from_str(&row.1).unwrap();
        assert_eq!(data["domain"], "light");
    }

    // ── search_states_by_text (real DB query) ───────────────────────────────────

    #[tokio::test]
    async fn text_search_returns_inserted_rows() {
        // FAILS against the old always-empty path: asserts real rows come back.
        let recorder = open_memory().await;
        recorder
            .record_state(&make_state_event("light.kitchen", "on", serde_json::json!({})))
            .await
            .unwrap();
        recorder
            .record_state(&make_state_event("light.bedroom", "off", serde_json::json!({})))
            .await
            .unwrap();
        recorder
            .record_state(&make_state_event("switch.fan", "on", serde_json::json!({})))
            .await
            .unwrap();

        // Match by entity_id substring.
        let rows = recorder.search_states_by_text("kitchen", 10).await.unwrap();
        assert_eq!(rows.len(), 1, "exactly one kitchen row");
        assert_eq!(rows[0].entity_id.as_str(), "light.kitchen");

        // Match by domain prefix → both lights.
        let lights = recorder.search_states_by_text("light.", 10).await.unwrap();
        assert_eq!(lights.len(), 2, "both light rows");

        // Match by state value.
        let on_rows = recorder.search_states_by_text("on", 10).await.unwrap();
        // "on" matches light.kitchen (state on) and switch.fan (state on);
        // "bedroom" has state "off" — substring "on" not present in its
        // entity_id/state. Two rows expected.
        assert_eq!(on_rows.len(), 2, "two rows with state 'on'");
    }

    #[tokio::test]
    async fn text_search_matches_attribute_blob() {
        let recorder = open_memory().await;
        recorder
            .record_state(&make_state_event(
                "sensor.weather",
                "cloudy",
                serde_json::json!({"location": "portland"}),
            ))
            .await
            .unwrap();
        let rows = recorder.search_states_by_text("portland", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entity_id.as_str(), "sensor.weather");
        assert_eq!(rows[0].attributes["location"], "portland");
    }

    #[tokio::test]
    async fn text_search_empty_query_returns_recent_rows() {
        let recorder = open_memory().await;
        for v in &["1", "2", "3"] {
            recorder
                .record_state(&make_state_event("counter.c", v, serde_json::json!({})))
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        }
        // Empty query → all rows, newest first, capped at k.
        let rows = recorder.search_states_by_text("", 2).await.unwrap();
        assert_eq!(rows.len(), 2, "k caps the result set");
        assert_eq!(rows[0].state, "3", "newest first");
        assert_eq!(rows[1].state, "2");
    }

    #[tokio::test]
    async fn text_search_no_match_returns_empty() {
        let recorder = open_memory().await;
        recorder
            .record_state(&make_state_event("light.kitchen", "on", serde_json::json!({})))
            .await
            .unwrap();
        let rows = recorder
            .search_states_by_text("nonexistent_entity_xyz", 10)
            .await
            .unwrap();
        assert!(rows.is_empty(), "genuine no-match is empty, not an error");
    }

    #[tokio::test]
    async fn search_semantic_falls_back_to_text_with_null_index() {
        // With the default NullSemanticIndex, search_semantic must STILL return
        // real rows via the text fallback — proving it's no longer always-empty.
        let recorder = open_memory().await;
        recorder
            .record_state(&make_state_event("light.kitchen", "on", serde_json::json!({})))
            .await
            .unwrap();
        let rows = recorder.search_semantic("kitchen", 5).await.unwrap();
        assert_eq!(rows.len(), 1, "fallback must surface the kitchen row");
        assert_eq!(rows[0].entity_id.as_str(), "light.kitchen");
    }
}
