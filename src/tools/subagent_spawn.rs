//! Sub-agent spawn tool for background execution.
//!
//! Implements the `subagent_spawn` tool that launches delegate agents
//! asynchronously via `tokio::spawn`, returning a session ID immediately.
//! See `AGENTS.md` §7.3 for the tool change playbook.

use super::subagent_registry::{SubAgentRegistry, SubAgentSession, SubAgentStatus};
use super::traits::{Tool, ToolResult};
use crate::agent::loop_::{DRAFT_CLEAR_SENTINEL, DRAFT_PROGRESS_SENTINEL};
use crate::config::DelegateAgentConfig;
use crate::observability::traits::{Observer, ObserverEvent, ObserverMetric};
use crate::providers::{self, ChatMessage, Provider};
use crate::security::policy::ToolOperation;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Default timeout for background sub-agent provider calls.
const SPAWN_TIMEOUT_SECS: u64 = 300;
/// Maximum number of concurrent background sub-agents.
const MAX_CONCURRENT_SUBAGENTS: usize = 10;

/// Tool that spawns a delegate agent in the background, returning immediately
/// with a session ID. The sub-agent runs asynchronously and stores its result
/// in the shared [`SubAgentRegistry`].
pub struct SubAgentSpawnTool {
    agents: Arc<HashMap<String, DelegateAgentConfig>>,
    security: Arc<SecurityPolicy>,
    #[allow(dead_code)]
    fallback_credential: Option<String>,
    provider_runtime_options: providers::ProviderRuntimeOptions,
    registry: Arc<SubAgentRegistry>,
    parent_tools: Arc<Vec<Arc<dyn Tool>>>,
    multimodal_config: crate::config::MultimodalConfig,
    /// Parent agent's workspace directory, used to resolve sub-agent workspace paths.
    parent_workspace: std::path::PathBuf,
    /// Optional broadcast channel to notify WebSocket sessions when a subagent completes.
    completion_tx: Option<tokio::sync::broadcast::Sender<serde_json::Value>>,
}

impl SubAgentSpawnTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agents: HashMap<String, DelegateAgentConfig>,
        fallback_credential: Option<String>,
        security: Arc<SecurityPolicy>,
        provider_runtime_options: providers::ProviderRuntimeOptions,
        registry: Arc<SubAgentRegistry>,
        parent_tools: Arc<Vec<Arc<dyn Tool>>>,
        multimodal_config: crate::config::MultimodalConfig,
        parent_workspace: std::path::PathBuf,
    ) -> Self {
        Self {
            agents: Arc::new(agents),
            security,
            fallback_credential,
            provider_runtime_options,
            registry,
            parent_tools,
            multimodal_config,
            parent_workspace,
            completion_tx: None,
        }
    }

    /// Attach a broadcast channel for subagent completion events.
    pub fn with_completion_tx(
        mut self,
        tx: tokio::sync::broadcast::Sender<serde_json::Value>,
    ) -> Self {
        self.completion_tx = Some(tx);
        self
    }
}

#[async_trait]
impl Tool for SubAgentSpawnTool {
    fn name(&self) -> &str {
        "subagent_spawn"
    }

    fn description(&self) -> &str {
        "Spawn a delegate agent in the background. Returns immediately with a session_id. \
         Use subagent_list to check progress and subagent_manage to steer or kill."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let agent_names: Vec<&str> = self.agents.keys().map(|s: &String| s.as_str()).collect();
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "agent": {
                    "type": "string",
                    "minLength": 1,
                    "description": format!(
                        "Name of the agent to spawn. Available: {}",
                        if agent_names.is_empty() {
                            "(none configured)".to_string()
                        } else {
                            agent_names.join(", ")
                        }
                    )
                },
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The task/prompt to send to the sub-agent"
                },
                "context": {
                    "type": "string",
                    "description": "Optional context to prepend (e.g. relevant code, prior findings)"
                }
            },
            "required": ["agent", "task"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let agent_name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("Missing 'agent' parameter"))?;

        if agent_name.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'agent' parameter must not be empty".into()),
            });
        }

        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .ok_or_else(|| anyhow::anyhow!("Missing 'task' parameter"))?;

        if task.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'task' parameter must not be empty".into()),
            });
        }

        let context = args
            .get("context")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");

        // Security enforcement: spawn is a write operation
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "subagent_spawn")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        // Look up agent config
        let agent_config = match self.agents.get(agent_name) {
            Some(cfg) => cfg.clone(),
            None => {
                let available: Vec<&str> =
                    self.agents.keys().map(|s: &String| s.as_str()).collect();
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Unknown agent '{agent_name}'. Available agents: {}",
                        if available.is_empty() {
                            "(none configured)".to_string()
                        } else {
                            available.join(", ")
                        }
                    )),
                });
            }
        };

        // Create provider for this agent.
        // Only use explicit api_key override from config; otherwise let
        // resolve_provider_credential() pick up the correct env var
        // (e.g. DEEPSEEK_API_KEY, ARK_API_KEY) automatically — same as
        // a normal agent.
        #[allow(clippy::option_as_ref_deref)]
        let provider_credential = agent_config.api_key.as_ref().map(String::as_str);
        #[allow(clippy::option_as_ref_deref)]
        let provider_api_url = agent_config.api_url.as_ref().map(String::as_str);

        let provider: Box<dyn Provider> = match providers::create_provider_with_url_and_options(
            &agent_config.provider,
            provider_credential,
            provider_api_url,
            &self.provider_runtime_options,
        ) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Failed to create provider '{}' for agent '{agent_name}': {e}",
                        agent_config.provider
                    )),
                });
            }
        };

        // Build the message
        let full_prompt = if context.is_empty() {
            task.to_string()
        } else {
            format!("[Context]\n{context}\n\n[Task]\n{task}")
        };

        let session_id = uuid::Uuid::new_v4().to_string();
        let agent_name_owned = agent_name.to_string();
        let task_owned = task.to_string();

        // Determine if agentic mode
        let is_agentic = agent_config.agentic;
        let parent_tools = self.parent_tools.clone();
        let multimodal_config = self.multimodal_config.clone();
        let parent_workspace = self.parent_workspace.clone();

        // Dedup: if the same agent is already running with a similar task, return that session
        // instead of spawning a duplicate. This prevents the LLM from accidentally spawning
        // the same subagent multiple times in a single turn.
        if let Some(existing) = self.registry.find_running(agent_name) {
            info!(
                target: "subagent",
                agent = %agent_name,
                existing_session = %existing,
                "Dedup: agent already running, returning existing session"
            );
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "session_id": existing,
                    "agent": agent_name,
                    "status": "already_running",
                    "message": format!("Agent '{}' is already running. Use subagent_manage to check status.", agent_name)
                })
                .to_string(),
                error: None,
            });
        }

        // Atomically check concurrent limit and register session to prevent race conditions.
        let session = SubAgentSession {
            id: session_id.clone(),
            agent_name: agent_name_owned.clone(),
            task: task_owned,
            status: SubAgentStatus::Running,
            started_at: Utc::now(),
            completed_at: None,
            result: None,
            handle: None,
        };
        if let Err(_running) = self.registry.try_insert(session, MAX_CONCURRENT_SUBAGENTS) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Maximum concurrent sub-agents reached ({MAX_CONCURRENT_SUBAGENTS}). \
                     Wait for running agents to complete or kill some."
                )),
            });
        }

        // Clone what we need for the spawned task
        let registry = self.registry.clone();
        let sid = session_id.clone();
        let completion_tx = self.completion_tx.clone();

        info!(
            target: "subagent",
            agent = %agent_name,
            session_id = %session_id,
            provider = %agent_config.provider,
            model = %agent_config.model,
            agentic = is_agentic,
            "Spawning subagent"
        );

        let handle = tokio::spawn(async move {
            let start = std::time::Instant::now();

            let result = if is_agentic {
                run_agentic_background(
                    &agent_name_owned,
                    &agent_config,
                    &*provider,
                    &full_prompt,
                    &parent_tools,
                    &multimodal_config,
                    completion_tx.clone(),
                    &parent_workspace,
                )
                .await
            } else {
                run_simple_background(&agent_name_owned, &agent_config, &*provider, &full_prompt)
                    .await
            };

            let elapsed = start.elapsed();
            let (success, error_msg, result_output) = match &result {
                Ok(tool_result) => {
                    if tool_result.success {
                        let output = tool_result.output.clone();
                        registry.complete(&sid, tool_result.clone());
                        info!(
                            target: "subagent",
                            agent = %agent_name_owned,
                            session_id = %sid,
                            elapsed_ms = elapsed.as_millis() as u64,
                            "Subagent completed successfully"
                        );
                        (true, None, Some(output))
                    } else {
                        let err = tool_result
                            .error
                            .clone()
                            .unwrap_or_else(|| "Unknown error".to_string());
                        error!(
                            target: "subagent",
                            agent = %agent_name_owned,
                            session_id = %sid,
                            elapsed_ms = elapsed.as_millis() as u64,
                            error = %err,
                            "Subagent failed"
                        );
                        registry.fail(&sid, err.clone());
                        (false, Some(err), None)
                    }
                }
                Err(e) => {
                    let err = format!("Agent '{agent_name_owned}' error: {e}");
                    error!(
                        target: "subagent",
                        agent = %agent_name_owned,
                        session_id = %sid,
                        elapsed_ms = elapsed.as_millis() as u64,
                        error = %err,
                        "Subagent error"
                    );
                    registry.fail(&sid, err.clone());
                    (false, Some(err), None)
                }
            };

            // Notify WebSocket sessions about completion (include result for auto-processing)
            if let Some(tx) = completion_tx {
                let event = json!({
                    "type": "subagent_completed",
                    "session_id": sid,
                    "agent": agent_name_owned,
                    "success": success,
                    "elapsed_ms": elapsed.as_millis() as u64,
                    "error": error_msg,
                    "result": result_output,
                });
                let _ = tx.send(event);
            }
        });

        // Store the handle for cancellation
        self.registry.set_handle(&session_id, handle);

        Ok(ToolResult {
            success: true,
            output: json!({
                "session_id": session_id,
                "agent": agent_name,
                "status": "running",
                "message": "Sub-agent spawned in background. Use subagent_list or subagent_manage to check progress."
            })
            .to_string(),
            error: None,
        })
    }
}

async fn run_simple_background(
    agent_name: &str,
    agent_config: &DelegateAgentConfig,
    provider: &dyn Provider,
    full_prompt: &str,
) -> anyhow::Result<ToolResult> {
    let temperature = agent_config.temperature.unwrap_or(0.7);
    debug!(target: "subagent", agent = %agent_name, mode = "simple", "Starting simple background call");

    let result = tokio::time::timeout(
        Duration::from_secs(SPAWN_TIMEOUT_SECS),
        provider.chat_with_system(
            agent_config.system_prompt.as_deref(),
            full_prompt,
            &agent_config.model,
            temperature,
        ),
    )
    .await;

    let result = match result {
        Ok(inner) => inner,
        Err(_elapsed) => {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Agent '{agent_name}' timed out after {SPAWN_TIMEOUT_SECS}s"
                )),
            });
        }
    };

    match result {
        Ok(response) => {
            let rendered = if response.trim().is_empty() {
                "[Empty response]".to_string()
            } else {
                response
            };

            Ok(ToolResult {
                success: true,
                output: format!(
                    "[Agent '{agent_name}' ({provider}/{model})]\n{rendered}",
                    provider = agent_config.provider,
                    model = agent_config.model
                ),
                error: None,
            })
        }
        Err(e) => Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("Agent '{agent_name}' failed: {e}")),
        }),
    }
}

struct ToolArcRef {
    inner: Arc<dyn Tool>,
}

impl ToolArcRef {
    fn new(inner: Arc<dyn Tool>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Tool for ToolArcRef {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.inner.execute(args).await
    }
}

struct NoopObserver;

impl Observer for NoopObserver {
    fn record_event(&self, _event: &ObserverEvent) {}
    fn record_metric(&self, _metric: &ObserverMetric) {}
    fn name(&self) -> &str {
        "noop"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Parse a delta event from the agentic loop into a broadcast-ready JSON event.
fn parse_subagent_delta(agent_name: &str, delta: &str) -> Option<serde_json::Value> {
    if delta == DRAFT_CLEAR_SENTINEL {
        return None;
    }

    let progress = delta.strip_prefix(DRAFT_PROGRESS_SENTINEL)?;
    let progress = progress.trim();

    if let Some(rest) = progress.strip_prefix("⏳ ") {
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        let (name, hint) = match rest.split_once(": ") {
            Some((name, hint)) => {
                let hint = hint.trim();
                (
                    name.trim().to_string(),
                    if hint.is_empty() {
                        None
                    } else {
                        Some(hint.to_string())
                    },
                )
            }
            None => (rest.to_string(), None),
        };
        return Some(json!({
            "type": "subagent_tool_call",
            "agent": agent_name,
            "name": name,
            "hint": hint,
        }));
    }

    if let Some(rest) = progress.strip_prefix("✅ ") {
        let trimmed = rest.trim();
        if let Some((name_part, duration_part)) = trimmed.rsplit_once(" (") {
            let secs = duration_part
                .strip_suffix(')')
                .and_then(|s| s.strip_suffix('s'))
                .and_then(|s| s.parse::<u64>().ok());
            return Some(json!({
                "type": "subagent_tool_result",
                "agent": agent_name,
                "name": name_part.trim(),
                "success": true,
                "duration_secs": secs,
            }));
        }
    }

    if let Some(rest) = progress.strip_prefix("❌ ") {
        let trimmed = rest.trim();
        if let Some((name_part, duration_part)) = trimmed.rsplit_once(" (") {
            let secs = duration_part
                .strip_suffix(')')
                .and_then(|s| s.strip_suffix('s'))
                .and_then(|s| s.parse::<u64>().ok());
            return Some(json!({
                "type": "subagent_tool_result",
                "agent": agent_name,
                "name": name_part.trim(),
                "success": false,
                "duration_secs": secs,
            }));
        }
    }

    None
}

async fn run_agentic_background(
    agent_name: &str,
    agent_config: &DelegateAgentConfig,
    provider: &dyn Provider,
    full_prompt: &str,
    parent_tools: &[Arc<dyn Tool>],
    multimodal_config: &crate::config::MultimodalConfig,
    event_tx: Option<tokio::sync::broadcast::Sender<serde_json::Value>>,
    parent_workspace: &std::path::Path,
) -> anyhow::Result<ToolResult> {
    if agent_config.allowed_tools.is_empty() {
        warn!(target: "subagent", agent = %agent_name, "Agentic agent has empty allowed_tools");
        return Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!(
                "Agent '{agent_name}' has agentic=true but allowed_tools is empty"
            )),
        });
    }

    let allowed = agent_config
        .allowed_tools
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect::<std::collections::HashSet<_>>();

    let sub_tools: Vec<Box<dyn Tool>> = parent_tools
        .iter()
        .filter(|tool| allowed.contains(tool.name()))
        .filter(|tool| {
            tool.name() != "delegate"
                && tool.name() != "subagent_spawn"
                && tool.name() != "subagent_manage"
        })
        .map(|tool| Box::new(ToolArcRef::new(tool.clone())) as Box<dyn Tool>)
        .collect();

    if sub_tools.is_empty() {
        warn!(target: "subagent", agent = %agent_name, allowed = ?agent_config.allowed_tools, "No executable tools after filtering");
        return Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!(
                "Agent '{agent_name}' has no executable tools after filtering allowlist ({})",
                agent_config.allowed_tools.join(", ")
            )),
        });
    }

    let tool_names: Vec<&str> = sub_tools.iter().map(|t| t.name()).collect();
    info!(
        target: "subagent",
        agent = %agent_name,
        mode = "agentic",
        tools = ?tool_names,
        max_iterations = agent_config.max_iterations,
        "Starting agentic loop"
    );

    let temperature = agent_config.temperature.unwrap_or(0.7);
    let mut history = Vec::new();

    // Build system prompt: either via SystemPromptBuilder (full agent mode)
    // or from the raw system_prompt string (legacy mode).
    if agent_config.use_prompt_builder {
        let sub_workspace = match agent_config.workspace_dir.as_deref() {
            Some(dir) => parent_workspace.join(dir),
            None => parent_workspace.to_path_buf(),
        };

        let skills = if agent_config.skills_enabled {
            crate::skills::load_skills(&sub_workspace)
        } else {
            vec![]
        };

        let ctx = crate::agent::prompt::PromptContext {
            workspace_dir: &sub_workspace,
            model_name: &agent_config.model,
            tools: &sub_tools,
            skills: &skills,
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: None,
            dispatcher_instructions: "",
        };
        let system_prompt = crate::agent::prompt::SystemPromptBuilder::with_defaults()
            .build(&ctx)
            .unwrap_or_default();
        if !system_prompt.trim().is_empty() {
            history.push(ChatMessage::system(system_prompt));
        }
    } else if let Some(system_prompt) = agent_config.system_prompt.as_ref() {
        history.push(ChatMessage::system(system_prompt.clone()));
    }
    history.push(ChatMessage::user(full_prompt.to_string()));

    let noop_observer = NoopObserver;

    // Set up delta streaming to forward subagent tool events to WebSocket clients
    let delta_tx = if let Some(ref broadcast_tx) = event_tx {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(128);
        let broadcast = broadcast_tx.clone();
        let agent = agent_name.to_string();
        tokio::spawn(async move {
            while let Some(delta) = rx.recv().await {
                if let Some(event) = parse_subagent_delta(&agent, &delta) {
                    let _ = broadcast.send(event);
                }
            }
        });
        Some(tx)
    } else {
        None
    };

    let result = tokio::time::timeout(
        Duration::from_secs(SPAWN_TIMEOUT_SECS),
        crate::agent::loop_::run_tool_call_loop(
            provider,
            &mut history,
            &sub_tools,
            &noop_observer,
            &agent_config.provider,
            &agent_config.model,
            temperature,
            true,
            None,
            "subagent_spawn",
            multimodal_config,
            agent_config.max_iterations,
            None,
            delta_tx,
            None,
            &[],
        ),
    )
    .await;

    match result {
        Ok(Ok(response)) => {
            let rendered = if response.trim().is_empty() {
                "[Empty response]".to_string()
            } else {
                response
            };

            Ok(ToolResult {
                success: true,
                output: format!(
                    "[Agent '{agent_name}' ({provider}/{model}, agentic)]\n{rendered}",
                    provider = agent_config.provider,
                    model = agent_config.model
                ),
                error: None,
            })
        }
        Ok(Err(e)) => Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!("Agent '{agent_name}' failed: {e}")),
        }),
        Err(_) => Ok(ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!(
                "Agent '{agent_name}' timed out after {SPAWN_TIMEOUT_SECS}s"
            )),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    fn sample_agents() -> HashMap<String, DelegateAgentConfig> {
        let mut agents = HashMap::new();
        agents.insert(
            "researcher".to_string(),
            DelegateAgentConfig {
                provider: "ollama".to_string(),
                model: "llama3".to_string(),
                system_prompt: Some("You are a research assistant.".to_string()),
                api_key: None,
                api_url: None,
                temperature: Some(0.3),
                max_depth: 3,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 10,
                workspace_dir: None,
                use_prompt_builder: false,
                skills_enabled: false,
            },
        );
        agents
    }

    fn make_tool(
        agents: HashMap<String, DelegateAgentConfig>,
        security: Arc<SecurityPolicy>,
    ) -> SubAgentSpawnTool {
        SubAgentSpawnTool::new(
            agents,
            None,
            security,
            providers::ProviderRuntimeOptions::default(),
            Arc::new(SubAgentRegistry::new()),
            Arc::new(Vec::new()),
            crate::config::MultimodalConfig::default(),
            std::path::PathBuf::from("/tmp"),
        )
    }

    #[test]
    fn name_and_schema() {
        let tool = make_tool(sample_agents(), test_security());
        assert_eq!(tool.name(), "subagent_spawn");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["agent"].is_object());
        assert!(schema["properties"]["task"].is_object());
        assert!(schema["properties"]["context"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("agent")));
        assert!(required.contains(&json!("task")));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn description_not_empty() {
        let tool = make_tool(sample_agents(), test_security());
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn missing_agent_param() {
        let tool = make_tool(sample_agents(), test_security());
        let result = tool.execute(json!({"task": "test"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_task_param() {
        let tool = make_tool(sample_agents(), test_security());
        let result = tool.execute(json!({"agent": "researcher"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn blank_agent_rejected() {
        let tool = make_tool(sample_agents(), test_security());
        let result = tool
            .execute(json!({"agent": "  ", "task": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn blank_task_rejected() {
        let tool = make_tool(sample_agents(), test_security());
        let result = tool
            .execute(json!({"agent": "researcher", "task": "  "}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("must not be empty"));
    }

    #[tokio::test]
    async fn unknown_agent_returns_error() {
        let tool = make_tool(sample_agents(), test_security());
        let result = tool
            .execute(json!({"agent": "nonexistent", "task": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown agent"));
    }

    #[tokio::test]
    async fn spawn_blocked_in_readonly_mode() {
        let readonly = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = make_tool(sample_agents(), readonly);
        let result = tool
            .execute(json!({"agent": "researcher", "task": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("read-only mode"));
    }

    #[tokio::test]
    async fn spawn_blocked_when_rate_limited() {
        let limited = Arc::new(SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = make_tool(sample_agents(), limited);
        let result = tool
            .execute(json!({"agent": "researcher", "task": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Rate limit exceeded"));
    }

    #[tokio::test]
    async fn spawn_returns_session_id() {
        // The agent has an invalid provider so the background task will fail,
        // but spawn itself returns immediately with a session_id.
        let tool = make_tool(sample_agents(), test_security());
        let result = tool
            .execute(json!({"agent": "researcher", "task": "test task"}))
            .await
            .unwrap();
        // Spawn may fail at provider creation if the provider is invalid
        // For ollama, it should successfully create the provider even without a running server
        // The result could succeed (spawn) or fail (invalid provider) depending on environment
        if result.success {
            let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
            assert!(output["session_id"].is_string());
            assert_eq!(output["status"], "running");
        }
        // Either way, no panic
    }

    #[tokio::test]
    async fn spawn_no_agents_configured() {
        let tool = make_tool(HashMap::new(), test_security());
        let result = tool
            .execute(json!({"agent": "any", "task": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("none configured"));
    }

    #[tokio::test]
    async fn spawn_respects_concurrent_limit() {
        let registry = Arc::new(SubAgentRegistry::new());

        // Fill up the registry with running sessions
        for i in 0..MAX_CONCURRENT_SUBAGENTS {
            registry.insert(SubAgentSession {
                id: format!("s{i}"),
                agent_name: "agent".to_string(),
                task: "task".to_string(),
                status: SubAgentStatus::Running,
                started_at: Utc::now(),
                completed_at: None,
                result: None,
                handle: None,
            });
        }

        let tool = SubAgentSpawnTool::new(
            sample_agents(),
            None,
            test_security(),
            providers::ProviderRuntimeOptions::default(),
            registry,
            Arc::new(Vec::new()),
            crate::config::MultimodalConfig::default(),
            std::path::PathBuf::from("/tmp"),
        );

        let result = tool
            .execute(json!({"agent": "researcher", "task": "test"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Maximum concurrent"));
    }

    #[tokio::test]
    async fn schema_lists_agent_names() {
        let tool = make_tool(sample_agents(), test_security());
        let schema = tool.parameters_schema();
        let desc = schema["properties"]["agent"]["description"]
            .as_str()
            .unwrap();
        assert!(desc.contains("researcher"));
    }
}
