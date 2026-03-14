use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

pub struct StorageListTool {
    gateway_url: String,
    team_id: String,
}

impl StorageListTool {
    pub fn new(gateway_url: String, team_id: String) -> Self {
        Self {
            gateway_url,
            team_id,
        }
    }
}

#[async_trait]
impl Tool for StorageListTool {
    fn name(&self) -> &str {
        "storage_list"
    }

    fn description(&self) -> &str {
        "List files in the team's cloud storage. All team members share this storage space."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prefix": {
                    "type": "string",
                    "description": "Filter files by key prefix (e.g. 'reports/' to list only reports). Leave empty to list all files."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let prefix = args["prefix"].as_str().unwrap_or("").to_string();

        let url = format!(
            "{}/api/teams/{}/storage?prefix={}",
            self.gateway_url,
            self.team_id,
            urlencoding::encode(&prefix)
        );

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to parse response: {e}")),
                        });
                    }
                };

                let objects = body.as_array().map(|arr| arr.as_slice()).unwrap_or(&[]);

                if objects.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: if prefix.is_empty() {
                            "Team storage is empty.".to_string()
                        } else {
                            format!("No files found with prefix '{prefix}'.")
                        },
                        error: None,
                    });
                }

                let mut lines = vec![format!("{:<40} {:>10} {}", "KEY", "SIZE", "MODIFIED")];
                lines.push("-".repeat(60));

                for obj in objects {
                    let key = obj["key"].as_str().unwrap_or("?");
                    let size = obj["size"].as_u64().unwrap_or(0);
                    let modified = obj["last_modified"].as_str().unwrap_or("");
                    let size_str = format_size(size);
                    lines.push(format!("{:<40} {:>10} {}", key, size_str, modified));
                }

                lines.push(format!("\nTotal: {} file(s)", objects.len()));

                Ok(ToolResult {
                    success: true,
                    output: lines.join("\n"),
                    error: None,
                })
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("List failed ({status}): {text}")),
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

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
