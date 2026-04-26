use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Mutex;
use std::time::Instant;

/// Max calls allowed within the rate-limit window.
const RATE_LIMIT_MAX: usize = 3;
/// Rate-limit window duration in seconds.
const RATE_LIMIT_WINDOW_SECS: u64 = 60;

pub struct FalGenerateTool {
    gateway_url: String,
    agent_id: String,
    api_key: String,
    /// Timestamps of recent calls for rate limiting.
    call_times: Mutex<Vec<Instant>>,
}

impl FalGenerateTool {
    pub fn new(gateway_url: String, agent_id: String) -> Self {
        let api_key = std::env::var("PLATFORM_API_KEY").unwrap_or_default();
        Self {
            gateway_url,
            agent_id,
            api_key,
            call_times: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Tool for FalGenerateTool {
    fn name(&self) -> &str {
        "fal_generate"
    }

    fn description(&self) -> &str {
        "Generate images or videos using AI models. Results are automatically sent to the conversation.\n\
         Models:\n\
         - nanobanana: Text-to-image (supports text rendering in images)\n\
         - nanobanana_edit: Image-to-image editing (requires image_urls, up to 14 reference images)\n\
         - seedance_t2v: Text-to-video (720p, 4-15 seconds, with audio)\n\
         - seedance_i2v: Image-to-video (requires image_url, 720p, 4-15 seconds)\n\
         The task runs asynchronously — you'll get a task_id back immediately."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "model": {
                    "type": "string",
                    "enum": ["nanobanana", "nanobanana_edit", "seedance_t2v", "seedance_i2v"],
                    "description": "nanobanana=文生图, nanobanana_edit=图生图, seedance_t2v=文生视频, seedance_i2v=图生视频"
                },
                "prompt": {
                    "type": "string",
                    "description": "Describe what to generate"
                },
                "image_url": {
                    "type": "string",
                    "description": "Input image path (required for seedance_i2v). Use the storage path from message content directly (e.g. 'abc123/images/fal_1.png'). Do NOT construct URLs yourself."
                },
                "image_urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Reference image paths (required for nanobanana_edit, max 14). Use storage paths from message content directly. Do NOT construct URLs."
                },
                "num_images": {
                    "type": "integer",
                    "description": "Number of images to generate (default 1)"
                },
                "duration": {
                    "type": "string",
                    "description": "Video duration in seconds: '5' to '15' (default '5')"
                },
                "resolution": {
                    "type": "string",
                    "description": "Resolution: 1K/2K for images, 480p/720p for videos"
                },
                "aspect_ratio": {
                    "type": "string",
                    "description": "Aspect ratio: 16:9, 9:16, 1:1, auto, etc."
                }
            },
            "required": ["model", "prompt"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Rate limiting: max RATE_LIMIT_MAX calls per RATE_LIMIT_WINDOW_SECS seconds
        {
            let mut times = self.call_times.lock().unwrap();
            let cutoff = Instant::now() - std::time::Duration::from_secs(RATE_LIMIT_WINDOW_SECS);
            times.retain(|t| *t > cutoff);
            if times.len() >= RATE_LIMIT_MAX {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "频率限制：{RATE_LIMIT_WINDOW_SECS}秒内最多调用{RATE_LIMIT_MAX}次 fal_generate。\
                        如需生成多张图片，请使用 num_images 参数而不是多次调用。\
                        请等待后再试。"
                    )),
                });
            }
            times.push(Instant::now());
        }

        let model = args["model"].as_str().unwrap_or("");
        let prompt = args["prompt"].as_str().unwrap_or("");

        if model.is_empty() || prompt.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Both 'model' and 'prompt' are required.".to_string()),
            });
        }

        // ── Model-specific parameter validation ──

        if model == "seedance_i2v" {
            let image_url = args.get("image_url").and_then(|v| v.as_str()).unwrap_or("");
            if image_url.is_empty() {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "seedance_i2v 需要 image_url 参数。请从会话消息中获取图片的存储路径（如 \"abc123/images/fal_1.png\"），\
                        直接作为 image_url 传入，不要自己拼接 URL。网关会自动处理图片中转。".to_string()
                    ),
                });
            }
            if image_url.starts_with("http") && !is_trusted_image_url(image_url) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "image_url 看起来不正确：\"{image_url}\"。\
                        不要自己拼接 URL！请直接使用消息中的图片存储路径（如 \"abc123/images/fal_1.png\"），\
                        网关会自动将 TOS 私有路径转为 fal.ai 可访问的公开链接。"
                    )),
                });
            }
        }

        if model == "nanobanana_edit" {
            let urls = args.get("image_urls").and_then(|v| v.as_array());
            if urls.map_or(true, |arr| arr.is_empty()) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "nanobanana_edit 需要 image_urls 参数（字符串数组，最多14张参考图）。\
                        请从会话消息中获取图片的存储路径，直接作为数组元素传入。".to_string()
                    ),
                });
            }
            for u in urls.unwrap() {
                if let Some(s) = u.as_str() {
                    if s.starts_with("http") && !is_trusted_image_url(s) {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "image_urls 中的 URL 看起来不正确：\"{s}\"。\
                                不要自己拼接 URL！请直接使用消息中的图片存储路径（如 \"abc123/images/fal_1.png\"）。"
                            )),
                        });
                    }
                }
            }
        }

        if let Some(dur) = args.get("duration").and_then(|v| v.as_str()) {
            if let Ok(secs) = dur.parse::<u32>() {
                if secs > 15 {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "duration 最大 15 秒，你传了 \"{dur}\"。请用 \"5\" 到 \"15\" 之间的值。"
                        )),
                    });
                }
            }
        }

        // Build parameters object from remaining args
        let mut parameters = json!({});
        for key in &[
            "image_url",
            "image_urls",
            "num_images",
            "duration",
            "resolution",
            "aspect_ratio",
        ] {
            if let Some(val) = args.get(*key) {
                if !val.is_null() {
                    parameters[*key] = val.clone();
                }
            }
        }

        let body = json!({
            "model": model,
            "prompt": prompt,
            "parameters": parameters,
        });

        let url = format!(
            "{}/api/internal/fal/submit",
            self.gateway_url.trim_end_matches('/')
        );

        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("X-API-Key", &self.api_key)
            .header("X-Agent-Id", &self.agent_id)
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                let text = r.text().await.unwrap_or_default();
                if (200..300).contains(&(status as usize)) {
                    Ok(ToolResult {
                        success: true,
                        output: format!(
                            "Generation task submitted successfully. The result will be sent to the conversation automatically when ready.\n\nResponse: {text}"
                        ),
                        error: None,
                    })
                } else {
                    Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Submit failed (HTTP {status}): {text}")),
                    })
                }
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Request failed: {e}")),
            }),
        }
    }
}

/// Check if a URL points to a known trusted image host (fal.ai CDN, TOS public, etc.).
/// Rejects hallucinated URLs from LLMs.
fn is_trusted_image_url(url: &str) -> bool {
    const TRUSTED_HOSTS: &[&str] = &[
        "fal.media",
        "fal.run",
        "fal.ai",
        "volces.com",
        "volcanicengine.com",
        "beeseed.ai",
        "localhost",
        "127.0.0.1",
    ];
    if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        let host = rest.split('/').next().unwrap_or("");
        let host = host.split(':').next().unwrap_or("");
        TRUSTED_HOSTS.iter().any(|t| host == *t || host.ends_with(&format!(".{t}")))
    } else {
        false
    }
}
