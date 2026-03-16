use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

const MAX_READ_BYTES: usize = 512 * 1024; // 512 KB

pub struct StorageTool {
    workspace_dir: PathBuf,
    gateway_url: String,
    team_id: String,
    key_prefix: String,
}

impl StorageTool {
    pub fn new(workspace_dir: PathBuf, gateway_url: String, team_id: String) -> Self {
        let key_prefix = std::env::var("AGENT_ROLE").unwrap_or_default();
        Self {
            workspace_dir,
            gateway_url,
            team_id,
            key_prefix,
        }
    }

    /// Prefix the key with agent role directory if AGENT_ROLE is set.
    fn prefixed_key(&self, key: &str) -> String {
        if self.key_prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.key_prefix, key)
        }
    }

    async fn do_list(&self, prefix: &str) -> ToolResult {
        let url = format!(
            "{}/api/teams/{}/storage?prefix={}",
            self.gateway_url,
            self.team_id,
            urlencoding::encode(prefix)
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
                        return ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to parse response: {e}")),
                        };
                    }
                };

                let objects = body.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
                if objects.is_empty() {
                    return ToolResult {
                        success: true,
                        output: if prefix.is_empty() {
                            "Team storage is empty.".to_string()
                        } else {
                            format!("No files found with prefix '{prefix}'.")
                        },
                        error: None,
                    };
                }

                let mut lines = vec![format!("{:<40} {:>10} {}", "KEY", "SIZE", "MODIFIED")];
                lines.push("-".repeat(60));
                for obj in objects {
                    let key = obj["key"].as_str().unwrap_or("?");
                    let size = obj["size"].as_u64().unwrap_or(0);
                    let modified = obj["last_modified"].as_str().unwrap_or("");
                    lines.push(format!("{:<40} {:>10} {}", key, format_size(size), modified));
                }
                lines.push(format!("\nTotal: {} file(s)", objects.len()));

                ToolResult {
                    success: true,
                    output: lines.join("\n"),
                    error: None,
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("List failed ({status}): {text}")),
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            },
        }
    }

    async fn do_read(&self, key: &str) -> ToolResult {
        let url = format!(
            "{}/api/teams/{}/storage/file/{}",
            self.gateway_url,
            self.team_id,
            urlencoding::encode(key)
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
                        return ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to read response: {e}")),
                        };
                    }
                };

                let size = body.len();
                if size > MAX_READ_BYTES {
                    return ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "File too large to read inline ({size} bytes, max {MAX_READ_BYTES}). Use action=download instead."
                        )),
                    };
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
                        Ok(text) => ToolResult {
                            success: true,
                            output: format!("=== {key} ({size} bytes) ===\n{text}"),
                            error: None,
                        },
                        Err(_) => ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "Binary file ({content_type}, {size} bytes). Use action=download to save locally."
                            )),
                        },
                    }
                } else {
                    ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Binary file ({content_type}, {size} bytes). Use action=download to save locally."
                        )),
                    }
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Read failed ({status}): {text}")),
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            },
        }
    }

    async fn do_write(&self, key: &str, content: &str) -> ToolResult {
        use base64::Engine;
        let key = self.prefixed_key(key);
        let content_b64 =
            base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        let content_type = mime_from_key(&key);
        let size = content.len();

        let url = format!(
            "{}/api/teams/{}/storage/upload",
            self.gateway_url, self.team_id
        );

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .json(&json!({
                "key": key,
                "content_base64": content_b64,
                "content_type": content_type,
            }))
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => ToolResult {
                success: true,
                output: format!("Written '{key}' to storage ({size} bytes)"),
                error: None,
            },
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Write failed ({status}): {text}")),
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            },
        }
    }

    async fn do_upload(&self, local_path: &str, remote_key: &str) -> ToolResult {
        if local_path.contains("..") || local_path.contains('\0') {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some("Invalid path: contains '..' or null bytes".to_string()),
            };
        }

        let full_path = self.workspace_dir.join(local_path);
        if !full_path.exists() {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("File not found: {local_path}")),
            };
        }

        let body = match tokio::fs::read(&full_path).await {
            Ok(b) => b,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read file: {e}")),
                };
            }
        };

        let key = if remote_key.is_empty() {
            self.prefixed_key(
                std::path::Path::new(local_path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(local_path),
            )
        } else {
            self.prefixed_key(remote_key)
        };

        use base64::Engine;
        let content_b64 = base64::engine::general_purpose::STANDARD.encode(&body);
        let content_type = mime_from_key(&key);
        let size = body.len();

        let url = format!(
            "{}/api/teams/{}/storage/upload",
            self.gateway_url, self.team_id
        );

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .json(&json!({
                "key": key,
                "content_base64": content_b64,
                "content_type": content_type,
            }))
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => ToolResult {
                success: true,
                output: format!("Uploaded '{local_path}' to storage as '{key}' ({size} bytes)"),
                error: None,
            },
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Upload failed ({status}): {text}")),
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            },
        }
    }

    async fn do_download(&self, key: &str, local_path: &str) -> ToolResult {
        let dest_path = if local_path.is_empty() {
            let filename = std::path::Path::new(key)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(key);
            format!("storage/{filename}")
        } else {
            local_path.to_string()
        };

        if dest_path.contains("..") || dest_path.contains('\0') {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some("Invalid local_path: contains '..' or null bytes".to_string()),
            };
        }

        let url = format!(
            "{}/api/teams/{}/storage/file/{}",
            self.gateway_url,
            self.team_id,
            urlencoding::encode(key)
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
                        return ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to read response: {e}")),
                        };
                    }
                };

                let dest = self.workspace_dir.join(&dest_path);
                if let Some(parent) = dest.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }

                match tokio::fs::write(&dest, &body).await {
                    Ok(()) => ToolResult {
                        success: true,
                        output: format!(
                            "Downloaded '{}' to '{}' ({} bytes)",
                            key,
                            dest_path,
                            body.len()
                        ),
                        error: None,
                    },
                    Err(e) => ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to write file: {e}")),
                    },
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Download failed ({status}): {text}")),
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            },
        }
    }

    async fn do_delete(&self, key: &str) -> ToolResult {
        let url = format!(
            "{}/api/teams/{}/storage/file/{}",
            self.gateway_url,
            self.team_id,
            urlencoding::encode(key)
        );

        let client = reqwest::Client::new();
        let resp = client
            .delete(&url)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => ToolResult {
                success: true,
                output: format!("Deleted '{key}' from storage"),
                error: None,
            },
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Delete failed ({status}): {text}")),
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
impl Tool for StorageTool {
    fn name(&self) -> &str {
        "storage"
    }

    fn description(&self) -> &str {
        "Team cloud storage. Actions: list (list files), read (view file content), write (create/update file directly), upload (workspace file → storage), download (storage → workspace), delete (remove file)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "read", "write", "upload", "download", "delete"],
                    "description": "Action to perform"
                },
                "key": {
                    "type": "string",
                    "description": "Storage key (e.g. 'docs/report.md'). Required for read/write/upload/download/delete."
                },
                "content": {
                    "type": "string",
                    "description": "File content to write. Required for action=write."
                },
                "local_path": {
                    "type": "string",
                    "description": "Workspace-relative path. Required for action=upload, optional for action=download."
                },
                "prefix": {
                    "type": "string",
                    "description": "Filter prefix for action=list (e.g. 'docs/'). Optional."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("").to_string();
        let key = args["key"].as_str().unwrap_or("").to_string();

        match action.as_str() {
            "list" => {
                let prefix = args["prefix"].as_str().unwrap_or("");
                Ok(self.do_list(prefix).await)
            }
            "read" => {
                if key.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("key is required for action=read".to_string()),
                    });
                }
                Ok(self.do_read(&key).await)
            }
            "write" => {
                if key.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("key is required for action=write".to_string()),
                    });
                }
                let content = args["content"].as_str().unwrap_or("");
                if content.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("content is required for action=write".to_string()),
                    });
                }
                Ok(self.do_write(&key, content).await)
            }
            "upload" => {
                let local_path = args["local_path"].as_str().unwrap_or("");
                if local_path.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("local_path is required for action=upload".to_string()),
                    });
                }
                Ok(self.do_upload(local_path, &key).await)
            }
            "download" => {
                if key.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("key is required for action=download".to_string()),
                    });
                }
                let local_path = args["local_path"].as_str().unwrap_or("");
                Ok(self.do_download(&key, local_path).await)
            }
            "delete" => {
                if key.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("key is required for action=delete".to_string()),
                    });
                }
                Ok(self.do_delete(&key).await)
            }
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{action}'. Use: list, read, write, upload, download, delete"
                )),
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

fn mime_from_key(key: &str) -> String {
    let ext = std::path::Path::new(key)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "md" => "text/markdown",
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "csv" => "text/csv",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "pdf" => "application/pdf",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "js" => "application/javascript",
        "ts" | "tsx" => "text/typescript",
        _ => "application/octet-stream",
    }
    .to_string()
}
