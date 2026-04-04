use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

const MAX_RESPONSE_SIZE: usize = 256 * 1024; // 256 KB

pub struct GatewayCallTool {
    gateway_url: String,
    agent_id: String,
    api_key: String,
}

impl GatewayCallTool {
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
impl Tool for GatewayCallTool {
    fn name(&self) -> &str {
        "gateway_call"
    }

    fn description(&self) -> &str {
        "Call any Gateway API endpoint. The tool automatically injects authentication headers — you only need to specify method, path, and optional body/query. Path must start with /api/ and must not access /api/admin/."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "method": {
                    "type": "string",
                    "enum": ["GET", "POST", "PATCH", "DELETE"],
                    "description": "HTTP method"
                },
                "path": {
                    "type": "string",
                    "description": "API path, must start with /api/"
                },
                "body": {
                    "type": "object",
                    "description": "Request body (for POST/PATCH)"
                },
                "query": {
                    "type": "object",
                    "description": "Query parameters as key-value pairs",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["method", "path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let method = args["method"].as_str().unwrap_or("");
        let path = args["path"].as_str().unwrap_or("");

        // Validate method
        if !matches!(method, "GET" | "POST" | "PATCH" | "DELETE") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Invalid method '{method}'. Must be GET, POST, PATCH, or DELETE.")),
            });
        }

        // Validate path
        if !path.starts_with("/api/") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Path must start with /api/".to_string()),
            });
        }
        if path.starts_with("/api/admin/") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Access to /api/admin/ is forbidden.".to_string()),
            });
        }
        if path.contains("..") {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Path must not contain '..'".to_string()),
            });
        }

        // Build URL with query params
        let base = self.gateway_url.trim_end_matches('/');
        let mut url = format!("{base}{path}");
        if let Some(query) = args["query"].as_object() {
            if !query.is_empty() {
                let pairs: Vec<String> = query
                    .iter()
                    .map(|(k, v)| {
                        let owned = v.to_string();
                        let val = v.as_str().unwrap_or(&owned);
                        format!(
                            "{}={}",
                            urlencoding::encode(k),
                            urlencoding::encode(val)
                        )
                    })
                    .collect();
                url.push('?');
                url.push_str(&pairs.join("&"));
            }
        }

        // Build request
        let client = reqwest::Client::new();
        let mut req = match method {
            "GET" => client.get(&url),
            "POST" => client.post(&url),
            "PATCH" => client.patch(&url),
            "DELETE" => client.delete(&url),
            _ => unreachable!(),
        };

        req = req
            .header("X-API-Key", &self.api_key)
            .header("X-Agent-Id", &self.agent_id)
            .timeout(std::time::Duration::from_secs(30));

        // Attach body for POST/PATCH
        if matches!(method, "POST" | "PATCH") {
            if let Some(body) = args.get("body") {
                if !body.is_null() {
                    req = req.json(body);
                }
            }
        }

        // Send
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                let body = if body.len() > MAX_RESPONSE_SIZE {
                    format!("{}... [truncated at 256KB]", &body[..MAX_RESPONSE_SIZE])
                } else {
                    body
                };
                let output = format!("Status: {status}\n\n{body}");
                Ok(ToolResult {
                    success: (200..300).contains(&(status as usize)),
                    output,
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            }),
        }
    }
}
