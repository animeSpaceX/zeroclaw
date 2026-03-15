use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

const MAX_READ_BYTES: usize = 512 * 1024; // 512 KB

pub struct StorageReadTool {
    gateway_url: String,
    team_id: String,
}

impl StorageReadTool {
    pub fn new(gateway_url: String, team_id: String) -> Self {
        Self {
            gateway_url,
            team_id,
        }
    }
}

#[async_trait]
impl Tool for StorageReadTool {
    fn name(&self) -> &str {
        "storage_read"
    }

    fn description(&self) -> &str {
        "Read a file's content directly from team cloud storage without downloading to workspace. Best for viewing text files. Use storage_download instead if you need to edit the file locally."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Storage key of the file to read (e.g. 'docs/test-report.md')"
                }
            },
            "required": ["key"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args["key"].as_str().unwrap_or("").to_string();

        if key.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("key is required".to_string()),
            });
        }

        let url = format!(
            "{}/api/teams/{}/storage/file/{}",
            self.gateway_url,
            self.team_id,
            urlencoding::encode(&key)
        );

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let content_type = r
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();

                let body = match r.bytes().await {
                    Ok(b) => b,
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to read response: {e}")),
                        });
                    }
                };

                let size = body.len();

                if size > MAX_READ_BYTES {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "File too large to read inline ({} bytes, max {}). Use storage_download instead.",
                            size, MAX_READ_BYTES
                        )),
                    });
                }

                let is_text = content_type.starts_with("text/")
                    || content_type.contains("json")
                    || content_type.contains("xml")
                    || content_type.contains("yaml")
                    || content_type.contains("toml")
                    || content_type.contains("javascript")
                    || content_type.contains("markdown");

                if is_text || content_type.is_empty() {
                    match String::from_utf8(body.to_vec()) {
                        Ok(text) => Ok(ToolResult {
                            success: true,
                            output: format!(
                                "=== {} ({} bytes) ===\n{}",
                                key, size, text
                            ),
                            error: None,
                        }),
                        Err(_) => Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "Binary file ({}, {} bytes). Use storage_download to save locally.",
                                content_type, size
                            )),
                        }),
                    }
                } else {
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Binary file ({}, {} bytes). Use storage_download to save locally.",
                            content_type, size
                        )),
                    })
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Read failed ({status}): {text}")),
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
