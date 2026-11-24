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
    pub ui: Ui,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Sources {
    pub claude: ClaudeSource,
    pub codex: CodexSource,
    pub antigravity: AntigravitySource,
    pub cursor: CursorSource,
    pub poll_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClaudeSource {
    pub dir: Option<PathBuf>,
    /// Sessions whose every `entrypoint` is listed here are not indexed.
    /// `sdk-cli` is `claude -p`.
    pub exclude_entrypoints: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CodexSource {
    pub dir: Option<PathBuf>,
    /// Sessions whose `session_meta.source` is listed here are not indexed.
    /// `exec` is `codex exec`.
    pub exclude_sources: Vec<String>,
}

/// The Antigravity CLI (`agy`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AntigravitySource {
    pub dir: Option<PathBuf>,
}

/// Cursor (`agent` CLI and IDE agent transcripts).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CursorSource {
    pub dir: Option<PathBuf>,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Ui {
    /// Hide `<system_instruction>` blocks that harnesses such as Conductor
    /// prepend to prompts, and from titles taken from such a prompt, in the
    /// web UI and the API. The text stays indexed and searchable.
    pub hide_system_instructions: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            hide_system_instructions: true,
        }
    }
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
                // Codex puts the id before the repo, so it must precede the generic layout.
                r"/\.codex/worktrees/[^/]+/(?P<repo>[^/]+)".into(),
                r"/\.cursor/worktrees/(?P<repo>[^/]+)/[^/]+".into(),
                r"/\.herdr/worktrees/(?P<repo>[^/]+)/[^/]+".into(),
                r"/conductor/workspaces/(?P<repo>[^/]+)/[^/]+".into(),
                r"/worktrees/(?P<repo>[^/]+)/[^/]+".into(),
                r"^(?P<root>.*/(?P<repo>[^/]+))/\.claude/worktrees/[^/]+".into(),
            ],
            llm: Llm::default(),
            search: Search::default(),
            ui: Ui::default(),
        }
    }
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            claude: ClaudeSource::default(),
            codex: CodexSource::default(),
            antigravity: AntigravitySource::default(),
            cursor: CursorSource::default(),
            poll_seconds: 30,
        }
    }
}

impl Default for ClaudeSource {
    fn default() -> Self {
        Self {
            dir: None,
            exclude_entrypoints: vec!["sdk-cli".into()],
        }
    }
}

impl Default for CodexSource {
    fn default() -> Self {
        Self {
            dir: None,
            exclude_sources: vec!["exec".into()],
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
            summarize_sources: vec![
                "claude".into(),
                "codex".into(),
                "antigravity".into(),
                "cursor".into(),
            ],
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

pub fn env_file() -> PathBuf {
    config_dir().join(".env")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvFileStatus {
    Missing,
    Loaded(PathBuf),
    Warning(String),
}

/// Loads `KEY=VALUE` pairs from `path` into the process environment without
/// overwriting variables that are already set. Refuses files accessible by
/// group or others on Unix. Must be called before spawning threads.
pub fn load_env_file(path: &Path) -> EnvFileStatus {
    match check_env_file(path) {
        Ok(false) => return EnvFileStatus::Missing,
        Ok(true) => {}
        Err(msg) => return EnvFileStatus::Warning(msg),
    }
    match dotenvy::from_path(path) {
        Ok(()) => EnvFileStatus::Loaded(path.to_path_buf()),
        Err(e) => EnvFileStatus::Warning(format!("loading {}: {e}", path.display())),
    }
}

fn check_env_file(path: &Path) -> Result<bool, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if meta.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = meta.permissions().mode() & 0o777;
        if perm & 0o077 != 0 {
            let quoted = crate::util::shell_quote(&path.to_string_lossy());
            return Err(format!(
                "{} is accessible by group or others (mode {:04o}); run `chmod 600 {quoted}`",
                path.display(),
                perm
            ));
        }
    }
    Ok(true)
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
        self.sources.claude.dir.clone().unwrap_or_else(|| {
            match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(d) => PathBuf::from(d),
                None => home().join(".claude"),
            }
        })
    }

    pub fn codex_dir(&self) -> PathBuf {
        self.sources
            .codex
            .dir
            .clone()
            .unwrap_or_else(|| match std::env::var_os("CODEX_HOME") {
                Some(d) => PathBuf::from(d),
                None => home().join(".codex"),
            })
    }

    pub fn antigravity_dir(&self) -> PathBuf {
        self.sources
            .antigravity
            .dir
            .clone()
            .unwrap_or_else(|| home().join(".gemini/antigravity-cli"))
    }

    pub fn cursor_dir(&self) -> PathBuf {
        self.sources
            .cursor
            .dir
            .clone()
            .unwrap_or_else(|| home().join(".cursor"))
    }

    pub fn is_loopback(&self) -> bool {
        self.listen.starts_with("127.")
            || self.listen.starts_with("localhost")
            || self.listen.starts_with("[::1]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_env(dir: &Path, name: &str, contents: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn missing_env_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_env_file(&dir.path().join(".env")),
            EnvFileStatus::Missing
        );
    }

    #[cfg(unix)]
    #[test]
    fn env_file_rejects_group_or_world_permissions() {
        let dir = tempfile::tempdir().unwrap();
        for (name, mode) in [("e644", 0o644), ("e640", 0o640), ("e604", 0o604)] {
            let path = write_env(dir.path(), name, "RECHERCHE_TEST_UNSAFE=1\n", mode);
            match load_env_file(&path) {
                EnvFileStatus::Warning(msg) => {
                    assert!(msg.contains("chmod 600"), "unexpected warning: {msg}");
                }
                other => panic!("expected warning for mode {mode:o}, got {other:?}"),
            }
            assert!(std::env::var_os("RECHERCHE_TEST_UNSAFE").is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn env_file_loads_600_and_preserves_existing_vars() {
        let dir = tempfile::tempdir().unwrap();
        let orig_home = std::env::var("HOME").unwrap();
        let path = write_env(
            dir.path(),
            ".env",
            "HOME=/should/not/overwrite\nRECHERCHE_TEST_DOTENV_KEY=sk-test-123\n",
            0o600,
        );
        assert_eq!(load_env_file(&path), EnvFileStatus::Loaded(path));
        assert_eq!(std::env::var("HOME").unwrap(), orig_home);
        assert_eq!(
            std::env::var("RECHERCHE_TEST_DOTENV_KEY").unwrap(),
            "sk-test-123"
        );
    }

    #[cfg(unix)]
    #[test]
    fn env_file_reports_syntax_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_env(dir.path(), ".env", "NOT VALID LINE\n", 0o600);
        assert!(matches!(load_env_file(&path), EnvFileStatus::Warning(_)));
    }
}
