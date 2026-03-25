use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct TaskManagementTool {
    gateway_url: String,
    team_id: String,
    api_key: String,
}

impl TaskManagementTool {
    pub fn new(gateway_url: String, team_id: String) -> Self {
        let api_key = std::env::var("PLATFORM_API_KEY").unwrap_or_default();
        Self {
            gateway_url,
            team_id,
            api_key,
        }
    }

    /// Build base URL. Use conversation_id if available, otherwise use team_id (gateway will resolve).
    fn base_url(&self, conversation_id: Option<&str>) -> String {
        let gw = self.gateway_url.trim_end_matches('/');
        match conversation_id {
            Some(cid) if !cid.is_empty() => format!("{}/api/conversations/{}", gw, cid),
            _ => format!("{}/api/teams/{}", gw, self.team_id),
        }
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    fn auth_header(&self) -> (&str, String) {
        ("X-API-Key", self.api_key.clone())
    }

    async fn api_get(&self, base: &str, path: &str) -> ToolResult {
        let url = format!("{}{}", base, path);
        let (header, value) = self.auth_header();
        match self
            .client()
            .get(&url)
            .header(header, value)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let body = r.text().await.unwrap_or_default();
                ToolResult {
                    success: true,
                    output: body,
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

    async fn api_post(&self, base: &str, path: &str, body: serde_json::Value) -> ToolResult {
        let url = format!("{}{}", base, path);
        let (header, value) = self.auth_header();
        match self
            .client()
            .post(&url)
            .header(header, value)
            .json(&body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let body = r.text().await.unwrap_or_default();
                ToolResult {
                    success: true,
                    output: body,
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

    async fn api_patch(&self, base: &str, path: &str, body: serde_json::Value) -> ToolResult {
        let url = format!("{}{}", base, path);
        let (header, value) = self.auth_header();
        match self
            .client()
            .patch(&url)
            .header(header, value)
            .json(&body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let body = r.text().await.unwrap_or_default();
                ToolResult {
                    success: true,
                    output: body,
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

    async fn api_delete(&self, base: &str, path: &str) -> ToolResult {
        let url = format!("{}{}", base, path);
        let (header, value) = self.auth_header();
        match self
            .client()
            .delete(&url)
            .header(header, value)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let body = r.text().await.unwrap_or_default();
                ToolResult {
                    success: true,
                    output: if body.is_empty() {
                        "Deleted successfully".to_string()
                    } else {
                        body
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

    async fn api_get_json(&self, url: &str) -> Result<Value, String> {
        let (header, value) = self.auth_header();
        match self
            .client()
            .get(url)
            .header(header, value)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let body = r.text().await.unwrap_or_default();
                serde_json::from_str(&body)
                    .map_err(|e| format!("Failed to parse API response from {url}: {e}"))
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                Err(format!("API error ({status}): {text}"))
            }
            Err(e) => Err(format!("Request failed: {e}")),
        }
    }

    fn normalize_role(input: &str) -> String {
        match input.trim() {
            "团队助手" | "助手" | "助理" => "assistant".to_string(),
            "编程专家" | "程序员" | "开发者" | "开发" | "码农" | "工程师" | "Coder" => {
                "coder".to_string()
            }
            "设计专家" | "设计师" | "设计" | "Designer" => "designer".to_string(),
            "项目经理" | "产品经理" | "产品" | "PM" | "Pm" => "pm".to_string(),
            "测试专家" | "测试" | "测试员" | "QA" | "Tester" => "tester".to_string(),
            "研究专家" | "研究员" | "研究" | "Researcher" => "researcher".to_string(),
            "运维专家" | "运维" | "DevOps" | "Devops" => "devops".to_string(),
            "翻译专家" | "翻译" | "Translator" => "translator".to_string(),
            "数据分析" | "数据分析师" | "分析师" => "data-analyst".to_string(),
            "技术文档" | "文档" | "技术写作" => "doc-writer".to_string(),
            "安全审计" | "安全" | "审计" => "security-reviewer".to_string(),
            "架构师" | "架构" | "Architect" => "architect".to_string(),
            other => other.trim().to_lowercase(),
        }
    }

    async fn list_projects_meta(&self, base: &str) -> Result<Vec<(i64, String)>, String> {
        let url = format!("{base}/projects");
        let value = self.api_get_json(&url).await?;
        let projects = value
            .get("projects")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "Invalid project list response".to_string())?;
        let mut result = Vec::with_capacity(projects.len());
        for project in projects {
            let Some(id) = project.get("id").and_then(|v| v.as_i64()) else {
                continue;
            };
            let title = project
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            result.push((id, title));
        }
        Ok(result)
    }

    fn format_projects(projects: &[(i64, String)]) -> String {
        projects
            .iter()
            .map(|(id, title)| {
                if title.is_empty() {
                    format!("#{id}")
                } else {
                    format!("#{id} {title}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    async fn resolve_project_id_for_task(
        &self,
        base: &str,
        requested_project_id: Option<i64>,
    ) -> Result<Option<i64>, String> {
        let projects = self.list_projects_meta(base).await?;
        match requested_project_id {
            Some(project_id) => {
                if projects.iter().any(|(id, _)| *id == project_id) {
                    Ok(Some(project_id))
                } else if projects.is_empty() {
                    Err(format!(
                        "project_id {project_id} does not exist in this conversation"
                    ))
                } else {
                    Err(format!(
                        "project_id {project_id} does not exist. Existing projects: {}",
                        Self::format_projects(&projects)
                    ))
                }
            }
            None => {
                if projects.is_empty() {
                    Ok(None)
                } else {
                    Err(format!(
                        "project_id is required for create_task because this conversation already has project(s): {}",
                        Self::format_projects(&projects)
                    ))
                }
            }
        }
    }

    async fn resolve_assigned_to_agent(
        &self,
        conversation_id: &str,
        assigned_to: &str,
    ) -> Result<String, String> {
        let role = Self::normalize_role(assigned_to);
        if role.is_empty() {
            return Err("assigned_to cannot be empty".to_string());
        }

        let url = format!(
            "{}/api/groups/{}/agents",
            self.gateway_url.trim_end_matches('/'),
            conversation_id
        );
        let value = self.api_get_json(&url).await?;
        let agents = value
            .get("agents")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "Invalid group agents response".to_string())?;

        let matches: Vec<String> = agents
            .iter()
            .filter_map(|agent| {
                let agent_role = agent.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if Self::normalize_role(agent_role) != role {
                    return None;
                }
                agent
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            })
            .collect();

        match matches.as_slice() {
            [agent_id] => Ok(agent_id.clone()),
            [] => Err(format!(
                "assigned_to '{assigned_to}' was not found in this group. Use team_management(action=\"list_agents\") to check available roles."
            )),
            _ => Err(format!(
                "assigned_to '{assigned_to}' matched multiple agents. Please pass assigned_to_id explicitly."
            )),
        }
    }
}

#[async_trait]
impl Tool for TaskManagementTool {
    fn name(&self) -> &str {
        "task_management"
    }

    fn description(&self) -> &str {
        "Manage projects, tasks, and comments for a group conversation. Requires conversation_id (from group context). Supports assigned_to role shorthand and enforces project_id when tasks are created under an existing project workflow. Actions: list_projects, create_project, update_project, delete_project, list_tasks, create_task, update_task, delete_task, clear_tasks, list_comments, add_comment."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "list_projects", "create_project", "update_project", "delete_project",
                        "list_tasks", "create_task", "update_task", "delete_task", "clear_tasks",
                        "list_comments", "add_comment"
                    ],
                    "description": "The operation to perform"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "The group conversation ID (UUID). Extract from the [群聊: ... | conversation_id: UUID] header in the message context. If omitted, uses CONVERSATION_ID env var."
                },
                "project_id": {
                    "type": "integer",
                    "description": "Project ID. Required for create_task once the conversation already has project(s)."
                },
                "task_id": {
                    "type": "integer",
                    "description": "Task ID (for update_task, delete_task, list_comments, add_comment)"
                },
                "title": {
                    "type": "string",
                    "description": "Title for project or task"
                },
                "description": {
                    "type": "string",
                    "description": "Description for project or task"
                },
                "assigned_to": {
                    "type": "string",
                    "description": "Agent role shorthand such as 'coder' or 'pm'. The tool resolves it to assigned_to_id automatically in group conversations."
                },
                "assigned_to_type": {
                    "type": "string",
                    "enum": ["user", "agent"],
                    "description": "Type of assignee for advanced usage"
                },
                "assigned_to_id": {
                    "type": "string",
                    "description": "UUID of the user or agent to assign the task to for advanced usage"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "done", "failed", "active", "completed", "archived"],
                    "description": "Task or project status"
                },
                "priority": {
                    "type": "integer",
                    "description": "Task priority (0=normal, higher=more urgent)"
                },
                "depends_on": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "List of task IDs this task depends on"
                },
                "content": {
                    "type": "string",
                    "description": "Comment content (for add_comment)"
                },
                "comment_type": {
                    "type": "string",
                    "enum": ["comment", "progress", "result", "system"],
                    "description": "Comment type (default: comment)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("");

        tracing::info!(action = action, args = %args, "task_management: execute called");

        // Resolve conversation_id: arg > env var > None (fall back to team_id route)
        let conv_id = args["conversation_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                std::env::var("CONVERSATION_ID")
                    .ok()
                    .filter(|s| !s.is_empty())
            });

        let base = self.base_url(conv_id.as_deref());
        tracing::info!(base_url = %base, "task_management: resolved base URL");

        match action {
            // ── Projects ──
            "list_projects" => Ok(self.api_get(&base, "/projects").await),

            "create_project" => {
                let title = args["title"].as_str().unwrap_or("");
                if title.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("title is required for create_project".to_string()),
                    });
                }
                let mut body = json!({ "title": title });
                if let Some(desc) = args["description"].as_str() {
                    body["description"] = json!(desc);
                }
                Ok(self.api_post(&base, "/projects", body).await)
            }

            "update_project" => {
                let pid = args["project_id"].as_i64().unwrap_or(0);
                if pid == 0 {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("project_id is required".to_string()),
                    });
                }
                let mut body = json!({});
                if let Some(v) = args["title"].as_str() {
                    body["title"] = json!(v);
                }
                if let Some(v) = args["description"].as_str() {
                    body["description"] = json!(v);
                }
                if let Some(v) = args["status"].as_str() {
                    body["status"] = json!(v);
                }
                Ok(self.api_patch(&base, &format!("/projects/{pid}"), body).await)
            }

            "delete_project" => {
                let pid = args["project_id"].as_i64().unwrap_or(0);
                if pid == 0 {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("project_id is required".to_string()),
                    });
                }
                Ok(self.api_delete(&base, &format!("/projects/{pid}")).await)
            }

            // ── Tasks ──
            "list_tasks" => {
                if let Some(pid) = args["project_id"].as_i64() {
                    Ok(self.api_get(&base, &format!("/tasks?project_id={pid}")).await)
                } else {
                    Ok(self.api_get(&base, "/tasks").await)
                }
            }

            "create_task" => {
                let title = args["title"].as_str().unwrap_or("");
                if title.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("title is required for create_task".to_string()),
                    });
                }
                let project_id = match self
                    .resolve_project_id_for_task(&base, args["project_id"].as_i64())
                    .await
                {
                    Ok(project_id) => project_id,
                    Err(error) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(error),
                        })
                    }
                };
                let mut body = json!({ "title": title });
                if let Some(v) = args["description"].as_str() {
                    body["description"] = json!(v);
                }
                if let Some(v) = args["assigned_to_type"].as_str() {
                    body["assigned_to_type"] = json!(v);
                }
                if let Some(v) = args["assigned_to_id"].as_str() {
                    body["assigned_to_id"] = json!(v);
                }
                if let Some(role) = args["assigned_to"].as_str() {
                    if !role.trim().is_empty() && args["assigned_to_id"].as_str().unwrap_or("").is_empty() {
                        let Some(conversation_id) = conv_id.as_deref() else {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some("conversation_id is required when using assigned_to role shorthand".to_string()),
                            });
                        };
                        match self.resolve_assigned_to_agent(conversation_id, role).await {
                            Ok(agent_id) => {
                                body["assigned_to_type"] = json!("agent");
                                body["assigned_to_id"] = json!(agent_id);
                            }
                            Err(error) => {
                                return Ok(ToolResult {
                                    success: false,
                                    output: String::new(),
                                    error: Some(error),
                                })
                            }
                        }
                    }
                }
                if let Some(v) = project_id {
                    body["project_id"] = json!(v);
                }
                if let Some(v) = args["priority"].as_i64() {
                    body["priority"] = json!(v);
                }
                if let Some(v) = args["depends_on"].as_array() {
                    body["depends_on"] = json!(v);
                }
                Ok(self.api_post(&base, "/tasks", body).await)
            }

            "update_task" => {
                let tid = args["task_id"].as_i64().unwrap_or(0);
                if tid == 0 {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("task_id is required".to_string()),
                    });
                }
                let mut body = json!({});
                if let Some(v) = args["title"].as_str() {
                    body["title"] = json!(v);
                }
                if let Some(v) = args["description"].as_str() {
                    body["description"] = json!(v);
                }
                if let Some(v) = args["status"].as_str() {
                    body["status"] = json!(v);
                }
                if let Some(v) = args["assigned_to_type"].as_str() {
                    body["assigned_to_type"] = json!(v);
                }
                if let Some(v) = args["assigned_to_id"].as_str() {
                    body["assigned_to_id"] = json!(v);
                }
                if let Some(role) = args["assigned_to"].as_str() {
                    if !role.trim().is_empty() && args["assigned_to_id"].as_str().unwrap_or("").is_empty() {
                        let Some(conversation_id) = conv_id.as_deref() else {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some("conversation_id is required when using assigned_to role shorthand".to_string()),
                            });
                        };
                        match self.resolve_assigned_to_agent(conversation_id, role).await {
                            Ok(agent_id) => {
                                body["assigned_to_type"] = json!("agent");
                                body["assigned_to_id"] = json!(agent_id);
                            }
                            Err(error) => {
                                return Ok(ToolResult {
                                    success: false,
                                    output: String::new(),
                                    error: Some(error),
                                })
                            }
                        }
                    }
                }
                if let Some(project_id) = args["project_id"].as_i64() {
                    match self.resolve_project_id_for_task(&base, Some(project_id)).await {
                        Ok(Some(pid)) => {
                            body["project_id"] = json!(pid);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(error),
                            })
                        }
                    }
                }
                if let Some(v) = args["priority"].as_i64() {
                    body["priority"] = json!(v);
                }
                if let Some(v) = args["depends_on"].as_array() {
                    body["depends_on"] = json!(v);
                }
                Ok(self.api_patch(&base, &format!("/tasks/{tid}"), body).await)
            }

            "delete_task" => {
                let tid = args["task_id"].as_i64().unwrap_or(0);
                if tid == 0 {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("task_id is required".to_string()),
                    });
                }
                Ok(self.api_delete(&base, &format!("/tasks/{tid}")).await)
            }

            "clear_tasks" => Ok(self.api_post(&base, "/tasks/clear", json!({})).await),

            // ── Comments ──
            "list_comments" => {
                let tid = args["task_id"].as_i64().unwrap_or(0);
                if tid == 0 {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("task_id is required".to_string()),
                    });
                }
                Ok(self.api_get(&base, &format!("/tasks/{tid}/comments")).await)
            }

            "add_comment" => {
                let tid = args["task_id"].as_i64().unwrap_or(0);
                let content = args["content"].as_str().unwrap_or("");
                if tid == 0 || content.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("task_id and content are required".to_string()),
                    });
                }
                let comment_type = args["comment_type"].as_str().unwrap_or("comment");
                let body = json!({
                    "content": content,
                    "comment_type": comment_type,
                });
                Ok(self.api_post(&base, &format!("/tasks/{tid}/comments"), body).await)
            }

            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{action}'. Use: list_projects, create_project, update_project, delete_project, list_tasks, create_task, update_task, delete_task, clear_tasks, list_comments, add_comment"
                )),
            }),
        }
    }
}
