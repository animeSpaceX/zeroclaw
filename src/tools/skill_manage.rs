use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

pub struct SkillManageTool {
    gateway_url: String,
    api_key: String,
    agent_id: String,
    skills_dir: PathBuf,
}

impl SkillManageTool {
    pub fn new(gateway_url: String, agent_id: String, workspace_dir: PathBuf) -> Self {
        let api_key = std::env::var("PLATFORM_API_KEY").unwrap_or_default();
        let skills_dir = workspace_dir.join("skills");
        Self {
            gateway_url,
            api_key,
            agent_id,
            skills_dir,
        }
    }

    /// Trigger self-restart via gateway (1s delay to let tool_result flush)
    fn schedule_restart(&self) {
        let url = format!(
            "{}/api/agents/{}/restart-self",
            self.gateway_url, self.agent_id
        );
        let api_key = self.api_key.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let client = reqwest::Client::new();
            let resp = client
                .post(&url)
                .header("X-API-Key", &api_key)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    tracing::info!("Agent self-restart triggered after skill install");
                }
                Ok(r) => {
                    let text = r.text().await.unwrap_or_default();
                    tracing::warn!("Self-restart failed: {text}");
                }
                Err(e) => {
                    tracing::warn!("Self-restart request failed: {e}");
                }
            }
        });
    }

    /// GET /api/skills — list available skills from platform
    async fn do_discover(&self) -> ToolResult {
        let url = format!("{}/api/skills", self.gateway_url);
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .timeout(std::time::Duration::from_secs(15))
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

                let skills = body["skills"].as_array();
                match skills {
                    Some(arr) if arr.is_empty() => ToolResult {
                        success: true,
                        output: "No skills available in the platform skill library.".to_string(),
                        error: None,
                    },
                    Some(arr) => {
                        let mut lines = vec!["Available skills:".to_string()];
                        for skill in arr {
                            let name = skill["name"].as_str().unwrap_or("?");
                            let desc = skill["description"].as_str().unwrap_or("");
                            if desc.is_empty() {
                                lines.push(format!("  - {name}"));
                            } else {
                                lines.push(format!("  - {name}: {desc}"));
                            }
                        }
                        ToolResult {
                            success: true,
                            output: lines.join("\n"),
                            error: None,
                        }
                    }
                    None => ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Unexpected response format".to_string()),
                    },
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Request failed ({status}): {text}")),
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            },
        }
    }

    /// Scan local workspace/skills/ directory
    async fn do_list(&self) -> ToolResult {
        if !self.skills_dir.exists() {
            return ToolResult {
                success: true,
                output: "No skills currently installed.".to_string(),
                error: None,
            };
        }

        let mut entries = match tokio::fs::read_dir(&self.skills_dir).await {
            Ok(e) => e,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read skills directory: {e}")),
                };
            }
        };

        let mut installed = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry.path().is_dir() {
                continue;
            }
            let skill_md = entry.path().join("SKILL.md");
            if skill_md.exists() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Parse first description line from frontmatter
                let desc = match tokio::fs::read_to_string(&skill_md).await {
                    Ok(content) => parse_description(&content).unwrap_or_default(),
                    Err(_) => String::new(),
                };
                if desc.is_empty() {
                    installed.push(format!("  - {name}"));
                } else {
                    installed.push(format!("  - {name}: {desc}"));
                }
            }
        }

        if installed.is_empty() {
            ToolResult {
                success: true,
                output: "No skills currently installed.".to_string(),
                error: None,
            }
        } else {
            let mut lines = vec!["Installed skills:".to_string()];
            lines.extend(installed);
            ToolResult {
                success: true,
                output: lines.join("\n"),
                error: None,
            }
        }
    }

    /// Download all skill files from gateway and write to local workspace
    async fn do_install(&self, skill_name: &str) -> ToolResult {
        // GET /api/skills/{name}/content — returns JSON { "files": { "path": "content", ... } }
        let url = format!("{}/api/skills/{}/content", self.gateway_url, skill_name);
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        let body: serde_json::Value = match resp {
            Ok(r) if r.status().is_success() => {
                match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        return ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("Failed to parse response: {e}")),
                        };
                    }
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Skill '{skill_name}' not found ({status}): {text}")),
                };
            }
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Request failed: {e}")),
                };
            }
        };

        let files = match body["files"].as_object() {
            Some(f) if !f.is_empty() => f,
            _ => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Skill '{skill_name}' has no files")),
                };
            }
        };

        // Write all files to workspace/skills/{name}/
        let skill_dir = self.skills_dir.join(skill_name);
        let mut written = 0u32;
        for (rel_path, content) in files {
            let content_str = match content.as_str() {
                Some(s) => s,
                None => continue,
            };
            let file_path = skill_dir.join(rel_path);
            if let Some(parent) = file_path.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to create directory {}: {e}", parent.display())),
                    };
                }
            }
            if let Err(e) = tokio::fs::write(&file_path, content_str).await {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to write {}: {e}", rel_path)),
                };
            }
            written += 1;
        }

        // Schedule delayed restart (15s) so agent can finish its response first
        self.schedule_restart();

        ToolResult {
            success: true,
            output: format!(
                "Skill '{skill_name}' installed successfully ({written} files). Agent will restart shortly to load the new skill."
            ),
            error: None,
        }
    }

    /// Load full skill instructions for on-demand use (no restart needed)
    async fn do_use(&self, skill_name: &str) -> ToolResult {
        let skill_dir = self.skills_dir.join(skill_name);
        if !skill_dir.exists() {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Skill '{skill_name}' is not installed. Use skill_manage(action=\"list\") to see installed skills, or skill_manage(action=\"install\", skill_name=\"{skill_name}\") to install it."
                )),
            };
        }

        let skill_md = skill_dir.join("SKILL.md");
        if !skill_md.exists() {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Skill '{skill_name}' has no SKILL.md file."
                )),
            };
        }

        // Read SKILL.md and replace {baseDir} with absolute path
        let content = match tokio::fs::read_to_string(&skill_md).await {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read SKILL.md: {e}")),
                };
            }
        };

        let abs_dir = match skill_dir.canonicalize() {
            Ok(p) => p.display().to_string(),
            Err(_) => skill_dir.display().to_string(),
        };
        let content = content.replace("{baseDir}", &abs_dir);

        // Recursively list all files in skill directory
        let files = list_skill_files(&skill_dir, &skill_dir);

        let mut output = format!(
            "<skill_instructions name=\"{skill_name}\" dir=\"{abs_dir}\">\n\n{content}\n\n</skill_instructions>\n\nFiles in this skill:\n"
        );
        for f in &files {
            output.push_str(&format!("  {f}\n"));
        }
        output.push_str("\nUse file_read to read any reference file if needed.\nFollow the instructions above to complete the user's request.");

        ToolResult {
            success: true,
            output,
            error: None,
        }
    }

    /// Delete local skill directory
    async fn do_remove(&self, skill_name: &str) -> ToolResult {
        let skill_dir = self.skills_dir.join(skill_name);
        if !skill_dir.exists() {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Skill '{skill_name}' is not installed.")),
            };
        }

        if let Err(e) = tokio::fs::remove_dir_all(&skill_dir).await {
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to remove skill: {e}")),
            };
        }

        self.schedule_restart();

        ToolResult {
            success: true,
            output: format!(
                "Skill '{skill_name}' removed successfully. Agent will restart shortly."
            ),
            error: None,
        }
    }
}

/// Recursively list all files in a directory, returning relative paths.
fn list_skill_files(dir: &std::path::Path, base: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            files.extend(list_skill_files(&path, base));
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                files.push(rel.display().to_string());
            }
        }
    }
    files
}

/// Parse description from YAML frontmatter in SKILL.md
fn parse_description(content: &str) -> Option<String> {
    let content = content.trim();
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("description:") {
            return Some(val.trim().trim_matches('"').to_string());
        }
    }
    None
}

#[async_trait]
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Manage agent skills. Actions: use (load skill instructions to execute now), discover (browse platform skill library), list (show installed skills), install (add a skill), update (reinstall a skill), remove (uninstall a skill). Use 'use' to load a skill's full instructions before executing it."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["use", "discover", "list", "install", "update", "remove"],
                    "description": "use: load full skill instructions for immediate execution; discover: browse platform library; list: show installed skills; install/update: install or update a skill; remove: uninstall a skill"
                },
                "skill_name": {
                    "type": "string",
                    "description": "Skill name (required for use/install/update/remove)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("").to_string();
        let skill_name = args["skill_name"].as_str().unwrap_or("").to_string();

        match action.as_str() {
            "use" => {
                if skill_name.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("skill_name is required for use".to_string()),
                    });
                }
                Ok(self.do_use(&skill_name).await)
            }
            "discover" => Ok(self.do_discover().await),
            "list" => Ok(self.do_list().await),
            "install" | "update" => {
                if skill_name.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("skill_name is required for install/update".to_string()),
                    });
                }
                Ok(self.do_install(&skill_name).await)
            }
            "remove" => {
                if skill_name.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("skill_name is required for remove".to_string()),
                    });
                }
                Ok(self.do_remove(&skill_name).await)
            }
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{action}'. Use: use, discover, list, install, update, remove"
                )),
            }),
        }
    }
}
