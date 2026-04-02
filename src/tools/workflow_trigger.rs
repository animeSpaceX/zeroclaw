use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

/// Tool for triggering workflows in other channels, enabling cross-channel
/// cascade automation (e.g. A → B → C pipelines).
pub struct WorkflowTriggerTool {
    gateway_url: String,
    team_id: String,
    api_key: String,
}

impl WorkflowTriggerTool {
    pub fn new(gateway_url: String, team_id: String) -> Self {
        let api_key = std::env::var("PLATFORM_API_KEY").unwrap_or_default();
        Self {
            gateway_url,
            team_id,
            api_key,
        }
    }

    async fn api_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> ToolResult {
        let url = format!("{}{}", self.gateway_url.trim_end_matches('/'), path);
        let mut req = reqwest::Client::new()
            .request(method, &url)
            .header("X-API-Key", &self.api_key)
            .timeout(std::time::Duration::from_secs(30));
        if let Some(b) = body {
            req = req.json(&b);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: true,
                    output: if text.is_empty() {
                        "OK".to_string()
                    } else {
                        text
                    },
                    error: None,
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("API error ({status}): {text}")),
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            },
        }
    }
}

#[async_trait]
impl Tool for WorkflowTriggerTool {
    fn name(&self) -> &str {
        "workflow_trigger"
    }

    fn description(&self) -> &str {
        "Trigger a workflow in another channel. Used for cross-channel cascade automation."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["trigger"],
                    "description": "Action to perform. Currently only 'trigger' is supported."
                },
                "target_conversation_id": {
                    "type": "string",
                    "description": "The conversation_id of the target channel to trigger"
                },
                "message": {
                    "type": "string",
                    "description": "Optional message to pass to the target channel's workflow"
                }
            },
            "required": ["action", "target_conversation_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("");
        match action {
            "trigger" => {
                let target_id = match args["target_conversation_id"].as_str() {
                    Some(id) if !id.is_empty() => id,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "target_conversation_id is required".to_string(),
                            ),
                        })
                    }
                };
                let message = args["message"].as_str().map(|s| s.to_string());

                let mut body = json!({
                    "target_conversation_id": target_id,
                });
                if let Some(msg) = message {
                    body["message"] = json!(msg);
                }

                Ok(self.api_request(
                    reqwest::Method::POST,
                    "/api/workflows/trigger",
                    Some(body),
                )
                .await)
            }
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{}'. Available: trigger",
                    action
                )),
            }),
        }
    }
}
