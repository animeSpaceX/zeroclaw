//! Automatic observation extraction from conversation sessions.
//!
//! After a WebSocket session ends, the extraction pipeline:
//! 1. Converts chat history to a plain-text transcript
//! 2. Sends transcript to an LLM with a structured extraction prompt
//! 3. Parses the JSON response into observations
//! 4. Stores observations in the SQLite observations table

use super::observations::{self, NewObservation};
use crate::providers::traits::Provider;
use crate::providers::ChatMessage;
use serde::Deserialize;
use std::path::Path;

const MAX_OBSERVATIONS: usize = 20;
const MAX_TRANSCRIPT_CHARS: usize = 32_000;

const EXTRACTION_SYSTEM_PROMPT: &str = r#"You are a Session Observer. Analyze the conversation and extract structured observations.

Categories:
- P1 (Critical): Architectural decisions, security patterns, breaking changes
- P2 (Important): Bug fixes with root causes, dependency issues, workarounds
- P3 (Useful): Code patterns, conventions, file relationships
- P4 (Minor): Failed approaches, environment details, dead ends

For each observation, return:
- content: precise factual statement (include file paths, error messages, function names)
- entities: key entities (files, functions, packages, services)
- topics: 2-4 topic tags
- priority: "P1", "P2", "P3", or "P4"
- importance: float 0.0 to 1.0

Rules:
- Be concrete, not vague. Include exact paths/names/errors
- Capture the "why", not just the "what"
- Note what DIDN'T work (valuable for future sessions)
- Max 20 observations
- Skip trivial file reads and simple searches

Return ONLY a JSON array. No markdown fences, no explanation."#;

/// Raw observation parsed from LLM response (before validation/clamping).
#[derive(Debug, Deserialize)]
struct RawObservation {
    #[serde(default)]
    content: String,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    importance: f64,
    #[serde(default)]
    source_file: Option<String>,
}

/// Convert chat history into a plain-text transcript.
/// Keeps the tail (most recent content is more important), truncated to MAX_TRANSCRIPT_CHARS.
fn summarize_history(history: &[ChatMessage]) -> String {
    let mut lines = Vec::new();
    for msg in history {
        let role_label = match msg.role.as_str() {
            "system" => continue, // skip system prompt, it's boilerplate
            "user" => "User",
            "assistant" => "Assistant",
            "tool" => "Tool",
            other => other,
        };
        // Truncate very long individual messages
        let content = if msg.content.len() > 2000 {
            format!("{}...(truncated)", &msg.content[..2000])
        } else {
            msg.content.clone()
        };
        lines.push(format!("{role_label}: {content}"));
    }

    let full = lines.join("\n\n");

    // Keep the tail if too long (recent content is more important)
    if full.len() > MAX_TRANSCRIPT_CHARS {
        let start = full.len() - MAX_TRANSCRIPT_CHARS;
        // Find a clean line boundary
        let boundary = full[start..]
            .find('\n')
            .map(|i| start + i + 1)
            .unwrap_or(start);
        format!("...(earlier conversation truncated)\n\n{}", &full[boundary..])
    } else {
        full
    }
}

/// Parse LLM response into observations, with tolerance for markdown fences and partial JSON.
fn parse_extraction_response(response: &str) -> Vec<RawObservation> {
    let trimmed = response.trim();

    // Try 1: parse directly
    if let Ok(obs) = serde_json::from_str::<Vec<RawObservation>>(trimmed) {
        return obs;
    }

    // Try 2: strip markdown fences (```json ... ```)
    let stripped = strip_markdown_fences(trimmed);
    if let Ok(obs) = serde_json::from_str::<Vec<RawObservation>>(&stripped) {
        return obs;
    }

    // Try 3: find first '[' to last ']'
    if let (Some(start), Some(end)) = (trimmed.find('['), trimmed.rfind(']')) {
        if start < end {
            let slice = &trimmed[start..=end];
            if let Ok(obs) = serde_json::from_str::<Vec<RawObservation>>(slice) {
                return obs;
            }
        }
    }

    // All attempts failed — return empty (silent failure, don't crash)
    tracing::debug!("Failed to parse extraction response as JSON array");
    Vec::new()
}

/// Strip ```json ... ``` or ``` ... ``` fences.
fn strip_markdown_fences(text: &str) -> String {
    let mut result = text.to_string();
    // Remove opening fence
    if let Some(start) = result.find("```") {
        let fence_end = result[start + 3..]
            .find('\n')
            .map(|i| start + 3 + i + 1)
            .unwrap_or(start + 3);
        result = result[fence_end..].to_string();
    }
    // Remove closing fence
    if let Some(end) = result.rfind("```") {
        result = result[..end].to_string();
    }
    result.trim().to_string()
}

/// Convert priority string ("P1"/"P2"/"P3"/"P4") to u8 (1-4).
fn parse_priority(s: &str) -> u8 {
    match s.trim().to_uppercase().as_str() {
        "P1" | "1" => 1,
        "P2" | "2" => 2,
        "P3" | "3" => 3,
        "P4" | "4" => 4,
        _ => 3, // default to P3
    }
}

/// Main extraction pipeline. Runs in a background task after session close.
///
/// Returns the number of observations stored.
pub async fn run_extraction(
    session_id: &str,
    history: &[ChatMessage],
    provider: &dyn Provider,
    model: &str,
    temperature: f64,
    min_transcript_chars: usize,
    workspace_dir: &Path,
) -> anyhow::Result<usize> {
    // 1. Build transcript
    let transcript = summarize_history(history);

    // 2. Check minimum length
    if transcript.len() < min_transcript_chars {
        tracing::debug!(
            session_id,
            len = transcript.len(),
            min = min_transcript_chars,
            "session too short for extraction, skipping"
        );
        return Ok(0);
    }

    // 3. Call LLM for extraction
    let response = provider
        .chat_with_system(
            Some(EXTRACTION_SYSTEM_PROMPT),
            &transcript,
            model,
            temperature,
        )
        .await?;

    // 4. Parse response
    let raw_observations = parse_extraction_response(&response);
    if raw_observations.is_empty() {
        tracing::debug!(session_id, "extraction produced no observations");
        return Ok(0);
    }

    // 5. Store observations (limit to MAX_OBSERVATIONS)
    let workspace_path = workspace_dir.to_path_buf();
    let session_id_owned = session_id.to_string();
    let observations: Vec<NewObservation> = raw_observations
        .into_iter()
        .take(MAX_OBSERVATIONS)
        .filter(|obs| !obs.content.trim().is_empty())
        .map(|raw| NewObservation {
            session_id: session_id_owned.clone(),
            content: raw.content,
            entities: raw.entities,
            topics: raw.topics,
            priority: parse_priority(&raw.priority),
            importance: raw.importance.clamp(0.0, 1.0),
            source_file: raw.source_file,
        })
        .collect();

    if observations.is_empty() {
        return Ok(0);
    }

    // Offload SQLite writes to blocking thread
    let count = observations.len();
    tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        let conn = observations::open_observations_db(&workspace_path)?;
        for obs in &observations {
            observations::store_observation(&conn, obs)?;
        }
        Ok(count)
    })
    .await??;

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_history_skips_system_messages() {
        let history = vec![
            ChatMessage::system("You are helpful"),
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ];
        let transcript = summarize_history(&history);
        assert!(!transcript.contains("You are helpful"));
        assert!(transcript.contains("User: Hello"));
        assert!(transcript.contains("Assistant: Hi there!"));
    }

    #[test]
    fn summarize_history_truncates_long_messages() {
        let long_content = "x".repeat(5000);
        let history = vec![ChatMessage::user(&long_content)];
        let transcript = summarize_history(&history);
        assert!(transcript.contains("...(truncated)"));
        assert!(transcript.len() < 5000);
    }

    #[test]
    fn summarize_history_keeps_tail_when_too_long() {
        let mut history = Vec::new();
        for i in 0..200 {
            history.push(ChatMessage::user(&format!("Message number {i} with some padding content to make it longer")));
            history.push(ChatMessage::assistant(&format!("Response to message {i} with some padding")));
        }
        let transcript = summarize_history(&history);
        assert!(transcript.len() <= MAX_TRANSCRIPT_CHARS + 100); // some slack for prefix
        // Should contain recent messages, not early ones
        assert!(transcript.contains("Message number 199"));
    }

    #[test]
    fn parse_extraction_response_handles_clean_json() {
        let json = r#"[
            {"content": "config.toml is rewritten on startup", "entities": ["config.toml"], "topics": ["config"], "priority": "P2", "importance": 0.7}
        ]"#;
        let result = parse_extraction_response(json);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "config.toml is rewritten on startup");
    }

    #[test]
    fn parse_extraction_response_handles_markdown_fences() {
        let json = "```json\n[\n{\"content\": \"test\", \"priority\": \"P3\", \"importance\": 0.5}\n]\n```";
        let result = parse_extraction_response(json);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_extraction_response_handles_extra_text() {
        let json = "Here are the observations:\n[\n{\"content\": \"test\", \"priority\": \"P3\", \"importance\": 0.5}\n]\nDone!";
        let result = parse_extraction_response(json);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_extraction_response_returns_empty_on_garbage() {
        let result = parse_extraction_response("this is not json at all");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_priority_handles_variants() {
        assert_eq!(parse_priority("P1"), 1);
        assert_eq!(parse_priority("P2"), 2);
        assert_eq!(parse_priority("p3"), 3);
        assert_eq!(parse_priority("P4"), 4);
        assert_eq!(parse_priority("1"), 1);
        assert_eq!(parse_priority("unknown"), 3); // default
    }

    #[test]
    fn strip_markdown_fences_removes_code_block() {
        let input = "```json\n[{\"a\": 1}]\n```";
        assert_eq!(strip_markdown_fences(input), "[{\"a\": 1}]");
    }

    #[test]
    fn strip_markdown_fences_handles_no_fences() {
        let input = "[{\"a\": 1}]";
        assert_eq!(strip_markdown_fences(input), "[{\"a\": 1}]");
    }
}
