use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

pub struct StorageUploadTool {
    workspace_dir: PathBuf,
    gateway_url: String,
    team_id: String,
}

impl StorageUploadTool {
    pub fn new(workspace_dir: PathBuf, gateway_url: String, team_id: String) -> Self {
        Self {
            workspace_dir,
            gateway_url,
            team_id,
        }
    }
}

#[async_trait]
impl Tool for StorageUploadTool {
    fn name(&self) -> &str {
        "storage_upload"
    }

    fn description(&self) -> &str {
        "Upload a file from workspace to the team's cloud storage. Other team members can then download it via storage_download. Returns the storage key for referencing the uploaded file."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "local_path": {
                    "type": "string",
                    "description": "Path to the file to upload, relative to your workspace directory"
                },
                "remote_key": {
                    "type": "string",
                    "description": "Storage key for the uploaded file (e.g. 'reports/output.md'). Defaults to the filename from local_path"
                }
            },
            "required": ["local_path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let local_path = args["local_path"]
            .as_str()
            .unwrap_or("")
            .to_string();

        if local_path.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("local_path is required".to_string()),
            });
        }

        // Validate path via SecurityPolicy
        if local_path.contains("..") || local_path.contains('\0') {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Invalid path: contains '..' or null bytes".to_string()),
            });
        }

        // Read the file from workspace
        let full_path = self.workspace_dir.join(&local_path);

        if !full_path.exists() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("File not found: {local_path}")),
            });
        }

        let body = match tokio::fs::read(&full_path).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read file: {e}")),
                });
            }
        };

        let remote_key = args["remote_key"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::path::Path::new(&local_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&local_path)
            })
            .to_string();

        // Base64 encode and POST to gateway
        use base64::Engine;
        let content_b64 = base64::engine::general_purpose::STANDARD.encode(&body);

        let content_type = mime_from_ext(&full_path);

        let url = format!(
            "{}/api/teams/{}/storage/upload",
            self.gateway_url, self.team_id
        );

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .json(&json!({
                "key": remote_key,
                "content_base64": content_b64,
                "content_type": content_type,
            }))
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let size = body.len();
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Uploaded '{local_path}' to storage as '{remote_key}' ({size} bytes)"
                    ),
                    error: None,
                })
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Upload failed ({status}): {text}")),
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

fn mime_from_ext(path: &std::path::Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some("md") => "text/markdown",
        Some("txt") => "text/plain",
        Some("html") | Some("htm") => "text/html",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("pdf") => "application/pdf",
        Some("rs") => "text/x-rust",
        Some("py") => "text/x-python",
        Some("js") => "application/javascript",
        Some("ts") | Some("tsx") => "text/typescript",
        _ => "application/octet-stream",
    }
    .to_string()
}
