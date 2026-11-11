//! Codex sessions: `<codex_dir>/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//!
//! Current Codex writes typed `item_completed` events (UserMessage,
//! AgentMessage, CommandExecution, FileChange, McpToolCall). Older files only
//! have `user_message`/`agent_message` events and `function_call` items; those
//! are read as a fallback.

use crate::model::{clip_chars, clip_output, parse_ts, Fact, ParsedSession, ToolCall, Turn};
use serde_json::Value;
use std::path::Path;

pub fn parse(path: &Path, bytes: &[u8]) -> ParsedSession {
    let mut s = ParsedSession { source: "codex", ..Default::default() };
    let mut branch: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut have_items = false;
    let mut legacy: Vec<(Option<chrono::DateTime<chrono::Utc>>, Value)> = vec![];

    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            s.bad_lines += 1;
            continue;
        };
        let ts = v["timestamp"].as_str().and_then(parse_ts);
        if let Some(t) = ts {
            s.started = Some(s.started.map_or(t, |x| x.min(t)));
            s.ended = Some(s.ended.map_or(t, |x| x.max(t)));
        }
        let p = &v["payload"];
        match v["type"].as_str().unwrap_or("") {
            "session_meta" => {
                s.id = p["id"].as_str().or(p["session_id"].as_str()).unwrap_or("").to_string();
                s.cwd = p["cwd"].as_str().map(String::from);
                cwd = s.cwd.clone();
                let src = p["source"].to_string();
                if src.contains("guardian") || p["thread_source"].as_str() == Some("guardian_review") {
                    s.skip = true;
                }
                s.parent_id = find_key(&p["source"], "parent_thread_id");
                let g = &p["git"];
                if let Some(url) = g["repository_url"].as_str()
                    && let Some(key) = crate::extract::normalize_remote(url) {
                        s.facts.push(Fact::new("repo", key.clone(), crate::extract::repo_display(&key), "recorded", 5));
                    }
                if let Some(b) = g["branch"].as_str() {
                    branch = Some(b.to_string());
                    s.facts.push(Fact::new("branch", b, b, "recorded", 1));
                }
                if let Some(c) = g["commit_hash"].as_str() {
                    s.facts.push(Fact::new("commit", c, &c[..c.len().min(10)], "recorded", 1));
                }
            }
            "turn_context" => {
                if let Some(c) = p["cwd"].as_str() {
                    cwd = Some(c.to_string());
                }
                if s.model.is_none() {
                    s.model = p["model"].as_str().map(String::from);
                }
            }
            "event_msg" if p["type"].as_str() == Some("item_completed") => {
                have_items = true;
                item(&mut s, &p["item"], ts, &branch, &cwd);
            }
            "event_msg" | "response_item" => legacy.push((ts, p.clone())),
            _ => {}
        }
    }
    if !have_items {
        for (ts, p) in &legacy {
            legacy_item(&mut s, p, *ts, &branch, &cwd);
        }
    }
    if s.id.is_empty() {
        s.id = path.file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
    }
    s
}

fn find_key(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(m) => m.get(key).and_then(|x| x.as_str()).map(String::from).or_else(|| m.values().find_map(|x| find_key(x, key))),
        _ => None,
    }
}

fn texts(content: &Value) -> String {
    match content {
        Value::String(t) => t.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn new_turn(s: &mut ParsedSession, ts: Option<chrono::DateTime<chrono::Utc>>, user: String, branch: &Option<String>, cwd: &Option<String>) {
    s.turns.push(Turn { ts, user, branch: branch.clone(), cwd: cwd.clone(), ..Default::default() });
}

fn current<'a>(s: &'a mut ParsedSession, ts: Option<chrono::DateTime<chrono::Utc>>, branch: &Option<String>, cwd: &Option<String>) -> &'a mut Turn {
    if s.turns.is_empty() {
        new_turn(s, ts, String::new(), branch, cwd);
    }
    s.turns.last_mut().unwrap()
}

fn item(s: &mut ParsedSession, it: &Value, ts: Option<chrono::DateTime<chrono::Utc>>, branch: &Option<String>, cwd: &Option<String>) {
    match it["type"].as_str().unwrap_or("") {
        "UserMessage" => {
            let t = texts(&it["content"]).trim().to_string();
            if !t.is_empty() {
                new_turn(s, ts, t, branch, cwd);
            }
        }
        "AgentMessage" => {
            let t = texts(&it["content"]).trim().to_string();
            let turn = current(s, ts, branch, cwd);
            if !t.is_empty() {
                if !turn.assistant.is_empty() {
                    turn.assistant.push('\n');
                }
                turn.assistant.push_str(&t);
            }
        }
        "CommandExecution" => {
            let cmd = match &it["command"] {
                Value::Array(a) => a.last().and_then(|x| x.as_str()).unwrap_or("").to_string(),
                Value::String(c) => c.clone(),
                _ => String::new(),
            };
            let exit = match &it["exit_code"] {
                Value::Number(n) => n.as_i64(),
                Value::String(x) => x.parse().ok(),
                _ => None,
            };
            let out = it["aggregated_output"].as_str().unwrap_or("");
            current(s, ts, branch, cwd).tools.push(ToolCall {
                name: "exec".into(),
                input: cmd,
                output: clip_output(out, 20, 20),
                is_error: exit.is_some_and(|e| e != 0),
                ..Default::default()
            });
        }
        "FileChange" => {
            let files: Vec<String> = it["changes"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
            current(s, ts, branch, cwd).tools.push(ToolCall {
                name: "apply_patch".into(),
                input: files.join(" "),
                files,
                ..Default::default()
            });
        }
        "McpToolCall" => {
            let name = format!("{}.{}", it["server"].as_str().unwrap_or("mcp"), it["tool"].as_str().unwrap_or(""));
            let result = texts(&it["result"]["content"]);
            current(s, ts, branch, cwd).tools.push(ToolCall {
                name,
                input: clip_chars(it["arguments"].as_str().unwrap_or(""), 600),
                output: clip_output(&result, 10, 5),
                ..Default::default()
            });
        }
        "SubAgentActivity" if it["kind"].as_str() == Some("started") => {
            if let Some(child) = it["agent_thread_id"].as_str() {
                s.facts.push(Fact::new("child", child, it["agent_path"].as_str().unwrap_or(child), "recorded", 1));
            }
        }
        _ => {}
    }
}

fn legacy_item(s: &mut ParsedSession, p: &Value, ts: Option<chrono::DateTime<chrono::Utc>>, branch: &Option<String>, cwd: &Option<String>) {
    match p["type"].as_str().unwrap_or("") {
        "user_message" => {
            let t = p["message"].as_str().unwrap_or("").trim().to_string();
            if !t.is_empty() {
                new_turn(s, ts, t, branch, cwd);
            }
        }
        "agent_message" => {
            let t = p["message"].as_str().unwrap_or("").trim().to_string();
            let turn = current(s, ts, branch, cwd);
            if !t.is_empty() {
                turn.assistant.push_str(&t);
                turn.assistant.push('\n');
            }
        }
        "function_call" => {
            let args: Value = serde_json::from_str(p["arguments"].as_str().unwrap_or("{}")).unwrap_or_default();
            let input = match &args["command"] {
                Value::Array(a) => a.last().and_then(|x| x.as_str()).unwrap_or("").to_string(),
                Value::String(c) => c.clone(),
                _ => clip_chars(&args.to_string(), 600),
            };
            current(s, ts, branch, cwd).tools.push(ToolCall {
                id: p["call_id"].as_str().unwrap_or("").into(),
                name: p["name"].as_str().unwrap_or("tool").into(),
                input,
                ..Default::default()
            });
        }
        "function_call_output" => {
            let id = p["call_id"].as_str().unwrap_or("");
            let out = match &p["output"] {
                Value::String(o) => o.clone(),
                other => texts(other),
            };
            if let Some(tc) = s.turns.last_mut().and_then(|t| t.tools.iter_mut().find(|t| t.id == id)) {
                tc.output = clip_output(&out, 20, 20);
            }
        }
        _ => {}
    }
}
