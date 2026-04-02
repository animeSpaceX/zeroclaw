use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

pub struct TeamManagementTool {
    gateway_url: String,
    team_id: String,
    api_key: String,
}

impl TeamManagementTool {
    pub fn new(gateway_url: String, team_id: String) -> Self {
        let api_key = std::env::var("PLATFORM_API_KEY").unwrap_or_default();
        Self {
            gateway_url,
            team_id,
            api_key,
        }
    }

    fn base_url(&self) -> String {
        format!(
            "{}/api/teams/{}",
            self.gateway_url.trim_end_matches('/'),
            self.team_id
        )
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    async fn api_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> ToolResult {
        self.api_request_with_timeout(method, path, body, 15).await
    }

    async fn api_request_with_timeout(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
        timeout_secs: u64,
    ) -> ToolResult {
        let url = format!("{}{}", self.base_url(), path);
        let mut req = self
            .client()
            .request(method, &url)
            .header("X-API-Key", &self.api_key)
            .timeout(std::time::Duration::from_secs(timeout_secs));
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
impl Tool for TeamManagementTool {
    fn name(&self) -> &str {
        "team_management"
    }

    fn description(&self) -> &str {
        "Manage team agents. Actions: list_roles (view all available roles on the platform), list_agents (view current team members), add_agent (add a role), remove_agent, start_agent, stop_agent, dispatch (send a message to an agent for async execution)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list_roles", "list_agents", "add_agent", "remove_agent", "start_agent", "stop_agent", "dispatch"],
                    "description": "The operation to perform"
                },
                "role": {
                    "type": "string",
                    "description": "Agent role name (e.g. 'coder', 'researcher', 'designer')"
                },
                "message": {
                    "type": "string",
                    "description": "Message to dispatch to an agent (for dispatch action)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("");
        let role = args["role"].as_str().unwrap_or("");

        match action {
            "list_roles" => {
                // Call /api/registry (top-level, not team-scoped)
                let url = format!(
                    "{}/api/registry",
                    self.gateway_url.trim_end_matches('/')
                );
                let result = match self
                    .client()
                    .get(&url)
                    .header("X-API-Key", &self.api_key)
                    .timeout(std::time::Duration::from_secs(15))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        let text = r.text().await.unwrap_or_default();
                        ToolResult {
                            success: true,
                            output: text,
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
                };
                Ok(result)
            }

            "list_agents" => {
                Ok(self
                    .api_request(reqwest::Method::GET, "/agents", None)
                    .await)
            }

            "add_agent" => {
                if role.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("role is required for add_agent".to_string()),
                    });
                }
                Ok(self
                    .api_request(
                        reqwest::Method::POST,
                        &format!("/agents/{role}"),
                        Some(json!({})),
                    )
                    .await)
            }

            "remove_agent" => {
                if role.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("role is required for remove_agent".to_string()),
                    });
                }
                Ok(self
                    .api_request(reqwest::Method::DELETE, &format!("/agents/{role}"), None)
                    .await)
            }

            "start_agent" => {
                if role.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("role is required for start_agent".to_string()),
                    });
                }
                Ok(self
                    .api_request(
                        reqwest::Method::POST,
                        &format!("/agents/{role}/start"),
                        None,
                    )
                    .await)
            }

            "stop_agent" => {
                if role.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("role is required for stop_agent".to_string()),
                    });
                }
                Ok(self
                    .api_request(
                        reqwest::Method::POST,
                        &format!("/agents/{role}/stop"),
                        None,
                    )
                    .await)
            }

            "dispatch" => {
                if role.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("role is required for dispatch".to_string()),
                    });
                }
                let message = args["message"].as_str().unwrap_or("");
                if message.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("message is required for dispatch".to_string()),
                    });
                }
                // dispatch is synchronous on gateway side (waits for agent response up to 120s)
                Ok(self
                    .api_request_with_timeout(
                        reqwest::Method::POST,
                        &format!("/agents/{role}/dispatch"),
                        Some(json!({ "message": message })),
                        150,
                    )
                    .await)
            }

            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{action}'. Use: list_agents, add_agent, remove_agent, start_agent, stop_agent, dispatch"
                )),
            }),
        }
    }
}
