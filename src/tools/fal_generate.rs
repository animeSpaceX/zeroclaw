use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

pub struct FalGenerateTool {
    gateway_url: String,
    agent_id: String,
    api_key: String,
}

impl FalGenerateTool {
    pub fn new(gateway_url: String, agent_id: String) -> Self {
        let api_key = std::env::var("PLATFORM_API_KEY").unwrap_or_default();
        Self {
            gateway_url,
            agent_id,
            api_key,
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
                    "description": "Input image URL (required for seedance_i2v)"
                },
                "image_urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Reference image URLs (required for nanobanana_edit, max 14)"
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
        let model = args["model"].as_str().unwrap_or("");
        let prompt = args["prompt"].as_str().unwrap_or("");

        if model.is_empty() || prompt.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Both 'model' and 'prompt' are required.".to_string()),
            });
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
