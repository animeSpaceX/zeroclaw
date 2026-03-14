use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

pub struct StorageDownloadTool {
    workspace_dir: PathBuf,
    gateway_url: String,
    team_id: String,
}

impl StorageDownloadTool {
    pub fn new(workspace_dir: PathBuf, gateway_url: String, team_id: String) -> Self {
        Self {
            workspace_dir,
            gateway_url,
            team_id,
        }
    }
}

#[async_trait]
impl Tool for StorageDownloadTool {
    fn name(&self) -> &str {
        "storage_download"
    }

    fn description(&self) -> &str {
        "Download a file from the team's cloud storage to your local workspace. Use storage_list to see available files first."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "remote_key": {
                    "type": "string",
                    "description": "Storage key of the file to download (e.g. 'reports/output.md')"
                },
                "local_path": {
                    "type": "string",
                    "description": "Where to save the file, relative to workspace. Defaults to 'storage/<filename>'"
                }
            },
            "required": ["remote_key"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let remote_key = args["remote_key"]
            .as_str()
            .unwrap_or("")
            .to_string();

        if remote_key.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("remote_key is required".to_string()),
            });
        }

        let local_path = args["local_path"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let filename = std::path::Path::new(&remote_key)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&remote_key);
                format!("storage/{filename}")
            });

        // Validate write path
        if local_path.contains("..") || local_path.contains('\0') {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Invalid local_path: contains '..' or null bytes".to_string()),
            });
        }

        let url = format!(
            "{}/api/teams/{}/storage/file/{}",
            self.gateway_url,
            self.team_id,
            urlencoding::encode(&remote_key)
        );

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
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

                let dest = self.workspace_dir.join(&local_path);

                // Create parent directories
                if let Some(parent) = dest.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }

                match tokio::fs::write(&dest, &body).await {
                    Ok(()) => Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Downloaded '{}' to '{}' ({} bytes)",
                            remote_key,
                            local_path,
                            body.len()
                        ),
                        error: None,
                    }),
                    Err(e) => Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to write file: {e}")),
                    }),
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Download failed ({status}): {text}")),
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
