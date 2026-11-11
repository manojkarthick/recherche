//! Configuration and XDG paths. Every OS uses XDG locations, including macOS,
//! so the data is easy to find and back up.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    /// Required when `listen` is not a loopback address.
    pub token: Option<String>,
    pub sources: Sources,
    /// Regexes matched against prompts, branches and commit subjects.
    pub ticket_patterns: Vec<String>,
    /// Prefixes that look like tickets but never are, such as `UTF-8`.
    pub ticket_stoplist: Vec<String>,
    /// Regexes with a named group `repo`, matched against a session's cwd.
    /// They name the repo of a worktree when no session recorded its remote.
    pub worktree_patterns: Vec<String>,
    pub llm: Llm,
    pub search: Search,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub claude_dir: Option<PathBuf>,
    pub codex_dir: Option<PathBuf>,
    pub poll_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Llm {
    pub enabled: bool,
    pub base_url: String,
    /// Environment variable that holds the API key.
    pub api_key_env: String,
    /// OpenRouter model ID used for summaries, and for rewrite unless
    /// `rewrite_model` is set.
    pub model: String,
    /// Empty means: use `model`.
    pub rewrite_model: String,
    /// Ask OpenRouter to route only to hosts that do not store or train on prompts.
    pub deny_data_collection: bool,
    pub rewrite: bool,
    pub summaries: bool,
    /// Sources whose sessions may be sent for summaries.
    pub summarize_sources: Vec<String>,
    /// cwd substrings whose sessions are never sent for summaries.
    pub summarize_exclude_paths: Vec<String>,
    /// Minutes without new lines before a session counts as idle.
    pub idle_minutes: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Search {
    pub vectors: bool,
    pub rerank: bool,
    /// "jina-turbo" (fast, English) or "bge-base" (larger).
    pub rerank_model: String,
    /// How many fused sessions the reranker rescores.
    pub rerank_top: usize,
    /// Unload the embedding and rerank models after this many idle seconds.
    pub model_idle_seconds: u64,
    /// Text blocks seen in at least this many sessions are treated as
    /// injected boilerplate and left out of the index.
    pub boilerplate_min_sessions: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7777".into(),
            token: None,
            sources: Sources::default(),
            ticket_patterns: vec![r"\b[A-Z][A-Z0-9]{1,9}-\d{1,6}\b".into()],
            ticket_stoplist: [
                "UTF", "SHA", "ISO", "GPT", "RFC", "CVE", "HTTP", "TLS", "SSL", "AES", "MD", "X",
                "LTS", "IPV", "PEP", "BGE", "CO", "UTC", "GMT", "PST", "PDT", "NODE", "PNPM",
                "NPM", "PR", "PY", "IOS", "MACOS", "PYTHON", "JAVA",
            ]
            .map(String::from)
            .to_vec(),
            worktree_patterns: vec![
                r"/worktrees/(?P<repo>[^/]+)/[^/]+".into(),
                r"^(?P<root>.*/(?P<repo>[^/]+))/\.claude/worktrees/[^/]+".into(),
            ],
            llm: Llm::default(),
            search: Search::default(),
        }
    }
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            claude_dir: None,
            codex_dir: None,
            poll_seconds: 30,
        }
    }
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key_env: "OPENROUTER_API_KEY".into(),
            model: "deepseek/deepseek-v4.1-flash".into(),
            rewrite_model: String::new(),
            deny_data_collection: true,
            rewrite: true,
            summaries: true,
            summarize_sources: vec!["claude".into(), "codex".into()],
            summarize_exclude_paths: vec![],
            idle_minutes: 20,
        }
    }
}

impl Default for Search {
    fn default() -> Self {
        Self {
            vectors: true,
            rerank: true,
            rerank_model: "jina-turbo".into(),
            rerank_top: 20,
            model_idle_seconds: 300,
            boilerplate_min_sessions: 4,
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("recherche")
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("RECHERCHE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| xdg("XDG_DATA_HOME", ".local/share").join("recherche"))
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("recherche")
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir().join("config.toml"));
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.sources.claude_dir.clone().unwrap_or_else(|| {
            match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(d) => PathBuf::from(d),
                None => home().join(".claude"),
            }
        })
    }

    pub fn codex_dir(&self) -> PathBuf {
        self.sources
            .codex_dir
            .clone()
            .unwrap_or_else(|| match std::env::var_os("CODEX_HOME") {
                Some(d) => PathBuf::from(d),
                None => home().join(".codex"),
            })
    }

    pub fn is_loopback(&self) -> bool {
        self.listen.starts_with("127.")
            || self.listen.starts_with("localhost")
            || self.listen.starts_with("[::1]")
    }
}
