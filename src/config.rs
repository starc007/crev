use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub review: ReviewConfig,

    #[serde(default)]
    pub privacy: PrivacyConfig,

    #[serde(default)]
    pub ignore: IgnoreConfig,

    #[serde(default)]
    pub rules: Vec<Rule>,

    #[serde(default)]
    pub packs: PacksConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewConfig {
    pub model: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_severity_threshold")]
    pub severity_threshold: String,
    #[serde(default = "default_backend")]
    pub backend: String,
    pub api_key_env: Option<String>,
}

fn default_max_tokens() -> usize {
    8000
}

fn default_severity_threshold() -> String {
    "low".to_string()
}

fn default_backend() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrivacyConfig {
    #[serde(default)]
    pub strip_comments: bool,
    #[serde(default)]
    pub strip_strings: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IgnoreConfig {
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    pub description: String,
    #[serde(default = "default_rule_severity")]
    pub severity: String,
}

fn default_rule_severity() -> String {
    "med".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PacksConfig {
    #[serde(rename = "use", default)]
    pub packs: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            review: ReviewConfig::default(),
            privacy: PrivacyConfig::default(),
            ignore: IgnoreConfig::default(),
            rules: Vec::new(),
            packs: PacksConfig::default(),
        }
    }
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            model: None,
            max_tokens: default_max_tokens(),
            severity_threshold: default_severity_threshold(),
            backend: default_backend(),
            api_key_env: None,
        }
    }
}

pub fn load_config(start_dir: &Path) -> Config {
    // Search upward from start_dir for .reviewrc
    let mut dir = start_dir.to_path_buf();
    loop {
        let candidate = dir.join(".reviewrc");
        if candidate.exists() {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                if let Ok(cfg) = toml::from_str::<Config>(&content) {
                    return cfg;
                }
            }
            break;
        }
        if !dir.pop() {
            break;
        }
    }

    // Fall back to ~/.config/crev/config.toml
    if let Some(home) = home_dir() {
        let global = home.join(".config/crev/config.toml");
        if global.exists() {
            if let Ok(content) = std::fs::read_to_string(&global) {
                if let Ok(cfg) = toml::from_str::<Config>(&content) {
                    return cfg;
                }
            }
        }
    }

    Config::default()
}

pub fn save_default_config(path: &Path) -> Result<()> {
    let content = r#"# crev configuration file
# See https://github.com/your-org/crev for documentation

[review]
# model = "qwen2.5-coder:14b"   # override auto-detected model
max_tokens = 8000
severity_threshold = "low"      # low | med | high — filter output below this
# backend = "auto"              # auto | ollama | anthropic | openai
# api_key_env = "ANTHROPIC_API_KEY"

[privacy]
strip_comments = false           # remove comments before sending to LLM
strip_strings = false            # replace string literals with <REDACTED>

[ignore]
paths = [
  "migrations/",
  "*.generated.rs",
  "vendor/",
  "*.pb.go",
]

# Example custom rules:
# [[rules]]
# name = "no-raw-sql"
# description = "All DB queries must go through QueryBuilder, never raw SQL strings"
#
# [[rules]]
# name = "no-unwrap"
# description = "Never use .unwrap() in non-test code paths, use ? or expect()"
"#;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

pub fn should_ignore_file(path: &Path, config: &Config) -> bool {
    let path_str = path.to_string_lossy();

    for pattern in &config.ignore.paths {
        if glob_match(pattern, &path_str) {
            return true;
        }
    }

    false
}

fn glob_match(pattern: &str, path: &str) -> bool {
    // Use the glob crate for matching
    if let Ok(pat) = glob::Pattern::new(pattern) {
        if pat.matches(path) {
            return true;
        }
    }

    // Also check if path contains the pattern as a substring (for directory prefixes)
    if pattern.ends_with('/') {
        let prefix = pattern.trim_end_matches('/');
        if path.contains(&format!("{}/", prefix)) || path.starts_with(prefix) {
            return true;
        }
    }

    false
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("USERPROFILE").ok().map(PathBuf::from))
}
