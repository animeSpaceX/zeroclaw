use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

pub struct KnowledgeSearchTool {
    gateway_url: String,
    agent_id: String,
    api_key: String,
}

impl KnowledgeSearchTool {
    pub fn new(gateway_url: String, agent_id: String) -> Self {
        let api_key = std::env::var("PLATFORM_API_KEY").unwrap_or_default();
        Self {
            gateway_url,
            agent_id,
            api_key,
        }
    }
}

#[async_trait]
impl Tool for KnowledgeSearchTool {
    fn name(&self) -> &str {
        "knowledge_search"
    }

    fn description(&self) -> &str {
        "Search the organization knowledge base for background information, historical decisions, \
         technical solutions, and past discussion conclusions. Use when you need context that may \
         have been documented or discussed before."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query — keywords or a natural language question"
                },
                "top_k": {
                    "type": "integer",
                    "description": "Number of results to return (default 5, max 20)",
                    "default": 5
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args["query"].as_str().unwrap_or("");
        if query.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("query parameter is required".to_string()),
            });
        }

        let top_k = args["top_k"].as_i64().unwrap_or(5).min(20);

        let url = format!(
            "{}/api/agents/{}/knowledge-search",
            self.gateway_url.trim_end_matches('/'),
            self.agent_id,
        );

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("X-API-Key", &self.api_key)
            .header("X-Agent-Id", &self.agent_id)
            .json(&json!({ "query": query, "limit": top_k }))
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        match resp {
            Ok(r) => {
                if r.status().is_success() {
                    let body: serde_json::Value = r.json().await.unwrap_or(json!({}));
                    let result_text = body["result"].as_str().unwrap_or("No results found.");
                    Ok(ToolResult {
                        success: true,
                        output: result_text.to_string(),
                        error: None,
                    })
                } else {
                    let status = r.status();
                    let err = r.text().await.unwrap_or_default();
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Knowledge search returned {}: {}", status, &err[..err.len().min(200)])),
                    })
                }
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Knowledge search request failed: {}", e)),
            }),
        }
    }
}
