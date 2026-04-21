use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// Global registry for pending ask_user calls.
/// Key: ask_id (UUID string), Value: oneshot sender for delivering the user's answer.
///
/// This is global because Tool::execute() cannot receive extra parameters,
/// and ws.rs needs access to deliver answers from the WS layer.
static ASK_USER_REGISTRY: std::sync::LazyLock<
    Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
> = std::sync::LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

/// Deliver an answer to a pending ask_user call.
/// Returns true if the answer was delivered, false if no pending ask with that id.
pub async fn deliver_ask_user_answer(ask_id: &str, answer: serde_json::Value) -> bool {
    let tx = {
        let mut registry = ASK_USER_REGISTRY.lock().await;
        registry.remove(ask_id)
    };
    match tx {
        Some(tx) => tx.send(answer).is_ok(),
        None => false,
    }
}

/// Tool for asking the user structured questions via interactive cards in the chat UI.
///
/// Question types: single_select, multi_select, text_input, confirm, image_grid.
/// In web/gateway mode, the tool blocks (up to 10 minutes) waiting for the user's answer.
/// The actual question card is rendered by the frontend from the tool_call event's full args.
pub struct AskUserTool;

impl AskUserTool {
    pub fn new() -> Self {
        Self
    }
}

/// Format user answers into readable text for the LLM.
fn format_answers(questions: &[serde_json::Value], answers: &serde_json::Value) -> String {
    let mut lines = Vec::new();
    for q in questions {
        let qid = q["id"].as_str().unwrap_or("");
        let title = q["title"].as_str().unwrap_or(qid);
        let answer = &answers[qid];
        if answer.is_null() {
            continue;
        }
        let qtype = q["type"].as_str().unwrap_or("");
        let line = match qtype {
            "confirm" => {
                let confirmed = answer.as_bool().unwrap_or(false);
                let yes = q["confirm_text"].as_str().unwrap_or("确认");
                let no = q["cancel_text"].as_str().unwrap_or("取消");
                format!("{}: {}", title, if confirmed { yes } else { no })
            }
            "single_select" => {
                let selected_id = answer.as_str().unwrap_or("");
                let label = q["options"]
                    .as_array()
                    .and_then(|opts| {
                        opts.iter()
                            .find(|o| o["id"].as_str() == Some(selected_id))
                            .and_then(|o| o["label"].as_str())
                    })
                    .unwrap_or(selected_id);
                format!("{}: {}", title, label)
            }
            "multi_select" | "image_grid" => {
                let ids: Vec<&str> = answer
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                let labels: Vec<&str> = ids
                    .iter()
                    .map(|id| {
                        q["options"]
                            .as_array()
                            .and_then(|opts| {
                                opts.iter()
                                    .find(|o| o["id"].as_str() == Some(id))
                                    .and_then(|o| o["label"].as_str())
                            })
                            .unwrap_or(id)
                    })
                    .collect();
                format!("{}: {}", title, labels.join(", "))
            }
            _ => {
                format!("{}: {}", title, answer.as_str().unwrap_or(&answer.to_string()))
            }
        };
        lines.push(line);
    }
    lines.join("\n")
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user structured questions (single select, multi select, text input, confirm, image grid). \
         Questions are displayed as interactive cards in the chat UI. \
         The user's answers will arrive in the next message. Do not repeat the question after calling this tool."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "target_user_id": {
                    "type": "string",
                    "description": "Optional user ID. If set, only this user can answer; others see a read-only card."
                },
                "questions": {
                    "type": "array",
                    "description": "List of questions to present to the user",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Unique identifier for this question"
                            },
                            "type": {
                                "type": "string",
                                "enum": ["single_select", "multi_select", "text_input", "confirm", "image_grid"],
                                "description": "Question type"
                            },
                            "title": {
                                "type": "string",
                                "description": "Question title displayed to the user"
                            },
                            "description": {
                                "type": "string",
                                "description": "Optional detailed description shown below the title"
                            },
                            "required": {
                                "type": "boolean",
                                "description": "Whether an answer is required (default true)"
                            },
                            "options": {
                                "type": "array",
                                "description": "Options for single_select, multi_select, and image_grid types",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string", "description": "Option identifier" },
                                        "label": { "type": "string", "description": "Display text" },
                                        "description": { "type": "string", "description": "Optional description" },
                                        "image_url": { "type": "string", "description": "Image URL for image_grid" }
                                    },
                                    "required": ["id", "label"]
                                }
                            },
                            "placeholder": {
                                "type": "string",
                                "description": "Placeholder text for text_input"
                            },
                            "multiline": {
                                "type": "boolean",
                                "description": "Use multiline textarea for text_input (default false)"
                            },
                            "confirm_text": {
                                "type": "string",
                                "description": "Custom confirm button text (default '确认')"
                            },
                            "cancel_text": {
                                "type": "string",
                                "description": "Custom cancel button text (default '取消')"
                            },
                            "columns": {
                                "type": "integer",
                                "description": "Grid columns for image_grid (default 3)"
                            },
                            "max_select": {
                                "type": "integer",
                                "description": "Max selections for image_grid (default unlimited)"
                            }
                        },
                        "required": ["id", "type", "title"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let questions = match args.get("questions").and_then(|q| q.as_array()) {
            Some(q) if !q.is_empty() => q,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "questions array is required and must not be empty".to_string(),
                    ),
                });
            }
        };

        for (i, q) in questions.iter().enumerate() {
            if q.get("id").and_then(|v| v.as_str()).is_none() {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("questions[{i}] missing required field 'id'")),
                });
            }
            let qtype = match q.get("type").and_then(|v| v.as_str()) {
                Some(t) => t,
                None => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "questions[{i}] missing required field 'type'"
                        )),
                    });
                }
            };
            match qtype {
                "single_select" | "multi_select" | "image_grid" => {
                    if q.get("options")
                        .and_then(|v| v.as_array())
                        .map(|a| a.is_empty())
                        .unwrap_or(true)
                    {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "questions[{i}] of type '{qtype}' requires non-empty 'options'"
                            )),
                        });
                    }
                }
                "text_input" | "confirm" => {}
                other => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "questions[{i}] has unknown type '{other}'. Valid: single_select, multi_select, text_input, confirm, image_grid"
                        )),
                    });
                }
            }
            if q.get("title").and_then(|v| v.as_str()).is_none() {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "questions[{i}] missing required field 'title'"
                    )),
                });
            }
        }

        // Check if there's a pre-registered ask_id (injected by loop_.rs before execute).
        // If present, block waiting for the user's answer.
        let ask_id = args
            .get("_ask_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let Some(ask_id) = ask_id {
            // Register in global registry and block
            let (tx, rx) = oneshot::channel();
            {
                let mut registry = ASK_USER_REGISTRY.lock().await;
                registry.insert(ask_id.clone(), tx);
            }

            tracing::info!(ask_id = %ask_id, "ask_user: blocking, waiting for user answer (10 min timeout)");

            match tokio::time::timeout(std::time::Duration::from_secs(600), rx).await {
                Ok(Ok(answer)) => {
                    // Clean up (already removed by deliver)
                    let answer_text = format_answers(questions, &answer);
                    tracing::info!(ask_id = %ask_id, "ask_user: received user answer");
                    Ok(ToolResult {
                        success: true,
                        output: format!("用户已回答：\n{}", answer_text),
                        error: None,
                    })
                }
                Ok(Err(_)) => {
                    // Channel closed (e.g. WS disconnected)
                    let mut registry = ASK_USER_REGISTRY.lock().await;
                    registry.remove(&ask_id);
                    Ok(ToolResult {
                        success: true,
                        output: "用户未回答（连接已断开），请直接继续或换种方式处理。".to_string(),
                        error: None,
                    })
                }
                Err(_) => {
                    // Timeout
                    let mut registry = ASK_USER_REGISTRY.lock().await;
                    registry.remove(&ask_id);
                    tracing::info!(ask_id = %ask_id, "ask_user: timed out after 10 minutes");
                    Ok(ToolResult {
                        success: true,
                        output: "用户在 10 分钟内未回答，请直接继续或换种方式处理。".to_string(),
                        error: None,
                    })
                }
            }
        } else {
            // No ask_id: non-blocking mode (CLI or legacy), return immediately
            Ok(ToolResult {
                success: true,
                output: "已向用户展示问题卡片，请等待用户在下一条消息中回复。不要重复提问。"
                    .to_string(),
                error: None,
            })
        }
    }
}
