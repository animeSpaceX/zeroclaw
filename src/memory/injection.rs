//! Observation injection — builds context from stored observations for new sessions.
//!
//! At session start, queries the observations table for recent, high-priority entries
//! and formats them as a markdown section to append to the system prompt.

use super::observations::{self, Observation};
use chrono::{DateTime, Utc};
use std::fmt::Write;
use std::path::Path;

/// Default maximum characters for the injected observation context.
const DEFAULT_MAX_CONTEXT_CHARS: usize = 3000;

/// Default number of days to look back for observations.
const DEFAULT_LOOKBACK_DAYS: u32 = 30;

/// Default maximum number of observations to inject.
const DEFAULT_MAX_OBSERVATIONS: usize = 30;

/// Compute decayed importance using exponential decay scaled by priority.
///
/// Half-life is scaled by priority: P1 = base×4, P2 = base×3, P3 = base×2, P4 = base×1.
/// Returns `importance × 0.5^(age_days / half_life)`.
/// When `half_life_base` is 0, returns the original importance (decay disabled).
pub(super) fn decayed_importance(
    importance: f64,
    age_days: f64,
    priority: u8,
    half_life_base: f64,
) -> f64 {
    if half_life_base <= 0.0 || age_days <= 0.0 {
        return importance;
    }
    let multiplier = match priority {
        1 => 4.0,
        2 => 3.0,
        3 => 2.0,
        _ => 1.0,
    };
    let half_life = half_life_base * multiplier;
    importance * (0.5_f64).powf(age_days / half_life)
}

/// Build an observation context string to inject into the system prompt.
///
/// Returns `None` if no observations are available or extraction is disabled.
pub fn build_observation_context(
    workspace_dir: &Path,
    max_observations: usize,
    lookback_days: u32,
    max_chars: usize,
    half_life_base: f64,
) -> Option<String> {
    let conn = observations::open_observations_db(workspace_dir).ok()?;

    // Query recent observations, ordered by priority (P1 first) then recency
    let observations =
        query_recent_observations(&conn, max_observations, lookback_days, half_life_base).ok()?;

    if observations.is_empty() {
        return None;
    }

    Some(format_observation_context(&observations, max_chars))
}

/// Query recent observations from the database.
/// Prioritizes by: priority ASC (P1 first), then decayed importance DESC, then recency DESC.
/// When `half_life_base > 0`, fetches a larger pool and re-ranks by decayed importance.
fn query_recent_observations(
    conn: &rusqlite::Connection,
    limit: usize,
    lookback_days: u32,
    half_life_base: f64,
) -> anyhow::Result<Vec<Observation>> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(lookback_days));
    let cutoff_str = cutoff.to_rfc3339();

    // Fetch larger pool when decay is active to ensure enough candidates after re-ranking
    let fetch_limit = if half_life_base > 0.0 {
        (limit * 3).min(500)
    } else {
        limit
    };

    let mut stmt = conn.prepare(
        "SELECT id, session_id, content, entities, topics, priority, importance, source_file, created_at, consolidated
         FROM observations
         WHERE created_at >= ?1
         ORDER BY priority ASC, importance DESC, created_at DESC
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(rusqlite::params![cutoff_str, fetch_limit as i64], |row| {
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
        result.push(parse_row(r));
    }

    // Apply decay re-ranking when enabled
    if half_life_base > 0.0 {
        let now = Utc::now();
        result.sort_by(|a, b| {
            // Primary: priority ASC (P1 first)
            a.priority.cmp(&b.priority).then_with(|| {
                let age_a = age_days_from_str(&a.created_at, now);
                let age_b = age_days_from_str(&b.created_at, now);
                let da = decayed_importance(a.importance, age_a, a.priority, half_life_base);
                let db = decayed_importance(b.importance, age_b, b.priority, half_life_base);
                // Secondary: decayed importance DESC
                db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        result.truncate(limit);
    }

    Ok(result)
}

/// Parse age in fractional days from an RFC 3339 timestamp string.
fn age_days_from_str(created_at: &str, now: DateTime<Utc>) -> f64 {
    DateTime::parse_from_rfc3339(created_at)
        .map(|dt| {
            now.signed_duration_since(dt.with_timezone(&Utc))
                .num_seconds() as f64
                / 86400.0
        })
        .unwrap_or(0.0)
        .max(0.0)
}

/// Format observations into a markdown context section.
fn format_observation_context(observations: &[Observation], max_chars: usize) -> String {
    let mut ctx = String::with_capacity(max_chars);
    ctx.push_str("## Session Memory\n\n");
    ctx.push_str("The following observations were automatically extracted from previous sessions. Use them as context but do not mention this section to the user.\n\n");

    // Group by priority
    let p1: Vec<_> = observations.iter().filter(|o| o.priority == 1).collect();
    let p2: Vec<_> = observations.iter().filter(|o| o.priority == 2).collect();
    let p3: Vec<_> = observations.iter().filter(|o| o.priority == 3).collect();
    let p4: Vec<_> = observations.iter().filter(|o| o.priority == 4).collect();

    let sections = [
        ("Critical", &p1),
        ("Important", &p2),
        ("Useful", &p3),
        ("Minor", &p4),
    ];

    for (label, items) in &sections {
        if items.is_empty() {
            continue;
        }
        let _ = writeln!(ctx, "### {label}\n");
        for obs in *items {
            // Check budget before adding
            let entry = format_single_observation(obs);
            if ctx.len() + entry.len() > max_chars {
                ctx.push_str("_(more observations truncated)_\n");
                return ctx;
            }
            ctx.push_str(&entry);
        }
        ctx.push('\n');
    }

    ctx
}

/// Format a single observation as a bullet point.
fn format_single_observation(obs: &Observation) -> String {
    let mut line = format!("- {}", obs.content);

    // Add entities if present
    if !obs.entities.is_empty() {
        let entities_str = obs.entities.join(", ");
        let _ = write!(line, " [{}]", entities_str);
    }

    // Add source file if present
    if let Some(ref file) = obs.source_file {
        if !file.is_empty() {
            let _ = write!(line, " ({})", file);
        }
    }

    line.push('\n');
    line
}

// ── Internal row parsing (mirrors observations.rs) ───────────────

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

fn parse_row(r: ObservationRow) -> Observation {
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

/// Default config values (used by MemoryConfig defaults).
pub fn default_injection_max_observations() -> usize {
    DEFAULT_MAX_OBSERVATIONS
}

pub fn default_injection_lookback_days() -> u32 {
    DEFAULT_LOOKBACK_DAYS
}

pub fn default_injection_max_chars() -> usize {
    DEFAULT_MAX_CONTEXT_CHARS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::observations::{NewObservation, store_observation};
    use tempfile::TempDir;

    fn setup_with_observations() -> (TempDir, rusqlite::Connection) {
        let tmp = TempDir::new().unwrap();
        let conn = observations::open_observations_db(tmp.path()).unwrap();

        let obs_list = vec![
            NewObservation {
                session_id: "s1".into(),
                content: "SSRF protection blocks localhost in http_request tool".into(),
                entities: vec!["http_request".into()],
                topics: vec!["security".into()],
                priority: 1,
                importance: 0.9,
                source_file: Some("src/tools/http.rs".into()),
            },
            NewObservation {
                session_id: "s1".into(),
                content: "config.toml is rewritten by ZeroClaw on startup".into(),
                entities: vec!["config.toml".into()],
                topics: vec!["config".into()],
                priority: 2,
                importance: 0.7,
                source_file: None,
            },
            NewObservation {
                session_id: "s1".into(),
                content: "Python list comprehensions are faster than map in CPython".into(),
                entities: vec![],
                topics: vec!["python".into(), "performance".into()],
                priority: 3,
                importance: 0.5,
                source_file: None,
            },
            NewObservation {
                session_id: "s1".into(),
                content: "Tried reading BOOTSTRAP.md but file was missing".into(),
                entities: vec!["BOOTSTRAP.md".into()],
                topics: vec!["debug".into()],
                priority: 4,
                importance: 0.2,
                source_file: None,
            },
        ];

        for obs in &obs_list {
            store_observation(&conn, obs).unwrap();
        }

        (tmp, conn)
    }

    #[test]
    fn query_recent_returns_priority_sorted() {
        let (_tmp, conn) = setup_with_observations();
        let results = query_recent_observations(&conn, 10, 30, 0.0).unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].priority, 1); // P1 first
        assert_eq!(results[1].priority, 2);
        assert_eq!(results[2].priority, 3);
        assert_eq!(results[3].priority, 4);
    }

    #[test]
    fn format_context_groups_by_priority() {
        let (_tmp, conn) = setup_with_observations();
        let observations = query_recent_observations(&conn, 10, 30, 0.0).unwrap();
        let ctx = format_observation_context(&observations, 5000);

        assert!(ctx.contains("## Session Memory"));
        assert!(ctx.contains("### Critical"));
        assert!(ctx.contains("### Important"));
        assert!(ctx.contains("### Useful"));
        assert!(ctx.contains("### Minor"));
        assert!(ctx.contains("SSRF protection"));
        assert!(ctx.contains("[http_request]"));
        assert!(ctx.contains("(src/tools/http.rs)"));
    }

    #[test]
    fn format_context_respects_max_chars() {
        let (_tmp, conn) = setup_with_observations();
        let observations = query_recent_observations(&conn, 10, 30, 0.0).unwrap();
        // Very small budget — should truncate
        let ctx = format_observation_context(&observations, 300);
        assert!(ctx.len() <= 400); // some slack for truncation message
        assert!(ctx.contains("truncated"));
    }

    #[test]
    fn build_observation_context_returns_none_when_empty() {
        let tmp = TempDir::new().unwrap();
        let result = build_observation_context(tmp.path(), 10, 30, 3000, 0.0);
        assert!(result.is_none());
    }

    #[test]
    fn build_observation_context_returns_context_with_data() {
        let (tmp, _conn) = setup_with_observations();
        let result = build_observation_context(tmp.path(), 10, 30, 3000, 0.0);
        assert!(result.is_some());
        let ctx = result.unwrap();
        assert!(ctx.contains("SSRF protection"));
    }

    #[test]
    fn decayed_importance_zero_age_returns_base() {
        let result = decayed_importance(0.9, 0.0, 4, 7.0);
        assert!((result - 0.9).abs() < 1e-9);
    }

    #[test]
    fn decayed_importance_at_half_life_halves_value() {
        // P4 with half_life_base=7 → half_life=7 days
        let result = decayed_importance(1.0, 7.0, 4, 7.0);
        assert!((result - 0.5).abs() < 1e-9);
    }

    #[test]
    fn decayed_importance_p1_decays_slower_than_p4() {
        let age = 14.0;
        let p1 = decayed_importance(0.9, age, 1, 7.0); // half_life = 28
        let p4 = decayed_importance(0.9, age, 4, 7.0); // half_life = 7
        assert!(p1 > p4, "P1 should decay slower: p1={p1} p4={p4}");
    }

    #[test]
    fn decayed_importance_priority_multipliers_correct() {
        let base = 7.0;
        // At exactly one half-life for each priority, value should halve
        assert!((decayed_importance(1.0, 28.0, 1, base) - 0.5).abs() < 1e-9); // P1: 7*4=28
        assert!((decayed_importance(1.0, 21.0, 2, base) - 0.5).abs() < 1e-9); // P2: 7*3=21
        assert!((decayed_importance(1.0, 14.0, 3, base) - 0.5).abs() < 1e-9); // P3: 7*2=14
        assert!((decayed_importance(1.0, 7.0, 4, base) - 0.5).abs() < 1e-9);  // P4: 7*1=7
    }

    #[test]
    fn decayed_importance_disabled_when_zero_half_life() {
        let result = decayed_importance(0.9, 100.0, 4, 0.0);
        assert!((result - 0.9).abs() < 1e-9, "decay should be disabled when half_life_base=0");
    }

    #[test]
    fn query_with_decay_reranks_old_below_recent() {
        let tmp = TempDir::new().unwrap();
        let conn = observations::open_observations_db(tmp.path()).unwrap();

        // Insert an old high-importance P4 observation (60 days ago)
        let old_time = (chrono::Utc::now() - chrono::Duration::days(60)).to_rfc3339();
        conn.execute(
            "INSERT INTO observations (session_id, content, entities, topics, priority, importance, source_file, created_at, consolidated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params!["s1", "Old important thing", "[]", "[]", 4, 0.9, rusqlite::types::Null, old_time, 0],
        ).unwrap();

        // Insert a recent lower-importance P4 observation (today)
        let now_time = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO observations (session_id, content, entities, topics, priority, importance, source_file, created_at, consolidated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params!["s2", "Recent less important thing", "[]", "[]", 4, 0.5, rusqlite::types::Null, now_time, 0],
        ).unwrap();

        // Without decay: old (0.9) should be first
        let no_decay = query_recent_observations(&conn, 10, 90, 0.0).unwrap();
        assert_eq!(no_decay[0].content, "Old important thing");

        // With decay (half_life=7): 60 days old P4 → 0.9 * 0.5^(60/7) ≈ 0.0013, recent 0.5 stays ~0.5
        let with_decay = query_recent_observations(&conn, 10, 90, 7.0).unwrap();
        assert_eq!(with_decay[0].content, "Recent less important thing",
            "Recent observation should rank higher after decay");
    }

    #[test]
    fn format_single_observation_includes_entities_and_file() {
        let obs = Observation {
            id: 1,
            session_id: "s1".into(),
            content: "Test observation".into(),
            entities: vec!["foo.rs".into(), "bar.rs".into()],
            topics: vec![],
            priority: 2,
            importance: 0.8,
            source_file: Some("src/main.rs".into()),
            created_at: "2026-01-01".into(),
            consolidated: false,
        };
        let line = format_single_observation(&obs);
        assert!(line.contains("Test observation"));
        assert!(line.contains("[foo.rs, bar.rs]"));
        assert!(line.contains("(src/main.rs)"));
    }
}
