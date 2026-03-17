//! Observation consolidation pipeline — cross-session deduplication and pattern discovery.
//!
//! After extraction stores raw observations, consolidation reviews accumulated
//! unconsolidated entries in batches and asks an LLM to:
//! - **keep**: mark as consolidated (no change to content)
//! - **delete**: remove duplicates or obsolete entries
//! - **merge**: combine related observations into a refined higher-priority entry
//!
//! Consolidation is triggered automatically after extraction when the number of
//! unconsolidated observations exceeds `consolidation_threshold`.

use super::observations::{self, NewObservation, Observation};
use crate::providers::traits::Provider;
use serde::Deserialize;
use std::path::Path;

/// Summary of what a consolidation pass accomplished.
#[derive(Debug, Default)]
pub struct ConsolidationReport {
    pub kept: usize,
    pub deleted: usize,
    pub merged: usize,
    pub new_observations: usize,
}

/// LLM-parsed consolidation plan.
#[derive(Debug, Deserialize)]
struct ConsolidationPlan {
    #[serde(default)]
    keep: Vec<i64>,
    #[serde(default)]
    delete: Vec<i64>,
    #[serde(default)]
    merge: Vec<MergeGroup>,
}

#[derive(Debug, Deserialize)]
struct MergeGroup {
    source_ids: Vec<i64>,
    content: String,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default = "default_priority")]
    priority: String,
    #[serde(default = "default_importance")]
    importance: f64,
}

fn default_priority() -> String {
    "P3".into()
}
fn default_importance() -> f64 {
    0.5
}

const CONSOLIDATION_SYSTEM_PROMPT: &str = r#"You are a Memory Consolidator. You receive a batch of observations extracted from past sessions.

Your job:
1. KEEP observations that are unique and valuable as-is → add their id to "keep"
2. DELETE observations that are duplicates, outdated, or trivially obvious → add their id to "delete"
3. MERGE observations that describe the same concept/pattern from different angles → combine into one refined observation

Rules:
- Every input observation id MUST appear in exactly one category (keep, delete, or merge source_ids)
- Merged observations should be strictly better: more precise, higher priority if warranted
- Priority scale: P1 (critical architectural/security), P2 (important bugs/workarounds), P3 (useful patterns), P4 (minor details)
- importance: 0.0-1.0 float reflecting how useful this is for future sessions
- Prefer merging over keeping when 2+ observations overlap significantly

Return ONLY valid JSON in this exact format. No markdown fences, no explanation:
{
  "keep": [1, 2],
  "delete": [3],
  "merge": [{
    "source_ids": [4, 5, 6],
    "content": "refined merged content — be specific, include file paths and function names",
    "entities": ["entity1", "entity2"],
    "topics": ["topic1", "topic2"],
    "priority": "P1",
    "importance": 0.9
  }]
}"#;

/// Run consolidation if the number of unconsolidated observations exceeds the threshold.
///
/// This is designed to be called after extraction in a background task.
/// Returns a report of actions taken, or an error if the pipeline fails.
pub async fn run_consolidation_if_needed(
    provider: &dyn Provider,
    model: &str,
    temperature: f64,
    threshold: usize,
    batch_size: usize,
    workspace_dir: &Path,
) -> anyhow::Result<ConsolidationReport> {
    let workspace_path = workspace_dir.to_path_buf();

    // 1. Check if consolidation is needed
    let count = tokio::task::spawn_blocking({
        let wp = workspace_path.clone();
        move || -> anyhow::Result<usize> {
            let conn = observations::open_observations_db(&wp)?;
            observations::count_unconsolidated(&conn)
        }
    })
    .await??;

    if count < threshold {
        tracing::debug!(
            unconsolidated = count,
            threshold,
            "consolidation threshold not met, skipping"
        );
        return Ok(ConsolidationReport::default());
    }

    tracing::info!(
        unconsolidated = count,
        threshold,
        batch_size,
        "🔄 Starting observation consolidation"
    );

    // 2. Fetch a batch of unconsolidated observations
    let batch = tokio::task::spawn_blocking({
        let wp = workspace_path.clone();
        move || -> anyhow::Result<Vec<Observation>> {
            let conn = observations::open_observations_db(&wp)?;
            observations::get_unconsolidated(&conn, batch_size)
        }
    })
    .await??;

    if batch.is_empty() {
        return Ok(ConsolidationReport::default());
    }

    // 3. Format observations for LLM
    let prompt = format_observations_for_llm(&batch);

    // 4. Call LLM
    let response = provider
        .chat_with_system(
            Some(CONSOLIDATION_SYSTEM_PROMPT),
            &prompt,
            model,
            temperature,
        )
        .await?;

    // 5. Parse response
    let plan = match parse_consolidation_response(&response) {
        Some(plan) => plan,
        None => {
            tracing::warn!("failed to parse consolidation response, skipping");
            return Ok(ConsolidationReport::default());
        }
    };

    // 6. Validate that all input IDs are accounted for
    let input_ids: std::collections::HashSet<i64> = batch.iter().map(|o| o.id).collect();
    let mut classified_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for &id in &plan.keep {
        classified_ids.insert(id);
    }
    for &id in &plan.delete {
        classified_ids.insert(id);
    }
    for group in &plan.merge {
        for &id in &group.source_ids {
            classified_ids.insert(id);
        }
    }

    // Only warn if some IDs were missed — don't fail the whole pipeline
    let missed: Vec<i64> = input_ids.difference(&classified_ids).copied().collect();
    if !missed.is_empty() {
        tracing::warn!(
            missed_count = missed.len(),
            "consolidation LLM missed some observation ids, treating them as keep"
        );
    }

    // Filter out any IDs that weren't in the input batch (hallucinated by LLM)
    let valid_plan = ConsolidationPlan {
        keep: plan
            .keep
            .into_iter()
            .filter(|id| input_ids.contains(id))
            .collect(),
        delete: plan
            .delete
            .into_iter()
            .filter(|id| input_ids.contains(id))
            .collect(),
        merge: plan
            .merge
            .into_iter()
            .map(|mut g| {
                g.source_ids.retain(|id| input_ids.contains(id));
                g
            })
            .filter(|g| !g.source_ids.is_empty())
            .collect(),
    };

    // 7. Execute plan
    let report = tokio::task::spawn_blocking({
        let wp = workspace_path;
        move || execute_plan(&wp, &valid_plan, &missed)
    })
    .await??;

    tracing::info!(
        kept = report.kept,
        deleted = report.deleted,
        merged = report.merged,
        new = report.new_observations,
        "🔄 Consolidation complete"
    );

    Ok(report)
}

/// Format observations into a JSON-like listing for the LLM prompt.
fn format_observations_for_llm(observations: &[Observation]) -> String {
    let mut lines = Vec::with_capacity(observations.len() + 2);
    lines.push(format!(
        "Review these {} observations and consolidate:\n",
        observations.len()
    ));

    for obs in observations {
        let priority_label = match obs.priority {
            1 => "P1",
            2 => "P2",
            3 => "P3",
            _ => "P4",
        };
        lines.push(format!(
            "[id={}] ({}, importance={:.1}) {}\n  entities: {:?}\n  topics: {:?}",
            obs.id, priority_label, obs.importance, obs.content, obs.entities, obs.topics,
        ));
    }

    lines.join("\n")
}

/// Parse the LLM response with 3-level tolerance.
fn parse_consolidation_response(response: &str) -> Option<ConsolidationPlan> {
    let trimmed = response.trim();

    // Try 1: direct parse
    if let Ok(plan) = serde_json::from_str::<ConsolidationPlan>(trimmed) {
        return Some(plan);
    }

    // Try 2: strip markdown fences
    let stripped = strip_markdown_fences(trimmed);
    if let Ok(plan) = serde_json::from_str::<ConsolidationPlan>(&stripped) {
        return Some(plan);
    }

    // Try 3: find first '{' to last '}'
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if start < end {
            let slice = &trimmed[start..=end];
            if let Ok(plan) = serde_json::from_str::<ConsolidationPlan>(slice) {
                return Some(plan);
            }
        }
    }

    tracing::debug!("failed to parse consolidation response as JSON object");
    None
}

/// Strip ```json ... ``` or ``` ... ``` fences.
fn strip_markdown_fences(text: &str) -> String {
    let mut result = text.to_string();
    if let Some(start) = result.find("```") {
        let fence_end = result[start + 3..]
            .find('\n')
            .map(|i| start + 3 + i + 1)
            .unwrap_or(start + 3);
        result = result[fence_end..].to_string();
    }
    if let Some(end) = result.rfind("```") {
        result = result[..end].to_string();
    }
    result.trim().to_string()
}

/// Convert priority string to u8.
fn parse_priority(s: &str) -> u8 {
    match s.trim().to_uppercase().as_str() {
        "P1" | "1" => 1,
        "P2" | "2" => 2,
        "P3" | "3" => 3,
        "P4" | "4" => 4,
        _ => 3,
    }
}

/// Execute the consolidation plan against the database.
fn execute_plan(
    workspace_dir: &Path,
    plan: &ConsolidationPlan,
    missed_ids: &[i64],
) -> anyhow::Result<ConsolidationReport> {
    let conn = observations::open_observations_db(workspace_dir)?;

    let mut report = ConsolidationReport::default();

    // Mark kept observations as consolidated
    let mut mark_ids: Vec<i64> = plan.keep.clone();
    // Also mark missed IDs as consolidated (treat as keep)
    mark_ids.extend_from_slice(missed_ids);

    // Mark merge source IDs as consolidated too
    for group in &plan.merge {
        mark_ids.extend_from_slice(&group.source_ids);
    }

    if !mark_ids.is_empty() {
        observations::mark_consolidated(&conn, &mark_ids)?;
        report.kept = plan.keep.len() + missed_ids.len();
    }

    // Delete marked observations
    if !plan.delete.is_empty() {
        let deleted = observations::delete_observations(&conn, &plan.delete)?;
        report.deleted = deleted;
    }

    // Create merged observations
    for group in &plan.merge {
        if group.content.trim().is_empty() {
            continue;
        }
        let new_obs = NewObservation {
            session_id: "consolidation".to_string(),
            content: group.content.clone(),
            entities: group.entities.clone(),
            topics: group.topics.clone(),
            priority: parse_priority(&group.priority),
            importance: group.importance.clamp(0.0, 1.0),
            source_file: None,
        };
        let new_id = observations::store_observation(&conn, &new_obs)?;
        // Mark the new observation as consolidated immediately
        observations::mark_consolidated(&conn, &[new_id])?;
        report.merged += 1;
        report.new_observations += 1;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::observations::{open_observations_db, store_observation, NewObservation};
    use tempfile::TempDir;

    #[test]
    fn parse_consolidation_response_handles_clean_json() {
        let json = r#"{"keep": [1, 2], "delete": [3], "merge": [{"source_ids": [4, 5], "content": "merged", "entities": [], "topics": [], "priority": "P2", "importance": 0.8}]}"#;
        let plan = parse_consolidation_response(json).unwrap();
        assert_eq!(plan.keep, vec![1, 2]);
        assert_eq!(plan.delete, vec![3]);
        assert_eq!(plan.merge.len(), 1);
        assert_eq!(plan.merge[0].source_ids, vec![4, 5]);
        assert_eq!(plan.merge[0].content, "merged");
    }

    #[test]
    fn parse_consolidation_response_handles_markdown_fences() {
        let json = "```json\n{\"keep\": [1], \"delete\": [], \"merge\": []}\n```";
        let plan = parse_consolidation_response(json).unwrap();
        assert_eq!(plan.keep, vec![1]);
    }

    #[test]
    fn parse_consolidation_response_handles_extra_text() {
        let json = "Here is my analysis:\n{\"keep\": [1, 2], \"delete\": [3], \"merge\": []}\nDone!";
        let plan = parse_consolidation_response(json).unwrap();
        assert_eq!(plan.keep, vec![1, 2]);
        assert_eq!(plan.delete, vec![3]);
    }

    #[test]
    fn parse_consolidation_response_returns_none_on_garbage() {
        assert!(parse_consolidation_response("not json at all").is_none());
    }

    #[test]
    fn parse_consolidation_response_handles_defaults() {
        // Missing merge field
        let json = r#"{"keep": [1, 2], "delete": [3]}"#;
        let plan = parse_consolidation_response(json).unwrap();
        assert_eq!(plan.keep, vec![1, 2]);
        assert!(plan.merge.is_empty());
    }

    #[test]
    fn format_observations_for_llm_includes_all_fields() {
        let obs = vec![Observation {
            id: 42,
            session_id: "sess-1".into(),
            content: "config.toml is rewritten on startup".into(),
            entities: vec!["config.toml".into()],
            topics: vec!["config".into(), "startup".into()],
            priority: 2,
            importance: 0.8,
            source_file: None,
            created_at: "2024-01-01T00:00:00Z".into(),
            consolidated: false,
        }];
        let formatted = format_observations_for_llm(&obs);
        assert!(formatted.contains("[id=42]"));
        assert!(formatted.contains("P2"));
        assert!(formatted.contains("importance=0.8"));
        assert!(formatted.contains("config.toml is rewritten on startup"));
        assert!(formatted.contains("config.toml"));
    }

    #[test]
    fn execute_plan_processes_keep_delete_merge() {
        let tmp = TempDir::new().unwrap();
        let conn = open_observations_db(tmp.path()).unwrap();

        // Insert 5 observations
        let mut ids = Vec::new();
        for i in 0..5 {
            let obs = NewObservation {
                session_id: "sess-1".into(),
                content: format!("observation {i}"),
                entities: vec![],
                topics: vec![],
                priority: 3,
                importance: 0.5,
                source_file: None,
            };
            let id = store_observation(&conn, &obs).unwrap();
            ids.push(id);
        }
        drop(conn);

        let plan = ConsolidationPlan {
            keep: vec![ids[0], ids[1]],
            delete: vec![ids[2]],
            merge: vec![MergeGroup {
                source_ids: vec![ids[3], ids[4]],
                content: "merged observation from 3 and 4".into(),
                entities: vec!["test".into()],
                topics: vec!["testing".into()],
                priority: "P2".into(),
                importance: 0.8,
            }],
        };

        let report = execute_plan(tmp.path(), &plan, &[]).unwrap();
        assert_eq!(report.kept, 2);
        assert_eq!(report.deleted, 1);
        assert_eq!(report.merged, 1);
        assert_eq!(report.new_observations, 1);

        // Verify DB state
        let conn = open_observations_db(tmp.path()).unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM observations", [], |row| row.get(0))
            .unwrap();
        // 5 original - 1 deleted + 1 merged = 5
        assert_eq!(total, 5);

        let unconsolidated = observations::count_unconsolidated(&conn).unwrap();
        assert_eq!(unconsolidated, 0);
    }

    #[test]
    fn execute_plan_handles_missed_ids_as_keep() {
        let tmp = TempDir::new().unwrap();
        let conn = open_observations_db(tmp.path()).unwrap();

        let obs = NewObservation {
            session_id: "sess-1".into(),
            content: "missed observation".into(),
            entities: vec![],
            topics: vec![],
            priority: 3,
            importance: 0.5,
            source_file: None,
        };
        let id = store_observation(&conn, &obs).unwrap();
        drop(conn);

        let plan = ConsolidationPlan {
            keep: vec![],
            delete: vec![],
            merge: vec![],
        };

        let report = execute_plan(tmp.path(), &plan, &[id]).unwrap();
        assert_eq!(report.kept, 1); // missed ID treated as keep

        let conn = open_observations_db(tmp.path()).unwrap();
        let unconsolidated = observations::count_unconsolidated(&conn).unwrap();
        assert_eq!(unconsolidated, 0);
    }
}
