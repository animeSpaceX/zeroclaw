use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

/// Tool for asking the user structured questions via interactive cards in the chat UI.
///
/// Question types: single_select, multi_select, text_input, confirm, image_grid.
/// The tool validates the question schema and returns immediately.
/// The actual question card is rendered by the frontend from the tool_call event's full args.
/// The user's answer arrives as the next regular chat message.
pub struct AskUserTool;

impl AskUserTool {
    pub fn new() -> Self {
        Self
    }
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

        Ok(ToolResult {
            success: true,
            output: "已向用户展示问题卡片，请等待用户在下一条消息中回复。不要重复提问。"
                .to_string(),
            error: None,
        })
    }
}
