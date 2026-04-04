use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

/// Tool for managing cron jobs on the agent's channel.
/// Supports list, create, get, update, delete actions via gateway CRUD API.
pub struct CronManageTool {
    gateway_url: String,
    agent_id: String,
    api_key: String,
}

impl CronManageTool {
    pub fn new(gateway_url: String, agent_id: String) -> Self {
        let api_key = std::env::var("PLATFORM_API_KEY").unwrap_or_default();
        Self {
            gateway_url,
            agent_id,
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
            .header("X-Agent-Id", &self.agent_id)
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

    fn base_path(&self) -> String {
        format!("/api/agents/{}/cron", self.agent_id)
    }
}

#[async_trait]
impl Tool for CronManageTool {
    fn name(&self) -> &str {
        "cron_manage"
    }

    fn description(&self) -> &str {
        "Manage cron jobs (scheduled tasks) for this agent's channel. Actions: list, create, get, update, delete."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "create", "get", "update", "delete"],
                    "description": "Action to perform on cron jobs."
                },
                "cron_id": {
                    "type": "integer",
                    "description": "Cron job ID. Required for get/update/delete."
                },
                "schedule": {
                    "type": "string",
                    "description": "5-field cron expression, e.g. '0 9 * * 1-5'. Required for create."
                },
                "name": {
                    "type": "string",
                    "description": "Human-readable name for the cron job."
                },
                "timezone": {
                    "type": "string",
                    "description": "IANA timezone, e.g. 'Asia/Shanghai'. Defaults to 'UTC'."
                },
                "message": {
                    "type": "string",
                    "description": "Message sent to the agent when the cron triggers."
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Enable or disable the cron job. Used with update."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("");
        match action {
            "list" => {
                Ok(self.api_request(reqwest::Method::GET, &self.base_path(), None).await)
            }
            "create" => {
                let schedule = match args["schedule"].as_str() {
                    Some(s) if !s.is_empty() => s,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("'schedule' is required for create".to_string()),
                        });
                    }
                };
                let mut body = json!({ "schedule": schedule });
                if let Some(v) = args["name"].as_str() { body["name"] = json!(v); }
                if let Some(v) = args["timezone"].as_str() { body["timezone"] = json!(v); }
                if let Some(v) = args["message"].as_str() { body["message"] = json!(v); }
                Ok(self.api_request(reqwest::Method::POST, &self.base_path(), Some(body)).await)
            }
            "get" => {
                let cron_id = match args["cron_id"].as_i64() {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("'cron_id' is required for get".to_string()),
                        });
                    }
                };
                let path = format!("{}/{}", self.base_path(), cron_id);
                Ok(self.api_request(reqwest::Method::GET, &path, None).await)
            }
            "update" => {
                let cron_id = match args["cron_id"].as_i64() {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("'cron_id' is required for update".to_string()),
                        });
                    }
                };
                let mut body = json!({});
                if let Some(v) = args["schedule"].as_str() { body["schedule"] = json!(v); }
                if let Some(v) = args["name"].as_str() { body["name"] = json!(v); }
                if let Some(v) = args["timezone"].as_str() { body["timezone"] = json!(v); }
                if let Some(v) = args["message"].as_str() { body["message"] = json!(v); }
                if let Some(v) = args["enabled"].as_bool() { body["enabled"] = json!(v); }
                let path = format!("{}/{}", self.base_path(), cron_id);
                Ok(self.api_request(reqwest::Method::PATCH, &path, Some(body)).await)
            }
            "delete" => {
                let cron_id = match args["cron_id"].as_i64() {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("'cron_id' is required for delete".to_string()),
                        });
                    }
                };
                let path = format!("{}/{}", self.base_path(), cron_id);
                Ok(self.api_request(reqwest::Method::DELETE, &path, None).await)
            }
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{}'. Available: list, create, get, update, delete",
                    action
                )),
            }),
        }
    }
}
