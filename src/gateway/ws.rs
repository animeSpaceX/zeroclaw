//! WebSocket agent chat handler.
//!
//! Protocol:
//! ```text
//! Client -> Server: {"type":"message","content":"Hello"}
//! Server -> Client: {"type":"chunk","content":"Hi! "}
//! Server -> Client: {"type":"tool_call","name":"shell","args":{...}}
//! Server -> Client: {"type":"tool_result","name":"shell","output":"..."}
//! Server -> Client: {"type":"done","full_response":"..."}
//! ```

use super::AppState;
use crate::agent::loop_::{run_tool_call_loop, DRAFT_CLEAR_SENTINEL, DRAFT_PROGRESS_SENTINEL};
use crate::approval::ApprovalManager;
use crate::providers::ChatMessage;
use axum::{
    extract::{
        connect_info::ConnectInfo,
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    http::{header, HeaderMap},
    response::IntoResponse,
};
use serde_json::json;
use std::path::Path;

const EMPTY_WS_RESPONSE_FALLBACK: &str =
    "Tool execution completed, but the model returned no final text response. Please ask me to summarize the result.";
const WS_CHAT_SUBPROTOCOL: &str = "zeroclaw.v1";

/// Append a chat log entry to `chat.jsonl` in the given config directory.
fn append_chat_log(config_dir: &Path, entry: &serde_json::Value) {
    use std::io::Write;
    let log_path = config_dir.join("chat.jsonl");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(file, "{}", entry);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WsDeltaEvent {
    ContentChunk(String),
    ToolCall {
        name: String,
        hint: Option<String>,
    },
    ToolResult {
        name: String,
        success: bool,
        duration_secs: Option<u64>,
    },
}

fn sanitize_ws_response(response: &str, tools: &[Box<dyn crate::tools::Tool>]) -> String {
    let sanitized = crate::channels::sanitize_channel_response(response, tools);
    if sanitized.is_empty() && !response.trim().is_empty() {
        "I encountered malformed tool-call output and could not produce a safe reply. Please try again."
            .to_string()
    } else {
        sanitized
    }
}

fn normalize_prompt_tool_results(content: &str) -> Option<String> {
    let mut cleaned_lines: Vec<&str> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("<tool_result") || trimmed == "</tool_result>" {
            continue;
        }
        cleaned_lines.push(line.trim_end());
    }

    if cleaned_lines.is_empty() {
        None
    } else {
        Some(cleaned_lines.join("\n"))
    }
}

fn extract_latest_tool_output(history: &[ChatMessage]) -> Option<String> {
    for msg in history.iter().rev() {
        match msg.role.as_str() {
            "tool" => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content) {
                    if let Some(content) = value
                        .get("content")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                    {
                        return Some(content.to_string());
                    }
                }

                let trimmed = msg.content.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            "user" => {
                if let Some(payload) = msg.content.strip_prefix("[Tool results]") {
                    let payload = payload.trim_start_matches('\n');
                    if let Some(cleaned) = normalize_prompt_tool_results(payload) {
                        return Some(cleaned);
                    }
                }
            }
            _ => {}
        }
    }

    None
}

fn finalize_ws_response(
    response: &str,
    history: &[ChatMessage],
    tools: &[Box<dyn crate::tools::Tool>],
) -> String {
    let sanitized = sanitize_ws_response(response, tools);
    if !sanitized.trim().is_empty() {
        return sanitized;
    }

    if let Some(tool_output) = extract_latest_tool_output(history) {
        let excerpt = crate::util::truncate_with_ellipsis(tool_output.trim(), 1200);
        return format!(
            "Tool execution completed, but the model returned no final text response.\n\nLatest tool output:\n{excerpt}"
        );
    }

    EMPTY_WS_RESPONSE_FALLBACK.to_string()
}

fn parse_tool_completion_payload(raw: &str) -> Option<(String, Option<u64>)> {
    let trimmed = raw.trim();
    let (name_part, duration_part) = trimmed.rsplit_once(" (")?;
    let duration_part = duration_part.strip_suffix(')')?;
    let secs = duration_part.strip_suffix('s')?.parse::<u64>().ok();
    Some((name_part.trim().to_string(), secs))
}

fn parse_ws_delta_event(delta: &str) -> Option<WsDeltaEvent> {
    if delta == DRAFT_CLEAR_SENTINEL {
        return None;
    }

    if let Some(progress) = delta.strip_prefix(DRAFT_PROGRESS_SENTINEL) {
        let progress = progress.trim();
        if let Some(rest) = progress.strip_prefix("⏳ ") {
            let rest = rest.trim();
            if rest.is_empty() {
                return None;
            }
            let (name, hint) = match rest.split_once(": ") {
                Some((name, hint)) => {
                    let hint = hint.trim();
                    (
                        name.trim().to_string(),
                        if hint.is_empty() {
                            None
                        } else {
                            Some(hint.to_string())
                        },
                    )
                }
                None => (rest.to_string(), None),
            };
            return Some(WsDeltaEvent::ToolCall { name, hint });
        }

        if let Some(rest) = progress.strip_prefix("✅ ") {
            if let Some((name, duration_secs)) = parse_tool_completion_payload(rest) {
                return Some(WsDeltaEvent::ToolResult {
                    name,
                    success: true,
                    duration_secs,
                });
            }
        }

        if let Some(rest) = progress.strip_prefix("❌ ") {
            if let Some((name, duration_secs)) = parse_tool_completion_payload(rest) {
                return Some(WsDeltaEvent::ToolResult {
                    name,
                    success: false,
                    duration_secs,
                });
            }
        }

        return None;
    }

    if delta.is_empty() {
        None
    } else {
        Some(WsDeltaEvent::ContentChunk(delta.to_string()))
    }
}

async fn emit_ws_delta_event(socket: &mut WebSocket, event: WsDeltaEvent) {
    let payload = match event {
        WsDeltaEvent::ContentChunk(content) => json!({
            "type": "chunk",
            "content": content,
        }),
        WsDeltaEvent::ToolCall { name, hint } => json!({
            "type": "tool_call",
            "name": name,
            "args": {
                "hint": hint,
            },
        }),
        WsDeltaEvent::ToolResult {
            name,
            success,
            duration_secs,
        } => {
            let status = if success { "ok" } else { "error" };
            let output = match duration_secs {
                Some(secs) => format!("{status} ({secs}s)"),
                None => status.to_string(),
            };
            json!({
                "type": "tool_result",
                "name": name,
                "success": success,
                "duration_secs": duration_secs,
                "output": output,
            })
        }
    };

    let _ = socket.send(Message::Text(payload.to_string().into())).await;
}

/// Build a notification message when a subagent completes, to be injected into the conversation.
fn build_subagent_notification(event: &serde_json::Value) -> String {
    let agent = event["agent"].as_str().unwrap_or("unknown");
    let success = event["success"].as_bool().unwrap_or(false);
    let elapsed_ms = event["elapsed_ms"].as_u64().unwrap_or(0);
    let elapsed_secs = elapsed_ms / 1000;

    if success {
        let result = event["result"].as_str().unwrap_or("[no output]");
        // Truncate very long results to avoid context overflow
        let truncated = if result.len() > 3000 {
            format!("{}...\n(truncated)", &result[..3000])
        } else {
            result.to_string()
        };
        format!(
            "[Subagent completed] '{}' finished successfully ({}s).\n\nResult:\n{}",
            agent, elapsed_secs, truncated
        )
    } else {
        let error = event["error"].as_str().unwrap_or("unknown error");
        format!(
            "[Subagent failed] '{}' failed ({}s).\n\nError: {}",
            agent, elapsed_secs, error
        )
    }
}

/// GET /ws/chat — WebSocket upgrade for agent chat
pub async fn handle_ws_chat(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Auth via Authorization header or websocket protocol token.
    if state.pairing.require_pairing() {
        let token = extract_ws_bearer_token(&headers).unwrap_or_default();
        if !state.pairing.is_authenticated(&token) {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                "Unauthorized — provide Authorization: Bearer <token> or Sec-WebSocket-Protocol: zeroclaw.v1, bearer.<token>",
            )
                .into_response();
        }
    }

    ws.protocols([WS_CHAT_SUBPROTOCOL])
        .on_upgrade(move |socket| handle_socket(socket, state, addr))
        .into_response()
}

async fn handle_socket(mut socket: WebSocket, state: AppState, peer_addr: std::net::SocketAddr) {
    // Maintain conversation history for this WebSocket session
    let mut history: Vec<ChatMessage> = Vec::new();

    // Session tracking for intelligent memory extraction
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut turn_count: u32 = 0;

    // Build system prompt once for the session
    let system_prompt = {
        let config_guard = state.config.lock();
        crate::channels::build_system_prompt(
            &config_guard.workspace_dir,
            &state.model,
            &[],
            &[],
            Some(&config_guard.identity),
            None,
        )
    };

    // Add system message to history
    history.push(ChatMessage::system(&system_prompt));

    let approval_manager = {
        let config_guard = state.config.lock();
        ApprovalManager::from_config(&config_guard.autonomy)
    };

    let (chat_log_enabled, chat_log_dir) = {
        let config_guard = state.config.lock();
        let enabled = config_guard.gateway.chat_log;
        let dir = config_guard
            .config_path
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        (enabled, dir)
    };

    // Register session start for observation extraction
    {
        let config_guard = state.config.lock();
        if config_guard.memory.extraction_enabled {
            if let Ok(obs_conn) = crate::memory::observations::open_observations_db(
                &config_guard.workspace_dir,
            ) {
                let _ = crate::memory::observations::start_session(
                    &obs_conn, &session_id, "webchat",
                );
            }
        }
    }

    // Subscribe to subagent completion events
    let mut completion_rx = state.event_tx.subscribe();

    loop {
    // Use select! to listen for both incoming WS messages and subagent completions.
    // Both sources produce a `content: String` that gets fed into the agent loop.
    let content: String = tokio::select! {
        ws_msg = socket.recv() => {
            match ws_msg {
                Some(Ok(Message::Text(text))) => {
                    let parsed: serde_json::Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => {
                            let err = serde_json::json!({"type": "error", "message": "Invalid JSON"});
                            let _ = socket.send(Message::Text(err.to_string().into())).await;
                            continue;
                        }
                    };
                    let msg_type = parsed["type"].as_str().unwrap_or("");
                    if msg_type != "message" { continue; }
                    let c = parsed["content"].as_str().unwrap_or("").to_string();
                    if c.is_empty() { continue; }
                    c
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => continue,
            }
        }
        completion = completion_rx.recv() => {
            if let Ok(event) = completion {
                match event.get("type").and_then(|v| v.as_str()) {
                    Some("subagent_completed") => {
                        // Forward event to frontend
                        let _ = socket.send(Message::Text(event.to_string().into())).await;
                        // Build notification and feed into agent loop
                        build_subagent_notification(&event)
                    }
                    Some("subagent_tool_call" | "subagent_tool_result") => {
                        let _ = socket.send(Message::Text(event.to_string().into())).await;
                        continue;
                    }
                    _ => continue,
                }
            } else {
                continue;
            }
        }
    };

    {
        // Add user message to history
        history.push(ChatMessage::user(&content));
        turn_count += 1;

        if chat_log_enabled {
            append_chat_log(&chat_log_dir, &json!({
                "ts": chrono::Utc::now().to_rfc3339(),
                "role": "user",
                "content": content,
            }));
        }

        let msg_recv_time = std::time::Instant::now();
        tracing::info!(target: "ws_timing", peer = %peer_addr, content_len = content.len(), "📥 WS message received");

        // Get provider info
        let provider_label = state
            .config
            .lock()
            .default_provider
            .clone()
            .unwrap_or_else(|| "unknown".to_string());

        // Broadcast agent_start event
        let _ = state.event_tx.send(serde_json::json!({
            "type": "agent_start",
            "provider": provider_label,
            "model": state.model,
        }));

        // Run the agent loop with real-time delta streaming for web clients.
        tracing::info!(target: "ws_timing", provider = %provider_label, model = %state.model, history_len = history.len(), "🚀 Starting agent loop");

        let result = {
            let (delta_tx, mut delta_rx) = tokio::sync::mpsc::channel::<String>(128);
            let mut loop_future = std::pin::pin!(run_tool_call_loop(
                state.provider.as_ref(),
                &mut history,
                state.tools_registry_exec.as_ref(),
                state.observer.as_ref(),
                &provider_label,
                &state.model,
                state.temperature,
                true, // silent - no console output
                Some(&approval_manager),
                "webchat",
                &state.multimodal,
                state.max_tool_iterations,
                None,           // cancellation token
                Some(delta_tx), // delta streaming
                None,           // hooks
                &[],            // excluded tools
            ));

            loop {
                tokio::select! {
                    maybe_delta = delta_rx.recv() => {
                        if let Some(delta) = maybe_delta {
                            if let Some(event) = parse_ws_delta_event(&delta) {
                                emit_ws_delta_event(&mut socket, event).await;
                            }
                        } else {
                            break loop_future.await;
                        }
                    }
                    response = &mut loop_future => {
                        while let Ok(delta) = delta_rx.try_recv() {
                            if let Some(event) = parse_ws_delta_event(&delta) {
                                emit_ws_delta_event(&mut socket, event).await;
                            }
                        }
                        break response;
                    }
                }
            }
        };

        let elapsed_ms = msg_recv_time.elapsed().as_millis();
        tracing::info!(target: "ws_timing", elapsed_ms = elapsed_ms, result = matches!(result, Ok(_)), "✅ Agent loop completed");

        match result {
            Ok(response) => {
                let safe_response =
                    finalize_ws_response(&response, &history, state.tools_registry_exec.as_ref());
                // Add assistant response to history
                history.push(ChatMessage::assistant(&safe_response));

                if chat_log_enabled {
                    append_chat_log(&chat_log_dir, &json!({
                        "ts": chrono::Utc::now().to_rfc3339(),
                        "role": "assistant",
                        "content": safe_response,
                        "elapsed_ms": elapsed_ms,
                        "model": &state.model,
                    }));
                }

                // Send the full response as a done message
                let done = serde_json::json!({
                    "type": "done",
                    "full_response": safe_response,
                });
                let _ = socket.send(Message::Text(done.to_string().into())).await;

                // Broadcast agent_end event
                let _ = state.event_tx.send(serde_json::json!({
                    "type": "agent_end",
                    "provider": provider_label,
                    "model": state.model,
                }));
            }
            Err(e) => {
                let sanitized = crate::providers::sanitize_api_error(&e.to_string());

                if chat_log_enabled {
                    append_chat_log(&chat_log_dir, &json!({
                        "ts": chrono::Utc::now().to_rfc3339(),
                        "role": "error",
                        "content": sanitized,
                        "elapsed_ms": elapsed_ms,
                    }));
                }

                let err = serde_json::json!({
                    "type": "error",
                    "message": sanitized,
                });
                let _ = socket.send(Message::Text(err.to_string().into())).await;

                // Broadcast error event
                let _ = state.event_tx.send(serde_json::json!({
                    "type": "error",
                    "component": "ws_chat",
                    "message": sanitized,
                }));
            }
        }
    } // end inner block
    } // end loop

    // ── Session ended — spawn background observation extraction ──
    {
        let config_guard = state.config.lock();
        if config_guard.memory.extraction_enabled && turn_count > 0 {
            let history_snapshot = history.clone();
            let workspace_dir = config_guard.workspace_dir.clone();
            let session_id_clone = session_id.clone();
            let provider_clone = state.provider.clone();
            let model = if config_guard.memory.extraction_model.is_empty() {
                state.model.clone()
            } else {
                config_guard.memory.extraction_model.clone()
            };
            let temperature = config_guard.memory.extraction_temperature;
            let min_chars = config_guard.memory.extraction_min_transcript_chars;

            tokio::spawn(async move {
                match crate::memory::extraction::run_extraction(
                    &session_id_clone,
                    &history_snapshot,
                    provider_clone.as_ref(),
                    &model,
                    temperature,
                    min_chars,
                    &workspace_dir,
                )
                .await
                {
                    Ok(count) => {
                        if count > 0 {
                            tracing::info!(
                                session_id = %session_id_clone,
                                count,
                                "📝 Extracted observations from session"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            session_id = %session_id_clone,
                            error = %e,
                            "observation extraction failed"
                        );
                    }
                }
            });
        }
    }

    // Record session end
    {
        let config_guard = state.config.lock();
        if config_guard.memory.extraction_enabled {
            if let Ok(obs_conn) = crate::memory::observations::open_observations_db(
                &config_guard.workspace_dir,
            ) {
                let _ = crate::memory::observations::end_session(
                    &obs_conn, &session_id, turn_count,
                );
            }
        }
    }
}

fn extract_ws_bearer_token(headers: &HeaderMap) -> Option<String> {
    if let Some(auth_header) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        if let Some(token) = auth_header.strip_prefix("Bearer ") {
            if !token.trim().is_empty() {
                return Some(token.trim().to_string());
            }
        }
    }

    let offered = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())?;

    for protocol in offered.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(token) = protocol.strip_prefix("bearer.") {
            if !token.trim().is_empty() {
                return Some(token.trim().to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Tool, ToolResult};
    use async_trait::async_trait;
    use axum::http::HeaderValue;

    #[test]
    fn extract_ws_bearer_token_prefers_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer from-auth-header"),
        );
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("zeroclaw.v1, bearer.from-protocol"),
        );

        assert_eq!(
            extract_ws_bearer_token(&headers).as_deref(),
            Some("from-auth-header")
        );
    }

    #[test]
    fn parse_ws_delta_event_maps_tool_start() {
        let delta = format!("{DRAFT_PROGRESS_SENTINEL}⏳ shell: ls -la\n");
        assert_eq!(
            parse_ws_delta_event(&delta),
            Some(WsDeltaEvent::ToolCall {
                name: "shell".to_string(),
                hint: Some("ls -la".to_string()),
            })
        );
    }

    #[test]
    fn parse_ws_delta_event_maps_tool_success() {
        let delta = format!("{DRAFT_PROGRESS_SENTINEL}✅ shell (2s)\n");
        assert_eq!(
            parse_ws_delta_event(&delta),
            Some(WsDeltaEvent::ToolResult {
                name: "shell".to_string(),
                success: true,
                duration_secs: Some(2),
            })
        );
    }

    #[test]
    fn parse_ws_delta_event_treats_plain_text_as_chunk() {
        let delta = "partial response ".to_string();
        assert_eq!(
            parse_ws_delta_event(&delta),
            Some(WsDeltaEvent::ContentChunk(delta))
        );
    }

    #[test]
    fn extract_ws_bearer_token_reads_websocket_protocol_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("zeroclaw.v1, bearer.protocol-token"),
        );

        assert_eq!(
            extract_ws_bearer_token(&headers).as_deref(),
            Some("protocol-token")
        );
    }

    #[test]
    fn extract_ws_bearer_token_ignores_protocol_without_bearer_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("zeroclaw.v1"),
        );

        assert!(extract_ws_bearer_token(&headers).is_none());
    }

    #[test]
    fn extract_ws_bearer_token_rejects_empty_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer    "),
        );
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("zeroclaw.v1, bearer."),
        );

        assert!(extract_ws_bearer_token(&headers).is_none());
    }

    struct MockScheduleTool;

    #[async_trait]
    impl Tool for MockScheduleTool {
        fn name(&self) -> &str {
            "schedule"
        }

        fn description(&self) -> &str {
            "Mock schedule tool"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string" }
                }
            })
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: "ok".to_string(),
                error: None,
            })
        }
    }

    #[test]
    fn sanitize_ws_response_removes_tool_call_tags() {
        let input = r#"Before
<tool_call>
{"name":"schedule","arguments":{"action":"create"}}
</tool_call>
After"#;

        let result = sanitize_ws_response(input, &[]);
        let normalized = result
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(normalized, "Before\nAfter");
        assert!(!result.contains("<tool_call>"));
        assert!(!result.contains("\"name\":\"schedule\""));
    }

    #[test]
    fn sanitize_ws_response_removes_isolated_tool_json_artifacts() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockScheduleTool)];
        let input = r#"{"name":"schedule","parameters":{"action":"create"}}
{"result":{"status":"scheduled"}}
Reminder set successfully."#;

        let result = sanitize_ws_response(input, &tools);
        assert_eq!(result, "Reminder set successfully.");
        assert!(!result.contains("\"name\":\"schedule\""));
        assert!(!result.contains("\"result\""));
    }

    #[test]
    fn finalize_ws_response_uses_prompt_mode_tool_output_when_final_text_empty() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockScheduleTool)];
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(
                "[Tool results]\n<tool_result name=\"schedule\">\nDisk usage: 72%\n</tool_result>",
            ),
        ];

        let result = finalize_ws_response("", &history, &tools);
        assert!(result.contains("Latest tool output:"));
        assert!(result.contains("Disk usage: 72%"));
        assert!(!result.contains("<tool_result"));
    }

    #[test]
    fn finalize_ws_response_uses_native_tool_message_output_when_final_text_empty() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockScheduleTool)];
        let history = vec![ChatMessage {
            role: "tool".to_string(),
            content: r#"{"tool_call_id":"call_1","content":"Filesystem /dev/disk3s1: 210G free"}"#
                .to_string(),
        }];

        let result = finalize_ws_response("", &history, &tools);
        assert!(result.contains("Latest tool output:"));
        assert!(result.contains("/dev/disk3s1"));
    }

    #[test]
    fn finalize_ws_response_uses_static_fallback_when_nothing_available() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(MockScheduleTool)];
        let history = vec![ChatMessage::system("sys")];

        let result = finalize_ws_response("", &history, &tools);
        assert_eq!(result, EMPTY_WS_RESPONSE_FALLBACK);
    }
}
