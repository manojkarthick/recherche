//! Cloud LLM calls through OpenRouter (any OpenAI-compatible chat
//! completions API works) for query rewrite and session summaries. Search
//! keeps working when the key is missing or the API is down. Spend limits are
//! left to the provider (OpenRouter key limits).

use crate::config;
use crate::store;
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub struct Llm {
    cfg: config::Llm,
    db: PathBuf,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rewrite {
    #[serde(default)]
    pub terms: Vec<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
}

impl Prices {
    /// Rough cost of one call: about 3 characters per input token, and the
    /// full `max_tokens` of output.
    pub fn estimate(&self, input_chars: usize, max_tokens: i64) -> f64 {
        ((input_chars / 3) as f64 + 50.0) * self.input + max_tokens as f64 * self.output
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Summary {
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

pub const KINDS: &[&str] = &[
    "bugfix",
    "feature",
    "design",
    "migration",
    "review",
    "research",
    "ops",
    "refactor",
    "docs",
];
pub const OUTCOMES: &[&str] = &["fixed", "shipped", "investigated", "planned", "abandoned"];

const SUMMARY_MAX_TOKENS: i64 = 400;

fn summary_system() -> String {
    format!(
        "You summarize one coding-agent session so its owner can find it later by a vague memory. Reply with one JSON object and nothing else: \
         {{\"title\": a specific title under 80 characters, \
         \"summary\": a string containing 2 to 4 short plain bullet points, each starting with '- ' and separated by a newline; \
         each bullet captures one useful fact about the work, a decision or change, or the result; \
         use no headings or labels, mention unfinished work only when important, and name relevant repos, PRs, tickets, tools and error messages, \
         \"kind\": one of {KINDS:?}, \"outcome\": one of {OUTCOMES:?}, \"tags\": up to 5 short lowercase topic tags}}."
    )
}

/// Upper-bound cost of summarizing one digest: the full output allowance is
/// counted even though most summaries stop well short of it.
pub fn summary_estimate(p: &Prices, digest_chars: usize) -> f64 {
    p.estimate(summary_system().len() + digest_chars, SUMMARY_MAX_TOKENS)
}

impl Llm {
    pub fn new(cfg: config::Llm, db: PathBuf) -> Self {
        // reqwest panics building a client when no rustls provider is
        // installed. Err only means one is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self {
            cfg,
            db,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap(),
        }
    }

    fn key(&self) -> Option<String> {
        std::env::var(&self.cfg.api_key_env)
            .ok()
            .filter(|k| !k.is_empty())
    }

    pub fn available(&self) -> bool {
        self.cfg.enabled && self.key().is_some()
    }

    pub fn rewrite_enabled(&self) -> bool {
        self.available() && self.cfg.rewrite
    }

    pub fn summaries_enabled(&self) -> bool {
        self.available() && self.cfg.summaries
    }

    /// Current USD-per-token prices of the summary model, from OpenRouter's
    /// public model list (no key needed).
    pub async fn prices(&self) -> Result<Prices> {
        let v: Value = self
            .http
            .get(format!(
                "{}/models",
                self.cfg.base_url.trim_end_matches('/')
            ))
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let m = v["data"]
            .as_array()
            .and_then(|ms| ms.iter().find(|m| m["id"] == self.cfg.model.as_str()))
            .ok_or_else(|| anyhow!("{} is not in the model list", self.cfg.model))?;
        // Prices are decimal strings, in USD per token.
        let price = |k: &str| -> Result<f64> {
            m["pricing"][k]
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow!("{} has no {k} price", self.cfg.model))
        };
        Ok(Prices {
            input: price("prompt")?,
            output: price("completion")?,
        })
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        user: &str,
        max_tokens: i64,
        timeout: Duration,
    ) -> Result<String> {
        if !self.cfg.enabled {
            bail!("LLM features are disabled");
        }
        let key = self
            .key()
            .ok_or_else(|| anyhow!("{} is not set", self.cfg.api_key_env))?;
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
            "response_format": {"type": "json_object"},
            // Reasoning models such as DeepSeek V4 otherwise spend the whole
            // `max_tokens` budget thinking and return empty content.
            "reasoning": {"enabled": false},
        });
        if self.cfg.deny_data_collection {
            // OpenRouter: only route to hosts that do not store or train on prompts.
            body["provider"] = json!({"data_collection": "deny"});
        }
        let resp = self
            .http
            .post(format!(
                "{}/chat/completions",
                self.cfg.base_url.trim_end_matches('/')
            ))
            .timeout(timeout)
            .bearer_auth(key)
            .header("X-Title", "recherche")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let v: Value = resp.json().await?;
        if !status.is_success() || v["error"].is_object() {
            bail!(
                "LLM API {status}: {}",
                v["error"]["message"].as_str().unwrap_or("")
            );
        }
        let output = v["usage"]["completion_tokens"].as_i64().unwrap_or(0);
        let text = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if json_object(&text).is_none() {
            bail!(
                "{model} replied without JSON (finish reason {}, {output} output tokens): {}",
                v["choices"][0]["finish_reason"]
                    .as_str()
                    .unwrap_or("unknown"),
                crate::model::clip_chars(&text, 200)
            );
        }
        Ok(text)
    }

    /// Expands a vague query into terms the transcript may have used instead.
    pub async fn rewrite(&self, query: &str, repos: &[String]) -> Result<Rewrite> {
        let norm = query.trim().to_lowercase();
        let db = store::open(&self.db)?;
        if let Ok(j) = db.query_row("SELECT json FROM rewrite_cache WHERE q = ?", [&norm], |r| {
            r.get::<_, String>(0)
        }) {
            return Ok(serde_json::from_str(&j)?);
        }
        drop(db);
        let system = format!(
            "You expand search queries for a personal archive of coding-agent sessions (Claude Code and Codex transcripts). \
             The user half-remembers a session. Reply with one JSON object and nothing else: \
             {{\"terms\": [up to 8 short alternative words, phrases or identifiers the transcript may use instead of the user's words], \
             \"repo\": one of the known repos if the query clearly names it, else null, \
             \"since\": \"YYYY-MM-DD\" or null, \"until\": \"YYYY-MM-DD\" or null (only if the query states a time), \
             \"kind\": one of {KINDS:?} or null, \"outcome\": one of {OUTCOMES:?} or null}}. \
             Today is {}. Prefer concrete technical synonyms (for \"history tables\": audit table, SCD2, temporal table, _history). Do not invent details.",
            chrono::Local::now().format("%Y-%m-%d")
        );
        let user = format!("Known repos: {}\nQuery: {query}", repos.join(", "));
        let model = if self.cfg.rewrite_model.is_empty() {
            &self.cfg.model
        } else {
            &self.cfg.rewrite_model
        };
        let text = self
            .complete(model, &system, &user, 200, Duration::from_secs(4))
            .await?;
        let rw: Rewrite = serde_json::from_str(
            json_object(&text).ok_or_else(|| anyhow!("rewrite reply had no JSON"))?,
        )?;
        let db = store::open(&self.db)?;
        db.execute(
            "INSERT OR REPLACE INTO rewrite_cache (q, json, ts) VALUES (?, ?, ?)",
            rusqlite::params![
                norm,
                serde_json::to_string(&rw)?,
                chrono::Utc::now().timestamp()
            ],
        )?;
        Ok(rw)
    }

    pub async fn summarize(&self, digest: &str) -> Result<Summary> {
        let system = summary_system();
        let text = self
            .complete(
                &self.cfg.model,
                &system,
                digest,
                SUMMARY_MAX_TOKENS,
                Duration::from_secs(60),
            )
            .await?;
        let mut s: Summary = serde_json::from_str(
            json_object(&text).ok_or_else(|| anyhow!("summary reply had no JSON"))?,
        )?;
        s.kind = s.kind.filter(|k| KINDS.contains(&k.as_str()));
        s.outcome = s.outcome.filter(|o| OUTCOMES.contains(&o.as_str()));
        s.tags.truncate(5);
        Ok(s)
    }
}

fn json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}
