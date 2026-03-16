use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "http://localhost:11434";

fn base_url() -> String {
    std::env::var("OLLAMA_HOST").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
}

#[derive(Deserialize)]
struct GenerateChunk {
    response: String,
    done: bool,
}

#[derive(Deserialize)]
struct ModelsResponse {
    models: Vec<ModelInfo>,
}

#[derive(Deserialize)]
struct ModelInfo {
    name: String,
}

pub async fn stream_completion(
    prompt: &str,
    model: &str,
    on_token: impl Fn(&str),
) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    let url = format!("{}/api/generate", base_url());

    let resp = client
        .post(&url)
        .json(&GenerateRequest {
            model,
            prompt,
            stream: true,
        })
        .send()
        .await
        .map_err(|e| {
            if e.is_connect() {
                anyhow::anyhow!(
                    "Ollama is not running. Start it with: ollama serve"
                )
            } else {
                anyhow::anyhow!("Request failed: {}", e)
            }
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Ollama returned {}: {}", status, body);
    }

    let mut stream = resp.bytes_stream();
    let mut full_response = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("Stream read error")?;
        let text = String::from_utf8_lossy(&bytes);

        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if let Ok(chunk) = serde_json::from_str::<GenerateChunk>(line) {
                on_token(&chunk.response);
                full_response.push_str(&chunk.response);
                if chunk.done {
                    break;
                }
            }
        }
    }

    Ok(full_response)
}

pub async fn list_models() -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/tags", base_url());

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list models: {}", e))?;

    let models_resp: ModelsResponse = resp
        .json()
        .await
        .context("Failed to parse models response")?;

    Ok(models_resp.models.into_iter().map(|m| m.name).collect())
}

pub async fn is_running() -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    client
        .get(format!("{}/api/tags", base_url()))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

pub async fn pull_model(model: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;

    let url = format!("{}/api/pull", base_url());

    #[derive(Serialize)]
    struct PullRequest<'a> {
        name: &'a str,
        stream: bool,
    }

    let resp = client
        .post(&url)
        .json(&PullRequest {
            name: model,
            stream: false,
        })
        .send()
        .await
        .context("Failed to pull model")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Failed to pull {}: {} {}", model, status, body);
    }

    println!("Pulled model: {}", model);
    Ok(())
}

pub fn detect_best_model(available: &[String]) -> Option<String> {
    let priority = [
        "qwen2.5-coder:14b",
        "qwen2.5-coder:7b",
        "deepseek-coder-v2:16b",
        "codellama:13b",
        "llama3:8b",
    ];

    for preferred in &priority {
        if available.iter().any(|m| m == preferred) {
            return Some(preferred.to_string());
        }
    }

    // Fuzzy fallback: any qwen coder
    for preferred_prefix in &["qwen2.5-coder", "deepseek-coder", "codellama", "llama3"] {
        if let Some(m) = available.iter().find(|m| m.starts_with(preferred_prefix)) {
            return Some(m.clone());
        }
    }

    available.first().cloned()
}
