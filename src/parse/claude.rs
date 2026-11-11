//! Claude Code sessions: `<claude_dir>/projects/<project>/<session>.jsonl`, and
//! subagents in `<project>/<session>/subagents/agent-<id>.jsonl`.

use crate::model::{Fact, ParsedSession, ToolCall, Turn, clip_chars, clip_output, parse_ts};
use regex::Regex;
use serde_json::Value;
use std::path::Path;
use std::sync::LazyLock;

pub fn parse(path: &Path, bytes: &[u8]) -> ParsedSession {
    let mut s = ParsedSession {
        source: "claude",
        ..Default::default()
    };
    let mut agent_id: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut title_ai: Option<String> = None;
    let mut title_named: Option<String> = None;

    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            s.bad_lines += 1;
            continue;
        };
        let ty = v["type"].as_str().unwrap_or("");
        if s.id.is_empty()
            && let Some(id) = v["sessionId"].as_str()
        {
            s.id = id.to_string();
        }
        if agent_id.is_none() {
            agent_id = v["agentId"].as_str().map(String::from);
        }
        if let Some(c) = v["cwd"].as_str() {
            if s.cwd.is_none() {
                s.cwd = Some(c.to_string());
            }
            cwd = Some(c.to_string());
        }
        if let Some(b) = v["gitBranch"]
            .as_str()
            .filter(|b| !b.is_empty() && *b != "HEAD")
        {
            branch = Some(b.to_string());
        }
        let ts = v["timestamp"].as_str().and_then(parse_ts);
        if let Some(t) = ts {
            s.started = Some(s.started.map_or(t, |x| x.min(t)));
            s.ended = Some(s.ended.map_or(t, |x| x.max(t)));
        }

        match ty {
            "user" => {
                if v["isMeta"].as_bool() == Some(true)
                    || v["isCompactSummary"].as_bool() == Some(true)
                {
                    continue;
                }
                let content = &v["message"]["content"];
                let mut prompt = String::new();
                match content {
                    Value::String(t) => prompt = clean_user_text(t),
                    Value::Array(items) => {
                        for it in items {
                            match it["type"].as_str() {
                                Some("tool_result") => attach_result(&mut s.turns, it),
                                Some("text") => {
                                    let t = clean_user_text(it["text"].as_str().unwrap_or(""));
                                    if !t.is_empty() {
                                        if !prompt.is_empty() {
                                            prompt.push('\n');
                                        }
                                        prompt.push_str(&t);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if !prompt.is_empty() {
                    s.turns.push(Turn {
                        ts,
                        user: prompt,
                        branch: branch.clone(),
                        cwd: cwd.clone(),
                        ..Default::default()
                    });
                }
            }
            "assistant" => {
                if s.model.is_none() {
                    s.model = v["message"]["model"]
                        .as_str()
                        .filter(|m| !m.starts_with('<'))
                        .map(String::from);
                }
                let Some(items) = v["message"]["content"].as_array() else {
                    continue;
                };
                if s.turns.is_empty() {
                    s.turns.push(Turn {
                        ts,
                        branch: branch.clone(),
                        cwd: cwd.clone(),
                        ..Default::default()
                    });
                }
                let turn = s.turns.last_mut().unwrap();
                if turn.branch.is_none() {
                    turn.branch = branch.clone();
                }
                for it in items {
                    match it["type"].as_str() {
                        Some("text") => {
                            let t = it["text"].as_str().unwrap_or("").trim();
                            if !t.is_empty() {
                                if !turn.assistant.is_empty() {
                                    turn.assistant.push('\n');
                                }
                                turn.assistant.push_str(t);
                            }
                        }
                        Some("tool_use") => turn.tools.push(tool_call(it)),
                        _ => {}
                    }
                }
            }
            "pr-link" => {
                if let (Some(repo), Some(n)) = (v["prRepository"].as_str(), v["prNumber"].as_i64())
                {
                    let key = format!("{}#{}", repo.to_lowercase(), n);
                    let f = Fact::new("pr", key, format!("{repo}#{n}"), "recorded", 3);
                    if !s.facts.contains(&f) {
                        s.facts.push(f);
                    }
                    let r = Fact::new(
                        "repo",
                        format!("github.com/{}", repo.to_lowercase()),
                        repo,
                        "recorded",
                        4,
                    );
                    if !s.facts.contains(&r) {
                        s.facts.push(r);
                    }
                }
            }
            "ai-title" => title_ai = v["aiTitle"].as_str().map(String::from),
            "agent-name" => title_named = v["agentName"].as_str().map(String::from),
            "system" if v["subtype"].as_str() == Some("away_summary") => {
                s.recap = v["content"].as_str().map(String::from);
            }
            _ => {}
        }
    }

    if let Some(aid) = agent_id {
        // A subagent file shares its parent's sessionId.
        s.parent_id = Some(s.id.clone());
        s.id = format!("{}/agent-{}", s.id, aid);
        title_named = title_named.or_else(|| subagent_name(path));
    }
    if s.id.is_empty() {
        s.id = path
            .file_stem()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    s.title = title_named.or(title_ai);
    s
}

fn subagent_name(path: &Path) -> Option<String> {
    let meta = path.with_extension("meta.json");
    let v: Value = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
    v["description"]
        .as_str()
        .or(v["name"].as_str())
        .map(String::from)
}

fn tool_call(it: &Value) -> ToolCall {
    let name = it["name"].as_str().unwrap_or("tool").to_string();
    let inp = &it["input"];
    let mut files = vec![];
    let s = |k: &str| inp[k].as_str().unwrap_or("").to_string();
    let input = match name.as_str() {
        "Bash" => s("command"),
        "Edit" | "Write" | "Read" | "NotebookEdit" | "MultiEdit" => {
            let p = if inp["file_path"].is_string() {
                s("file_path")
            } else {
                s("notebook_path")
            };
            if !p.is_empty() {
                files.push(p.clone());
            }
            p
        }
        "Grep" | "Glob" => format!("{} {}", s("pattern"), s("path")).trim().to_string(),
        "WebSearch" => s("query"),
        "WebFetch" => s("url"),
        "Agent" | "Task" => format!("{}\n{}", s("description"), clip_chars(&s("prompt"), 1500)),
        "Skill" => s("skill"),
        _ => clip_chars(&inp.to_string(), 600),
    };
    ToolCall {
        id: it["id"].as_str().unwrap_or("").to_string(),
        name,
        input,
        files,
        ..Default::default()
    }
}

fn attach_result(turns: &mut [Turn], it: &Value) {
    let id = it["tool_use_id"].as_str().unwrap_or("");
    let text = match &it["content"] {
        Value::String(t) => t.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    for turn in turns.iter_mut().rev().take(3) {
        if let Some(tc) = turn.tools.iter_mut().find(|t| t.id == id) {
            tc.is_error = it["is_error"].as_bool().unwrap_or(false);
            // File reads repeat file contents that are already on disk; keep less.
            tc.output = if tc.name == "Read" {
                clip_output(&text, 5, 0)
            } else {
                clip_output(&text, 20, 20)
            };
            return;
        }
    }
}

static DROP_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<(system-reminder|local-command-stdout|local-command-stderr|local-command-caveat|bash-stdout|bash-stderr|task-notification|command-message)>.*?</(system-reminder|local-command-stdout|local-command-stderr|local-command-caveat|bash-stdout|bash-stderr|task-notification|command-message)>").unwrap()
});
static KEEP_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"</?(command-name|command-args|bash-input|teammate-message)[^>]*>").unwrap()
});

/// Removes text Claude Code injects into user turns, keeping what the user typed.
pub fn clean_user_text(t: &str) -> String {
    let t = t.trim();
    if t.starts_with("Base directory for this skill:")
        || t.starts_with("Caveat: The messages below were generated")
        || t.starts_with("[Request interrupted")
        || t.starts_with("This session is being continued from a previous conversation")
    {
        return String::new();
    }
    let t = DROP_TAGS.replace_all(t, "");
    let t = KEEP_TAGS.replace_all(&t, " ");
    let mut out = String::new();
    for line in t.lines().map(str::trim_end) {
        if line.is_empty() && (out.is_empty() || out.ends_with("\n\n")) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}
