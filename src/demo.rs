//! `recherche demo`: serves made-up Claude Code and Codex sessions, so UI
//! changes can be tried and screenshots taken without showing real work.
//!
//! Each file in `demo/sessions/` is one session. The command writes them as
//! the agents would, into `<dir>/claude` and `<dir>/codex`, indexes them into
//! `<dir>/data`, stores their summaries and serves the result. Dates are
//! relative to now, so the demo looks the same whenever it runs. See
//! `demo/README.md` for the fixture format.

use crate::config::Config;
use crate::llm::Summary;
use crate::search::Engine;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(RustEmbed)]
#[folder = "demo/sessions/"]
struct Fixtures;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    agent: Agent,
    /// Working directory, relative to the demo directory.
    cwd: String,
    /// Leave the working directory uncreated, like a deleted worktree.
    #[serde(default)]
    cwd_gone: bool,
    branch: Option<String>,
    /// When the session started, such as `3d`, `5h` or `2d4h30m`.
    ago: String,
    model: Option<String>,
    /// Claude: the title Claude Code generated. Without it, the first prompt
    /// becomes the title.
    title: Option<String>,
    /// Claude: a PR the session opened, as `owner/repo#123`.
    pr: Option<String>,
    /// Claude: makes this a subagent of the named fixture.
    parent: Option<String>,
    /// Codex: the remote recorded in the session, as `owner/repo`.
    repo: Option<String>,
    turns: Vec<Turn>,
    summary: Option<Summary>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Agent {
    Claude,
    Codex,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {
    prompt: String,
    reply: String,
    #[serde(default)]
    tools: Vec<Tool>,
}

/// A shell `command`, or for Claude any tool by `name` and `input`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tool {
    command: Option<String>,
    name: Option<String>,
    input: Option<Value>,
    #[serde(default)]
    output: String,
    #[serde(default)]
    error: bool,
}

/// Fixtures by file stem, with `{root}` in any string replaced by `root`.
fn fixtures(root: &Path) -> Result<Vec<(String, Fixture)>> {
    let root = root.to_string_lossy();
    let mut out = vec![];
    for file in Fixtures::iter() {
        let data = Fixtures::get(&file).unwrap().data;
        let text = std::str::from_utf8(&data)?.replace("{root}", &root);
        let f: Fixture = toml::from_str(&text).with_context(|| format!("parsing {file}"))?;
        let name = file.trim_end_matches(".toml").to_string();
        out.push((name, f));
    }
    Ok(out)
}

/// Marks a directory as made by this command, so a rerun may replace it.
const MARKER: &str = ".recherche-demo";

/// Writes the sessions, indexes them and stores their summaries. Replaces
/// what an earlier run wrote, because the archive would otherwise keep
/// sessions whose fixtures were renamed or removed.
pub fn prepare(mut cfg: Config, dir: &Path) -> Result<Engine> {
    let dir = &std::path::absolute(dir)?;
    if dir.exists() {
        if !dir.join(MARKER).exists() && std::fs::read_dir(dir)?.next().is_some() {
            bail!(
                "{} is not empty and was not made by `recherche demo`; pick another directory",
                dir.display()
            );
        }
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(MARKER), "")?;
    cfg.sources.claude.dir = Some(dir.join("claude"));
    cfg.sources.codex.dir = Some(dir.join("codex"));
    // The UI explains missing summaries only when summaries are on, which
    // needs a key. Any set variable will do: the API is a closed local port,
    // so made-up sessions are never sent anywhere.
    cfg.llm.enabled = true;
    cfg.llm.summaries = true;
    cfg.llm.rewrite = false;
    cfg.llm.api_key_env = "HOME".into();
    cfg.llm.base_url = "http://127.0.0.1:9".into();
    let now = Utc::now();
    let mut summaries = vec![];
    for (name, f) in fixtures(dir)? {
        let id = write(dir, &name, &f, now).with_context(|| format!("writing {name}"))?;
        if let Some(s) = f.summary {
            summaries.push((id, s));
        }
    }
    let engine = Engine::open_in(cfg, &dir.join("data"))?;
    let c = engine.conn()?;
    // As if `serve` first ran 30 days ago: older sessions without a summary
    // wait for `recherche summarize`, newer ones are queued or in progress.
    let since = now - Duration::days(30);
    crate::store::set_meta(&c, "auto_summary_since", &since.timestamp().to_string())?;
    // The server's first pass embeds; keyword search works without it.
    engine.refresh_with(false, false)?;
    let cat = engine.catalog();
    for (id, s) in &summaries {
        crate::summarize::save(&c, id, s, cat.sessions[id].turn_count)?;
    }
    engine.refresh_with(false, false)?;
    eprintln!(
        "{} demo sessions in {}",
        engine.catalog().sessions.len(),
        dir.display()
    );
    Ok(engine)
}

/// Writes one session file and returns the session id recherche will see.
fn write(dir: &Path, name: &str, f: &Fixture, now: DateTime<Utc>) -> Result<String> {
    let cwd = dir.join(&f.cwd);
    if !f.cwd_gone {
        std::fs::create_dir_all(&cwd)?;
    }
    let cwd = cwd.to_string_lossy();
    let start = now - parse_ago(&f.ago)?;
    let (path, lines, id) = match f.agent {
        Agent::Claude => claude(dir, name, f, &cwd, start)?,
        Agent::Codex => codex(dir, name, f, &cwd, start)?,
    };
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut text = String::new();
    for l in lines {
        text.push_str(&l.to_string());
        text.push('\n');
    }
    std::fs::write(&path, text)?;
    Ok(id)
}

type Session = (std::path::PathBuf, Vec<Value>, String);

fn claude(dir: &Path, name: &str, f: &Fixture, cwd: &str, start: DateTime<Utc>) -> Result<Session> {
    let model = f.model.as_deref().unwrap_or("claude-opus-4-5");
    // A subagent's lines carry its parent's session id.
    let sid = uuid(f.parent.as_deref().unwrap_or(name));
    let agent = f.parent.as_ref().map(|_| uuid(name)[..8].to_string());
    let mut base =
        json!({"sessionId": sid, "cwd": cwd, "gitBranch": f.branch.as_deref().unwrap_or("")});
    if let Some(a) = &agent {
        base["agentId"] = json!(a);
    }
    let line = |t: DateTime<Utc>, ty: &str, message: Value| {
        let mut l = base.clone();
        l["type"] = json!(ty);
        l["timestamp"] = json!(ts(t));
        l["message"] = message;
        l
    };
    let mut lines = vec![];
    let mut t = start;
    let mut n = 0;
    for turn in &f.turns {
        lines.push(line(
            t,
            "user",
            json!({"role": "user", "content": turn.prompt}),
        ));
        t += Duration::minutes(1);
        for tool in &turn.tools {
            n += 1;
            let id = format!("toolu_{n:04}");
            let (name, input) = match (&tool.command, &tool.name) {
                (Some(cmd), None) => ("Bash", json!({ "command": cmd })),
                (None, Some(name)) => (name.as_str(), tool.input.clone().unwrap_or(json!({}))),
                _ => bail!("a tool needs either `command` or `name`"),
            };
            let call = json!({"type": "tool_use", "id": id, "name": name, "input": input});
            lines.push(line(
                t,
                "assistant",
                json!({"model": model, "content": [call]}),
            ));
            let result = json!({"type": "tool_result", "tool_use_id": id, "content": tool.output, "is_error": tool.error});
            lines.push(line(
                t,
                "user",
                json!({"role": "user", "content": [result]}),
            ));
            t += Duration::seconds(40);
        }
        let text = json!({"type": "text", "text": turn.reply});
        lines.push(line(
            t,
            "assistant",
            json!({"model": model, "content": [text]}),
        ));
        t += Duration::minutes(4);
    }
    if let Some(title) = &f.title
        && agent.is_none()
    {
        lines.push(json!({"type": "ai-title", "aiTitle": title, "sessionId": sid}));
    }
    if let Some(pr) = &f.pr {
        let (repo, num) = pr
            .split_once('#')
            .context("`pr` must look like owner/repo#123")?;
        lines.push(json!({"type": "pr-link", "prRepository": repo, "prNumber": num.parse::<i64>()?, "sessionId": sid}));
    }
    // Claude Code names project directories after the cwd.
    let project: String = cwd
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let project = dir.join("claude/projects").join(project);
    Ok(match agent {
        Some(a) => {
            let path = project
                .join(&sid)
                .join(format!("subagents/agent-{a}.jsonl"));
            if let Some(title) = &f.title {
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(
                    path.with_extension("meta.json"),
                    json!({ "description": title }).to_string(),
                )?;
            }
            (path, lines, format!("{sid}/agent-{a}"))
        }
        None => (project.join(format!("{sid}.jsonl")), lines, sid),
    })
}

fn codex(dir: &Path, name: &str, f: &Fixture, cwd: &str, start: DateTime<Utc>) -> Result<Session> {
    if f.title.is_some() || f.pr.is_some() || f.parent.is_some() {
        bail!("`title`, `pr` and `parent` are Claude-only");
    }
    let sid = uuid(name);
    let mut git = json!({ "commit_hash": hex(&format!("commit {name}"), 20) });
    if let Some(repo) = &f.repo {
        git["repository_url"] = json!(format!("git@github.com:{repo}.git"));
    }
    if let Some(b) = &f.branch {
        git["branch"] = json!(b);
    }
    let model = f.model.as_deref().unwrap_or("gpt-5.2-codex");
    let mut t = start;
    let mut lines = vec![
        json!({"timestamp": ts(t), "type": "session_meta", "payload": {"id": sid, "cwd": cwd, "source": "cli", "git": git}}),
        json!({"timestamp": ts(t), "type": "turn_context", "payload": {"cwd": cwd, "model": model}}),
    ];
    let item = |t: DateTime<Utc>, item: Value| json!({"timestamp": ts(t), "type": "event_msg", "payload": {"type": "item_completed", "item": item}});
    for turn in &f.turns {
        lines.push(item(
            t,
            json!({"type": "UserMessage", "content": [{"type": "text", "text": turn.prompt}]}),
        ));
        t += Duration::minutes(1);
        for tool in &turn.tools {
            let Some(cmd) = &tool.command else {
                bail!("Codex tools need a `command`");
            };
            lines.push(item(
                t,
                json!({
                    "type": "CommandExecution",
                    "command": ["bash", "-lc", cmd],
                    "exit_code": i32::from(tool.error),
                    "aggregated_output": tool.output,
                }),
            ));
            t += Duration::seconds(30);
        }
        lines.push(item(
            t,
            json!({"type": "AgentMessage", "content": [{"type": "text", "text": turn.reply}]}),
        ));
        t += Duration::minutes(4);
    }
    let path = dir
        .join("codex/sessions")
        .join(start.format("%Y/%m/%d").to_string())
        .join(format!(
            "rollout-{}-{sid}.jsonl",
            start.format("%Y-%m-%dT%H-%M-%S")
        ));
    Ok((path, lines, sid))
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `n` bytes of hex derived from `seed`, so ids stay the same across runs.
fn hex(seed: &str, n: usize) -> String {
    Sha256::digest(seed.as_bytes())[..n]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn uuid(name: &str) -> String {
    let h = hex(name, 16);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// `3d`, `5h`, `45m` or a combination such as `2d4h30m`.
fn parse_ago(s: &str) -> Result<Duration> {
    let mut total = Duration::zero();
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().with_context(|| format!("bad `ago`: {s}"))?;
        num.clear();
        total += match c {
            'd' => Duration::days(n),
            'h' => Duration::hours(n),
            'm' => Duration::minutes(n),
            _ => bail!("bad `ago`: {s}; use d, h and m, such as 2d4h"),
        };
    }
    if !num.is_empty() || s.is_empty() {
        bail!("bad `ago`: {s}; use d, h and m, such as 2d4h");
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{KINDS, OUTCOMES};

    #[test]
    fn ago_parses_combined_units() {
        assert_eq!(
            parse_ago("2d4h30m").unwrap(),
            Duration::minutes(2 * 1440 + 4 * 60 + 30)
        );
        for bad in ["", "3", "3w", "d"] {
            assert!(parse_ago(bad).is_err(), "accepted {bad}");
        }
    }

    /// Catches fixture mistakes in CI instead of when someone runs the demo.
    #[test]
    fn fixtures_load_and_index() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut cfg = Config::default();
        cfg.search.vectors = false;
        cfg.search.rerank = false;
        let fixtures = fixtures(dir.path())?;
        for (name, f) in &fixtures {
            if let Some(s) = &f.summary {
                assert!(
                    s.kind.as_deref().is_none_or(|k| KINDS.contains(&k)),
                    "{name}: kind must be one of {KINDS:?}"
                );
                assert!(
                    s.outcome.as_deref().is_none_or(|o| OUTCOMES.contains(&o)),
                    "{name}: outcome must be one of {OUTCOMES:?}"
                );
            }
            if let Some(p) = &f.parent {
                assert!(
                    fixtures.iter().any(|(n, _)| n == p),
                    "{name}: no parent fixture {p}"
                );
            }
        }
        let engine = prepare(cfg, dir.path())?;
        let cat = engine.catalog();
        assert_eq!(cat.sessions.len(), fixtures.len());
        let summarized = fixtures.iter().filter(|f| f.1.summary.is_some()).count();
        assert_eq!(
            cat.sessions
                .values()
                .filter(|s| s.summary.is_some())
                .count(),
            summarized
        );
        let bad: i64 =
            engine
                .conn()?
                .query_row("SELECT SUM(bad_lines) FROM sessions", [], |r| r.get(0))?;
        assert_eq!(bad, 0);
        Ok(())
    }
}
