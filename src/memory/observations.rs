//! Observations storage layer for the intelligent memory system.
//!
//! Stores structured observations extracted from conversation sessions,
//! along with session lifecycle metadata. Uses the same brain.db file
//! as the main memory backend (additive tables, zero migration risk).

use anyhow::Context;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A stored observation extracted from a conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: i64,
    pub session_id: String,
    pub content: String,
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub priority: u8,
    pub importance: f64,
    pub source_file: Option<String>,
    pub created_at: String,
    pub consolidated: bool,
}

/// Input for storing a new observation (no id/created_at yet).
#[derive(Debug, Clone)]
pub struct NewObservation {
    pub session_id: String,
    pub content: String,
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub priority: u8,
    pub importance: f64,
    pub source_file: Option<String>,
}

/// Open an independent connection to brain.db for observations.
/// Uses WAL mode for concurrent read/write safety.
pub fn open_observations_db(workspace_dir: &Path) -> anyhow::Result<Connection> {
    let db_path = workspace_dir.join("memory").join("brain.db");

    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn =
        Connection::open(&db_path).context("Failed to open brain.db for observations")?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous  = NORMAL;
         PRAGMA temp_store   = MEMORY;",
    )?;

    init_observations_schema(&conn)?;
    Ok(conn)
}

/// Initialize observations and session_log tables (idempotent).
/// Called from both `open_observations_db` and `SqliteMemory::init_schema`.
pub fn init_observations_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "-- Observations table
        CREATE TABLE IF NOT EXISTS observations (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id   TEXT NOT NULL,
            content      TEXT NOT NULL,
            entities     TEXT NOT NULL DEFAULT '[]',
            topics       TEXT NOT NULL DEFAULT '[]',
            priority     INTEGER NOT NULL DEFAULT 3,
            importance   REAL NOT NULL DEFAULT 0.5,
            source_file  TEXT,
            created_at   TEXT NOT NULL,
            consolidated INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_obs_session ON observations(session_id);
        CREATE INDEX IF NOT EXISTS idx_obs_priority ON observations(priority);
        CREATE INDEX IF NOT EXISTS idx_obs_created ON observations(created_at);

        -- FTS5 full-text search for observations
        CREATE VIRTUAL TABLE IF NOT EXISTS observations_fts USING fts5(
            content, content='observations', content_rowid='id'
        );

        -- FTS triggers: keep in sync
        CREATE TRIGGER IF NOT EXISTS obs_fts_ai AFTER INSERT ON observations BEGIN
            INSERT INTO observations_fts(rowid, content)
            VALUES (new.id, new.content);
        END;
        CREATE TRIGGER IF NOT EXISTS obs_fts_ad AFTER DELETE ON observations BEGIN
            INSERT INTO observations_fts(observations_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
        END;
        CREATE TRIGGER IF NOT EXISTS obs_fts_au AFTER UPDATE ON observations BEGIN
            INSERT INTO observations_fts(observations_fts, rowid, content)
            VALUES ('delete', old.id, old.content);
            INSERT INTO observations_fts(rowid, content)
            VALUES (new.id, new.content);
        END;

        -- Session lifecycle log
        CREATE TABLE IF NOT EXISTS session_log (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL UNIQUE,
            channel    TEXT NOT NULL DEFAULT 'webchat',
            started_at TEXT NOT NULL,
            ended_at   TEXT,
            turn_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_session_started ON session_log(started_at);",
    )?;
    Ok(())
}

/// Store a single observation. Returns the new row id.
pub fn store_observation(conn: &Connection, obs: &NewObservation) -> anyhow::Result<i64> {
    let entities_json = serde_json::to_string(&obs.entities)?;
    let topics_json = serde_json::to_string(&obs.topics)?;
    let now = Utc::now().to_rfc3339();
    let priority = obs.priority.clamp(1, 4) as i32;
    let importance = obs.importance.clamp(0.0, 1.0);

    conn.execute(
        "INSERT INTO observations (session_id, content, entities, topics, priority, importance, source_file, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            obs.session_id,
            obs.content,
            entities_json,
            topics_json,
            priority,
            importance,
            obs.source_file,
            now,
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Get unconsolidated observations (for future consolidation passes).
pub fn get_unconsolidated(conn: &Connection, limit: usize) -> anyhow::Result<Vec<Observation>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, content, entities, topics, priority, importance, source_file, created_at, consolidated
         FROM observations WHERE consolidated = 0 ORDER BY created_at ASC LIMIT ?1",
    )?;

    let rows = stmt.query_map(params![limit as i64], |row| {
        Ok(ObservationRow {
            id: row.get(0)?,
            session_id: row.get(1)?,
            content: row.get(2)?,
            entities: row.get(3)?,
            topics: row.get(4)?,
            priority: row.get(5)?,
            importance: row.get(6)?,
            source_file: row.get(7)?,
            created_at: row.get(8)?,
            consolidated: row.get(9)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        let r = row?;
        result.push(parse_observation_row(r));
    }
    Ok(result)
}

/// Full-text search over observations using FTS5.
pub fn search_observations(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<Observation>> {
    let mut stmt = conn.prepare(
        "SELECT o.id, o.session_id, o.content, o.entities, o.topics, o.priority, o.importance, o.source_file, o.created_at, o.consolidated
         FROM observations o
         JOIN observations_fts f ON o.id = f.rowid
         WHERE observations_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![query, limit as i64], |row| {
        Ok(ObservationRow {
            id: row.get(0)?,
            session_id: row.get(1)?,
            content: row.get(2)?,
            entities: row.get(3)?,
            topics: row.get(4)?,
            priority: row.get(5)?,
            importance: row.get(6)?,
            source_file: row.get(7)?,
            created_at: row.get(8)?,
            consolidated: row.get(9)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        let r = row?;
        result.push(parse_observation_row(r));
    }
    Ok(result)
}

/// Record session start.
pub fn start_session(conn: &Connection, session_id: &str, channel: &str) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO session_log (session_id, channel, started_at) VALUES (?1, ?2, ?3)",
        params![session_id, channel, now],
    )?;
    Ok(())
}

/// Record session end with turn count.
pub fn end_session(conn: &Connection, session_id: &str, turn_count: u32) -> anyhow::Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE session_log SET ended_at = ?1, turn_count = ?2 WHERE session_id = ?3",
        params![now, turn_count, session_id],
    )?;
    Ok(())
}

/// Count total observations in the database.
pub fn count_observations(conn: &Connection) -> anyhow::Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))?;
    Ok(count as usize)
}

// ── Consolidation helpers ────────────────────────────────────────

/// Mark observations as consolidated (processed by the consolidation pipeline).
pub fn mark_consolidated(conn: &Connection, ids: &[i64]) -> anyhow::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders: Vec<&str> = ids.iter().map(|_| "?").collect();
    let sql = format!(
        "UPDATE observations SET consolidated = 1 WHERE id IN ({})",
        placeholders.join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let updated = stmt.execute(params.as_slice())?;
    Ok(updated)
}

/// Delete observations by id (used by consolidation to remove duplicates/obsolete entries).
pub fn delete_observations(conn: &Connection, ids: &[i64]) -> anyhow::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders: Vec<&str> = ids.iter().map(|_| "?").collect();
    let sql = format!(
        "DELETE FROM observations WHERE id IN ({})",
        placeholders.join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let deleted = stmt.execute(params.as_slice())?;
    Ok(deleted)
}

/// Count observations that have not yet been processed by consolidation.
pub fn count_unconsolidated(conn: &Connection) -> anyhow::Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM observations WHERE consolidated = 0",
        [],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

// ── Internal helpers ──────────────────────────────────────────────

/// Raw row from SQLite (entities/topics as JSON strings).
struct ObservationRow {
    id: i64,
    session_id: String,
    content: String,
    entities: String,
    topics: String,
    priority: i32,
    importance: f64,
    source_file: Option<String>,
    created_at: String,
    consolidated: i32,
}

fn parse_observation_row(r: ObservationRow) -> Observation {
    let entities: Vec<String> = serde_json::from_str(&r.entities).unwrap_or_default();
    let topics: Vec<String> = serde_json::from_str(&r.topics).unwrap_or_default();
    Observation {
        id: r.id,
        session_id: r.session_id,
        content: r.content,
        entities,
        topics,
        priority: r.priority.clamp(1, 4) as u8,
        importance: r.importance,
        source_file: r.source_file,
        created_at: r.created_at,
        consolidated: r.consolidated != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_db() -> (TempDir, Connection) {
        let tmp = TempDir::new().unwrap();
        let conn = open_observations_db(tmp.path()).unwrap();
        (tmp, conn)
    }

    #[test]
    fn schema_creation_is_idempotent() {
        let (_tmp, conn) = setup_db();
        // Calling init again should not fail
        init_observations_schema(&conn).unwrap();
        init_observations_schema(&conn).unwrap();
    }

    #[test]
    fn store_and_count_observations() {
        let (_tmp, conn) = setup_db();

        let obs = NewObservation {
            session_id: "test-session-1".into(),
            content: "The config.toml is rewritten on startup".into(),
            entities: vec!["config.toml".into()],
            topics: vec!["config".into(), "startup".into()],
            priority: 2,
            importance: 0.8,
            source_file: None,
        };

        let id = store_observation(&conn, &obs).unwrap();
        assert!(id > 0);
        assert_eq!(count_observations(&conn).unwrap(), 1);
    }

    #[test]
    fn store_and_search_observations() {
        let (_tmp, conn) = setup_db();

        let obs = NewObservation {
            session_id: "test-session-1".into(),
            content: "SSRF protection blocks localhost requests in http_request tool".into(),
            entities: vec!["http_request".into()],
            topics: vec!["security".into(), "ssrf".into()],
            priority: 1,
            importance: 0.9,
            source_file: Some("src/tools/http.rs".into()),
        };
        store_observation(&conn, &obs).unwrap();

        let results = search_observations(&conn, "SSRF localhost", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].priority, 1);
        assert!(results[0].content.contains("SSRF"));
    }

    #[test]
    fn get_unconsolidated_returns_stored_observations() {
        let (_tmp, conn) = setup_db();

        for i in 0..3 {
            let obs = NewObservation {
                session_id: "sess-1".into(),
                content: format!("observation {i}"),
                entities: vec![],
                topics: vec![],
                priority: 3,
                importance: 0.5,
                source_file: None,
            };
            store_observation(&conn, &obs).unwrap();
        }

        let unconsolidated = get_unconsolidated(&conn, 10).unwrap();
        assert_eq!(unconsolidated.len(), 3);
        assert!(!unconsolidated[0].consolidated);
    }

    #[test]
    fn session_lifecycle_tracking() {
        let (_tmp, conn) = setup_db();

        start_session(&conn, "sess-abc", "webchat").unwrap();
        end_session(&conn, "sess-abc", 5).unwrap();

        let turn_count: i32 = conn
            .query_row(
                "SELECT turn_count FROM session_log WHERE session_id = ?1",
                params!["sess-abc"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(turn_count, 5);

        let ended_at: Option<String> = conn
            .query_row(
                "SELECT ended_at FROM session_log WHERE session_id = ?1",
                params!["sess-abc"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(ended_at.is_some());
    }

    #[test]
    fn priority_and_importance_are_clamped() {
        let (_tmp, conn) = setup_db();

        let obs = NewObservation {
            session_id: "sess-1".into(),
            content: "test clamping".into(),
            entities: vec![],
            topics: vec![],
            priority: 10, // should clamp to 4
            importance: 2.0, // should clamp to 1.0
            source_file: None,
        };
        store_observation(&conn, &obs).unwrap();

        let results = get_unconsolidated(&conn, 1).unwrap();
        assert_eq!(results[0].priority, 4);
        assert!((results[0].importance - 1.0).abs() < f64::EPSILON);
    }
}
