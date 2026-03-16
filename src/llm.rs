use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ── trait ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn complete(&self, prompt: &str, on_token: &(dyn for<'a> Fn(&'a str) + Send + Sync)) -> Result<String>;
    fn name(&self) -> &str;
    fn is_local(&self) -> bool;
}

// ── backend resolution ────────────────────────────────────────────────────────

pub enum Backend {
    Ollama,
    Anthropic,
    OpenAi,
    Gemini,
}

/// Resolve which backend to use and which model to call.
/// Returns (backend_impl, resolved_model_name).
pub async fn resolve(
    requested_model: Option<&str>,
    config_backend: &str,
    config_api_key_env: Option<&str>,
    no_cloud: bool,
) -> Result<(Box<dyn LlmBackend>, String)> {
    // If the user named a model, infer the backend from the name
    let backend_hint = requested_model.and_then(infer_backend_from_model);

    let backend = match backend_hint {
        Some(b) => b,
        None => match config_backend {
            "ollama" => Backend::Ollama,
            "anthropic" => Backend::Anthropic,
            "openai" => Backend::OpenAi,
            "gemini" => Backend::Gemini,
            _ => auto_detect(no_cloud, config_api_key_env).await?,
        },
    };

    match backend {
        Backend::Ollama => {
            let models = crate::ollama::list_models().await?;
            let model = requested_model
                .map(|s| s.to_string())
                .or_else(|| crate::ollama::detect_best_model(&models))
                .unwrap_or_else(|| "llama3:8b".to_string());
            Ok((Box::new(OllamaBackend { model: model.clone() }) as Box<dyn LlmBackend>, model))
        }
        Backend::Anthropic => {
            let model = requested_model.unwrap_or("claude-sonnet-4-5").to_string();
            Ok((Box::new(AnthropicBackend::new(config_api_key_env, model.clone())?) as Box<dyn LlmBackend>, model))
        }
        Backend::OpenAi => {
            let model = requested_model.unwrap_or("gpt-4o").to_string();
            Ok((Box::new(OpenAiBackend::new(model.clone())?) as Box<dyn LlmBackend>, model))
        }
        Backend::Gemini => {
            let model = requested_model.unwrap_or("gemini-1.5-pro").to_string();
            Ok((Box::new(GeminiBackend::new(model.clone())?) as Box<dyn LlmBackend>, model))
        }
    }
}

fn infer_backend_from_model(model: &str) -> Option<Backend> {
    let m = model.to_lowercase();
    if m.contains("claude") {
        Some(Backend::Anthropic)
    } else if m.contains("gpt") || m.contains("o1") || m.contains("o3") || m.contains("o4") {
        Some(Backend::OpenAi)
    } else if m.contains("gemini") {
        Some(Backend::Gemini)
    } else {
        None // assume Ollama for everything else
    }
}

async fn auto_detect(no_cloud: bool, api_key_env: Option<&str>) -> Result<Backend> {
    if crate::ollama::is_running().await {
        return Ok(Backend::Ollama);
    }
    if no_cloud {
        anyhow::bail!(
            "Ollama is not running and --no-cloud is set.\n\
             Start Ollama with: ollama serve"
        );
    }
    // Check for cloud API keys
    let anthropic_env = api_key_env.unwrap_or("ANTHROPIC_API_KEY");
    if std::env::var(anthropic_env).is_ok() {
        return Ok(Backend::Anthropic);
    }
    if std::env::var("OPENAI_API_KEY").is_ok() {
        return Ok(Backend::OpenAi);
    }
    if std::env::var("GEMINI_API_KEY").is_ok() || std::env::var("GOOGLE_API_KEY").is_ok() {
        return Ok(Backend::Gemini);
    }
    anyhow::bail!(
        "Ollama is not running and no cloud API key was found.\n\
         Options:\n\
           • Start Ollama:          ollama serve\n\
           • Use Anthropic:         export ANTHROPIC_API_KEY=sk-...\n\
           • Use OpenAI:            export OPENAI_API_KEY=sk-...\n\
           • Use Gemini:            export GEMINI_API_KEY=..."
    );
}

// ── Ollama ────────────────────────────────────────────────────────────────────

pub struct OllamaBackend {
    model: String,
}

#[async_trait]
impl LlmBackend for OllamaBackend {
    async fn complete(&self, prompt: &str, on_token: &(dyn for<'a> Fn(&'a str) + Send + Sync)) -> Result<String> {
        crate::ollama::stream_completion(prompt, &self.model, |t| on_token(t)).await
    }

    fn name(&self) -> &str { "Ollama" }
    fn is_local(&self) -> bool { true }
}

// ── Anthropic ─────────────────────────────────────────────────────────────────

pub struct AnthropicBackend {
    api_key: String,
    model: String,
}

impl AnthropicBackend {
    fn new(key_env: Option<&str>, model: String) -> Result<Self> {
        let env = key_env.unwrap_or("ANTHROPIC_API_KEY");
        let api_key = std::env::var(env)
            .with_context(|| format!("${} is not set", env))?;
        Ok(Self { api_key, model })
    }
}

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    messages: Vec<AnthropicMessage<'a>>,
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct AnthropicEvent {
    #[serde(rename = "type")]
    event_type: String,
    delta: Option<AnthropicDelta>,
}

#[derive(Deserialize)]
struct AnthropicDelta {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    text: Option<String>,
}

#[async_trait]
impl LlmBackend for AnthropicBackend {
    async fn complete(&self, prompt: &str, on_token: &(dyn for<'a> Fn(&'a str) + Send + Sync)) -> Result<String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;

        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&AnthropicRequest {
                model: &self.model,
                max_tokens: 2048,
                stream: true,
                messages: vec![AnthropicMessage { role: "user", content: prompt }],
            })
            .send()
            .await
            .context("Failed to connect to Anthropic API")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API returned {}: {}", status, body);
        }

        let mut stream = resp.bytes_stream();
        let mut full = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("Stream error")?;
            let text = String::from_utf8_lossy(&bytes);

            for line in text.lines() {
                let line = line.trim();
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" { break; }
                    if let Ok(event) = serde_json::from_str::<AnthropicEvent>(data) {
                        if event.event_type == "content_block_delta" {
                            if let Some(delta) = event.delta {
                                if delta.delta_type.as_deref() == Some("text_delta") {
                                    if let Some(text) = delta.text {
                                        on_token(&text);
                                        full.push_str(&text);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(full)
    }

    fn name(&self) -> &str { "Anthropic" }
    fn is_local(&self) -> bool { false }
}

// ── OpenAI ────────────────────────────────────────────────────────────────────

pub struct OpenAiBackend {
    api_key: String,
    base_url: String,
    model: String,
}

impl OpenAiBackend {
    fn new(model: String) -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .context("$OPENAI_API_KEY is not set")?;
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com".to_string());
        Ok(Self { api_key, base_url, model })
    }
}

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    stream: bool,
    messages: Vec<OpenAiMessage<'a>>,
}

#[derive(Serialize)]
struct OpenAiMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OpenAiChunk {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    delta: OpenAiDelta,
}

#[derive(Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
}

#[async_trait]
impl LlmBackend for OpenAiBackend {
    async fn complete(&self, prompt: &str, on_token: &(dyn for<'a> Fn(&'a str) + Send + Sync)) -> Result<String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;

        let resp = client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&OpenAiRequest {
                model: &self.model,
                stream: true,
                messages: vec![OpenAiMessage { role: "user", content: prompt }],
            })
            .send()
            .await
            .context("Failed to connect to OpenAI API")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API returned {}: {}", status, body);
        }

        let mut stream = resp.bytes_stream();
        let mut full = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("Stream error")?;
            let text = String::from_utf8_lossy(&bytes);

            for line in text.lines() {
                let line = line.trim();
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" { break; }
                    if let Ok(chunk) = serde_json::from_str::<OpenAiChunk>(data) {
                        for choice in chunk.choices {
                            if let Some(text) = choice.delta.content {
                                on_token(&text);
                                full.push_str(&text);
                            }
                        }
                    }
                }
            }
        }
        Ok(full)
    }

    fn name(&self) -> &str { "OpenAI" }
    fn is_local(&self) -> bool { false }
}

// ── Gemini ────────────────────────────────────────────────────────────────────

pub struct GeminiBackend {
    api_key: String,
    model: String,
}

impl GeminiBackend {
    fn new(model: String) -> Result<Self> {
        let api_key = std::env::var("GEMINI_API_KEY")
            .or_else(|_| std::env::var("GOOGLE_API_KEY"))
            .context("$GEMINI_API_KEY or $GOOGLE_API_KEY is not set")?;
        Ok(Self { api_key, model })
    }
}

#[derive(Serialize)]
struct GeminiRequest<'a> {
    contents: Vec<GeminiContent<'a>>,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Deserialize)]
struct GeminiChunk {
    candidates: Option<Vec<GeminiCandidate>>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiCandidateContent>,
}

#[derive(Deserialize)]
struct GeminiCandidateContent {
    parts: Option<Vec<GeminiTextPart>>,
}

#[derive(Deserialize)]
struct GeminiTextPart {
    text: Option<String>,
}

#[async_trait]
impl LlmBackend for GeminiBackend {
    async fn complete(&self, prompt: &str, on_token: &(dyn for<'a> Fn(&'a str) + Send + Sync)) -> Result<String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?key={}&alt=sse",
            self.model, self.api_key
        );

        let resp = client
            .post(&url)
            .json(&GeminiRequest {
                contents: vec![GeminiContent {
                    parts: vec![GeminiPart { text: prompt }],
                }],
            })
            .send()
            .await
            .context("Failed to connect to Gemini API")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API returned {}: {}", status, body);
        }

        let mut stream = resp.bytes_stream();
        let mut full = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("Stream error")?;
            let text = String::from_utf8_lossy(&bytes);

            for line in text.lines() {
                let line = line.trim();
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(chunk) = serde_json::from_str::<GeminiChunk>(data) {
                        if let Some(candidates) = chunk.candidates {
                            for candidate in candidates {
                                if let Some(content) = candidate.content {
                                    if let Some(parts) = content.parts {
                                        for part in parts {
                                            if let Some(text) = part.text {
                                                on_token(&text);
                                                full.push_str(&text);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(full)
    }

    fn name(&self) -> &str { "Gemini" }
    fn is_local(&self) -> bool { false }
}
