//! Cursor agent transcripts:
//! `<cursor_dir>/projects/<project>/agent-transcripts/<id>/<id>.jsonl` (or flat
//! `<id>.jsonl`), and subagents in `<parent_id>/subagents/<sub_id>.jsonl`.
//!
//! Transcripts do not carry titles, models or working directories. Cursor keeps
//! those in `<cursor_dir>/chats/<hash>/<id>/{meta.json,store.db}`, in
//! `<cursor_dir>/projects/<project>/sdk-agent-store/<hash>/index.db`, and in
//! `<cursor_dir>/projects/<project>/.workspace-trusted`. Ingest copies the
//! merged metadata into `<id>.meta.json` beside the archived transcript (see
//! `write_meta`), so a rebuild still has them after Cursor drops the originals.

use crate::model::{ParsedSession, ToolCall, Turn, clip_chars, clip_output, parse_ts};
use anyhow::Result;
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

pub fn parse(path: &Path, bytes: &[u8]) -> ParsedSession {
    let mut s = ParsedSession {
        source: "cursor",
        id: path
            .file_stem()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_default(),
        parent_id: subagent_parent(path),
        ..Default::default()
    };
    let mut meta_started = None;
    let mut meta_ended = None;
    if let Some(meta) = std::fs::read(path.with_extension("meta.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        let text = |k: &str| meta[k].as_str().filter(|t| !t.is_empty()).map(String::from);
        s.title = text("title");
        s.cwd = text("cwd");
        s.model = text("model");
        meta_started = meta["created_at_ms"]
            .as_i64()
            .and_then(DateTime::from_timestamp_millis);
        meta_ended = meta["updated_at_ms"]
            .as_i64()
            .and_then(DateTime::from_timestamp_millis);
    }

    let mut path_hints: Vec<String> = vec![];

    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            s.bad_lines += 1;
            continue;
        };
        match v["role"].as_str() {
            Some("user") => {
                let mut raw = String::new();
                match &v["message"]["content"] {
                    Value::String(t) => raw = t.clone(),
                    Value::Array(items) => {
                        for it in items {
                            match it["type"].as_str() {
                                Some("tool_result") => attach_result(&mut s.turns, it),
                                Some("text") => {
                                    if let Some(t) = it["text"].as_str() {
                                        if !raw.is_empty() {
                                            raw.push('\n');
                                        }
                                        raw.push_str(t);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                let ts = extract_timestamp(&raw);
                if let Some(t) = ts {
                    record_ts(&mut s, t);
                }
                let user = user_text(&raw);
                if !user.is_empty() {
                    s.turns.push(Turn {
                        ts,
                        user,
                        cwd: s.cwd.clone(),
                        ..Default::default()
                    });
                }
            }
            Some("assistant") => {
                fn ensure_turn(s: &mut ParsedSession) -> &mut Turn {
                    if s.turns.is_empty() {
                        s.turns.push(Turn {
                            cwd: s.cwd.clone(),
                            ..Default::default()
                        });
                    }
                    s.turns.last_mut().unwrap()
                }
                match &v["message"]["content"] {
                    Value::String(t) => {
                        let t = t.trim();
                        if !t.is_empty() {
                            let turn = ensure_turn(&mut s);
                            append_assistant(&mut turn.assistant, t);
                        }
                    }
                    Value::Array(items) => {
                        let turn = ensure_turn(&mut s);
                        for it in items {
                            match it["type"].as_str() {
                                Some("text") => {
                                    let t = it["text"].as_str().unwrap_or("").trim();
                                    if !t.is_empty() {
                                        append_assistant(&mut turn.assistant, t);
                                    }
                                }
                                Some("tool_use") => {
                                    let (tc, hint) = tool_call(it);
                                    if let Some(h) = hint {
                                        path_hints.push(h);
                                    }
                                    for f in &tc.files {
                                        if f.starts_with('/') {
                                            path_hints.push(f.clone());
                                        }
                                    }
                                    turn.tools.push(tc);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if s.cwd.is_none()
        && let Some(project) = project_dir_name(path)
        && let Some(cwd) = infer_cwd_from_paths(project, &path_hints)
    {
        for turn in &mut s.turns {
            if turn.cwd.is_none() {
                turn.cwd = Some(cwd.clone());
            }
        }
        s.cwd = Some(cwd);
    }

    // Metadata timestamps are millisecond-precise, whereas prompt `<timestamp>`
    // tags are truncated to whole minutes. Prefer metadata for `started`, and
    // keep the latest of metadata and transcript timestamps for `ended`.
    if let Some(t) = meta_started {
        s.started = Some(t);
        s.ended = Some(s.ended.map_or(t, |x| x.max(t)));
    }
    if let Some(t) = meta_ended {
        s.started = Some(s.started.map_or(t, |x| x.min(t)));
        s.ended = Some(s.ended.map_or(t, |x| x.max(t)));
    }

    s
}

fn record_ts(s: &mut ParsedSession, t: DateTime<Utc>) {
    s.started = Some(s.started.map_or(t, |x| x.min(t)));
    s.ended = Some(s.ended.map_or(t, |x| x.max(t)));
}

fn append_assistant(out: &mut String, text: &str) {
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(text);
}

/// Subagent transcripts live under `<parent_id>/subagents/<sub_id>.jsonl`.
fn subagent_parent(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    if parent.file_name().and_then(|n| n.to_str()) != Some("subagents") {
        return None;
    }
    parent
        .parent()?
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// The `<project>` segment in `.../projects/<project>/agent-transcripts/...`.
fn project_dir_name(path: &Path) -> Option<&str> {
    let mut prev = None;
    let mut before_transcripts = None;
    let mut after_projects = None;
    for comp in path.components() {
        let name = comp.as_os_str().to_str();
        if name == Some("agent-transcripts") {
            before_transcripts = prev;
        }
        if prev == Some("projects") {
            after_projects = name;
        }
        prev = name;
    }
    before_transcripts.or(after_projects)
}

/// Cursor names each project directory by replacing runs of non-ASCII-alphanumeric
/// characters in the workspace path with `-` and trimming edge hyphens.
fn project_slug(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut prev_dash = true;
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
}

/// Recovers the workspace path when no sidecar recorded it, by finding an
/// ancestor of a tool's absolute file path whose slug matches `<project>`.
fn infer_cwd_from_paths(project: &str, paths: &[String]) -> Option<String> {
    for raw in paths {
        let mut p = Path::new(raw);
        while let Some(anc) = p.parent() {
            let s = anc.to_string_lossy();
            if s.is_empty() || s == "/" {
                break;
            }
            if project_slug(&s) == project {
                return Some(s.into_owned());
            }
            p = anc;
        }
        if project_slug(raw) == project {
            return Some(raw.clone());
        }
    }
    None
}

static USER_QUERY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<user_query>(.*?)</user_query>").unwrap());
static TIMESTAMP_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<timestamp>(.*?)</timestamp>").unwrap());

/// The prompt without Cursor's surrounding `<timestamp>`, `<image_files>`, or
/// `<manually_attached_skills>` wrappers.
fn user_text(content: &str) -> String {
    if let Some(c) = USER_QUERY.captures(content) {
        return c.get(1).unwrap().as_str().trim().to_string();
    }
    TIMESTAMP_TAG.replace_all(content, "").trim().to_string()
}

fn extract_timestamp(content: &str) -> Option<DateTime<Utc>> {
    let raw = TIMESTAMP_TAG.captures(content)?.get(1)?.as_str().trim();
    parse_cursor_ts(raw)
}

/// Parses timestamps like `Friday, Nov 20, 2026, 4:26 PM (UTC-7)` or RFC 3339.
fn parse_cursor_ts(s: &str) -> Option<DateTime<Utc>> {
    if let Some(t) = parse_ts(s) {
        return Some(t);
    }
    let (dt_part, tz_part) = s.rsplit_once('(')?;
    let tz = tz_part.trim().strip_suffix(')')?.trim();
    let offset = parse_utc_offset(tz)?;
    let naive = NaiveDateTime::parse_from_str(dt_part.trim(), "%A, %b %d, %Y, %I:%M %p").ok()?;
    naive
        .and_local_timezone(offset)
        .single()
        .map(|d| d.with_timezone(&Utc))
}

fn parse_utc_offset(tz: &str) -> Option<FixedOffset> {
    let rest = tz.strip_prefix("UTC")?;
    if rest.is_empty() {
        return FixedOffset::east_opt(0);
    }
    let (sign, digits) = match rest.as_bytes().first()? {
        b'+' => (1, &rest[1..]),
        b'-' => (-1, &rest[1..]),
        _ => return None,
    };
    let (hours, mins): (i32, i32) = match digits.split_once(':') {
        Some((h, m)) => (h.parse().ok()?, m.parse().ok()?),
        None => (digits.parse().ok()?, 0),
    };
    FixedOffset::east_opt(sign * (hours * 3600 + mins * 60))
}

fn tool_call(it: &Value) -> (ToolCall, Option<String>) {
    let name = it["name"].as_str().unwrap_or("tool").to_string();
    let inp = &it["input"];
    let mut files = vec![];
    let mut path_hint = None;
    let s = |k: &str| inp[k].as_str().unwrap_or("").to_string();
    let input = match name.as_str() {
        "Shell" => {
            let wd = s("working_directory");
            if wd.starts_with('/') {
                path_hint = Some(wd);
            }
            s("command")
        }
        "Read" | "ReadFile" | "StrReplace" | "Write" | "Edit" | "EditNotebook" | "Delete" => {
            let p = ["path", "file_path", "target_notebook"]
                .into_iter()
                .map(s)
                .find(|v| !v.is_empty())
                .unwrap_or_default();
            if !p.is_empty() {
                files.push(p.clone());
            }
            p
        }
        "Grep" | "rg" => {
            let p = s("path");
            if p.starts_with('/') {
                path_hint = Some(p.clone());
            }
            format!("{} {p}", s("pattern")).trim().to_string()
        }
        "Glob" => {
            let pat = if inp["glob_pattern"].is_string() {
                s("glob_pattern")
            } else {
                s("pattern")
            };
            let dir = if inp["target_directory"].is_string() {
                s("target_directory")
            } else {
                s("path")
            };
            if dir.starts_with('/') {
                path_hint = Some(dir.clone());
            }
            format!("{pat} {dir}").trim().to_string()
        }
        "WebFetch" => s("url"),
        "WebSearch" => {
            if inp["search_term"].is_string() {
                s("search_term")
            } else {
                s("query")
            }
        }
        "Task" => format!("{}\n{}", s("description"), clip_chars(&s("prompt"), 1500)),
        "CallDynamicTool" => {
            let ns = s("namespace");
            let tn = s("toolName");
            let target = match (ns.is_empty(), tn.is_empty()) {
                (false, false) => format!("{ns}.{tn}"),
                (true, false) => tn,
                (false, true) => ns,
                (true, true) => String::new(),
            };
            let args = if inp["arguments"].is_null() {
                String::new()
            } else {
                clip_chars(&inp["arguments"].to_string(), 500)
            };
            format!("{target} {args}").trim().to_string()
        }
        _ => clip_chars(&inp.to_string(), 600),
    };
    (
        ToolCall {
            id: it["id"].as_str().unwrap_or("").to_string(),
            name,
            input,
            files,
            ..Default::default()
        },
        path_hint,
    )
}

fn attach_result(turns: &mut [Turn], it: &Value) {
    let id = it["tool_use_id"].as_str().unwrap_or("");
    if id.is_empty() {
        return;
    }
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
            tc.output = if matches!(tc.name.as_str(), "Read" | "ReadFile") {
                clip_output(&text, 5, 0)
            } else {
                clip_output(&text, 20, 20)
            };
            return;
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SessionMetaFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at_ms: Option<i64>,
}

impl SessionMetaFile {
    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.cwd.is_none()
            && self.model.is_none()
            && self.created_at_ms.is_none()
            && self.updated_at_ms.is_none()
    }

    fn merge_over(self, existing: Self) -> Self {
        Self {
            title: self.title.or(existing.title),
            cwd: self.cwd.or(existing.cwd),
            model: self.model.or(existing.model),
            created_at_ms: self.created_at_ms.or(existing.created_at_ms),
            updated_at_ms: self.updated_at_ms.or(existing.updated_at_ms),
        }
    }
}

#[derive(Debug, Default)]
pub struct CursorMeta {
    by_id: HashMap<String, SessionMetaFile>,
    project_cwds: HashMap<String, String>,
}

/// Reads session titles, working directories, models and timestamps from
/// `<cursor_dir>/chats/`, `<cursor_dir>/projects/*/.workspace-trusted`, and
/// `<cursor_dir>/projects/*/sdk-agent-store/`.
pub fn read_meta(cursor_dir: &Path) -> Result<CursorMeta> {
    let mut out = CursorMeta::default();
    if !cursor_dir.exists() {
        return Ok(out);
    }

    // 1. Chats store (`<cursor_dir>/chats/<hash>/<id>/{meta.json,store.db}`).
    let chats = cursor_dir.join("chats");
    for hash_dir in std::fs::read_dir(&chats).into_iter().flatten().flatten() {
        if !hash_dir.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        for chat_dir in std::fs::read_dir(hash_dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            if !chat_dir.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let id = chat_dir.file_name().to_string_lossy().into_owned();
            let entry = out.by_id.entry(id).or_default();
            if let Ok(bytes) = std::fs::read(chat_dir.path().join("meta.json"))
                && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
            {
                if let Some(t) = v["title"].as_str().filter(|s| !s.is_empty()) {
                    entry.title = Some(t.to_string());
                }
                if let Some(c) = v["cwd"].as_str().filter(|s| !s.is_empty()) {
                    entry.cwd = Some(c.to_string());
                }
                if let Some(ms) = v["createdAtMs"].as_i64() {
                    entry.created_at_ms = Some(ms);
                }
                if let Some(ms) = v["updatedAtMs"].as_i64() {
                    entry.updated_at_ms = Some(ms);
                }
            }
            let db = chat_dir.path().join("store.db");
            if db.is_file() {
                read_chat_store_db(&db, entry);
            }
        }
    }

    // 2. Project `.workspace-trusted` and `sdk-agent-store/*/index.db`.
    let projects = cursor_dir.join("projects");
    for proj in std::fs::read_dir(&projects).into_iter().flatten().flatten() {
        if !proj.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let proj_name = proj.file_name().to_string_lossy().into_owned();
        if let Ok(bytes) = std::fs::read(proj.path().join(".workspace-trusted"))
            && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
            && let Some(wp) = v["workspacePath"].as_str().filter(|s| !s.is_empty())
        {
            out.project_cwds.insert(proj_name, wp.to_string());
        }
        let sdk_root = proj.path().join("sdk-agent-store");
        for store_dir in std::fs::read_dir(&sdk_root).into_iter().flatten().flatten() {
            let db = store_dir.path().join("index.db");
            if db.is_file() {
                read_sdk_index_db(&db, &mut out.by_id);
            }
        }
    }

    Ok(out)
}

fn read_chat_store_db(db: &Path, entry: &mut SessionMetaFile) {
    let Ok(c) =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return;
    };
    let Ok(hex) = c.query_row("SELECT value FROM meta WHERE key = '0'", [], |r| {
        r.get::<_, String>(0)
    }) else {
        return;
    };
    let Some(raw) = decode_hex(&hex) else {
        return;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&raw) else {
        return;
    };
    if entry.model.is_none()
        && let Some(m) = v["lastUsedModel"]
            .as_str()
            .filter(|s| !s.is_empty() && *s != "default")
    {
        entry.model = Some(m.to_string());
    }
    if entry.title.is_none()
        && let Some(t) = v["name"].as_str().filter(|s| !s.is_empty())
    {
        entry.title = Some(t.to_string());
    }
    if entry.created_at_ms.is_none()
        && let Some(ms) = v["createdAt"].as_i64()
    {
        entry.created_at_ms = Some(ms);
    }
}

fn read_sdk_index_db(db: &Path, by_id: &mut HashMap<String, SessionMetaFile>) {
    let Ok(c) =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return;
    };
    if let Ok(mut st) =
        c.prepare("SELECT agent_id, workspace_ref, created_at, updated_at FROM agents")
        && let Ok(rows) = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
    {
        for (id, cwd, created, updated) in rows.flatten() {
            let entry = by_id.entry(id).or_default();
            if entry.cwd.is_none() && !cwd.is_empty() {
                entry.cwd = Some(cwd);
            }
            if entry.created_at_ms.is_none()
                && let Some(ts) = parse_ts(&created)
            {
                entry.created_at_ms = Some(ts.timestamp_millis());
            }
            if entry.updated_at_ms.is_none()
                && let Some(ts) = parse_ts(&updated)
            {
                entry.updated_at_ms = Some(ts.timestamp_millis());
            }
        }
    }
    if let Ok(mut st) = c.prepare(
        "SELECT agent_id, model FROM runs WHERE model IS NOT NULL AND model != '' AND model != 'default' ORDER BY turn_number DESC",
    ) && let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
    {
        for (id, model) in rows.flatten() {
            let entry = by_id.entry(id).or_default();
            if entry.model.is_none() {
                entry.model = Some(model);
            }
        }
    }
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.as_chunks::<2>().0 {
        let hex = std::str::from_utf8(chunk).ok()?;
        out.push(u8::from_str_radix(hex, 16).ok()?);
    }
    Some(out)
}

/// Writes the session's merged metadata beside its archived transcript.
/// Returns true when it changed, since the session then needs reparsing even
/// if the transcript did not grow (for example when Cursor generates a title
/// after the last message). Existing archived fields are preserved when Cursor
/// later drops its `chats/` entry while the transcript remains.
pub fn write_meta(meta: &CursorMeta, archived: &Path) -> Result<bool> {
    let id = archived.file_stem().unwrap_or_default().to_string_lossy();
    let mut fresh = meta.by_id.get(id.as_ref()).cloned().unwrap_or_default();
    if fresh.cwd.is_none()
        && let Some(proj) = project_dir_name(archived)
        && let Some(cwd) = meta.project_cwds.get(proj)
    {
        fresh.cwd = Some(cwd.clone());
    }
    if fresh.is_empty() {
        return Ok(false);
    }
    let path = archived.with_extension("meta.json");
    let old_text = std::fs::read_to_string(&path).ok();
    let merged = match old_text
        .as_deref()
        .and_then(|s| serde_json::from_str::<SessionMetaFile>(s).ok())
    {
        Some(existing) => fresh.merge_over(existing),
        None => fresh,
    };
    let encoded = serde_json::to_string(&merged)?;
    if old_text.as_deref() == Some(encoded.as_str()) {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, encoded)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn transcript() -> String {
        [
            json!({
                "role": "user",
                "message": {
                    "content": [{
                        "type": "text",
                        "text": "<timestamp>Friday, Nov 20, 2026, 4:26 PM (UTC-7)</timestamp>\n<user_query>\nwhy does `cargo test` fail on macOS?\n</user_query>"
                    }]
                }
            }),
            json!({
                "role": "assistant",
                "message": {
                    "content": [
                        {"type": "tool_use", "name": "Shell", "input": {"command": "cargo test", "description": "Run tests"}},
                        {"type": "tool_use", "name": "Read", "input": {"path": "/work/my-app/src/lib.rs"}}
                    ]
                }
            }),
            json!({
                "role": "assistant",
                "message": {
                    "content": [
                        {"type": "text", "text": "The fixture path uses a Linux-only temp dir."},
                        {"type": "tool_use", "name": "StrReplace", "input": {"path": "/work/my-app/src/lib.rs", "old_string": "a", "new_string": "b"}}
                    ]
                }
            }),
            json!({
                "role": "assistant",
                "message": {
                    "content": [{"type": "text", "text": "Fixed and verified with `cargo test`."}]
                }
            }),
            json!({
                "role": "user",
                "message": {
                    "content": [{
                        "type": "text",
                        "text": "<manually_attached_skills>\nSkill details\n</manually_attached_skills>\n<timestamp>Friday, Nov 20, 2026, 4:29 PM (UTC-7)</timestamp>\n<user_query>\ncommit that change\n</user_query>"
                    }]
                }
            }),
            json!({
                "role": "assistant",
                "message": {
                    "content": [{"type": "text", "text": "Committed."}]
                }
            }),
            json!({"type": "turn_ended", "status": "success"}),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n"
    }

    #[test]
    fn messages_become_turns_with_tools_and_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("projects/work-my-app/agent-transcripts/sess-1/sess-1.jsonl");
        let s = parse(&path, transcript().as_bytes());
        assert_eq!(s.id, "sess-1");
        assert_eq!(s.source, "cursor");
        assert!(s.parent_id.is_none());
        assert_eq!(s.bad_lines, 0);
        assert_eq!(s.turns.len(), 2);
        // Cwd inferred from tool path matching project directory slug `work-my-app`.
        assert_eq!(s.cwd.as_deref(), Some("/work/my-app"));

        let t0 = &s.turns[0];
        assert_eq!(t0.user, "why does `cargo test` fail on macOS?");
        assert_eq!(
            t0.assistant,
            "The fixture path uses a Linux-only temp dir.\n\nFixed and verified with `cargo test`."
        );
        assert_eq!(t0.tools.len(), 3);
        assert_eq!(t0.tools[0].name, "Shell");
        assert_eq!(t0.tools[0].input, "cargo test");
        assert_eq!(t0.tools[1].files, ["/work/my-app/src/lib.rs"]);
        assert_eq!(t0.tools[2].name, "StrReplace");
        assert_eq!(t0.cwd.as_deref(), Some("/work/my-app"));
        assert_eq!(t0.ts, parse_ts("2026-11-20T23:26:00Z"));

        let t1 = &s.turns[1];
        assert_eq!(t1.user, "commit that change");
        assert_eq!(t1.assistant, "Committed.");
        assert_eq!(t1.ts, parse_ts("2026-11-20T23:29:00Z"));

        assert_eq!(s.started, parse_ts("2026-11-20T23:26:00Z"));
        assert_eq!(s.ended, parse_ts("2026-11-20T23:29:00Z"));
    }

    #[test]
    fn subagent_path_sets_parent_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("projects/work-app/agent-transcripts/parent-1/subagents/child-1.jsonl");
        let s = parse(&path, b"");
        assert_eq!(s.id, "child-1");
        assert_eq!(s.parent_id.as_deref(), Some("parent-1"));
    }

    #[test]
    fn timestamps_parse_across_offsets() {
        assert_eq!(
            parse_cursor_ts("Friday, Nov 20, 2026, 5:13 AM (UTC)"),
            parse_ts("2026-11-20T05:13:00Z")
        );
        assert_eq!(
            parse_cursor_ts("Monday, Nov 2, 2026, 9:05 AM (UTC+5:30)"),
            parse_ts("2026-11-02T03:35:00Z")
        );
        assert_eq!(
            parse_cursor_ts("Thursday, Nov 19, 2026, 12:00 AM (UTC-8)"),
            parse_ts("2026-11-19T08:00:00Z")
        );
        assert_eq!(
            parse_cursor_ts("2026-11-20T23:26:00Z"),
            parse_ts("2026-11-20T23:26:00Z")
        );
    }

    #[test]
    fn metadata_reads_chats_store_workspace_trusted_and_sdk_store() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();

        // 1. Chat with meta.json + store.db
        let chat = root.join("chats/abc123/sess-1");
        std::fs::create_dir_all(&chat)?;
        std::fs::write(
            chat.join("meta.json"),
            json!({
                "schemaVersion": 1,
                "createdAtMs": 1_795_217_165_000_i64,
                "updatedAtMs": 1_795_217_400_000_i64,
                "title": "Fix macOS temp dir",
                "cwd": "/work/my-app"
            })
            .to_string(),
        )?;
        let c = rusqlite::Connection::open(chat.join("store.db"))?;
        let store_meta = json!({
            "agentId": "sess-1",
            "name": "Fallback title",
            "lastUsedModel": "gemini-4-neon"
        })
        .to_string();
        let hex: String = store_meta.bytes().map(|b| format!("{b:02x}")).collect();
        c.execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta VALUES ('0', '{hex}');"
        ))?;
        drop(c);

        // 2. Project .workspace-trusted (used by a subagent without cwd in chats/)
        let proj = root.join("projects/work-my-app");
        std::fs::create_dir_all(&proj)?;
        std::fs::write(
            proj.join(".workspace-trusted"),
            json!({"workspacePath": "/work/my-app"}).to_string(),
        )?;

        // 3. SDK agent store
        let sdk = proj.join("sdk-agent-store/hash1");
        std::fs::create_dir_all(&sdk)?;
        let sc = rusqlite::Connection::open(sdk.join("index.db"))?;
        sc.execute_batch(
            "CREATE TABLE agents (agent_id TEXT PRIMARY KEY, workspace_ref TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE runs (run_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, turn_number INTEGER NOT NULL, model TEXT);
             INSERT INTO agents VALUES ('agent-sdk-1', '/work/my-app', '2026-11-20T23:00:00Z', '2026-11-20T23:10:00Z');
             INSERT INTO runs VALUES ('r1', 'agent-sdk-1', 1, 'default'), ('r2', 'agent-sdk-1', 2, 'gpt-5.6-sol');",
        )?;
        drop(sc);

        let meta = read_meta(root)?;
        let archived =
            root.join("archive/projects/work-my-app/agent-transcripts/sess-1/sess-1.jsonl");
        assert!(write_meta(&meta, &archived)?);
        assert!(
            !write_meta(&meta, &archived)?,
            "unchanged metadata rewritten"
        );
        let s = parse(&archived, transcript().as_bytes());
        assert_eq!(s.title.as_deref(), Some("Fix macOS temp dir"));
        assert_eq!(s.cwd.as_deref(), Some("/work/my-app"));
        assert_eq!(s.model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(
            s.started,
            DateTime::from_timestamp_millis(1_795_217_165_000)
        );
        assert_eq!(s.ended, DateTime::from_timestamp_millis(1_795_217_400_000));

        // Subagent inherits cwd from .workspace-trusted
        let sub = root
            .join("archive/projects/work-my-app/agent-transcripts/sess-1/subagents/sub-1.jsonl");
        assert!(write_meta(&meta, &sub)?);
        let s = parse(&sub, b"");
        assert_eq!(s.parent_id.as_deref(), Some("sess-1"));
        assert_eq!(s.cwd.as_deref(), Some("/work/my-app"));

        // SDK session gets cwd, model, and timestamps
        let sdk_arch = root
            .join("archive/projects/work-my-app/agent-transcripts/agent-sdk-1/agent-sdk-1.jsonl");
        assert!(write_meta(&meta, &sdk_arch)?);
        let s = parse(&sdk_arch, b"");
        assert_eq!(s.cwd.as_deref(), Some("/work/my-app"));
        assert_eq!(s.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(s.started, parse_ts("2026-11-20T23:00:00Z"));
        assert_eq!(s.ended, parse_ts("2026-11-20T23:10:00Z"));

        assert!(read_meta(&root.join("missing"))?.by_id.is_empty());
        Ok(())
    }

    #[test]
    fn project_dir_name_ignores_earlier_projects_ancestors() {
        let p = Path::new(
            "/Users/me/projects/recherche-data/archive/cursor/projects/work-my-app/agent-transcripts/sess-1/subagents/sub-1.jsonl",
        );
        assert_eq!(project_dir_name(p), Some("work-my-app"));
        let flat = Path::new(
            "/home/u/projects/data/archive/cursor/projects/home-u-repo/agent-transcripts/sess-1.jsonl",
        );
        assert_eq!(project_dir_name(flat), Some("home-u-repo"));
    }

    #[test]
    fn write_meta_preserves_archived_fields_when_chat_is_deleted() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();

        let chat = root.join("chats/abc123/sess-1");
        std::fs::create_dir_all(&chat)?;
        std::fs::write(
            chat.join("meta.json"),
            json!({
                "createdAtMs": 1_795_217_165_000_i64,
                "updatedAtMs": 1_795_217_400_000_i64,
                "title": "Fix macOS temp dir",
                "cwd": "/work/my-app"
            })
            .to_string(),
        )?;
        let c = rusqlite::Connection::open(chat.join("store.db"))?;
        let store_meta = json!({
            "agentId": "sess-1",
            "lastUsedModel": "gemini-4-neon"
        })
        .to_string();
        let hex: String = store_meta.bytes().map(|b| format!("{b:02x}")).collect();
        c.execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta VALUES ('0', '{hex}');"
        ))?;
        drop(c);

        let proj = root.join("projects/work-my-app");
        std::fs::create_dir_all(&proj)?;
        std::fs::write(
            proj.join(".workspace-trusted"),
            json!({"workspacePath": "/work/my-app"}).to_string(),
        )?;

        let archived = root.join(
            "projects/data/archive/cursor/projects/work-my-app/agent-transcripts/sess-1/sess-1.jsonl",
        );
        let meta = read_meta(root)?;
        assert!(write_meta(&meta, &archived)?);

        // Cursor drops the chats/<hash>/<id> folder while the transcript and
        // .workspace-trusted stay on disk.
        std::fs::remove_dir_all(&chat)?;
        let meta_after = read_meta(root)?;
        assert!(!meta_after.by_id.contains_key("sess-1"));
        assert!(
            !write_meta(&meta_after, &archived)?,
            "dropping chats/ must not overwrite archived metadata"
        );

        let s = parse(&archived, transcript().as_bytes());
        assert_eq!(s.title.as_deref(), Some("Fix macOS temp dir"));
        assert_eq!(s.cwd.as_deref(), Some("/work/my-app"));
        assert_eq!(s.model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(
            s.started,
            DateTime::from_timestamp_millis(1_795_217_165_000)
        );
        assert_eq!(s.ended, DateTime::from_timestamp_millis(1_795_217_400_000));
        Ok(())
    }
}
