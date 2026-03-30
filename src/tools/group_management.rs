use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

/// Tool for managing IM group conversations — full lifecycle:
/// create/delete groups, manage members (users + agents), update settings.
pub struct GroupManagementTool {
    gateway_url: String,
    team_id: String,
    api_key: String,
}

impl GroupManagementTool {
    pub fn new(gateway_url: String, team_id: String) -> Self {
        let api_key = std::env::var("PLATFORM_API_KEY").unwrap_or_default();
        Self {
            gateway_url,
            team_id,
            api_key,
        }
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    /// Resolve the owner user_id from TEAM_ID by querying the gateway.
    async fn resolve_owner_user_id(&self) -> Option<String> {
        let url = format!(
            "{}/api/teams/{}",
            self.gateway_url.trim_end_matches('/'),
            self.team_id
        );
        let resp = self
            .client()
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: serde_json::Value = resp.json().await.ok()?;
        body["created_by"]
            .as_str()
            .or_else(|| body["owner"].as_str())
            .or_else(|| body["owner_id"].as_str())
            .map(|s| s.to_string())
    }

    async fn api_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> ToolResult {
        let url = format!("{}{}", self.gateway_url.trim_end_matches('/'), path);
        let mut req = self
            .client()
            .request(method, &url)
            .header("X-API-Key", &self.api_key)
            .timeout(std::time::Duration::from_secs(30));
        if let Some(b) = body {
            req = req.json(&b);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                let text = r.text().await.unwrap_or_default();
                ToolResult {
                    success: true,
                    output: if text.is_empty() {
                        "OK".to_string()
                    } else {
                        text
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
}

#[async_trait]
impl Tool for GroupManagementTool {
    fn name(&self) -> &str {
        "group_management"
    }

    fn description(&self) -> &str {
        "Manage IM group conversations — create/delete groups, invite/remove agents and users, update settings, list members, list available roles, dispatch tasks to agents."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "list_groups", "create_group", "delete_group", "update_group",
                        "list_members", "invite_agent", "remove_agent",
                        "invite_user", "list_roles", "dispatch", "update_member_profile"
                    ],
                    "description": "The operation to perform"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "Group conversation ID (from the [群聊: ... | conversation_id: xxx] context header). Required for all actions except create_group."
                },
                "name": {
                    "type": "string",
                    "description": "Group name (for create_group or update_group)"
                },
                "purpose": {
                    "type": "string",
                    "description": "Group purpose/announcement (for create_group or update_group)"
                },
                "agent_roles": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Initial agent roles to invite (for create_group, e.g. ['coder', 'designer'])"
                },
                "role": {
                    "type": "string",
                    "description": "Agent role to invite or remove (for invite_agent / remove_agent)"
                },
                "user_id": {
                    "type": "string",
                    "description": "User ID to invite (for invite_user)"
                },
                "user_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "User IDs to invite at creation (for create_group)"
                },
                "message": {
                    "type": "string",
                    "description": "Message to dispatch to an agent (for dispatch action)"
                },
                "member_type": {
                    "type": "string",
                    "enum": ["user", "agent"],
                    "description": "Member type for update_member_profile"
                },
                "member_id": {
                    "type": "string",
                    "description": "User UUID or Agent UUID for update_member_profile"
                },
                "nickname": {
                    "type": "string",
                    "description": "New nickname for update_member_profile"
                },
                "notes": {
                    "type": "string",
                    "description": "Notes/remarks about the member for update_member_profile"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args["action"].as_str().unwrap_or("");
        let conv_id = args["conversation_id"].as_str().unwrap_or("");

        match action {
            "list_groups" => {
                // List all conversations for the owner user (includes my_role: owner/member)
                let owner_id = match self.resolve_owner_user_id().await {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Failed to resolve owner user_id".to_string()),
                        });
                    }
                };
                Ok(self
                    .api_request(
                        reqwest::Method::GET,
                        &format!("/api/conversations?minimal=true&user_id={}", urlencoding::encode(&owner_id)),
                        None,
                    )
                    .await)
            }

            "create_group" => {
                let name = args["name"].as_str().unwrap_or("");
                if name.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("name is required for create_group".to_string()),
                    });
                }

                // Resolve the owner user_id from the agent's team
                let owner_id = match self.resolve_owner_user_id().await {
                    Some(id) => id,
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Failed to resolve owner user_id from team".to_string()),
                        });
                    }
                };

                let mut body = json!({
                    "name": name,
                    "owner_user_id": owner_id,
                });
                if let Some(purpose) = args["purpose"].as_str() {
                    body["purpose"] = json!(purpose);
                }
                if let Some(roles) = args["agent_roles"].as_array() {
                    body["agent_roles"] = json!(roles);
                }
                // Collect user IDs to invite (including the owner automatically)
                let mut invite_ids = vec![owner_id];
                if let Some(ids) = args["user_ids"].as_array() {
                    for id in ids {
                        if let Some(s) = id.as_str() {
                            if !invite_ids.contains(&s.to_string()) {
                                invite_ids.push(s.to_string());
                            }
                        }
                    }
                }
                body["invite_user_ids"] = json!(invite_ids);

                Ok(self
                    .api_request(
                        reqwest::Method::POST,
                        "/api/groups/create",
                        Some(body),
                    )
                    .await)
            }

            "delete_group" => {
                if conv_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("conversation_id is required".to_string()),
                    });
                }
                Ok(self
                    .api_request(
                        reqwest::Method::DELETE,
                        &format!("/api/conversations/{conv_id}"),
                        None,
                    )
                    .await)
            }

            "update_group" => {
                if conv_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("conversation_id is required".to_string()),
                    });
                }
                let mut body = json!({});
                if let Some(v) = args["name"].as_str() {
                    body["name"] = json!(v);
                }
                if let Some(v) = args["purpose"].as_str() {
                    body["settings"] = json!({ "purpose": v });
                }
                Ok(self
                    .api_request(
                        reqwest::Method::PATCH,
                        &format!("/api/conversations/{conv_id}"),
                        Some(body),
                    )
                    .await)
            }

            "list_members" => {
                if conv_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("conversation_id is required".to_string()),
                    });
                }
                // GET /api/conversations/{id} returns conversation + all members
                Ok(self
                    .api_request(
                        reqwest::Method::GET,
                        &format!("/api/conversations/{conv_id}"),
                        None,
                    )
                    .await)
            }

            "invite_agent" => {
                if conv_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("conversation_id is required".to_string()),
                    });
                }
                let role = args["role"].as_str().unwrap_or("");
                if role.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("role is required for invite_agent".to_string()),
                    });
                }
                Ok(self
                    .api_request(
                        reqwest::Method::POST,
                        &format!("/api/groups/{conv_id}/invite-agent"),
                        Some(json!({ "role": role })),
                    )
                    .await)
            }

            "remove_agent" => {
                if conv_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("conversation_id is required".to_string()),
                    });
                }
                let role = args["role"].as_str().unwrap_or("");
                if role.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("role is required for remove_agent".to_string()),
                    });
                }
                Ok(self
                    .api_request(
                        reqwest::Method::POST,
                        &format!("/api/groups/{conv_id}/remove-agent"),
                        Some(json!({ "role": role })),
                    )
                    .await)
            }

            "invite_user" => {
                if conv_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("conversation_id is required".to_string()),
                    });
                }
                let user_id = args["user_id"].as_str().unwrap_or("");
                if user_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("user_id is required for invite_user".to_string()),
                    });
                }
                Ok(self
                    .api_request(
                        reqwest::Method::POST,
                        &format!("/api/conversations/{conv_id}/members"),
                        Some(json!({
                            "member_type": "user",
                            "user_id": user_id,
                        })),
                    )
                    .await)
            }

            "list_roles" => {
                // List all available agent roles from the platform registry
                Ok(self
                    .api_request(
                        reqwest::Method::GET,
                        "/api/registry",
                        None,
                    )
                    .await)
            }

            "dispatch" => {
                let role = args["role"].as_str().unwrap_or("");
                let message = args["message"].as_str().unwrap_or("");
                if role.is_empty() || message.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("role and message are required for dispatch".to_string()),
                    });
                }
                // Dispatch to agent via the team API (synchronous, waits up to 120s)
                let url = format!(
                    "{}/api/teams/{}/agents/{}/dispatch",
                    self.gateway_url.trim_end_matches('/'),
                    self.team_id,
                    role
                );
                let mut req = self
                    .client()
                    .post(&url)
                    .header("X-API-Key", &self.api_key)
                    .json(&json!({ "message": message }))
                    .timeout(std::time::Duration::from_secs(150));
                match req.send().await {
                    Ok(r) if r.status().is_success() => {
                        let text = r.text().await.unwrap_or_default();
                        Ok(ToolResult { success: true, output: text, error: None })
                    }
                    Ok(r) => {
                        let status = r.status();
                        let text = r.text().await.unwrap_or_default();
                        Ok(ToolResult { success: false, output: String::new(), error: Some(format!("API error ({status}): {text}")) })
                    }
                    Err(e) => Ok(ToolResult { success: false, output: String::new(), error: Some(format!("Request failed: {e}")) }),
                }
            }

            "update_member_profile" => {
                if conv_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("conversation_id is required".to_string()),
                    });
                }
                let member_type = args["member_type"].as_str().unwrap_or("");
                let member_id = args["member_id"].as_str().unwrap_or("");
                if member_type.is_empty() || member_id.is_empty() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("member_type and member_id are required".to_string()),
                    });
                }
                let mut body = json!({});
                if let Some(v) = args["nickname"].as_str() {
                    body["nickname"] = json!(v);
                }
                if let Some(v) = args["notes"].as_str() {
                    body["notes"] = json!(v);
                }
                Ok(self
                    .api_request(
                        reqwest::Method::PATCH,
                        &format!(
                            "/api/conversations/{}/members/{}/{}/profile",
                            conv_id, member_type, member_id
                        ),
                        Some(body),
                    )
                    .await)
            }

            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{action}'. Use: create_group, delete_group, update_group, list_members, invite_agent, remove_agent, invite_user, list_roles, dispatch, update_member_profile"
                )),
            }),
        }
    }
}
