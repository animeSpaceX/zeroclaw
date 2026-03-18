use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

pub struct SkillManageTool {
    gateway_url: String,
    team_id: String,
    agent_role: String,
}

impl SkillManageTool {
    pub fn new(gateway_url: String, team_id: String, agent_role: String) -> Self {
        Self {
            gateway_url,
            team_id,
            agent_role,
        }
    }

    async fn do_discover(&self) -> ToolResult {
        let url = format!("{}/api/skills", self.gateway_url);
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
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

    async fn do_list(&self) -> ToolResult {
        let url = format!(
            "{}/api/teams/{}/agents/{}/skills",
            self.gateway_url, self.team_id, self.agent_role
        );
        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
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
                        output: "No skills currently installed.".to_string(),
                        error: None,
                    },
                    Some(arr) => {
                        let mut lines = vec!["Installed skills:".to_string()];
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

    async fn do_install(&self, skill_name: &str) -> ToolResult {
        let url = format!(
            "{}/api/teams/{}/agents/{}/skills",
            self.gateway_url, self.team_id, self.agent_role
        );
        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .json(&json!({ "name": skill_name }))
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => ToolResult {
                success: true,
                output: format!(
                    "Skill '{skill_name}' installed successfully.\n\
                     Note: Agent restart is required for the skill to take effect."
                ),
                error: None,
            },
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Install failed ({status}): {text}")),
                }
            }
            Err(e) => ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            },
        }
    }

    async fn do_remove(&self, skill_name: &str) -> ToolResult {
        let url = format!(
            "{}/api/teams/{}/agents/{}/skills/{}",
            self.gateway_url,
            self.team_id,
            self.agent_role,
            urlencoding::encode(skill_name)
        );
        let client = reqwest::Client::new();
        let resp = client
            .delete(&url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => ToolResult {
                success: true,
                output: format!(
                    "Skill '{skill_name}' removed successfully.\n\
                     Note: Agent restart is required for the change to take effect."
                ),
                error: None,
            },
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Remove failed ({status}): {text}")),
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
impl Tool for SkillManageTool {
    fn name(&self) -> &str {
        "skill_manage"
    }

    fn description(&self) -> &str {
        "Manage agent skills at runtime. Actions: discover (list available skills from platform library), list (show installed skills), install (add a skill), update (reinstall a skill), remove (uninstall a skill). Skills take effect after agent restart."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["discover", "list", "install", "update", "remove"],
                    "description": "discover: view available skills; list: view installed skills; install/update: install or update a skill; remove: uninstall a skill"
                },
                "skill_name": {
                    "type": "string",
                    "description": "Skill name (required for install/update/remove)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("").to_string();
        let skill_name = args["skill_name"].as_str().unwrap_or("").to_string();

        match action.as_str() {
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
                    "Unknown action '{action}'. Use: discover, list, install, update, remove"
                )),
            }),
        }
    }
}
