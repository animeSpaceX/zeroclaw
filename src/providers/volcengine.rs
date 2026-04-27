//! Dedicated provider for Volcengine / Doubao (火山引擎 / 豆包).
//!
//! Unlike the generic `compatible.rs` adapter, this module:
//! - Always sends `stream_options.include_usage` so streaming responses carry
//!   token usage in the final chunk.
//! - Keeps `ThinkingConfig::disabled()` on every request to avoid slow reasoning
//!   tokens with doubao models.
//! - Handles SSE parsing and tool-call delta accumulation in a single function
//!   (`stream_sse_to_events`) for easier debugging.

use crate::multimodal;
use crate::providers::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, ProviderCapabilities, StreamChunk, StreamError, StreamEvent, StreamOptions,
    StreamResult, TokenUsage, ToolCall as ProviderToolCall, ToolsPayload,
};
use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use reqwest::header::{HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, warn};

const BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/v3";

// ─── Provider struct ──────────────────────────────────────────────────

pub struct VolcengineProvider {
    base_url: String,
    credential: Option<String>,
    reasoning_enabled: bool,
}

impl VolcengineProvider {
    pub fn new(credential: Option<String>, reasoning_enabled: bool) -> Self {
        Self {
            base_url: BASE_URL.to_string(),
            credential,
            reasoning_enabled,
        }
    }

    fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn build_client() -> reqwest::Client {
        crate::config::build_runtime_proxy_client_with_timeouts("volcengine", 300, 10)
    }
}

// ─── Request types ────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<RequestMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptionsRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
}

#[derive(Serialize)]
struct StreamOptionsRequest {
    include_usage: bool,
}

#[derive(Serialize)]
struct ThinkingConfig {
    #[serde(rename = "type")]
    kind: String,
}

impl ThinkingConfig {
    fn disabled() -> Self {
        Self {
            kind: "disabled".to_string(),
        }
    }
}

#[derive(Serialize)]
struct RequestMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<RequestToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Serialize, Clone)]
#[serde(tag = "type")]
enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlRef },
}

#[derive(Serialize, Clone)]
struct ImageUrlRef {
    url: String,
}

#[derive(Serialize)]
struct RequestToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: RequestFunction,
}

#[derive(Serialize)]
struct RequestFunction {
    name: String,
    arguments: String,
}

// ─── Response types ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Option<Vec<ResponseChoice>>,
    usage: Option<UsageInfo>,
    #[serde(default)]
    suggestions: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ResponseChoice {
    message: Option<ResponseMessage>,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Deserialize)]
struct ResponseToolCall {
    id: Option<String>,
    function: Option<ResponseFunction>,
}

#[derive(Deserialize)]
struct ResponseFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize, Clone)]
struct UsageInfo {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize, Clone)]
struct PromptTokensDetails {
    cached_tokens: Option<u64>,
}

// ─── Stream response types ────────────────────────────────────────────

#[derive(Deserialize)]
struct StreamChunkResponse {
    choices: Option<Vec<StreamChoice>>,
    usage: Option<UsageInfo>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Option<StreamDelta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Deserialize)]
struct StreamToolCallDelta {
    index: Option<usize>,
    id: Option<String>,
    function: Option<StreamFunctionDelta>,
}

#[derive(Deserialize)]
struct StreamFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

// ─── Tool call accumulator ────────────────────────────────────────────

struct ToolCallAccumulator {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    fn new() -> Self {
        Self {
            id: None,
            name: None,
            arguments: String::new(),
        }
    }

    fn apply_delta(&mut self, delta: &StreamToolCallDelta) {
        if let Some(id) = &delta.id {
            if !id.is_empty() {
                self.id = Some(id.clone());
            }
        }
        if let Some(f) = &delta.function {
            if let Some(name) = &f.name {
                if !name.is_empty() && self.name.is_none() {
                    self.name = Some(name.clone());
                }
            }
            if let Some(args) = &f.arguments {
                self.arguments.push_str(args);
            }
        }
    }

    fn into_tool_call(self) -> Option<ProviderToolCall> {
        let name = self.name?;
        let id = self
            .id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let arguments = if self.arguments.is_empty() {
            "{}".to_string()
        } else if serde_json::from_str::<Value>(&self.arguments).is_err() {
            warn!(
                name,
                raw = self.arguments,
                "invalid JSON in tool call arguments, defaulting to {{}}"
            );
            "{}".to_string()
        } else {
            self.arguments
        };
        Some(ProviderToolCall {
            id,
            name,
            arguments,
        })
    }
}

// ─── Conversion helpers ───────────────────────────────────────────────

fn convert_messages(messages: &[ChatMessage], supports_vision: bool) -> Vec<RequestMessage> {
    messages
        .iter()
        .map(|msg| {
            if msg.role == "assistant" {
                convert_assistant_message(msg)
            } else if msg.role == "tool" {
                convert_tool_message(msg)
            } else {
                convert_regular_message(msg, supports_vision)
            }
        })
        .collect()
}

fn convert_assistant_message(msg: &ChatMessage) -> RequestMessage {
    // Assistant messages may contain serialized tool_calls in their content.
    if let Ok(parsed) = serde_json::from_str::<Value>(&msg.content) {
        if let Some(tc_arr) = parsed.get("tool_calls").and_then(|v| v.as_array()) {
            let tool_calls: Vec<RequestToolCall> = tc_arr
                .iter()
                .filter_map(|tc| {
                    let func = tc.get("function")?;
                    Some(RequestToolCall {
                        id: tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        kind: "function".to_string(),
                        function: RequestFunction {
                            name: func
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            arguments: func
                                .get("arguments")
                                .and_then(|v| v.as_str())
                                .unwrap_or("{}")
                                .to_string(),
                        },
                    })
                })
                .collect();

            let content_text = parsed
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reasoning = parsed
                .get("reasoning_content")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            return RequestMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text(content_text)),
                tool_call_id: None,
                tool_calls: if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                },
                reasoning_content: reasoning,
            };
        }
    }

    // Plain assistant text
    RequestMessage {
        role: "assistant".to_string(),
        content: Some(MessageContent::Text(msg.content.clone())),
        tool_call_id: None,
        tool_calls: None,
        reasoning_content: None,
    }
}

fn convert_tool_message(msg: &ChatMessage) -> RequestMessage {
    // Tool result messages may have JSON-encoded content with tool_call_id.
    if let Ok(parsed) = serde_json::from_str::<Value>(&msg.content) {
        if let Some(tcid) = parsed.get("tool_call_id").and_then(|v| v.as_str()) {
            let content = parsed
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or(&msg.content)
                .to_string();
            return RequestMessage {
                role: "tool".to_string(),
                content: Some(MessageContent::Text(content)),
                tool_call_id: Some(tcid.to_string()),
                tool_calls: None,
                reasoning_content: None,
            };
        }
    }

    RequestMessage {
        role: "tool".to_string(),
        content: Some(MessageContent::Text(msg.content.clone())),
        tool_call_id: None,
        tool_calls: None,
        reasoning_content: None,
    }
}

fn convert_regular_message(msg: &ChatMessage, supports_vision: bool) -> RequestMessage {
    let content = if msg.role == "user" && supports_vision && msg.content.contains("[IMAGE:") {
        let (text, urls) = multimodal::parse_image_markers(&msg.content);
        if urls.is_empty() {
            MessageContent::Text(msg.content.clone())
        } else {
            let mut parts = Vec::with_capacity(1 + urls.len());
            if !text.is_empty() {
                parts.push(ContentPart::Text { text });
            }
            for url in urls {
                parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrlRef { url },
                });
            }
            MessageContent::Parts(parts)
        }
    } else {
        MessageContent::Text(msg.content.clone())
    };

    RequestMessage {
        role: msg.role.clone(),
        content: Some(content),
        tool_call_id: None,
        tool_calls: None,
        reasoning_content: None,
    }
}

fn convert_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

fn parse_usage(info: &UsageInfo) -> TokenUsage {
    TokenUsage {
        input_tokens: info.prompt_tokens,
        output_tokens: info.completion_tokens,
        cached_tokens: info
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens),
    }
}

fn parse_response(resp: ChatCompletionResponse) -> ProviderChatResponse {
    let choice = resp
        .choices
        .as_ref()
        .and_then(|c| c.first())
        .and_then(|c| c.message.as_ref());

    let text = choice.and_then(|m| {
        m.content
            .as_ref()
            .filter(|s| !s.is_empty())
            .cloned()
    });
    let reasoning_content = choice.and_then(|m| m.reasoning_content.clone());

    let tool_calls: Vec<ProviderToolCall> = choice
        .and_then(|m| m.tool_calls.as_ref())
        .map(|tcs| {
            tcs.iter()
                .filter_map(|tc| {
                    let func = tc.function.as_ref()?;
                    let name = func.name.as_ref()?.clone();
                    let id = tc
                        .id
                        .clone()
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let arguments = func
                        .arguments
                        .as_ref()
                        .filter(|a| serde_json::from_str::<Value>(a).is_ok())
                        .cloned()
                        .unwrap_or_else(|| "{}".to_string());
                    Some(ProviderToolCall {
                        id,
                        name,
                        arguments,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let usage = resp.usage.as_ref().map(parse_usage);

    ProviderChatResponse {
        text,
        tool_calls,
        usage,
        reasoning_content,
        suggestions: resp.suggestions,
    }
}

// ─── SSE streaming ────────────────────────────────────────────────────

fn stream_sse_to_events(
    response: reqwest::Response,
    count_tokens: bool,
) -> stream::BoxStream<'static, StreamResult<StreamEvent>> {
    let (tx, rx) = mpsc::channel::<StreamResult<StreamEvent>>(100);

    tokio::spawn(async move {
        let mut tool_accumulators: Vec<ToolCallAccumulator> = Vec::new();
        let mut last_usage: Option<TokenUsage> = None;
        let mut emitted_tool_calls = false;
        let mut buffer = String::new();

        let mut byte_stream = response.bytes_stream();

        loop {
            let chunk_result =
                tokio::time::timeout(std::time::Duration::from_secs(60), byte_stream.next()).await;

            let chunk = match chunk_result {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(Some(Err(e))) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    break;
                }
                Ok(None) => break, // stream ended
                Err(_) => {
                    let _ = tx
                        .send(Err(StreamError::Provider("stream timeout (60s)".into())))
                        .await;
                    break;
                }
            };

            let text = match std::str::from_utf8(&chunk) {
                Ok(t) => t,
                Err(e) => {
                    warn!("non-UTF8 SSE chunk: {e}");
                    continue;
                }
            };

            buffer.push_str(text);

            // Process complete lines
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer = buffer[line_end + 1..].to_string();

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                let data = if let Some(stripped) = line.strip_prefix("data:") {
                    stripped.trim()
                } else {
                    continue;
                };

                if data == "[DONE]" {
                    continue;
                }

                let parsed: StreamChunkResponse = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("SSE parse skip: {e}");
                        continue;
                    }
                };

                // Capture usage from any chunk that carries it
                if let Some(u) = &parsed.usage {
                    last_usage = Some(parse_usage(u));
                }

                let Some(choices) = &parsed.choices else {
                    continue;
                };

                for choice in choices {
                    let Some(delta) = &choice.delta else {
                        continue;
                    };

                    // Text delta
                    if let Some(text_content) = &delta.content {
                        if !text_content.is_empty() {
                            let mut chunk = StreamChunk::delta(text_content.clone());
                            if count_tokens {
                                chunk = chunk.with_token_estimate();
                            }
                            let _ = tx.send(Ok(StreamEvent::TextDelta(chunk))).await;
                        }
                    }

                    // Reasoning/thinking content delta
                    if let Some(reasoning) = &delta.reasoning_content {
                        if !reasoning.is_empty() {
                            let _ = tx.send(Ok(StreamEvent::ThinkingDelta(reasoning.clone()))).await;
                        }
                    }

                    // Tool call deltas
                    if let Some(tc_deltas) = &delta.tool_calls {
                        for tc_delta in tc_deltas {
                            let idx = tc_delta.index.unwrap_or(tool_accumulators.len());
                            while tool_accumulators.len() <= idx {
                                tool_accumulators.push(ToolCallAccumulator::new());
                            }
                            tool_accumulators[idx].apply_delta(tc_delta);
                        }
                    }

                    // Emit tool calls on finish_reason
                    if choice.finish_reason.as_deref() == Some("tool_calls") && !emitted_tool_calls
                    {
                        emitted_tool_calls = true;
                        for acc in tool_accumulators.drain(..) {
                            if let Some(tc) = acc.into_tool_call() {
                                let _ = tx.send(Ok(StreamEvent::ToolCall(tc))).await;
                            }
                        }
                    }
                }
            }
        }

        // Emit any remaining un-emitted tool calls
        if !emitted_tool_calls {
            for acc in tool_accumulators.drain(..) {
                if let Some(tc) = acc.into_tool_call() {
                    let _ = tx.send(Ok(StreamEvent::ToolCall(tc))).await;
                }
            }
        }

        // Final event with usage (the key fix over compatible.rs)
        let _ = tx.send(Ok(StreamEvent::Final(last_usage))).await;
    });

    stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    })
    .boxed()
}

fn stream_sse_to_chunks(
    response: reqwest::Response,
    count_tokens: bool,
) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
    let (tx, rx) = mpsc::channel::<StreamResult<StreamChunk>>(100);

    tokio::spawn(async move {
        let mut last_usage: Option<TokenUsage> = None;
        let mut buffer = String::new();
        let mut byte_stream = response.bytes_stream();

        loop {
            let chunk_result =
                tokio::time::timeout(std::time::Duration::from_secs(60), byte_stream.next()).await;

            let chunk = match chunk_result {
                Ok(Some(Ok(bytes))) => bytes,
                Ok(Some(Err(e))) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    let _ = tx
                        .send(Err(StreamError::Provider("stream timeout (60s)".into())))
                        .await;
                    break;
                }
            };

            let text = match std::str::from_utf8(&chunk) {
                Ok(t) => t,
                Err(_) => continue,
            };

            buffer.push_str(text);

            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim().to_string();
                buffer = buffer[line_end + 1..].to_string();

                if line.is_empty() || line.starts_with(':') {
                    continue;
                }

                let data = if let Some(stripped) = line.strip_prefix("data:") {
                    stripped.trim()
                } else {
                    continue;
                };

                if data == "[DONE]" {
                    continue;
                }

                let parsed: StreamChunkResponse = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if let Some(u) = &parsed.usage {
                    last_usage = Some(parse_usage(u));
                }

                if let Some(choices) = &parsed.choices {
                    for choice in choices {
                        if let Some(delta) = &choice.delta {
                            if let Some(content) = &delta.content {
                                if !content.is_empty() {
                                    let mut chunk = StreamChunk::delta(content.clone());
                                    if count_tokens {
                                        chunk = chunk.with_token_estimate();
                                    }
                                    let _ = tx.send(Ok(chunk)).await;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Final chunk with usage
        let mut final_chunk = StreamChunk::final_chunk();
        final_chunk.usage = last_usage;
        let _ = tx.send(Ok(final_chunk)).await;
    });

    stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (chunk, rx))
    })
    .boxed()
}

// ─── Provider trait implementation ────────────────────────────────────

#[async_trait]
impl Provider for VolcengineProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
        }
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        ToolsPayload::OpenAI {
            tools: convert_tools(tools),
        }
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_streaming_tool_events(&self) -> bool {
        true
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(RequestMessage {
                role: "system".to_string(),
                content: Some(MessageContent::Text(sys.to_string())),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
            });
        }
        messages.push(RequestMessage {
            role: "user".to_string(),
            content: Some(MessageContent::Text(message.to_string())),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        });

        let body = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            temperature,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            thinking: if self.reasoning_enabled { None } else { Some(ThinkingConfig::disabled()) },
        };

        let key = self
            .credential
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Volcengine API key not configured"))?;

        let resp = Self::build_client()
            .post(self.chat_completions_url())
            .header(AUTHORIZATION, format!("Bearer {key}"))
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(super::api_error("volcengine", resp).await);
        }

        let data: ChatCompletionResponse = resp.json().await?;
        let text = data
            .choices
            .as_ref()
            .and_then(|c| c.first())
            .and_then(|c| c.message.as_ref())
            .and_then(|m| m.content.clone())
            .unwrap_or_default();

        Ok(text)
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let api_messages = convert_messages(request.messages, true);
        let tools = request.tools.map(convert_tools);

        let body = ChatCompletionRequest {
            model: model.to_string(),
            messages: api_messages,
            temperature,
            stream: Some(false),
            stream_options: None,
            tools,
            tool_choice: Some(json!("auto")),
            thinking: if self.reasoning_enabled { None } else { Some(ThinkingConfig::disabled()) },
        };

        let key = self
            .credential
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Volcengine API key not configured"))?;

        let resp = Self::build_client()
            .post(self.chat_completions_url())
            .header(AUTHORIZATION, format!("Bearer {key}"))
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(super::api_error("volcengine", resp).await);
        }

        let data: ChatCompletionResponse = resp.json().await?;
        Ok(parse_response(data))
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let api_messages = convert_messages(messages, true);

        let body = ChatCompletionRequest {
            model: model.to_string(),
            messages: api_messages,
            temperature,
            stream: Some(false),
            stream_options: None,
            tools: Some(tools.to_vec()),
            tool_choice: Some(json!("auto")),
            thinking: if self.reasoning_enabled { None } else { Some(ThinkingConfig::disabled()) },
        };

        let key = self
            .credential
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Volcengine API key not configured"))?;

        let resp = Self::build_client()
            .post(self.chat_completions_url())
            .header(AUTHORIZATION, format!("Bearer {key}"))
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(super::api_error("volcengine", resp).await);
        }

        let data: ChatCompletionResponse = resp.json().await?;
        Ok(parse_response(data))
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        if !options.enabled {
            return stream::empty().boxed();
        }

        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(RequestMessage {
                role: "system".to_string(),
                content: Some(MessageContent::Text(sys.to_string())),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
            });
        }
        messages.push(RequestMessage {
            role: "user".to_string(),
            content: Some(MessageContent::Text(message.to_string())),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        });

        let body = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            temperature,
            stream: Some(true),
            stream_options: Some(StreamOptionsRequest {
                include_usage: true,
            }),
            tools: None,
            tool_choice: None,
            thinking: if self.reasoning_enabled { None } else { Some(ThinkingConfig::disabled()) },
        };

        let url = self.chat_completions_url();
        let key = self.credential.clone();
        let count_tokens = options.count_tokens;

        let (tx, rx) = mpsc::channel::<StreamResult<StreamChunk>>(1);

        tokio::spawn(async move {
            let Some(key) = key else {
                let _ = tx
                    .send(Err(StreamError::Provider(
                        "Volcengine API key not configured".into(),
                    )))
                    .await;
                return;
            };

            let resp = match Self::build_client()
                .post(&url)
                .header(AUTHORIZATION, format!("Bearer {key}"))
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
                .json(&body)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    let status = r.status();
                    let body_text = r.text().await.unwrap_or_default();
                    let _ = tx
                        .send(Err(StreamError::Provider(format!(
                            "volcengine {status}: {body_text}"
                        ))))
                        .await;
                    return;
                }
                Err(e) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    return;
                }
            };

            let mut inner_stream = stream_sse_to_chunks(resp, count_tokens);
            while let Some(item) = inner_stream.next().await {
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        });

        stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        })
        .boxed()
    }

    fn stream_chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamEvent>> {
        if !options.enabled {
            return stream::empty().boxed();
        }

        let api_messages = convert_messages(request.messages, true);
        let tools = request.tools.map(convert_tools);
        let has_tools = tools.is_some();

        let body = ChatCompletionRequest {
            model: model.to_string(),
            messages: api_messages,
            temperature,
            stream: Some(true),
            stream_options: Some(StreamOptionsRequest {
                include_usage: true,
            }),
            tools,
            tool_choice: if has_tools {
                Some(json!("auto"))
            } else {
                None
            },
            thinking: if self.reasoning_enabled { None } else { Some(ThinkingConfig::disabled()) },
        };

        let url = self.chat_completions_url();
        let key = self.credential.clone();
        let count_tokens = options.count_tokens;

        let (tx, rx) = mpsc::channel::<StreamResult<StreamEvent>>(1);

        tokio::spawn(async move {
            let Some(key) = key else {
                let _ = tx
                    .send(Err(StreamError::Provider(
                        "Volcengine API key not configured".into(),
                    )))
                    .await;
                return;
            };

            let resp = match Self::build_client()
                .post(&url)
                .header(AUTHORIZATION, format!("Bearer {key}"))
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
                .json(&body)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    let status = r.status();
                    let body_text = r.text().await.unwrap_or_default();
                    let _ = tx
                        .send(Err(StreamError::Provider(format!(
                            "volcengine {status}: {body_text}"
                        ))))
                        .await;
                    return;
                }
                Err(e) => {
                    let _ = tx.send(Err(StreamError::Http(e))).await;
                    return;
                }
            };

            let mut inner_stream = stream_sse_to_events(resp, count_tokens);
            while let Some(item) = inner_stream.next().await {
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        });

        stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed()
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        let _ = Self::build_client();
        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thinking_config_serializes_disabled() {
        let cfg = ThinkingConfig::disabled();
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json, json!({"type": "disabled"}));
    }

    #[test]
    fn test_stream_options_serializes() {
        let opts = StreamOptionsRequest {
            include_usage: true,
        };
        let json = serde_json::to_value(&opts).unwrap();
        assert_eq!(json, json!({"include_usage": true}));
    }

    #[test]
    fn test_convert_tools_format() {
        let tools = vec![ToolSpec {
            name: "get_weather".into(),
            description: "Get weather info".into(),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        }];
        let result = convert_tools(&tools);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["type"], "function");
        assert_eq!(result[0]["function"]["name"], "get_weather");
    }

    #[test]
    fn test_parse_usage() {
        let info = UsageInfo {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(20),
            }),
        };
        let usage = parse_usage(&info);
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.cached_tokens, Some(20));
    }

    #[test]
    fn test_parse_usage_no_details() {
        let info = UsageInfo {
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            prompt_tokens_details: None,
        };
        let usage = parse_usage(&info);
        assert_eq!(usage.cached_tokens, None);
    }

    #[test]
    fn test_tool_call_accumulator_basic() {
        let mut acc = ToolCallAccumulator::new();
        acc.apply_delta(&StreamToolCallDelta {
            index: Some(0),
            id: Some("call_123".into()),
            function: Some(StreamFunctionDelta {
                name: Some("test_fn".into()),
                arguments: Some("{\"a\":".into()),
            }),
        });
        acc.apply_delta(&StreamToolCallDelta {
            index: Some(0),
            id: None,
            function: Some(StreamFunctionDelta {
                name: None,
                arguments: Some("1}".into()),
            }),
        });

        let tc = acc.into_tool_call().unwrap();
        assert_eq!(tc.name, "test_fn");
        assert_eq!(tc.id, "call_123");
        assert_eq!(tc.arguments, "{\"a\":1}");
    }

    #[test]
    fn test_tool_call_accumulator_no_name_returns_none() {
        let acc = ToolCallAccumulator::new();
        assert!(acc.into_tool_call().is_none());
    }

    #[test]
    fn test_tool_call_accumulator_invalid_json_defaults() {
        let mut acc = ToolCallAccumulator::new();
        acc.name = Some("fn".into());
        acc.arguments = "not json".into();
        let tc = acc.into_tool_call().unwrap();
        assert_eq!(tc.arguments, "{}");
    }

    #[test]
    fn test_convert_plain_user_message() {
        let msgs = vec![ChatMessage::user("hello")];
        let result = convert_messages(&msgs, false);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].role, "user");
    }

    #[test]
    fn test_convert_assistant_with_tool_calls() {
        let content = json!({
            "content": "Let me check",
            "tool_calls": [{
                "id": "call_1",
                "function": {
                    "name": "search",
                    "arguments": "{\"q\":\"test\"}"
                }
            }]
        })
        .to_string();

        let msgs = vec![ChatMessage {
            role: "assistant".into(),
            content,
        }];
        let result = convert_messages(&msgs, false);
        assert_eq!(result[0].role, "assistant");
        assert!(result[0].tool_calls.is_some());
        let tcs = result[0].tool_calls.as_ref().unwrap();
        assert_eq!(tcs[0].function.name, "search");
    }

    #[test]
    fn test_convert_tool_result_message() {
        let content = json!({
            "tool_call_id": "call_1",
            "content": "result data"
        })
        .to_string();

        let msgs = vec![ChatMessage {
            role: "tool".into(),
            content,
        }];
        let result = convert_messages(&msgs, false);
        assert_eq!(result[0].role, "tool");
        assert_eq!(result[0].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let resp = ChatCompletionResponse {
            choices: Some(vec![ResponseChoice {
                message: Some(ResponseMessage {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![ResponseToolCall {
                        id: Some("c1".into()),
                        function: Some(ResponseFunction {
                            name: Some("test".into()),
                            arguments: Some("{\"x\":1}".into()),
                        }),
                    }]),
                }),
            }]),
            usage: Some(UsageInfo {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                prompt_tokens_details: None,
            }),
            suggestions: None,
        };

        let result = parse_response(resp);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "test");
        assert!(result.usage.is_some());
        assert_eq!(result.usage.unwrap().input_tokens, Some(10));
    }

    #[test]
    fn test_parse_response_text_only() {
        let resp = ChatCompletionResponse {
            choices: Some(vec![ResponseChoice {
                message: Some(ResponseMessage {
                    content: Some("Hello world".into()),
                    reasoning_content: None,
                    tool_calls: None,
                }),
            }]),
            usage: None,
            suggestions: Some(vec!["follow up?".into()]),
        };

        let result = parse_response(resp);
        assert_eq!(result.text.as_deref(), Some("Hello world"));
        assert!(result.tool_calls.is_empty());
        assert!(result.suggestions.is_some());
    }

    #[test]
    fn test_provider_capabilities() {
        let p = VolcengineProvider::new(None);
        let caps = p.capabilities();
        assert!(caps.native_tool_calling);
        assert!(caps.vision);
        assert!(p.supports_streaming());
        assert!(p.supports_streaming_tool_events());
    }

    #[test]
    fn test_chat_completions_url() {
        let p = VolcengineProvider::new(None);
        assert_eq!(
            p.chat_completions_url(),
            "https://ark.cn-beijing.volces.com/api/v3/chat/completions"
        );
    }

    #[test]
    fn test_request_serialization_with_all_fields() {
        let req = ChatCompletionRequest {
            model: "doubao-seed-2-0-mini-260215".into(),
            messages: vec![RequestMessage {
                role: "user".into(),
                content: Some(MessageContent::Text("hi".into())),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
            }],
            temperature: 0.7,
            stream: Some(true),
            stream_options: Some(StreamOptionsRequest {
                include_usage: true,
            }),
            tools: None,
            tool_choice: None,
            thinking: if self.reasoning_enabled { None } else { Some(ThinkingConfig::disabled()) },
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["stream"], true);
        assert_eq!(json["stream_options"]["include_usage"], true);
        assert_eq!(json["thinking"]["type"], "disabled");
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn test_multipart_content_serialization() {
        let content = MessageContent::Parts(vec![
            ContentPart::Text {
                text: "Look at this:".into(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrlRef {
                    url: "https://example.com/img.png".into(),
                },
            },
        ]);
        let json = serde_json::to_value(&content).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
    }
}
