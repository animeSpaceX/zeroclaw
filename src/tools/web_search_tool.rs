use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Unified web search tool with three engines:
/// - `brave`: Web/news/images/videos search via Brave Search API
/// - `exa`: Semantic search, similar-page finding, and content extraction via Exa.ai
/// - `social`: Social media data via ScrapeCreators API
pub struct WebSearchTool {
    security: Arc<SecurityPolicy>,
}

impl WebSearchTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }

    fn http_client(&self) -> reqwest::Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
    }

    // ── Brave Search ────────────────────────────────────────────

    async fn search_brave(&self, args: &serde_json::Value) -> anyhow::Result<String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            anyhow::bail!("Brave search requires a 'query' parameter");
        }

        // Platform gateway proxy path
        let gateway_url = std::env::var("GATEWAY_URL").ok().filter(|s| !s.is_empty());
        let platform_key = std::env::var("PLATFORM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        let agent_id = std::env::var("AGENT_ID").ok().filter(|s| !s.is_empty());

        if let (Some(gw), Some(key), Some(aid)) = (gateway_url, platform_key, agent_id) {
            return self
                .brave_via_gateway(&gw, &key, &aid, query, args)
                .await;
        }

        let api_key = std::env::var("BRAVE_SEARCH_API_KEY")
            .map_err(|_| anyhow::anyhow!("BRAVE_SEARCH_API_KEY not set"))?;

        let search_type = args
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("web");
        let count = args
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(20);

        let mut url = format!(
            "https://api.search.brave.com/res/v1/{}/search?q={}&count={}",
            search_type,
            urlencoding::encode(query),
            count
        );

        if let Some(country) = args.get("country").and_then(|v| v.as_str()) {
            url.push_str(&format!("&country={}", urlencoding::encode(country)));
        }
        if let Some(lang) = args.get("lang").and_then(|v| v.as_str()) {
            url.push_str(&format!("&search_lang={}", urlencoding::encode(lang)));
        }
        if let Some(freshness) = args.get("freshness").and_then(|v| v.as_str()) {
            url.push_str(&format!("&freshness={}", urlencoding::encode(freshness)));
        }

        let client = self.http_client()?;
        let response = client
            .get(&url)
            .header("Accept", "application/json")
            .header("Accept-Encoding", "gzip")
            .header("X-Subscription-Token", &api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Brave search failed ({}): {}", status, body);
        }

        let data: serde_json::Value = response.json().await?;
        self.format_brave_results(&data, query, search_type)
    }

    async fn brave_via_gateway(
        &self,
        gateway_url: &str,
        api_key: &str,
        agent_id: &str,
        query: &str,
        args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        let url = format!(
            "{}/api/internal/search/brave",
            gateway_url.trim_end_matches('/')
        );
        let search_type = args
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("web");
        let count = args
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(20);

        let client = self.http_client()?;
        let response = client
            .post(&url)
            .header("X-Agent-Id", agent_id)
            .header("X-API-Key", api_key)
            .json(&json!({
                "query": query,
                "count": count,
                "type": search_type,
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Gateway brave search failed ({}): {}", status, body);
        }

        let data: serde_json::Value = response.json().await?;
        self.format_brave_results(&data, query, search_type)
    }

    fn format_brave_results(
        &self,
        data: &serde_json::Value,
        query: &str,
        search_type: &str,
    ) -> anyhow::Result<String> {
        // Determine results path based on search type
        let results_key = match search_type {
            "images" => "images",
            "videos" => "videos",
            "news" => "news",
            _ => "web",
        };
        let results = data
            .get(results_key)
            .and_then(|w| w.get("results"))
            .and_then(|r| r.as_array());

        let results = match results {
            Some(r) if !r.is_empty() => r,
            _ => return Ok(format!("No results found for: {}", query)),
        };

        let mut output = json!({
            "engine": "brave",
            "query": query,
            "type": search_type,
            "results": []
        });

        let out_results = output["results"].as_array_mut().unwrap();

        for item in results {
            let mut entry = json!({});
            if let Some(v) = item.get("title").and_then(|v| v.as_str()) {
                entry["title"] = json!(v);
            }
            if let Some(v) = item.get("url").and_then(|v| v.as_str()) {
                entry["url"] = json!(v);
            }
            if let Some(v) = item.get("description").and_then(|v| v.as_str()) {
                entry["snippet"] = json!(v);
            }
            if let Some(v) = item.get("age").and_then(|v| v.as_str()) {
                entry["published_date"] = json!(v);
            }
            // Images: image_url
            if let Some(v) = item
                .get("properties")
                .and_then(|p| p.get("url"))
                .and_then(|v| v.as_str())
            {
                entry["image_url"] = json!(v);
            }
            // Videos: duration
            if let Some(v) = item
                .get("video")
                .and_then(|p| p.get("duration"))
                .and_then(|v| v.as_str())
            {
                entry["duration"] = json!(v);
            }
            // Source
            if let Some(v) = item
                .get("meta_url")
                .and_then(|p| p.get("hostname"))
                .and_then(|v| v.as_str())
            {
                entry["source"] = json!(v);
            }
            // Thumbnail
            if let Some(v) = item
                .get("thumbnail")
                .and_then(|p| p.get("src"))
                .and_then(|v| v.as_str())
            {
                entry["thumbnail"] = json!(v);
            }

            out_results.push(entry);
        }

        serde_json::to_string_pretty(&output)
            .map_err(|e| anyhow::anyhow!("JSON serialization failed: {}", e))
    }

    // ── Exa Search ──────────────────────────────────────────────

    async fn search_exa(&self, args: &serde_json::Value) -> anyhow::Result<String> {
        let api_key =
            std::env::var("EXA_API_KEY").map_err(|_| anyhow::anyhow!("EXA_API_KEY not set"))?;

        let client = self.http_client()?;
        let base_url = "https://api.exa.ai";

        // Determine mode: extract_urls → contents, find_similar_url → findSimilar, else → search
        let extract_urls = args
            .get("extract_urls")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let find_similar_url = args
            .get("find_similar_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let (endpoint, body) = if !extract_urls.is_empty() {
            // Contents mode
            let urls: Vec<&str> = extract_urls.split(',').map(|s| s.trim()).collect();
            let body = json!({
                "urls": urls,
                "text": true,
            });
            (format!("{}/contents", base_url), body)
        } else if !find_similar_url.is_empty() {
            // FindSimilar mode
            let num_results = args
                .get("count")
                .and_then(|v| v.as_u64())
                .unwrap_or(10)
                .min(20);
            let mut body = json!({
                "url": find_similar_url,
                "numResults": num_results,
                "excludeSourceDomain": true,
                "contents": {
                    "text": true,
                    "highlights": { "query": find_similar_url }
                }
            });
            if let Some(category) = args.get("category").and_then(|v| v.as_str()) {
                body["category"] = json!(category);
            }
            (format!("{}/findSimilar", base_url), body)
        } else {
            // Search mode
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if query.is_empty() {
                anyhow::bail!("Exa search requires a 'query' parameter");
            }
            let num_results = args
                .get("count")
                .and_then(|v| v.as_u64())
                .unwrap_or(10)
                .min(20);
            let mut body = json!({
                "query": query,
                "type": "auto",
                "numResults": num_results,
                "useAutoprompt": true,
                "contents": {
                    "text": true,
                    "highlights": { "query": query }
                }
            });
            if let Some(category) = args.get("category").and_then(|v| v.as_str()) {
                body["category"] = json!(category);
            }
            if let Some(domains) = args.get("include_domains").and_then(|v| v.as_str()) {
                let domain_list: Vec<&str> = domains.split(',').map(|s| s.trim()).collect();
                body["includeDomains"] = json!(domain_list);
            }
            if let Some(start) = args.get("start_date").and_then(|v| v.as_str()) {
                body["startPublishedDate"] = json!(start);
            }
            if let Some(end) = args.get("end_date").and_then(|v| v.as_str()) {
                body["endPublishedDate"] = json!(end);
            }
            (format!("{}/search", base_url), body)
        };

        let response = client
            .post(&endpoint)
            .header("x-api-key", &api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Exa API failed ({}): {}", status, body);
        }

        let data: serde_json::Value = response.json().await?;
        self.format_exa_results(&data, args)
    }

    fn format_exa_results(
        &self,
        data: &serde_json::Value,
        args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        let results = data.get("results").and_then(|r| r.as_array());

        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("find_similar_url").and_then(|v| v.as_str()))
            .or_else(|| args.get("extract_urls").and_then(|v| v.as_str()))
            .unwrap_or("");

        let mode = if args
            .get("extract_urls")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
        {
            "contents"
        } else if args
            .get("find_similar_url")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
        {
            "findSimilar"
        } else {
            "search"
        };

        let results = match results {
            Some(r) if !r.is_empty() => r,
            _ => return Ok(format!("No results found for: {}", query)),
        };

        let mut output = json!({
            "engine": "exa",
            "mode": mode,
            "query": query,
            "results": []
        });

        if let Some(rid) = data.get("requestId").and_then(|v| v.as_str()) {
            output["metadata"] = json!({ "requestId": rid });
        }

        let out_results = output["results"].as_array_mut().unwrap();

        for item in results {
            let mut entry = json!({});
            if let Some(v) = item.get("title").and_then(|v| v.as_str()) {
                entry["title"] = json!(v);
            }
            if let Some(v) = item.get("url").and_then(|v| v.as_str()) {
                entry["url"] = json!(v);
            }
            if let Some(v) = item.get("publishedDate").and_then(|v| v.as_str()) {
                entry["published_date"] = json!(v);
            }
            if let Some(v) = item.get("author").and_then(|v| v.as_str()) {
                entry["author"] = json!(v);
            }
            if let Some(v) = item.get("score").and_then(|v| v.as_f64()) {
                entry["score"] = json!(v);
            }
            // Snippet from highlights or text
            if let Some(highlights) = item.get("highlights").and_then(|v| v.as_array()) {
                let snippet: Vec<&str> = highlights
                    .iter()
                    .filter_map(|h| h.as_str())
                    .collect();
                if !snippet.is_empty() {
                    entry["snippet"] = json!(snippet.join("\n"));
                }
            } else if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                // Truncate to first 500 chars for snippet
                let snippet: String = text.chars().take(500).collect();
                entry["snippet"] = json!(snippet);
            }
            // Full text for contents mode
            if mode == "contents" {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    entry["content"] = json!(text);
                }
            }

            out_results.push(entry);
        }

        serde_json::to_string_pretty(&output)
            .map_err(|e| anyhow::anyhow!("JSON serialization failed: {}", e))
    }

    // ── Volcengine Search ─────────────────────────────────────────

    async fn search_volcengine(&self, args: &serde_json::Value) -> anyhow::Result<String> {
        let api_key = std::env::var("VOLCENGINE_SEARCH_KEY")
            .map_err(|_| anyhow::anyhow!("VOLCENGINE_SEARCH_KEY not set"))?;

        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            anyhow::bail!("Volcengine search requires a 'query' parameter");
        }

        let search_type = args
            .get("search_type")
            .and_then(|v| v.as_str())
            .unwrap_or("web");
        let count = args
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(10);
        let max_count = if search_type == "image" { 5 } else { 50 };
        let count = count.min(max_count);

        let mut body = json!({
            "Query": query,
            "SearchType": search_type,
            "Count": count,
            "NeedSummary": true,
            "ContentFormats": "markdown",
        });

        if let Some(time_range) = args.get("time_range").and_then(|v| v.as_str()) {
            body["TimeRange"] = json!(time_range);
        }

        let body_obj = body.as_object_mut().unwrap();

        // Build Filter object
        let sites = args.get("sites").and_then(|v| v.as_str());
        let need_content = args.get("need_content").and_then(|v| v.as_bool());
        if sites.is_some() || need_content.is_some() {
            let mut filter = serde_json::Map::new();
            if let Some(s) = sites {
                filter.insert("Sites".to_string(), json!(s));
            }
            if let Some(nc) = need_content {
                filter.insert("NeedContent".to_string(), json!(nc));
            }
            body_obj.insert("Filter".to_string(), json!(filter));
        }

        let client = self.http_client()?;
        let response = client
            .post("https://open.feedcoopapi.com/search_api/web_search")
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let resp_body = response.text().await.unwrap_or_default();
            anyhow::bail!("Volcengine search failed ({}): {}", status, resp_body);
        }

        // web_summary returns SSE stream (data: prefix lines), others return plain JSON
        let raw_text = response.text().await?;
        let data = if search_type == "web_summary" {
            self.parse_volcengine_sse(&raw_text)?
        } else {
            let parsed: serde_json::Value = serde_json::from_str(&raw_text)
                .map_err(|e| anyhow::anyhow!("JSON parse failed: {}", e))?;
            parsed
                .get("Result")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        };

        self.format_volcengine_results(&data, query, search_type)
    }

    /// Parse Volcengine SSE stream (web_summary mode).
    /// First event has WebResults, subsequent events have Choices with streamed AI summary.
    /// Merges all into a single JSON object with WebResults + assembled Choices.
    fn parse_volcengine_sse(&self, raw: &str) -> anyhow::Result<serde_json::Value> {
        let mut web_results = serde_json::Value::Null;
        let mut summary_parts: Vec<String> = Vec::new();

        for line in raw.lines() {
            let line = line.trim();
            let json_str = if let Some(stripped) = line.strip_prefix("data:") {
                stripped.trim()
            } else if line.starts_with('{') {
                line
            } else {
                continue;
            };

            if json_str.is_empty() {
                continue;
            }

            let parsed: serde_json::Value = match serde_json::from_str(json_str) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let result = match parsed.get("Result") {
                Some(r) => r,
                None => continue,
            };

            // First event with WebResults
            if result.get("WebResults").and_then(|v| v.as_array()).is_some() {
                web_results = result.clone();
            }

            // Collect streamed summary content from Choices
            if let Some(choices) = result.get("Choices").and_then(|v| v.as_array()) {
                for choice in choices {
                    if let Some(content) = choice
                        .get("Delta")
                        .and_then(|d| d.get("Content"))
                        .and_then(|v| v.as_str())
                    {
                        summary_parts.push(content.to_string());
                    }
                    // Also check Message.Content (final event)
                    if let Some(content) = choice
                        .get("Message")
                        .and_then(|m| m.get("Content"))
                        .and_then(|v| v.as_str())
                    {
                        if !content.is_empty() {
                            summary_parts.push(content.to_string());
                        }
                    }
                }
            }
        }

        // Merge: put assembled summary into Choices format for format_volcengine_results
        if !summary_parts.is_empty() {
            let full_summary = summary_parts.join("");
            let choices = json!([{
                "Message": {
                    "Content": full_summary
                }
            }]);
            if web_results.is_object() {
                web_results["Choices"] = choices;
            } else {
                web_results = json!({ "Choices": choices });
            }
        }

        Ok(web_results)
    }

    fn format_volcengine_results(
        &self,
        data: &serde_json::Value,
        query: &str,
        search_type: &str,
    ) -> anyhow::Result<String> {
        let mut results: Vec<serde_json::Value> = Vec::new();
        let mut ai_summary: Option<String> = None;

        if search_type == "image" {
            // Image results
            if let Some(items) = data.get("ImageResults").and_then(|v| v.as_array()) {
                for item in items {
                    let mut entry = json!({});
                    if let Some(v) = item.get("Title").and_then(|v| v.as_str()) {
                        entry["title"] = json!(v);
                    }
                    if let Some(v) = item.get("SiteName").and_then(|v| v.as_str()) {
                        entry["source"] = json!(v);
                    }
                    if let Some(v) = item.get("Url").and_then(|v| v.as_str()) {
                        entry["url"] = json!(v);
                    }
                    if let Some(img) = item.get("Image") {
                        if let Some(u) = img.get("Url").and_then(|v| v.as_str()) {
                            entry["image_url"] = json!(u);
                        }
                        if let Some(w) = img.get("Width").and_then(|v| v.as_u64()) {
                            entry["width"] = json!(w);
                        }
                        if let Some(h) = img.get("Height").and_then(|v| v.as_u64()) {
                            entry["height"] = json!(h);
                        }
                    }
                    results.push(entry);
                }
            }
        } else {
            // Web / web_summary results
            if let Some(items) = data.get("WebResults").and_then(|v| v.as_array()) {
                for item in items {
                    let mut entry = json!({});
                    if let Some(v) = item.get("Title").and_then(|v| v.as_str()) {
                        entry["title"] = json!(v);
                    }
                    if let Some(v) = item.get("Url").and_then(|v| v.as_str()) {
                        entry["url"] = json!(v);
                    }
                    if let Some(v) = item.get("SiteName").and_then(|v| v.as_str()) {
                        entry["source"] = json!(v);
                    }
                    if let Some(v) = item.get("Summary").and_then(|v| v.as_str()) {
                        entry["summary"] = json!(v);
                    }
                    if let Some(v) = item.get("Content").and_then(|v| v.as_str()) {
                        // Truncate content to 1000 chars for output size
                        let content: String = v.chars().take(1000).collect();
                        entry["content"] = json!(content);
                    }
                    if let Some(v) = item.get("PublishTime").and_then(|v| v.as_str()) {
                        entry["published_date"] = json!(v);
                    }
                    results.push(entry);
                }
            }

            // web_summary: extract LLM summary from Choices
            if search_type == "web_summary" {
                if let Some(choices) = data.get("Choices").and_then(|v| v.as_array()) {
                    let summary_parts: Vec<&str> = choices
                        .iter()
                        .filter_map(|c| {
                            c.get("Message")
                                .and_then(|m| m.get("Content"))
                                .and_then(|v| v.as_str())
                        })
                        .collect();
                    if !summary_parts.is_empty() {
                        ai_summary = Some(summary_parts.join("\n"));
                    }
                }
            }
        }

        if results.is_empty() && ai_summary.is_none() {
            return Ok(format!("No results found for: {}", query));
        }

        let mut output = json!({
            "engine": "volcengine",
            "query": query,
            "search_type": search_type,
            "results": results
        });

        if let Some(summary) = ai_summary {
            output["ai_summary"] = json!(summary);
        }

        serde_json::to_string_pretty(&output)
            .map_err(|e| anyhow::anyhow!("JSON serialization failed: {}", e))
    }

    // ── Social Search (ScrapeCreators) ──────────────────────────

    async fn search_social(&self, args: &serde_json::Value) -> anyhow::Result<String> {
        let api_key = std::env::var("SCRAPECREATORS_API_KEY")
            .map_err(|_| anyhow::anyhow!("SCRAPECREATORS_API_KEY not set"))?;

        let platform = args
            .get("platform")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let social_action = args
            .get("social_action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();

        if platform.is_empty() || social_action.is_empty() {
            anyhow::bail!("Social search requires 'platform' and 'social_action' parameters");
        }

        let endpoint_key = format!("{}:{}", platform, social_action);
        let path = match endpoint_key.as_str() {
            // Twitter
            "twitter:profile" => "/v1/twitter/profile",
            "twitter:user-tweets" => "/v1/twitter/user-tweets",
            "twitter:tweet" => "/v1/twitter/tweet",
            "twitter:transcript" => "/v1/twitter/tweet/transcript",
            // Instagram
            "instagram:profile" => "/v1/instagram/profile",
            "instagram:posts" => "/v2/instagram/user/posts",
            "instagram:comments" => "/v2/instagram/post/comments",
            "instagram:transcript" => "/v2/instagram/media/transcript",
            "instagram:search" => "/v2/instagram/reels/search",
            // TikTok
            "tiktok:profile" => "/v1/tiktok/profile",
            "tiktok:transcript" => "/v1/tiktok/video/transcript",
            "tiktok:search" => "/v1/tiktok/search/keyword",
            "tiktok:comments" => "/v1/tiktok/video/comments",
            // YouTube
            "youtube:search" => "/v1/youtube/search",
            "youtube:channel" => "/v1/youtube/channel",
            "youtube:video" => "/v1/youtube/video",
            "youtube:transcript" => "/v1/youtube/video/transcript",
            "youtube:comments" => "/v1/youtube/video/comments",
            // Reddit
            "reddit:search" => "/v1/reddit/search",
            "reddit:subreddit" => "/v1/reddit/subreddit",
            "reddit:comments" => "/v1/reddit/post/comments",
            // Threads
            "threads:profile" => "/v1/threads/profile",
            "threads:posts" => "/v1/threads/user/posts",
            "threads:search" => "/v1/threads/search",
            // Bluesky
            "bluesky:profile" => "/v1/bluesky/profile",
            "bluesky:posts" => "/v1/bluesky/user/posts",
            // LinkedIn
            "linkedin:profile" => "/v1/linkedin/profile",
            "linkedin:company" => "/v1/linkedin/company",
            "linkedin:posts" => "/v1/linkedin/company/posts",
            // Facebook
            "facebook:profile" => "/v1/facebook/profile",
            "facebook:posts" => "/v1/facebook/profile/posts",
            // Pinterest
            "pinterest:search" => "/v1/pinterest/search",
            "pinterest:pin" => "/v1/pinterest/pin",
            // Google
            "google:search" => "/v1/google/search",
            _ => {
                anyhow::bail!(
                    "Unknown social endpoint: {}:{}. Supported platforms: twitter, instagram, tiktok, youtube, reddit, threads, bluesky, linkedin, facebook, pinterest, google",
                    platform,
                    social_action
                );
            }
        };

        let mut url = format!("https://api.scrapecreators.com{}", path);
        let mut params: Vec<String> = Vec::new();

        if let Some(handle) = args.get("handle").and_then(|v| v.as_str()) {
            params.push(format!("handle={}", urlencoding::encode(handle)));
        }
        if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
            params.push(format!("query={}", urlencoding::encode(query)));
        }
        if let Some(u) = args.get("url").and_then(|v| v.as_str()) {
            params.push(format!("url={}", urlencoding::encode(u)));
        }
        if let Some(count) = args.get("count").and_then(|v| v.as_u64()) {
            params.push(format!("amount={}", count.min(20)));
        }

        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }

        let client = self.http_client()?;
        let response = client
            .get(&url)
            .header("x-api-key", &api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "ScrapeCreators API failed ({}): {}",
                status,
                body
            );
        }

        let data: serde_json::Value = response.json().await?;
        self.format_social_results(&data, platform, social_action)
    }

    fn format_social_results(
        &self,
        data: &serde_json::Value,
        platform: &str,
        action: &str,
    ) -> anyhow::Result<String> {
        let output = json!({
            "engine": "social",
            "platform": platform,
            "action": action,
            "data": data,
        });

        serde_json::to_string_pretty(&output)
            .map_err(|e| anyhow::anyhow!("JSON serialization failed: {}", e))
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search_tool"
    }

    fn description(&self) -> &str {
        "Search the web using four engines: brave (web/news/images/videos search), exa (semantic search, find similar pages, extract content from URLs), social (social media platforms: twitter, instagram, tiktok, youtube, reddit, threads, bluesky, linkedin, facebook, pinterest), volcengine (high-quality Chinese web/image search with optional AI summary). Use the 'action' parameter to select an engine."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["brave", "exa", "social", "volcengine"],
                    "description": "Search engine: brave (web/news/images/videos), exa (semantic/similar/extract), social (social media platforms), volcengine (Chinese web/image search with AI summary)"
                },
                "query": {
                    "type": "string",
                    "description": "Search query (required for brave and exa search mode)"
                },
                "type": {
                    "type": "string",
                    "enum": ["web", "news", "images", "videos"],
                    "description": "Brave search type (default: web)"
                },
                "count": {
                    "type": "integer",
                    "description": "Number of results (default: 10, max: 20)"
                },
                "country": {
                    "type": "string",
                    "description": "Country code for brave (CN, US, etc.)"
                },
                "lang": {
                    "type": "string",
                    "description": "Language for brave (zh, en, etc.)"
                },
                "freshness": {
                    "type": "string",
                    "description": "Time filter for brave: pd (24h), pw (week), pm (month)"
                },
                "category": {
                    "type": "string",
                    "description": "Exa category filter (news, research paper, company, tweet, etc.)"
                },
                "include_domains": {
                    "type": "string",
                    "description": "Exa: comma-separated domain allowlist"
                },
                "start_date": {
                    "type": "string",
                    "description": "Exa: start date (ISO format)"
                },
                "end_date": {
                    "type": "string",
                    "description": "Exa: end date (ISO format)"
                },
                "find_similar_url": {
                    "type": "string",
                    "description": "Exa: find similar content to this URL (switches to findSimilar mode)"
                },
                "extract_urls": {
                    "type": "string",
                    "description": "Exa: extract content from these URLs (comma-separated, switches to contents mode)"
                },
                "platform": {
                    "type": "string",
                    "description": "Social platform: twitter, instagram, tiktok, youtube, reddit, threads, bluesky, linkedin, facebook, pinterest, google"
                },
                "social_action": {
                    "type": "string",
                    "description": "Social action: profile, user-tweets, tweet, search, posts, comments, transcript, channel, video, subreddit, pin, company, etc."
                },
                "handle": {
                    "type": "string",
                    "description": "Social: username/handle"
                },
                "url": {
                    "type": "string",
                    "description": "Social: content URL (for tweet/transcript/comments)"
                },
                "search_type": {
                    "type": "string",
                    "description": "Volcengine search type: web (default), web_summary (with AI summary), image"
                },
                "time_range": {
                    "type": "string",
                    "description": "Volcengine time filter: OneDay, OneWeek, OneMonth, OneYear, or YYYY-MM-DD..YYYY-MM-DD"
                },
                "sites": {
                    "type": "string",
                    "description": "Volcengine: pipe-separated domain allowlist (e.g. zhihu.com|baidu.com, max 20)"
                },
                "need_content": {
                    "type": "boolean",
                    "description": "Volcengine: only return results with full page content"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: rate limit exceeded".into()),
            });
        }

        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        tracing::info!(action = action, "web_search_tool executing");

        let result = match action {
            "brave" => self.search_brave(&args).await?,
            "exa" => self.search_exa(&args).await?,
            "social" => self.search_social(&args).await?,
            "volcengine" => self.search_volcengine(&args).await?,
            "" => anyhow::bail!("Missing required parameter: action"),
            other => anyhow::bail!(
                "Unknown action: '{}'. Use 'brave', 'exa', 'social', or 'volcengine'.",
                other
            ),
        };

        Ok(ToolResult {
            success: true,
            output: result,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn test_tool_name() {
        let tool = WebSearchTool::new(test_security());
        assert_eq!(tool.name(), "web_search_tool");
    }

    #[test]
    fn test_tool_description() {
        let tool = WebSearchTool::new(test_security());
        assert!(tool.description().contains("brave"));
        assert!(tool.description().contains("exa"));
        assert!(tool.description().contains("social"));
        assert!(tool.description().contains("volcengine"));
    }

    #[test]
    fn test_parameters_schema() {
        let tool = WebSearchTool::new(test_security());
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["platform"].is_object());
        assert_eq!(schema["required"], json!(["action"]));
    }

    #[tokio::test]
    async fn test_execute_missing_action() {
        let tool = WebSearchTool::new(test_security());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_unknown_action() {
        let tool = WebSearchTool::new(test_security());
        let result = tool.execute(json!({"action": "google"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_execute_volcengine_without_api_key() {
        std::env::remove_var("VOLCENGINE_SEARCH_KEY");
        let tool = WebSearchTool::new(test_security());
        let result = tool
            .execute(json!({"action": "volcengine", "query": "test"}))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("VOLCENGINE_SEARCH_KEY"));
    }

    #[tokio::test]
    async fn test_execute_volcengine_missing_query() {
        std::env::set_var("VOLCENGINE_SEARCH_KEY", "test-key");
        let tool = WebSearchTool::new(test_security());
        let result = tool
            .execute(json!({"action": "volcengine"}))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("'query'"));
        std::env::remove_var("VOLCENGINE_SEARCH_KEY");
    }

    #[tokio::test]
    async fn test_execute_brave_without_api_key() {
        // Clear env to ensure no key
        std::env::remove_var("BRAVE_SEARCH_API_KEY");
        std::env::remove_var("GATEWAY_URL");
        let tool = WebSearchTool::new(test_security());
        let result = tool
            .execute(json!({"action": "brave", "query": "test"}))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("BRAVE_SEARCH_API_KEY"));
    }

    #[tokio::test]
    async fn test_execute_exa_without_api_key() {
        std::env::remove_var("EXA_API_KEY");
        let tool = WebSearchTool::new(test_security());
        let result = tool
            .execute(json!({"action": "exa", "query": "test"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("EXA_API_KEY"));
    }

    #[tokio::test]
    async fn test_execute_social_without_api_key() {
        std::env::remove_var("SCRAPECREATORS_API_KEY");
        let tool = WebSearchTool::new(test_security());
        let result = tool
            .execute(json!({"action": "social", "platform": "twitter", "social_action": "profile", "handle": "test"}))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("SCRAPECREATORS_API_KEY"));
    }

    #[tokio::test]
    async fn test_execute_social_missing_params() {
        std::env::set_var("SCRAPECREATORS_API_KEY", "test-key");
        let tool = WebSearchTool::new(test_security());
        let result = tool.execute(json!({"action": "social"})).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("'platform' and 'social_action'"));
        std::env::remove_var("SCRAPECREATORS_API_KEY");
    }

    #[tokio::test]
    async fn test_execute_social_unknown_endpoint() {
        std::env::set_var("SCRAPECREATORS_API_KEY", "test-key");
        let tool = WebSearchTool::new(test_security());
        let result = tool
            .execute(json!({"action": "social", "platform": "mastodon", "social_action": "profile"}))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unknown social endpoint"));
        std::env::remove_var("SCRAPECREATORS_API_KEY");
    }

    #[tokio::test]
    async fn test_execute_blocked_in_read_only_mode() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = WebSearchTool::new(security);
        let result = tool
            .execute(json!({"action": "brave", "query": "rust"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[test]
    fn test_format_brave_results_empty() {
        let tool = WebSearchTool::new(test_security());
        let data = json!({"web": {"results": []}});
        let result = tool.format_brave_results(&data, "test", "web").unwrap();
        assert!(result.contains("No results found"));
    }

    #[test]
    fn test_format_brave_results_with_data() {
        let tool = WebSearchTool::new(test_security());
        let data = json!({
            "web": {
                "results": [{
                    "title": "Example",
                    "url": "https://example.com",
                    "description": "A test page"
                }]
            }
        });
        let result = tool.format_brave_results(&data, "test", "web").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["engine"], "brave");
        assert_eq!(parsed["results"][0]["title"], "Example");
        assert_eq!(parsed["results"][0]["url"], "https://example.com");
    }

    #[test]
    fn test_format_volcengine_results_web() {
        let tool = WebSearchTool::new(test_security());
        let data = json!({
            "WebResults": [{
                "Title": "测试标题",
                "Url": "https://example.com/article",
                "SiteName": "Example",
                "Summary": "这是一篇测试文章的摘要",
                "PublishTime": "2026-04-20"
            }]
        });
        let result = tool.format_volcengine_results(&data, "测试", "web").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["engine"], "volcengine");
        assert_eq!(parsed["search_type"], "web");
        assert_eq!(parsed["results"][0]["title"], "测试标题");
        assert_eq!(parsed["results"][0]["source"], "Example");
    }

    #[test]
    fn test_format_volcengine_results_image() {
        let tool = WebSearchTool::new(test_security());
        let data = json!({
            "ImageResults": [{
                "Title": "Cat photo",
                "SiteName": "Photos",
                "Url": "https://example.com/page",
                "Image": {
                    "Url": "https://example.com/cat.jpg",
                    "Width": 800,
                    "Height": 600
                }
            }]
        });
        let result = tool.format_volcengine_results(&data, "猫", "image").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["engine"], "volcengine");
        assert_eq!(parsed["results"][0]["image_url"], "https://example.com/cat.jpg");
        assert_eq!(parsed["results"][0]["width"], 800);
    }

    #[test]
    fn test_format_volcengine_results_web_summary() {
        let tool = WebSearchTool::new(test_security());
        let data = json!({
            "WebResults": [{
                "Title": "AI News",
                "Url": "https://example.com/ai",
                "Summary": "AI progress summary"
            }],
            "Choices": [{
                "Message": {
                    "Content": "根据搜索结果，AI 领域最新进展包括..."
                }
            }]
        });
        let result = tool.format_volcengine_results(&data, "AI", "web_summary").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["engine"], "volcengine");
        assert!(parsed["ai_summary"].as_str().unwrap().contains("AI 领域"));
    }

    #[test]
    fn test_format_volcengine_results_empty() {
        let tool = WebSearchTool::new(test_security());
        let data = json!({});
        let result = tool.format_volcengine_results(&data, "nothing", "web").unwrap();
        assert!(result.contains("No results found"));
    }

    #[test]
    fn test_format_exa_results_empty() {
        let tool = WebSearchTool::new(test_security());
        let data = json!({"results": []});
        let args = json!({"query": "test"});
        let result = tool.format_exa_results(&data, &args).unwrap();
        assert!(result.contains("No results found"));
    }

    #[test]
    fn test_format_exa_results_with_data() {
        let tool = WebSearchTool::new(test_security());
        let data = json!({
            "results": [{
                "title": "ML Paper",
                "url": "https://arxiv.org/paper",
                "score": 0.95,
                "text": "This is a paper about machine learning."
            }],
            "requestId": "abc123"
        });
        let args = json!({"query": "machine learning"});
        let result = tool.format_exa_results(&data, &args).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["engine"], "exa");
        assert_eq!(parsed["mode"], "search");
        assert_eq!(parsed["results"][0]["title"], "ML Paper");
        assert_eq!(parsed["metadata"]["requestId"], "abc123");
    }
}
